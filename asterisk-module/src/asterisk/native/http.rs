//! Rust-native Asterisk HTTP registration and request adapter.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::mem;
use std::num::NonZeroUsize;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};

use crate::asterisk::boundary::{CondvarExt as _, MutexExt as _};
use crate::asterisk::sys;
use crate::http::{
    HttpField, HttpFramingError, HttpLimits, HttpMethod, HttpMethodSet, HttpRequest, HttpResponse,
    HttpResponseError, SharedHttpHandler, http_status_title, request_body_length,
};

use super::handles::AsteriskAllocation;
use super::registry::{
    CallbackAdmissionError, CallbackRegistration, acquire_from_native, contain_callback_panic,
    release_from_native, retain_for_native,
};

const SOURCE_FILE: &CStr = c"asterisk/native/http.rs";
const SOURCE_FUNCTION: &CStr = c"sccp_http";
const MAX_ACTIVE_CALLBACKS: usize = 1024;

#[repr(C)]
struct File {
    _private: [u8; 0],
}

unsafe extern "C" {
    fn tmpfile() -> *mut File;
    fn fwrite(data: *const c_void, size: usize, count: usize, file: *mut File) -> usize;
    fn fflush(file: *mut File) -> c_int;
    fn fileno(file: *mut File) -> c_int;
    fn fclose(file: *mut File) -> c_int;
}

struct HttpPayload {
    strings: HttpRegistrationStrings,
    allowed_methods: HttpMethodSet,
    limits: HttpLimits,
    route: String,
    handler: SharedHttpHandler,
}

struct HttpRegistrationStrings {
    path: CString,
    description: CString,
}

impl HttpRegistrationStrings {
    fn new(
        path: &str,
        description: &str,
        maximum_string: usize,
        maximum_total: usize,
    ) -> Result<Self, NativeHttpRegistrationError> {
        let values = [path, description];
        let mut total = 0usize;
        for value in values {
            let bytes = value
                .len()
                .checked_add(1)
                .ok_or(NativeHttpRegistrationError)?;
            if bytes > maximum_string {
                return Err(NativeHttpRegistrationError);
            }
            total = total
                .checked_add(bytes)
                .ok_or(NativeHttpRegistrationError)?;
        }
        if total > maximum_total {
            return Err(NativeHttpRegistrationError);
        }
        Ok(Self {
            path: CString::new(path).map_err(|_| NativeHttpRegistrationError)?,
            description: CString::new(description).map_err(|_| NativeHttpRegistrationError)?,
        })
    }
}

/// Stable Asterisk-visible descriptor and pre-callback admission gate.
///
/// Asterisk drops its URI-list read lock before invoking the selected
/// callback. Consequently unlink can race a worker which owns only the raw
/// descriptor pointer. The descriptor is deliberately retained as a closed
/// tombstone after unregister; this tiny allocation contains no handler state
/// and prevents that unavoidable upstream lookup window from becoming a UAF.
struct HttpRouteGate {
    uri: sys::ast_http_uri,
    registration: *mut c_void,
    closing: AtomicBool,
    readers: AtomicUsize,
    wait_lock: Mutex<()>,
    wait: Condvar,
}

unsafe impl Send for HttpRouteGate {}
unsafe impl Sync for HttpRouteGate {}

impl HttpRouteGate {
    fn enter(&self) -> bool {
        self.readers.fetch_add(1, Ordering::SeqCst);
        if self.closing.load(Ordering::SeqCst) {
            self.leave();
            false
        } else {
            true
        }
    }

    fn leave(&self) {
        if self.readers.fetch_sub(1, Ordering::SeqCst) == 1 && self.closing.load(Ordering::SeqCst) {
            let _guard = self.wait_lock.lock_unpoisoned();
            self.wait.notify_all();
        }
    }

    fn close_and_drain_readers(&self) -> bool {
        if self.closing.swap(true, Ordering::SeqCst) {
            return false;
        }
        let mut guard = self.wait_lock.lock_unpoisoned();
        while self.readers.load(Ordering::SeqCst) != 0 {
            guard = self.wait.wait_unpoisoned(guard);
        }
        true
    }
}

/// Owned registration handle. Dropping it closes callback admission, unlinks
/// the URI and drains admitted callbacks before releasing the handler.
pub struct NativeHttpRegistration {
    gate: NonNull<HttpRouteGate>,
}

// The pointed-to gate and callback registration use atomics and locks for all
// cross-thread state. Ownership of this handle is unique.
unsafe impl Send for NativeHttpRegistration {}

impl Drop for NativeHttpRegistration {
    fn drop(&mut self) {
        unregister_http(self.gate);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeHttpRegistrationError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RequestMetadataError {
    Missing,
    InvalidUtf8,
    TooLarge,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BodyReadError {
    UnexpectedEnd,
}

#[derive(Debug, Eq, PartialEq)]
enum HttpCallbackError {
    ShuttingDown,
    Saturated,
    UnsupportedMethod,
    MethodNotAllowed { allow_header: String },
    Metadata(RequestMetadataError),
    PathTooLarge,
    Framing(HttpFramingError),
    BodyRead(BodyReadError),
    HandlerPanicked,
    InvalidResponse(HttpResponseError),
    ResponseSendFailed,
}

enum ErrorDisposition {
    Standard {
        status: c_int,
        title: &'static CStr,
        message: &'static CStr,
    },
    MethodNotAllowed {
        allow_header: String,
    },
}

impl HttpCallbackError {
    fn disposition(self) -> ErrorDisposition {
        match self {
            Self::ShuttingDown | Self::Saturated => ErrorDisposition::Standard {
                status: 503,
                title: c"Service Unavailable",
                message: c"HTTP handler is shutting down",
            },
            Self::UnsupportedMethod => ErrorDisposition::Standard {
                status: 501,
                title: c"Not Implemented",
                message: c"Unsupported HTTP method",
            },
            Self::MethodNotAllowed { allow_header } => {
                ErrorDisposition::MethodNotAllowed { allow_header }
            }
            Self::Metadata(RequestMetadataError::TooLarge) | Self::PathTooLarge => {
                ErrorDisposition::Standard {
                    status: 431,
                    title: c"Request Header Fields Too Large",
                    message: c"Request metadata exceeds configured limits",
                }
            }
            Self::Metadata(RequestMetadataError::Missing | RequestMetadataError::InvalidUtf8) => {
                ErrorDisposition::Standard {
                    status: 400,
                    title: c"Bad Request",
                    message: c"Invalid request metadata",
                }
            }
            Self::Framing(HttpFramingError::UnsupportedTransferEncoding) => {
                ErrorDisposition::Standard {
                    status: 501,
                    title: c"Not Implemented",
                    message: c"Transfer encoding is unsupported",
                }
            }
            Self::Framing(HttpFramingError::BodyTooLarge) => ErrorDisposition::Standard {
                status: 413,
                title: c"Content Too Large",
                message: c"Request body exceeds configured limit",
            },
            Self::Framing(HttpFramingError::InvalidContentLength) => ErrorDisposition::Standard {
                status: 400,
                title: c"Bad Request",
                message: c"Invalid Content-Length",
            },
            Self::BodyRead(BodyReadError::UnexpectedEnd) => ErrorDisposition::Standard {
                status: 400,
                title: c"Bad Request",
                message: c"Unable to read request body",
            },
            Self::HandlerPanicked => ErrorDisposition::Standard {
                status: 500,
                title: c"Internal Server Error",
                message: c"HTTP handler panicked",
            },
            Self::InvalidResponse(_) => ErrorDisposition::Standard {
                status: 500,
                title: c"Internal Server Error",
                message: c"Invalid HTTP handler response",
            },
            Self::ResponseSendFailed => ErrorDisposition::Standard {
                status: 500,
                title: c"Internal Server Error",
                message: c"Unable to send HTTP handler response",
            },
        }
    }
}

fn method(method: sys::ast_http_method) -> HttpMethod {
    match method {
        sys::AST_HTTP_GET => HttpMethod::Get,
        sys::AST_HTTP_POST => HttpMethod::Post,
        sys::AST_HTTP_HEAD => HttpMethod::Head,
        sys::AST_HTTP_PUT => HttpMethod::Put,
        sys::AST_HTTP_DELETE => HttpMethod::Delete,
        sys::AST_HTTP_OPTIONS => HttpMethod::Options,
        _ => HttpMethod::Unknown,
    }
}

unsafe fn native_text(
    value: *const c_char,
    maximum: usize,
) -> Result<String, RequestMetadataError> {
    if value.is_null() {
        return Err(RequestMetadataError::Missing);
    }
    let value = unsafe { CStr::from_ptr(value) };
    if value.to_bytes().len() > maximum {
        return Err(RequestMetadataError::TooLarge);
    }
    value
        .to_str()
        .map(str::to_owned)
        .map_err(|_| RequestMetadataError::InvalidUtf8)
}

unsafe fn variables(
    mut variable: *const sys::ast_variable,
    maximum_count: usize,
    limits: HttpLimits,
) -> Result<Vec<HttpField>, RequestMetadataError> {
    let mut fields = Vec::new();
    while !variable.is_null() {
        if fields.len() == maximum_count {
            return Err(RequestMetadataError::TooLarge);
        }
        fields.push(HttpField {
            name: unsafe { native_text((*variable).name, limits.max_field_name_bytes) }?,
            value: unsafe { native_text((*variable).value, limits.max_field_value_bytes) }?,
        });
        variable = unsafe { (*variable).next };
    }
    Ok(fields)
}

unsafe fn read_body(
    session: *mut sys::ast_tcptls_session_instance,
    length: usize,
) -> Result<Vec<u8>, BodyReadError> {
    let mut body = vec![0; length];
    let mut total = 0;
    while total < length {
        let read = unsafe {
            sys::ast_iostream_read(
                (*session).stream,
                body.as_mut_ptr().add(total).cast(),
                length - total,
            )
        };
        if read <= 0 {
            unsafe { sys::ast_http_body_read_status(session, 0) };
            return Err(BodyReadError::UnexpectedEnd);
        }
        total += read as usize;
    }
    if length != 0 {
        unsafe { sys::ast_http_body_read_status(session, 1) };
    }
    Ok(body)
}

unsafe fn ast_string(value: &CStr) -> *mut sys::ast_str {
    let bytes = value.to_bytes();
    let allocation_size = mem::size_of::<sys::ast_str>() + bytes.len() + 1;
    let output = unsafe {
        sys::__ast_calloc(
            1,
            allocation_size,
            SOURCE_FILE.as_ptr(),
            line!() as c_int,
            SOURCE_FUNCTION.as_ptr(),
        )
    }
    .cast::<sys::ast_str>();
    if output.is_null() {
        return output;
    }
    unsafe {
        (*output).len = bytes.len() + 1;
        (*output).used = bytes.len();
        (*output).ts = ptr::dangling_mut::<sys::ast_threadstorage>();
        ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            (*output).str_.as_mut_ptr().cast(),
            bytes.len(),
        );
        *(*output).str_.as_mut_ptr().add(bytes.len()) = 0;
    }
    output
}

unsafe fn send_response(
    session: *mut sys::ast_tcptls_session_instance,
    method: sys::ast_http_method,
    limits: HttpLimits,
    response: &HttpResponse,
) -> Result<(), HttpCallbackError> {
    response
        .validate_for(limits)
        .map_err(HttpCallbackError::InvalidResponse)?;

    let mut header_block = format!("Content-Type: {}\r\n", response.content_type());
    for field in response.headers() {
        header_block.push_str(&field.name);
        header_block.push_str(": ");
        header_block.push_str(&field.value);
        header_block.push_str("\r\n");
    }
    let header_block =
        CString::new(header_block).map_err(|_| HttpCallbackError::ResponseSendFailed)?;
    let status_title = CString::new(http_status_title(response.status()))
        .map_err(|_| HttpCallbackError::ResponseSendFailed)?;
    let header = unsafe { AsteriskAllocation::from_owned(ast_string(&header_block)) }
        .ok_or(HttpCallbackError::ResponseSendFailed)?;

    let mut body_file = ptr::null_mut();
    let mut body_fd = 0;
    if !response.body().is_empty() {
        body_file = unsafe { tmpfile() };
        if body_file.is_null()
            || unsafe {
                fwrite(
                    response.body().as_ptr().cast(),
                    1,
                    response.body().len(),
                    body_file,
                ) != response.body().len()
                    || fflush(body_file) != 0
            }
        {
            if !body_file.is_null() {
                unsafe { fclose(body_file) };
            }
            return Err(HttpCallbackError::ResponseSendFailed);
        }
        body_fd = unsafe { fileno(body_file) };
    }

    unsafe {
        sys::ast_http_send(
            session,
            method,
            c_int::from(response.status()),
            status_title.as_ptr(),
            header.into_raw(),
            ptr::null_mut(),
            body_fd,
            0,
        )
    };
    if !body_file.is_null() {
        unsafe { fclose(body_file) };
    }
    Ok(())
}

unsafe fn http_error(
    session: *mut sys::ast_tcptls_session_instance,
    status: c_int,
    title: &'static CStr,
    message: &'static CStr,
) {
    unsafe { sys::ast_http_error(session, status, title.as_ptr(), message.as_ptr()) };
}

unsafe fn send_error(
    session: *mut sys::ast_tcptls_session_instance,
    method: sys::ast_http_method,
    error: HttpCallbackError,
) {
    match error.disposition() {
        ErrorDisposition::Standard {
            status,
            title,
            message,
        } => unsafe { http_error(session, status, title, message) },
        ErrorDisposition::MethodNotAllowed { allow_header } => {
            let Ok(allow_header) = CString::new(allow_header) else {
                unsafe {
                    http_error(
                        session,
                        500,
                        c"Internal Server Error",
                        c"Unable to allocate response",
                    )
                };
                return;
            };
            let allow = unsafe { AsteriskAllocation::from_owned(ast_string(&allow_header)) };
            if let Some(allow) = allow {
                unsafe {
                    sys::ast_http_send(
                        session,
                        method,
                        405,
                        c"Method Not Allowed".as_ptr(),
                        allow.into_raw(),
                        ptr::null_mut(),
                        0,
                        0,
                    )
                };
            } else {
                unsafe {
                    http_error(
                        session,
                        500,
                        c"Internal Server Error",
                        c"Unable to allocate response",
                    )
                };
            }
        }
    }
}

unsafe fn build_request(
    session: *mut sys::ast_tcptls_session_instance,
    path: *const c_char,
    request_method: HttpMethod,
    query: *mut sys::ast_variable,
    headers: *mut sys::ast_variable,
    payload: &HttpPayload,
) -> Result<HttpRequest, HttpCallbackError> {
    let query = unsafe { variables(query, payload.limits.max_fields, payload.limits) }
        .map_err(HttpCallbackError::Metadata)?;
    let remaining = payload.limits.max_fields - query.len();
    let headers = unsafe { variables(headers, remaining, payload.limits) }
        .map_err(HttpCallbackError::Metadata)?;

    let tail = unsafe { native_text(path, payload.limits.max_path_bytes) }
        .map_err(HttpCallbackError::Metadata)?;
    let path_length = payload
        .route
        .len()
        .checked_add(usize::from(!tail.is_empty()))
        .and_then(|length| length.checked_add(tail.len()))
        .ok_or(HttpCallbackError::PathTooLarge)?;
    if path_length > payload.limits.max_path_bytes {
        return Err(HttpCallbackError::PathTooLarge);
    }
    let full_path = if tail.is_empty() {
        payload.route.clone()
    } else {
        format!("{}/{}", payload.route, tail)
    };

    let body_length = request_body_length(&headers, payload.limits.max_body_bytes)
        .map_err(HttpCallbackError::Framing)?;
    let body = unsafe { read_body(session, body_length) }.map_err(HttpCallbackError::BodyRead)?;
    let remote_address = unsafe {
        native_text(
            sys::ast_sockaddr_stringify_fmt(
                &(*session).remote_address,
                sys::AST_SOCKADDR_STR_ADDR as c_int,
            ),
            payload.limits.max_field_value_bytes,
        )
    }
    .map_err(HttpCallbackError::Metadata)?;

    Ok(HttpRequest {
        method: request_method,
        route: payload.route.clone(),
        path: full_path,
        remote_address,
        query,
        headers,
        body,
    })
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn dispatch(
    session: *mut sys::ast_tcptls_session_instance,
    uri: *const sys::ast_http_uri,
    path: *const c_char,
    native_method: sys::ast_http_method,
    query: *mut sys::ast_variable,
    headers: *mut sys::ast_variable,
) -> Result<(), HttpCallbackError> {
    if uri.is_null() {
        return Err(HttpCallbackError::ShuttingDown);
    }
    let gate = unsafe { (*uri).data.cast::<HttpRouteGate>() };
    if gate.is_null() || !(*gate).enter() {
        return Err(HttpCallbackError::ShuttingDown);
    }
    let registration = unsafe { acquire_from_native::<HttpPayload>((*gate).registration) };
    unsafe { (*gate).leave() };
    let registration = registration.ok_or(HttpCallbackError::ShuttingDown)?;
    let lease = registration.enter().map_err(|error| match error {
        CallbackAdmissionError::ShuttingDown => HttpCallbackError::ShuttingDown,
        CallbackAdmissionError::Saturated => HttpCallbackError::Saturated,
    })?;
    let payload = lease.payload();

    let request_method = method(native_method);
    if request_method == HttpMethod::Unknown {
        return Err(HttpCallbackError::UnsupportedMethod);
    }
    if !payload.allowed_methods.contains(request_method) {
        return Err(HttpCallbackError::MethodNotAllowed {
            allow_header: payload.allowed_methods.allow_header(),
        });
    }

    let request = unsafe { build_request(session, path, request_method, query, headers, payload) }?;
    let response = catch_unwind(AssertUnwindSafe(|| payload.handler.handle(request)))
        .map_err(|_| HttpCallbackError::HandlerPanicked)?;
    unsafe { send_response(session, native_method, payload.limits, &response) }
}

unsafe extern "C" fn callback(
    session: *mut sys::ast_tcptls_session_instance,
    uri: *const sys::ast_http_uri,
    path: *const c_char,
    native_method: sys::ast_http_method,
    query: *mut sys::ast_variable,
    headers: *mut sys::ast_variable,
) -> c_int {
    contain_callback_panic(0, || unsafe {
        if let Err(error) = dispatch(session, uri, path, native_method, query, headers) {
            send_error(session, native_method, error);
        }
        0
    })
}

#[allow(clippy::too_many_arguments)]
pub fn register_http(
    path: String,
    description: String,
    route: String,
    has_subtree: bool,
    allowed_methods: HttpMethodSet,
    limits: HttpLimits,
    handler: SharedHttpHandler,
) -> Result<NativeHttpRegistration, NativeHttpRegistrationError> {
    unsafe {
        let maximum_string = limits.max_path_bytes.max(4096);
        let maximum_total = maximum_string
            .checked_mul(2)
            .ok_or(NativeHttpRegistrationError)?;
        let strings =
            HttpRegistrationStrings::new(&path, &description, maximum_string, maximum_total)?;

        let payload = HttpPayload {
            strings,
            allowed_methods,
            limits,
            route,
            handler,
        };
        let maximum_callbacks =
            NonZeroUsize::new(MAX_ACTIVE_CALLBACKS).ok_or(NativeHttpRegistrationError)?;
        let registration = CallbackRegistration::new(maximum_callbacks, payload);
        let payload = registration
            .payload_for_owner()
            .ok_or(NativeHttpRegistrationError)?;
        let native = retain_for_native(&registration);
        let mut gate = Box::new(HttpRouteGate {
            uri: mem::zeroed(),
            registration: native.as_ptr(),
            closing: AtomicBool::new(false),
            readers: AtomicUsize::new(0),
            wait_lock: Mutex::new(()),
            wait: Condvar::new(),
        });
        gate.uri.description = payload.strings.description.as_ptr();
        gate.uri.uri = payload.strings.path.as_ptr();
        gate.uri.callback = Some(callback);
        gate.uri.set_has_subtree(c_int::from(has_subtree) as u32);
        gate.uri.data = (&raw mut *gate).cast();
        gate.uri.key = payload.strings.path.as_ptr();
        if sys::ast_http_uri_link(&raw mut gate.uri) != 0 {
            registration.shutdown();
            release_from_native::<HttpPayload>(native.as_ptr());
            return Err(NativeHttpRegistrationError);
        }
        Ok(NativeHttpRegistration {
            gate: NonNull::new_unchecked(Box::into_raw(gate)),
        })
    }
}

fn unregister_http(gate: NonNull<HttpRouteGate>) {
    unsafe {
        let gate = gate.as_ref();
        if !gate.close_and_drain_readers() {
            return;
        }
        sys::ast_http_uri_unlink((&raw const gate.uri).cast_mut());
        let Some(registration) = acquire_from_native::<HttpPayload>(gate.registration) else {
            return;
        };
        registration.close_admission();
        registration.drain();
        release_from_native::<HttpPayload>(gate.registration);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_errors_have_explicit_http_dispositions() {
        let cases = [
            (
                HttpCallbackError::Framing(HttpFramingError::InvalidContentLength),
                400,
            ),
            (
                HttpCallbackError::Framing(HttpFramingError::UnsupportedTransferEncoding),
                501,
            ),
            (
                HttpCallbackError::Framing(HttpFramingError::BodyTooLarge),
                413,
            ),
            (
                HttpCallbackError::Metadata(RequestMetadataError::TooLarge),
                431,
            ),
            (HttpCallbackError::HandlerPanicked, 500),
        ];
        for (error, expected_status) in cases {
            let ErrorDisposition::Standard { status, .. } = error.disposition() else {
                panic!("expected standard disposition");
            };
            assert_eq!(status, expected_status);
        }
    }
}
