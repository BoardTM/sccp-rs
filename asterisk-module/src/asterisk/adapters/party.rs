//! Typed composition adapter for Asterisk party metadata.

use crate::pbx::party::{
    AsteriskChannel, ConnectedLineUpdate, Delivery, PartySnapshot, PartyUpdateError,
    RedirectingUpdate, dispatch_connected, dispatch_redirecting, dispatch_snapshot,
};

use crate::asterisk::raw::channel::NativePartyAdapter;

#[derive(Clone, Copy, Debug, Default)]
pub struct AsteriskPartyUpdates;

impl AsteriskPartyUpdates {
    pub const fn new() -> Self {
        Self
    }

    pub fn snapshot(
        &self,
        channel: &AsteriskChannel<'_>,
    ) -> Result<PartySnapshot, PartyUpdateError> {
        dispatch_snapshot(&NativePartyAdapter, channel)
    }

    pub fn set_connected_line(
        &self,
        channel: &AsteriskChannel<'_>,
        update: &ConnectedLineUpdate,
    ) -> Result<(), PartyUpdateError> {
        dispatch_connected(&NativePartyAdapter, channel, update, Delivery::Set)
    }

    pub fn set_redirecting(
        &self,
        channel: &AsteriskChannel<'_>,
        update: &RedirectingUpdate,
    ) -> Result<(), PartyUpdateError> {
        dispatch_redirecting(&NativePartyAdapter, channel, update, Delivery::Set)
    }
}
