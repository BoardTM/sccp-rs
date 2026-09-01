//! Asterisk-native extension hint lookup and subscription ownership.
//!
//!
//! A watcher owns the Rust callback and one Asterisk registry reference. The
//! destroy callback releases that registry reference exactly once; the RAII
//! subscription unregisters the watcher when its Rust owner is dropped.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::mem;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use crate::asterisk::boundary::nullable_lossy_c_text;
use crate::asterisk::raw::handles::{Ao2Object, ChannelLock, ChannelRef};
use crate::asterisk::raw::registry::contain_callback_panic;
use crate::asterisk::sys;
use crate::config::HintTarget;
use crate::presence::hints::{
    ExtensionState, HintCallback, HintCaller, HintError, HintProvider, HintSnapshot,
    HintUpdateReason,
};

const SOURCE_FILE: &CStr = c"asterisk/native/presence/hints.rs";
const SOURCE_FUNCTION: &CStr = c"sccp_presence";

struct HintWatcher {
    id: AtomicI32,
    watcher_reference: AtomicBool,
    target: HintTarget,
    callback: HintCallback,
}

impl HintWatcher {
    fn deliver(&self, update: HintSnapshot) -> c_int {
        (self.callback)(update);
        0
    }
}

pub struct NativeHintSubscription {
    watcher: Arc<HintWatcher>,
}

impl Drop for NativeHintSubscription {
    fn drop(&mut self) {
        let id = self.watcher.id.load(Ordering::Acquire);
        if id >= 0 {
            unsafe { sys::ast_extension_state_del(id, None) };
        }
    }
}

unsafe fn ast_free(value: *mut c_void) {
    if !value.is_null() {
        unsafe {
            sys::__ast_free(
                value,
                SOURCE_FILE.as_ptr(),
                line!() as c_int,
                SOURCE_FUNCTION.as_ptr(),
            );
        }
    }
}

struct AstBuffer(*mut sys::ast_str);

impl AstBuffer {
    fn new(capacity: usize) -> Result<Self, HintError> {
        let allocation_size = mem::size_of::<sys::ast_str>()
            .checked_add(capacity)
            .ok_or(HintError::LookupFailed)?;
        let value = unsafe {
            sys::__ast_calloc(
                1,
                allocation_size,
                SOURCE_FILE.as_ptr(),
                line!() as c_int,
                SOURCE_FUNCTION.as_ptr(),
            )
        }
        .cast::<sys::ast_str>();
        if value.is_null() {
            return Err(HintError::LookupFailed);
        }
        unsafe {
            (*value).len = capacity;
            (*value).used = 0;
            (*value).ts = ptr::dangling_mut::<sys::ast_threadstorage>();
        }
        Ok(Self(value))
    }

    fn as_mut_ptr(&mut self) -> *mut *mut sys::ast_str {
        &mut self.0
    }

    fn text(&self, field: &'static str) -> Result<String, HintError> {
        let pointer = if self.0.is_null() || unsafe { (*self.0).len } == 0 {
            c"".as_ptr()
        } else {
            unsafe { (*self.0).str_.as_ptr() }
        };
        unsafe { CStr::from_ptr(pointer) }
            .to_str()
            .map(str::to_owned)
            .map_err(|_| HintError::InvalidUtf8 { field })
    }
}

impl Drop for AstBuffer {
    fn drop(&mut self) {
        unsafe { ast_free(self.0.cast()) };
    }
}

const fn hint_state(state: c_int) -> ExtensionState {
    if state == sys::AST_EXTENSION_REMOVED as c_int {
        return ExtensionState::REMOVED;
    }
    if state == sys::AST_EXTENSION_DEACTIVATED as c_int {
        return ExtensionState::DEACTIVATED;
    }
    let mut mapped = ExtensionState::IDLE.raw();
    if state & sys::AST_EXTENSION_INUSE as c_int != 0 {
        mapped |= ExtensionState::IN_USE.raw();
    }
    if state & sys::AST_EXTENSION_BUSY as c_int != 0 {
        mapped |= ExtensionState::BUSY.raw();
    }
    if state & sys::AST_EXTENSION_UNAVAILABLE as c_int != 0 {
        mapped |= ExtensionState::UNAVAILABLE.raw();
    }
    if state & sys::AST_EXTENSION_RINGING as c_int != 0 {
        mapped |= ExtensionState::RINGING.raw();
    }
    if state & sys::AST_EXTENSION_ONHOLD as c_int != 0 {
        mapped |= ExtensionState::ON_HOLD.raw();
    }
    ExtensionState::from_raw(mapped)
}

unsafe fn hint_caller(info: &sys::ast_state_cb_info) -> Option<HintCaller> {
    if info.device_state_info.is_null() {
        return None;
    }
    let mut candidates = Vec::new();
    let mut iterator = unsafe { sys::ao2_iterator_init(info.device_state_info, 0) };
    loop {
        let device = unsafe {
            sys::__ao2_iterator_next(
                &mut iterator,
                ptr::null(),
                SOURCE_FILE.as_ptr(),
                line!() as c_int,
                SOURCE_FUNCTION.as_ptr(),
            )
        }
        .cast::<sys::ast_device_state_info>();
        let Some(device) = (unsafe { Ao2Object::from_owned(device) }) else {
            break;
        };
        let channel = unsafe { (*device.as_ptr()).causing_channel };
        if channel.is_null() {
            continue;
        }
        // Count every raw causal channel before retaining or locking it. A
        // failed acquisition is still a cause and makes another identity
        // ambiguous.
        let candidate = resolve_causal_candidate(
            || unsafe { ChannelRef::acquire(channel) },
            |channel| ChannelLock::acquire(channel).ok(),
            |channel| {
                let pickup_private = unsafe {
                    let value = sys::pbx_builtin_getvar_helper(
                        channel.as_ptr(),
                        c"SCCP_PICKUP_PRIVATE".as_ptr(),
                    );
                    sys::ast_true(value) != 0
                };
                let caller = unsafe { sys::ast_channel_caller(channel.as_ptr()) };
                let caller = unsafe { caller.as_ref() }.map(|caller| HintCaller {
                    name: (caller.id.name.valid != 0)
                        .then(|| unsafe { nullable_lossy_c_text(caller.id.name.str_) }),
                    number: (caller.id.number.valid != 0)
                        .then(|| unsafe { nullable_lossy_c_text(caller.id.number.str_) }),
                    name_presentation: caller.id.name.presentation,
                    number_presentation: caller.id.number.presentation,
                });
                visible_caller(pickup_private, caller)
            },
        );
        candidates.push(candidate);
    }
    unsafe { sys::ao2_iterator_destroy(&mut iterator) };
    unambiguous_caller(candidates)
}

/// Caller identity is meaningful only when Asterisk reports exactly one
/// causal channel. Selecting the first member of an aggregate would make
/// privacy and identity depend on AO2 iteration order.
fn unambiguous_caller(mut candidates: Vec<Option<HintCaller>>) -> Option<HintCaller> {
    if candidates.len() == 1 {
        candidates.pop().flatten()
    } else {
        None
    }
}

fn resolve_causal_candidate<Acquired, Locked>(
    acquire: impl FnOnce() -> Option<Acquired>,
    lock: impl FnOnce(Acquired) -> Option<Locked>,
    inspect: impl FnOnce(&Locked) -> Option<HintCaller>,
) -> Option<HintCaller> {
    let acquired = acquire()?;
    let locked = lock(acquired)?;
    inspect(&locked)
}

fn visible_caller(pickup_private: bool, caller: Option<HintCaller>) -> Option<HintCaller> {
    (!pickup_private)
        .then_some(caller)
        .flatten()
        .filter(HintCaller::presentation_allowed)
}

unsafe extern "C" fn hint_update(
    context: *const c_char,
    extension: *const c_char,
    info: *mut sys::ast_state_cb_info,
    userdata: *mut c_void,
) -> c_int {
    if info.is_null() || userdata.is_null() {
        return -1;
    }
    let watcher = userdata.cast::<HintWatcher>();
    unsafe { Arc::increment_strong_count(watcher) };
    let watcher = unsafe { Arc::from_raw(watcher) };
    contain_callback_panic(-1, || {
        let info = unsafe { &*info };
        let _ = (context, extension);
        let update = HintSnapshot {
            target: watcher.target.clone(),
            state: hint_state(info.exten_state as c_int),
            reason: if info.reason as c_int == sys::AST_HINT_UPDATE_PRESENCE as c_int {
                HintUpdateReason::Presence
            } else {
                HintUpdateReason::Device
            },
            caller: unsafe { hint_caller(info) },
        };
        watcher.deliver(update)
    })
}

unsafe extern "C" fn hint_watcher_destroy(_id: c_int, userdata: *mut c_void) {
    if userdata.is_null() {
        return;
    }
    contain_callback_panic((), || unsafe {
        let watcher = userdata.cast::<HintWatcher>();
        (*watcher).id.store(-1, Ordering::Release);
        release_watcher_reference(watcher);
    });
}

unsafe fn release_watcher_reference(watcher: *const HintWatcher) {
    let watcher_ref = unsafe { &*watcher };
    if watcher_ref
        .watcher_reference
        .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        drop(unsafe { Arc::from_raw(watcher) });
    }
}

pub struct NativeHintAdapter;

impl HintProvider for NativeHintAdapter {
    type Subscription = NativeHintSubscription;
    type Error = HintError;

    fn lookup(&self, target: &HintTarget) -> Result<Option<HintSnapshot>, HintError> {
        let context = CString::new(target.context())
            .map_err(|_| HintError::InvalidText { field: "context" })?;
        let extension = CString::new(target.extension())
            .map_err(|_| HintError::InvalidText { field: "extension" })?;
        let mut devices = AstBuffer::new(64)?;
        let mut name = AstBuffer::new(64)?;
        let found = unsafe {
            sys::ast_str_get_hint(
                devices.as_mut_ptr(),
                0,
                name.as_mut_ptr(),
                0,
                ptr::null_mut(),
                context.as_ptr(),
                extension.as_ptr(),
            )
        };
        if found == 0 {
            return Ok(None);
        }
        // Force strict UTF-8 validation of Asterisk's diagnostic fields even
        // though the unified semantic model intentionally does not expose
        // technology strings or hint labels.
        let _ = (devices.text("hint devices")?, name.text("hint name")?);
        Ok(Some(HintSnapshot {
            target: target.clone(),
            state: hint_state(unsafe {
                sys::ast_extension_state(ptr::null_mut(), context.as_ptr(), extension.as_ptr())
            }),
            reason: HintUpdateReason::Device,
            caller: None,
        }))
    }

    fn subscribe(
        &self,
        target: &HintTarget,
        callback: HintCallback,
    ) -> Result<Self::Subscription, HintError> {
        let context = CString::new(target.context())
            .map_err(|_| HintError::InvalidText { field: "context" })?;
        let extension = CString::new(target.extension())
            .map_err(|_| HintError::InvalidText { field: "extension" })?;
        let watcher = Arc::new(HintWatcher {
            id: AtomicI32::new(-1),
            watcher_reference: AtomicBool::new(true),
            target: target.clone(),
            callback,
        });
        let watcher_reference = Arc::into_raw(Arc::clone(&watcher));
        let id = unsafe {
            sys::ast_extension_state_add_destroy(
                context.as_ptr(),
                extension.as_ptr(),
                Some(hint_update),
                Some(hint_watcher_destroy),
                watcher_reference.cast_mut().cast(),
            )
        };
        if id < 0 {
            unsafe { release_watcher_reference(watcher_reference) };
            return Err(HintError::SubscribeFailed);
        }
        watcher.id.store(id, Ordering::Release);
        Ok(NativeHintSubscription { watcher })
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    fn caller(number: &str) -> HintCaller {
        HintCaller {
            name: Some("Desk".into()),
            number: Some(number.into()),
            name_presentation: 0,
            number_presentation: 0,
        }
    }

    #[test]
    fn caller_requires_exactly_one_causal_channel() {
        assert_eq!(unambiguous_caller(Vec::new()), None);
        assert_eq!(unambiguous_caller(vec![None]), None);
        assert_eq!(
            unambiguous_caller(vec![Some(caller("4000"))]),
            Some(caller("4000"))
        );
        assert_eq!(
            unambiguous_caller(vec![Some(caller("4000")), Some(caller("4000"))]),
            None
        );
        assert_eq!(
            unambiguous_caller(vec![Some(caller("4000")), Some(caller("5000"))]),
            None
        );
        assert_eq!(unambiguous_caller(vec![Some(caller("4000")), None]), None);
    }

    #[test]
    fn acquisition_failure_still_counts_as_an_ambiguous_raw_cause() {
        let lock_called = Cell::new(false);
        let inspect_called = Cell::new(false);
        let acquisition_failed = resolve_causal_candidate(
            || None::<()>,
            |_| {
                lock_called.set(true);
                Some(())
            },
            |_| {
                inspect_called.set(true);
                Some(caller("5000"))
            },
        );
        assert!(!lock_called.get());
        assert!(!inspect_called.get());
        assert_eq!(
            unambiguous_caller(vec![Some(caller("4000")), acquisition_failed]),
            None
        );
    }

    #[test]
    fn lock_failure_still_counts_as_an_ambiguous_raw_cause() {
        let inspect_called = Cell::new(false);
        let lock_failed = resolve_causal_candidate(
            || Some(()),
            |_| None::<()>,
            |_| {
                inspect_called.set(true);
                Some(caller("5000"))
            },
        );
        assert!(!inspect_called.get());
        assert_eq!(
            unambiguous_caller(vec![Some(caller("4000")), lock_failed]),
            None
        );
    }

    #[test]
    fn pickup_private_channel_never_exposes_caller() {
        assert_eq!(visible_caller(true, Some(caller("4000"))), None);
        assert_eq!(
            visible_caller(false, Some(caller("4000"))),
            Some(caller("4000"))
        );
    }

    #[test]
    fn restricted_presentation_never_crosses_the_native_boundary() {
        let mut restricted_name = caller("4000");
        restricted_name.name_presentation = 0x20;
        assert_eq!(visible_caller(false, Some(restricted_name)), None);

        let mut restricted_number = caller("4000");
        restricted_number.number_presentation = 0x20;
        assert_eq!(visible_caller(false, Some(restricted_number)), None);
    }
}
