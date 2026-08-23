//! Parking commands, detached retrieval, and Stasis subscription ownership.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::mem;
use std::num::NonZeroUsize;
use std::ptr;
use std::sync::Arc;

use super::{SOURCE_FILE, SOURCE_FUNCTION, ao2_ref, lock_channel};
use crate::asterisk::direct::module_info;
use crate::asterisk::raw::handles::{ChannelRef, ModuleReference};
use crate::asterisk::raw::registry::{
    CallbackAdmissionError, CallbackRegistration, ShutdownDisposition, acquire_from_native,
    contain_callback_panic, release_from_native, retain_for_native,
};
use crate::asterisk::sys;
use crate::call::parking::{
    ParkingError, ParkingEvent, ParkingEventKind, ParkingSubscriptionControl,
};
use crate::pbx::operations::CallFeatureError;
use crate::pbx::party::AsteriskChannel;

unsafe fn park_channel_native(
    channel: *mut sys::ast_channel,
    lot: Option<&CStr>,
) -> Result<(), CallFeatureError> {
    const OPERATION: &str = "park";
    if channel.is_null() {
        return Err(CallFeatureError::InvalidInput {
            operation: OPERATION,
        });
    }
    if unsafe { sys::ast_parking_provider_registered() } == 0 {
        return Err(CallFeatureError::Unavailable {
            operation: OPERATION,
        });
    }
    let Some(channel) = (unsafe { lock_channel(channel) }) else {
        return Err(CallFeatureError::NativeFailure {
            operation: OPERATION,
        });
    };
    if let Some(lot) = lot {
        unsafe {
            sys::pbx_builtin_setvar_helper(
                channel.as_ptr().cast(),
                c"PARKINGLOT".as_ptr(),
                lot.as_ptr(),
            )
        };
    }
    let bridge_channel = unsafe { sys::ast_channel_get_bridge_channel(channel.as_ptr().cast()) };
    drop(channel);
    if bridge_channel.is_null() {
        return Err(CallFeatureError::NotFound {
            operation: OPERATION,
        });
    }
    let result = unsafe { sys::ast_parking_park_call(bridge_channel, ptr::null_mut(), 0) };
    unsafe { ao2_ref(bridge_channel.cast(), -1) };
    if result == 0 {
        Ok(())
    } else {
        Err(CallFeatureError::NativeFailure {
            operation: OPERATION,
        })
    }
}

pub fn parking_peer_uniqueid(
    channel: &AsteriskChannel<'_>,
) -> Result<Option<String>, CallFeatureError> {
    let peer: *mut sys::ast_channel =
        unsafe { sys::ast_channel_bridge_peer(channel.as_raw().cast()).cast() };
    let Some(peer) = (unsafe { ChannelRef::from_owned(peer) }) else {
        return Ok(None);
    };
    let uniqueid = unsafe { sys::ast_channel_uniqueid(peer.as_ptr().cast()) };
    if uniqueid.is_null() {
        return Err(CallFeatureError::NativeFailure {
            operation: "read parking peer unique ID",
        });
    }
    let uniqueid = unsafe { CStr::from_ptr(uniqueid) }
        .to_string_lossy()
        .into_owned();
    Ok((!uniqueid.is_empty()).then_some(uniqueid))
}

struct AsyncApplication {
    channel: ChannelRef,
    _module: ModuleReference,
    arguments: CString,
}

unsafe extern "C" fn async_application_thread(data: *mut c_void) -> *mut c_void {
    contain_callback_panic(ptr::null_mut(), || unsafe {
        if data.is_null() {
            return ptr::null_mut();
        }
        let request = Box::from_raw(data.cast::<AsyncApplication>());
        sys::ast_pbx_exec_application(
            request.channel.as_ptr().cast(),
            c"ParkedCall".as_ptr(),
            request.arguments.as_ptr(),
        );
        drop(request);
        ptr::null_mut()
    })
}

unsafe fn start_async_application(
    channel: *mut sys::ast_channel,
    arguments: &CStr,
    operation: &'static str,
) -> Result<(), CallFeatureError> {
    if channel.is_null() {
        return Err(CallFeatureError::InvalidInput { operation });
    }
    if unsafe { sys::pbx_findapp(c"ParkedCall".as_ptr()) }.is_null() {
        return Err(CallFeatureError::Unavailable { operation });
    }
    let arguments = arguments.to_owned();
    let module = unsafe { ModuleReference::acquire(module_info::module_self()) }
        .ok_or(CallFeatureError::NativeFailure { operation })?;
    let Some(channel) = (unsafe { ChannelRef::acquire(channel) }) else {
        return Err(CallFeatureError::NativeFailure { operation });
    };
    let request = Box::new(AsyncApplication {
        channel,
        _module: module,
        arguments,
    });
    let request = Box::into_raw(request);
    let mut thread = unsafe { mem::zeroed() };
    let result = unsafe {
        sys::ast_pthread_create_detached_stack(
            &mut thread,
            ptr::null_mut(),
            Some(async_application_thread),
            request.cast(),
            sys::ast_background_stacksize() as usize,
            SOURCE_FILE.as_ptr(),
            SOURCE_FUNCTION.as_ptr(),
            line!() as c_int,
            c"async_application_thread".as_ptr(),
        )
    };
    if result != 0 {
        unsafe { drop(Box::from_raw(request)) };
        Err(CallFeatureError::NativeFailure { operation })
    } else {
        Ok(())
    }
}

// === Typed parking commands =================================================

pub fn park_channel(
    channel: &AsteriskChannel<'_>,
    lot: Option<&str>,
) -> Result<(), CallFeatureError> {
    let lot = lot
        .map(CString::new)
        .transpose()
        .map_err(|_| CallFeatureError::InvalidText {
            field: "parking lot",
        })?;
    unsafe { park_channel_native(channel.as_raw().cast(), lot.as_deref()) }
}

pub fn retrieve_parked_channel(
    channel: &AsteriskChannel<'_>,
    lot: Option<&str>,
    slot: &str,
) -> Result<(), CallFeatureError> {
    let arguments =
        CString::new(format!("{},{}", lot.unwrap_or_default(), slot)).map_err(|_| {
            CallFeatureError::InvalidText {
                field: "parking retrieval arguments",
            }
        })?;
    unsafe { start_async_application(channel.as_raw().cast(), &arguments, "retrieve parked call") }
}

// === Typed parking subscription ============================================

type ParkingCallback = dyn Fn(ParkingEvent) + Send + Sync + 'static;
type ParkingRegistration = CallbackRegistration<Arc<ParkingCallback>>;

/// An owning Stasis parking subscription. Drop prevents new callback
/// admission and drains user callbacks before cancelling Stasis. External
/// teardown joins synchronously; self-unsubscribe defers completion to the
/// serialized final Stasis callback, which releases native userdata.
struct NativeParkingSubscription {
    subscription: *mut sys::stasis_subscription,
    registration: Arc<ParkingRegistration>,
}

unsafe impl Send for NativeParkingSubscription {}

impl NativeParkingSubscription {
    fn unsubscribe(&mut self) {
        self.registration.close_admission();
        let disposition = self.registration.drain();
        let subscription = mem::replace(&mut self.subscription, ptr::null_mut());
        if subscription.is_null() {
            return;
        }
        match disposition {
            ShutdownDisposition::Drained => {
                unsafe { sys::stasis_unsubscribe_and_join(subscription) };
            }
            ShutdownDisposition::DeferredToCallback => {
                // Joining the serialized Stasis consumer from its own callback
                // would deadlock. The final message releases native userdata.
                unsafe { sys::stasis_unsubscribe(subscription) };
            }
        }
    }
}

impl Drop for NativeParkingSubscription {
    fn drop(&mut self) {
        contain_callback_panic((), || self.unsubscribe());
    }
}

impl ParkingSubscriptionControl for NativeParkingSubscription {
    fn unsubscribe(mut self: Box<Self>) {
        NativeParkingSubscription::unsubscribe(&mut self);
    }
}

fn owned_text(value: *const c_char) -> String {
    if value.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(value) }
            .to_string_lossy()
            .into_owned()
    }
}

unsafe fn decode_parking_event(message: *mut sys::stasis_message) -> Option<ParkingEvent> {
    if message.is_null()
        || unsafe { sys::stasis_message_type(message) } != unsafe { sys::ast_parked_call_type() }
    {
        return None;
    }
    let payload =
        unsafe { sys::stasis_message_data(message) }.cast::<sys::ast_parked_call_payload>();
    let payload = unsafe { payload.as_ref() }?;
    let parkee = unsafe { payload.parkee.as_ref() }?;
    let base = unsafe { parkee.base.as_ref() }?;
    let caller = unsafe { parkee.caller.as_ref() }?;
    let connected = unsafe { parkee.connected.as_ref() }?;
    let caller_allowed =
        (caller.pres & sys::AST_PRES_RESTRICTION as c_int) == sys::AST_PRES_ALLOWED as c_int;
    let kind = match payload.event_type as c_int {
        0 => ParkingEventKind::Parked,
        1 => ParkingEventKind::Timeout,
        2 => ParkingEventKind::GiveUp,
        3 => ParkingEventKind::Retrieved,
        4 => ParkingEventKind::Failed,
        5 => ParkingEventKind::Swap,
        _ => return None,
    };
    let retriever_channel = (unsafe { payload.retriever.as_ref() })
        .and_then(|snapshot| unsafe { snapshot.base.as_ref() })
        .map_or_else(String::new, |snapshot| owned_text(snapshot.name));
    Some(ParkingEvent {
        kind,
        lot: owned_text(payload.parkinglot),
        slot: payload.parkingspace,
        timeout_seconds: u64::try_from(payload.timeout).ok()?,
        duration_seconds: u64::try_from(payload.duration).ok()?,
        parker_dial_string: owned_text(payload.parker_dial_string),
        parkee_channel: owned_text(base.name),
        parkee_unique_id: owned_text(base.uniqueid),
        caller_name: if caller_allowed {
            owned_text(caller.name)
        } else {
            String::new()
        },
        caller_number: if caller_allowed {
            owned_text(caller.number)
        } else {
            String::new()
        },
        connected_name: owned_text(connected.name.as_ptr()),
        connected_number: owned_text(connected.number),
        retriever_channel,
    })
}

unsafe extern "C" fn parking_event(
    data: *mut c_void,
    subscription: *mut sys::stasis_subscription,
    message: *mut sys::stasis_message,
) {
    contain_callback_panic((), || {
        if unsafe { sys::stasis_subscription_final_message(subscription, message) } != 0 {
            // The subscription owns exactly one native registration reference
            // until its final serialized callback, including self-unsubscribe.
            unsafe { release_from_native::<Arc<ParkingCallback>>(data) };
            return;
        }
        let Some(registration) = (unsafe { acquire_from_native::<Arc<ParkingCallback>>(data) })
        else {
            return;
        };
        let lease = match registration.enter() {
            Ok(lease) => lease,
            Err(CallbackAdmissionError::ShuttingDown | CallbackAdmissionError::Saturated) => {
                return;
            }
        };
        let Some(event) = (unsafe { decode_parking_event(message) }) else {
            return;
        };
        (lease.payload())(event);
    });
}

pub fn subscribe_parking(
    callback: Arc<ParkingCallback>,
) -> Result<Box<dyn ParkingSubscriptionControl>, ParkingError> {
    let topic = unsafe { sys::ast_parking_topic() };
    let message_type = unsafe { sys::ast_parked_call_type() };
    if topic.is_null() || message_type.is_null() {
        return Err(ParkingError::SubscribeFailed);
    }
    let registration = CallbackRegistration::new(NonZeroUsize::MAX, callback);
    let userdata = retain_for_native(&registration);
    let subscription = unsafe {
        sys::__stasis_subscribe(
            topic,
            Some(parking_event),
            userdata.as_ptr(),
            SOURCE_FILE.as_ptr(),
            line!() as c_int,
            SOURCE_FUNCTION.as_ptr(),
        )
    };
    if subscription.is_null() {
        unsafe { release_from_native::<Arc<ParkingCallback>>(userdata.as_ptr()) };
        return Err(ParkingError::SubscribeFailed);
    }
    unsafe {
        sys::stasis_subscription_accept_message_type(subscription, message_type);
        sys::stasis_subscription_set_filter(
            subscription,
            sys::STASIS_SUBSCRIPTION_FILTER_SELECTIVE,
        );
    }
    Ok(Box::new(NativeParkingSubscription {
        subscription,
        registration,
    }))
}
