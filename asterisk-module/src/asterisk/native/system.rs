//! Process-wide Asterisk logging, CLI output, and device-state publication.

use std::ffi::{CStr, CString, c_char, c_int};

#[cfg(feature = "telemetry")]
use uuid::Uuid;

use crate::asterisk::boundary::{DeviceState, LogLevel, native_c_string};
use crate::asterisk::sys;

const SOURCE_FILE: &CStr = c"asterisk/native/system.rs";
const SOURCE_FUNCTION: &CStr = c"sccp_system";
pub const CONFIG_STATUS_VARIABLE: &CStr = c"SCCP_CONFIG_STATUS";

#[cfg(feature = "telemetry")]
pub(super) fn pbx_uuid() -> Option<Uuid> {
    let mut buffer = [0 as c_char; sys::AST_UUID_STR_LEN as usize];
    let status =
        unsafe { sys::ast_pbx_uuid_get(buffer.as_mut_ptr(), c_int::try_from(buffer.len()).ok()?) };
    if status != 0 {
        return None;
    }
    let bytes = unsafe { CStr::from_ptr(buffer.as_ptr()) }.to_bytes();
    Uuid::try_parse_ascii(bytes).ok()
}

pub fn log_message(level: LogLevel, message: &str) {
    let level = match level {
        LogLevel::Error => sys::__LOG_ERROR,
        LogLevel::Warning => sys::__LOG_WARNING,
        LogLevel::Notice => sys::__LOG_NOTICE,
        LogLevel::Debug => sys::__LOG_DEBUG,
    };
    let Ok(message) = native_c_string(message) else {
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
    let Ok(message) = native_c_string(message) else {
        return;
    };
    unsafe { sys::ast_cli(fd, c"%s".as_ptr(), message.as_ptr()) };
}

pub fn set_global_variable(name: &CStr, value: Option<&str>) -> Result<(), ()> {
    if name.to_bytes().is_empty() {
        return Err(());
    }
    let value = value.map(native_c_string).transpose().map_err(|_| ())?;
    let value = value
        .as_ref()
        .map_or(std::ptr::null(), |value| value.as_ptr());
    if unsafe { sys::pbx_builtin_setvar_helper(std::ptr::null_mut(), name.as_ptr(), value) } == 0 {
        Ok(())
    } else {
        Err(())
    }
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
    let mapped = device_state_raw(state);
    let Ok(device) = CString::new(format!("SCCP/{line}")) else {
        return;
    };
    unsafe {
        sys::ast_devstate_changed_literal(mapped, sys::AST_DEVSTATE_CACHABLE, device.as_ptr());
    }
}

/// Exhaustive conversion shared by publication and channel-tech callbacks.
pub const fn device_state_raw(state: DeviceState) -> sys::ast_device_state {
    match state {
        DeviceState::NotInUse => sys::AST_DEVICE_NOT_INUSE,
        DeviceState::InUse => sys::AST_DEVICE_INUSE,
        DeviceState::Busy => sys::AST_DEVICE_BUSY,
        DeviceState::Removed => sys::AST_DEVICE_INVALID,
        DeviceState::Unavailable => sys::AST_DEVICE_UNAVAILABLE,
        DeviceState::Ringing => sys::AST_DEVICE_RINGING,
        DeviceState::RingInUse => sys::AST_DEVICE_RINGINUSE,
        DeviceState::OnHold => sys::AST_DEVICE_ONHOLD,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_logical_device_state_has_the_native_value() {
        let mappings = [
            (DeviceState::NotInUse, sys::AST_DEVICE_NOT_INUSE),
            (DeviceState::InUse, sys::AST_DEVICE_INUSE),
            (DeviceState::Busy, sys::AST_DEVICE_BUSY),
            (DeviceState::Removed, sys::AST_DEVICE_INVALID),
            (DeviceState::Unavailable, sys::AST_DEVICE_UNAVAILABLE),
            (DeviceState::Ringing, sys::AST_DEVICE_RINGING),
            (DeviceState::RingInUse, sys::AST_DEVICE_RINGINUSE),
            (DeviceState::OnHold, sys::AST_DEVICE_ONHOLD),
        ];
        for (logical, native) in mappings {
            assert_eq!(device_state_raw(logical), native);
        }
    }
}
