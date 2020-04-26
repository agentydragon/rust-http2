//! The module contains the implementation of the `DATA` frame and associated flags.

use crate::solicit::frame::flags::*;
use crate::solicit::frame::parse_padded_payload;
use crate::solicit::frame::Frame;
use crate::solicit::frame::FrameBuilder;
use crate::solicit::frame::FrameHeader;
use crate::solicit::frame::FrameIR;
use crate::solicit::frame::ParseFrameError;
use crate::solicit::frame::ParseFrameResult;
use crate::solicit::frame::RawFrame;

use crate::codec::write_buffer::WriteBuffer;
use crate::misc::BsDebug;
use crate::solicit::stream_id::StreamId;
use bytes::Bytes;
use std::fmt;

pub const DATA_FRAME_TYPE: u8 = 0x0;

/// An enum representing the flags that a `DataFrame` can have.
/// The integer representation associated to each variant is that flag's
/// bitmask.
///
/// HTTP/2 spec, section 6.1.
#[derive(Clone, PartialEq, Debug, Copy)]
pub enum DataFlag {
    /// Flag
    EndStream = 0x1,
    /// Flag
    Padded = 0x8,
}

impl Flag for DataFlag {
    #[inline]
    fn bitmask(&self) -> u8 {
        *self as u8
    }

    fn flags() -> &'static [Self] {
        static FLAGS: &'static [DataFlag] = &[DataFlag::EndStream, DataFlag::Padded];
        FLAGS
    }
}

/// A struct representing the DATA frames of HTTP/2, as defined in the HTTP/2
/// spec, section 6.1.
#[derive(PartialEq, Clone)]
pub struct DataFrame {
    /// Represents the flags currently set on the `DataFrame`, packed into a
    /// single byte.
    flags: Flags<DataFlag>,
    /// The ID of the stream with which the frame is associated.
    pub stream_id: StreamId,
    /// The data found in the frame as an opaque byte sequence. It never
    /// includes padding bytes.
    pub data: Bytes,
    /// The length of the padding applied to the data. Since the spec defines
    /// that the padding length is at most an unsigned integer value, we also
    /// keep a `u8`, instead of a `usize`.
    padding_len: u8,
}

impl fmt::Debug for DataFrame {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("DataFrame")
            .field("flags", &self.flags)
            .field("stream_id", &self.stream_id)
            .field("data", &BsDebug(&self.data[..]))
            .field("padding_len", &self.padding_len)
            .finish()
    }
}

impl DataFrame {
    /// Creates a new empty `DataFrame`, associated to the stream with the
    /// given ID.
    pub fn new(stream_id: StreamId) -> DataFrame {
        DataFrame {
            stream_id: stream_id,
            // All flags unset by default
            flags: Flags::default(),
            // No data stored in the frame yet
            data: Bytes::new(),
            // No padding
            padding_len: 0,
        }
    }

    /// Creates a new `DataFrame` with the given `DataChunk`.
    ///
    /// The chunk can be any type that can be converted into a `DataChunk` instance and, as such,
    /// can either pass ownership of the buffer to the DataFrame or provide a temporary borrow.
    pub fn with_data(stream_id: StreamId, data: Bytes) -> DataFrame {
        DataFrame {
            stream_id: stream_id,
            flags: Flags::default(),
            data: data,
            padding_len: 0,
        }
    }

    /// Returns `true` if the DATA frame is padded, otherwise false.
    pub fn is_padded(&self) -> bool {
        self.flags.is_set(DataFlag::Padded)
    }

    /// Returns whther this frame ends the stream it is associated with.
    pub fn is_end_of_stream(&self) -> bool {
        self.flags.is_set(DataFlag::EndStream)
    }

    /// Sets the number of bytes that should be used as padding for this
    /// frame.
    pub fn set_padding(&mut self, pad_len: u8) {
        self.set_flag(DataFlag::Padded);
        self.padding_len = pad_len;
    }

    /// Returns the total length of the payload, taking into account possible
    /// padding.
    pub fn payload_len(&self) -> u32 {
        if self.is_padded() {
            1 + (self.data.len() as u32) + (self.padding_len as u32)
        } else {
            // Downcasting here is all right, because the HTTP/2 frames cannot
            // have a length larger than a 32 bit unsigned integer.
            self.data.len() as u32
        }
    }

    /// Parses the given slice as a DATA frame's payload. Depending on the
    /// `padded` flag, it will treat the given bytes as a data frame with
    /// padding or without.
    ///
    /// # Returns
    ///
    /// A tuple wrapped in the `Some` variant, representing the true data and
    /// the original padding length.
    /// If there was no padding, returns `None` for the second tuple member.
    ///
    /// If the payload was invalid for a DATA frame, returns `None`
    fn parse_payload(payload: Bytes, padded: bool) -> ParseFrameResult<(Bytes, u8)> {
        parse_padded_payload(payload, padded)
    }

    /// Sets the given flag for the frame.
    pub fn set_flag(&mut self, flag: DataFlag) {
        self.flags.0 |= flag.bitmask();
    }
}

impl Frame for DataFrame {
    type FlagType = DataFlag;

    /// Creates a new `DataFrame` from the given `RawFrame` (i.e. header and
    /// payload), if possible.  Returns `None` if a valid `DataFrame` cannot be
    /// constructed from the given `RawFrame`.
    fn from_raw(raw_frame: &RawFrame) -> ParseFrameResult<DataFrame> {
        // Unpack the header
        let FrameHeader {
            payload_len,
            frame_type,
            flags,
            stream_id,
        } = raw_frame.header();
        // Check that the frame type is correct for this frame implementation
        if frame_type != DATA_FRAME_TYPE {
            return Err(ParseFrameError::InternalError);
        }
        // Check that the length given in the header matches the payload
        // length; if not, something went wrong and we do not consider this a
        // valid frame.
        if (payload_len as usize) != raw_frame.payload().len() {
            return Err(ParseFrameError::InternalError);
        }
        // A DATA frame cannot be associated to the connection itself.
        if stream_id == 0x0 {
            return Err(ParseFrameError::StreamIdMustBeNonZero);
        }
        // No validation is required for the flags, since according to the spec,
        // unknown flags MUST be ignored.
        // Everything has been validated so far: try to extract the data from
        // the payload.
        let padded = (flags & DataFlag::Padded.bitmask()) != 0;
        let (data, padding_len) = DataFrame::parse_payload(raw_frame.payload(), padded)?;
        // The data got extracted (from a padded frame)
        Ok(DataFrame {
            stream_id,
            flags: Flags::new(flags),
            data,
            padding_len,
        })
    }

    /// Tests if the given flag is set for the frame.
    fn flags(&self) -> Flags<DataFlag> {
        self.flags
    }

    /// Returns the `StreamId` of the stream to which the frame is associated.
    fn get_stream_id(&self) -> StreamId {
        self.stream_id
    }

    /// Returns a `FrameHeader` based on the current state of the frame.
    fn get_header(&self) -> FrameHeader {
        FrameHeader {
            payload_len: self.payload_len(),
            frame_type: DATA_FRAME_TYPE,
            flags: self.flags.0,
            stream_id: self.stream_id,
        }
    }
}

impl FrameIR for DataFrame {
    fn serialize_into(self, b: &mut WriteBuffer) {
        b.write_header(self.get_header());
        if self.is_padded() {
            let pad_len: u8 = self.padding_len;
            b.extend_from_slice(&[pad_len]);
            b.extend_from_bytes(self.data);
            b.write_padding(pad_len);
        } else {
            b.extend_from_bytes(self.data);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::DataFlag;
    use super::DataFrame;
    use crate::solicit::frame::pack_header;
    use crate::solicit::frame::tests::build_padded_frame_payload;
    use crate::solicit::frame::Frame;
    use crate::solicit::frame::FrameHeader;
    use crate::solicit::frame::FrameIR;
    use crate::solicit::tests::common::raw_frame_from_parts;
    use bytes::Bytes;

    /// Tests that the `DataFrame` struct correctly interprets a DATA frame
    /// with no padding set.
    #[test]
    fn test_data_frame_parse_no_padding() {
        let data = b"asdf";
        let payload = data.to_vec();
        // A header with the flag indicating no padding
        let header = FrameHeader::new(payload.len() as u32, 0u8, 0u8, 1u32);

        let raw = raw_frame_from_parts(header.clone(), payload.to_vec());
        let frame: DataFrame = Frame::from_raw(&raw).unwrap();

        // The frame correctly returns the data?
        assert_eq!(&frame.data[..], &data[..]);
        // ...and the headers?
        assert_eq!(frame.get_header(), header);
    }

    /// Tests that the `DataFrame` struct correctly knows when it represents the end of the
    /// corresponding stream.
    #[test]
    fn test_data_frame_is_end_of_stream() {
        let mut frame = DataFrame::new(1);
        assert!(!frame.is_end_of_stream());
        frame.set_flag(DataFlag::EndStream);
        assert!(frame.is_end_of_stream());
    }

    /// Tests that the `DataFrame` struct correctly interprets a DATA frame
    /// with a number of padding bytes set.
    #[test]
    fn test_data_frame_padded() {
        let data = b"asdf";
        let payload = build_padded_frame_payload(data, 5);
        // A header with the flag indicating padding
        let header = FrameHeader::new(payload.len() as u32, 0u8, 8u8, 1u32);

        let raw = raw_frame_from_parts(header.clone(), payload.to_vec());
        let frame: DataFrame = Frame::from_raw(&raw).unwrap();

        // The frame correctly returns the data?
        assert_eq!(&frame.data[..], &data[..]);
        // ...and the headers?
        assert_eq!(frame.get_header(), header);
    }

    /// Tests that a DATA frame with a zero-length payload is still considered
    /// valid.
    ///
    /// There doesn't seem to be anything in the spec that would make it invalid.
    /// The spec says that frames are considered invalid if their size is too
    /// small to contain all the mandatory parts of the frame of a particular
    /// type. Since the DATA frame does not have any mandatory fields (of size
    /// greater than 1), a zero-len payload should be all right.
    #[test]
    fn test_data_frame_zero_len_payload() {
        let data = b"";
        let payload = data.to_vec();
        // A header with the flag indicating no padding
        let header = FrameHeader::new(payload.len() as u32, 0u8, 0u8, 1u32);

        let raw = raw_frame_from_parts(header.clone(), payload.to_vec());
        let frame: DataFrame = Frame::from_raw(&raw).unwrap();

        // The frame correctly returns the data?
        assert_eq!(&frame.data[..], &data[..]);
        // ...and the headers?
        assert_eq!(frame.get_header(), header);
    }

    /// Tests that the `DataFrame` struct correctly handles the case where the
    /// padding is invalid: the size of the padding given is greater than or
    /// equal to the total size of the frame.
    #[test]
    fn test_data_frame_padding_invalid() {
        let payload = vec![5, b'a', b's', b'd', b'f'];
        // A header with the flag indicating padding
        let header = FrameHeader::new(payload.len() as u32, 0u8, 8u8, 1u32);

        let raw = raw_frame_from_parts(header, payload);
        let frame = DataFrame::from_raw(&raw);

        // The frame was not even created since the raw bytes are invalid
        assert!(frame.is_err())
    }

    /// Tests that if a frame that should be parsed has a stream ID of 0, it is
    /// not considered a valid DATA frame.
    #[test]
    fn test_data_frame_stream_zero() {
        let data = b"asdf";
        let payload = data.to_vec();
        // Stream 0
        let header = FrameHeader::new(payload.len() as u32, 0u8, 0u8, 0u32);

        let raw = raw_frame_from_parts(header, payload.to_vec());
        let frame = DataFrame::from_raw(&raw);

        // The frame is not valid.
        assert!(frame.is_err());
    }

    /// Tests that the `DataFrame` struct correctly interprets a DATA frame
    /// with no padding and no data.
    #[test]
    fn test_data_frame_no_padding_empty() {
        let payload = [];
        let header = FrameHeader::new(payload.len() as u32, 0u8, 0u8, 1u32);

        let raw = raw_frame_from_parts(header.clone(), payload.to_vec());
        let frame: DataFrame = Frame::from_raw(&raw).unwrap();

        // The frame correctly returns the data -- i.e. an empty array?
        assert_eq!(&frame.data[..], &[][..]);
        // ...and the headers?
        assert_eq!(frame.get_header(), header);
    }

    /// Tests that the `DataFrame` struct correctly interprets a DATA frame
    /// with padding, but an empty payload.
    #[test]
    fn test_data_frame_padding_empty_payload() {
        let payload = vec![];
        let header = FrameHeader::new(payload.len() as u32, 0u8, 8u8, 1u32);

        let raw = raw_frame_from_parts(header, payload);
        let frame = DataFrame::from_raw(&raw);

        // In this case, we cannot receive a frame, since the payload did not
        // contain even the first byte, necessary to find the padding length.
        assert!(frame.is_err());
    }

    /// Tests that the `DataFrame` struct correctly interprets a DATA frame
    /// with padding of size 0.
    #[test]
    fn test_data_frame_null_padding() {
        let data = b"test string";
        let payload = build_padded_frame_payload(data, 0);
        // A header with the flag indicating padding
        let header = FrameHeader::new(payload.len() as u32, 0u8, 8u8, 1u32);

        let raw = raw_frame_from_parts(header.clone(), payload.to_vec());
        let frame: DataFrame = Frame::from_raw(&raw).unwrap();

        // The frame correctly returns the data?
        assert_eq!(&frame.data[..], &data[..]);
        // ...and the headers?
        assert_eq!(frame.get_header(), header);
    }

    /// Tests that the `DataFrame` struct correctly handles the situation
    /// where the header does not contain a frame type corresponding to the
    /// DATA frame type.
    #[test]
    fn test_data_frame_invalid_type() {
        let data = b"dummy";
        let payload = build_padded_frame_payload(data, 0);
        // The header has an invalid type (0x1 instead of 0x0).
        let header = FrameHeader::new(payload.len() as u32, 1u8, 8u8, 1u32);

        let raw = raw_frame_from_parts(header, payload);
        let frame = DataFrame::from_raw(&raw);

        assert!(frame.is_err());
    }

    /// Tests that `DataFrame`s get correctly serialized when created with no
    /// padding and with no data.
    #[test]
    fn test_data_frame_serialize_no_padding_empty() {
        let frame = DataFrame::new(1);
        let expected = {
            let headers = pack_header(&FrameHeader::new(0, 0, 0, 1));
            let mut res: Vec<u8> = Vec::new();
            res.extend(headers.to_vec());

            res
        };

        let serialized = frame.serialize_into_vec();

        assert_eq!(serialized, expected);
    }

    /// Tests that `DataFrame`s get correctly serialized when created with no
    /// padding and with some amount of data.
    #[test]
    fn test_data_frame_serialize_no_padding() {
        let data = vec![1, 2, 3, 4, 5, 100];
        let frame = DataFrame::with_data_conv(1, Bytes::copy_from_slice(&data[..]));
        let expected = {
            let headers = pack_header(&FrameHeader::new(6, 0, 0, 1));
            let mut res: Vec<u8> = Vec::new();
            res.extend(headers.to_vec());
            res.extend(data.clone());

            res
        };

        let serialized = frame.serialize_into_vec();

        assert_eq!(serialized, expected);
    }

    /// Tests that `DataFrame`s get correctly serialized when created with
    /// some amount of padding and some data.
    #[test]
    fn test_data_frame_serialize_padding() {
        let data = vec![1, 2, 3, 4, 5, 100];
        let mut frame = DataFrame::with_data_conv(1, Bytes::copy_from_slice(&data[..]));
        frame.set_padding(5);
        let expected = {
            let headers = pack_header(&FrameHeader::new(6 + 1 + 5, 0, 8, 1));
            let mut res: Vec<u8> = Vec::new();
            // Headers
            res.extend(headers.to_vec());
            // Padding len
            res.push(5);
            // Data
            res.extend(data.clone());
            // Actual padding
            for _ in 0..5 {
                res.push(0);
            }

            res
        };

        let serialized = frame.serialize_into_vec();

        assert_eq!(serialized, expected);
    }

    /// Tests that `DataFrame`s get correctly serialized when created with
    /// 0 padding. This is a distinct case from having *no padding*.
    #[test]
    fn test_data_frame_serialize_null_padding() {
        let data = vec![1, 2, 3, 4, 5, 100];
        let cloned = data.clone();
        let mut frame = DataFrame::with_data_conv(1, data);
        frame.set_flag(DataFlag::Padded);
        let expected = {
            let headers = pack_header(&FrameHeader::new(6 + 1, 0, 8, 1));
            let mut res: Vec<u8> = Vec::new();
            // Headers
            res.extend(headers.to_vec());
            // Padding len
            res.push(0);
            // Data
            res.extend(cloned);

            res
        };

        let serialized = frame.serialize_into_vec();

        assert_eq!(serialized, expected);
    }
}
