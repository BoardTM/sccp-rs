//! Mixing-bridge and barge-bridge ownership.

use std::ffi::{CStr, CString, c_int, c_uint};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{ao2_ref, current_bridge, lock_channel};
use crate::asterisk::direct::module_info;
use crate::asterisk::raw::handles::{ChannelRef, ModuleReference};
use crate::asterisk::sys;
use crate::pbx::operations::{BargeBridgeControl, BridgeControl, CallFeatureError};
use crate::pbx::party::AsteriskChannel;

const MAX_MERGE_CHANNELS: usize = 16;

struct BridgeReference(*mut sys::ast_bridge);

impl Drop for BridgeReference {
    fn drop(&mut self) {
        unsafe { ao2_ref(self.0.cast(), -1) };
    }
}

struct BridgeBinding {
    bridge: *mut sys::ast_bridge,
    anchor: Option<ChannelRef>,
    owns_bridge: bool,
}

impl BridgeBinding {
    unsafe fn destroy(&mut self, operation: &'static str) -> Result<(), CallFeatureError> {
        if self.bridge.is_null() {
            return Err(CallFeatureError::InvalidInput { operation });
        }
        let result = if self.owns_bridge {
            let result = unsafe {
                sys::ast_bridge_destroy(self.bridge, sys::AST_CAUSE_NORMAL_CLEARING as c_int)
            };
            if result == 0 {
                Ok(())
            } else {
                Err(CallFeatureError::NativeFailure { operation })
            }
        } else {
            unsafe { ao2_ref(self.bridge.cast(), -1) };
            Ok(())
        };
        self.bridge = ptr::null_mut();
        drop(self.anchor.take());
        result
    }
}

impl Drop for BridgeBinding {
    fn drop(&mut self) {
        if !self.bridge.is_null() {
            let _ = unsafe { self.destroy("drop bridge") };
        }
    }
}

unsafe fn bridge_create(
    bridge_id: &CStr,
    operation: &'static str,
) -> Result<Box<BridgeBinding>, CallFeatureError> {
    let existing = unsafe { sys::ast_bridge_find_by_id(bridge_id.as_ptr()) };
    if !existing.is_null() {
        unsafe { ao2_ref(existing.cast(), -1) };
        return Err(CallFeatureError::Conflict { operation });
    }
    let bridge = unsafe {
        sys::ast_bridge_base_new(
            sys::AST_BRIDGE_CAPABILITY_1TO1MIX | sys::AST_BRIDGE_CAPABILITY_MULTIMIX,
            sys::AST_BRIDGE_FLAG_SMART as c_uint,
            c"SCCP".as_ptr(),
            bridge_id.as_ptr(),
            bridge_id.as_ptr(),
        )
    };
    if bridge.is_null() {
        let existing = unsafe { sys::ast_bridge_find_by_id(bridge_id.as_ptr()) };
        if !existing.is_null() {
            unsafe { ao2_ref(existing.cast(), -1) };
            return Err(CallFeatureError::Conflict { operation });
        }
        return Err(CallFeatureError::NativeFailure { operation });
    }
    Ok(Box::new(BridgeBinding {
        bridge,
        anchor: None,
        owns_bridge: true,
    }))
}

unsafe fn bridge_add(
    binding: &BridgeBinding,
    channel: *mut sys::ast_channel,
    operation: &'static str,
) -> Result<(), CallFeatureError> {
    if binding.bridge.is_null() || channel.is_null() {
        return Err(CallFeatureError::InvalidInput { operation });
    }
    let existing = unsafe { current_bridge(channel) };
    if !existing.is_null() {
        unsafe { ao2_ref(existing.cast(), -1) };
        return Err(CallFeatureError::Conflict { operation });
    }
    let Some(channel) = (unsafe { ChannelRef::acquire(channel) }) else {
        return Err(CallFeatureError::NativeFailure { operation });
    };
    if unsafe {
        sys::ast_bridge_impart(
            binding.bridge,
            channel.as_ptr().cast(),
            ptr::null_mut(),
            ptr::null_mut(),
            sys::AST_BRIDGE_IMPART_CHAN_INDEPENDENT,
        )
    } != 0
    {
        Err(CallFeatureError::NativeFailure { operation })
    } else {
        let _ = channel.into_raw();
        Ok(())
    }
}

unsafe fn bridge_remove(
    binding: &BridgeBinding,
    channel: *mut sys::ast_channel,
    operation: &'static str,
) -> Result<(), CallFeatureError> {
    if binding.bridge.is_null() || channel.is_null() {
        return Err(CallFeatureError::InvalidInput { operation });
    }
    let existing = unsafe { current_bridge(channel) };
    if existing.is_null() {
        return Err(CallFeatureError::NotFound { operation });
    }
    let matches = existing == binding.bridge;
    unsafe { ao2_ref(existing.cast(), -1) };
    if !matches {
        return Err(CallFeatureError::Conflict { operation });
    }
    if unsafe { sys::ast_bridge_remove(binding.bridge, channel.cast()) } != 0 {
        Err(CallFeatureError::NativeFailure { operation })
    } else {
        Ok(())
    }
}

unsafe fn bridge_merge_channels(
    binding: &BridgeBinding,
    channels: &[*mut sys::ast_channel],
    operation: &'static str,
) -> Result<(), CallFeatureError> {
    if binding.bridge.is_null() || !(2..=MAX_MERGE_CHANNELS).contains(&channels.len()) {
        return Err(CallFeatureError::InvalidInput { operation });
    }
    let mut sources = Vec::<BridgeReference>::with_capacity(channels.len());
    for (index, &channel) in channels.iter().enumerate() {
        if channel.is_null() {
            return Err(CallFeatureError::InvalidInput { operation });
        }
        let source = unsafe { current_bridge(channel) };
        if source.is_null() {
            return Err(CallFeatureError::NotFound { operation });
        }
        let source = BridgeReference(source);
        if source.0 == binding.bridge
            || channels[..index].contains(&channel)
            || sources.iter().any(|existing| existing.0 == source.0)
        {
            return Err(CallFeatureError::Conflict { operation });
        }
        sources.push(source);
    }

    for (merged_count, source) in sources.iter().enumerate() {
        if unsafe { sys::ast_bridge_merge(binding.bridge, source.0, 0, ptr::null_mut(), 0) } != 0 {
            if merged_count > 0 {
                let _ = unsafe {
                    sys::ast_bridge_merge(sources[0].0, binding.bridge, 0, ptr::null_mut(), 0)
                };
            }
            return Err(CallFeatureError::NativeFailure { operation });
        }
    }
    Ok(())
}

unsafe fn bridge_merge_participant(
    binding: &BridgeBinding,
    channel: *mut sys::ast_channel,
) -> Result<(), CallFeatureError> {
    const OPERATION: &str = "merge conference participant";
    if binding.bridge.is_null() || channel.is_null() {
        return Err(CallFeatureError::InvalidInput {
            operation: OPERATION,
        });
    }
    let source = unsafe { current_bridge(channel) };
    if source.is_null() {
        return Err(CallFeatureError::NotFound {
            operation: OPERATION,
        });
    }
    let source = BridgeReference(source);
    if source.0 == binding.bridge {
        Err(CallFeatureError::Conflict {
            operation: OPERATION,
        })
    } else if unsafe { sys::ast_bridge_merge(binding.bridge, source.0, 0, ptr::null_mut(), 0) } == 0
    {
        Ok(())
    } else {
        Err(CallFeatureError::NativeFailure {
            operation: OPERATION,
        })
    }
}

unsafe fn verified_bridge_member(
    binding: &BridgeBinding,
    channel: *mut sys::ast_channel,
) -> Result<*mut sys::ast_bridge, CallFeatureError> {
    const OPERATION: &str = "verify conference participant";
    if binding.bridge.is_null() || channel.is_null() {
        return Err(CallFeatureError::InvalidInput {
            operation: OPERATION,
        });
    }
    let existing = unsafe { current_bridge(channel) };
    if existing.is_null() {
        return Err(CallFeatureError::NotFound {
            operation: OPERATION,
        });
    }
    if existing != binding.bridge {
        unsafe { ao2_ref(existing.cast(), -1) };
        return Err(CallFeatureError::Conflict {
            operation: OPERATION,
        });
    }
    Ok(existing)
}

unsafe fn bridge_set_participant_muted(
    binding: &BridgeBinding,
    channel: *mut sys::ast_channel,
    muted: bool,
) -> Result<(), CallFeatureError> {
    const OPERATION: &str = "set conference participant mute";
    if binding.bridge.is_null() || channel.is_null() {
        return Err(CallFeatureError::InvalidInput {
            operation: OPERATION,
        });
    }
    let Some(channel) = (unsafe { lock_channel(channel) }) else {
        return Err(CallFeatureError::NativeFailure {
            operation: OPERATION,
        });
    };
    let existing = unsafe { sys::ast_channel_get_bridge(channel.as_ptr().cast()) };
    if existing.is_null() {
        return Err(CallFeatureError::NotFound {
            operation: OPERATION,
        });
    }
    if existing != binding.bridge {
        unsafe { ao2_ref(existing.cast(), -1) };
        return Err(CallFeatureError::Conflict {
            operation: OPERATION,
        });
    }
    let result = if muted {
        unsafe {
            sys::ast_channel_suppress(
                channel.as_ptr().cast(),
                sys::AST_MUTE_DIRECTION_READ,
                sys::AST_FRAME_VOICE,
            )
        }
    } else {
        unsafe {
            sys::ast_channel_unsuppress(
                channel.as_ptr().cast(),
                sys::AST_MUTE_DIRECTION_READ,
                sys::AST_FRAME_VOICE,
            )
        }
    };
    unsafe { ao2_ref(existing.cast(), -1) };
    if result == 0 {
        Ok(())
    } else {
        Err(CallFeatureError::NativeFailure {
            operation: OPERATION,
        })
    }
}

unsafe fn bridge_set_participant_music_on_hold(
    binding: &BridgeBinding,
    channel: *mut sys::ast_channel,
    class_name: &CStr,
    enabled: bool,
) -> Result<(), CallFeatureError> {
    const OPERATION: &str = "set conference participant music on hold";
    let existing = unsafe { verified_bridge_member(binding, channel) }?;
    unsafe { ao2_ref(existing.cast(), -1) };
    let result = if enabled {
        unsafe { sys::ast_moh_start(channel.cast(), class_name.as_ptr(), ptr::null()) }
    } else {
        unsafe { sys::ast_moh_stop(channel.cast()) };
        0
    };
    if result == 0 {
        Ok(())
    } else {
        Err(CallFeatureError::NativeFailure {
            operation: OPERATION,
        })
    }
}

unsafe fn bridge_remove_participant_and_hangup(
    binding: &BridgeBinding,
    channel: *mut sys::ast_channel,
) -> Result<(), CallFeatureError> {
    const OPERATION: &str = "remove conference participant";
    let existing = unsafe { verified_bridge_member(binding, channel) }?;
    unsafe { ao2_ref(existing.cast(), -1) };
    if unsafe {
        sys::ast_queue_hangup_with_cause(channel.cast(), sys::AST_CAUSE_NORMAL_CLEARING as c_int)
    } == 0
    {
        Ok(())
    } else {
        Err(CallFeatureError::NativeFailure {
            operation: OPERATION,
        })
    }
}

// === Typed bridge ownership ==================================================

/// Rust owner for one created SCCP mixing bridge.
struct NativeBridgeSession {
    binding: Option<Box<BridgeBinding>>,
}

/// Rust owner for one borrowed or newly created barge bridge.
struct NativeBargeBridgeSession {
    binding: Option<Box<BridgeBinding>>,
}

unsafe impl Send for NativeBridgeSession {}
unsafe impl Send for NativeBargeBridgeSession {}

fn binding<'a>(
    binding: &'a Option<Box<BridgeBinding>>,
    operation: &'static str,
) -> Result<&'a BridgeBinding, CallFeatureError> {
    binding
        .as_deref()
        .ok_or(CallFeatureError::InvalidInput { operation })
}

pub fn create_bridge(bridge_id: &str) -> Result<Box<dyn BridgeControl>, CallFeatureError> {
    let bridge_id = CString::new(bridge_id).map_err(|_| CallFeatureError::InvalidText {
        field: "bridge identifier",
    })?;
    let binding = unsafe { bridge_create(&bridge_id, "create bridge") }?;
    Ok(Box::new(NativeBridgeSession {
        binding: Some(binding),
    }))
}

pub fn acquire_barge_bridge(
    bridge_id: &str,
    target: &AsteriskChannel<'_>,
) -> Result<Box<dyn BargeBridgeControl>, CallFeatureError> {
    let bridge_id = CString::new(bridge_id).map_err(|_| CallFeatureError::InvalidText {
        field: "bridge identifier",
    })?;
    let target = target.as_raw().cast();
    let existing = unsafe { current_bridge(target) };
    let binding = if existing.is_null() {
        let mut binding = unsafe { bridge_create(&bridge_id, "acquire barge bridge") }?;
        binding.anchor = unsafe { ChannelRef::acquire(target) };
        if binding.anchor.is_none() {
            return Err(CallFeatureError::NativeFailure {
                operation: "acquire barge bridge",
            });
        }
        unsafe { sys::ast_setstate(target, sys::AST_STATE_UP) };
        if unsafe { bridge_add(&binding, target, "acquire barge bridge") }.is_err() {
            return Err(CallFeatureError::Conflict {
                operation: "acquire barge bridge",
            });
        }
        binding
    } else {
        let Some(anchor) = (unsafe { ChannelRef::acquire(target) }) else {
            unsafe { ao2_ref(existing.cast(), -1) };
            return Err(CallFeatureError::NativeFailure {
                operation: "acquire barge bridge",
            });
        };
        Box::new(BridgeBinding {
            bridge: existing,
            anchor: Some(anchor),
            owns_bridge: false,
        })
    };
    Ok(Box::new(NativeBargeBridgeSession {
        binding: Some(binding),
    }))
}

impl BridgeControl for NativeBridgeSession {
    fn add(&mut self, channel: &AsteriskChannel<'_>) -> Result<(), CallFeatureError> {
        unsafe {
            bridge_add(
                binding(&self.binding, "add bridge participant")?,
                channel.as_raw().cast(),
                "add bridge participant",
            )
        }
    }

    fn remove(&mut self, channel: &AsteriskChannel<'_>) -> Result<(), CallFeatureError> {
        unsafe {
            bridge_remove(
                binding(&self.binding, "remove bridge participant")?,
                channel.as_raw().cast(),
                "remove bridge participant",
            )
        }
    }

    fn merge_consultation(
        &mut self,
        original: &AsteriskChannel<'_>,
        consultation: &AsteriskChannel<'_>,
    ) -> Result<(), CallFeatureError> {
        let channels = [original.as_raw().cast(), consultation.as_raw().cast()];
        unsafe {
            bridge_merge_channels(
                binding(&self.binding, "merge conference consultation")?,
                &channels,
                "merge conference consultation",
            )
        }
    }

    fn merge_calls(&mut self, channels: &[AsteriskChannel<'_>]) -> Result<(), CallFeatureError> {
        let channels: Vec<*mut sys::ast_channel> = channels
            .iter()
            .map(|channel| channel.as_raw().cast())
            .collect();
        unsafe {
            bridge_merge_channels(
                binding(&self.binding, "merge conference calls")?,
                &channels,
                "merge conference calls",
            )
        }
    }

    fn merge_participant(&mut self, channel: &AsteriskChannel<'_>) -> Result<(), CallFeatureError> {
        unsafe {
            bridge_merge_participant(
                binding(&self.binding, "merge conference participant")?,
                channel.as_raw().cast(),
            )
        }
    }

    fn set_participant_muted(
        &mut self,
        channel: &AsteriskChannel<'_>,
        muted: bool,
    ) -> Result<(), CallFeatureError> {
        unsafe {
            bridge_set_participant_muted(
                binding(&self.binding, "set conference participant mute")?,
                channel.as_raw().cast(),
                muted,
            )
        }
    }

    fn set_participant_music_on_hold(
        &mut self,
        channel: &AsteriskChannel<'_>,
        class: &str,
        enabled: bool,
    ) -> Result<(), CallFeatureError> {
        let class = CString::new(class).map_err(|_| CallFeatureError::InvalidText {
            field: "music on hold class",
        })?;
        unsafe {
            bridge_set_participant_music_on_hold(
                binding(&self.binding, "set conference participant music on hold")?,
                channel.as_raw().cast(),
                &class,
                enabled,
            )
        }
    }

    fn remove_participant_and_hangup(
        &mut self,
        channel: &AsteriskChannel<'_>,
    ) -> Result<(), CallFeatureError> {
        unsafe {
            bridge_remove_participant_and_hangup(
                binding(&self.binding, "remove conference participant")?,
                channel.as_raw().cast(),
            )
        }
    }

    fn destroy(mut self: Box<Self>) -> Result<(), CallFeatureError> {
        let Some(mut binding) = self.binding.take() else {
            return Ok(());
        };
        unsafe { binding.destroy("destroy bridge") }
    }
}

impl BargeBridgeControl for NativeBargeBridgeSession {
    fn add(&mut self, channel: &AsteriskChannel<'_>) -> Result<(), CallFeatureError> {
        unsafe { sys::ast_setstate(channel.as_raw().cast(), sys::AST_STATE_UP) };
        unsafe {
            bridge_add(
                binding(&self.binding, "add barge participant")?,
                channel.as_raw().cast(),
                "add barge participant",
            )
        }
    }

    fn remove(&mut self, channel: &AsteriskChannel<'_>) -> Result<(), CallFeatureError> {
        unsafe {
            bridge_remove(
                binding(&self.binding, "remove barge participant")?,
                channel.as_raw().cast(),
                "remove barge participant",
            )
        }
    }

    fn release(mut self: Box<Self>) -> Result<(), CallFeatureError> {
        let Some(mut binding) = self.binding.take() else {
            return Ok(());
        };
        let remove_result = if let (true, Some(anchor)) = (binding.owns_bridge, &binding.anchor) {
            let anchor = anchor.as_ptr();
            match unsafe { bridge_remove(&binding, anchor, "release barge bridge") } {
                Err(CallFeatureError::NotFound { .. }) => Ok(()),
                result => result,
            }
        } else {
            Ok(())
        };
        let destroy_result = unsafe { binding.destroy("release barge bridge") };
        remove_result.and(destroy_result)
    }
}

pub struct ConferenceApplication {
    channel: ChannelRef,
    _module: ModuleReference,
    arguments: CString,
    cancelled: Arc<AtomicBool>,
}

pub struct ConferenceApplicationCancellation {
    channel: ChannelRef,
    cancelled: Arc<AtomicBool>,
}

impl ConferenceApplication {
    pub fn run(self) -> Result<(), CallFeatureError> {
        const OPERATION: &str = "run conference destination";
        if self.cancelled.load(Ordering::Acquire) {
            queue_normal_hangup(&self.channel);
            return Ok(());
        }
        let result = unsafe {
            sys::ast_pbx_exec_application(
                self.channel.as_ptr().cast(),
                c"ConfBridge".as_ptr(),
                self.arguments.as_ptr(),
            )
        };
        queue_normal_hangup(&self.channel);
        if result == 0 {
            Ok(())
        } else {
            Err(CallFeatureError::NativeFailure {
                operation: OPERATION,
            })
        }
    }
}

impl ConferenceApplicationCancellation {
    pub fn cancel(self) {
        self.cancelled.store(true, Ordering::Release);
        queue_normal_hangup(&self.channel);
    }
}

fn queue_normal_hangup(channel: &ChannelRef) {
    unsafe {
        if sys::ast_check_hangup(channel.as_ptr().cast()) == 0 {
            sys::ast_queue_hangup_with_cause(
                channel.as_ptr().cast(),
                sys::AST_CAUSE_NORMAL_CLEARING as c_int,
            );
        }
    }
}

pub fn prepare_conference_destination(
    channel: &AsteriskChannel<'_>,
    arguments: &str,
) -> Result<(ConferenceApplication, ConferenceApplicationCancellation), CallFeatureError> {
    const OPERATION: &str = "start conference destination";
    let arguments = CString::new(arguments).map_err(|_| CallFeatureError::InvalidText {
        field: "conference destination arguments",
    })?;
    if unsafe { sys::pbx_findapp(c"ConfBridge".as_ptr()) }.is_null() {
        return Err(CallFeatureError::Unavailable {
            operation: OPERATION,
        });
    }
    let module = unsafe { ModuleReference::acquire(module_info::module_self()) }.ok_or(
        CallFeatureError::NativeFailure {
            operation: OPERATION,
        },
    )?;
    let Some(channel) = (unsafe { ChannelRef::acquire(channel.as_raw().cast()) }) else {
        return Err(CallFeatureError::NativeFailure {
            operation: OPERATION,
        });
    };
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancellation = ConferenceApplicationCancellation {
        channel: channel.clone(),
        cancelled: Arc::clone(&cancelled),
    };
    Ok((
        ConferenceApplication {
            channel,
            _module: module,
            arguments,
            cancelled,
        },
        cancellation,
    ))
}
