//! Stable storage for Asterisk callback and technology descriptors.

use std::ffi::{CStr, c_int};
use std::ptr::{self, NonNull};
use std::rc::Rc;

use crate::asterisk::sys;

const SOURCE_FILE: &CStr = c"asterisk/direct/handles.rs";
const SOURCE_FUNCTION: &CStr = c"temporarily_unlocked_channel";
const SOURCE_VARIABLE: &CStr = c"channel";
const REGISTRATION_SOURCE_FUNCTION: &CStr = c"native_channel_registration";
struct TechnologyCapabilities(NonNull<sys::ast_format_cap>);

impl TechnologyCapabilities {
    unsafe fn new(formats: impl IntoIterator<Item = *mut sys::ast_format>) -> Option<Self> {
        let capabilities = NonNull::new(unsafe {
            sys::__ast_format_cap_alloc(
                sys::AST_FORMAT_CAP_FLAG_DEFAULT,
                c"SCCP technology capabilities".as_ptr(),
                SOURCE_FILE.as_ptr(),
                line!() as c_int,
                REGISTRATION_SOURCE_FUNCTION.as_ptr(),
            )
        })?;
        let owned = Self(capabilities);
        formats
            .into_iter()
            .all(|format| unsafe {
                sys::__ast_format_cap_append(
                    owned.0.as_ptr(),
                    format,
                    0,
                    c"SCCP technology capabilities".as_ptr(),
                    SOURCE_FILE.as_ptr(),
                    line!() as c_int,
                    REGISTRATION_SOURCE_FUNCTION.as_ptr(),
                ) == 0
            })
            .then_some(owned)
    }
}

impl Drop for TechnologyCapabilities {
    fn drop(&mut self) {
        unsafe { sys::__ao2_cleanup(self.0.as_ptr().cast()) };
    }
}

struct RegisteredChannelTechnology {
    technology: NonNull<sys::ast_channel_tech>,
    _capabilities: TechnologyCapabilities,
}

impl RegisteredChannelTechnology {
    unsafe fn register(
        technology: NonNull<sys::ast_channel_tech>,
        capabilities: TechnologyCapabilities,
    ) -> Option<Self> {
        unsafe { (*technology.as_ptr()).capabilities = capabilities.0.as_ptr() };
        if unsafe { sys::ast_channel_register(technology.as_ptr()) } != 0 {
            unsafe { (*technology.as_ptr()).capabilities = ptr::null_mut() };
            return None;
        }
        Some(Self {
            technology,
            _capabilities: capabilities,
        })
    }
}

impl Drop for RegisteredChannelTechnology {
    fn drop(&mut self) {
        unsafe {
            sys::ast_channel_unregister(self.technology.as_ptr());
            (*self.technology.as_ptr()).capabilities = ptr::null_mut();
        }
    }
}

struct RegisteredRtpGlue(NonNull<sys::ast_rtp_glue>);

impl RegisteredRtpGlue {
    unsafe fn register(
        glue: NonNull<sys::ast_rtp_glue>,
        module: *mut sys::ast_module,
    ) -> Option<Self> {
        (unsafe { sys::ast_rtp_glue_register2(glue.as_ptr(), module) } == 0).then_some(Self(glue))
    }
}

impl Drop for RegisteredRtpGlue {
    fn drop(&mut self) {
        unsafe { sys::ast_rtp_glue_unregister(self.0.as_ptr()) };
    }
}

struct RegisteredCli {
    entries: NonNull<sys::ast_cli_entry>,
    count: c_int,
}

impl RegisteredCli {
    unsafe fn register<const N: usize>(
        entries: NonNull<[sys::ast_cli_entry; N]>,
        module: *mut sys::ast_module,
    ) -> Option<Self> {
        let count = c_int::try_from(N).ok()?;
        let entries = entries.cast();
        if unsafe { sys::__ast_cli_register_multiple(entries.as_ptr(), count, module) } != 0 {
            unsafe { sys::ast_cli_unregister_multiple(entries.as_ptr(), count) };
            return None;
        }
        Some(Self { entries, count })
    }
}

impl Drop for RegisteredCli {
    fn drop(&mut self) {
        unsafe { sys::ast_cli_unregister_multiple(self.entries.as_ptr(), self.count) };
    }
}

/// Owns every native registration installed for the channel technology.
/// Rust drops these fields top-to-bottom: CLI, RTP glue, then technology.
pub(super) struct NativeChannelRegistration {
    _cli: RegisteredCli,
    _rtp: RegisteredRtpGlue,
    _technology: RegisteredChannelTechnology,
}

// Registration and removal run only on the serialized module lifecycle, and
// the native core owns callback synchronization after registration succeeds.
unsafe impl Send for NativeChannelRegistration {}

impl NativeChannelRegistration {
    pub(super) unsafe fn register<const N: usize>(
        technology: NonNull<sys::ast_channel_tech>,
        glue: NonNull<sys::ast_rtp_glue>,
        cli: NonNull<[sys::ast_cli_entry; N]>,
        module: *mut sys::ast_module,
        formats: impl IntoIterator<Item = *mut sys::ast_format>,
    ) -> Option<Self> {
        let capabilities = unsafe { TechnologyCapabilities::new(formats) }?;
        let technology =
            unsafe { RegisteredChannelTechnology::register(technology, capabilities) }?;
        let rtp = unsafe { RegisteredRtpGlue::register(glue, module) }?;
        let cli = unsafe { RegisteredCli::register(cli, module) }?;
        Some(Self {
            _cli: cli,
            _rtp: rtp,
            _technology: technology,
        })
    }
}

/// Temporarily releases a channel lock owned by the enclosing native callback
/// and restores it on the same thread before returning.
pub(super) struct TemporarilyUnlockedChannel {
    channel: NonNull<sys::ast_channel>,
    _same_thread: std::marker::PhantomData<Rc<()>>,
}

impl TemporarilyUnlockedChannel {
    pub(super) unsafe fn new(channel: NonNull<sys::ast_channel>) -> Self {
        unsafe {
            sys::__ao2_unlock(
                channel.as_ptr().cast(),
                SOURCE_FILE.as_ptr(),
                SOURCE_FUNCTION.as_ptr(),
                line!() as i32,
                SOURCE_VARIABLE.as_ptr(),
            );
        }
        Self {
            channel,
            _same_thread: std::marker::PhantomData,
        }
    }
}

impl Drop for TemporarilyUnlockedChannel {
    fn drop(&mut self) {
        let result = unsafe {
            sys::__ao2_lock(
                self.channel.as_ptr().cast(),
                sys::AO2_LOCK_REQ_MUTEX,
                SOURCE_FILE.as_ptr(),
                SOURCE_FUNCTION.as_ptr(),
                line!() as i32,
                SOURCE_VARIABLE.as_ptr(),
            )
        };
        debug_assert_eq!(result, 0);
    }
}
