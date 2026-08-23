//! Asterisk adapter for the domain call-completion backend.

use std::ptr::NonNull;

use crate::asterisk::raw::channel::{accept_completion_request, configure_generic_completion};
use crate::asterisk::sys;
use crate::call::completion::{
    CallCompletionBackend, CallCompletionError, CallCompletionOwnership, CallCompletionTicket,
    request_owned_with,
};
use crate::pbx::party::AsteriskChannel;

#[derive(Clone, Copy, Debug, Default)]
pub struct AsteriskCallCompletion;

impl AsteriskCallCompletion {
    pub const fn new() -> Self {
        Self
    }

    pub fn configure(&self, channel: &AsteriskChannel<'_>) -> Result<(), CallCompletionError> {
        <Self as CallCompletionBackend<AsteriskChannel<'_>>>::configure(self, channel)
    }

    pub fn request_owned(
        &self,
        ownership: CallCompletionOwnership<'_>,
        channel: Option<&AsteriskChannel<'_>>,
    ) -> Result<CallCompletionTicket, CallCompletionError> {
        request_owned_with(self, ownership, channel)
    }
}

impl<'a> CallCompletionBackend<AsteriskChannel<'a>> for AsteriskCallCompletion {
    fn configure(&self, channel: &AsteriskChannel<'a>) -> Result<(), CallCompletionError> {
        let channel = NonNull::new(channel.as_raw().cast::<sys::ast_channel>())
            .ok_or(CallCompletionError::Unavailable)?;
        unsafe { configure_generic_completion(channel) }
    }

    fn request(
        &self,
        channel: &AsteriskChannel<'a>,
    ) -> Result<CallCompletionTicket, CallCompletionError> {
        let channel = NonNull::new(channel.as_raw().cast::<sys::ast_channel>())
            .ok_or(CallCompletionError::Unavailable)?;
        unsafe { accept_completion_request(channel) }
    }
}
