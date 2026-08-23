//! Bounded conversion of borrowed C callback arguments into owned Rust text.

use std::ffi::c_char;
use std::str;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeTextError {
    Missing,
    Unterminated,
    InvalidUtf8,
}

/// Copy a required NUL-terminated callback argument without an unbounded scan.
///
/// # Safety
///
/// `value` must be null or point to a valid C string for the duration of this
/// call. At most `maximum_bytes + 1` bytes are inspected and the returned
/// `String` does not borrow that storage.
pub unsafe fn required_c_text(
    value: *const c_char,
    maximum_bytes: usize,
) -> Result<String, NativeTextError> {
    if value.is_null() {
        return Err(NativeTextError::Missing);
    }
    let mut length = 0usize;
    while length <= maximum_bytes {
        if unsafe { *value.add(length) } == 0 {
            break;
        }
        length = length.checked_add(1).ok_or(NativeTextError::Unterminated)?;
    }
    if length > maximum_bytes {
        return Err(NativeTextError::Unterminated);
    }
    let bytes = unsafe { std::slice::from_raw_parts(value.cast::<u8>(), length) };
    str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| NativeTextError::InvalidUtf8)
}

/// Copy an optional bounded callback argument. Null maps to `None`; malformed
/// non-null input remains an error.
///
/// # Safety
///
/// A non-null `value` has the same requirements as [`required_c_text`].
pub unsafe fn optional_c_text(
    value: *const c_char,
    maximum_bytes: usize,
) -> Result<Option<String>, NativeTextError> {
    if value.is_null() {
        Ok(None)
    } else {
        unsafe { required_c_text(value, maximum_bytes) }.map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_text_is_bounded_owned_and_utf8_checked() {
        let valid = b"line-1000\0tail";
        assert_eq!(
            unsafe { required_c_text(valid.as_ptr().cast(), 9) },
            Ok("line-1000".to_owned())
        );
        assert_eq!(
            unsafe { required_c_text(valid.as_ptr().cast(), 4) },
            Err(NativeTextError::Unterminated)
        );

        let invalid = [0xff, 0];
        assert_eq!(
            unsafe { required_c_text(invalid.as_ptr().cast(), 1) },
            Err(NativeTextError::InvalidUtf8)
        );
        assert_eq!(
            unsafe { required_c_text(std::ptr::null(), 1) },
            Err(NativeTextError::Missing)
        );
    }

    #[test]
    fn optional_text_distinguishes_null_from_empty() {
        assert_eq!(unsafe { optional_c_text(std::ptr::null(), 4) }, Ok(None));
        assert_eq!(
            unsafe { optional_c_text(c"".as_ptr(), 4) },
            Ok(Some(String::new()))
        );
    }
}
