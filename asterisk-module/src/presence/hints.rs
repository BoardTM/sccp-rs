//! Policy-free extension-hint lookup and typed state subscriptions.

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
use std::sync::Arc;

use thiserror::Error;

/// The devices and display name configured by an extension hint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Hint {
    pub devices: String,
    pub name: String,
    pub state: ExtensionState,
}

/// A stable representation of Asterisk's combinable extension state flags.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExtensionState(i32);

impl ExtensionState {
    pub const REMOVED: Self = Self(-2);
    pub const DEACTIVATED: Self = Self(-1);
    pub const IDLE: Self = Self(0);
    pub const IN_USE: Self = Self(1 << 0);
    pub const BUSY: Self = Self(1 << 1);
    pub const UNAVAILABLE: Self = Self(1 << 2);
    pub const RINGING: Self = Self(1 << 3);
    pub const ON_HOLD: Self = Self(1 << 4);

    pub const fn from_raw(value: i32) -> Self {
        Self(value)
    }

    pub const fn raw(self) -> i32 {
        self.0
    }

    pub const fn contains(self, state: Self) -> bool {
        if state.0 == 0 {
            self.0 == 0
        } else {
            self.0 >= 0 && state.0 > 0 && self.0 & state.0 == state.0
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HintUpdateReason {
    Device,
    Presence,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HintUpdate {
    pub context: String,
    pub extension: String,
    pub state: ExtensionState,
    pub reason: HintUpdateReason,
    pub caller: Option<HintCaller>,
}

/// Caller metadata attached to a device-state transition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HintCaller {
    pub name: Option<String>,
    pub number: Option<String>,
    pub name_presentation: i32,
    pub number_presentation: i32,
}

impl HintCaller {
    /// Returns true only when every present identity component is explicitly
    /// presentation-allowed.
    pub fn presentation_allowed(&self) -> bool {
        const RESTRICTION_MASK: i32 = 0x60;
        const ALLOWED: i32 = 0x00;
        let any = self.name.is_some() || self.number.is_some();
        any && self
            .name
            .as_ref()
            .is_none_or(|_| self.name_presentation & RESTRICTION_MASK == ALLOWED)
            && self
                .number
                .as_ref()
                .is_none_or(|_| self.number_presentation & RESTRICTION_MASK == ALLOWED)
    }
}

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
pub(crate) type OwnedHintCallback = Arc<dyn Fn(HintUpdate) + Send + Sync + 'static>;

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
pub(crate) trait HintBackend {
    type Subscription: Send + 'static;

    fn lookup(&self, context: &str, extension: &str) -> Result<Option<Hint>, HintError>;

    fn subscribe(
        &self,
        context: &str,
        extension: &str,
        callback: OwnedHintCallback,
    ) -> Result<Self::Subscription, HintError>;
}

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
pub(crate) fn dispatch_lookup(
    backend: &impl HintBackend,
    context: &str,
    extension: &str,
) -> Result<Option<Hint>, HintError> {
    validate_hint_key("context", context)?;
    validate_hint_key("extension", extension)?;
    backend.lookup(context, extension)
}

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
pub(crate) fn dispatch_subscribe<Backend: HintBackend>(
    backend: &Backend,
    context: &str,
    extension: &str,
    callback: OwnedHintCallback,
) -> Result<Backend::Subscription, HintError> {
    validate_hint_key("context", context)?;
    validate_hint_key("extension", extension)?;
    backend.subscribe(context, extension, callback)
}

#[derive(Debug, Error)]
pub enum HintError {
    #[error("{field} contains a NUL byte")]
    InvalidText { field: &'static str },
    #[error("extension-hint lookup failed")]
    LookupFailed,
    #[error("unable to subscribe to extension hint")]
    SubscribeFailed,
    #[error("{field} is not valid UTF-8")]
    InvalidUtf8 { field: &'static str },
    #[error("Asterisk hint services are unavailable in development builds")]
    Unavailable,
}

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
pub(crate) fn validate_hint_key(field: &'static str, value: &str) -> Result<(), HintError> {
    if value.contains('\0') {
        Err(HintError::InvalidText { field })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct FakeBackend {
        callback: Mutex<Option<OwnedHintCallback>>,
    }

    impl HintBackend for FakeBackend {
        type Subscription = ();

        fn lookup(&self, context: &str, extension: &str) -> Result<Option<Hint>, HintError> {
            assert_eq!((context, extension), ("internal", "1000"));
            Ok(Some(Hint {
                devices: "PJSIP/1000".into(),
                name: "Desk".into(),
                state: ExtensionState::IN_USE,
            }))
        }

        fn subscribe(
            &self,
            _context: &str,
            _extension: &str,
            callback: OwnedHintCallback,
        ) -> Result<Self::Subscription, HintError> {
            *self.callback.lock().unwrap() = Some(callback);
            Ok(())
        }
    }

    #[test]
    fn typed_backend_delivers_owned_updates() {
        let backend = FakeBackend::default();
        let received = Arc::new(Mutex::new(None));
        let capture = Arc::clone(&received);
        dispatch_subscribe(
            &backend,
            "internal",
            "1000",
            Arc::new(move |update| *capture.lock().unwrap() = Some(update)),
        )
        .unwrap();
        backend.callback.lock().unwrap().as_ref().unwrap()(HintUpdate {
            context: "internal".into(),
            extension: "1000".into(),
            state: ExtensionState::RINGING,
            reason: HintUpdateReason::Device,
            caller: None,
        });
        assert_eq!(
            received.lock().unwrap().as_ref().unwrap().state,
            ExtensionState::RINGING
        );
        assert_eq!(
            dispatch_lookup(&backend, "internal", "1000")
                .unwrap()
                .unwrap()
                .name,
            "Desk"
        );
    }

    #[test]
    fn combined_extension_states_preserve_each_flag() {
        let state = ExtensionState::from_raw(
            ExtensionState::IN_USE.raw()
                | ExtensionState::RINGING.raw()
                | ExtensionState::ON_HOLD.raw(),
        );
        assert!(state.contains(ExtensionState::IN_USE));
        assert!(state.contains(ExtensionState::RINGING));
        assert!(state.contains(ExtensionState::ON_HOLD));
        assert!(!state.contains(ExtensionState::BUSY));
        assert!(!state.contains(ExtensionState::IDLE));
        assert!(ExtensionState::IDLE.contains(ExtensionState::IDLE));
        assert!(!ExtensionState::DEACTIVATED.contains(ExtensionState::IN_USE));
    }

    #[test]
    fn caller_presentation_requires_every_present_component_to_be_allowed() {
        let allowed = HintCaller {
            name: Some("Taylor".into()),
            number: Some("5550100".into()),
            name_presentation: 0x01,
            number_presentation: 0x03,
        };
        assert!(allowed.presentation_allowed());
        let restricted = HintCaller {
            number_presentation: 0x20,
            ..allowed
        };
        assert!(!restricted.presentation_allowed());
    }

    #[test]
    fn rejects_nul_bytes_before_hint_lookup() {
        let error = validate_hint_key("extension", "10\0x").unwrap_err();
        assert!(matches!(
            error,
            HintError::InvalidText { field: "extension" }
        ));
    }
}
