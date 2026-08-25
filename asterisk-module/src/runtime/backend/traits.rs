//! Backend capability traits and their effect dispatch contract.

use super::*;

/// Channel lifecycle and signaling operations.
pub trait ChannelBackend: PbxBackendError {
    fn create_channel(
        &self,
        handset_call_id: CallId,
        call_id: PbxCallId,
        binding: &LineBinding,
        codec: Codec,
    ) -> Result<(), Self::Error>;
    fn create_consultation_channel(
        &self,
        source_call_id: PbxCallId,
        handset_call_id: CallId,
        call_id: PbxCallId,
        binding: &LineBinding,
        codec: Codec,
    ) -> Result<(), Self::Error>;
    fn start_routing(
        &self,
        call_id: PbxCallId,
        context: &str,
        destination: &str,
    ) -> Result<(), Self::Error>;
    fn answer(&self, call_id: PbxCallId) -> Result<(), Self::Error>;
    fn hangup(&self, call_id: PbxCallId) -> Result<(), Self::Error>;
    fn send_digit(&self, call_id: PbxCallId, digit: char) -> Result<(), Self::Error>;
    fn hold(&self, call_id: PbxCallId) -> Result<(), Self::Error>;
    fn resume(&self, call_id: PbxCallId) -> Result<(), Self::Error>;
}

/// RTP endpoint configuration operations.
pub trait MediaBackend: PbxBackendError {
    /// Reports only profiles this adapter can establish for a live audio leg.
    fn audio_encryption_capabilities(&self) -> LocalEncryptionCapabilities;

    fn configure_media(
        &self,
        call_id: PbxCallId,
        remote: MediaEndpoint,
        codec: Codec,
    ) -> Result<MediaEndpoint, Self::Error>;
}

/// Bridge membership, transfer, and barge operations.
pub trait BridgeBackend: PbxBackendError {
    fn transfer(&self, operation: &TransferCompletion) -> Result<(), Self::Error>;
    fn bridge(&self, operation: &BridgeOperation) -> Result<(), Self::Error>;
    fn barge(&self, operation: &BargeOperation) -> Result<(), Self::Error>;
    fn announce(&self, operation: &ConferenceAnnouncementOperation) -> Result<(), Self::Error>;
}

/// Forwarding, voicemail, and conference-destination services.
pub trait SupplementaryBackend: PbxBackendError {
    fn forward(&self, operation: &ForwardingOperation) -> Result<(), Self::Error>;
    fn voicemail(&self, operation: &VoicemailOperation) -> Result<(), Self::Error>;
    fn start_conference_destination(
        &self,
        operation: &ConferenceDestinationOperation,
    ) -> Result<(), Self::Error>;
}

/// Pickup and parking services, whose successful pickup may return handset
/// presentation data.
pub trait CallServiceBackend: PbxBackendError {
    fn pickup(&self, operation: &PickupOperation) -> Result<PickupOutcome, Self::Error>;
    fn parking(&self, operation: &ParkingOperation) -> Result<(), Self::Error>;
}

/// Publication of adapter-neutral management events.
pub trait ManagementBackend: PbxBackendError {
    fn publish_management_event(&self, event: &ManagementEvent) -> Result<(), Self::Error>;
}

/// Complete backend capability set consumed by the ordered effect executor.
pub trait PbxBackend:
    PbxServiceCapabilities
    + ChannelBackend
    + MediaBackend
    + BridgeBackend
    + SupplementaryBackend
    + CallServiceBackend
    + ManagementBackend
{
    fn execute(&self, effect: &PbxEffect) -> Result<Option<HandsetEffect>, Self::Error> {
        match effect {
            PbxEffect::CreateChannel {
                handset_call_id,
                call_id,
                binding,
                codec,
            } => self
                .create_channel(*handset_call_id, *call_id, binding, *codec)
                .map(|()| None),
            PbxEffect::CreateConsultationChannel {
                source_call_id,
                handset_call_id,
                call_id,
                binding,
                codec,
            } => self
                .create_consultation_channel(
                    *source_call_id,
                    *handset_call_id,
                    *call_id,
                    binding,
                    *codec,
                )
                .map(|()| None),
            PbxEffect::StartRouting {
                call_id,
                context,
                destination,
            } => self
                .start_routing(*call_id, context, destination)
                .map(|()| None),
            PbxEffect::Forward { operation } => self.forward(operation).map(|()| None),
            PbxEffect::Voicemail { operation } => self.voicemail(operation).map(|()| None),
            PbxEffect::StartConferenceDestination { operation } => {
                self.start_conference_destination(operation).map(|()| None)
            }
            PbxEffect::Answer { call_id } => self.answer(*call_id).map(|()| None),
            PbxEffect::Hangup { call_id } => self.hangup(*call_id).map(|()| None),
            PbxEffect::SendDigit { call_id, digit } => {
                self.send_digit(*call_id, *digit).map(|()| None)
            }
            PbxEffect::ConfigureMedia {
                call_id,
                device_id,
                handset_call_id,
                codec,
                remote,
            } => self
                .configure_media(*call_id, *remote, *codec)
                .map(|endpoint| {
                    Some(HandsetEffect::StartMedia {
                        device_id: device_id.clone(),
                        call_id: *handset_call_id,
                        endpoint,
                    })
                }),
            PbxEffect::ConfigureMediaOnly {
                call_id,
                codec,
                remote,
            } => self
                .configure_media(*call_id, *remote, *codec)
                .map(|_| None),
            PbxEffect::Hold { call_id } => self.hold(*call_id).map(|()| None),
            PbxEffect::Resume { call_id } => self.resume(*call_id).map(|()| None),
            PbxEffect::Transfer { operation } => self.transfer(operation).map(|()| None),
            PbxEffect::Bridge { operation } => self.bridge(operation).map(|()| None),
            PbxEffect::Barge { operation } => self.barge(operation).map(|()| None),
            PbxEffect::Pickup { operation } => self.pickup(operation).map(|parties| {
                let (device_id, call_id, codec, answer) = operation.handset();
                Some(HandsetEffect::PickupCompleted {
                    device_id: device_id.clone(),
                    call_id,
                    codec,
                    answer,
                    parties,
                })
            }),
            PbxEffect::Parking { operation } => self.parking(operation).map(|()| None),
            PbxEffect::ConferenceAnnouncement { operation } => {
                self.announce(operation).map(|()| None)
            }
            PbxEffect::PublishManagementEvent { event } => {
                self.publish_management_event(event).map(|()| None)
            }
        }
    }
}

impl<T> PbxBackend for T where
    T: PbxServiceCapabilities
        + ChannelBackend
        + MediaBackend
        + BridgeBackend
        + SupplementaryBackend
        + CallServiceBackend
        + ManagementBackend
{
}
