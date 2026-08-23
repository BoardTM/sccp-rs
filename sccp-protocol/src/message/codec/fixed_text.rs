//! Fixed-width text fields used by declarative wire layouts.
//!
//! This module keeps capacity checks, NUL termination, and legacy station
//! encoding in one place so individual message codecs share the same policy.

use binrw::{BinRead, BinWrite};

use crate::message::wire::CodecError;
use crate::types::LegacyCodePage;

/// A fixed-width, NUL-terminated text field from a declarative wire layout.
///
/// Construction owns both the capacity check and the legacy station encoding
/// policy so individual message codecs cannot accidentally implement subtly
/// different truncation or replacement rules.
#[derive(BinRead, BinWrite, Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct WireFixedText<const N: usize> {
    bytes: [u8; N],
}

impl<const N: usize> WireFixedText<N> {
    pub(super) fn new(
        message_id: u32,
        field: &'static str,
        value: &str,
    ) -> Result<Self, CodecError> {
        Self::from_bytes(message_id, field, value.as_bytes())
    }

    pub(super) fn new_station(
        message_id: u32,
        field: &'static str,
        value: &str,
        code_page: Option<LegacyCodePage>,
    ) -> Result<Self, CodecError> {
        let bytes = station_text_bytes(value, code_page)?;
        Self::from_bytes(message_id, field, &bytes)
    }

    pub(super) fn text(&self) -> Result<String, CodecError> {
        let end = self.bytes.iter().position(|byte| *byte == 0).unwrap_or(N);
        let text = std::str::from_utf8(&self.bytes[..end]).map_err(|_| CodecError::InvalidText)?;
        Ok(text.trim().to_owned())
    }

    fn from_bytes(message_id: u32, field: &'static str, value: &[u8]) -> Result<Self, CodecError> {
        let maximum = N.saturating_sub(1);
        if value.len() > maximum {
            return Err(CodecError::TextTooLong {
                message_id,
                field,
                actual: value.len(),
                maximum,
            });
        }
        let mut bytes = [0; N];
        bytes[..value.len()].copy_from_slice(value);
        Ok(Self { bytes })
    }
}

pub(super) fn station_text_bytes(
    value: &str,
    code_page: Option<LegacyCodePage>,
) -> Result<Vec<u8>, CodecError> {
    if value.contains('\0') {
        return Err(CodecError::InvalidText);
    }
    Ok(match code_page {
        None => value.as_bytes().to_vec(),
        Some(LegacyCodePage::Iso8859_1) => value
            .chars()
            .map(|character| u8::try_from(u32::from(character)).unwrap_or(b'?'))
            .collect(),
        Some(LegacyCodePage::Ascii) => value
            .chars()
            .map(|character| {
                u8::try_from(character)
                    .ok()
                    .filter(u8::is_ascii)
                    .unwrap_or(b'?')
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn station_encoding_is_bounded_and_replaces_unrepresentable_text() {
        let ascii =
            WireFixedText::<5>::new_station(1, "label", "café", Some(LegacyCodePage::Ascii))
                .unwrap();
        assert_eq!(ascii.text().unwrap(), "caf?");

        assert!(matches!(
            WireFixedText::<4>::new(1, "label", "four"),
            Err(CodecError::TextTooLong { maximum: 3, .. })
        ));
    }
}
