//! Rust-native Asterisk Manager Interface registration and event adapter.
//!
//! The registration payload owns named Asterisk registration strings and a
//! typed Rust handler. Incoming Asterisk headers are decoded directly into an
//! owned [`ManagerRequest`], and owned responses/events are serialized only at
//! the Asterisk edge. No project-owned C representation, userdata trampoline,
//! or response-owner callback participates in the Rust-to-Rust path.

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_int};
use std::num::NonZeroUsize;
use std::ptr;
use std::sync::{Arc, OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard};

use super::registry::{CallbackAdmissionError, CallbackRegistration, contain_callback_panic};
use crate::ami::manager::{
    ManagerActionHandler, ManagerError, ManagerEvent, ManagerField, ManagerLimits, ManagerRequest,
    ManagerRequestField, ManagerResponse, ManagerResponseKind,
};
use crate::asterisk::direct::module_info;
use crate::asterisk::sys;

const MAX_ACTIVE_CALLBACKS: usize = u32::MAX as usize;
const REDACTED_MANAGER_VALUE: &str = "<redacted>";

const ACTION_HEADER: &CStr = c"Action";
const FORMAT_STRING: &CStr = c"%s";
const ERROR_SHUTTING_DOWN: &CStr = c"Action is shutting down";
const ERROR_METADATA_LIMIT: &CStr = c"Request metadata exceeds configured limits";
const ERROR_INVALID_METADATA: &CStr = c"Invalid request metadata";
const ERROR_HANDLER_FAILED: &CStr = c"Action handler failed";
const ERROR_INVALID_RESPONSE: &CStr = c"Invalid action response";
const SOURCE_FILE: &CStr = c"asterisk/native/manager.rs";
const SOURCE_FUNCTION: &CStr = c"publish_manager_event";

struct ManagerRegistrationStrings {
    action: CString,
    synopsis: CString,
    description: CString,
}

struct ManagerPayload {
    strings: ManagerRegistrationStrings,
    limits: ManagerLimits,
    handler: Box<ManagerActionHandler>,
}

type Registration = CallbackRegistration<ManagerPayload>;
type RegistrationMap = HashMap<Vec<u8>, Arc<Registration>>;

static REGISTRATIONS: OnceLock<RwLock<RegistrationMap>> = OnceLock::new();

/// Typed ownership handle returned to the domain AMI API.
pub struct NativeManagerActionRegistration {
    registration: Arc<Registration>,
}

impl Drop for NativeManagerActionRegistration {
    fn drop(&mut self) {
        unregister_action(&self.registration);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActionFailure {
    ShuttingDown,
    MetadataLimit,
    InvalidMetadata,
    HandlerFailed,
    InvalidResponse,
}

fn registrations() -> &'static RwLock<RegistrationMap> {
    REGISTRATIONS.get_or_init(|| RwLock::new(HashMap::new()))
}

fn read_registrations() -> RwLockReadGuard<'static, RegistrationMap> {
    registrations()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn write_registrations() -> RwLockWriteGuard<'static, RegistrationMap> {
    registrations()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn action_key(action: &CStr) -> Vec<u8> {
    action
        .to_bytes()
        .iter()
        .map(u8::to_ascii_lowercase)
        .collect()
}

fn same_registration(left: &Arc<Registration>, right: &Arc<Registration>) -> bool {
    Arc::ptr_eq(left, right)
}

fn decode_header_lines<'a>(
    action: &str,
    headers: impl IntoIterator<Item = &'a str>,
    header_count: usize,
    limits: ManagerLimits,
) -> Result<ManagerRequest, ActionFailure> {
    if header_count > limits.max_fields {
        return Err(ActionFailure::MetadataLimit);
    }
    let mut fields = Vec::with_capacity(header_count);
    for header in headers {
        let Some((name, value)) = header.split_once(':') else {
            return Err(ActionFailure::InvalidMetadata);
        };
        let name = name.trim_end_matches([' ', '\t']);
        let value = value.trim_start_matches([' ', '\t']);
        if name.is_empty()
            || !name.is_ascii()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
            || name.len() > limits.max_field_name_bytes
            || value.len() > limits.max_field_value_bytes
            || value.contains(['\r', '\n'])
        {
            return Err(ActionFailure::InvalidMetadata);
        }
        fields.push(ManagerRequestField::new(name, value));
    }
    if fields.len() != header_count {
        return Err(ActionFailure::InvalidMetadata);
    }
    Ok(ManagerRequest {
        action: action.to_owned(),
        fields,
    })
}

unsafe fn decode_request(
    message: *const sys::message,
    payload: &ManagerPayload,
) -> Result<ManagerRequest, ActionFailure> {
    let header_count = unsafe { (*message).hdrcount as usize };
    if header_count > payload.limits.max_fields {
        return Err(ActionFailure::MetadataLimit);
    }
    if header_count > unsafe { (*message).headers.len() } {
        return Err(ActionFailure::InvalidMetadata);
    }
    let action = payload
        .strings
        .action
        .to_str()
        .map_err(|_| ActionFailure::InvalidMetadata)?;
    let headers = unsafe { &(&(*message).headers)[..header_count] };
    let mut decoded = Vec::with_capacity(header_count);
    for &header in headers {
        if header.is_null() {
            return Err(ActionFailure::InvalidMetadata);
        }
        decoded.push(
            unsafe { CStr::from_ptr(header) }
                .to_str()
                .map_err(|_| ActionFailure::InvalidMetadata)?,
        );
    }
    decode_header_lines(action, decoded, header_count, payload.limits)
}

fn append_field(
    serialized: &mut String,
    field: &ManagerField,
    limits: ManagerLimits,
) -> Result<(), ActionFailure> {
    if field.name().len() > limits.max_field_name_bytes {
        return Err(ActionFailure::InvalidResponse);
    }
    let value = match field.public_value() {
        Some(value) if value.len() > limits.max_field_value_bytes => {
            return Err(ActionFailure::InvalidResponse);
        }
        Some(value) => value,
        None => REDACTED_MANAGER_VALUE,
    };
    serialized.push_str(field.name());
    serialized.push_str(": ");
    serialized.push_str(value);
    serialized.push_str("\r\n");
    Ok(())
}

fn encode_response(
    response: &ManagerResponse,
    action_id: Option<&str>,
    limits: ManagerLimits,
) -> Result<CString, ActionFailure> {
    if response.fields().len() > limits.max_fields
        || response
            .message()
            .is_some_and(|message| message.len() > limits.max_field_value_bytes)
    {
        return Err(ActionFailure::InvalidResponse);
    }
    let response_name = match response.kind() {
        ManagerResponseKind::Success => "Success",
        ManagerResponseKind::Error => "Error",
    };
    let mut serialized = format!("Response: {response_name}\r\n");
    if let Some(action_id) = action_id.filter(|value| !value.is_empty()) {
        serialized.push_str("ActionID: ");
        serialized.push_str(action_id);
        serialized.push_str("\r\n");
    }
    if let Some(message) = response.message() {
        serialized.push_str("Message: ");
        serialized.push_str(message);
        serialized.push_str("\r\n");
    }
    for field in response.fields() {
        append_field(&mut serialized, field, limits)?;
    }
    serialized.push_str("\r\n");
    if serialized.len() > limits.max_response_bytes {
        return Err(ActionFailure::InvalidResponse);
    }
    CString::new(serialized).map_err(|_| ActionFailure::InvalidResponse)
}

fn encode_event(event: &ManagerEvent, limits: ManagerLimits) -> Result<CString, ManagerError> {
    if event.fields().len() > limits.max_fields {
        return Err(ManagerError::PublishFailed);
    }
    let mut serialized = String::new();
    for field in event.fields() {
        append_field(&mut serialized, field, limits).map_err(|_| ManagerError::PublishFailed)?;
    }
    if serialized.len() > limits.max_response_bytes {
        return Err(ManagerError::PublishFailed);
    }
    CString::new(serialized).map_err(|_| ManagerError::PublishFailed)
}

fn execute_action(message: *const sys::message) -> Result<CString, ActionFailure> {
    if message.is_null() {
        return Err(ActionFailure::InvalidMetadata);
    }
    let action = unsafe { sys::astman_get_header(message, ACTION_HEADER.as_ptr().cast_mut()) };
    if action.is_null() {
        return Err(ActionFailure::ShuttingDown);
    }
    let key = action_key(unsafe { CStr::from_ptr(action) });
    let registration = read_registrations()
        .get(&key)
        .cloned()
        .ok_or(ActionFailure::ShuttingDown)?;
    let lease = registration.enter().map_err(|failure| match failure {
        CallbackAdmissionError::ShuttingDown | CallbackAdmissionError::Saturated => {
            ActionFailure::ShuttingDown
        }
    })?;
    let payload = lease.payload();
    let request = unsafe { decode_request(message, payload) }?;
    let action_id = request.values("ActionID").next().map(str::to_owned);
    let response = (payload.handler)(request);
    encode_response(&response, action_id.as_deref(), payload.limits)
}

unsafe fn send_error(
    session: *mut sys::mansession,
    message: *const sys::message,
    error: ActionFailure,
) {
    let text = match error {
        ActionFailure::ShuttingDown => ERROR_SHUTTING_DOWN,
        ActionFailure::MetadataLimit => ERROR_METADATA_LIMIT,
        ActionFailure::InvalidMetadata => ERROR_INVALID_METADATA,
        ActionFailure::HandlerFailed => ERROR_HANDLER_FAILED,
        ActionFailure::InvalidResponse => ERROR_INVALID_RESPONSE,
    };
    unsafe { sys::astman_send_error(session, message, text.as_ptr().cast_mut()) };
}

unsafe extern "C" fn manager_action(
    session: *mut sys::mansession,
    message: *const sys::message,
) -> c_int {
    if session.is_null() || message.is_null() {
        return 0;
    }
    let result = contain_callback_panic(Err(ActionFailure::HandlerFailed), || {
        execute_action(message)
    });
    match result {
        Ok(serialized) => unsafe {
            sys::astman_append(session, FORMAT_STRING.as_ptr(), serialized.as_ptr());
        },
        Err(error) => unsafe { send_error(session, message, error) },
    }
    0
}

/// Register a typed Rust AMI handler with Asterisk.
pub fn register_manager_action<F>(
    action: &str,
    authority: u32,
    synopsis: &str,
    description: &str,
    limits: ManagerLimits,
    handler: F,
) -> Result<NativeManagerActionRegistration, ManagerError>
where
    F: Fn(ManagerRequest) -> ManagerResponse + Send + Sync + 'static,
{
    let strings = ManagerRegistrationStrings {
        action: CString::new(action).map_err(|_| ManagerError::RegistrationFailed)?,
        synopsis: CString::new(synopsis).map_err(|_| ManagerError::RegistrationFailed)?,
        description: CString::new(description).map_err(|_| ManagerError::RegistrationFailed)?,
    };
    let key = action_key(&strings.action);
    let maximum_active =
        NonZeroUsize::new(MAX_ACTIVE_CALLBACKS).ok_or(ManagerError::RegistrationFailed)?;
    let registration = CallbackRegistration::new(
        maximum_active,
        ManagerPayload {
            strings,
            limits,
            handler: Box::new(handler),
        },
    );
    {
        let mut registry = write_registrations();
        if registry.contains_key(&key) {
            return Err(ManagerError::RegistrationFailed);
        }
        registry.insert(key.clone(), Arc::clone(&registration));
    }

    let Some(payload) = registration.payload_for_owner() else {
        write_registrations().remove(&key);
        return Err(ManagerError::RegistrationFailed);
    };
    let result = unsafe {
        sys::ast_manager_register2(
            payload.strings.action.as_ptr(),
            authority as c_int,
            Some(manager_action),
            module_info::module_self(),
            payload.strings.synopsis.as_ptr(),
            payload.strings.description.as_ptr(),
        )
    };
    if result != 0 {
        let mut registry = write_registrations();
        if registry
            .get(&key)
            .is_some_and(|current| same_registration(current, &registration))
        {
            registry.remove(&key);
        }
        return Err(ManagerError::RegistrationFailed);
    }
    Ok(NativeManagerActionRegistration { registration })
}

fn unregister_action(registration: &Arc<Registration>) {
    let Some(payload) = registration.payload_for_owner() else {
        return;
    };
    let key = action_key(&payload.strings.action);
    {
        let registry = write_registrations();
        if !registry
            .get(&key)
            .is_some_and(|current| same_registration(current, registration))
        {
            return;
        }
        registration.close_admission();
        // Keep the closed entry visible until Asterisk has unlinked it. A
        // concurrent replacement must not register successfully and then be
        // removed by this registration's name-based unregister call.
    }
    unsafe { sys::ast_manager_unregister(payload.strings.action.as_ptr()) };
    {
        let mut registry = write_registrations();
        if registry
            .get(&key)
            .is_some_and(|current| same_registration(current, registration))
        {
            registry.remove(&key);
        }
    }
    registration.drain();
}

/// Serialize and publish one owned AMI event at the Asterisk edge.
pub fn publish_manager_event(
    event: &ManagerEvent,
    limits: ManagerLimits,
) -> Result<(), ManagerError> {
    let serialized = encode_event(event, limits)?;
    let event_name = CString::new(event.name()).map_err(|_| ManagerError::PublishFailed)?;
    let result = unsafe {
        sys::__ast_manager_event_multichan(
            event.category().bits() as c_int,
            event_name.as_ptr(),
            0,
            ptr::null_mut(),
            SOURCE_FILE.as_ptr(),
            line!() as c_int,
            SOURCE_FUNCTION.as_ptr(),
            FORMAT_STRING.as_ptr(),
            serialized.as_ptr(),
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(ManagerError::PublishFailed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ami::manager::{ManagerField, ManagerPrivilege};

    #[test]
    fn action_keys_match_ascii_case_insensitively() {
        assert_eq!(
            action_key(c"SCCPShowDevices"),
            action_key(c"sccpshowdevices")
        );
    }

    #[test]
    fn owned_header_decode_preserves_order_repeats_and_sensitivity() {
        let request = decode_header_lines(
            "SccpInspect",
            [
                "Line: 1001",
                "Line:\t1002",
                "Secret: do-not-log",
                "ActionID: request-7",
            ],
            4,
            ManagerLimits::default(),
        )
        .unwrap();
        assert_eq!(request.values("line").collect::<Vec<_>>(), ["1001", "1002"]);
        assert!(request.fields[2].sensitive);
        assert_eq!(request.values("ActionID").next(), Some("request-7"));
    }

    #[test]
    fn header_decode_rejects_count_mismatch_and_metadata_overflow() {
        let limits = ManagerLimits {
            max_fields: 2,
            max_field_name_bytes: 4,
            max_field_value_bytes: 4,
            max_response_bytes: 64,
        };
        assert_eq!(
            decode_header_lines("Inspect", ["Line: 1001"], 2, limits),
            Err(ActionFailure::InvalidMetadata)
        );
        assert_eq!(
            decode_header_lines(
                "Inspect",
                ["Line: 1001", "Peer: 1002", "More: 3"],
                3,
                limits
            ),
            Err(ActionFailure::MetadataLimit)
        );
        assert_eq!(
            decode_header_lines("Inspect", ["LongName: ok"], 1, limits),
            Err(ActionFailure::InvalidMetadata)
        );
        assert_eq!(
            decode_header_lines("Inspect", ["Line: 12345"], 1, limits),
            Err(ActionFailure::InvalidMetadata)
        );
    }

    #[test]
    fn response_encoding_preserves_action_id_bounds_and_redaction() {
        let response = ManagerResponse::success("inspected")
            .unwrap()
            .with_fields(vec![
                ManagerField::public("Count", "2").unwrap(),
                ManagerField::redacted("Credential").unwrap(),
            ]);
        let encoded =
            encode_response(&response, Some("request-7"), ManagerLimits::default()).unwrap();
        assert_eq!(
            encoded.to_str().unwrap(),
            concat!(
                "Response: Success\r\n",
                "ActionID: request-7\r\n",
                "Message: inspected\r\n",
                "Count: 2\r\n",
                "Credential: <redacted>\r\n",
                "\r\n"
            )
        );
        assert_eq!(
            encode_response(
                &response,
                None,
                ManagerLimits {
                    max_response_bytes: 4,
                    ..ManagerLimits::default()
                }
            ),
            Err(ActionFailure::InvalidResponse)
        );
    }

    #[test]
    fn event_encoding_preserves_fields_and_redaction() {
        let event = ManagerEvent::new(
            ManagerPrivilege::CALL,
            "SccpState",
            vec![
                ManagerField::public("Device", "SEP001").unwrap(),
                ManagerField::redacted("TokenDigest").unwrap(),
            ],
        )
        .unwrap();
        assert_eq!(
            encode_event(&event, ManagerLimits::default())
                .unwrap()
                .to_str()
                .unwrap(),
            "Device: SEP001\r\nTokenDigest: <redacted>\r\n"
        );
    }
}
