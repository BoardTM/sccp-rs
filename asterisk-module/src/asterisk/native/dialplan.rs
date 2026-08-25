//! Asterisk-native custom-function and application registration.
//!
//! Stable Asterisk descriptors live in process-wide slots. Each compatible
//! registration installs a new typed Rust handler generation behind a bounded
//! callback-admission gate; dropping the public handle closes admission and
//! drains in-flight calls. A callback may unregister its own generation
//! without deadlocking because its lease retains the handler until callback
//! exit.
//!
//! Only the three callbacks invoked by Asterisk use the C ABI. They decode
//! Asterisk pointers into owned Rust requests, invoke the typed domain handler,
//! and encode the typed result directly into Asterisk's output contract.

use std::cell::UnsafeCell;
use std::ffi::{CStr, CString, c_char, c_int};
use std::mem;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, OnceLock};

use crate::asterisk::boundary::MutexExt as _;
use crate::asterisk::boundary::{CallbackStatus, write_c_text};
use crate::asterisk::direct::module_info::module_self;
use crate::asterisk::sys;
use crate::pbx::dialplan::{
    ApplicationHandler, DialplanApplicationInvocation, DialplanError, DialplanEscalation,
    DialplanFunctionHandlers, DialplanFunctionRead, DialplanFunctionWrite, DialplanLimits,
};
use crate::pbx::party::AsteriskChannel;

use super::registry::{CallbackRegistration, contain_callback_panic};

enum HandlerSet {
    Function(DialplanFunctionHandlers),
    Application(Box<ApplicationHandler>),
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Kind {
    Function {
        has_read: bool,
        has_write: bool,
        escalation: DialplanEscalation,
    },
    Application,
}

impl Kind {
    const fn is_function(self) -> bool {
        matches!(self, Self::Function { .. })
    }

    const fn matches_handlers(self, handlers: &HandlerSet) -> bool {
        matches!(
            (self, handlers),
            (Self::Function { .. }, HandlerSet::Function(_))
                | (Self::Application, HandlerSet::Application(_))
        )
    }
}

struct Generation {
    limits: DialplanLimits,
    handlers: Option<HandlerSet>,
}

impl Generation {
    fn handlers(&self) -> Option<&HandlerSet> {
        self.handlers.as_ref()
    }
}

impl Drop for Generation {
    fn drop(&mut self) {
        if let Some(handlers) = self.handlers.take() {
            drop(handlers);
        }
    }
}

struct Slot {
    kind: Kind,
    name: CString,
    synopsis: CString,
    description: CString,
    max_output_bytes: usize,
    function: UnsafeCell<sys::ast_custom_function>,
    current: Mutex<Option<Arc<CallbackRegistration<Generation>>>>,
}

// The stable descriptor is initialized before publication and then read only by
// Asterisk. Generation replacement is serialized by `current`.
unsafe impl Send for Slot {}
unsafe impl Sync for Slot {}

pub struct NativeDialplanRegistration {
    slot: Arc<Slot>,
    generation: Arc<CallbackRegistration<Generation>>,
}

impl Drop for NativeDialplanRegistration {
    fn drop(&mut self) {
        retire_generation(&self.slot, &self.generation);
    }
}

static SLOTS: OnceLock<Mutex<Vec<Arc<Slot>>>> = OnceLock::new();

fn slots() -> &'static Mutex<Vec<Arc<Slot>>> {
    SLOTS.get_or_init(|| Mutex::new(Vec::new()))
}

fn names_equal(kind: Kind, first: &CStr, second: &CStr) -> bool {
    if kind.is_function() {
        first.to_bytes() == second.to_bytes()
    } else {
        first.to_bytes().eq_ignore_ascii_case(second.to_bytes())
    }
}

unsafe fn lookup(
    function: bool,
    name: *const c_char,
) -> Option<(Arc<Slot>, Arc<CallbackRegistration<Generation>>)> {
    let name = if name.is_null() {
        c""
    } else {
        unsafe { CStr::from_ptr(name) }
    };
    let slot = slots()
        .lock_unpoisoned()
        .iter()
        .find(|slot| {
            slot.kind.is_function() == function && names_equal(slot.kind, &slot.name, name)
        })
        .cloned()?;
    let generation = slot.current.lock_unpoisoned().clone()?;
    Some((slot, generation))
}

#[derive(Clone, Copy, Debug)]
enum CallbackFailure {
    InvalidInput,
    Unavailable,
    HandlerFailed,
    InvalidOutput,
}

fn decode_owned(value: *const c_char, maximum_bytes: usize) -> Result<String, CallbackFailure> {
    if value.is_null() {
        return Ok(String::new());
    }
    let bytes = unsafe { CStr::from_ptr(value) }.to_bytes();
    if bytes.len() > maximum_bytes {
        return Err(CallbackFailure::InvalidInput);
    }
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| CallbackFailure::InvalidInput)
}

unsafe fn borrowed_channel<'a>(
    channel: *mut sys::ast_channel,
) -> Result<Option<AsteriskChannel<'a>>, CallbackFailure> {
    if channel.is_null() {
        return Ok(None);
    }
    unsafe { AsteriskChannel::from_raw(channel.cast()) }
        .map(Some)
        .map_err(|_| CallbackFailure::InvalidInput)
}

fn canonical_name(slot: &Slot) -> Result<String, CallbackFailure> {
    slot.name
        .to_str()
        .map(str::to_owned)
        .map_err(|_| CallbackFailure::Unavailable)
}

unsafe fn read_typed(
    channel: *mut sys::ast_channel,
    name: *const c_char,
    arguments: *mut c_char,
    output: *mut c_char,
    output_size: usize,
) -> Result<(), CallbackFailure> {
    let (slot, generation) = unsafe { lookup(true, name) }.ok_or(CallbackFailure::Unavailable)?;
    let lease = generation
        .enter()
        .map_err(|_| CallbackFailure::Unavailable)?;
    if output.is_null() || output_size == 0 {
        return Err(CallbackFailure::InvalidInput);
    }
    unsafe { *output = 0 };

    let payload = lease.payload();
    let Some(HandlerSet::Function(handlers)) = payload.handlers() else {
        return Err(CallbackFailure::Unavailable);
    };
    let handler = handlers.read().ok_or(CallbackFailure::Unavailable)?;
    let arguments = decode_owned(arguments, payload.limits.max_arguments_bytes)?;
    let channel = unsafe { borrowed_channel(channel) }?;
    let result = handler(DialplanFunctionRead {
        channel,
        name: canonical_name(&slot)?,
        arguments,
    })
    .map_err(|_| CallbackFailure::HandlerFailed)?;

    if result.len() > payload.limits.max_output_bytes {
        return Err(CallbackFailure::InvalidOutput);
    }
    unsafe { write_c_text(output, output_size, &result) }
        .map_err(|_| CallbackFailure::InvalidOutput)?;
    Ok(())
}

unsafe extern "C" fn function_read(
    channel: *mut sys::ast_channel,
    name: *const c_char,
    arguments: *mut c_char,
    output: *mut c_char,
    output_size: usize,
) -> c_int {
    contain_callback_panic(CallbackStatus::Failure.as_raw(), || {
        CallbackStatus::from_result(unsafe {
            read_typed(channel, name, arguments, output, output_size)
        })
        .as_raw()
    })
}

unsafe fn write_typed(
    channel: *mut sys::ast_channel,
    name: *const c_char,
    arguments: *mut c_char,
    value: *const c_char,
) -> Result<(), CallbackFailure> {
    let (slot, generation) = unsafe { lookup(true, name) }.ok_or(CallbackFailure::Unavailable)?;
    let lease = generation
        .enter()
        .map_err(|_| CallbackFailure::Unavailable)?;
    let payload = lease.payload();
    let Some(HandlerSet::Function(handlers)) = payload.handlers() else {
        return Err(CallbackFailure::Unavailable);
    };
    let handler = handlers.write().ok_or(CallbackFailure::Unavailable)?;
    let arguments = decode_owned(arguments, payload.limits.max_arguments_bytes)?;
    let value = decode_owned(value, payload.limits.max_value_bytes)?;
    let channel = unsafe { borrowed_channel(channel) }?;
    handler(DialplanFunctionWrite {
        channel,
        name: canonical_name(&slot)?,
        arguments,
        value,
    })
    .map_err(|_| CallbackFailure::HandlerFailed)
}

unsafe extern "C" fn function_write(
    channel: *mut sys::ast_channel,
    name: *const c_char,
    arguments: *mut c_char,
    value: *const c_char,
) -> c_int {
    contain_callback_panic(CallbackStatus::Failure.as_raw(), || {
        CallbackStatus::from_result(unsafe { write_typed(channel, name, arguments, value) })
            .as_raw()
    })
}

unsafe fn execute_typed(
    channel: *mut sys::ast_channel,
    arguments: *const c_char,
) -> Result<c_int, CallbackFailure> {
    if channel.is_null() {
        return Err(CallbackFailure::InvalidInput);
    }
    let application = unsafe { sys::ast_channel_appl(channel) };
    let (slot, generation) =
        unsafe { lookup(false, application) }.ok_or(CallbackFailure::Unavailable)?;
    let lease = generation
        .enter()
        .map_err(|_| CallbackFailure::Unavailable)?;
    let payload = lease.payload();
    let Some(HandlerSet::Application(handler)) = payload.handlers() else {
        return Err(CallbackFailure::Unavailable);
    };
    let arguments = decode_owned(arguments, payload.limits.max_arguments_bytes)?;
    let channel = unsafe { borrowed_channel(channel) }?.ok_or(CallbackFailure::InvalidInput)?;
    handler(DialplanApplicationInvocation {
        channel,
        name: canonical_name(&slot)?,
        arguments,
    })
    .map(|result| result.raw())
    .map_err(|_| CallbackFailure::HandlerFailed)
}

unsafe extern "C" fn application_execute(
    channel: *mut sys::ast_channel,
    arguments: *const c_char,
) -> c_int {
    contain_callback_panic(CallbackStatus::Failure.as_raw(), || {
        match unsafe { execute_typed(channel, arguments) } {
            Ok(result) => result,
            Err(_) => CallbackStatus::Failure.as_raw(),
        }
    })
}

unsafe fn register_with_asterisk(slot: &Arc<Slot>) -> c_int {
    match slot.kind {
        Kind::Function {
            has_read,
            has_write,
            escalation,
        } => {
            let mut function = unsafe { mem::zeroed::<sys::ast_custom_function>() };
            function.name = slot.name.as_ptr();
            function.synopsis = slot.synopsis.as_ptr();
            function.desc = slot.description.as_ptr();
            function.read = has_read.then_some(function_read);
            function.read_max = slot.max_output_bytes + 1;
            function.write = has_write.then_some(function_write);
            unsafe { *slot.function.get() = function };
            let escalation = match escalation {
                DialplanEscalation::None => sys::AST_CFE_NONE,
                DialplanEscalation::Read => sys::AST_CFE_READ,
                DialplanEscalation::Write => sys::AST_CFE_WRITE,
                DialplanEscalation::Both => sys::AST_CFE_BOTH,
            };
            unsafe {
                sys::__ast_custom_function_register_escalating(
                    slot.function.get(),
                    escalation,
                    module_self(),
                )
            }
        }
        Kind::Application => unsafe {
            sys::ast_register_application2(
                slot.name.as_ptr(),
                Some(application_execute),
                slot.synopsis.as_ptr(),
                slot.description.as_ptr(),
                module_self().cast(),
            )
        },
    }
}

fn register(
    kind: Kind,
    name: CString,
    synopsis: CString,
    description: CString,
    limits: DialplanLimits,
    handlers: HandlerSet,
) -> Result<NativeDialplanRegistration, DialplanError> {
    if !kind.matches_handlers(&handlers) {
        return Err(DialplanError::RegistrationFailed);
    }
    let generation = CallbackRegistration::new(
        NonZeroUsize::MAX,
        Generation {
            limits,
            handlers: Some(handlers),
        },
    );

    let mut registry = slots().lock_unpoisoned();
    if let Some(slot) = registry
        .iter()
        .find(|slot| {
            slot.kind.is_function() == kind.is_function() && names_equal(kind, &slot.name, &name)
        })
        .cloned()
    {
        if slot.kind != kind
            || slot.synopsis != synopsis
            || slot.description != description
            || slot.max_output_bytes != limits.max_output_bytes
        {
            drop(registry);
            drop(generation);
            return Err(DialplanError::RegistrationFailed);
        }
        let mut current = slot.current.lock_unpoisoned();
        if current.is_some() {
            drop(current);
            drop(registry);
            drop(generation);
            return Err(DialplanError::RegistrationFailed);
        }
        *current = Some(generation.clone());
        drop(current);
        drop(registry);
        return Ok(NativeDialplanRegistration { slot, generation });
    }

    let slot = Arc::new(Slot {
        kind,
        name,
        synopsis,
        description,
        max_output_bytes: limits.max_output_bytes,
        function: UnsafeCell::new(unsafe { mem::zeroed() }),
        current: Mutex::new(Some(generation.clone())),
    });
    if unsafe { register_with_asterisk(&slot) } != 0 {
        drop(registry);
        drop(slot);
        drop(generation);
        return Err(DialplanError::RegistrationFailed);
    }
    registry.push(slot.clone());
    drop(registry);
    Ok(NativeDialplanRegistration { slot, generation })
}

#[allow(clippy::too_many_arguments)]
pub fn register_dialplan_function(
    name: String,
    synopsis: String,
    description: String,
    escalation: DialplanEscalation,
    limits: DialplanLimits,
    handlers: DialplanFunctionHandlers,
) -> Result<NativeDialplanRegistration, DialplanError> {
    let name = CString::new(name).map_err(|_| DialplanError::RegistrationFailed)?;
    let synopsis = CString::new(synopsis).map_err(|_| DialplanError::RegistrationFailed)?;
    let description = CString::new(description).map_err(|_| DialplanError::RegistrationFailed)?;
    let kind = Kind::Function {
        has_read: handlers.has_read(),
        has_write: handlers.has_write(),
        escalation,
    };
    register(
        kind,
        name,
        synopsis,
        description,
        limits,
        HandlerSet::Function(handlers),
    )
}

pub fn register_dialplan_application(
    name: String,
    synopsis: String,
    description: String,
    limits: DialplanLimits,
    handler: Box<ApplicationHandler>,
) -> Result<NativeDialplanRegistration, DialplanError> {
    let name = CString::new(name).map_err(|_| DialplanError::RegistrationFailed)?;
    let synopsis = CString::new(synopsis).map_err(|_| DialplanError::RegistrationFailed)?;
    let description = CString::new(description).map_err(|_| DialplanError::RegistrationFailed)?;
    register(
        Kind::Application,
        name,
        synopsis,
        description,
        limits,
        HandlerSet::Application(handler),
    )
}

fn retire_generation(slot: &Slot, generation: &Arc<CallbackRegistration<Generation>>) {
    let mut current = slot.current.lock_unpoisoned();
    let is_current = current
        .as_ref()
        .is_some_and(|active| Arc::ptr_eq(active, generation));
    if is_current {
        generation.close_admission();
        *current = None;
    }
    drop(current);
    if is_current {
        let _ = generation.drain();
    }
}

pub fn cleanup() {
    let registrations = mem::take(&mut *slots().lock_unpoisoned());
    for slot in registrations {
        if let Some(generation) = slot.current.lock_unpoisoned().take() {
            generation.close_admission();
            let _ = generation.drain();
        }
        unsafe {
            if slot.kind.is_function() {
                sys::ast_custom_function_unregister(slot.function.get());
            } else {
                sys::ast_unregister_application(slot.name.as_ptr());
            }
        }
    }
}
