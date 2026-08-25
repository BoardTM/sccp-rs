//! Backend-neutral effects produced by the controller and their execution boundary.
//!
//! Effects are ordered transactions, not an eventually consistent command
//! list. In particular, ordinary [`PbxEffect::ConfigureMedia`] updates the PBX
//! endpoint and returns the handset transmit request that must complete before
//! the next effect. [`PbxEffect::ConfigureMediaOnly`] is reserved for an
//! already-coupled early-media transaction and deliberately emits no duplicate
//! transmit request. Cleanup callers attempt every terminal effect even after
//! an individual backend or handset failure.

use std::error::Error;
use std::fmt;
use std::future::Future;
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
use std::ops::BitOrAssign;

use sccp_protocol::{
    CallId, CallInfo, CallState as HandsetCallState, Codec, ConferenceId, ConferenceListEntry,
    DeviceId, MediaEndpoint, ParticipantId, PassthroughPartyId, SessionGeneration, Tone,
};

use crate::call::forwarding::ForwardingOperation;
use crate::call::transfer::TransferCompletion;
use crate::call::voicemail::VoicemailOperation;
use crate::config::LineBinding;
use crate::media::encryption::LocalEncryptionCapabilities;
use crate::media::recording::RecordingProvider;
use crate::presence::blf::HintProvider;
use crate::state::persistence::PersistentStore;

use super::controller::ConferenceMutationToken;

macro_rules! backend_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(pub u64);

        impl $name {
            pub const fn new(value: u64) -> Self {
                Self(value)
            }

            pub const fn get(self) -> u64 {
                self.0
            }
        }

        impl From<u64> for $name {
            fn from(value: u64) -> Self {
                Self::new(value)
            }
        }

        impl From<$name> for u64 {
            fn from(value: $name) -> Self {
                value.get()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

backend_id!(/// Driver-owned identity for a PBX channel.
    PbxCallId);

backend_id!(/// Driver-owned identity for a backend bridge, independent of PBX pointers and names.
    PbxBridgeId);

/// One configured request to run a PBX-hosted conference application for an
/// already-created outbound channel. The application name is deliberately not
/// configurable: this operation always targets the host's ConfBridge service.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConferenceDestinationOperation {
    pub call_id: PbxCallId,
    pub destination: String,
    pub application_options: String,
    pub(crate) handset_call_id: CallId,
    pub(crate) held_calls: Vec<PbxCallId>,
    pub(crate) mutation: ConferenceMutationToken,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BridgeOperation {
    Create {
        bridge_id: PbxBridgeId,
    },
    Destroy {
        bridge_id: PbxBridgeId,
    },
    AddParticipant {
        bridge_id: PbxBridgeId,
        call_id: PbxCallId,
    },
    RemoveParticipant {
        bridge_id: PbxBridgeId,
        call_id: PbxCallId,
    },
    /// Atomically merge the two live call bridges that make up an attended
    /// consultation into the driver-owned conference bridge.
    MergeConsultation {
        bridge_id: PbxBridgeId,
        original_call_id: PbxCallId,
        consultation_call_id: PbxCallId,
    },
    /// Atomically merge every selected live-call bridge into one conference.
    /// The call order is policy-significant: the first call is the moderator.
    MergeCalls {
        bridge_id: PbxBridgeId,
        call_ids: Vec<PbxCallId>,
    },
    /// Merge one newly answered consultation bridge into an existing
    /// conference without disturbing its current participants.
    MergeParticipant {
        bridge_id: PbxBridgeId,
        call_id: PbxCallId,
    },
    /// Suppress or restore audio entering the conference from one participant.
    /// The bridge and channel identities are both validated by the backend.
    SetParticipantMuted {
        bridge_id: PbxBridgeId,
        participant_id: ParticipantId,
        call_id: PbxCallId,
        muted: bool,
    },
    /// Remove one non-moderator member from a live conference and queue normal
    /// clearing for its channel after validating exact bridge membership.
    RemoveConferenceParticipant {
        bridge_id: PbxBridgeId,
        participant_id: ParticipantId,
        call_id: PbxCallId,
    },
    /// Start or stop PBX-generated music for one exact live bridge member.
    SetParticipantMusicOnHold {
        bridge_id: PbxBridgeId,
        participant_id: ParticipantId,
        call_id: PbxCallId,
        class: String,
        enabled: bool,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConferenceAnnouncement {
    Connected,
    ParticipantJoined(ParticipantId),
    ParticipantMuted(ParticipantId),
    ParticipantUnmuted(ParticipantId),
    ParticipantRemoved(ParticipantId),
    ModeratorDeparted(ParticipantId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConferenceAnnouncementTarget {
    pub participant_id: ParticipantId,
    pub call_id: PbxCallId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConferenceAnnouncementOperation {
    pub conference_id: ConferenceId,
    pub targets: Vec<ConferenceAnnouncementTarget>,
    pub announcement: ConferenceAnnouncement,
}

/// A live shared-call barge uses a separate PBX channel for the barging
/// handset while retaining the original call as the bridge anchor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BargeOperation {
    Join {
        bridge_id: PbxBridgeId,
        target_call_id: PbxCallId,
        barger_call_id: PbxCallId,
    },
    Leave {
        bridge_id: PbxBridgeId,
        barger_call_id: PbxCallId,
        last_participant: bool,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PickupOperation {
    Group {
        call_id: PbxCallId,
        device_id: DeviceId,
        handset_call_id: CallId,
        codec: Codec,
        answer: bool,
    },
    Directed {
        call_id: PbxCallId,
        device_id: DeviceId,
        handset_call_id: CallId,
        codec: Codec,
        extension: String,
        context: String,
        answer: bool,
    },
}

impl PickupOperation {
    fn handset(&self) -> (&DeviceId, CallId, Codec, bool) {
        match self {
            Self::Group {
                device_id,
                handset_call_id,
                codec,
                answer,
                ..
            }
            | Self::Directed {
                device_id,
                handset_call_id,
                codec,
                answer,
                ..
            } => (device_id, *handset_call_id, *codec, *answer),
        }
    }
}

/// Party information captured from the ringing channel before pickup moves it
/// onto the picking handset's PBX channel.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PickupOutcome {
    pub calling_name: String,
    pub calling_number: String,
    pub connected_name: String,
    pub connected_number: String,
    pub redirecting_name: String,
    pub redirecting_number: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParkingOperation {
    Park {
        call_id: PbxCallId,
        lot: Option<String>,
    },
    Retrieve {
        call_id: PbxCallId,
        lot: Option<String>,
        slot: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ManagementEventKind {
    Registration,
    Alarm,
    Feature,
    Media,
    Call,
}

#[derive(Clone, Eq, PartialEq)]
pub enum ManagementValue {
    Text(String),
    Signed(i64),
    Unsigned(u64),
    Boolean(bool),
    Redacted,
}

impl From<String> for ManagementValue {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl From<&str> for ManagementValue {
    fn from(value: &str) -> Self {
        Self::Text(value.to_owned())
    }
}

impl From<i64> for ManagementValue {
    fn from(value: i64) -> Self {
        Self::Signed(value)
    }
}

impl From<u64> for ManagementValue {
    fn from(value: u64) -> Self {
        Self::Unsigned(value)
    }
}

impl From<bool> for ManagementValue {
    fn from(value: bool) -> Self {
        Self::Boolean(value)
    }
}

impl fmt::Debug for ManagementValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text(_) => formatter.write_str("Text(<redacted>)"),
            Self::Signed(value) => formatter.debug_tuple("Signed").field(value).finish(),
            Self::Unsigned(value) => formatter.debug_tuple("Unsigned").field(value).finish(),
            Self::Boolean(value) => formatter.debug_tuple("Boolean").field(value).finish(),
            Self::Redacted => formatter.write_str("Redacted"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagementField {
    pub name: String,
    pub value: ManagementValue,
}

impl ManagementField {
    pub fn new(name: impl Into<String>, value: impl Into<ManagementValue>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }

    pub fn redacted(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: ManagementValue::Redacted,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagementEvent {
    pub kind: ManagementEventKind,
    pub fields: Vec<ManagementField>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PbxEffect {
    CreateChannel {
        handset_call_id: CallId,
        call_id: PbxCallId,
        binding: Box<LineBinding>,
        codec: Codec,
    },
    CreateConsultationChannel {
        source_call_id: PbxCallId,
        handset_call_id: CallId,
        call_id: PbxCallId,
        binding: Box<LineBinding>,
        codec: Codec,
    },
    StartRouting {
        call_id: PbxCallId,
        context: String,
        destination: String,
    },
    Forward {
        operation: ForwardingOperation,
    },
    Voicemail {
        operation: VoicemailOperation,
    },
    StartConferenceDestination {
        operation: ConferenceDestinationOperation,
    },
    Answer {
        call_id: PbxCallId,
    },
    Hangup {
        call_id: PbxCallId,
    },
    SendDigit {
        call_id: PbxCallId,
        digit: char,
    },
    ConfigureMedia {
        call_id: PbxCallId,
        device_id: DeviceId,
        handset_call_id: CallId,
        codec: Codec,
        remote: MediaEndpoint,
    },
    /// Point Asterisk at the handset after an outbound hole-punch transaction.
    /// StartMediaTransmission was already sent alongside OpenReceiveChannel,
    /// so this effect deliberately has no handset follow-up.
    ConfigureMediaOnly {
        call_id: PbxCallId,
        codec: Codec,
        remote: MediaEndpoint,
    },
    Hold {
        call_id: PbxCallId,
    },
    Resume {
        call_id: PbxCallId,
    },
    Transfer {
        operation: TransferCompletion,
    },
    Bridge {
        operation: BridgeOperation,
    },
    Barge {
        operation: BargeOperation,
    },
    Pickup {
        operation: PickupOperation,
    },
    Parking {
        operation: ParkingOperation,
    },
    ConferenceAnnouncement {
        operation: ConferenceAnnouncementOperation,
    },
    PublishManagementEvent {
        event: ManagementEvent,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HandsetEffect {
    BeginCall {
        device_id: DeviceId,
        line_instance: u32,
        call_id: CallId,
        codec: Codec,
    },
    BeginTransfer {
        device_id: DeviceId,
        source_call_id: CallId,
        consultation_call_id: CallId,
        consultation_line_instance: u32,
        codec: Codec,
    },
    StartTone {
        device_id: DeviceId,
        call_id: CallId,
        tone: Tone,
    },
    CommitOutboundCall {
        device_id: DeviceId,
        call_id: CallId,
        info: CallInfo,
    },
    PresentOutboundProceeding {
        device_id: DeviceId,
        call_id: CallId,
        info: CallInfo,
    },
    PresentOutboundRinging {
        device_id: DeviceId,
        call_id: CallId,
        info: CallInfo,
    },
    SetCallInfo {
        device_id: DeviceId,
        call_id: CallId,
        info: CallInfo,
    },
    BeginMedia {
        device_id: DeviceId,
        call_id: CallId,
        codec: Codec,
    },
    /// Open receive media for a physical inbound answer whose provisional
    /// OffHook UI has already been emitted by the protocol session. Full
    /// Connected presentation is deferred until OpenReceiveChannelAck.
    BeginAnswerMedia {
        device_id: DeviceId,
        call_id: CallId,
        codec: Codec,
    },
    /// Open both outbound media directions as one ordered transaction. Older
    /// 79x1 phones may not acknowledge receive media until transmit is open.
    BeginOutboundMedia {
        device_id: DeviceId,
        call_id: CallId,
        codec: Codec,
    },
    /// Open normal receive media while presenting a one-way intercom call.
    /// The microphone policy is a separate confirmed effect so partial
    /// execution can be compensated exactly.
    BeginOneWayMedia {
        device_id: DeviceId,
        call_id: CallId,
        codec: Codec,
    },
    BeginEarlyMedia {
        device_id: DeviceId,
        call_id: CallId,
        codec: Codec,
    },
    OpenVideoReceive {
        device_id: DeviceId,
        call_id: CallId,
        session_generation: SessionGeneration,
    },
    StartVideoTransmit {
        device_id: DeviceId,
        call_id: CallId,
        session_generation: SessionGeneration,
    },
    RefreshVideo {
        device_id: DeviceId,
        call_id: CallId,
        session_generation: SessionGeneration,
        passthrough_party_id: PassthroughPartyId,
    },
    StopVideo {
        device_id: DeviceId,
        call_id: CallId,
        session_generation: SessionGeneration,
    },
    StartMedia {
        device_id: DeviceId,
        call_id: CallId,
        endpoint: MediaEndpoint,
    },
    SetCallState {
        device_id: DeviceId,
        call_id: CallId,
        state: HandsetCallState,
        stop_media: bool,
    },
    SetMicrophoneMode {
        device_id: DeviceId,
        call_id: CallId,
        enabled: bool,
    },
    PickupCompleted {
        device_id: DeviceId,
        call_id: CallId,
        codec: Codec,
        answer: bool,
        parties: PickupOutcome,
    },
    ShowConferenceList {
        device_id: DeviceId,
        call_id: CallId,
        conference_id: ConferenceId,
        participants: Vec<ConferenceListEntry>,
    },
    ShowConferenceParticipantActions {
        device_id: DeviceId,
        call_id: CallId,
        conference_id: ConferenceId,
        participant: ConferenceListEntry,
        removable: bool,
        demotable: bool,
    },
}

impl HandsetEffect {
    pub fn device_id(&self) -> &DeviceId {
        match self {
            Self::BeginCall { device_id, .. }
            | Self::BeginTransfer { device_id, .. }
            | Self::StartTone { device_id, .. }
            | Self::CommitOutboundCall { device_id, .. }
            | Self::PresentOutboundProceeding { device_id, .. }
            | Self::PresentOutboundRinging { device_id, .. }
            | Self::SetCallInfo { device_id, .. }
            | Self::BeginMedia { device_id, .. }
            | Self::BeginAnswerMedia { device_id, .. }
            | Self::BeginOutboundMedia { device_id, .. }
            | Self::BeginOneWayMedia { device_id, .. }
            | Self::BeginEarlyMedia { device_id, .. }
            | Self::OpenVideoReceive { device_id, .. }
            | Self::StartVideoTransmit { device_id, .. }
            | Self::RefreshVideo { device_id, .. }
            | Self::StopVideo { device_id, .. }
            | Self::StartMedia { device_id, .. }
            | Self::SetCallState { device_id, .. }
            | Self::SetMicrophoneMode { device_id, .. }
            | Self::PickupCompleted { device_id, .. }
            | Self::ShowConferenceList { device_id, .. }
            | Self::ShowConferenceParticipantActions { device_id, .. } => device_id,
        }
    }

    /// Returns the call directly acted on by this handset effect.
    pub const fn subject_call_id(&self) -> CallId {
        match self {
            Self::BeginTransfer {
                consultation_call_id,
                ..
            } => *consultation_call_id,
            Self::BeginCall { call_id, .. }
            | Self::StartTone { call_id, .. }
            | Self::CommitOutboundCall { call_id, .. }
            | Self::PresentOutboundProceeding { call_id, .. }
            | Self::PresentOutboundRinging { call_id, .. }
            | Self::SetCallInfo { call_id, .. }
            | Self::BeginMedia { call_id, .. }
            | Self::BeginAnswerMedia { call_id, .. }
            | Self::BeginOutboundMedia { call_id, .. }
            | Self::BeginOneWayMedia { call_id, .. }
            | Self::BeginEarlyMedia { call_id, .. }
            | Self::OpenVideoReceive { call_id, .. }
            | Self::StartVideoTransmit { call_id, .. }
            | Self::RefreshVideo { call_id, .. }
            | Self::StopVideo { call_id, .. }
            | Self::StartMedia { call_id, .. }
            | Self::SetCallState { call_id, .. }
            | Self::SetMicrophoneMode { call_id, .. }
            | Self::PickupCompleted { call_id, .. }
            | Self::ShowConferenceList { call_id, .. }
            | Self::ShowConferenceParticipantActions { call_id, .. } => *call_id,
        }
    }

    /// Returns the call whose transition milestone this effect can settle.
    pub const fn transition_call_id(&self) -> Option<CallId> {
        match self {
            Self::BeginCall { call_id, .. }
            | Self::StartTone { call_id, .. }
            | Self::CommitOutboundCall { call_id, .. }
            | Self::PresentOutboundProceeding { call_id, .. }
            | Self::PresentOutboundRinging { call_id, .. }
            | Self::SetCallInfo { call_id, .. }
            | Self::BeginMedia { call_id, .. }
            | Self::BeginAnswerMedia { call_id, .. }
            | Self::BeginOutboundMedia { call_id, .. }
            | Self::BeginOneWayMedia { call_id, .. }
            | Self::BeginEarlyMedia { call_id, .. }
            | Self::OpenVideoReceive { call_id, .. }
            | Self::StartVideoTransmit { call_id, .. }
            | Self::RefreshVideo { call_id, .. }
            | Self::StopVideo { call_id, .. }
            | Self::StartMedia { call_id, .. }
            | Self::SetCallState { call_id, .. }
            | Self::SetMicrophoneMode { call_id, .. } => Some(*call_id),
            Self::BeginTransfer { .. }
            | Self::PickupCompleted { .. }
            | Self::ShowConferenceList { .. }
            | Self::ShowConferenceParticipantActions { .. } => None,
        }
    }
}

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ConferenceStartProgress {
    active_leg_held: bool,
    active_handset_held: bool,
    channel_created: bool,
}

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
impl ConferenceStartProgress {
    pub(crate) const fn active_leg_held(self) -> bool {
        self.active_leg_held
    }

    pub(crate) const fn active_handset_held(self) -> bool {
        self.active_handset_held
    }

    pub(crate) const fn channel_created(self) -> bool {
        self.channel_created
    }
}

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
impl From<&DriverEffect> for ConferenceStartProgress {
    fn from(effect: &DriverEffect) -> Self {
        Self {
            active_leg_held: matches!(effect, DriverEffect::Backend(PbxEffect::Hold { .. })),
            active_handset_held: matches!(
                effect,
                DriverEffect::Handset(HandsetEffect::SetCallState {
                    state: HandsetCallState::Hold,
                    ..
                })
            ),
            channel_created: matches!(
                effect,
                DriverEffect::Backend(PbxEffect::CreateChannel { .. })
            ),
        }
    }
}

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
impl BitOrAssign for ConferenceStartProgress {
    fn bitor_assign(&mut self, completed: Self) {
        self.active_leg_held |= completed.active_leg_held;
        self.active_handset_held |= completed.active_handset_held;
        self.channel_created |= completed.channel_created;
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DriverEffect {
    Backend(PbxEffect),
    Handset(HandsetEffect),
}

impl From<PbxEffect> for DriverEffect {
    fn from(effect: PbxEffect) -> Self {
        Self::Backend(effect)
    }
}

impl From<HandsetEffect> for DriverEffect {
    fn from(effect: HandsetEffect) -> Self {
        Self::Handset(effect)
    }
}

/// Direct backend services whose return values, callbacks, or owned handles do
/// not fit the queued effect executor.
pub trait PbxServiceCapabilities {
    type Persistence: PersistentStore;
    type Hints: HintProvider;
    type Recordings: RecordingProvider;

    fn persistence(&self) -> &Self::Persistence;
    fn hints(&self) -> &Self::Hints;
    fn recordings(&self) -> &Self::Recordings;
}

/// One error domain shared by the PBX capabilities implemented by a backend.
pub trait PbxBackendError {
    type Error;
}

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

#[derive(Debug)]
pub enum EffectExecutionError<BackendError, HandsetError> {
    Backend {
        index: usize,
        effect: Box<PbxEffect>,
        error: BackendError,
    },
    Handset {
        index: usize,
        effect: Box<HandsetEffect>,
        error: HandsetError,
    },
}

impl<BackendError: fmt::Display, HandsetError: fmt::Display> fmt::Display
    for EffectExecutionError<BackendError, HandsetError>
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Backend {
                index,
                effect,
                error,
            } => write!(
                formatter,
                "backend effect {index} ({effect:?}) failed: {error}"
            ),
            Self::Handset {
                index,
                effect,
                error,
            } => write!(
                formatter,
                "handset effect {index} ({effect:?}) failed: {error}"
            ),
        }
    }
}

impl<BackendError, HandsetError> Error for EffectExecutionError<BackendError, HandsetError>
where
    BackendError: Error + 'static,
    HandsetError: Error + 'static,
{
}

/// Execute effects sequentially and stop at the first failed backend or
/// handset operation. A media backend result is delivered to the handset
/// immediately before the next queued effect.
pub async fn execute_effects<Backend, SendHandset, SendFuture, HandsetError>(
    backend: &Backend,
    effects: Vec<DriverEffect>,
    mut send_handset: SendHandset,
) -> Result<(), EffectExecutionError<Backend::Error, HandsetError>>
where
    Backend: PbxBackend,
    SendHandset: FnMut(HandsetEffect) -> SendFuture,
    SendFuture: Future<Output = Result<(), HandsetError>>,
{
    for (index, effect) in effects.into_iter().enumerate() {
        match effect {
            DriverEffect::Backend(effect) => {
                let followup =
                    backend
                        .execute(&effect)
                        .map_err(|error| EffectExecutionError::Backend {
                            index,
                            effect: Box::new(effect.clone()),
                            error,
                        })?;
                if let Some(effect) = followup {
                    send_handset(effect.clone()).await.map_err(|error| {
                        EffectExecutionError::Handset {
                            index,
                            effect: Box::new(effect),
                            error,
                        }
                    })?;
                }
            }
            DriverEffect::Handset(effect) => {
                send_handset(effect.clone()).await.map_err(|error| {
                    EffectExecutionError::Handset {
                        index,
                        effect: Box::new(effect),
                        error,
                    }
                })?;
            }
        }
    }
    Ok(())
}

/// Execute terminal cleanup effects in order while attempting every queued
/// operation. Once the controller has committed terminal state, later cleanup
/// must not be skipped because an earlier native or handset target vanished.
pub async fn execute_cleanup_effects<Backend, SendHandset, SendFuture, HandsetError>(
    backend: &Backend,
    effects: Vec<DriverEffect>,
    mut send_handset: SendHandset,
) -> Vec<EffectExecutionError<Backend::Error, HandsetError>>
where
    Backend: PbxBackend,
    SendHandset: FnMut(HandsetEffect) -> SendFuture,
    SendFuture: Future<Output = Result<(), HandsetError>>,
{
    let mut errors = Vec::new();
    for (index, effect) in effects.into_iter().enumerate() {
        match effect {
            DriverEffect::Backend(effect) => match backend.execute(&effect) {
                Ok(Some(followup)) => {
                    if let Err(error) = send_handset(followup.clone()).await {
                        errors.push(EffectExecutionError::Handset {
                            index,
                            effect: Box::new(followup),
                            error,
                        });
                    }
                }
                Ok(None) => {}
                Err(error) => errors.push(EffectExecutionError::Backend {
                    index,
                    effect: Box::new(effect),
                    error,
                }),
            },
            DriverEffect::Handset(effect) => {
                if let Err(error) = send_handset(effect.clone()).await {
                    errors.push(EffectExecutionError::Handset {
                        index,
                        effect: Box::new(effect),
                        error,
                    });
                }
            }
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use super::*;
    use crate::call::forwarding::{
        ForwardingContext, ForwardingDestination, ForwardingRouteReason,
    };
    use crate::call::transfer::{TransferCompletionKind, TransferId, TransferLeg};
    use crate::call::voicemail::{VoicemailAction, VoicemailTarget, VoicemailTransactionId};
    use crate::config::HintTarget;
    use crate::config::LineConfig;
    use crate::media::recording::{
        RecordingCallback, RecordingDirection, RecordingEvent, RecordingProvider,
        RecordingSessionControl, RecordingState,
    };
    use crate::presence::blf::{HintCallback, HintSnapshot};
    use crate::presence::hints::{ExtensionState, HintUpdateReason};
    use crate::runtime::controller::Controller;
    use crate::state::persistence::PersistenceError;
    use sccp_protocol::{DeviceRegistration, DeviceType, ProtocolVersion, StationTransport};

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct FakeError(&'static str);

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.0)
        }
    }

    impl Error for FakeError {}

    #[derive(Clone, Debug, Eq, PartialEq)]
    enum AdvancedOperation {
        ConferenceDestination(ConferenceDestinationOperation),
        Forward(ForwardingOperation),
        Voicemail(VoicemailOperation),
        Transfer(TransferCompletion),
        Bridge(BridgeOperation),
        Barge(BargeOperation),
        Announcement(ConferenceAnnouncementOperation),
        Pickup(PickupOperation),
        Parking(ParkingOperation),
        Management(ManagementEvent),
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    enum ServiceRequest {
        Get(String, String),
        Put(String, String, String),
        Delete(String, String),
        HintLookup(String, String),
        HintSubscribe(String, String),
        RecordingStart(PbxCallId, String, String),
        RecordingId,
        RecordingState,
        RecordingMute(RecordingDirection, bool),
        RecordingStop,
    }

    #[derive(Clone, Default)]
    struct ServiceHarness {
        requests: Arc<Mutex<Vec<ServiceRequest>>>,
        failures: Arc<Mutex<HashSet<&'static str>>>,
        controller_probe: Option<Arc<Mutex<Controller>>>,
    }

    impl ServiceHarness {
        fn record(
            &self,
            operation: &'static str,
            request: ServiceRequest,
        ) -> Result<(), FakeError> {
            self.requests.lock().unwrap().push(request);
            if let Some(controller) = &self.controller_probe {
                assert!(
                    controller.try_lock().is_ok(),
                    "direct service operation ran while the controller was locked"
                );
            }
            if self.failures.lock().unwrap().contains(operation) {
                Err(FakeError(operation))
            } else {
                Ok(())
            }
        }

        fn fail(&self, operation: &'static str) {
            self.failures.lock().unwrap().insert(operation);
        }
    }

    #[derive(Clone, Default)]
    struct FakePersistence {
        harness: ServiceHarness,
    }

    impl PersistentStore for FakePersistence {
        fn get(&self, family: &str, key: &str) -> Result<Option<String>, PersistenceError> {
            self.harness
                .record(
                    "persistence:get",
                    ServiceRequest::Get(family.into(), key.into()),
                )
                .map_err(|_| PersistenceError::Backend { operation: "get" })?;
            Ok(Some("stored".into()))
        }

        fn put(&self, family: &str, key: &str, value: &str) -> Result<(), PersistenceError> {
            self.harness
                .record(
                    "persistence:put",
                    ServiceRequest::Put(family.into(), key.into(), value.into()),
                )
                .map_err(|_| PersistenceError::Backend { operation: "put" })
        }

        fn delete(&self, family: &str, key: &str) -> Result<(), PersistenceError> {
            self.harness
                .record(
                    "persistence:delete",
                    ServiceRequest::Delete(family.into(), key.into()),
                )
                .map_err(|_| PersistenceError::Backend {
                    operation: "delete",
                })
        }
    }

    #[derive(Clone, Default)]
    struct FakeHints {
        harness: ServiceHarness,
    }

    struct FakeHintSubscription;

    impl HintProvider for FakeHints {
        type Subscription = FakeHintSubscription;
        type Error = FakeError;

        fn lookup(&self, target: &HintTarget) -> Result<Option<HintSnapshot>, Self::Error> {
            self.harness.record(
                "hints:lookup",
                ServiceRequest::HintLookup(target.context().into(), target.extension().into()),
            )?;
            Ok(Some(HintSnapshot {
                target: target.clone(),
                state: ExtensionState::IDLE,
                reason: HintUpdateReason::Device,
                caller: None,
            }))
        }

        fn subscribe(
            &self,
            target: &HintTarget,
            callback: HintCallback,
        ) -> Result<Self::Subscription, Self::Error> {
            self.harness.record(
                "hints:subscribe",
                ServiceRequest::HintSubscribe(target.context().into(), target.extension().into()),
            )?;
            callback(HintSnapshot {
                target: target.clone(),
                state: ExtensionState::RINGING,
                reason: HintUpdateReason::Device,
                caller: None,
            });
            Ok(FakeHintSubscription)
        }
    }

    #[derive(Clone, Default)]
    struct FakeRecordings {
        harness: ServiceHarness,
    }

    struct FakeRecordingSession {
        harness: ServiceHarness,
        state: RecordingState,
    }

    impl RecordingSessionControl for FakeRecordingSession {
        type Error = FakeError;

        fn id(&self) -> Result<String, Self::Error> {
            self.harness
                .record("recording:id", ServiceRequest::RecordingId)?;
            Ok("recording-1".into())
        }

        fn state(&self) -> Result<RecordingState, Self::Error> {
            self.harness
                .record("recording:state", ServiceRequest::RecordingState)?;
            Ok(self.state)
        }

        fn stop(&mut self) -> Result<(), Self::Error> {
            self.harness
                .record("recording:stop", ServiceRequest::RecordingStop)?;
            self.state = RecordingState::Stopped;
            Ok(())
        }

        fn set_muted(
            &mut self,
            direction: RecordingDirection,
            muted: bool,
        ) -> Result<usize, Self::Error> {
            self.harness.record(
                "recording:mute",
                ServiceRequest::RecordingMute(direction, muted),
            )?;
            self.state = if muted {
                RecordingState::Muted
            } else {
                RecordingState::Active
            };
            Ok(1)
        }
    }

    impl RecordingProvider for FakeRecordings {
        type Session = FakeRecordingSession;
        type StartError = FakeError;

        fn start_recording(
            &self,
            call_id: PbxCallId,
            filename: &str,
            options: &str,
            callback: RecordingCallback,
        ) -> Result<Self::Session, Self::StartError> {
            self.harness.record(
                "recording:start",
                ServiceRequest::RecordingStart(call_id, filename.into(), options.into()),
            )?;
            callback(RecordingEvent::Started);
            Ok(FakeRecordingSession {
                harness: self.harness.clone(),
                state: RecordingState::Active,
            })
        }
    }

    #[derive(Default)]
    struct FakeCapabilities {
        persistence: FakePersistence,
        hints: FakeHints,
        recordings: FakeRecordings,
    }

    impl FakeCapabilities {
        fn with_harness(harness: ServiceHarness) -> Self {
            Self {
                persistence: FakePersistence {
                    harness: harness.clone(),
                },
                hints: FakeHints {
                    harness: harness.clone(),
                },
                recordings: FakeRecordings { harness },
            }
        }
    }

    struct FakeBackend {
        events: Arc<Mutex<Vec<&'static str>>>,
        advanced_operations: Arc<Mutex<Vec<AdvancedOperation>>>,
        capabilities: FakeCapabilities,
        fail: Option<&'static str>,
        controller_probe: Option<Arc<Mutex<Controller>>>,
    }

    impl FakeBackend {
        fn record(&self, operation: &'static str) -> Result<(), FakeError> {
            self.events.lock().unwrap().push(operation);
            if let Some(controller) = &self.controller_probe {
                assert!(
                    controller.try_lock().is_ok(),
                    "backend operation ran while the controller was locked"
                );
            }
            if self.fail == Some(operation) {
                Err(FakeError(operation))
            } else {
                Ok(())
            }
        }
    }

    impl PbxServiceCapabilities for FakeBackend {
        type Persistence = FakePersistence;
        type Hints = FakeHints;
        type Recordings = FakeRecordings;

        fn persistence(&self) -> &Self::Persistence {
            &self.capabilities.persistence
        }

        fn hints(&self) -> &Self::Hints {
            &self.capabilities.hints
        }

        fn recordings(&self) -> &Self::Recordings {
            &self.capabilities.recordings
        }
    }

    impl PbxBackendError for FakeBackend {
        type Error = FakeError;
    }

    impl ChannelBackend for FakeBackend {
        fn create_channel(
            &self,
            _: CallId,
            _: PbxCallId,
            _: &LineBinding,
            _: Codec,
        ) -> Result<(), Self::Error> {
            self.record("backend:create")
        }

        fn create_consultation_channel(
            &self,
            _: PbxCallId,
            _: CallId,
            _: PbxCallId,
            _: &LineBinding,
            _: Codec,
        ) -> Result<(), Self::Error> {
            self.record("backend:create")
        }

        fn start_routing(&self, _: PbxCallId, _: &str, _: &str) -> Result<(), Self::Error> {
            self.record("backend:route")
        }

        fn answer(&self, _: PbxCallId) -> Result<(), Self::Error> {
            self.record("backend:answer")
        }

        fn hangup(&self, _: PbxCallId) -> Result<(), Self::Error> {
            self.record("backend:hangup")
        }

        fn send_digit(&self, _: PbxCallId, _: char) -> Result<(), Self::Error> {
            self.record("backend:digit")
        }

        fn hold(&self, _: PbxCallId) -> Result<(), Self::Error> {
            self.record("backend:hold")
        }

        fn resume(&self, _: PbxCallId) -> Result<(), Self::Error> {
            self.record("backend:resume")
        }
    }

    impl SupplementaryBackend for FakeBackend {
        fn forward(&self, operation: &ForwardingOperation) -> Result<(), Self::Error> {
            self.advanced_operations
                .lock()
                .unwrap()
                .push(AdvancedOperation::Forward(operation.clone()));
            self.record("backend:forward")
        }

        fn voicemail(&self, operation: &VoicemailOperation) -> Result<(), Self::Error> {
            self.advanced_operations
                .lock()
                .unwrap()
                .push(AdvancedOperation::Voicemail(operation.clone()));
            self.record("backend:voicemail")
        }

        fn start_conference_destination(
            &self,
            operation: &ConferenceDestinationOperation,
        ) -> Result<(), Self::Error> {
            self.advanced_operations
                .lock()
                .unwrap()
                .push(AdvancedOperation::ConferenceDestination(operation.clone()));
            self.record("backend:conference-destination")
        }
    }

    impl MediaBackend for FakeBackend {
        fn audio_encryption_capabilities(&self) -> LocalEncryptionCapabilities {
            LocalEncryptionCapabilities::default()
        }

        fn configure_media(
            &self,
            _: PbxCallId,
            remote: MediaEndpoint,
            _: Codec,
        ) -> Result<MediaEndpoint, Self::Error> {
            self.record("backend:media")?;
            Ok(remote)
        }
    }

    impl BridgeBackend for FakeBackend {
        fn transfer(&self, operation: &TransferCompletion) -> Result<(), Self::Error> {
            self.advanced_operations
                .lock()
                .unwrap()
                .push(AdvancedOperation::Transfer(operation.clone()));
            self.record("backend:bridge-transfer")
        }

        fn bridge(&self, operation: &BridgeOperation) -> Result<(), Self::Error> {
            self.advanced_operations
                .lock()
                .unwrap()
                .push(AdvancedOperation::Bridge(operation.clone()));
            let operation = match operation {
                BridgeOperation::Create { .. } => "backend:bridge-create",
                BridgeOperation::Destroy { .. } => "backend:bridge-destroy",
                BridgeOperation::AddParticipant { .. } => "backend:bridge-add",
                BridgeOperation::RemoveParticipant { .. } => "backend:bridge-remove",
                BridgeOperation::MergeConsultation { .. } => "backend:bridge-merge-consultation",
                BridgeOperation::MergeCalls { .. } => "backend:bridge-merge-calls",
                BridgeOperation::MergeParticipant { .. } => "backend:bridge-merge-participant",
                BridgeOperation::SetParticipantMuted { muted: true, .. } => {
                    "backend:bridge-mute-participant"
                }
                BridgeOperation::SetParticipantMuted { muted: false, .. } => {
                    "backend:bridge-unmute-participant"
                }
                BridgeOperation::RemoveConferenceParticipant { .. } => {
                    "backend:bridge-remove-conference-participant"
                }
                BridgeOperation::SetParticipantMusicOnHold { enabled: true, .. } => {
                    "backend:bridge-start-music"
                }
                BridgeOperation::SetParticipantMusicOnHold { enabled: false, .. } => {
                    "backend:bridge-stop-music"
                }
            };
            self.record(operation)
        }

        fn barge(&self, operation: &BargeOperation) -> Result<(), Self::Error> {
            self.advanced_operations
                .lock()
                .unwrap()
                .push(AdvancedOperation::Barge(operation.clone()));
            self.record(match operation {
                BargeOperation::Join { .. } => "backend:barge-join",
                BargeOperation::Leave { .. } => "backend:barge-leave",
            })
        }

        fn announce(&self, operation: &ConferenceAnnouncementOperation) -> Result<(), Self::Error> {
            self.advanced_operations
                .lock()
                .unwrap()
                .push(AdvancedOperation::Announcement(operation.clone()));
            self.record("backend:conference-announcement")
        }
    }

    impl CallServiceBackend for FakeBackend {
        fn pickup(&self, operation: &PickupOperation) -> Result<PickupOutcome, Self::Error> {
            self.advanced_operations
                .lock()
                .unwrap()
                .push(AdvancedOperation::Pickup(operation.clone()));
            let operation = match operation {
                PickupOperation::Group { .. } => "backend:pickup-group",
                PickupOperation::Directed { .. } => "backend:pickup-directed",
            };
            self.record(operation)?;
            Ok(PickupOutcome {
                calling_name: "Caller".into(),
                calling_number: "2100".into(),
                connected_name: "Target".into(),
                connected_number: "2200".into(),
                redirecting_name: "Reception".into(),
                redirecting_number: "2000".into(),
            })
        }

        fn parking(&self, operation: &ParkingOperation) -> Result<(), Self::Error> {
            self.advanced_operations
                .lock()
                .unwrap()
                .push(AdvancedOperation::Parking(operation.clone()));
            let operation = match operation {
                ParkingOperation::Park { .. } => "backend:park",
                ParkingOperation::Retrieve { .. } => "backend:parking-retrieve",
            };
            self.record(operation)
        }
    }

    impl ManagementBackend for FakeBackend {
        fn publish_management_event(&self, event: &ManagementEvent) -> Result<(), Self::Error> {
            self.advanced_operations
                .lock()
                .unwrap()
                .push(AdvancedOperation::Management(event.clone()));
            self.record("backend:management")
        }
    }

    fn binding() -> LineBinding {
        LineBinding {
            device_id: DeviceId::new("SEP001122334455").unwrap(),
            line_instance: 1,
            appearance: sccp_protocol::LineAppearance::new(
                1,
                sccp_protocol::LineDefinition {
                    number: "1001".into(),
                    display_name: "Desk".into(),
                },
            ),
            line: LineConfig {
                number: "1001".into(),
                label: "Desk".into(),
                context: "from-sccp".into(),
                caller_name: "Desk".into(),
                caller_number: "1001".into(),
                mailbox: None,
                language: "en".into(),
                account_code: None,
                channel_variables: Vec::new(),
            },
        }
    }

    fn registration() -> DeviceRegistration {
        DeviceRegistration {
            id: binding().device_id,
            peer: "192.0.2.10:2000".parse().unwrap(),
            transport: StationTransport::Clear,
            reported_address: Some("192.0.2.10".parse().unwrap()),
            reported_ipv6_address: None,
            device_type: DeviceType::Unknown(0),
            protocol: ProtocolVersion::V22,
            firmware: "test".into(),
        }
    }

    fn connected_outbound_controller() -> Controller {
        let mut controller = Controller::new(Duration::from_secs(1));
        controller.registered(registration());
        controller.begin_phone_call(CallId(1), binding(), Codec::Pcmu, Instant::now());
        controller.enbloc(CallId(1), "2100".into());
        controller.pbx_answer(PbxCallId(1));
        controller
    }

    fn active_conference_controller() -> Controller {
        let mut controller = connected_outbound_controller();
        controller
            .begin_conference(
                CallId(1),
                CallId(2),
                binding(),
                Codec::Pcmu,
                Instant::now(),
                true,
            )
            .unwrap();
        controller.enbloc(CallId(2), "2200".into());
        controller.pbx_answer(PbxCallId(2));
        controller.confirm_conference(CallId(2)).unwrap();
        assert!(controller.conference_merged(CallId(2)));
        controller
    }

    fn fake_backend(
        events: &Arc<Mutex<Vec<&'static str>>>,
        fail: Option<&'static str>,
    ) -> FakeBackend {
        FakeBackend {
            events: Arc::clone(events),
            advanced_operations: Arc::new(Mutex::new(Vec::new())),
            capabilities: FakeCapabilities::default(),
            fail,
            controller_probe: None,
        }
    }

    fn conference_progress(effects: &[DriverEffect], completed: usize) -> ConferenceStartProgress {
        effects[..completed].iter().fold(
            ConferenceStartProgress::default(),
            |mut progress, effect| {
                progress |= effect.into();
                progress
            },
        )
    }

    fn handset_operation(effect: &HandsetEffect) -> &'static str {
        match effect {
            HandsetEffect::BeginCall { .. } => "handset:begin-call",
            HandsetEffect::SetCallState {
                state: HandsetCallState::Hold,
                ..
            } => "handset:hold",
            HandsetEffect::ShowConferenceList { .. } => "handset:conference-list",
            HandsetEffect::ShowConferenceParticipantActions { .. } => {
                "handset:conference-participant-actions"
            }
            _ => "handset:other",
        }
    }

    fn info_effect() -> HandsetEffect {
        HandsetEffect::SetCallInfo {
            device_id: binding().device_id,
            call_id: CallId(7),
            info: CallInfo {
                direction: sccp_protocol::CallDirection::Outbound,
                calling_name: "Desk".into(),
                calling_number: "1001".into(),
                called_name: String::new(),
                called_number: String::new(),
                ..CallInfo::default()
            },
        }
    }

    fn backend_with_services(harness: ServiceHarness) -> FakeBackend {
        FakeBackend {
            events: Arc::new(Mutex::new(Vec::new())),
            advanced_operations: Arc::new(Mutex::new(Vec::new())),
            capabilities: FakeCapabilities::with_harness(harness),
            fail: None,
            controller_probe: None,
        }
    }

    #[tokio::test]
    async fn fake_backend_and_handset_effects_execute_in_order() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let backend = FakeBackend {
            events: Arc::clone(&events),
            advanced_operations: Arc::new(Mutex::new(Vec::new())),
            capabilities: FakeCapabilities::default(),
            fail: None,
            controller_probe: None,
        };
        let handset_events = Arc::clone(&events);
        execute_effects(
            &backend,
            vec![
                PbxEffect::CreateChannel {
                    handset_call_id: CallId(7),
                    call_id: PbxCallId(1),
                    binding: Box::new(binding()),
                    codec: Codec::Pcmu,
                }
                .into(),
                info_effect().into(),
                PbxEffect::Answer {
                    call_id: PbxCallId(1),
                }
                .into(),
            ],
            move |_| {
                handset_events.lock().unwrap().push("handset");
                async { Ok::<_, FakeError>(()) }
            },
        )
        .await
        .unwrap();

        assert_eq!(
            *events.lock().unwrap(),
            ["backend:create", "handset", "backend:answer"]
        );
    }

    #[tokio::test]
    async fn rejected_audio_admission_emits_no_open_and_cleanup_allows_a_new_call() {
        let mut controller = Controller::new(Duration::from_secs(1));
        controller.registered(registration());
        controller.begin_phone_call(CallId(1), binding(), Codec::Pcmu, Instant::now());
        controller.enbloc(CallId(1), "2100".into());
        let effects = controller.pbx_answer(PbxCallId(1));
        assert!(matches!(
            effects.as_slice(),
            [DriverEffect::Handset(HandsetEffect::BeginMedia {
                call_id: CallId(1),
                ..
            })]
        ));

        let admission = crate::media::encryption::AudioEncryptionAdmission::new(
            crate::media::encryption::MediaEncryptionPolicy::new(
                crate::media::encryption::MediaEncryptionRequirement::Required,
                [crate::media::encryption::MediaEncryptionProfile::AES_128_HMAC_SHA1_80],
            )
            .unwrap(),
            crate::media::encryption::StationEncryptionCapabilities::NotReported,
            LocalEncryptionCapabilities::default(),
        );
        let events = Arc::new(Mutex::new(Vec::new()));
        let backend = fake_backend(&events, None);
        let handset_events = Arc::clone(&events);
        let result = execute_effects(&backend, effects, move |_| {
            let decision = admission.decide();
            let handset_events = Arc::clone(&handset_events);
            async move {
                decision.map_err(|_| FakeError("media-admission"))?;
                handset_events.lock().unwrap().push("handset:media-open");
                Ok(())
            }
        })
        .await;

        assert!(matches!(
            result,
            Err(EffectExecutionError::Handset {
                error: FakeError("media-admission"),
                ..
            })
        ));
        assert!(events.lock().unwrap().is_empty());

        let cleanup = controller
            .pbx_hangup_with_effects(PbxCallId(1))
            .expect("admitted call remains available for failure cleanup");
        assert!(controller.call(CallId(1)).is_none());
        assert!(controller.pbx_call(PbxCallId(1)).is_none());

        let cleanup_events = Arc::clone(&events);
        execute_cleanup_effects(&backend, cleanup.effects, move |_| {
            cleanup_events.lock().unwrap().push("handset:cleanup");
            async { Ok::<_, FakeError>(()) }
        })
        .await;
        assert_eq!(*events.lock().unwrap(), ["handset:cleanup"]);

        let retry = controller.begin_phone_call(CallId(2), binding(), Codec::Pcmu, Instant::now());
        assert!(!retry.is_empty());
        assert!(controller.call(CallId(2)).is_some());
    }

    #[tokio::test]
    async fn conference_consultation_executes_confirmed_begin_call_before_channel_creation() {
        let mut controller = connected_outbound_controller();
        let effects = controller
            .begin_conference(
                CallId(1),
                CallId(2),
                binding(),
                Codec::Pcmu,
                Instant::now(),
                true,
            )
            .unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let backend = fake_backend(&events, None);
        let handset_events = Arc::clone(&events);

        execute_effects(&backend, effects, move |effect| {
            handset_events
                .lock()
                .unwrap()
                .push(handset_operation(&effect));
            async { Ok::<_, FakeError>(()) }
        })
        .await
        .unwrap();

        assert_eq!(
            *events.lock().unwrap(),
            [
                "backend:hold",
                "handset:hold",
                "handset:begin-call",
                "backend:create",
            ]
        );
    }

    #[tokio::test]
    async fn conference_consultation_failure_before_begin_call_executes_exact_abort() {
        let mut controller = connected_outbound_controller();
        let effects = controller
            .begin_conference(
                CallId(1),
                CallId(2),
                binding(),
                Codec::Pcmu,
                Instant::now(),
                true,
            )
            .unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let backend = fake_backend(&events, None);
        let handset_events = Arc::clone(&events);
        let error = execute_effects(&backend, effects.clone(), move |effect| {
            let operation = handset_operation(&effect);
            handset_events.lock().unwrap().push(operation);
            async move {
                if operation == "handset:hold" {
                    Err(FakeError("handset:hold"))
                } else {
                    Ok(())
                }
            }
        })
        .await
        .unwrap_err();
        let EffectExecutionError::Handset { index, .. } = error else {
            panic!("expected handset execution error");
        };
        assert_eq!(*events.lock().unwrap(), ["backend:hold", "handset:hold"]);

        let progress = conference_progress(&effects, index);
        let cleanup = controller.abort_conference(
            CallId(2),
            false,
            progress.channel_created(),
            progress.active_leg_held(),
            progress.active_handset_held(),
        );
        let cleanup_events = Arc::new(Mutex::new(Vec::new()));
        let cleanup_backend = fake_backend(&cleanup_events, None);
        let handset_events = Arc::clone(&cleanup_events);
        let errors = execute_cleanup_effects(&cleanup_backend, cleanup, move |effect| {
            handset_events
                .lock()
                .unwrap()
                .push(handset_operation(&effect));
            async { Ok::<_, FakeError>(()) }
        })
        .await;

        assert!(errors.is_empty());
        assert_eq!(
            *cleanup_events.lock().unwrap(),
            ["backend:resume", "handset:other"]
        );
        assert!(controller.call(CallId(2)).is_none());
    }

    #[tokio::test]
    async fn conference_consultation_failure_after_begin_call_executes_exact_abort() {
        let mut controller = connected_outbound_controller();
        let effects = controller
            .begin_conference(
                CallId(1),
                CallId(2),
                binding(),
                Codec::Pcmu,
                Instant::now(),
                true,
            )
            .unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let backend = fake_backend(&events, Some("backend:create"));
        let handset_events = Arc::clone(&events);
        let error = execute_effects(&backend, effects.clone(), move |effect| {
            handset_events
                .lock()
                .unwrap()
                .push(handset_operation(&effect));
            async { Ok::<_, FakeError>(()) }
        })
        .await
        .unwrap_err();
        let EffectExecutionError::Backend { index, .. } = error else {
            panic!("expected backend execution error");
        };
        assert_eq!(
            *events.lock().unwrap(),
            [
                "backend:hold",
                "handset:hold",
                "handset:begin-call",
                "backend:create",
            ]
        );

        let progress = conference_progress(&effects, index);
        let cleanup = controller.abort_conference(
            CallId(2),
            false,
            progress.channel_created(),
            progress.active_leg_held(),
            progress.active_handset_held(),
        );
        let cleanup_events = Arc::new(Mutex::new(Vec::new()));
        let cleanup_backend = fake_backend(&cleanup_events, None);
        let handset_events = Arc::clone(&cleanup_events);
        let errors = execute_cleanup_effects(&cleanup_backend, cleanup, move |effect| {
            handset_events
                .lock()
                .unwrap()
                .push(handset_operation(&effect));
            async { Ok::<_, FakeError>(()) }
        })
        .await;

        assert!(errors.is_empty());
        assert_eq!(
            *cleanup_events.lock().unwrap(),
            ["backend:resume", "handset:other", "handset:other"]
        );
        assert!(controller.call(CallId(2)).is_none());
    }

    #[tokio::test]
    async fn conference_invite_failures_before_and_after_begin_call_preserve_the_conference() {
        for (handset_failure, backend_failure, expected_events) in [
            (
                Some("handset:hold"),
                None,
                vec!["backend:hold", "handset:hold"],
            ),
            (
                None,
                Some("backend:create"),
                vec![
                    "backend:hold",
                    "handset:hold",
                    "handset:begin-call",
                    "backend:create",
                ],
            ),
        ] {
            let mut controller = active_conference_controller();
            let conference_id = controller.conference_session(CallId(1)).unwrap().id;
            let effects = controller
                .begin_conference_invite(
                    CallId(1),
                    CallId(3),
                    binding(),
                    Codec::Pcmu,
                    Instant::now(),
                )
                .unwrap();
            let events = Arc::new(Mutex::new(Vec::new()));
            let backend = fake_backend(&events, backend_failure);
            let handset_events = Arc::clone(&events);
            let error = execute_effects(&backend, effects.clone(), move |effect| {
                let operation = handset_operation(&effect);
                handset_events.lock().unwrap().push(operation);
                async move {
                    if handset_failure == Some(operation) {
                        Err(FakeError(operation))
                    } else {
                        Ok(())
                    }
                }
            })
            .await
            .unwrap_err();
            let index = match error {
                EffectExecutionError::Backend { index, .. }
                | EffectExecutionError::Handset { index, .. } => index,
            };
            assert_eq!(*events.lock().unwrap(), expected_events);

            let progress = conference_progress(&effects, index);
            let cleanup = controller.abort_conference_invite(
                CallId(3),
                progress.channel_created(),
                progress.active_leg_held(),
                progress.active_handset_held(),
            );
            let cleanup_backend = fake_backend(&Arc::new(Mutex::new(Vec::new())), None);
            let errors = execute_cleanup_effects(&cleanup_backend, cleanup, |_| async {
                Ok::<_, FakeError>(())
            })
            .await;

            assert!(errors.is_empty());
            assert!(controller.call(CallId(3)).is_none());
            let session = controller.conference_session(CallId(1)).unwrap();
            assert_eq!(session.id, conference_id);
            assert!(session.pending_invite.is_none());
            assert_eq!(session.participants.iter().len(), 2);
        }
    }

    #[tokio::test]
    async fn conference_invite_and_ui_use_the_confirmed_effect_boundary() {
        let mut controller = active_conference_controller();
        let invite_effects = controller
            .begin_conference_invite(CallId(1), CallId(3), binding(), Codec::Pcmu, Instant::now())
            .unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let backend = fake_backend(&events, None);
        let handset_events = Arc::clone(&events);
        execute_effects(&backend, invite_effects, move |effect| {
            handset_events
                .lock()
                .unwrap()
                .push(handset_operation(&effect));
            async { Ok::<_, FakeError>(()) }
        })
        .await
        .unwrap();
        assert_eq!(
            *events.lock().unwrap(),
            [
                "backend:hold",
                "handset:hold",
                "handset:begin-call",
                "backend:create",
            ]
        );

        controller.abort_conference_invite(CallId(3), true, true, true);
        let session = controller.conference_session(CallId(1)).unwrap();
        let participant_id = session
            .participants
            .iter()
            .find(|participant| !participant.moderator)
            .unwrap()
            .id;
        let ui_effects = vec![
            session.list_effect(CallId(1)).into(),
            session
                .participant_actions_effect(participant_id)
                .unwrap()
                .into(),
            PbxEffect::Answer {
                call_id: PbxCallId(1),
            }
            .into(),
        ];
        let ui_events = Arc::new(Mutex::new(Vec::new()));
        let backend = fake_backend(&ui_events, None);
        let handset_events = Arc::clone(&ui_events);
        let error = execute_effects(&backend, ui_effects, move |effect| {
            let operation = handset_operation(&effect);
            handset_events.lock().unwrap().push(operation);
            async move {
                if operation == "handset:conference-participant-actions" {
                    Err(FakeError("handset:conference-participant-actions"))
                } else {
                    Ok(())
                }
            }
        })
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            EffectExecutionError::Handset { index: 1, .. }
        ));
        assert_eq!(
            *ui_events.lock().unwrap(),
            [
                "handset:conference-list",
                "handset:conference-participant-actions",
            ]
        );
    }

    #[tokio::test]
    async fn typed_transfer_reaches_backend_once_in_source_target_order() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let operations = Arc::new(Mutex::new(Vec::new()));
        let backend = FakeBackend {
            events: Arc::clone(&events),
            advanced_operations: Arc::clone(&operations),
            capabilities: FakeCapabilities::default(),
            fail: None,
            controller_probe: None,
        };
        let operation = TransferCompletion {
            transaction_id: TransferId(7),
            device_id: DeviceId::new("SEP001122334455").unwrap(),
            source: TransferLeg {
                handset_call_id: CallId(10),
                pbx_call_id: PbxCallId(100),
            },
            consultation: TransferLeg {
                handset_call_id: CallId(20),
                pbx_call_id: PbxCallId(200),
            },
            kind: TransferCompletionKind::Attended,
        };
        execute_effects(
            &backend,
            vec![
                PbxEffect::Transfer {
                    operation: operation.clone(),
                }
                .into(),
            ],
            |_| async { Ok::<_, FakeError>(()) },
        )
        .await
        .unwrap();

        assert_eq!(*events.lock().unwrap(), ["backend:bridge-transfer"]);
        assert_eq!(
            *operations.lock().unwrap(),
            [AdvancedOperation::Transfer(operation)]
        );
    }

    #[tokio::test]
    async fn typed_transfer_failure_retains_transaction_identity_and_stops_handset_work() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let backend = FakeBackend {
            events: Arc::clone(&events),
            advanced_operations: Arc::new(Mutex::new(Vec::new())),
            capabilities: FakeCapabilities::default(),
            fail: Some("backend:bridge-transfer"),
            controller_probe: None,
        };
        let operation = TransferCompletion {
            transaction_id: TransferId(9),
            device_id: DeviceId::new("SEP001122334455").unwrap(),
            source: TransferLeg {
                handset_call_id: CallId(10),
                pbx_call_id: PbxCallId(100),
            },
            consultation: TransferLeg {
                handset_call_id: CallId(20),
                pbx_call_id: PbxCallId(200),
            },
            kind: TransferCompletionKind::Blind,
        };
        let error = execute_effects(
            &backend,
            vec![
                PbxEffect::Transfer {
                    operation: operation.clone(),
                }
                .into(),
                info_effect().into(),
            ],
            |_| async { Err(FakeError("unexpected handset work")) },
        )
        .await
        .unwrap_err();

        assert_eq!(*events.lock().unwrap(), ["backend:bridge-transfer"]);
        assert!(matches!(
            error,
            EffectExecutionError::Backend {
                index: 0,
                effect,
                error: FakeError("backend:bridge-transfer"),
            } if *effect == (PbxEffect::Transfer { operation })
        ));
    }

    #[tokio::test]
    async fn backend_error_stops_later_effects_and_reports_position() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let backend = FakeBackend {
            events: Arc::clone(&events),
            advanced_operations: Arc::new(Mutex::new(Vec::new())),
            capabilities: FakeCapabilities::default(),
            fail: Some("backend:answer"),
            controller_probe: None,
        };
        let handset_events = Arc::clone(&events);
        let error = execute_effects(
            &backend,
            vec![
                info_effect().into(),
                PbxEffect::Answer {
                    call_id: PbxCallId(1),
                }
                .into(),
                info_effect().into(),
            ],
            move |_| {
                handset_events.lock().unwrap().push("handset");
                async { Ok::<_, FakeError>(()) }
            },
        )
        .await
        .unwrap_err();

        let EffectExecutionError::Backend {
            index,
            effect,
            error,
        } = error
        else {
            panic!("expected backend execution error");
        };
        assert_eq!(index, 1);
        assert!(matches!(*effect, PbxEffect::Answer { .. }));
        assert_eq!(error, FakeError("backend:answer"));
        assert_eq!(*events.lock().unwrap(), ["handset", "backend:answer"]);
    }

    #[tokio::test]
    async fn terminal_cleanup_attempts_every_backend_and_handset_effect_after_failures() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let backend = FakeBackend {
            events: Arc::clone(&events),
            advanced_operations: Arc::new(Mutex::new(Vec::new())),
            capabilities: FakeCapabilities::default(),
            fail: Some("backend:bridge-destroy"),
            controller_probe: None,
        };
        let handset_events = Arc::clone(&events);
        let errors = execute_cleanup_effects(
            &backend,
            vec![
                PbxEffect::Bridge {
                    operation: BridgeOperation::Destroy {
                        bridge_id: PbxBridgeId(9),
                    },
                }
                .into(),
                PbxEffect::Hangup {
                    call_id: PbxCallId(1),
                }
                .into(),
                info_effect().into(),
                PbxEffect::Answer {
                    call_id: PbxCallId(2),
                }
                .into(),
            ],
            move |_| {
                handset_events.lock().unwrap().push("handset");
                async { Err::<(), _>(FakeError("handset")) }
            },
        )
        .await;

        assert_eq!(
            *events.lock().unwrap(),
            [
                "backend:bridge-destroy",
                "backend:hangup",
                "handset",
                "backend:answer",
            ]
        );
        assert_eq!(errors.len(), 2);
        assert!(matches!(
            &errors[0],
            EffectExecutionError::Backend { index: 0, .. }
        ));
        assert!(matches!(
            &errors[1],
            EffectExecutionError::Handset { index: 2, .. }
        ));
    }

    #[tokio::test]
    async fn handset_error_is_propagated_with_the_effect_position() {
        let backend = FakeBackend {
            events: Arc::new(Mutex::new(Vec::new())),
            advanced_operations: Arc::new(Mutex::new(Vec::new())),
            capabilities: FakeCapabilities::default(),
            fail: None,
            controller_probe: None,
        };
        let error = execute_effects(&backend, vec![info_effect().into()], |_| async {
            Err::<(), _>(FakeError("handset"))
        })
        .await
        .unwrap_err();

        let EffectExecutionError::Handset {
            index,
            effect,
            error,
        } = error
        else {
            panic!("expected handset execution error");
        };
        assert_eq!(index, 0);
        assert!(matches!(*effect, HandsetEffect::SetCallInfo { .. }));
        assert_eq!(error, FakeError("handset"));
    }

    #[tokio::test]
    async fn backend_media_result_is_delivered_before_the_next_effect() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let backend = FakeBackend {
            events: Arc::clone(&events),
            advanced_operations: Arc::new(Mutex::new(Vec::new())),
            capabilities: FakeCapabilities::default(),
            fail: None,
            controller_probe: None,
        };
        let handset_events = Arc::clone(&events);
        let endpoint = MediaEndpoint {
            address: "192.0.2.20".parse().unwrap(),
            rtp_port: 20_000,
            rtcp_port: 20_001,
            codec: Codec::Pcmu,
            packet_ms: 20,
            max_frames_per_packet: 1,
            telephone_event_payload: 101,
        };
        execute_effects(
            &backend,
            vec![
                PbxEffect::ConfigureMedia {
                    call_id: PbxCallId(1),
                    device_id: binding().device_id,
                    handset_call_id: CallId(7),
                    codec: Codec::Pcmu,
                    remote: endpoint,
                }
                .into(),
                PbxEffect::Answer {
                    call_id: PbxCallId(1),
                }
                .into(),
            ],
            move |effect| {
                assert!(matches!(effect, HandsetEffect::StartMedia { .. }));
                handset_events.lock().unwrap().push("handset:media");
                async { Ok::<_, FakeError>(()) }
            },
        )
        .await
        .unwrap();

        assert_eq!(
            *events.lock().unwrap(),
            ["backend:media", "handset:media", "backend:answer"]
        );
    }

    #[tokio::test]
    async fn coupled_media_configuration_never_sends_a_second_transmit_request() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let backend = FakeBackend {
            events: Arc::clone(&events),
            advanced_operations: Arc::new(Mutex::new(Vec::new())),
            capabilities: FakeCapabilities::default(),
            fail: None,
            controller_probe: None,
        };
        let endpoint = MediaEndpoint {
            address: "192.0.2.20".parse().unwrap(),
            rtp_port: 20_000,
            rtcp_port: 20_001,
            codec: Codec::Pcmu,
            packet_ms: 20,
            max_frames_per_packet: 1,
            telephone_event_payload: 101,
        };
        let handset_events = Arc::clone(&events);

        execute_effects(
            &backend,
            vec![
                PbxEffect::ConfigureMediaOnly {
                    call_id: PbxCallId(1),
                    codec: Codec::Pcmu,
                    remote: endpoint,
                }
                .into(),
            ],
            move |_| {
                handset_events.lock().unwrap().push("handset:unexpected");
                async { Ok::<(), FakeError>(()) }
            },
        )
        .await
        .unwrap();

        assert_eq!(*events.lock().unwrap(), ["backend:media"]);
    }

    #[tokio::test]
    async fn pickup_result_is_delivered_with_parties_before_the_next_effect() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let handset_effects = Arc::new(Mutex::new(Vec::new()));
        let backend = FakeBackend {
            events: Arc::clone(&events),
            advanced_operations: Arc::new(Mutex::new(Vec::new())),
            capabilities: FakeCapabilities::default(),
            fail: None,
            controller_probe: None,
        };
        let received = Arc::clone(&handset_effects);
        let handset_events = Arc::clone(&events);
        execute_effects(
            &backend,
            vec![
                PbxEffect::Pickup {
                    operation: PickupOperation::Directed {
                        call_id: PbxCallId(2),
                        device_id: binding().device_id,
                        handset_call_id: CallId(20),
                        codec: Codec::Pcma,
                        extension: "2100".into(),
                        context: "from-phones".into(),
                        answer: false,
                    },
                }
                .into(),
                PbxEffect::Answer {
                    call_id: PbxCallId(2),
                }
                .into(),
            ],
            move |effect| {
                handset_events.lock().unwrap().push("handset:pickup");
                received.lock().unwrap().push(effect);
                async { Ok::<_, FakeError>(()) }
            },
        )
        .await
        .unwrap();

        assert_eq!(
            *events.lock().unwrap(),
            [
                "backend:pickup-directed",
                "handset:pickup",
                "backend:answer"
            ]
        );
        assert_eq!(
            *handset_effects.lock().unwrap(),
            [HandsetEffect::PickupCompleted {
                device_id: DeviceId::new("SEP001122334455").unwrap(),
                call_id: CallId(20),
                codec: Codec::Pcma,
                answer: false,
                parties: PickupOutcome {
                    calling_name: "Caller".into(),
                    calling_number: "2100".into(),
                    connected_name: "Target".into(),
                    connected_number: "2200".into(),
                    redirecting_name: "Reception".into(),
                    redirecting_number: "2000".into(),
                },
            }]
        );
    }

    #[tokio::test]
    async fn advanced_operations_dispatch_typed_payloads_in_order() {
        let bridge = PbxBridgeId(9);
        let operations = vec![
            AdvancedOperation::Forward(ForwardingOperation {
                call_id: PbxCallId(7),
                context: ForwardingContext::new("from-sccp").unwrap(),
                destination: ForwardingDestination::new("private-2000").unwrap(),
                reason: ForwardingRouteReason::Busy,
            }),
            AdvancedOperation::Voicemail(VoicemailOperation {
                transaction_id: VoicemailTransactionId(3),
                device_id: DeviceId::new("SEP001122334455").unwrap(),
                handset_call_id: CallId(8),
                pbx_call_id: PbxCallId(8),
                action: VoicemailAction::ImmediateDivert,
                target: VoicemailTarget::new("from-sccp", "private-voicemail").unwrap(),
            }),
            AdvancedOperation::ConferenceDestination(ConferenceDestinationOperation {
                call_id: PbxCallId(8),
                destination: "700".into(),
                application_options: "Mac".into(),
                handset_call_id: CallId(8),
                held_calls: Vec::new(),
                mutation: ConferenceMutationToken::for_test(PbxCallId(8)),
            }),
            AdvancedOperation::Bridge(BridgeOperation::Create { bridge_id: bridge }),
            AdvancedOperation::Bridge(BridgeOperation::AddParticipant {
                bridge_id: bridge,
                call_id: PbxCallId(1),
            }),
            AdvancedOperation::Bridge(BridgeOperation::MergeConsultation {
                bridge_id: bridge,
                original_call_id: PbxCallId(1),
                consultation_call_id: PbxCallId(2),
            }),
            AdvancedOperation::Bridge(BridgeOperation::MergeCalls {
                bridge_id: bridge,
                call_ids: vec![PbxCallId(1), PbxCallId(2), PbxCallId(3)],
            }),
            AdvancedOperation::Bridge(BridgeOperation::MergeParticipant {
                bridge_id: bridge,
                call_id: PbxCallId(4),
            }),
            AdvancedOperation::Bridge(BridgeOperation::SetParticipantMuted {
                bridge_id: bridge,
                participant_id: ParticipantId::new(7),
                call_id: PbxCallId(4),
                muted: true,
            }),
            AdvancedOperation::Bridge(BridgeOperation::SetParticipantMuted {
                bridge_id: bridge,
                participant_id: ParticipantId::new(7),
                call_id: PbxCallId(4),
                muted: false,
            }),
            AdvancedOperation::Bridge(BridgeOperation::RemoveConferenceParticipant {
                bridge_id: bridge,
                participant_id: ParticipantId::new(7),
                call_id: PbxCallId(4),
            }),
            AdvancedOperation::Bridge(BridgeOperation::SetParticipantMusicOnHold {
                bridge_id: bridge,
                participant_id: ParticipantId::new(7),
                call_id: PbxCallId(4),
                class: "office".into(),
                enabled: true,
            }),
            AdvancedOperation::Bridge(BridgeOperation::SetParticipantMusicOnHold {
                bridge_id: bridge,
                participant_id: ParticipantId::new(7),
                call_id: PbxCallId(4),
                class: "office".into(),
                enabled: false,
            }),
            AdvancedOperation::Barge(BargeOperation::Join {
                bridge_id: PbxBridgeId(10),
                target_call_id: PbxCallId(1),
                barger_call_id: PbxCallId(6),
            }),
            AdvancedOperation::Pickup(PickupOperation::Group {
                call_id: PbxCallId(2),
                device_id: DeviceId::new("SEP001122334455").unwrap(),
                handset_call_id: CallId(20),
                codec: Codec::Pcmu,
                answer: true,
            }),
            AdvancedOperation::Pickup(PickupOperation::Directed {
                call_id: PbxCallId(3),
                device_id: DeviceId::new("SEP001122334455").unwrap(),
                handset_call_id: CallId(30),
                codec: Codec::Pcma,
                extension: "2100".into(),
                context: "from-phones".into(),
                answer: false,
            }),
            AdvancedOperation::Parking(ParkingOperation::Park {
                call_id: PbxCallId(4),
                lot: Some("executive".into()),
            }),
            AdvancedOperation::Parking(ParkingOperation::Retrieve {
                call_id: PbxCallId(5),
                lot: None,
                slot: "701".into(),
            }),
            AdvancedOperation::Management(ManagementEvent {
                kind: ManagementEventKind::Call,
                fields: vec![ManagementField::new("CallId", 5_u64)],
            }),
            AdvancedOperation::Bridge(BridgeOperation::RemoveParticipant {
                bridge_id: bridge,
                call_id: PbxCallId(1),
            }),
            AdvancedOperation::Bridge(BridgeOperation::Destroy { bridge_id: bridge }),
            AdvancedOperation::Barge(BargeOperation::Leave {
                bridge_id: PbxBridgeId(10),
                barger_call_id: PbxCallId(6),
                last_participant: true,
            }),
            AdvancedOperation::Announcement(ConferenceAnnouncementOperation {
                conference_id: ConferenceId::new(11),
                targets: vec![ConferenceAnnouncementTarget {
                    participant_id: ParticipantId::new(1),
                    call_id: PbxCallId(1),
                }],
                announcement: ConferenceAnnouncement::Connected,
            }),
        ];
        let effects = operations
            .iter()
            .cloned()
            .map(|operation| match operation {
                AdvancedOperation::ConferenceDestination(operation) => {
                    PbxEffect::StartConferenceDestination { operation }.into()
                }
                AdvancedOperation::Forward(operation) => PbxEffect::Forward { operation }.into(),
                AdvancedOperation::Voicemail(operation) => {
                    PbxEffect::Voicemail { operation }.into()
                }
                AdvancedOperation::Transfer(operation) => PbxEffect::Transfer { operation }.into(),
                AdvancedOperation::Bridge(operation) => PbxEffect::Bridge { operation }.into(),
                AdvancedOperation::Barge(operation) => PbxEffect::Barge { operation }.into(),
                AdvancedOperation::Pickup(operation) => PbxEffect::Pickup { operation }.into(),
                AdvancedOperation::Parking(operation) => PbxEffect::Parking { operation }.into(),
                AdvancedOperation::Announcement(operation) => {
                    PbxEffect::ConferenceAnnouncement { operation }.into()
                }
                AdvancedOperation::Management(event) => {
                    PbxEffect::PublishManagementEvent { event }.into()
                }
            })
            .collect();
        let events = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let backend = FakeBackend {
            events: Arc::clone(&events),
            advanced_operations: Arc::clone(&recorded),
            capabilities: FakeCapabilities::default(),
            fail: None,
            controller_probe: None,
        };

        execute_effects(&backend, effects, |_| async { Ok::<_, FakeError>(()) })
            .await
            .unwrap();

        assert_eq!(*recorded.lock().unwrap(), operations);
        assert_eq!(
            *events.lock().unwrap(),
            [
                "backend:forward",
                "backend:voicemail",
                "backend:conference-destination",
                "backend:bridge-create",
                "backend:bridge-add",
                "backend:bridge-merge-consultation",
                "backend:bridge-merge-calls",
                "backend:bridge-merge-participant",
                "backend:bridge-mute-participant",
                "backend:bridge-unmute-participant",
                "backend:bridge-remove-conference-participant",
                "backend:bridge-start-music",
                "backend:bridge-stop-music",
                "backend:barge-join",
                "backend:pickup-group",
                "backend:pickup-directed",
                "backend:park",
                "backend:parking-retrieve",
                "backend:management",
                "backend:bridge-remove",
                "backend:bridge-destroy",
                "backend:barge-leave",
                "backend:conference-announcement",
            ]
        );
    }

    #[tokio::test]
    async fn every_advanced_effect_propagates_errors_and_stops_the_queue() {
        let cases = [
            (
                "backend:forward",
                PbxEffect::Forward {
                    operation: ForwardingOperation {
                        call_id: PbxCallId(7),
                        context: ForwardingContext::new("from-sccp").unwrap(),
                        destination: ForwardingDestination::new("private-2000").unwrap(),
                        reason: ForwardingRouteReason::NoAnswer,
                    },
                },
            ),
            (
                "backend:voicemail",
                PbxEffect::Voicemail {
                    operation: VoicemailOperation {
                        transaction_id: VoicemailTransactionId(4),
                        device_id: DeviceId::new("SEP001122334455").unwrap(),
                        handset_call_id: CallId(8),
                        pbx_call_id: PbxCallId(8),
                        action: VoicemailAction::TransferSelected,
                        target: VoicemailTarget::new("from-sccp", "private-voicemail").unwrap(),
                    },
                },
            ),
            (
                "backend:conference-destination",
                PbxEffect::StartConferenceDestination {
                    operation: ConferenceDestinationOperation {
                        call_id: PbxCallId(8),
                        destination: "700".into(),
                        application_options: "Mac".into(),
                        handset_call_id: CallId(8),
                        held_calls: Vec::new(),
                        mutation: ConferenceMutationToken::for_test(PbxCallId(8)),
                    },
                },
            ),
            (
                "backend:bridge-create",
                PbxEffect::Bridge {
                    operation: BridgeOperation::Create {
                        bridge_id: PbxBridgeId(9),
                    },
                },
            ),
            (
                "backend:bridge-add",
                PbxEffect::Bridge {
                    operation: BridgeOperation::AddParticipant {
                        bridge_id: PbxBridgeId(9),
                        call_id: PbxCallId(1),
                    },
                },
            ),
            (
                "backend:bridge-merge-consultation",
                PbxEffect::Bridge {
                    operation: BridgeOperation::MergeConsultation {
                        bridge_id: PbxBridgeId(9),
                        original_call_id: PbxCallId(1),
                        consultation_call_id: PbxCallId(2),
                    },
                },
            ),
            (
                "backend:bridge-merge-calls",
                PbxEffect::Bridge {
                    operation: BridgeOperation::MergeCalls {
                        bridge_id: PbxBridgeId(9),
                        call_ids: vec![PbxCallId(1), PbxCallId(2)],
                    },
                },
            ),
            (
                "backend:bridge-merge-participant",
                PbxEffect::Bridge {
                    operation: BridgeOperation::MergeParticipant {
                        bridge_id: PbxBridgeId(9),
                        call_id: PbxCallId(3),
                    },
                },
            ),
            (
                "backend:bridge-mute-participant",
                PbxEffect::Bridge {
                    operation: BridgeOperation::SetParticipantMuted {
                        bridge_id: PbxBridgeId(9),
                        participant_id: ParticipantId::new(7),
                        call_id: PbxCallId(3),
                        muted: true,
                    },
                },
            ),
            (
                "backend:bridge-unmute-participant",
                PbxEffect::Bridge {
                    operation: BridgeOperation::SetParticipantMuted {
                        bridge_id: PbxBridgeId(9),
                        participant_id: ParticipantId::new(7),
                        call_id: PbxCallId(3),
                        muted: false,
                    },
                },
            ),
            (
                "backend:bridge-remove-conference-participant",
                PbxEffect::Bridge {
                    operation: BridgeOperation::RemoveConferenceParticipant {
                        bridge_id: PbxBridgeId(9),
                        participant_id: ParticipantId::new(7),
                        call_id: PbxCallId(3),
                    },
                },
            ),
            (
                "backend:bridge-start-music",
                PbxEffect::Bridge {
                    operation: BridgeOperation::SetParticipantMusicOnHold {
                        bridge_id: PbxBridgeId(9),
                        participant_id: ParticipantId::new(7),
                        call_id: PbxCallId(3),
                        class: "office".into(),
                        enabled: true,
                    },
                },
            ),
            (
                "backend:bridge-stop-music",
                PbxEffect::Bridge {
                    operation: BridgeOperation::SetParticipantMusicOnHold {
                        bridge_id: PbxBridgeId(9),
                        participant_id: ParticipantId::new(7),
                        call_id: PbxCallId(3),
                        class: "office".into(),
                        enabled: false,
                    },
                },
            ),
            (
                "backend:barge-join",
                PbxEffect::Barge {
                    operation: BargeOperation::Join {
                        bridge_id: PbxBridgeId(10),
                        target_call_id: PbxCallId(1),
                        barger_call_id: PbxCallId(6),
                    },
                },
            ),
            (
                "backend:barge-leave",
                PbxEffect::Barge {
                    operation: BargeOperation::Leave {
                        bridge_id: PbxBridgeId(10),
                        barger_call_id: PbxCallId(6),
                        last_participant: true,
                    },
                },
            ),
            (
                "backend:bridge-remove",
                PbxEffect::Bridge {
                    operation: BridgeOperation::RemoveParticipant {
                        bridge_id: PbxBridgeId(9),
                        call_id: PbxCallId(1),
                    },
                },
            ),
            (
                "backend:bridge-destroy",
                PbxEffect::Bridge {
                    operation: BridgeOperation::Destroy {
                        bridge_id: PbxBridgeId(9),
                    },
                },
            ),
            (
                "backend:pickup-group",
                PbxEffect::Pickup {
                    operation: PickupOperation::Group {
                        call_id: PbxCallId(2),
                        device_id: DeviceId::new("SEP001122334455").unwrap(),
                        handset_call_id: CallId(20),
                        codec: Codec::Pcmu,
                        answer: false,
                    },
                },
            ),
            (
                "backend:pickup-directed",
                PbxEffect::Pickup {
                    operation: PickupOperation::Directed {
                        call_id: PbxCallId(2),
                        device_id: DeviceId::new("SEP001122334455").unwrap(),
                        handset_call_id: CallId(20),
                        codec: Codec::Pcmu,
                        extension: "2100".into(),
                        context: "from-phones".into(),
                        answer: true,
                    },
                },
            ),
            (
                "backend:park",
                PbxEffect::Parking {
                    operation: ParkingOperation::Park {
                        call_id: PbxCallId(3),
                        lot: None,
                    },
                },
            ),
            (
                "backend:parking-retrieve",
                PbxEffect::Parking {
                    operation: ParkingOperation::Retrieve {
                        call_id: PbxCallId(3),
                        lot: Some("executive".into()),
                        slot: "701".into(),
                    },
                },
            ),
            (
                "backend:management",
                PbxEffect::PublishManagementEvent {
                    event: ManagementEvent {
                        kind: ManagementEventKind::Alarm,
                        fields: vec![ManagementField::new("Text", "warning")],
                    },
                },
            ),
            (
                "backend:conference-announcement",
                PbxEffect::ConferenceAnnouncement {
                    operation: ConferenceAnnouncementOperation {
                        conference_id: ConferenceId::new(11),
                        targets: vec![ConferenceAnnouncementTarget {
                            participant_id: ParticipantId::new(1),
                            call_id: PbxCallId(3),
                        }],
                        announcement: ConferenceAnnouncement::Connected,
                    },
                },
            ),
        ];

        for (failure, effect) in cases {
            let events = Arc::new(Mutex::new(Vec::new()));
            let backend = FakeBackend {
                events: Arc::clone(&events),
                advanced_operations: Arc::new(Mutex::new(Vec::new())),
                capabilities: FakeCapabilities::default(),
                fail: Some(failure),
                controller_probe: None,
            };
            let error = execute_effects(
                &backend,
                vec![effect.clone().into(), info_effect().into()],
                |_| async { Ok::<_, FakeError>(()) },
            )
            .await
            .unwrap_err();
            let EffectExecutionError::Backend {
                index,
                effect: failed_effect,
                error,
            } = error
            else {
                panic!("expected backend execution error");
            };
            assert_eq!(index, 0);
            assert_eq!(*failed_effect, effect);
            assert_eq!(error, FakeError(failure));
            assert_eq!(*events.lock().unwrap(), [failure]);
        }
    }

    #[test]
    fn direct_capabilities_preserve_typed_requests_callbacks_and_sessions() {
        let harness = ServiceHarness::default();
        let backend = backend_with_services(harness.clone());
        let hint_target = HintTarget::parse("1001@internal").unwrap();

        assert_eq!(
            backend.persistence().get("driver", "device/dnd").unwrap(),
            Some("stored".into())
        );
        backend
            .persistence()
            .put("driver", "device/dnd", "silent")
            .unwrap();
        backend
            .persistence()
            .delete("driver", "device/dnd")
            .unwrap();

        assert_eq!(
            backend.hints().lookup(&hint_target).unwrap(),
            Some(HintSnapshot {
                target: hint_target.clone(),
                state: ExtensionState::IDLE,
                reason: HintUpdateReason::Device,
                caller: None,
            })
        );
        let hint_updates = Arc::new(Mutex::new(Vec::new()));
        let callback_updates = Arc::clone(&hint_updates);
        let _subscription = backend
            .hints()
            .subscribe(
                &hint_target,
                Arc::new(move |update| callback_updates.lock().unwrap().push(update)),
            )
            .unwrap();
        assert!(matches!(
            hint_updates.lock().unwrap().as_slice(),
            [HintSnapshot {
                state: ExtensionState::RINGING,
                ..
            }]
        ));

        let recording_events = Arc::new(Mutex::new(Vec::new()));
        let callback_events = Arc::clone(&recording_events);
        let mut recording = backend
            .recordings()
            .start_recording(
                PbxCallId(7),
                "call.wav",
                "b",
                Arc::new(move |event| callback_events.lock().unwrap().push(event)),
            )
            .unwrap();
        assert_eq!(recording.id().unwrap(), "recording-1");
        assert_eq!(recording.state().unwrap(), RecordingState::Active);
        assert_eq!(
            recording.set_muted(RecordingDirection::Both, true).unwrap(),
            1
        );
        assert_eq!(recording.state().unwrap(), RecordingState::Muted);
        recording.stop().unwrap();
        assert_eq!(recording.state().unwrap(), RecordingState::Stopped);
        assert_eq!(*recording_events.lock().unwrap(), [RecordingEvent::Started]);

        assert_eq!(
            *harness.requests.lock().unwrap(),
            [
                ServiceRequest::Get("driver".into(), "device/dnd".into()),
                ServiceRequest::Put("driver".into(), "device/dnd".into(), "silent".into(),),
                ServiceRequest::Delete("driver".into(), "device/dnd".into()),
                ServiceRequest::HintLookup("internal".into(), "1001".into()),
                ServiceRequest::HintSubscribe("internal".into(), "1001".into()),
                ServiceRequest::RecordingStart(PbxCallId(7), "call.wav".into(), "b".into()),
                ServiceRequest::RecordingId,
                ServiceRequest::RecordingState,
                ServiceRequest::RecordingMute(RecordingDirection::Both, true),
                ServiceRequest::RecordingState,
                ServiceRequest::RecordingStop,
                ServiceRequest::RecordingState,
            ]
        );
    }

    #[test]
    fn every_direct_capability_propagates_its_backend_error() {
        let harness = ServiceHarness::default();
        let backend = backend_with_services(harness.clone());
        let hint_target = HintTarget::parse("1001@internal").unwrap();

        harness.fail("persistence:get");
        assert!(matches!(
            backend.persistence().get("driver", "key"),
            Err(PersistenceError::Backend { operation: "get" })
        ));
        harness.fail("persistence:put");
        assert!(matches!(
            backend.persistence().put("driver", "key", "value"),
            Err(PersistenceError::Backend { operation: "put" })
        ));
        harness.fail("persistence:delete");
        assert!(matches!(
            backend.persistence().delete("driver", "key"),
            Err(PersistenceError::Backend {
                operation: "delete"
            })
        ));

        harness.fail("hints:lookup");
        assert_eq!(
            backend.hints().lookup(&hint_target).unwrap_err(),
            FakeError("hints:lookup")
        );
        harness.fail("hints:subscribe");
        assert_eq!(
            backend
                .hints()
                .subscribe(&hint_target, Arc::new(|_| {}))
                .err(),
            Some(FakeError("hints:subscribe"))
        );

        harness.fail("recording:start");
        assert_eq!(
            backend
                .recordings()
                .start_recording(PbxCallId(7), "call.wav", "", Arc::new(|_| {}))
                .err(),
            Some(FakeError("recording:start"))
        );

        let session_harness = ServiceHarness::default();
        let session_backend = backend_with_services(session_harness.clone());
        let mut recording = session_backend
            .recordings()
            .start_recording(PbxCallId(8), "call.wav", "", Arc::new(|_| {}))
            .unwrap();
        session_harness.fail("recording:id");
        assert_eq!(recording.id().unwrap_err(), FakeError("recording:id"));
        session_harness.fail("recording:state");
        assert_eq!(recording.state().unwrap_err(), FakeError("recording:state"));
        session_harness.fail("recording:mute");
        assert_eq!(
            recording
                .set_muted(RecordingDirection::Read, true)
                .unwrap_err(),
            FakeError("recording:mute")
        );
        session_harness.fail("recording:stop");
        assert_eq!(recording.stop().unwrap_err(), FakeError("recording:stop"));
    }

    #[test]
    fn direct_capabilities_run_after_controller_locks_are_released() {
        let controller = Arc::new(Mutex::new(Controller::new(Duration::from_secs(1))));
        let harness = ServiceHarness {
            controller_probe: Some(Arc::clone(&controller)),
            ..ServiceHarness::default()
        };
        let backend = backend_with_services(harness);
        let hint_target = HintTarget::parse("1001@internal").unwrap();
        let effects = {
            controller.lock().unwrap().begin_phone_call(
                CallId(7),
                binding(),
                Codec::Pcmu,
                Instant::now(),
            )
        };
        assert!(!effects.is_empty());

        backend.persistence().get("driver", "key").unwrap();
        backend.hints().lookup(&hint_target).unwrap();
        let hint_controller = Arc::clone(&controller);
        backend
            .hints()
            .subscribe(
                &hint_target,
                Arc::new(move |_| {
                    assert!(
                        hint_controller.try_lock().is_ok(),
                        "hint callback entered while the controller was locked"
                    );
                }),
            )
            .unwrap();
        let recording_controller = controller;
        backend
            .recordings()
            .start_recording(
                PbxCallId(1),
                "call.wav",
                "",
                Arc::new(move |_| {
                    assert!(
                        recording_controller.try_lock().is_ok(),
                        "recording callback entered while the controller was locked"
                    );
                }),
            )
            .unwrap();
    }

    #[tokio::test]
    async fn controller_lock_is_released_before_backend_execution() {
        let controller = Arc::new(Mutex::new(Controller::new(Duration::from_secs(1))));
        let effects = {
            controller.lock().unwrap().begin_phone_call(
                CallId(7),
                binding(),
                Codec::Pcmu,
                Instant::now(),
            )
        };
        let backend = FakeBackend {
            events: Arc::new(Mutex::new(Vec::new())),
            advanced_operations: Arc::new(Mutex::new(Vec::new())),
            capabilities: FakeCapabilities::default(),
            fail: None,
            controller_probe: Some(controller),
        };
        execute_effects(&backend, effects, |_| async { Ok::<_, FakeError>(()) })
            .await
            .unwrap();
    }
}
