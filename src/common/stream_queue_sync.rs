#![allow(dead_code)]

use futures::stream::Stream;
use futures::sync::mpsc::unbounded;
use futures::sync::mpsc::UnboundedReceiver;
use futures::sync::mpsc::UnboundedSender;
use futures::Async;
use futures::Poll;

use crate::error;

use crate::client::stream_handler::ClientStreamHandler;
use crate::client::types::ClientTypes;
use crate::common::types::Types;
use crate::data_or_headers::DataOrHeaders;
use crate::data_or_headers_with_flag::DataOrHeadersWithFlag;
use crate::result;
use crate::server::stream_handler::ServerStreamHandler;
use crate::server::types::ServerTypes;
use crate::ErrorCode;
use crate::Headers;
use bytes::Bytes;
use std::marker;

pub(crate) struct StreamQueueSyncSender<T: Types> {
    sender: UnboundedSender<Result<DataOrHeadersWithFlag, error::Error>>,
    _marker: marker::PhantomData<T>,
}

pub(crate) struct StreamQueueSyncReceiver<T: Types> {
    receiver: UnboundedReceiver<Result<DataOrHeadersWithFlag, error::Error>>,
    eof_received: bool,
    _marker: marker::PhantomData<T>,
}

impl<T: Types> StreamQueueSyncSender<T> {
    fn send(&self, item: Result<DataOrHeadersWithFlag, error::Error>) -> result::Result<()> {
        if let Err(_send_error) = self.sender.unbounded_send(item) {
            // TODO: better error
            Err(error::Error::PullStreamDied)
        } else {
            Ok(())
        }
    }
}

impl ServerStreamHandler for StreamQueueSyncSender<ServerTypes> {
    fn data_frame(&mut self, data: Bytes, end_stream: bool) -> result::Result<()> {
        self.send(Ok(DataOrHeadersWithFlag {
            content: DataOrHeaders::Data(data),
            last: end_stream,
        }))
    }

    fn trailers(&mut self, trailers: Headers) -> result::Result<()> {
        self.send(Ok(DataOrHeadersWithFlag {
            content: DataOrHeaders::Headers(trailers),
            last: true,
        }))
    }

    fn rst(&mut self, error_code: ErrorCode) -> result::Result<()> {
        self.send(Err(error::Error::RstStreamReceived(error_code)))
    }

    fn error(&mut self, error: error::Error) -> result::Result<()> {
        self.send(Err(error))
    }
}

impl ClientStreamHandler for StreamQueueSyncSender<ClientTypes> {
    fn headers(&mut self, headers: Headers, end_stream: bool) -> result::Result<()> {
        self.send(Ok(DataOrHeadersWithFlag {
            content: DataOrHeaders::Headers(headers),
            last: end_stream,
        }))
    }

    fn data_frame(&mut self, data: Bytes, end_stream: bool) -> result::Result<()> {
        self.send(Ok(DataOrHeadersWithFlag {
            content: DataOrHeaders::Data(data),
            last: end_stream,
        }))
    }

    fn trailers(&mut self, trailers: Headers) -> result::Result<()> {
        self.send(Ok(DataOrHeadersWithFlag {
            content: DataOrHeaders::Headers(trailers),
            last: true,
        }))
    }

    fn rst(&mut self, error_code: ErrorCode) -> result::Result<()> {
        self.send(Err(error::Error::RstStreamReceived(error_code)))
    }

    fn error(&mut self, error: error::Error) -> result::Result<()> {
        self.send(Err(error))
    }
}

impl<T: Types> Stream for StreamQueueSyncReceiver<T> {
    type Item = DataOrHeadersWithFlag;
    type Error = error::Error;

    fn poll(&mut self) -> Poll<Option<DataOrHeadersWithFlag>, error::Error> {
        if self.eof_received {
            return Ok(Async::Ready(None));
        }

        let part = match self.receiver.poll() {
            Err(()) => unreachable!(),
            Ok(Async::NotReady) => return Ok(Async::NotReady),
            Ok(Async::Ready(None)) => {
                // should be impossible, because
                // callbacks are notified of client death in
                // `HttpStreamCommon::conn_died`
                return Err(error::Error::InternalError(
                    "internal error: unexpected EOF".to_owned(),
                ));
            }
            Ok(Async::Ready(Some(Err(e)))) => {
                self.eof_received = true;
                return Err(e);
            }
            Ok(Async::Ready(Some(Ok(part)))) => {
                if part.last {
                    self.eof_received = true;
                }
                part
            }
        };

        Ok(Async::Ready(Some(part)))
    }
}

pub(crate) fn stream_queue_sync<T: Types>() -> (StreamQueueSyncSender<T>, StreamQueueSyncReceiver<T>)
{
    let (utx, urx) = unbounded();

    let tx = StreamQueueSyncSender {
        sender: utx,
        _marker: marker::PhantomData,
    };
    let rx = StreamQueueSyncReceiver {
        receiver: urx,
        eof_received: false,
        _marker: marker::PhantomData,
    };

    (tx, rx)
}
