//! Asterisk-backed HTTP route registration.

use std::sync::Arc;

use crate::asterisk::raw::http::{NativeHttpRegistration, register_http};
use crate::http::{
    HttpBackend, HttpError, HttpLimits, HttpMethodSet, HttpRequest, HttpResponse,
    SharedHttpHandler, nonempty_text, registered_path,
};

#[derive(Clone, Copy, Debug, Default)]
pub struct AsteriskHttp;

impl AsteriskHttp {
    pub const fn new() -> Self {
        Self
    }
}

pub struct HttpRegistration {
    _inner: NativeHttpRegistration,
}

impl HttpBackend for AsteriskHttp {
    type Registration = HttpRegistration;

    fn register<F>(
        &self,
        path: &str,
        description: &str,
        has_subtree: bool,
        methods: HttpMethodSet,
        limits: HttpLimits,
        handler: F,
    ) -> Result<Self::Registration, HttpError>
    where
        F: Fn(HttpRequest) -> HttpResponse + Send + Sync + 'static,
    {
        let (native_path, route) = registered_path(path)?;
        let description = nonempty_text("description", description)?;
        if !methods.is_valid() {
            return Err(HttpError::EmptyMethods);
        }
        let limits = limits.validate()?;
        if route.len() > limits.max_path_bytes {
            return Err(HttpError::PathExceedsLimit);
        }
        let handler: SharedHttpHandler = Arc::new(handler);
        let inner = register_http(
            native_path,
            description,
            route,
            has_subtree,
            methods,
            limits,
            handler,
        )
        .map_err(|_| HttpError::RegistrationFailed)?;
        Ok(HttpRegistration { _inner: inner })
    }
}
