//! Raw bridge, pickup, parking, and conference ownership primitives.
//!
//! Ownership is explicit in Rust: bridge handles own or reference exactly one AO2 bridge,
//! channel-returning APIs balance their references, application tasks retain
//! both their channel and this module, and parking subscriptions join
//! before destroying callback userdata.

mod conference;
mod parking;
mod pickup;

pub use conference::{
    ConferenceApplicationCancellation, acquire_barge_bridge, create_bridge,
    prepare_conference_destination,
};
pub use parking::{
    park_channel, parking_peer_uniqueid, retrieve_parked_channel, subscribe_parking,
};
pub use pickup::{NativePickupChannel, configure_pickup, pickup_directed, pickup_group};

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::mem;
use std::ptr;

use crate::asterisk::boundary::required_c_text;
use crate::asterisk::raw::handles::{ChannelLock, ChannelRef};
use crate::asterisk::sys;
use crate::config::JitterBufferImplementation;
use crate::pbx::operations::CallFeatureError;
use crate::pbx::party::AsteriskChannel;

const SOURCE_FILE: &CStr = c"asterisk/native/bridge/mod.rs";
const SOURCE_FUNCTION: &CStr = c"bridge";
unsafe fn ao2_ref(object: *mut c_void, delta: c_int) {
    if !object.is_null() {
        unsafe {
            sys::__ao2_ref(
                object,
                delta,
                ptr::null(),
                SOURCE_FILE.as_ptr(),
                line!() as c_int,
                SOURCE_FUNCTION.as_ptr(),
            );
        }
    }
}

unsafe fn lock_channel(channel: *mut sys::ast_channel) -> Option<ChannelLock> {
    let channel = unsafe { ChannelRef::acquire(channel) }?;
    ChannelLock::acquire(channel).ok()
}

unsafe fn current_bridge(channel: *mut sys::ast_channel) -> *mut sys::ast_bridge {
    let Some(channel) = (unsafe { lock_channel(channel) }) else {
        return ptr::null_mut();
    };
    unsafe { sys::ast_channel_get_bridge(channel.as_ptr().cast()) }
}

pub fn set_channel_parking_lot(
    channel: &AsteriskChannel<'_>,
    lot: &str,
) -> Result<(), CallFeatureError> {
    const OPERATION: &str = "set channel parking lot";
    let lot = CString::new(lot).map_err(|_| CallFeatureError::InvalidText {
        field: "parking lot",
    })?;
    let Some(channel) = (unsafe { lock_channel(channel.as_raw().cast()) }) else {
        return Err(CallFeatureError::NativeFailure {
            operation: OPERATION,
        });
    };
    unsafe { sys::ast_channel_parkinglot_set(channel.as_ptr(), lot.as_ptr()) };
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn configure_jitter_buffer(
    channel: &AsteriskChannel<'_>,
    enabled: bool,
    forced: bool,
    log_frames: bool,
    max_size_ms: u32,
    resync_threshold_ms: u32,
    implementation: JitterBufferImplementation,
) -> Result<(), CallFeatureError> {
    const OPERATION: &str = "configure jitter buffer";
    if max_size_ms == 0
        || max_size_ms > c_int::MAX as u32
        || resync_threshold_ms == 0
        || resync_threshold_ms > c_int::MAX as u32
    {
        return Err(CallFeatureError::InvalidInput {
            operation: OPERATION,
        });
    }
    let implementation_name = match implementation {
        JitterBufferImplementation::Fixed => b"fixed\0".as_slice(),
        JitterBufferImplementation::Adaptive => b"adaptive\0".as_slice(),
    };
    unsafe {
        let mut config = mem::zeroed::<sys::ast_jb_conf>();
        sys::ast_jb_conf_default(&mut config);
        config.flags = (if enabled { sys::AST_JB_ENABLED } else { 0 })
            | (if forced { sys::AST_JB_FORCED } else { 0 })
            | (if log_frames { sys::AST_JB_LOG } else { 0 });
        config.max_size = max_size_ms.into();
        config.resync_threshold = resync_threshold_ms.into();
        for (target, source) in config
            .impl_
            .iter_mut()
            .zip(implementation_name.iter().copied())
        {
            *target = source as c_char;
        }
        sys::ast_jb_configure(channel.as_raw().cast(), &config);
    }
    Ok(())
}

pub fn channel_name(channel: &ChannelRef) -> Result<Option<String>, CallFeatureError> {
    const OPERATION: &str = "read channel name";
    const MAXIMUM_CHANNEL_NAME_BYTES: usize = 256;
    let name = unsafe { sys::ast_channel_name(channel.as_ptr()) };
    if name.is_null() {
        return Ok(None);
    }
    let name = unsafe { required_c_text(name, MAXIMUM_CHANNEL_NAME_BYTES) }.map_err(|_| {
        CallFeatureError::NativeFailure {
            operation: OPERATION,
        }
    })?;
    Ok((!name.is_empty()).then_some(name))
}
