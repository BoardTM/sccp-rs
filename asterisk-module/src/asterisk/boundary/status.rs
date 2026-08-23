//! Stable numeric results used only where Asterisk requires an integer ABI.

use std::ffi::c_int;

/// Success/failure status returned by Asterisk callbacks.
#[repr(i32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CallbackStatus {
    Success = 0,
    Failure = -1,
}

impl CallbackStatus {
    pub fn from_result<T, E>(result: Result<T, E>) -> Self {
        if result.is_ok() {
            Self::Success
        } else {
            Self::Failure
        }
    }

    pub const fn as_raw(self) -> c_int {
        self as c_int
    }
}

/// Severity understood by the Asterisk logging edge.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LogLevel {
    Error,
    Warning,
    Notice,
    Debug,
}

/// Logical SCCP line state before conversion to Asterisk's device-state enum.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceState {
    Unknown,
    NotInUse,
    InUse,
    Unavailable,
    Ringing,
}
