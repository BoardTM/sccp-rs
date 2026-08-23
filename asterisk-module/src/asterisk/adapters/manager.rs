//! Asterisk-backed AMI backend and owning action registration.

use crate::ami::manager::{
    ManagerBackend, ManagerError, ManagerEvent, ManagerLimits, ManagerPrivilege, ManagerRequest,
    ManagerResponse, validate_description, validate_manager_token, validate_synopsis,
};
use crate::asterisk::raw::manager::{
    NativeManagerActionRegistration, publish_manager_event, register_manager_action,
};

#[derive(Clone, Copy, Debug, Default)]
pub struct AsteriskManager;

impl AsteriskManager {
    pub const fn new() -> Self {
        Self
    }
}

pub struct ManagerActionRegistration {
    _inner: NativeManagerActionRegistration,
}

impl ManagerBackend for AsteriskManager {
    type Registration = ManagerActionRegistration;

    fn register_action<F>(
        &self,
        action: &str,
        authority: ManagerPrivilege,
        synopsis: &str,
        description: &str,
        limits: ManagerLimits,
        handler: F,
    ) -> Result<Self::Registration, ManagerError>
    where
        F: Fn(ManagerRequest) -> ManagerResponse + Send + Sync + 'static,
    {
        validate_manager_token(action, "action")?;
        if !authority.is_valid() {
            return Err(ManagerError::InvalidPrivileges);
        }
        validate_synopsis(synopsis)?;
        validate_description(description)?;
        let limits = limits.validate()?;
        let inner = register_manager_action(
            action,
            authority.bits(),
            synopsis,
            description,
            limits,
            handler,
        )?;
        Ok(ManagerActionRegistration { _inner: inner })
    }

    fn publish(&self, event: &ManagerEvent, limits: ManagerLimits) -> Result<(), ManagerError> {
        publish_manager_event(event, limits.validate()?)
    }
}
