//! Asterisk-backed dialplan function/application registration.

use crate::asterisk::raw::dialplan::{
    NativeDialplanRegistration, register_dialplan_application, register_dialplan_function,
};
use crate::pbx::dialplan::{
    DialplanApplicationInvocation, DialplanApplicationResult, DialplanBackend,
    DialplanCallbackResult, DialplanError, DialplanEscalation, DialplanFunctionHandlers,
    DialplanLimits, validated_description, validated_name, validated_synopsis,
};

#[derive(Clone, Copy, Debug, Default)]
pub struct AsteriskDialplan;

impl AsteriskDialplan {
    pub const fn new() -> Self {
        Self
    }
}

pub struct DialplanRegistration {
    _inner: NativeDialplanRegistration,
}

impl DialplanBackend for AsteriskDialplan {
    type Registration = DialplanRegistration;

    fn register_function(
        &self,
        name: &str,
        synopsis: &str,
        description: &str,
        escalation: DialplanEscalation,
        limits: DialplanLimits,
        handlers: DialplanFunctionHandlers,
    ) -> Result<Self::Registration, DialplanError> {
        let name = validated_name(name)?;
        let synopsis = validated_synopsis(synopsis)?;
        let description = validated_description(description)?;
        handlers.validate_for(escalation)?;
        let limits = limits.validate()?;
        let inner =
            register_dialplan_function(name, synopsis, description, escalation, limits, handlers)?;
        Ok(DialplanRegistration { _inner: inner })
    }

    fn register_application<F>(
        &self,
        name: &str,
        synopsis: &str,
        description: &str,
        limits: DialplanLimits,
        handler: F,
    ) -> Result<Self::Registration, DialplanError>
    where
        F: for<'a> Fn(
                DialplanApplicationInvocation<'a>,
            ) -> DialplanCallbackResult<DialplanApplicationResult>
            + Send
            + Sync
            + 'static,
    {
        let name = validated_name(name)?;
        let synopsis = validated_synopsis(synopsis)?;
        let description = validated_description(description)?;
        let limits = limits.validate()?;
        let inner =
            register_dialplan_application(name, synopsis, description, limits, Box::new(handler))?;
        Ok(DialplanRegistration { _inner: inner })
    }
}
