//! Bounded, secret-safe destinations used by configured and guest hotlines.

use std::fmt;

use thiserror::Error;

pub const MAX_HOTLINE_DESTINATION_BYTES: usize = 79;

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct HotlineDestination(String);

impl HotlineDestination {
    pub fn new(value: impl AsRef<str>) -> Result<Self, HotlineDestinationError> {
        let value = value.as_ref().trim();
        if value.is_empty()
            || value.len() > MAX_HOTLINE_DESTINATION_BYTES
            || value.chars().any(char::is_control)
        {
            return Err(HotlineDestinationError::Invalid);
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for HotlineDestination {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HotlineDestination")
            .field("bytes", &self.0.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum HotlineDestinationError {
    #[error("invalid hotline destination")]
    Invalid,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destination_is_bounded_and_debug_never_discloses_its_value() {
        let destination = HotlineDestination::new(" 9911 ").unwrap();
        assert_eq!(destination.as_str(), "9911");
        let debug = format!("{destination:?}");
        assert!(debug.contains("4"));
        assert!(!debug.contains("9911"));

        for invalid in [
            "".to_owned(),
            "\n".to_owned(),
            "9".repeat(MAX_HOTLINE_DESTINATION_BYTES + 1),
        ] {
            assert_eq!(
                HotlineDestination::new(invalid),
                Err(HotlineDestinationError::Invalid)
            );
        }
    }
}
