//! Owned bridge, pickup, and parking operations.
//!
//! This module owns validation, typed outcomes, and public RAII handles. Raw
//! Asterisk bridges, channels, statuses, and rollback mechanics remain in the
//! native adapter.

use thiserror::Error;

use crate::pbx::party::AsteriskChannel;
use crate::runtime::backend::{PbxBridgeId, PickupOutcome};

const MAX_NATIVE_TEXT: usize = 255;
const MAX_MERGE_CHANNELS: usize = 16;

/// Typed call-feature port implemented by PBX adapters and domain fakes.
pub trait CallFeatureProvider {
    fn create_bridge(&self, bridge_id: PbxBridgeId) -> Result<BridgeSession, CallFeatureError>;
    fn acquire_barge_bridge(
        &self,
        bridge_id: PbxBridgeId,
        target: &AsteriskChannel<'_>,
    ) -> Result<BargeBridgeSession, CallFeatureError>;
    fn group_pickup(
        &self,
        channel: &AsteriskChannel<'_>,
        answer: bool,
    ) -> Result<PickupResult, CallFeatureError>;
    fn directed_pickup(
        &self,
        channel: &AsteriskChannel<'_>,
        extension: &str,
        context: &str,
        answer: bool,
    ) -> Result<PickupResult, CallFeatureError>;
    #[allow(clippy::too_many_arguments)]
    fn configure_pickup(
        &self,
        channel: &AsteriskChannel<'_>,
        call_groups: u64,
        pickup_groups: u64,
        named_call_groups: &str,
        named_pickup_groups: &str,
        private_call: bool,
    ) -> Result<(), CallFeatureError>;
    fn park(
        &self,
        channel: &AsteriskChannel<'_>,
        lot: Option<&str>,
    ) -> Result<(), CallFeatureError>;
    fn retrieve(
        &self,
        channel: &AsteriskChannel<'_>,
        lot: Option<&str>,
        slot: &str,
    ) -> Result<(), CallFeatureError>;
}

/// Owns the retained PBX channel that carries a successfully picked-up call.
/// Consuming the result transfers that reference to the adapter's call index.
pub struct PickupResult {
    channel: Option<Box<dyn PickupChannelControl>>,
    parties: PickupOutcome,
}

impl PickupResult {
    pub fn new(channel: Box<dyn PickupChannelControl>, parties: PickupOutcome) -> Self {
        Self {
            channel: Some(channel),
            parties,
        }
    }

    pub fn parties(&self) -> &PickupOutcome {
        &self.parties
    }

    pub fn into_parts(mut self) -> (Box<dyn PickupChannelControl>, PickupOutcome) {
        let channel = self
            .channel
            .take()
            .expect("successful pickup owns its retained channel");
        let parties = std::mem::take(&mut self.parties);
        (channel, parties)
    }
}

/// Opaque ownership token for a successfully picked-up PBX channel.
pub trait PickupChannelControl: Send {
    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any + Send>;
}

/// Owns one native mixing bridge. Dropping it removes its participants and
/// releases the bridge.
pub struct BridgeSession {
    inner: Box<dyn BridgeControl>,
}

impl BridgeSession {
    pub fn new(inner: Box<dyn BridgeControl>) -> Self {
        Self { inner }
    }
}

/// Owns a reference to the bridge hosting a shared call. It may own a newly
/// created bridge or borrow the call's existing bridge.
pub struct BargeBridgeSession {
    inner: Box<dyn BargeBridgeControl>,
}

impl BargeBridgeSession {
    pub fn new(inner: Box<dyn BargeBridgeControl>) -> Self {
        Self { inner }
    }
}

/// Typed bridge port implemented by the native Asterisk adapter and by domain
/// fakes. No Asterisk pointer, status integer, or ABI record crosses it.
pub trait BridgeControl: Send {
    fn add(&mut self, channel: &AsteriskChannel<'_>) -> Result<(), CallFeatureError>;
    fn remove(&mut self, channel: &AsteriskChannel<'_>) -> Result<(), CallFeatureError>;
    fn merge_consultation(
        &mut self,
        original: &AsteriskChannel<'_>,
        consultation: &AsteriskChannel<'_>,
    ) -> Result<(), CallFeatureError>;
    fn merge_calls(&mut self, channels: &[AsteriskChannel<'_>]) -> Result<(), CallFeatureError>;
    fn merge_participant(&mut self, channel: &AsteriskChannel<'_>) -> Result<(), CallFeatureError>;
    fn set_participant_muted(
        &mut self,
        channel: &AsteriskChannel<'_>,
        muted: bool,
    ) -> Result<(), CallFeatureError>;
    fn set_participant_music_on_hold(
        &mut self,
        channel: &AsteriskChannel<'_>,
        class: &str,
        enabled: bool,
    ) -> Result<(), CallFeatureError>;
    fn remove_participant_and_hangup(
        &mut self,
        channel: &AsteriskChannel<'_>,
    ) -> Result<(), CallFeatureError>;
    fn destroy(self: Box<Self>) -> Result<(), CallFeatureError>;
}

/// Typed barge-bridge port. Borrowed and owned Asterisk bridge semantics stay
/// behind this interface.
pub trait BargeBridgeControl: Send {
    fn add(&mut self, channel: &AsteriskChannel<'_>) -> Result<(), CallFeatureError>;
    fn remove(&mut self, channel: &AsteriskChannel<'_>) -> Result<(), CallFeatureError>;
    fn release(self: Box<Self>) -> Result<(), CallFeatureError>;
}

impl BargeBridgeSession {
    pub fn add(&mut self, channel: &AsteriskChannel<'_>) -> Result<(), CallFeatureError> {
        self.inner.add(channel)
    }

    pub fn remove(&mut self, channel: &AsteriskChannel<'_>) -> Result<(), CallFeatureError> {
        self.inner.remove(channel)
    }

    pub fn release(self) -> Result<(), CallFeatureError> {
        self.inner.release()
    }
}

impl BridgeSession {
    pub fn add(&mut self, channel: &AsteriskChannel<'_>) -> Result<(), CallFeatureError> {
        self.inner.add(channel)
    }

    pub fn remove(&mut self, channel: &AsteriskChannel<'_>) -> Result<(), CallFeatureError> {
        self.inner.remove(channel)
    }

    pub fn merge_consultation(
        &mut self,
        original: &AsteriskChannel<'_>,
        consultation: &AsteriskChannel<'_>,
    ) -> Result<(), CallFeatureError> {
        self.inner.merge_consultation(original, consultation)
    }

    pub fn merge_calls(
        &mut self,
        channels: &[AsteriskChannel<'_>],
    ) -> Result<(), CallFeatureError> {
        if !(2..=MAX_MERGE_CHANNELS).contains(&channels.len()) {
            return Err(CallFeatureError::InvalidInput {
                operation: "merge conference calls",
            });
        }
        self.inner.merge_calls(channels)
    }

    pub fn merge_participant(
        &mut self,
        channel: &AsteriskChannel<'_>,
    ) -> Result<(), CallFeatureError> {
        self.inner.merge_participant(channel)
    }

    pub fn set_participant_muted(
        &mut self,
        channel: &AsteriskChannel<'_>,
        muted: bool,
    ) -> Result<(), CallFeatureError> {
        self.inner.set_participant_muted(channel, muted)
    }

    pub fn set_participant_music_on_hold(
        &mut self,
        channel: &AsteriskChannel<'_>,
        class: &str,
        enabled: bool,
    ) -> Result<(), CallFeatureError> {
        bounded_text("music on hold class", class)?;
        self.inner
            .set_participant_music_on_hold(channel, class, enabled)
    }

    pub fn remove_participant_and_hangup(
        &mut self,
        channel: &AsteriskChannel<'_>,
    ) -> Result<(), CallFeatureError> {
        self.inner.remove_participant_and_hangup(channel)
    }

    pub fn destroy(self) -> Result<(), CallFeatureError> {
        self.inner.destroy()
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum CallFeatureError {
    #[error("{field} contains a NUL byte")]
    InvalidText { field: &'static str },
    #[error("{field} must not be empty")]
    EmptyText { field: &'static str },
    #[error("{field} exceeds the native {MAX_NATIVE_TEXT}-byte limit")]
    TextTooLong { field: &'static str },
    #[error("parking lot contains unsupported characters")]
    InvalidParkingLot,
    #[error("parking slot must be an integer in 1..=2147483647")]
    InvalidParkingSlot,
    #[error("native operation {operation} rejected invalid input")]
    InvalidInput { operation: &'static str },
    #[error("native operation {operation} could not find its target")]
    NotFound { operation: &'static str },
    #[error("native operation {operation} conflicts with existing state")]
    Conflict { operation: &'static str },
    #[error("native operation {operation} is unavailable")]
    Unavailable { operation: &'static str },
    #[error("native operation {operation} failed")]
    NativeFailure { operation: &'static str },
}

pub(crate) fn validate_nul(field: &'static str, value: &str) -> Result<(), CallFeatureError> {
    if value.contains('\0') {
        Err(CallFeatureError::InvalidText { field })
    } else {
        Ok(())
    }
}

pub(crate) fn bounded_text(field: &'static str, value: &str) -> Result<(), CallFeatureError> {
    if value.is_empty() {
        return Err(CallFeatureError::EmptyText { field });
    }
    if value.len() > MAX_NATIVE_TEXT {
        return Err(CallFeatureError::TextTooLong { field });
    }
    validate_nul(field, value)
}

pub fn validate_optional_identifier(
    field: &'static str,
    value: Option<&str>,
) -> Result<(), CallFeatureError> {
    if let Some(value) = value {
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(CallFeatureError::InvalidParkingLot);
        }
        bounded_text(field, value)?;
    }
    Ok(())
}

pub fn validate_parking_slot(value: &str) -> Result<(), CallFeatureError> {
    if value.is_empty()
        || value.len() > MAX_NATIVE_TEXT
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || !matches!(value.parse::<i32>(), Ok(1..))
    {
        return Err(CallFeatureError::InvalidParkingSlot);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    struct FakeBridge {
        calls: Arc<Mutex<Vec<&'static str>>>,
    }

    impl BridgeControl for FakeBridge {
        fn add(&mut self, _channel: &AsteriskChannel<'_>) -> Result<(), CallFeatureError> {
            self.calls.lock().unwrap().push("add");
            Ok(())
        }

        fn remove(&mut self, _channel: &AsteriskChannel<'_>) -> Result<(), CallFeatureError> {
            Ok(())
        }

        fn merge_consultation(
            &mut self,
            _original: &AsteriskChannel<'_>,
            _consultation: &AsteriskChannel<'_>,
        ) -> Result<(), CallFeatureError> {
            Ok(())
        }

        fn merge_calls(
            &mut self,
            _channels: &[AsteriskChannel<'_>],
        ) -> Result<(), CallFeatureError> {
            Ok(())
        }

        fn merge_participant(
            &mut self,
            _channel: &AsteriskChannel<'_>,
        ) -> Result<(), CallFeatureError> {
            Ok(())
        }

        fn set_participant_muted(
            &mut self,
            _channel: &AsteriskChannel<'_>,
            _muted: bool,
        ) -> Result<(), CallFeatureError> {
            self.calls.lock().unwrap().push("mute");
            Ok(())
        }

        fn set_participant_music_on_hold(
            &mut self,
            _channel: &AsteriskChannel<'_>,
            _class: &str,
            _enabled: bool,
        ) -> Result<(), CallFeatureError> {
            Ok(())
        }

        fn remove_participant_and_hangup(
            &mut self,
            _channel: &AsteriskChannel<'_>,
        ) -> Result<(), CallFeatureError> {
            Ok(())
        }

        fn destroy(self: Box<Self>) -> Result<(), CallFeatureError> {
            self.calls.lock().unwrap().push("destroy");
            Ok(())
        }
    }

    #[test]
    fn validation_rejects_unsafe_text_and_slots() {
        assert_eq!(
            bounded_text("extension", ""),
            Err(CallFeatureError::EmptyText { field: "extension" })
        );
        assert_eq!(
            bounded_text("context", "bad\0context"),
            Err(CallFeatureError::InvalidText { field: "context" })
        );
        assert_eq!(
            validate_optional_identifier("parking lot", Some("bad lot")),
            Err(CallFeatureError::InvalidParkingLot)
        );
        assert_eq!(
            validate_parking_slot("0"),
            Err(CallFeatureError::InvalidParkingSlot)
        );
        assert_eq!(
            validate_parking_slot("2147483648"),
            Err(CallFeatureError::InvalidParkingSlot)
        );
    }

    #[test]
    fn typed_bridge_fake_observes_owned_session_commands() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut session = BridgeSession {
            inner: Box::new(FakeBridge {
                calls: Arc::clone(&calls),
            }),
        };
        let channel = unsafe { AsteriskChannel::from_raw(std::ptr::dangling_mut()) }.unwrap();

        session.add(&channel).unwrap();
        session.set_participant_muted(&channel, true).unwrap();
        session.destroy().unwrap();

        assert_eq!(*calls.lock().unwrap(), ["add", "mute", "destroy"]);
    }
}
