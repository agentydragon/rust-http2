use std::net::SocketAddr;
use std::sync::Arc;
use std::io;

use error;
use error::ErrorCode;
use error::Error;

use solicit::StreamId;
use solicit::HttpScheme;
use solicit::header::*;
use solicit::connection::EndStream;

use bytes::Bytes;

use futures;
use futures::Future;
use futures::stream::Stream;

use native_tls::TlsConnector;

use tokio_core::net::TcpStream;
use tokio_core::reactor;
use tokio_timer::Timer;
use tokio_io::AsyncWrite;
use tokio_io::AsyncRead;
use tokio_tls::TlsConnectorExt;

use futures_misc::*;

use solicit_async::*;

use conn::*;
use stream_part::*;
use client_conf::*;
use client_tls::*;


struct ClientStream {
    common: HttpStreamCommon,
    response_handler: Option<futures::sync::mpsc::UnboundedSender<ResultOrEof<HttpStreamPart, Error>>>,
}

impl HttpStream for ClientStream {
    fn common(&self) -> &HttpStreamCommon {
        &self.common
    }

    fn common_mut(&mut self) -> &mut HttpStreamCommon {
        &mut self.common
    }

    fn new_data_chunk(&mut self, data: &[u8], last: bool) {
        if let Some(ref mut response_handler) = self.response_handler {
            // TODO: reset stream if called is dead
            drop(response_handler.send(ResultOrEof::Item(HttpStreamPart {
                content: HttpStreamPartContent::Data(Bytes::from(data)),
                last: last,
            })));
        }
    }

    fn rst(&mut self, error_code: ErrorCode) {
        if let Some(ref mut response_handler) = self.response_handler {
            // TODO: reset stream if called is dead
            drop(response_handler.send(ResultOrEof::Error(Error::CodeError(error_code))));
        }
    }

    fn closed_remote(&mut self) {
        if let Some(response_handler) = self.response_handler.take() {
            // it is OK to ignore error: handler may be already dead
            drop(response_handler.send(ResultOrEof::Eof));
        }
    }
}

impl ClientStream {
}

struct ClientSessionState {
    next_stream_id: StreamId,
    loop_handle: reactor::Handle,
}

struct ClientInner {
    common: LoopInnerCommon<ClientStream>,
    to_write_tx: futures::sync::mpsc::UnboundedSender<ClientToWriteMessage>,
    session_state: ClientSessionState,
}

impl ClientInner {
    fn insert_stream(&mut self, stream: ClientStream) -> StreamId {
        let id = self.session_state.next_stream_id;
        if let Some(..) = self.common.streams.insert(id, stream) {
            panic!("inserted stream that already existed");
        }
        self.session_state.next_stream_id += 2;
        id
    }
}

impl LoopInner for ClientInner {
    type LoopHttpStream = ClientStream;

    fn common(&mut self) -> &mut LoopInnerCommon<ClientStream> {
        &mut self.common
    }

    fn send_common(&mut self, message: CommonToWriteMessage) {
        self.to_write_tx.send(ClientToWriteMessage::Common(message))
            .expect("read to write common");
    }

    fn process_headers(&mut self, stream_id: StreamId, end_stream: EndStream, headers: Headers) {
        let mut stream: &mut ClientStream = match self.common.get_stream_mut(stream_id) {
            None => {
                // TODO(mlalic): This means that the server's header is not associated to any
                //               request made by the client nor any server-initiated stream (pushed)
                return;
            }
            Some(stream) => stream,
        };
        // TODO: hack
        if headers.0.len() != 0 {

            if let Some(ref mut response_handler) = stream.response_handler {
                // TODO: reset stream if called is dead
                drop(response_handler.send(ResultOrEof::Item(HttpStreamPart {
                    content: HttpStreamPartContent::Headers(headers),
                    last: end_stream == EndStream::Yes,
                })));
            }
        }
    }
}

pub struct ClientConnection {
    call_tx: futures::sync::mpsc::UnboundedSender<ClientToWriteMessage>,
    command_tx: futures::sync::mpsc::UnboundedSender<ClientCommandMessage>,
    _remote: reactor::Remote,
}

unsafe impl Sync for ClientConnection {}

pub struct StartRequestMessage {
    pub headers: Headers,
    pub body: HttpPartStream,
    pub resp_tx: futures::sync::mpsc::UnboundedSender<ResultOrEof<HttpStreamPart, Error>>,
}

struct BodyChunkMessage {
    stream_id: StreamId,
    chunk: Bytes,
}

struct EndRequestMessage {
    stream_id: StreamId,
}

enum ClientToWriteMessage {
    Start(StartRequestMessage),
    BodyChunk(BodyChunkMessage),
    End(EndRequestMessage),
    Common(CommonToWriteMessage),
}

enum ClientCommandMessage {
    DumpState(futures::sync::oneshot::Sender<ConnectionStateSnapshot>),
}


impl<I : AsyncRead + AsyncWrite + Send + 'static> ClientWriteLoop<I> {
    fn process_start(self, start: StartRequestMessage) -> HttpFuture<Self> {
        let StartRequestMessage { headers, body, resp_tx } = start;

        let stream_id = self.inner.with(move |inner: &mut ClientInner| {

            let mut stream = ClientStream {
                common: HttpStreamCommon::new(inner.common.conn.peer_settings.initial_window_size),
                response_handler: Some(resp_tx),
            };

            stream.common.outgoing.push_back(HttpStreamPartContent::Headers(headers));

            inner.insert_stream(stream)
        });

        let to_write_tx_1 = self.inner.with(|inner| inner.to_write_tx.clone());
        let to_write_tx_2 = to_write_tx_1.clone();

        self.inner.with(|inner: &mut ClientInner| {
            let future = body
                .check_only_data() // TODO: headers too
                .fold((), move |(), chunk| {
                    to_write_tx_1.send(ClientToWriteMessage::BodyChunk(BodyChunkMessage {
                        stream_id: stream_id,
                        chunk: chunk,
                    })).expect("client must be dead");
                    futures::finished::<_, Error>(())
                });
            let future = future
                .and_then(move |()| {
                    to_write_tx_2.send(ClientToWriteMessage::End(EndRequestMessage {
                        stream_id: stream_id,
                    })).expect("client must be dead");
                    futures::finished::<_, Error>(())
                });

            let future = future.map_err(|e| {
                warn!("{:?}", e);
                ()
            });

            inner.session_state.loop_handle.spawn(future);
        });

        self.send_outg_stream(stream_id)
    }

    fn process_body_chunk(self, body_chunk: BodyChunkMessage) -> HttpFuture<Self> {
        let BodyChunkMessage { stream_id, chunk } = body_chunk;

        self.inner.with(move |inner: &mut ClientInner| {
            let stream = inner.common.get_stream_mut(stream_id)
                .expect(&format!("stream not found: {}", stream_id));
            // TODO: check stream state

            stream.common.outgoing.push_back(HttpStreamPartContent::Data(Bytes::from(chunk)));
        });

        self.send_outg_stream(stream_id)
    }

    fn process_end(self, end: EndRequestMessage) -> HttpFuture<Self> {
        let EndRequestMessage { stream_id } = end;

        self.inner.with(move |inner: &mut ClientInner| {
            let stream = inner.common.get_stream_mut(stream_id)
                .expect(&format!("stream not found: {}", stream_id));

            // TODO: check stream state
            stream.common.outgoing_end = Some(ErrorCode::NoError);
        });

        self.send_outg_stream(stream_id)
    }

    fn process_message(self, message: ClientToWriteMessage) -> HttpFuture<Self> {
        match message {
            ClientToWriteMessage::Start(start) => self.process_start(start),
            ClientToWriteMessage::BodyChunk(body_chunk) => self.process_body_chunk(body_chunk),
            ClientToWriteMessage::End(end) => self.process_end(end),
            ClientToWriteMessage::Common(common) => self.process_common(common),
        }
    }

    fn run(self, requests: HttpFutureStreamSend<ClientToWriteMessage>) -> HttpFuture<()> {
        let requests = requests.map_err(Error::from);
        Box::new(requests
            .fold(self, move |wl, message: ClientToWriteMessage| {
                wl.process_message(message)
            })
            .map(|_| ()))
    }
}

type ClientReadLoop<I> = ReadLoopData<I, ClientInner>;
type ClientWriteLoop<I> = WriteLoopData<I, ClientInner>;
type ClientCommandLoop = CommandLoopData<ClientInner>;


impl ClientConnection {
    fn connected<I : AsyncWrite + AsyncRead + Send + 'static>(
        lh: reactor::Handle, connect: HttpFutureSend<I>, _conf: ClientConf)
            -> (Self, HttpFuture<()>)
    {
        let (to_write_tx, to_write_rx) = futures::sync::mpsc::unbounded();
        let (command_tx, command_rx) = futures::sync::mpsc::unbounded();

        let to_write_rx = Box::new(to_write_rx.map_err(|()| Error::IoError(io::Error::new(io::ErrorKind::Other, "to_write"))));
        let command_rx = Box::new(command_rx.map_err(|()| Error::IoError(io::Error::new(io::ErrorKind::Other, "to_write"))));

        let c = ClientConnection {
            _remote: lh.remote().clone(),
            call_tx: to_write_tx.clone(),
            command_tx: command_tx,
        };

        let handshake = connect.and_then(client_handshake);

        let future = handshake.and_then(move |conn| {
            debug!("handshake done");
            let (read, write) = conn.split();

            let inner = TaskRcMut::new(ClientInner {
                common: LoopInnerCommon::new(HttpScheme::Http),
                to_write_tx: to_write_tx.clone(),
                session_state: ClientSessionState {
                    next_stream_id: 1,
                    loop_handle: lh,
                }
            });

            let run_write = ClientWriteLoop { write: write, inner: inner.clone() }.run(to_write_rx);
            let run_read = ClientReadLoop { read: read, inner: inner.clone() }.run();
            let run_command = ClientCommandLoop { inner: inner.clone() }.run(command_rx);

            run_write.join(run_read).join(run_command).map(|_| ())
        });

        (c, Box::new(future))
    }

    pub fn new(lh: reactor::Handle, addr: &SocketAddr, tls: ClientTlsOption, conf: ClientConf) -> (Self, HttpFuture<()>) {
        match tls {
            ClientTlsOption::Plain =>
                ClientConnection::new_plain(lh, addr, conf),
            ClientTlsOption::Tls(domain, connector) =>
                ClientConnection::new_tls(lh, &domain, connector, addr, conf),
        }
    }

    pub fn new_plain(lh: reactor::Handle, addr: &SocketAddr, conf: ClientConf) -> (Self, HttpFuture<()>) {
        let addr = addr.clone();

        let no_delay = conf.no_delay.unwrap_or(true);
        let connect = TcpStream::connect(&addr, &lh).map_err(Into::into);
        let map_callback = move |socket: TcpStream| {
            info!("connected to {}", addr);

            socket.set_nodelay(no_delay).expect("failed to set TCP_NODELAY");

            socket
        };

        let connect = if let Some(timeout) = conf.connection_timeout {
            let timer = Timer::default();
            timer.timeout(connect, timeout).map(map_callback).boxed()
        } else {
            connect.map(map_callback).boxed()
        };

        ClientConnection::connected(lh, connect, conf)
    }

    pub fn new_tls(
        lh: reactor::Handle,
        domain: &str,
        connector: Arc<TlsConnector>,
        addr: &SocketAddr,
        conf: ClientConf)
            -> (Self, HttpFuture<()>)
    {
        let domain = domain.to_owned();
        let addr = addr.clone();

        let connect = TcpStream::connect(&addr, &lh)
            .map(move |c| { info!("connected to {}", addr); c })
            .map_err(|e| e.into());

        let tls_conn = connect.and_then(move |conn| {
            connector.connect_async(&domain, conn).map_err(|e| {
                Error::IoError(io::Error::new(io::ErrorKind::Other, e))
            })
        });

        let tls_conn = tls_conn.map_err(Error::from);

        ClientConnection::connected(lh, Box::new(tls_conn), conf)
    }

    pub fn start_request_with_resp_sender(
        &self,
        start: StartRequestMessage)
            -> Result<(), StartRequestMessage>
    {
        self.call_tx.send(ClientToWriteMessage::Start(start))
            .map_err(|send_error| {
                match send_error.into_inner() {
                    ClientToWriteMessage::Start(start) => start,
                    _ => unreachable!(),
                }
            })
    }

    pub fn start_request(
        &self,
        headers: Headers,
        body: HttpPartStream)
            -> Response
    {
        let (resp_tx, resp_rx) = futures::sync::mpsc::unbounded();

        let start = StartRequestMessage {
            headers: headers,
            body: body,
            resp_tx: resp_tx,
        };

        if let Err(_) = self.start_request_with_resp_sender(start) {
            return Response::err(error::Error::Other("client died"));
        }

        let req_rx = resp_rx.map_err(|()| Error::from(io::Error::new(io::ErrorKind::Other, "req")));

        let req_rx = stream_with_eof_and_error(req_rx, || error::Error::Other("client is likely died"));

        Response::from_stream(req_rx)
    }

    pub fn dump_state_with_resp_sender(&self, tx: futures::sync::oneshot::Sender<ConnectionStateSnapshot>) {
        // ignore error
        drop(self.command_tx.send(ClientCommandMessage::DumpState(tx)));
    }

    /// For tests
    pub fn dump_state(&self) -> HttpFutureSend<ConnectionStateSnapshot> {
        let (tx, rx) = futures::oneshot();

        self.dump_state_with_resp_sender(tx);

        let rx = rx.map_err(|_| Error::from(io::Error::new(io::ErrorKind::Other, "oneshot canceled")));

        Box::new(rx)
    }
}

impl ClientCommandLoop {
    fn process_dump_state(self, sender: futures::sync::oneshot::Sender<ConnectionStateSnapshot>) -> HttpFuture<Self> {
        // ignore send error, client might be already dead
        drop(sender.send(self.inner.with(|inner| inner.common.dump_state())));
        Box::new(futures::finished(self))
    }

    fn process_message(self, message: ClientCommandMessage) -> HttpFuture<Self> {
        match message {
            ClientCommandMessage::DumpState(sender) => self.process_dump_state(sender),
        }
    }

    fn run(self, requests: HttpFutureStreamSend<ClientCommandMessage>) -> HttpFuture<()> {
        let requests = requests.map_err(Error::from);
        Box::new(requests
            .fold(self, move |l, message: ClientCommandMessage| {
                l.process_message(message)
            })
            .map(|_| ()))
    }
}