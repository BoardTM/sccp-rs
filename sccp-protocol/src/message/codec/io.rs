//! Little-endian helpers for exact, prefixed, and padded payload layouts.
//!
//! These helpers make each codec state whether trailing bytes are forbidden,
//! preserved by its caller, or accepted only as alignment padding.

use std::io::Cursor;

use binrw::{BinRead, BinWrite, Endian};

use crate::message::wire::CodecError;

pub(super) fn encode<T>(message_id: u32, value: &T) -> Result<Vec<u8>, CodecError>
where
    for<'a> T: BinWrite<Args<'a> = ()>,
{
    let mut output = Cursor::new(Vec::new());
    value
        .write_options(&mut output, Endian::Little, ())
        .map_err(|error| CodecError::wire("encode", message_id, &error))?;
    Ok(output.into_inner())
}

pub(super) fn decode<T>(message_id: u32, payload: &[u8]) -> Result<T, CodecError>
where
    for<'a> T: BinRead<Args<'a> = ()>,
{
    let mut input = Cursor::new(payload);
    let value = T::read_options(&mut input, Endian::Little, ())
        .map_err(|error| CodecError::wire("decode", message_id, &error))?;
    let consumed = usize::try_from(input.position()).map_err(|_| CodecError::InvalidValue {
        message_id,
        field: "decoded payload position",
        value: input.position(),
    })?;
    if consumed != payload.len() {
        return Err(CodecError::TrailingBytes {
            message_id,
            count: payload.len() - consumed,
        });
    }
    Ok(value)
}

pub(super) fn decode_prefix<T>(message_id: u32, payload: &[u8]) -> Result<T, CodecError>
where
    for<'a> T: BinRead<Args<'a> = ()>,
{
    let mut input = Cursor::new(payload);
    T::read_options(&mut input, Endian::Little, ())
        .map_err(|error| CodecError::wire("decode", message_id, &error))
}

pub(super) fn decode_zero_padded<T>(message_id: u32, payload: &[u8]) -> Result<T, CodecError>
where
    for<'a> T: BinRead<Args<'a> = ()>,
{
    if !payload.len().is_multiple_of(4) {
        return Err(CodecError::InvalidAlignment {
            message_id,
            actual: payload.len(),
        });
    }
    let mut input = Cursor::new(payload);
    let value = T::read_options(&mut input, Endian::Little, ())
        .map_err(|error| CodecError::wire("decode", message_id, &error))?;
    let consumed = usize::try_from(input.position()).map_err(|_| CodecError::InvalidValue {
        message_id,
        field: "decoded payload position",
        value: input.position(),
    })?;
    let padding = &payload[consumed..];
    if padding.len() > 3 || padding.iter().any(|byte| *byte != 0) {
        return Err(CodecError::TrailingBytes {
            message_id,
            count: padding.len(),
        });
    }
    Ok(value)
}

pub(super) fn validate_exact_payload(
    payload: &[u8],
    message_id: u32,
    expected: usize,
) -> Result<(), CodecError> {
    match payload.len().cmp(&expected) {
        std::cmp::Ordering::Less => Err(CodecError::Truncated {
            message_id,
            needed: expected,
            actual: payload.len(),
        }),
        std::cmp::Ordering::Greater => Err(CodecError::TrailingBytes {
            message_id,
            count: payload.len() - expected,
        }),
        std::cmp::Ordering::Equal => Ok(()),
    }
}

pub(super) fn validate_payload_bounds(
    payload: &[u8],
    message_id: u32,
    minimum: usize,
    maximum: usize,
) -> Result<(), CodecError> {
    debug_assert!(minimum <= maximum);
    if payload.len() < minimum {
        return Err(CodecError::Truncated {
            message_id,
            needed: minimum,
            actual: payload.len(),
        });
    }
    if payload.len() > maximum {
        return Err(CodecError::TrailingBytes {
            message_id,
            count: payload.len() - maximum,
        });
    }
    Ok(())
}

pub(super) fn validate_zero_payload(
    payload: &[u8],
    message_id: u32,
    expected: usize,
) -> Result<(), CodecError> {
    validate_exact_payload(payload, message_id, expected)?;
    if payload.iter().any(|byte| *byte != 0) {
        return Err(CodecError::InvalidValue {
            message_id,
            field: "reserved payload byte",
            value: 1,
        });
    }
    Ok(())
}

pub(super) fn usize_from_wire(
    message_id: u32,
    field: &'static str,
    value: u32,
) -> Result<usize, CodecError> {
    usize::try_from(value).map_err(|_| CodecError::InvalidValue {
        message_id,
        field,
        value: u64::from(value),
    })
}

pub(super) fn wire_count(
    message_id: u32,
    field: &'static str,
    value: usize,
) -> Result<u32, CodecError> {
    u32::try_from(value).map_err(|_| CodecError::CountTooLarge {
        message_id,
        field,
        count: value,
        maximum: u32::MAX as usize,
    })
}
