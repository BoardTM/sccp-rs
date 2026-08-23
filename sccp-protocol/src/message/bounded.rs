//! Size-bounded storage for opaque or partially understood wire fields.
//!
//! [`BoundedBytes`] makes allocation limits part of a message type. Use
//! [`BoundedBytes::new`] or a `TryFrom` implementation at trust boundaries,
//! then borrow the retained value with [`BoundedBytes::as_bytes`].

use std::error::Error;
use std::fmt;
use std::ops::Deref;

/// Owned wire bytes whose allocation is capped by the message contract.
///
/// The bytes are otherwise uninterpreted. Message-specific decoders can read
/// every complete field they understand while retaining the rest verbatim.
#[derive(Clone, Default, Eq, Hash, PartialEq)]
pub struct BoundedBytes<const MAX: usize>(Box<[u8]>);

impl<const MAX: usize> BoundedBytes<MAX> {
    /// Retains `bytes` when its length does not exceed `MAX`.
    ///
    /// The error reports both the configured maximum and the supplied length;
    /// the rejected allocation is not retained.
    pub fn new(bytes: impl Into<Box<[u8]>>) -> Result<Self, BoundedBytesError> {
        let bytes = bytes.into();
        if bytes.len() > MAX {
            return Err(BoundedBytesError {
                maximum: MAX,
                actual: bytes.len(),
            });
        }
        Ok(Self(bytes))
    }

    pub const fn maximum_len() -> usize {
        MAX
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn into_boxed_slice(self) -> Box<[u8]> {
        self.0
    }
}

impl<const MAX: usize> AsRef<[u8]> for BoundedBytes<MAX> {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl<const MAX: usize> Deref for BoundedBytes<MAX> {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.as_bytes()
    }
}

impl<const MAX: usize> TryFrom<Vec<u8>> for BoundedBytes<MAX> {
    type Error = BoundedBytesError;

    fn try_from(bytes: Vec<u8>) -> Result<Self, Self::Error> {
        Self::new(bytes)
    }
}

impl<const MAX: usize> TryFrom<Box<[u8]>> for BoundedBytes<MAX> {
    type Error = BoundedBytesError;

    fn try_from(bytes: Box<[u8]>) -> Result<Self, Self::Error> {
        Self::new(bytes)
    }
}

impl<const MAX: usize> TryFrom<&[u8]> for BoundedBytes<MAX> {
    type Error = BoundedBytesError;

    fn try_from(bytes: &[u8]) -> Result<Self, Self::Error> {
        Self::new(bytes)
    }
}

impl<const MAX: usize> From<BoundedBytes<MAX>> for Box<[u8]> {
    fn from(bytes: BoundedBytes<MAX>) -> Self {
        bytes.into_boxed_slice()
    }
}

impl<const MAX: usize> From<BoundedBytes<MAX>> for Vec<u8> {
    fn from(bytes: BoundedBytes<MAX>) -> Self {
        bytes.into_boxed_slice().into_vec()
    }
}

impl<const MAX: usize> fmt::Debug for BoundedBytes<MAX> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BoundedBytes")
            .field("len", &self.len())
            .field("maximum", &MAX)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Failure returned when a value exceeds a [`BoundedBytes`] allocation limit.
pub struct BoundedBytesError {
    pub maximum: usize,
    pub actual: usize,
}

impl fmt::Display for BoundedBytesError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "payload contains {} bytes, exceeding the {}-byte bound",
            self.actual, self.maximum
        )
    }
}

impl Error for BoundedBytesError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_every_length_through_the_bound() {
        for length in 0..=8 {
            let bytes = BoundedBytes::<8>::try_from(vec![0xa5; length]).unwrap();
            assert_eq!(bytes.len(), length);
            assert!(bytes.iter().all(|byte| *byte == 0xa5));
        }
    }

    #[test]
    fn rejects_the_first_oversized_value_without_retaining_it() {
        assert_eq!(
            BoundedBytes::<8>::try_from(vec![0xa5; 9]).unwrap_err(),
            BoundedBytesError {
                maximum: 8,
                actual: 9,
            }
        );
    }

    #[test]
    fn debug_reports_shape_without_contents() {
        let bytes = BoundedBytes::<8>::try_from(b"secret".as_slice()).unwrap();
        let rendered = format!("{bytes:?}");
        assert!(rendered.contains("len: 6"));
        assert!(!rendered.contains("secret"));
    }
}
