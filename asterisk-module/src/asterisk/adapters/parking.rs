//! Asterisk parking commands and Stasis event subscriptions.

use std::sync::Arc;

use crate::asterisk::raw::bridge as native;
use crate::call::parking::{ParkingError, ParkingEvent, ParkingEventSource, ParkingSubscription};
use crate::pbx::operations::{
    CallFeatureError, validate_optional_identifier, validate_parking_slot,
};
use crate::pbx::party::AsteriskChannel;

#[derive(Clone, Copy, Debug, Default)]
pub struct AsteriskParking;

impl AsteriskParking {
    pub const fn new() -> Self {
        Self
    }
}

impl ParkingEventSource for AsteriskParking {
    fn subscribe<F>(&self, callback: F) -> Result<ParkingSubscription, ParkingError>
    where
        F: Fn(ParkingEvent) + Send + Sync + 'static,
    {
        native::subscribe_parking(Arc::new(callback)).map(ParkingSubscription::new)
    }
}

pub(super) fn park(
    channel: &AsteriskChannel<'_>,
    lot: Option<&str>,
) -> Result<(), CallFeatureError> {
    validate_optional_identifier("parking lot", lot)?;
    native::park_channel(channel, lot)
}

pub(super) fn retrieve(
    channel: &AsteriskChannel<'_>,
    lot: Option<&str>,
    slot: &str,
) -> Result<(), CallFeatureError> {
    validate_optional_identifier("parking lot", lot)?;
    validate_parking_slot(slot)?;
    native::retrieve_parked_channel(channel, lot, slot)
}
