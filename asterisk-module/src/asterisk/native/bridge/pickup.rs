//! Group/directed pickup policy and retained pickup-channel ownership.

use std::ffi::{CStr, CString, c_char, c_int};
use std::ptr;

use super::lock_channel;
use crate::asterisk::raw::handles::{ChannelLock, ChannelRef};
use crate::asterisk::sys;
use crate::pbx::operations::{CallFeatureError, PickupChannelControl};
use crate::pbx::party::AsteriskChannel;
use crate::runtime::backend::PickupOutcome;

fn owned_text(value: *const c_char) -> String {
    if value.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(value) }
            .to_string_lossy()
            .into_owned()
    }
}

struct NamedGroups(*mut sys::ast_namedgroups);

impl Drop for NamedGroups {
    fn drop(&mut self) {
        unsafe {
            sys::ast_unref_namedgroups(self.0);
        }
    }
}

unsafe fn configure_pickup_policy(
    channel: *mut sys::ast_channel,
    call_groups: u64,
    pickup_groups: u64,
    named_call_groups: &CStr,
    named_pickup_groups: &CStr,
    private_call: bool,
) -> Result<(), CallFeatureError> {
    const OPERATION: &str = "configure pickup policy";
    if channel.is_null() {
        return Err(CallFeatureError::InvalidInput {
            operation: OPERATION,
        });
    }
    let (named_call, named_pickup) = if private_call {
        (NamedGroups(ptr::null_mut()), NamedGroups(ptr::null_mut()))
    } else {
        let call = NamedGroups(unsafe { sys::ast_get_namedgroups(named_call_groups.as_ptr()) });
        let pickup = NamedGroups(unsafe { sys::ast_get_namedgroups(named_pickup_groups.as_ptr()) });
        if (!named_call_groups.to_bytes().is_empty() && call.0.is_null())
            || (!named_pickup_groups.to_bytes().is_empty() && pickup.0.is_null())
        {
            return Err(CallFeatureError::NativeFailure {
                operation: OPERATION,
            });
        }
        (call, pickup)
    };
    unsafe {
        sys::pbx_builtin_setvar_helper(
            channel.cast(),
            c"SCCP_PICKUP_PRIVATE".as_ptr(),
            if private_call {
                c"1".as_ptr()
            } else {
                c"0".as_ptr()
            },
        );
    }
    let Some(channel) = (unsafe { lock_channel(channel) }) else {
        return Err(CallFeatureError::NativeFailure {
            operation: OPERATION,
        });
    };
    unsafe {
        sys::ast_channel_callgroup_set(
            channel.as_ptr().cast(),
            if private_call { 0 } else { call_groups },
        );
        sys::ast_channel_pickupgroup_set(
            channel.as_ptr().cast(),
            if private_call { 0 } else { pickup_groups },
        );
        sys::ast_channel_named_callgroups_set(channel.as_ptr().cast(), named_call.0);
        sys::ast_channel_named_pickupgroups_set(channel.as_ptr().cast(), named_pickup.0);
    }
    Ok(())
}

unsafe fn pickup_private(channel: *mut sys::ast_channel) -> bool {
    let value =
        unsafe { sys::pbx_builtin_getvar_helper(channel.cast(), c"SCCP_PICKUP_PRIVATE".as_ptr()) };
    unsafe { sys::ast_true(value) != 0 }
}

unsafe fn pickup_allowed(channel: *mut sys::ast_channel, target: *mut sys::ast_channel) -> bool {
    !unsafe { pickup_private(target) }
        && (unsafe {
            sys::ast_channel_pickupgroup(channel.cast()) & sys::ast_channel_callgroup(target.cast())
        } != 0
            || unsafe {
                sys::ast_namedgroups_intersect(
                    sys::ast_channel_named_pickupgroups(channel.cast()),
                    sys::ast_channel_named_callgroups(target.cast()),
                ) != 0
            })
}

unsafe fn pickup_identity(identity: *const sys::ast_party_id) -> (String, String) {
    let Some(identity) = (unsafe { identity.as_ref() }) else {
        return (String::new(), String::new());
    };
    let name = if identity.name.valid != 0
        && (identity.name.presentation & sys::AST_PRES_RESTRICTION as c_int)
            == sys::AST_PRES_ALLOWED as c_int
    {
        owned_text(identity.name.str_)
    } else {
        String::new()
    };
    let number = if identity.number.valid != 0
        && (identity.number.presentation & sys::AST_PRES_RESTRICTION as c_int)
            == sys::AST_PRES_ALLOWED as c_int
    {
        owned_text(identity.number.str_)
    } else {
        String::new()
    };
    (name, number)
}

/// Owns the reference and lock returned by Asterisk's pickup searches.
struct LockedPickupTarget(ChannelLock);

impl LockedPickupTarget {
    fn as_ptr(&self) -> *mut sys::ast_channel {
        self.0.as_ptr()
    }
}

struct OwnedPickup {
    channel: ChannelRef,
    parties: PickupOutcome,
}

unsafe fn pickup_target(
    channel: *mut sys::ast_channel,
    target: LockedPickupTarget,
    answer: bool,
    operation: &'static str,
) -> Result<OwnedPickup, CallFeatureError> {
    let caller = unsafe { sys::ast_channel_caller(target.as_ptr().cast()) };
    let connected = unsafe { sys::ast_channel_connected(target.as_ptr().cast()) };
    let redirecting = unsafe { sys::ast_channel_redirecting(target.as_ptr().cast()) };
    let (calling_name, calling_number) = unsafe {
        pickup_identity(if caller.is_null() {
            ptr::null()
        } else {
            ptr::addr_of!((*caller).id)
        })
    };
    let (connected_name, connected_number) = unsafe {
        pickup_identity(if connected.is_null() {
            ptr::null()
        } else {
            ptr::addr_of!((*connected).id)
        })
    };
    let (redirecting_name, redirecting_number) = unsafe {
        pickup_identity(if redirecting.is_null() {
            ptr::null()
        } else {
            ptr::addr_of!((*redirecting).from)
        })
    };
    let pickup_result = unsafe { sys::ast_do_pickup(channel.cast(), target.as_ptr().cast()) };
    if pickup_result == 0 {
        if !answer {
            unsafe { sys::ast_setstate(target.as_ptr().cast(), sys::AST_STATE_RINGING) };
        }
        let channel = target.0.clone_channel();
        Ok(OwnedPickup {
            channel,
            parties: PickupOutcome {
                calling_name,
                calling_number,
                connected_name,
                connected_number,
                redirecting_name,
                redirecting_number,
            },
        })
    } else {
        Err(CallFeatureError::NativeFailure { operation })
    }
}

unsafe fn pickup_group_native(
    channel: *mut sys::ast_channel,
    answer: bool,
) -> Result<OwnedPickup, CallFeatureError> {
    const OPERATION: &str = "group pickup";
    if channel.is_null() {
        return Err(CallFeatureError::InvalidInput {
            operation: OPERATION,
        });
    }
    let target: *mut sys::ast_channel =
        unsafe { sys::ast_pickup_find_by_group(channel.cast()).cast() };
    if target.is_null() {
        return Err(CallFeatureError::NotFound {
            operation: OPERATION,
        });
    }
    let target = unsafe { ChannelRef::from_owned(target) }
        .map(|target| LockedPickupTarget(unsafe { ChannelLock::from_locked(target) }))
        .ok_or(CallFeatureError::NativeFailure {
            operation: OPERATION,
        })?;
    if unsafe { pickup_private(target.as_ptr()) } {
        return Err(CallFeatureError::Unavailable {
            operation: OPERATION,
        });
    }
    unsafe { pickup_target(channel, target, answer, OPERATION) }
}

unsafe fn pickup_directed_native(
    channel: *mut sys::ast_channel,
    extension: &CStr,
    context: &CStr,
    answer: bool,
) -> Result<OwnedPickup, CallFeatureError> {
    const OPERATION: &str = "directed pickup";
    if channel.is_null() {
        return Err(CallFeatureError::InvalidInput {
            operation: OPERATION,
        });
    }
    let iterator =
        unsafe { sys::ast_channel_iterator_by_exten_new(extension.as_ptr(), context.as_ptr()) };
    if iterator.is_null() {
        return Err(CallFeatureError::NativeFailure {
            operation: OPERATION,
        });
    }
    let mut selected = None;
    let mut denied = false;
    'outer: loop {
        let target: *mut sys::ast_channel =
            unsafe { sys::ast_channel_iterator_next(iterator).cast() };
        if target.is_null() {
            break;
        }
        let Some(target) = (unsafe { ChannelRef::from_owned(target) }) else {
            continue;
        };
        let Ok(mut target) = ChannelLock::acquire(target) else {
            continue;
        };
        if target.as_ptr() != channel && unsafe { sys::ast_can_pickup(target.as_ptr().cast()) } != 0
        {
            let Some(mut request) = (unsafe { ChannelRef::acquire(channel) }) else {
                continue;
            };
            let request_lock = loop {
                match ChannelLock::try_acquire(request) {
                    Ok(request) => break request,
                    Err(unlocked_request) => {
                        let unlocked_target = target.unlock();
                        request = unlocked_request;
                        std::thread::yield_now();
                        let Ok(relocked_target) = ChannelLock::acquire(unlocked_target) else {
                            break 'outer;
                        };
                        target = relocked_target;
                    }
                }
            };
            if unsafe { sys::ast_can_pickup(target.as_ptr().cast()) } != 0 {
                if unsafe { pickup_allowed(request_lock.as_ptr(), target.as_ptr()) } {
                    drop(request_lock);
                    selected = Some(LockedPickupTarget(target));
                    break;
                }
                denied = true;
            }
            drop(request_lock);
        }
    }
    unsafe { sys::ast_channel_iterator_destroy(iterator) };
    match selected {
        Some(target) => unsafe { pickup_target(channel, target, answer, OPERATION) },
        None if denied => Err(CallFeatureError::Unavailable {
            operation: OPERATION,
        }),
        None => Err(CallFeatureError::NotFound {
            operation: OPERATION,
        }),
    }
}

// === Typed pickup operations ================================================

/// One retained channel reference returned by a successful pickup.
pub struct NativePickupChannel {
    channel: ChannelRef,
}

unsafe impl Send for NativePickupChannel {}

impl NativePickupChannel {
    pub fn into_channel_ref(self) -> ChannelRef {
        self.channel
    }
}

impl PickupChannelControl for NativePickupChannel {
    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any + Send> {
        self
    }
}

fn typed_pickup_result(result: OwnedPickup) -> (NativePickupChannel, PickupOutcome) {
    (
        NativePickupChannel {
            channel: result.channel,
        },
        result.parties,
    )
}

pub fn pickup_group(
    channel: &AsteriskChannel<'_>,
    answer: bool,
) -> Result<(NativePickupChannel, PickupOutcome), CallFeatureError> {
    let result = unsafe { pickup_group_native(channel.as_raw().cast(), answer) }?;
    Ok(typed_pickup_result(result))
}

pub fn pickup_directed(
    channel: &AsteriskChannel<'_>,
    extension: &str,
    context: &str,
    answer: bool,
) -> Result<(NativePickupChannel, PickupOutcome), CallFeatureError> {
    let extension = CString::new(extension)
        .map_err(|_| CallFeatureError::InvalidText { field: "extension" })?;
    let context =
        CString::new(context).map_err(|_| CallFeatureError::InvalidText { field: "context" })?;
    let result =
        unsafe { pickup_directed_native(channel.as_raw().cast(), &extension, &context, answer) }?;
    Ok(typed_pickup_result(result))
}

pub fn configure_pickup(
    channel: &AsteriskChannel<'_>,
    call_groups: u64,
    pickup_groups: u64,
    named_call_groups: &str,
    named_pickup_groups: &str,
    private_call: bool,
) -> Result<(), CallFeatureError> {
    let named_call_groups =
        CString::new(named_call_groups).map_err(|_| CallFeatureError::InvalidText {
            field: "named call groups",
        })?;
    let named_pickup_groups =
        CString::new(named_pickup_groups).map_err(|_| CallFeatureError::InvalidText {
            field: "named pickup groups",
        })?;
    unsafe {
        configure_pickup_policy(
            channel.as_raw().cast(),
            call_groups,
            pickup_groups,
            &named_call_groups,
            &named_pickup_groups,
            private_call,
        )
    }
}
