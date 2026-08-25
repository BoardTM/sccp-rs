//! Shared resource-bound mechanics for phone-facing text formats.
//!
//! Callers retain their own character policy and error wording; this module
//! only owns the identical count and Unicode-length arithmetic.

use super::xml::PhoneXmlError;

pub(super) fn validate_count(
    kind: &'static str,
    actual: usize,
    maximum: usize,
) -> Result<(), PhoneXmlError> {
    if actual > maximum {
        Err(PhoneXmlError::LimitExceeded {
            kind,
            actual,
            maximum,
        })
    } else {
        Ok(())
    }
}

pub(super) fn text_length_is_within(value: &str, minimum: usize, maximum: usize) -> bool {
    (minimum..=maximum).contains(&value.chars().count())
}
