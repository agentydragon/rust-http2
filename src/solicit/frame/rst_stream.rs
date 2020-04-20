//! The module contains the implementation of the `RST_STREAM` frame.

use crate::solicit::frame::flags::*;
use crate::solicit::frame::Frame;
use crate::solicit::frame::FrameBuilder;
use crate::solicit::frame::FrameHeader;
use crate::solicit::frame::FrameIR;
use crate::solicit::frame::ParseFrameError;
use crate::solicit::frame::ParseFrameResult;
use crate::solicit::frame::RawFrame;

use crate::codec::write_buffer::WriteBuffer;
use crate::solicit::error_code::ErrorCodeOrUnknown;
use crate::solicit::stream_id::StreamId;
use crate::ErrorCode;

/// The total allowed size for the `RST_STREAM` frame payload.
pub const RST_STREAM_FRAME_LEN: u32 = 4;
/// The frame type of the `RST_STREAM` frame.
pub const RST_STREAM_FRAME_TYPE: u8 = 0x3;

/// The struct represents the `RST_STREAM` HTTP/2 frame.
#[derive(Clone, Debug, PartialEq)]
pub struct RstStreamFrame {
    error_code: ErrorCodeOrUnknown,
    pub stream_id: StreamId,
    flags: Flags<NoFlag>,
}

impl RstStreamFrame {
    /// Constructs a new `RstStreamFrame` with the given `ErrorCode`.
    pub fn new(stream_id: StreamId, error_code: ErrorCode) -> RstStreamFrame {
        RstStreamFrame {
            error_code: error_code.into(),
            stream_id: stream_id,
            flags: Flags::default(),
        }
    }

    /// Constructs a new `RstStreamFrame` that will use the given `raw_error_code` for its payload.
    pub fn with_raw_error_code(stream_id: StreamId, error_code: u32) -> RstStreamFrame {
        RstStreamFrame {
            error_code: ErrorCodeOrUnknown(error_code),
            stream_id,
            flags: Flags::default(),
        }
    }

    /// Returns the interpreted error code of the frame. Any unknown error codes are mapped into
    /// the `InternalError` variant of the enum.
    pub fn error_code(&self) -> ErrorCode {
        self.error_code.into()
    }

    /// Returns the original raw error code of the frame. If the code is unknown, it will not be
    /// changed.
    pub fn raw_error_code(&self) -> u32 {
        self.error_code.0
    }
}

impl Frame for RstStreamFrame {
    type FlagType = NoFlag;

    fn from_raw(raw_frame: &RawFrame) -> ParseFrameResult<Self> {
        let FrameHeader {
            payload_len,
            frame_type,
            flags,
            stream_id,
        } = raw_frame.header();
        if payload_len != RST_STREAM_FRAME_LEN {
            return Err(ParseFrameError::InternalError);
        }
        if frame_type != RST_STREAM_FRAME_TYPE {
            return Err(ParseFrameError::InternalError);
        }
        if stream_id == 0x0 {
            return Err(ParseFrameError::StreamIdMustBeNonZero);
        }

        let error = unpack_octets_4!(raw_frame.payload(), 0, u32);

        Ok(RstStreamFrame {
            error_code: ErrorCodeOrUnknown(error),
            stream_id,
            flags: Flags::new(flags),
        })
    }

    fn flags(&self) -> Flags<NoFlag> {
        self.flags
    }

    fn get_stream_id(&self) -> StreamId {
        self.stream_id
    }

    fn get_header(&self) -> FrameHeader {
        FrameHeader {
            payload_len: RST_STREAM_FRAME_LEN,
            frame_type: RST_STREAM_FRAME_TYPE,
            flags: self.flags.0,
            stream_id: self.stream_id,
        }
    }
}

impl FrameIR for RstStreamFrame {
    fn serialize_into(self, builder: &mut WriteBuffer) {
        builder.write_header(self.get_header());
        builder.write_u32(self.error_code.0);
    }
}

#[cfg(test)]
mod tests {
    use super::RstStreamFrame;

    use crate::solicit::frame::FrameIR;
    use crate::solicit::frame::{pack_header, Frame, FrameHeader};
    use crate::ErrorCode;

    /// A helper function that creates a new Vec containing the serialized representation of the
    /// given `FrameHeader` followed by the raw provided payload.
    fn prepare_frame_bytes(header: FrameHeader, payload: Vec<u8>) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend(pack_header(&header).to_vec());
        buf.extend(payload);
        buf
    }

    #[test]
    fn test_parse_valid() {
        let raw = prepare_frame_bytes(FrameHeader::new(4, 0x3, 0, 1), vec![0, 0, 0, 1]);
        let rst = RstStreamFrame::from_raw(&raw.into()).expect("Valid frame expected");
        assert_eq!(rst.error_code(), ErrorCode::ProtocolError);
        assert_eq!(rst.get_stream_id(), 1);
    }

    #[test]
    fn test_parse_valid_with_unknown_flags() {
        let raw = prepare_frame_bytes(FrameHeader::new(4, 0x3, 0x80, 1), vec![0, 0, 0, 1]);
        let rst = RstStreamFrame::from_raw(&raw.into()).expect("Valid frame expected");
        assert_eq!(rst.error_code(), ErrorCode::ProtocolError);
        assert_eq!(rst.get_stream_id(), 1);
        // The raw flag set is correctly reported from the header, but the parsed frame itself does
        // not know how to interpret them.
        assert_eq!(rst.get_header().flags, 0x80);
    }

    #[test]
    fn test_parse_unknown_error_code() {
        let raw = prepare_frame_bytes(FrameHeader::new(4, 0x3, 0x80, 1), vec![1, 0, 0, 1]);
        let rst = RstStreamFrame::from_raw(&raw.into()).expect("Valid frame expected");
        // Unknown error codes are considered equivalent to an internal error.
        assert_eq!(rst.error_code(), ErrorCode::InternalError);
        // ...but the frame still surfaces the original raw code, so that clients can handle that,
        // if they so choose.
        assert_eq!(rst.raw_error_code(), 0x01000001);
    }

    #[test]
    fn test_parse_invalid_stream_id() {
        let raw = prepare_frame_bytes(FrameHeader::new(4, 0x3, 0x80, 0), vec![0, 0, 0, 1]);
        assert!(RstStreamFrame::from_raw(&raw.into()).is_err());
    }

    #[test]
    fn test_parse_invalid_payload_size() {
        let raw = prepare_frame_bytes(FrameHeader::new(5, 0x3, 0x00, 2), vec![0, 0, 0, 1, 0]);
        assert!(RstStreamFrame::from_raw(&raw.into()).is_err());
    }

    #[test]
    fn test_parse_invalid_id() {
        let raw = prepare_frame_bytes(FrameHeader::new(4, 0x1, 0x00, 2), vec![0, 0, 0, 1, 0]);
        assert!(RstStreamFrame::from_raw(&raw.into()).is_err());
    }

    #[test]
    fn test_serialize_protocol_error() {
        let frame = RstStreamFrame::new(1, ErrorCode::ProtocolError);
        let raw = frame.serialize_into_vec();
        assert_eq!(
            raw,
            prepare_frame_bytes(FrameHeader::new(4, 0x3, 0, 1), vec![0, 0, 0, 1])
        );
    }

    #[test]
    fn test_serialize_stream_closed() {
        let frame = RstStreamFrame::new(2, ErrorCode::StreamClosed);
        let raw = frame.serialize_into_vec();
        assert_eq!(
            raw,
            prepare_frame_bytes(FrameHeader::new(4, 0x3, 0, 2), vec![0, 0, 0, 0x5])
        );
    }

    #[test]
    fn test_serialize_raw_error_code() {
        let frame = RstStreamFrame::with_raw_error_code(3, 1024);
        let raw = frame.serialize_into_vec();
        assert_eq!(
            raw,
            prepare_frame_bytes(FrameHeader::new(4, 0x3, 0, 3), vec![0, 0, 0x04, 0])
        );
    }

    #[test]
    fn test_partial_eq() {
        let frame1 = RstStreamFrame::with_raw_error_code(3, 1);
        let frame2 = RstStreamFrame::new(3, ErrorCode::ProtocolError);
        assert_eq!(frame1, frame2);
    }
}
