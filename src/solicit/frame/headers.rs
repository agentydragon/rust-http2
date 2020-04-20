//! The module contains the implementation of the `HEADERS` frame and associated flags.

use bytes::Bytes;

use crate::codec::write_buffer::{WriteBuffer, WriteBufferTailVec};
use crate::hpack;
use crate::hpack::encoder::EncodeBuf;
use crate::solicit::frame::continuation::ContinuationFlag;
use crate::solicit::frame::flags::*;
use crate::solicit::frame::pack_header;
use crate::solicit::frame::HttpFrameType;
use crate::solicit::frame::ParseFrameError;
use crate::solicit::frame::ParseFrameResult;
use crate::solicit::frame::FRAME_HEADER_LEN;
use crate::solicit::frame::{
    parse_padded_payload, Frame, FrameBuilder, FrameHeader, FrameIR, RawFrame,
};
use crate::solicit::stream_id::StreamId;
use crate::Headers;
use std::cmp;
use std::fmt;

pub const HEADERS_FRAME_TYPE: u8 = 0x1;

/// An enum representing the flags that a `HeadersFrame` can have.
/// The integer representation associated to each variant is that flag's
/// bitmask.
///
/// HTTP/2 spec, section 6.2.
#[derive(Clone, PartialEq, Debug, Copy)]
pub enum HeadersFlag {
    EndStream = 0x1,
    EndHeaders = 0x4,
    Padded = 0x8,
    Priority = 0x20,
}

impl Flag for HeadersFlag {
    #[inline]
    fn bitmask(&self) -> u8 {
        *self as u8
    }

    fn flags() -> &'static [Self] {
        static FLAGS: &'static [HeadersFlag] = &[
            HeadersFlag::EndStream,
            HeadersFlag::EndHeaders,
            HeadersFlag::Padded,
            HeadersFlag::Priority,
        ];
        FLAGS
    }
}

/// The struct represents the dependency information that can be attached to
/// a stream and sent within a HEADERS frame (one with the Priority flag set).
#[derive(PartialEq, Debug, Clone)]
pub struct StreamDependency {
    /// The ID of the stream that a particular stream depends on
    pub stream_id: StreamId,
    /// The weight for the stream. The value exposed (and set) here is always
    /// in the range [0, 255], instead of [1, 256] \(as defined in section 5.3.2.)
    /// so that the value fits into a `u8`.
    pub weight: u8,
    /// A flag indicating whether the stream dependency is exclusive.
    pub is_exclusive: bool,
}

impl StreamDependency {
    /// Creates a new `StreamDependency` with the given stream ID, weight, and
    /// exclusivity.
    pub fn new(stream_id: StreamId, weight: u8, is_exclusive: bool) -> StreamDependency {
        StreamDependency {
            stream_id: stream_id,
            weight: weight,
            is_exclusive: is_exclusive,
        }
    }

    /// Parses the first 5 bytes in the buffer as a `StreamDependency`.
    /// (Each 5-byte sequence is always decodable into a stream dependency
    /// structure).
    ///
    /// # Panics
    ///
    /// If the given buffer has less than 5 elements, the method will panic.
    pub fn parse(buf: &[u8]) -> StreamDependency {
        // The most significant bit of the first byte is the "E" bit indicating
        // whether the dependency is exclusive.
        let is_exclusive = buf[0] & 0x80 != 0;
        let stream_id = {
            // Parse the first 4 bytes into a u32...
            let mut id = unpack_octets_4!(buf, 0, u32);
            // ...clear the first bit since the stream id is only 31 bits.
            id &= !(1 << 31);
            id
        };

        StreamDependency {
            stream_id: stream_id,
            weight: buf[4],
            is_exclusive: is_exclusive,
        }
    }

    /// Serializes the `StreamDependency` into a 5-byte buffer representing the
    /// dependency description, as described in section 6.2. of the HTTP/2
    /// spec:
    ///
    /// ```notest
    ///  0                   1                   2                   3
    ///  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
    /// +-+-------------+-----------------------------------------------+
    /// |E|                 Stream Dependency  (31)                     |
    /// +-+-------------+-----------------------------------------------+
    /// |  Weight  (8)  |
    /// +-+-------------+-----------------------------------------------+
    /// ```
    ///
    /// Where "E" is set if the dependency is exclusive.
    pub fn serialize(&self) -> [u8; 5] {
        let e_bit = if self.is_exclusive { 1 << 7 } else { 0 };
        [
            (((self.stream_id >> 24) & 0x000000FF) as u8) | e_bit,
            (((self.stream_id >> 16) & 0x000000FF) as u8),
            (((self.stream_id >> 8) & 0x000000FF) as u8),
            (((self.stream_id) & 0x000000FF) as u8),
            self.weight,
        ]
    }
}

/// A struct representing the HEADERS frames of HTTP/2, as defined in the
/// HTTP/2 spec, section 6.2.
#[derive(PartialEq, Clone, Debug)]
pub struct HeadersFrame {
    /// The set of flags for the frame, packed into a single byte.
    pub flags: Flags<HeadersFlag>,
    /// The ID of the stream with which this frame is associated
    pub stream_id: StreamId,
    /// The header fragment bytes stored within the frame.
    pub header_fragment: Bytes,
    /// The stream dependency information, if any.
    pub stream_dep: Option<StreamDependency>,
    /// The length of the padding, if any.
    pub padding_len: u8,
}

impl HeadersFrame {
    /// Creates a new `HeadersFrame` with the given header fragment and stream
    /// ID. No padding, no stream dependency, and no flags are set.
    pub fn new(fragment: Bytes, stream_id: StreamId) -> HeadersFrame {
        HeadersFrame {
            header_fragment: fragment,
            stream_id,
            stream_dep: None,
            padding_len: 0,
            flags: Flags::default(),
        }
    }

    /// Separate constructor from `new` to avoid accidental invocation with incorrect type
    /// which may result in unnecessary memory allocation (e. g. `&Vec` instead of `Vec`)
    pub fn new_conv<B: Into<Bytes>>(fragment: B, stream_id: StreamId) -> HeadersFrame {
        HeadersFrame::new(fragment.into(), stream_id)
    }

    /// Creates a new `HeadersFrame` with the given header fragment, stream ID
    /// and stream dependency information. No padding and no flags are set.
    pub fn with_dependency(
        fragment: Vec<u8>,
        stream_id: StreamId,
        stream_dep: StreamDependency,
    ) -> HeadersFrame {
        HeadersFrame {
            header_fragment: Bytes::from(fragment),
            stream_id: stream_id,
            stream_dep: Some(stream_dep),
            padding_len: 0,
            flags: HeadersFlag::Priority.to_flags(),
        }
    }

    /// Returns whether this frame ends the headers. If not, there MUST be a
    /// number of follow up CONTINUATION frames that send the rest of the
    /// header data.
    pub fn is_headers_end(&self) -> bool {
        self.flags.is_set(HeadersFlag::EndHeaders)
    }

    /// Returns whther this frame ends the stream it is associated with.
    pub fn is_end_of_stream(&self) -> bool {
        self.flags.is_set(HeadersFlag::EndStream)
    }

    /// Sets the padding length for the frame, as well as the corresponding
    /// Padded flag.
    pub fn set_padding(&mut self, padding_len: u8) {
        self.set_flag(HeadersFlag::Padded);
        self.padding_len = padding_len;
    }

    /// Returns the length of the payload of the current frame, including any
    /// possible padding in the number of bytes.
    fn payload_len(&self) -> u32 {
        let padding = if self.flags.is_set(HeadersFlag::Padded) {
            1 + self.padding_len as u32
        } else {
            0
        };
        let priority = if self.flags.is_set(HeadersFlag::Priority) {
            5
        } else {
            0
        };

        self.header_fragment.len() as u32 + priority + padding
    }

    pub fn header_fragment(&self) -> &[u8] {
        &self.header_fragment
    }

    /// Sets the given flag for the frame.
    pub fn set_flag(&mut self, flag: HeadersFlag) {
        self.flags.set(flag);
    }
}

impl Frame for HeadersFrame {
    /// The type that represents the flags that the particular `Frame` can take.
    /// This makes sure that only valid `Flag`s are used with each `Frame`.
    type FlagType = HeadersFlag;

    /// Creates a new `HeadersFrame` with the given `RawFrame` (i.e. header and
    /// payload), if possible.
    ///
    /// # Returns
    ///
    /// `None` if a valid `HeadersFrame` cannot be constructed from the given
    /// `RawFrame`. The stream ID *must not* be 0.
    ///
    /// Otherwise, returns a newly constructed `HeadersFrame`.
    fn from_raw(raw_frame: &RawFrame) -> ParseFrameResult<HeadersFrame> {
        // Unpack the header
        let FrameHeader {
            payload_len,
            frame_type,
            flags,
            stream_id,
        } = raw_frame.header();
        // Check that the frame type is correct for this frame implementation
        if frame_type != HEADERS_FRAME_TYPE {
            return Err(ParseFrameError::InternalError);
        }
        // Check that the length given in the header matches the payload
        // length; if not, something went wrong and we do not consider this a
        // valid frame.
        if (payload_len as usize) != raw_frame.payload().len() {
            return Err(ParseFrameError::InternalError);
        }
        // Check that the HEADERS frame is not associated to stream 0
        if stream_id == 0 {
            return Err(ParseFrameError::StreamIdMustBeNonZero);
        }

        let flags = Flags::new(flags);

        // First, we get a slice containing the actual payload, depending on if
        // the frame is padded.
        let padded = flags.is_set(HeadersFlag::Padded);

        let (actual, pad_len) = parse_padded_payload(raw_frame.payload(), padded)?;

        // From the actual payload we extract the stream dependency info, if
        // the appropriate flag is set.
        let priority = flags.is_set(HeadersFlag::Priority);
        let (data, stream_dep) = if priority {
            let dep = StreamDependency::parse(&actual[..5]);
            if dep.stream_id == stream_id {
                // 5.3.1
                // A stream cannot depend on itself.  An endpoint MUST treat this as a
                // stream error (Section 5.4.2) of type PROTOCOL_ERROR.
                return Err(ParseFrameError::StreamDependencyOnItself(stream_id));
            }
            (actual.slice(5..), Some(dep))
        } else {
            (actual, None)
        };

        Ok(HeadersFrame {
            header_fragment: data,
            stream_id,
            stream_dep,
            padding_len: pad_len,
            flags,
        })
    }

    /// Tests if the given flag is set for the frame.
    fn flags(&self) -> Flags<HeadersFlag> {
        self.flags
    }

    /// Returns the `StreamId` of the stream to which the frame is associated.
    fn get_stream_id(&self) -> StreamId {
        self.stream_id
    }

    /// Returns a `FrameHeader` based on the current state of the `Frame`.
    fn get_header(&self) -> FrameHeader {
        FrameHeader {
            payload_len: self.payload_len(),
            frame_type: HEADERS_FRAME_TYPE,
            flags: self.flags.0,
            stream_id: self.stream_id,
        }
    }
}

impl FrameIR for HeadersFrame {
    fn serialize_into(self, b: &mut WriteBuffer) {
        b.write_header(self.get_header());
        let padded = self.flags.is_set(HeadersFlag::Padded);
        if padded {
            b.extend_from_slice(&[self.padding_len]);
        }
        // The stream dependency fields follow, if the priority flag is set
        if self.flags.is_set(HeadersFlag::Priority) {
            let dep_buf = match self.stream_dep {
                Some(ref dep) => dep.serialize(),
                None => panic!("Priority flag set, but no dependency information given"),
            };
            b.extend_from_slice(&dep_buf);
        }
        // Now the actual headers fragment
        b.extend_from_bytes(self.header_fragment);
        // Finally, add the trailing padding, if required
        if padded {
            b.write_padding(self.padding_len);
        }
    }
}

#[derive(Debug, Clone)]
pub struct HeadersDecodedFrame {
    /// The set of flags for the frame, packed into a single byte.
    pub flags: Flags<HeadersFlag>,
    /// The ID of the stream with which this frame is associated
    pub stream_id: StreamId,
    /// The header fragment bytes stored within the frame.
    pub headers: Headers,
    /// The stream dependency information, if any.
    pub stream_dep: Option<StreamDependency>,
    /// The length of the padding, if any.
    pub padding_len: u8,
}

impl HeadersDecodedFrame {
    /// Returns whther this frame ends the stream it is associated with.
    pub fn is_end_of_stream(&self) -> bool {
        self.flags.is_set(HeadersFlag::EndStream)
    }

    pub fn get_stream_id(&self) -> StreamId {
        self.stream_id
    }
}

/// Encoder headers into multiple frame without additional allocations
pub struct HeadersMultiFrame<'a> {
    /// The set of flags for the frame, packed into a single byte.
    pub flags: Flags<HeadersFlag>,
    /// The ID of the stream with which this frame is associated
    pub stream_id: StreamId,
    /// The header fragment bytes stored within the frame.
    pub headers: Headers,
    /// The stream dependency information, if any.
    pub stream_dep: Option<StreamDependency>,
    /// The length of the padding, if any.
    pub padding_len: u8,

    // state
    pub encoder: &'a mut hpack::Encoder,
    pub max_frame_size: u32,
}

impl<'a> fmt::Debug for HeadersMultiFrame<'a> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("HeadersMultiFrame")
            .field("flags", &self.flags)
            .field("stream_id", &self.stream_id)
            .field("headers", &self.headers)
            .field("stream_id", &self.stream_id)
            .field("padding_len", &self.padding_len)
            .field("max_frame_size", &self.max_frame_size)
            .finish()
    }
}

enum HeadersFrameType {
    Headers,
    Continuation,
}

impl HeadersFrameType {
    fn frame_type(&self) -> HttpFrameType {
        match self {
            HeadersFrameType::Headers => HttpFrameType::Headers,
            HeadersFrameType::Continuation => HttpFrameType::Continuation,
        }
    }

    /// Make HEADERS or CONTINUATION flags from HEADERS flags
    fn make_flags(&self, header_flags: Flags<HeadersFlag>, last: bool) -> u8 {
        assert!(!header_flags.is_set(HeadersFlag::EndHeaders));
        match self {
            HeadersFrameType::Headers => {
                match last {
                    true => header_flags.with(HeadersFlag::EndHeaders),
                    false => header_flags,
                }
                .0
            }
            HeadersFrameType::Continuation => match last {
                true => ContinuationFlag::EndHeaders.bitmask(),
                false => 0,
            },
        }
    }
}

struct EncodeBufForHeadersMultiFrame<'a> {
    current_frame_type: HeadersFrameType,
    current_frame_offset: usize,
    stream_id: StreamId,
    flags: Flags<HeadersFlag>,
    builder: WriteBufferTailVec<'a>,
    max_frame_size: u32,
}

impl<'a> EncodeBufForHeadersMultiFrame<'a> {
    fn open_frame(&mut self) {
        self.current_frame_offset = self.builder.remaining();
        // Length is not known at the moment so write an empty head
        // It will be patched later in `finish_frame`.
        // Can be optimized a little by writing all fields except length here.
        self.builder.extend_from_slice(&pack_header(&FrameHeader {
            payload_len: 0,
            frame_type: 0,
            flags: 0,
            stream_id: 0,
        }));
    }

    fn finish_frame(&mut self, last: bool) {
        let frame_length = (self.builder.remaining() - self.current_frame_offset) as u32;
        debug_assert!(frame_length >= FRAME_HEADER_LEN as u32);
        let length = frame_length - FRAME_HEADER_LEN as u32;
        self.builder.patch_buf(
            self.current_frame_offset,
            &pack_header(&FrameHeader {
                payload_len: length,
                frame_type: self.current_frame_type.frame_type().frame_type(),
                flags: self.current_frame_type.make_flags(self.flags, last),
                stream_id: self.stream_id,
            }),
        );
    }

    /// How much payload can be written into the current frame.
    fn rem_in_current_frame(&self) -> usize {
        let current_frame_len = self.builder.remaining() - self.current_frame_offset;
        debug_assert!(current_frame_len >= FRAME_HEADER_LEN);
        let current_frame_payload_len = current_frame_len - FRAME_HEADER_LEN;
        debug_assert!(current_frame_payload_len <= self.max_frame_size as usize);
        self.max_frame_size as usize - current_frame_payload_len
    }
}

impl<'a> EncodeBuf for EncodeBufForHeadersMultiFrame<'a> {
    fn write_all(&mut self, mut bytes: &[u8]) {
        loop {
            let copy_here = cmp::min(bytes.len(), self.rem_in_current_frame());
            self.builder.extend_from_slice(&bytes[..copy_here]);
            bytes = &bytes[copy_here..];

            if bytes.is_empty() {
                return;
            }

            self.finish_frame(false);
            self.open_frame();
            self.current_frame_type = HeadersFrameType::Continuation;
        }
    }

    fn reserve(&mut self, additional: usize) {
        // TODO: reserve better if spans frame boundaries
        self.builder.reserve(additional);
    }
}

impl<'a> FrameIR for HeadersMultiFrame<'a> {
    fn serialize_into(self, builder: &mut WriteBuffer) {
        assert!(!self.flags.is_set(HeadersFlag::EndHeaders));

        let tail_vec = builder.tail_vec();

        let mut buf = EncodeBufForHeadersMultiFrame {
            flags: self.flags,
            stream_id: self.stream_id,
            current_frame_type: HeadersFrameType::Headers,
            current_frame_offset: tail_vec.remaining(),
            builder: tail_vec,
            max_frame_size: self.max_frame_size,
        };

        buf.open_frame();

        let headers = self
            .headers
            .iter()
            .map(|h| (h.name().as_bytes(), h.value()));

        self.encoder.encode_into(headers, &mut buf);

        buf.finish_frame(true);
    }
}

#[cfg(test)]
mod tests {
    use super::{HeadersFlag, HeadersFrame, StreamDependency};
    use crate::hpack;
    use crate::solicit::frame::continuation::ContinuationFlag;
    use crate::solicit::frame::flags::Flags;
    use crate::solicit::frame::headers::HeadersMultiFrame;
    use crate::solicit::frame::tests::build_padded_frame_payload;
    use crate::solicit::frame::unpack_frames_for_test;
    use crate::solicit::frame::FrameHeader;
    use crate::solicit::frame::FrameIR;
    use crate::solicit::frame::HttpFrame;
    use crate::solicit::frame::{pack_header, Frame};
    use crate::solicit::tests::common::raw_frame_from_parts;
    use crate::Headers;

    /// Tests that a stream dependency structure can be correctly parsed by the
    /// `StreamDependency::parse` method.
    #[test]
    fn test_parse_stream_dependency() {
        {
            let buf = [0, 0, 0, 1, 5];

            let dep = StreamDependency::parse(&buf);

            assert_eq!(dep.stream_id, 1);
            assert_eq!(dep.weight, 5);
            // This one was not exclusive!
            assert!(!dep.is_exclusive)
        }
        {
            // Most significant bit set => is exclusive!
            let buf = [128, 0, 0, 1, 5];

            let dep = StreamDependency::parse(&buf);

            assert_eq!(dep.stream_id, 1);
            assert_eq!(dep.weight, 5);
            // This one was indeed exclusive!
            assert!(dep.is_exclusive)
        }
        {
            // Most significant bit set => is exclusive!
            let buf = [255, 255, 255, 255, 5];

            let dep = StreamDependency::parse(&buf);

            assert_eq!(dep.stream_id, (1 << 31) - 1);
            assert_eq!(dep.weight, 5);
            // This one was indeed exclusive!
            assert!(dep.is_exclusive);
        }
        {
            let buf = [127, 255, 255, 255, 5];

            let dep = StreamDependency::parse(&buf);

            assert_eq!(dep.stream_id, (1 << 31) - 1);
            assert_eq!(dep.weight, 5);
            // This one was not exclusive!
            assert!(!dep.is_exclusive);
        }
    }

    /// Tests that a stream dependency structure can be correctly serialized by
    /// the `StreamDependency::serialize` method.
    #[test]
    fn test_serialize_stream_dependency() {
        {
            let buf = [0, 0, 0, 1, 5];
            let dep = StreamDependency::new(1, 5, false);

            assert_eq!(buf, dep.serialize());
        }
        {
            // Most significant bit set => is exclusive!
            let buf = [128, 0, 0, 1, 5];
            let dep = StreamDependency::new(1, 5, true);

            assert_eq!(buf, dep.serialize());
        }
        {
            // Most significant bit set => is exclusive!
            let buf = [255, 255, 255, 255, 5];
            let dep = StreamDependency::new((1 << 31) - 1, 5, true);

            assert_eq!(buf, dep.serialize());
        }
        {
            let buf = [127, 255, 255, 255, 5];
            let dep = StreamDependency::new((1 << 31) - 1, 5, false);

            assert_eq!(buf, dep.serialize());
        }
    }

    /// Tests that a simple HEADERS frame is correctly parsed. The frame does
    /// not contain any padding nor priority information.
    #[test]
    fn test_headers_frame_parse_simple() {
        let data = b"123";
        let payload = data.to_vec();
        let header = FrameHeader::new(payload.len() as u32, 0x1, 0, 1);

        let raw = raw_frame_from_parts(header.clone(), payload.to_vec());
        let frame: HeadersFrame = Frame::from_raw(&raw).unwrap();

        assert_eq!(frame.header_fragment(), &data[..]);
        assert_eq!(frame.flags.0, 0);
        assert_eq!(frame.get_stream_id(), 1);
        assert!(frame.stream_dep.is_none());
        assert_eq!(0, frame.padding_len);
    }

    /// Tests that a HEADERS frame with padding is correctly parsed.
    #[test]
    fn test_headers_frame_parse_with_padding() {
        let data = b"123";
        let payload = build_padded_frame_payload(data, 6);
        let header = FrameHeader::new(payload.len() as u32, 0x1, 0x08, 1);

        let raw = raw_frame_from_parts(header.clone(), payload.to_vec());
        let frame: HeadersFrame = Frame::from_raw(&raw).unwrap();

        assert_eq!(frame.header_fragment(), &data[..]);
        assert_eq!(frame.flags.0, 8);
        assert_eq!(frame.get_stream_id(), 1);
        assert!(frame.stream_dep.is_none());
        assert_eq!(6, frame.padding_len);
    }

    /// Tests that a HEADERS frame with the priority flag (and necessary fields)
    /// is correctly parsed.
    #[test]
    fn test_headers_frame_parse_with_priority() {
        let data = b"123";
        let dep = StreamDependency::new(0, 5, true);
        let payload = {
            let mut buf: Vec<u8> = Vec::new();
            buf.extend(dep.serialize().to_vec().into_iter());
            buf.extend(data.to_vec().into_iter());

            buf
        };
        let header = FrameHeader::new(payload.len() as u32, 0x1, 0x20, 1);

        let raw = raw_frame_from_parts(header.clone(), payload.to_vec());
        let frame: HeadersFrame = Frame::from_raw(&raw).unwrap();

        assert_eq!(frame.header_fragment(), &data[..]);
        assert_eq!(frame.flags.0, 0x20);
        assert_eq!(frame.get_stream_id(), 1);
        assert_eq!(frame.stream_dep.unwrap(), dep);
        assert_eq!(0, frame.padding_len);
    }

    /// Tests that a HEADERS frame with both padding and priority gets
    /// correctly parsed.
    #[test]
    fn test_headers_frame_parse_padding_and_priority() {
        let data = b"123";
        let dep = StreamDependency::new(0, 5, true);
        let full = {
            let mut buf: Vec<u8> = Vec::new();
            buf.extend(dep.serialize().to_vec().into_iter());
            buf.extend(data.to_vec().into_iter());

            buf
        };
        let payload = build_padded_frame_payload(&full, 4);
        let header = FrameHeader::new(payload.len() as u32, 0x1, 0x20 | 0x8, 1);

        let raw = raw_frame_from_parts(header.clone(), payload.to_vec());
        let frame: HeadersFrame = Frame::from_raw(&raw).unwrap();

        assert_eq!(frame.header_fragment(), &data[..]);
        assert_eq!(frame.flags.0, 0x20 | 0x8);
        assert_eq!(frame.get_stream_id(), 1);
        assert_eq!(frame.stream_dep.unwrap(), dep);
        assert_eq!(4, frame.padding_len);
    }

    /// Tests that a HEADERS with stream ID 0 is considered invalid.
    #[test]
    fn test_headers_frame_parse_invalid_stream_id() {
        let data = b"123";
        let payload = data.to_vec();
        let header = FrameHeader::new(payload.len() as u32, 0x1, 0, 0);

        let raw = raw_frame_from_parts(header, payload);
        let frame = HeadersFrame::from_raw(&raw);

        assert!(frame.is_err());
    }

    /// Tests that the `HeadersFrame::parse` method considers any frame with
    /// a frame ID other than 1 in the frame header invalid.
    #[test]
    fn test_headers_frame_parse_invalid_type() {
        let data = b"123";
        let payload = data.to_vec();
        let header = FrameHeader::new(payload.len() as u32, 0x2, 0, 1);

        let raw = raw_frame_from_parts(header, payload);
        let frame = HeadersFrame::from_raw(&raw);

        assert!(frame.is_err());
    }

    /// Tests that a simple HEADERS frame (no padding, no priority) gets
    /// correctly serialized.
    #[test]
    fn test_headers_frame_serialize_simple() {
        let data = b"123";
        let payload = data.to_vec();
        let header = FrameHeader::new(payload.len() as u32, 0x1, 0, 1);
        let expected = {
            let headers = pack_header(&header);
            let mut res: Vec<u8> = Vec::new();
            res.extend(headers.to_vec().into_iter());
            res.extend(payload.into_iter());

            res
        };
        let frame = HeadersFrame::new_conv(data.to_vec(), 1);

        let actual = frame.serialize_into_vec();

        assert_eq!(expected, actual);
    }

    /// Tests that a HEADERS frame with padding is correctly serialized.
    #[test]
    fn test_headers_frame_serialize_with_padding() {
        let data = b"123";
        let payload = build_padded_frame_payload(data, 6);
        let header = FrameHeader::new(payload.len() as u32, 0x1, 0x08, 1);
        let expected = {
            let headers = pack_header(&header);
            let mut res: Vec<u8> = Vec::new();
            res.extend(headers.to_vec().into_iter());
            res.extend(payload.into_iter());

            res
        };
        let mut frame = HeadersFrame::new_conv(data.to_vec(), 1);
        frame.set_padding(6);

        let actual = frame.serialize_into_vec();

        assert_eq!(expected, actual);
    }

    /// Tests that a HEADERS frame with priority gets correctly serialized.
    #[test]
    fn test_headers_frame_serialize_with_priority() {
        let data = b"123";
        let dep = StreamDependency::new(0, 5, true);
        let payload = {
            let mut buf: Vec<u8> = Vec::new();
            buf.extend(dep.serialize().to_vec().into_iter());
            buf.extend(data.to_vec().into_iter());

            buf
        };
        let header = FrameHeader::new(payload.len() as u32, 0x1, 0x20, 1);
        let expected = {
            let headers = pack_header(&header);
            let mut res: Vec<u8> = Vec::new();
            res.extend(headers.to_vec().into_iter());
            res.extend(payload.into_iter());

            res
        };
        let frame = HeadersFrame::with_dependency(data.to_vec(), 1, dep.clone());

        let actual = frame.serialize_into_vec();

        assert_eq!(expected, actual);
    }

    /// Tests that a HEADERS frame with both padding and a priority gets correctly
    /// serialized.
    #[test]
    fn test_headers_frame_serialize_padding_and_priority() {
        let data = b"123";
        let dep = StreamDependency::new(0, 5, true);
        let full = {
            let mut buf: Vec<u8> = Vec::new();
            buf.extend(dep.serialize().to_vec().into_iter());
            buf.extend(data.to_vec().into_iter());

            buf
        };
        let payload = build_padded_frame_payload(&full, 4);
        let header = FrameHeader::new(payload.len() as u32, 0x1, 0x20 | 0x8, 1);
        let expected = {
            let headers = pack_header(&header);
            let mut res: Vec<u8> = Vec::new();
            res.extend(headers.to_vec().into_iter());
            res.extend(payload.into_iter());

            res
        };
        let mut frame = HeadersFrame::with_dependency(data.to_vec(), 1, dep.clone());
        frame.set_padding(4);

        let actual = frame.serialize_into_vec();

        assert_eq!(expected, actual);
    }

    /// Tests that the `HeadersFrame::is_headers_end` method returns the correct
    /// value depending on the `EndHeaders` flag being set or not.
    #[test]
    fn test_headers_frame_is_headers_end() {
        let mut frame = HeadersFrame::new_conv(Vec::new(), 1);
        assert!(!frame.is_headers_end());

        frame.set_flag(HeadersFlag::EndHeaders);
        assert!(frame.is_headers_end());
    }

    #[test]
    fn test_headers_multi_frame() {
        let mut encoder = hpack::Encoder::new();

        let mut headers = Headers::ok_200();
        for i in 0..1000 {
            headers.add(format!("h-{}", i), format!("v-{}", i))
        }

        let max_frame_size = 1000;

        let serialized = HeadersMultiFrame {
            flags: Flags::new(0).with(HeadersFlag::EndStream),
            stream_id: 2,
            headers,
            stream_dep: None,
            padding_len: 0,
            encoder: &mut encoder,
            max_frame_size,
        }
        .serialize_into_vec();

        let frames = unpack_frames_for_test(&serialized);
        assert!(frames.len() > 2);
        for (i, f) in frames.iter().enumerate() {
            match f {
                HttpFrame::Headers(h) => {
                    assert_eq!(0, i);
                    assert_eq!(max_frame_size as usize, h.header_fragment.len());
                    assert_eq!(Flags::new(0).with(HeadersFlag::EndStream), h.flags);
                }
                HttpFrame::Continuation(h) => {
                    assert_ne!(0, i);
                    let last = i == frames.len() - 1;
                    if !last {
                        assert_eq!(max_frame_size as usize, h.header_fragment.len());
                        assert_eq!(Flags::new(0), h.flags);
                    } else {
                        assert_eq!(Flags::new(0).with(ContinuationFlag::EndHeaders), h.flags);
                    }
                }
                _ => panic!("wrong frame type"),
            }
        }
    }
}
