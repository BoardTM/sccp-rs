//! Access to SCCP-owned channel metadata.

use std::ptr::NonNull;

use crate::asterisk::sys;

use super::allocation::{
    ChannelIdentity, NativeChannelSecurity, channel_private, private_identity, private_security,
    take_private_identity,
};

pub unsafe fn channel_identity(channel: NonNull<sys::ast_channel>) -> Option<ChannelIdentity> {
    let private = unsafe { channel_private(channel.as_ptr()) }?;
    unsafe { private_identity(private) }
}

pub unsafe fn channel_pbx_id(channel: NonNull<sys::ast_channel>) -> Option<u64> {
    Some(unsafe { channel_identity(channel) }?.pbx_id)
}

pub unsafe fn take_channel_identity(channel: NonNull<sys::ast_channel>) -> Option<ChannelIdentity> {
    let private = unsafe { channel_private(channel.as_ptr()) }?;
    unsafe { take_private_identity(private) }
}

pub unsafe fn channel_security(
    channel: NonNull<sys::ast_channel>,
) -> Option<NativeChannelSecurity> {
    let private = unsafe { channel_private(channel.as_ptr()) }?;
    Some(unsafe { private_security(private) })
}
