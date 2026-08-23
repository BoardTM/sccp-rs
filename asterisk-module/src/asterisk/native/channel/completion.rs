//! Typed primitives for Asterisk's generic CCSS implementation.

use std::ffi::c_int;
use std::ptr::NonNull;

use crate::asterisk::raw::handles::BorrowedChannelLock as ChannelLock;
use crate::asterisk::sys;
use crate::call::completion::{CallCompletionError, CallCompletionTicket};

pub unsafe fn configure_generic_completion(
    channel: NonNull<sys::ast_channel>,
) -> Result<(), CallCompletionError> {
    let _lock =
        unsafe { ChannelLock::acquire(channel) }.map_err(|_| CallCompletionError::Unavailable)?;
    let parameters = unsafe { sys::ast_channel_get_cc_config_params(channel.as_ptr()) };
    if parameters.is_null() {
        return Err(CallCompletionError::Unavailable);
    }
    if unsafe { sys::ast_set_cc_agent_policy(parameters, sys::AST_CC_AGENT_GENERIC) } != 0
        || unsafe { sys::ast_set_cc_monitor_policy(parameters, sys::AST_CC_MONITOR_GENERIC) } != 0
    {
        return Err(CallCompletionError::Rejected);
    }
    Ok(())
}

pub unsafe fn accept_completion_request(
    channel: NonNull<sys::ast_channel>,
) -> Result<CallCompletionTicket, CallCompletionError> {
    let core_id = unsafe { sys::ast_cc_get_current_core_id(channel.as_ptr()) };
    let ticket = call_completion_ticket(core_id)?;
    if unsafe { sys::ast_cc_request_is_within_limits() } == 0 {
        return Err(CallCompletionError::LimitReached);
    }
    if unsafe {
        sys::ast_cc_agent_accept_request(
            core_id,
            c"%s".as_ptr(),
            c"SCCP handset accepted call-completion offer".as_ptr(),
        )
    } != 0
    {
        return Err(CallCompletionError::Rejected);
    }
    Ok(ticket)
}

fn call_completion_ticket(core_id: c_int) -> Result<CallCompletionTicket, CallCompletionError> {
    let core_id = u32::try_from(core_id).map_err(|_| CallCompletionError::NoOffer)?;
    Ok(CallCompletionTicket { core_id })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_core_ids_become_typed_tickets_or_no_offer() {
        assert_eq!(
            call_completion_ticket(73),
            Ok(CallCompletionTicket { core_id: 73 })
        );
        assert_eq!(
            call_completion_ticket(-1),
            Err(CallCompletionError::NoOffer)
        );
    }
}
