//! call service backend-effect translation.

use super::{
    AsteriskBackend, AsteriskBackendError, CallFeatureProvider as _, CallServiceBackend,
    ChannelBinding, MutexExt as _, ParkingOperation, PickupOperation, PickupOutcome,
    native_bridging, native_pickup_result,
};

impl CallServiceBackend for AsteriskBackend<'_> {
    fn pickup(&self, operation: &PickupOperation) -> Result<PickupOutcome, Self::Error> {
        let (call_id, result) = match operation {
            PickupOperation::Group {
                call_id, answer, ..
            } => (
                *call_id,
                self.with_call_feature_channel("group pickup", *call_id, |channel| {
                    self.call_features
                        .group_pickup(channel, *answer)
                        .map_err(AsteriskBackendError::CallFeature)
                })?,
            ),
            PickupOperation::Directed {
                call_id,
                extension,
                context,
                answer,
                ..
            } => (
                *call_id,
                self.with_call_feature_channel("directed pickup", *call_id, |channel| {
                    self.call_features
                        .directed_pickup(channel, extension, context, *answer)
                        .map_err(AsteriskBackendError::CallFeature)
                })?,
            ),
        };
        let (replacement, parties) =
            native_pickup_result(result).map_err(AsteriskBackendError::CallFeature)?;
        let replaced = self
            .access
            .shared
            .channels
            .lock_unpoisoned()
            .insert(call_id, ChannelBinding::new(replacement));
        if let Some(replaced) = replaced {
            drop(replaced.close());
        }
        Ok(parties)
    }

    fn parking(&self, operation: &ParkingOperation) -> Result<(), Self::Error> {
        match operation {
            ParkingOperation::Park { call_id, lot } => {
                self.with_call_feature_channel("park call", *call_id, |channel| {
                    let unique_id = native_bridging::parking_peer_uniqueid(channel)
                        .map_err(AsteriskBackendError::CallFeature)?;
                    if let Some(unique_id) = unique_id {
                        let mut pending = self.access.shared.pending_parks.lock_unpoisoned();
                        if let Some(attempt) = pending
                            .values_mut()
                            .find(|attempt| attempt.pbx_id == *call_id)
                        {
                            attempt.parkee_unique_id = Some(unique_id);
                        }
                    }
                    self.call_features
                        .park(channel, lot.as_deref())
                        .map_err(AsteriskBackendError::CallFeature)
                })
            }
            ParkingOperation::Retrieve { call_id, lot, slot } => {
                self.with_call_feature_channel("retrieve parked call", *call_id, |channel| {
                    self.call_features
                        .retrieve(channel, lot.as_deref(), slot)
                        .map_err(AsteriskBackendError::CallFeature)
                })
            }
        }
    }
}
