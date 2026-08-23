//! Checked all-or-nothing writes to Asterisk-owned output buffers.

use std::ffi::c_char;
use std::ptr;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputError {
    Missing,
    InteriorNul,
    TooSmall,
}

/// Write one NUL-terminated string only after all validation succeeds.
///
/// # Safety
///
/// On success, `output` must be writable for `capacity` bytes. It may be null
/// only when `capacity` is zero, which is reported as [`OutputError::Missing`].
pub unsafe fn write_c_text(
    output: *mut c_char,
    capacity: usize,
    value: &str,
) -> Result<(), OutputError> {
    if output.is_null() || capacity == 0 {
        return Err(OutputError::Missing);
    }
    if value.as_bytes().contains(&0) {
        return Err(OutputError::InteriorNul);
    }
    let required = value.len().checked_add(1).ok_or(OutputError::TooSmall)?;
    if required > capacity {
        return Err(OutputError::TooSmall);
    }
    unsafe {
        ptr::copy_nonoverlapping(value.as_ptr().cast::<c_char>(), output, value.len());
        *output.add(value.len()) = 0;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_is_validated_before_it_is_modified() {
        let mut output = [b'x' as c_char; 8];
        assert_eq!(
            unsafe { write_c_text(output.as_mut_ptr(), 3, "hello") },
            Err(OutputError::TooSmall)
        );
        assert_eq!(output, [b'x' as c_char; 8]);

        unsafe { write_c_text(output.as_mut_ptr(), output.len(), "hello") }.unwrap();
        assert_eq!(&output[..6], &[b'h', b'e', b'l', b'l', b'o', 0]);
    }
}
