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

use crate::asterisk::raw::handles::{Ao2Object, ChannelLock, ChannelRef};
use crate::asterisk::raw::registry::contain_callback_panic;
use crate::asterisk::sys;
use crate::presence::hints::{
    ExtensionState, Hint, HintBackend, HintCaller, HintError, HintUpdate, HintUpdateReason,
    OwnedHintCallback,
};

const SOURCE_FILE: &CStr = c"asterisk/native/presence/hints.rs";
const SOURCE_FUNCTION: &CStr = c"sccp_presence";

struct HintWatcher {
    id: AtomicI32,
    watcher_reference: AtomicBool,
    callback: OwnedHintCallback,
}

impl HintWatcher {
    fn deliver(&self, update: HintUpdate) -> c_int {
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
            (*value).ts = 1usize as *mut sys::ast_threadstorage;
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

fn lossy_text(value: *const c_char) -> String {
    if value.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(value) }
            .to_string_lossy()
            .into_owned()
    }
}

unsafe fn hint_caller(info: &sys::ast_state_cb_info) -> Option<HintCaller> {
    if info.device_state_info.is_null() {
        return None;
    }
    let mut result = None;
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
        let mut inspected_channel = false;
        if let Some(channel) = unsafe { ChannelRef::acquire(channel) } {
            if let Ok(channel) = ChannelLock::acquire(channel) {
                inspected_channel = true;
                let caller = unsafe { sys::ast_channel_caller(channel.as_ptr()) };
                if let Some(caller) = unsafe { caller.as_ref() } {
                    result = Some(HintCaller {
                        name: (caller.id.name.valid != 0).then(|| lossy_text(caller.id.name.str_)),
                        number: (caller.id.number.valid != 0)
                            .then(|| lossy_text(caller.id.number.str_)),
                        name_presentation: caller.id.name.presentation,
                        number_presentation: caller.id.number.presentation,
                    });
                }
            }
        }
        if inspected_channel {
            break;
        }
    }
    unsafe { sys::ao2_iterator_destroy(&mut iterator) };
    result
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
        let update = HintUpdate {
            context: lossy_text(context),
            extension: lossy_text(extension),
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
        release_watcher_reference(userdata.cast());
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

impl HintBackend for NativeHintAdapter {
    type Subscription = NativeHintSubscription;

    fn lookup(&self, context: &str, extension: &str) -> Result<Option<Hint>, HintError> {
        let context =
            CString::new(context).map_err(|_| HintError::InvalidText { field: "context" })?;
        let extension =
            CString::new(extension).map_err(|_| HintError::InvalidText { field: "extension" })?;
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
        Ok(Some(Hint {
            devices: devices.text("hint devices")?,
            name: name.text("hint name")?,
            state: hint_state(unsafe {
                sys::ast_extension_state(ptr::null_mut(), context.as_ptr(), extension.as_ptr())
            }),
        }))
    }

    fn subscribe(
        &self,
        context: &str,
        extension: &str,
        callback: OwnedHintCallback,
    ) -> Result<Self::Subscription, HintError> {
        let context =
            CString::new(context).map_err(|_| HintError::InvalidText { field: "context" })?;
        let extension =
            CString::new(extension).map_err(|_| HintError::InvalidText { field: "extension" })?;
        let watcher = Arc::new(HintWatcher {
            id: AtomicI32::new(-1),
            watcher_reference: AtomicBool::new(true),
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
