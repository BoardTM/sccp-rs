//! Process-wide Asterisk logging, CLI output, and device-state publication.

use std::ffi::{CStr, CString, c_char, c_int};

use crate::asterisk::boundary::{DeviceState, LogLevel};
use crate::asterisk::sys;

const SOURCE_FILE: &CStr = c"asterisk/native/system.rs";
const SOURCE_FUNCTION: &CStr = c"sccp_system";

pub fn log_message(level: LogLevel, message: &str) {
    let level = match level {
        LogLevel::Error => sys::__LOG_ERROR,
        LogLevel::Warning => sys::__LOG_WARNING,
        LogLevel::Notice => sys::__LOG_NOTICE,
        LogLevel::Debug => sys::__LOG_DEBUG,
    };
    let Ok(message) = CString::new(message.replace('\0', "")) else {
        return;
    };
    unsafe {
        sys::ast_log(
            level as c_int,
            SOURCE_FILE.as_ptr(),
            line!() as c_int,
            SOURCE_FUNCTION.as_ptr(),
            c"%s\n".as_ptr(),
            message.as_ptr(),
        );
    }
}

pub fn cli_write(fd: c_int, message: &str) {
    let Ok(message) = CString::new(message.replace('\0', "")) else {
        return;
    };
    unsafe { sys::ast_cli(fd, c"%s".as_ptr(), message.as_ptr()) };
}

/// Copy a completion into memory owned by the native CLI.
pub fn cli_completion(value: &CStr) -> *mut c_char {
    unsafe {
        sys::__ast_strdup(
            value.as_ptr(),
            SOURCE_FILE.as_ptr(),
            line!() as c_int,
            SOURCE_FUNCTION.as_ptr(),
        )
    }
}

pub fn publish_device_state(line: &str, state: DeviceState) {
    let mapped = match state {
        DeviceState::Unknown => sys::AST_DEVICE_UNKNOWN,
        DeviceState::NotInUse => sys::AST_DEVICE_NOT_INUSE,
        DeviceState::InUse => sys::AST_DEVICE_INUSE,
        DeviceState::Unavailable => sys::AST_DEVICE_UNAVAILABLE,
        DeviceState::Ringing => sys::AST_DEVICE_RINGING,
    };
    let Ok(device) = CString::new(format!("SCCP/{line}")) else {
        return;
    };
    unsafe {
        sys::ast_devstate_changed_literal(mapped, sys::AST_DEVSTATE_CACHABLE, device.as_ptr());
    }
}
