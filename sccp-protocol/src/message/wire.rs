//! SCCP frame encoding and incremental stream decoding.
//!
//! Construct an outbound [`Frame`] and call [`Frame::encode`]. For inbound TCP
//! data, retain one [`FrameDecoder`] per connection and feed each received
//! chunk to [`FrameDecoder::push`]; it handles both fragmented and coalesced
//! frames while enforcing [`MAX_FRAME_SIZE`]. Typed message decoding happens
//! after framing through the message enums in the parent module.

use std::fmt;
use std::io::Cursor;

use binrw::{BinRead, BinWrite};
use thiserror::Error;

use super::catalog::MessageId;
use super::catalog::MessageRoute;

/// Number of bytes in an SCCP frame header, including the message identifier.
pub const HEADER_SIZE: usize = 12;
/// Largest complete frame accepted or emitted by the framing layer.
pub const MAX_FRAME_SIZE: usize = 8192;

/// The fixed SCCP framing header. Keeping this separate from [`Frame`] lets
/// the streaming decoder inspect a header before the complete payload has
/// arrived, while still giving encoding and decoding one declarative layout.
#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
#[brw(little)]
struct WireHeader {
    wire_len: u32,
    protocol_version: u32,
    message_id: u32,
}

#[derive(Clone, Eq, PartialEq)]
/// One complete SCCP frame with an uninterpreted payload.
///
/// [`FrameDecoder`] produces frames from a byte stream. Call the appropriate
/// typed message decoder afterward to validate the payload contract.
pub struct Frame {
    pub protocol_version: u32,
    pub message_id: u32,
    pub payload: Vec<u8>,
}

impl fmt::Debug for Frame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Frame")
            .field("protocol_version", &self.protocol_version)
            .field("message_id", &format_args!("0x{:04x}", self.message_id))
            .field("payload_len", &self.payload.len())
            .finish()
    }
}

impl Frame {
    /// Creates a frame without validating its payload or total encoded size.
    ///
    /// Size validation occurs in [`Frame::encode`] or in typed message codecs.
    pub fn new(protocol_version: u32, message_id: u32, payload: Vec<u8>) -> Self {
        Self {
            protocol_version,
            message_id,
            payload,
        }
    }

    /// SCCP length includes the four-byte message ID, but excludes the first
    /// two header words.
    pub fn encode(&self) -> Result<Vec<u8>, CodecError> {
        let wire_len = self
            .payload
            .len()
            .checked_add(4)
            .ok_or(CodecError::FrameTooLarge(usize::MAX))?;
        let total = wire_len + 8;
        if total > MAX_FRAME_SIZE {
            return Err(CodecError::FrameTooLarge(total));
        }
        let header = WireHeader {
            wire_len: u32::try_from(wire_len).map_err(|_| CodecError::FrameTooLarge(wire_len))?,
            protocol_version: self.protocol_version,
            message_id: self.message_id,
        };
        let mut output = Cursor::new(Vec::with_capacity(total));
        header
            .write(&mut output)
            .map_err(|error| CodecError::wire("encode", self.message_id, &error))?;
        output.get_mut().extend_from_slice(&self.payload);
        Ok(output.into_inner())
    }

    /// Resolves the numeric identifier to its typed catalog value.
    ///
    /// Unrecognized identifiers become [`MessageId::Unknown`].
    pub fn message_type(&self) -> MessageId {
        MessageId::from(self.message_id)
    }
}

#[derive(Debug, Error, Clone, Eq, PartialEq)]
/// Validation and serialization failures produced by framing and message codecs.
pub enum CodecError {
    /// The header length cannot describe a valid frame.
    #[error("SCCP frame length {0} is invalid")]
    InvalidLength(u32),
    /// The complete frame would exceed [`MAX_FRAME_SIZE`].
    #[error("SCCP frame size {0} exceeds the configured maximum")]
    FrameTooLarge(usize),
    /// A fixed or mandatory payload prefix is incomplete.
    #[error("message 0x{message_id:04x} is truncated: need {needed} bytes, got {actual}")]
    Truncated {
        message_id: u32,
        needed: usize,
        actual: usize,
    },
    /// A station identifier failed its textual or structural validation.
    #[error("invalid SCCP device ID: {0}")]
    InvalidDeviceId(String),
    /// A runtime device definition violates a message-level constraint.
    #[error("invalid SCCP device definition: {0}")]
    InvalidDefinition(String),
    /// A wire text field is not valid under the selected text policy.
    #[error("invalid UTF-8-compatible SCCP text field")]
    InvalidText,
    /// The requested protocol version is outside the supported range.
    #[error("unsupported SCCP protocol version {0}")]
    UnsupportedProtocol(u32),
    /// A known message was decoded through the wrong protocol role.
    #[error("message 0x{message_id:04x} has route {actual:?}, expected {expected}")]
    UnexpectedRoute {
        message_id: u32,
        actual: MessageRoute,
        expected: &'static str,
    },
    /// A scalar field contains a value forbidden by its wire contract.
    #[error("message 0x{message_id:04x} contains invalid {field}: {value}")]
    InvalidValue {
        message_id: u32,
        field: &'static str,
        value: u64,
    },
    /// A declared or decoded item count exceeds its bounded capacity.
    #[error("message 0x{message_id:04x} {field} count {count} exceeds maximum {maximum}")]
    CountTooLarge {
        message_id: u32,
        field: &'static str,
        count: usize,
        maximum: usize,
    },
    /// Reserved bytes in a fixed field are not zero-filled.
    #[error("message 0x{message_id:04x} contains non-zero {field} padding")]
    NonZeroPadding {
        message_id: u32,
        field: &'static str,
    },
    /// Bytes remain after decoding a layout that requires exact consumption.
    #[error("message 0x{message_id:04x} has {count} unexpected trailing bytes")]
    TrailingBytes { message_id: u32, count: usize },
    /// A payload that requires 32-bit alignment has an invalid length.
    #[error(
        "message 0x{message_id:04x} payload length {actual} is not aligned to a four-byte boundary"
    )]
    InvalidAlignment { message_id: u32, actual: usize },
    /// A text value exceeds the capacity of its wire field.
    #[error(
        "message 0x{message_id:04x} field {field} is too long: {actual} bytes, maximum {maximum}"
    )]
    TextTooLong {
        message_id: u32,
        field: &'static str,
        actual: usize,
        maximum: usize,
    },
    /// Secret key material exceeds its fixed wire capacity.
    #[error("secret field {field} is too long: {actual} bytes, maximum {maximum}")]
    SecretTooLong {
        field: &'static str,
        actual: usize,
        maximum: usize,
    },
    /// The binary reader or writer failed at a known wire offset.
    #[error("could not {operation} SCCP message 0x{message_id:04x} at byte {offset}: {detail}")]
    Wire {
        operation: &'static str,
        message_id: u32,
        offset: u64,
        detail: String,
    },
}

impl CodecError {
    pub(crate) fn wire(operation: &'static str, message_id: u32, error: &binrw::Error) -> Self {
        let offset = match error {
            binrw::Error::BadMagic { pos, .. }
            | binrw::Error::AssertFail { pos, .. }
            | binrw::Error::Custom { pos, .. }
            | binrw::Error::NoVariantMatch { pos }
            | binrw::Error::EnumErrors { pos, .. } => *pos,
            binrw::Error::Io(_) | binrw::Error::Backtrace(_) => 0,
            _ => 0,
        };
        Self::Wire {
            operation,
            message_id,
            offset,
            detail: error.to_string(),
        }
    }
}

#[derive(Debug, Default)]
/// Incremental decoder for an SCCP byte stream.
///
/// The decoder retains an incomplete trailing frame between calls. Complete
/// frames are returned in stream order, including when a chunk contains more
/// than one frame.
pub struct FrameDecoder {
    buffer: Vec<u8>,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a stream chunk and returns every newly completed frame.
    ///
    /// An empty vector means the buffered bytes do not yet complete a frame.
    /// Framing errors are terminal for the current buffered stream; callers
    /// should discard the decoder with the connection.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Frame>, CodecError> {
        self.buffer.extend_from_slice(bytes);
        let mut frames = Vec::new();
        loop {
            if self.buffer.len() < HEADER_SIZE {
                break;
            }
            let header = WireHeader::read(&mut Cursor::new(&self.buffer[..HEADER_SIZE]))
                .map_err(|error| CodecError::wire("decode header for", 0, &error))?;
            let length = header.wire_len;
            if length < 4 {
                return Err(CodecError::InvalidLength(length));
            }
            let total =
                usize::try_from(length).map_err(|_| CodecError::FrameTooLarge(usize::MAX))? + 8;
            if total > MAX_FRAME_SIZE {
                return Err(CodecError::FrameTooLarge(total));
            }
            if self.buffer.len() < total {
                break;
            }
            let payload = self.buffer[HEADER_SIZE..total].to_vec();
            self.buffer.drain(..total);
            frames.push(Frame {
                protocol_version: header.protocol_version,
                message_id: header.message_id,
                payload,
            });
        }
        debug_assert!(self.buffer.len() < MAX_FRAME_SIZE);
        Ok(frames)
    }

    pub fn buffered_len(&self) -> usize {
        self.buffer.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_large_chunk_of_small_frames_is_drained_incrementally() {
        let encoded = Frame::new(22, 0x0100, Vec::new()).encode().unwrap();
        let frame_count = MAX_FRAME_SIZE * 2 / encoded.len() + 1;
        let stream = encoded.repeat(frame_count);
        assert!(stream.len() > MAX_FRAME_SIZE * 2);

        let frames = FrameDecoder::new().push(&stream).unwrap();

        assert_eq!(frames.len(), frame_count);
        assert!(frames.iter().all(|frame| {
            frame.protocol_version == 22 && frame.message_id == 0x0100 && frame.payload.is_empty()
        }));
    }

    #[test]
    fn frame_length_matches_skinny_header_definition() {
        let frame = Frame::new(22, 0x0006, vec![1, 2, 3, 4]);
        let encoded = frame.encode().unwrap();
        assert_eq!(&encoded[..4], &8_u32.to_le_bytes());
        assert_eq!(encoded.len(), 16);
    }

    #[test]
    fn decoder_handles_fragmented_and_coalesced_frames() {
        let first = Frame::new(0, 0, Vec::new()).encode().unwrap();
        let second = Frame::new(22, 6, vec![1; 8]).encode().unwrap();
        let mut decoder = FrameDecoder::new();
        assert!(decoder.push(&first[..5]).unwrap().is_empty());
        let mut rest = first[5..].to_vec();
        rest.extend_from_slice(&second);
        rest.extend_from_slice(&first);
        rest.extend_from_slice(&second);
        let frames = decoder.push(&rest).unwrap();
        assert_eq!(
            frames
                .iter()
                .map(|frame| frame.message_id)
                .collect::<Vec<_>>(),
            [0, 6, 0, 6]
        );
        assert_eq!(frames[1], frames[3], "duplicate frames changed in transit");
        assert_eq!(frames[0], frames[2], "reordered frames changed in transit");
    }

    #[test]
    fn decoder_accepts_every_possible_single_fragment_boundary() {
        let bytes = Frame::new(22, 0x22, (0_u8..64).collect()).encode().unwrap();
        for split in 0..bytes.len() {
            let mut decoder = FrameDecoder::new();
            assert!(decoder.push(&bytes[..split]).unwrap().is_empty());
            let frames = decoder.push(&bytes[split..]).unwrap();
            assert_eq!(frames.len(), 1, "split at byte {split}");
            assert_eq!(frames[0].payload, (0_u8..64).collect::<Vec<_>>());
            assert_eq!(decoder.buffered_len(), 0);
        }
    }

    #[test]
    fn invalid_short_length_is_rejected() {
        let mut decoder = FrameDecoder::new();
        let mut bytes = vec![0; 12];
        bytes[..4].copy_from_slice(&3_u32.to_le_bytes());
        assert_eq!(decoder.push(&bytes), Err(CodecError::InvalidLength(3)));
    }
}
