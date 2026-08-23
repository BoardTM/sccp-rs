//! Typed access to fixed-size scalar callback arguments.

use std::ffi::{c_int, c_void};
use std::mem;
use std::ptr;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeValueError {
    Missing,
    WrongSize,
    TooSmall,
}

/// Read one possibly unaligned integer supplied by a native callback.
///
/// # Safety
///
/// A non-null `value` must be readable for `length` bytes for this call.
pub unsafe fn read_c_int(value: *const c_void, length: usize) -> Result<c_int, NativeValueError> {
    if value.is_null() {
        return Err(NativeValueError::Missing);
    }
    if length != mem::size_of::<c_int>() {
        return Err(NativeValueError::WrongSize);
    }
    Ok(unsafe { ptr::read_unaligned(value.cast::<c_int>()) })
}

/// Write one possibly unaligned integer and report its required size.
///
/// # Safety
///
/// `length` must point to a writable integer. When its input value is large
/// enough, `output` must point to writable storage for one `c_int`.
pub unsafe fn write_c_int(
    output: *mut c_void,
    length: *mut c_int,
    value: c_int,
) -> Result<(), NativeValueError> {
    if output.is_null() || length.is_null() {
        return Err(NativeValueError::Missing);
    }
    let required = mem::size_of::<c_int>() as c_int;
    if unsafe { ptr::read_unaligned(length) } < required {
        return Err(NativeValueError::TooSmall);
    }
    unsafe {
        ptr::write_unaligned(output.cast::<c_int>(), value);
        ptr::write_unaligned(length, required);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_input_requires_one_complete_value() {
        let value = 42_c_int;
        assert_eq!(
            unsafe { read_c_int(ptr::from_ref(&value).cast(), mem::size_of_val(&value)) },
            Ok(value)
        );
        assert_eq!(
            unsafe { read_c_int(ptr::from_ref(&value).cast(), 1) },
            Err(NativeValueError::WrongSize)
        );
        assert_eq!(
            unsafe { read_c_int(ptr::null(), mem::size_of::<c_int>()) },
            Err(NativeValueError::Missing)
        );
    }

    #[test]
    fn integer_output_checks_and_updates_capacity() {
        let mut output = 0_c_int;
        let mut length = mem::size_of::<c_int>() as c_int;
        assert_eq!(
            unsafe {
                write_c_int(
                    ptr::from_mut(&mut output).cast(),
                    ptr::from_mut(&mut length),
                    7,
                )
            },
            Ok(())
        );
        assert_eq!(output, 7);
        assert_eq!(length, mem::size_of::<c_int>() as c_int);

        length = 1;
        assert_eq!(
            unsafe {
                write_c_int(
                    ptr::from_mut(&mut output).cast(),
                    ptr::from_mut(&mut length),
                    9,
                )
            },
            Err(NativeValueError::TooSmall)
        );
        assert_eq!(output, 7);
    }
}
