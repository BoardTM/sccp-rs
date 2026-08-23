//! Asterisk-backed implementation of the typed bridge and pickup domain port.

use crate::asterisk::raw::bridge as native;
use crate::asterisk::raw::handles::ChannelRef;
use crate::pbx::operations::{
    BargeBridgeSession, BridgeSession, CallFeatureError, CallFeatureProvider, PickupResult,
    bounded_text, validate_nul,
};
use crate::pbx::party::AsteriskChannel;
use crate::runtime::backend::{PbxBridgeId, PickupOutcome};

#[derive(Clone, Copy, Debug, Default)]
pub struct AsteriskCallFeatures;

impl AsteriskCallFeatures {
    pub const fn new() -> Self {
        Self
    }
}

impl CallFeatureProvider for AsteriskCallFeatures {
    fn create_bridge(&self, bridge_id: PbxBridgeId) -> Result<BridgeSession, CallFeatureError> {
        native::create_bridge(&format!("sccp-bridge-{}", bridge_id.0)).map(BridgeSession::new)
    }

    fn acquire_barge_bridge(
        &self,
        bridge_id: PbxBridgeId,
        target: &AsteriskChannel<'_>,
    ) -> Result<BargeBridgeSession, CallFeatureError> {
        native::acquire_barge_bridge(&format!("sccp-barge-{}", bridge_id.0), target)
            .map(BargeBridgeSession::new)
    }

    fn group_pickup(
        &self,
        channel: &AsteriskChannel<'_>,
        answer: bool,
    ) -> Result<PickupResult, CallFeatureError> {
        native::pickup_group(channel, answer)
            .map(|(channel, parties)| PickupResult::new(Box::new(channel), parties))
    }

    fn directed_pickup(
        &self,
        channel: &AsteriskChannel<'_>,
        extension: &str,
        context: &str,
        answer: bool,
    ) -> Result<PickupResult, CallFeatureError> {
        bounded_text("extension", extension)?;
        bounded_text("context", context)?;
        native::pickup_directed(channel, extension, context, answer)
            .map(|(channel, parties)| PickupResult::new(Box::new(channel), parties))
    }

    fn configure_pickup(
        &self,
        channel: &AsteriskChannel<'_>,
        call_groups: u64,
        pickup_groups: u64,
        named_call_groups: &str,
        named_pickup_groups: &str,
        private_call: bool,
    ) -> Result<(), CallFeatureError> {
        validate_nul("named call groups", named_call_groups)?;
        validate_nul("named pickup groups", named_pickup_groups)?;
        native::configure_pickup(
            channel,
            call_groups,
            pickup_groups,
            named_call_groups,
            named_pickup_groups,
            private_call,
        )
    }

    fn park(
        &self,
        channel: &AsteriskChannel<'_>,
        lot: Option<&str>,
    ) -> Result<(), CallFeatureError> {
        super::parking::park(channel, lot)
    }

    fn retrieve(
        &self,
        channel: &AsteriskChannel<'_>,
        lot: Option<&str>,
        slot: &str,
    ) -> Result<(), CallFeatureError> {
        super::parking::retrieve(channel, lot, slot)
    }
}

/// Recover the private native pickup owner at the composition edge. The
/// domain result itself remains pointer- and ABI-free.
pub fn native_pickup_result(
    result: PickupResult,
) -> Result<(ChannelRef, PickupOutcome), CallFeatureError> {
    let (channel, parties) = result.into_parts();
    let channel = channel
        .into_any()
        .downcast::<native::NativePickupChannel>()
        .map_err(|_| CallFeatureError::NativeFailure {
            operation: "recover pickup channel",
        })?
        .into_channel_ref();
    Ok((channel, parties))
}
