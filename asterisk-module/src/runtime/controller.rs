//! Backend-neutral SCCP call state used by the Asterisk adapter.
//!
//! This module contains no Asterisk pointers or constants. It is deliberately
//! small enough to test without loading a PBX and is the seam through which a
//! second PBX backend can reuse the handset state machine.
//!
//! PBX calls, handset appearances, and media directions have separate typed
//! identities and lifetimes. A physical inbound answer claims one appearance
//! but does not answer the PBX or publish Connected until its receive-channel
//! acknowledgement. An outbound PBX answer publishes Connected immediately;
//! its ordinary media path then opens receive and starts transmit in sequence.
//! Only an explicitly selected NAT early-progress transaction couples those
//! two wire requests, and its receive acknowledgement settles both protocol
//! directions before the runtime receives the corresponding typed events.
//!
//! Terminal transitions remove every call/appearance/media index before their
//! idempotent handset cleanup effects cross the backend boundary. Consequently
//! late media acknowledgements cannot resurrect a hung-up, transferred,
//! parked, shared, or conference-owned call.

use std::collections::{HashMap, HashSet};
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[cfg(test)]
use sccp_protocol::MediaCapability;
use sccp_protocol::{
    AppearanceRingMode, CallDirection, CallId, CallInfo, CallState as HandsetCallState, Codec,
    CodecKind, ConferenceId, ConferenceListEntry, DeviceId, DeviceRegistration, Digit,
    MediaEndpoint, MediaEndpointAddress, MultimediaPayload, ParticipantId, PassthroughPartyId,
    ProtocolVersion, SessionGeneration, StationMediaCapabilities, Tone,
};

use crate::call::auto_answer::AutoAnswerMode;
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
use crate::call::auto_answer::{AutoAnswerPolicy, AutoAnswerRequest};
use crate::call::forwarding::{ForwardingDestination, ForwardingRouteReason};
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
use crate::call::hotline::HotlineDestination;
use crate::call::metadata::{CallMetadata, MetadataError};
use crate::call::transfer::{
    DeferredTransferAction, TransferCancellationReason, TransferCompletion, TransferId,
    TransferLeg, TransferMode, TransferPhase, TransferRegistry, TransferRejection,
    TransferSetupMilestone, TransferSourceRecovery, TransferSourceState, TransferTransaction,
    TransferTrigger,
};
use crate::call::voicemail::{
    VoicemailAction, VoicemailPhase, VoicemailRegistry, VoicemailRejection, VoicemailTarget,
    VoicemailTransaction, VoicemailTransactionId,
};
use crate::conference::{
    ConferenceParticipant, ConferenceParticipantIdentity, ConferenceParticipantRegistry,
    MAX_CONFERENCE_PARTICIPANTS,
};
use crate::config::{LineBinding, LineDialToneConfig, VideoMode};
use crate::media::encryption::StationEncryptionCapabilities;
use crate::media::formats::OwnedNegotiatedVideo;
pub use crate::runtime::backend::PbxCallId;
use crate::runtime::backend::{
    BargeOperation, ConferenceAnnouncement, ConferenceAnnouncementOperation,
    ConferenceAnnouncementTarget, ConferenceDestinationOperation, DriverEffect, HandsetEffect,
    ParkingOperation, PbxBridgeId, PbxEffect, PickupOperation,
};

/// Identity of one handset presentation of a PBX call.
///
/// This is intentionally distinct from the configured logical-line
/// appearance identifier exported by the protocol crate.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CallAppearanceId(pub u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CallState {
    Collecting,
    PickupCollecting,
    Ringing,
    Calling,
    Connected,
    Parking,
    Retrieving,
    Held,
    RemoteInUse,
    SharedHeld,
    Barged,
    TransferCollecting,
    Ended,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum OutboundCallPhase {
    Collecting,
    Routing,
    Proceeding,
    Ringing,
    Progress,
    Answered,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum OutboundIdentityStage {
    #[default]
    Awaiting,
    Ready,
    RingOutPublished,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CallSwitchRejection {
    Unavailable,
    Conflict,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AutoAnswerScheduleRejection {
    Unavailable,
    GenerationExhausted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HookFlashAction {
    AnswerWaiting(CallId),
    Transfer,
    Ignore,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
pub(crate) struct CallTransitionId(u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
enum CallTransitionKind {
    Additional,
    Switch(CallState),
}

#[derive(Clone, Debug)]
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
pub(crate) struct CallTransition {
    pub id: CallTransitionId,
    pub effects: Vec<DriverEffect>,
    pub device_id: DeviceId,
    pub target_call_id: CallId,
    pub target_pbx_id: PbxCallId,
    previous_call_id: Option<CallId>,
    previous_pbx_id: Option<PbxCallId>,
    kind: CallTransitionKind,
    auto_answer_mode: Option<AutoAnswerMode>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
enum CallTransitionMilestone {
    PreviousBackendHeld,
    PreviousHandsetHeld,
    TargetBackendStarted,
    TargetHandsetChanged,
    TargetMicrophoneDisabled,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
pub(crate) struct CallTransitionProgress {
    completed: HashSet<CallTransitionMilestone>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
pub(crate) struct CallTransitionCompensation {
    pub effects: Vec<DriverEffect>,
    pub remove_target_channel: bool,
}

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
impl CallTransitionProgress {
    pub fn record_success(&mut self, transition: &CallTransition, effect: &DriverEffect) {
        match effect {
            DriverEffect::Backend(PbxEffect::Hold { call_id })
                if Some(*call_id) == transition.previous_pbx_id =>
            {
                self.completed
                    .insert(CallTransitionMilestone::PreviousBackendHeld);
            }
            DriverEffect::Handset(HandsetEffect::SetCallState {
                call_id,
                state: HandsetCallState::Hold,
                ..
            }) if Some(*call_id) == transition.previous_call_id => {
                self.completed
                    .insert(CallTransitionMilestone::PreviousHandsetHeld);
            }
            DriverEffect::Backend(
                PbxEffect::CreateChannel { call_id, .. }
                | PbxEffect::Answer { call_id }
                | PbxEffect::Resume { call_id },
            ) if *call_id == transition.target_pbx_id => {
                self.completed
                    .insert(CallTransitionMilestone::TargetBackendStarted);
            }
            DriverEffect::Handset(effect)
                if effect.transition_call_id() == Some(transition.target_call_id) =>
            {
                self.completed
                    .insert(CallTransitionMilestone::TargetHandsetChanged);
                if matches!(
                    effect,
                    HandsetEffect::SetMicrophoneMode { enabled: false, .. }
                ) {
                    self.completed
                        .insert(CallTransitionMilestone::TargetMicrophoneDisabled);
                }
            }
            _ => {}
        }
    }

    fn completed(&self, milestone: CallTransitionMilestone) -> bool {
        self.completed.contains(&milestone)
    }

    #[cfg(test)]
    fn with_completed(milestones: impl IntoIterator<Item = CallTransitionMilestone>) -> Self {
        Self {
            completed: milestones.into_iter().collect(),
        }
    }
}

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
impl CallTransition {
    pub fn remove_target_channel_on_abort(&self, progress: &CallTransitionProgress) -> bool {
        progress.completed(CallTransitionMilestone::TargetBackendStarted)
            && matches!(
                self.kind,
                CallTransitionKind::Additional | CallTransitionKind::Switch(CallState::Ringing)
            )
    }
}

#[derive(Clone)]
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
struct CallDomainSnapshot {
    devices: HashMap<DeviceId, RegisteredDevice>,
    pbx_calls: HashMap<PbxCallId, PbxCall>,
    appearances: HashMap<CallAppearanceId, CallAppearance>,
    appearance_by_sccp: HashMap<CallId, CallAppearanceId>,
    shared_control_claims: HashMap<PbxCallId, SharedControlClaim>,
    call_waiting_tones: HashMap<CallId, CallWaitingToneSchedule>,
    pending_phone_answers: HashMap<CallId, PbxCallId>,
    pending_route_media: HashSet<CallId>,
}

#[derive(Clone, Debug, Default)]
struct CallRegistry {
    pbx: HashMap<PbxCallId, PbxCall>,
    appearances: HashMap<CallAppearanceId, CallAppearance>,
    by_sccp: HashMap<CallId, CallAppearanceId>,
    by_device: HashMap<DeviceId, HashSet<CallAppearanceId>>,
}

#[derive(Clone)]
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
struct PendingCallTransition {
    transition: CallTransition,
    snapshot: CallDomainSnapshot,
    progress: CallTransitionProgress,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MediaStreamState {
    #[default]
    Closed,
    Opening,
    Open(MediaEndpoint),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoFallbackReason {
    NotNegotiated,
    NativeRtpUnavailable,
    LocalEndpointUnavailable,
    DescriptorUnavailable,
    ReceiveFailed,
    TransmitFailed,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum VideoStreamState {
    #[default]
    Closed,
    Opening,
    Open {
        codec: Codec,
        endpoint: MediaEndpointAddress,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VideoPlan {
    pub session_generation: SessionGeneration,
    pub protocol: ProtocolVersion,
    pub mode: VideoMode,
    pub negotiated: OwnedNegotiatedVideo,
    pub payload: MultimediaPayload,
    pub local_endpoint: MediaEndpointAddress,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VideoMediaState {
    AudioOnly(VideoFallbackReason),
    Blocked {
        plan: VideoPlan,
        reason: VideoFallbackReason,
    },
    Ready {
        plan: VideoPlan,
        receive: VideoStreamState,
        transmit: VideoStreamState,
        transmit_token: Option<PassthroughPartyId>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoPlanReadiness {
    Ready,
    Blocked(VideoFallbackReason),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VideoFallbackOutcome {
    Ignored,
    Applied { cleanup: Option<VideoCleanup> },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VideoCleanup {
    pub device_id: DeviceId,
    pub call_id: CallId,
    pub session_generation: SessionGeneration,
}

impl From<VideoCleanup> for HandsetEffect {
    fn from(cleanup: VideoCleanup) -> Self {
        Self::StopVideo {
            device_id: cleanup.device_id,
            call_id: cleanup.call_id,
            session_generation: cleanup.session_generation,
        }
    }
}

impl VideoFallbackOutcome {
    pub const fn is_applied(&self) -> bool {
        matches!(self, Self::Applied { .. })
    }

    pub fn into_effects(self) -> Vec<DriverEffect> {
        match self {
            Self::Ignored => Vec::new(),
            Self::Applied { cleanup } => cleanup
                .into_iter()
                .map(HandsetEffect::from)
                .map(DriverEffect::from)
                .collect(),
        }
    }
}

impl Default for VideoMediaState {
    fn default() -> Self {
        Self::audio_only(VideoFallbackReason::NotNegotiated)
    }
}

impl VideoMediaState {
    pub const fn audio_only(reason: VideoFallbackReason) -> Self {
        Self::AudioOnly(reason)
    }

    pub fn blocked(plan: VideoPlan, reason: VideoFallbackReason) -> Self {
        Self::Blocked { plan, reason }
    }

    pub fn ready(plan: VideoPlan) -> Self {
        Self::Ready {
            plan,
            receive: VideoStreamState::Closed,
            transmit: VideoStreamState::Closed,
            transmit_token: None,
        }
    }

    pub const fn plan(&self) -> Option<&VideoPlan> {
        match self {
            Self::AudioOnly(_) => None,
            Self::Blocked { plan, .. } | Self::Ready { plan, .. } => Some(plan),
        }
    }

    pub const fn receive(&self) -> VideoStreamState {
        match self {
            Self::Ready { receive, .. } => *receive,
            Self::AudioOnly(_) | Self::Blocked { .. } => VideoStreamState::Closed,
        }
    }

    pub const fn transmit(&self) -> VideoStreamState {
        match self {
            Self::Ready { transmit, .. } => *transmit,
            Self::AudioOnly(_) | Self::Blocked { .. } => VideoStreamState::Closed,
        }
    }

    pub const fn fallback_reason(&self) -> Option<VideoFallbackReason> {
        match self {
            Self::AudioOnly(reason) | Self::Blocked { reason, .. } => Some(*reason),
            Self::Ready { .. } => None,
        }
    }

    pub const fn is_idle(&self) -> bool {
        match self {
            Self::Ready {
                receive, transmit, ..
            } => {
                matches!(receive, VideoStreamState::Closed)
                    && matches!(transmit, VideoStreamState::Closed)
            }
            Self::AudioOnly(_) | Self::Blocked { .. } => true,
        }
    }

    fn accepts_failure(&self, reason: VideoFallbackReason) -> bool {
        match reason {
            VideoFallbackReason::ReceiveFailed => matches!(
                self.receive(),
                VideoStreamState::Opening | VideoStreamState::Open { .. }
            ),
            VideoFallbackReason::TransmitFailed => matches!(
                self.transmit(),
                VideoStreamState::Opening | VideoStreamState::Open { .. }
            ),
            VideoFallbackReason::NotNegotiated
            | VideoFallbackReason::DescriptorUnavailable
            | VideoFallbackReason::NativeRtpUnavailable
            | VideoFallbackReason::LocalEndpointUnavailable => true,
        }
    }

    pub fn close_streams(&mut self) {
        if let Self::Ready {
            receive,
            transmit,
            transmit_token,
            ..
        } = self
        {
            *receive = VideoStreamState::Closed;
            *transmit = VideoStreamState::Closed;
            *transmit_token = None;
        }
    }

    fn cleanup(&self, device_id: &DeviceId, call_id: CallId) -> Option<VideoCleanup> {
        (!self.is_idle()).then(|| VideoCleanup {
            device_id: device_id.clone(),
            call_id,
            session_generation: self
                .plan()
                .expect("non-idle video state retains its plan")
                .session_generation,
        })
    }

    fn begin_receive(&mut self) -> bool {
        match self {
            Self::Ready {
                receive, transmit, ..
            } if matches!(receive, VideoStreamState::Closed)
                && matches!(transmit, VideoStreamState::Closed) =>
            {
                *receive = VideoStreamState::Opening;
                true
            }
            Self::AudioOnly(_) | Self::Blocked { .. } | Self::Ready { .. } => false,
        }
    }

    fn opened_receive(&mut self, codec: Codec, endpoint: MediaEndpointAddress) -> bool {
        if let Self::Ready { plan, receive, .. } = self
            && matches!(receive, VideoStreamState::Opening)
            && plan.negotiated.codec() == codec
        {
            *receive = VideoStreamState::Open { codec, endpoint };
            true
        } else {
            false
        }
    }

    fn begin_transmit(&mut self) -> bool {
        match self {
            Self::Ready {
                receive, transmit, ..
            } if matches!(receive, VideoStreamState::Open { .. })
                && matches!(transmit, VideoStreamState::Closed) =>
            {
                *transmit = VideoStreamState::Opening;
                true
            }
            Self::AudioOnly(_) | Self::Blocked { .. } | Self::Ready { .. } => false,
        }
    }

    fn opened_transmit(
        &mut self,
        codec: Codec,
        endpoint: MediaEndpointAddress,
        token: PassthroughPartyId,
    ) -> bool {
        if let Self::Ready {
            plan,
            transmit,
            transmit_token,
            ..
        } = self
            && matches!(transmit, VideoStreamState::Opening)
            && plan.negotiated.codec() == codec
        {
            *transmit = VideoStreamState::Open { codec, endpoint };
            *transmit_token = Some(token);
            true
        } else {
            false
        }
    }
}

/// Wire strategy for opening an outbound handset media path.
///
/// A staged path opens receive media first and starts transmission only after
/// the handset acknowledges it. A coupled path is reserved for a station for
/// which the adapter has positively selected NAT traversal: both requests are
/// written without an acknowledgement boundary because older 79x1 firmware
/// can otherwise withhold the receive acknowledgement indefinitely.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OutboundMediaMode {
    #[default]
    Staged,
    Coupled,
}

#[derive(Clone, Debug)]
pub struct CallSnapshot {
    pub sccp_id: CallId,
    pub pbx_id: PbxCallId,
    pub device_id: DeviceId,
    pub line_instance: u32,
    pub line: String,
    pub direction: CallDirection,
    pub state: CallState,
    pub digits: String,
    pub info: CallInfo,
    pub metadata: CallMetadata,
    pub codec: Codec,
    /// Handset receive-channel lifecycle and acknowledged RTP endpoint.
    pub audio: MediaStreamState,
    /// Handset transmit-channel lifecycle and acknowledged RTP endpoint.
    pub audio_transmit: MediaStreamState,
    pub video: VideoMediaState,
    digit_deadline: Option<Instant>,
}

/// State owned by one PBX channel, independent of how many phones display it.
#[derive(Clone, Debug)]
pub struct PbxCall {
    pub id: PbxCallId,
    pub line: String,
    pub context: String,
    pub direction: CallDirection,
    pub state: CallState,
    outbound_phase: Option<OutboundCallPhase>,
    outbound_identity_stage: OutboundIdentityStage,
    pub digits: String,
    /// Runtime privacy for this call. Once enabled it protects every shared
    /// presentation until the owning handset explicitly disables it.
    pub privacy: bool,
    pub metadata: CallMetadata,
    pending_pickup: Option<PendingDirectedPickup>,
    appearance_ids: Vec<CallAppearanceId>,
    active_appearance: Option<CallAppearanceId>,
    digit_deadline: Option<Instant>,
    last_digit_at: Option<Instant>,
    simulated_enbloc_eligible: bool,
    overlap_enabled: bool,
}

#[derive(Clone, Debug)]
struct PendingDirectedPickup {
    context: String,
    answer: bool,
}

impl PbxCall {
    pub fn appearance_ids(&self) -> impl Iterator<Item = CallAppearanceId> + '_ {
        self.appearance_ids.iter().copied()
    }

    pub fn active_appearance(&self) -> Option<CallAppearanceId> {
        self.active_appearance
    }
}

/// One phone's independently addressable presentation of a PBX call.
#[derive(Clone, Debug)]
pub struct CallAppearance {
    pub id: CallAppearanceId,
    pub sccp_id: CallId,
    pub pbx_id: PbxCallId,
    pub device_id: DeviceId,
    pub line_instance: u32,
    pub state: CallState,
    pub ring_mode: AppearanceRingMode,
    /// Privacy configured specifically for this logical-line appearance.
    pub privacy: bool,
    /// Handset-visible party metadata for this specific appearance.
    pub info: CallInfo,
    pub codec: Codec,
    /// Handset receive-channel lifecycle and acknowledged RTP endpoint.
    pub audio: MediaStreamState,
    /// Handset transmit-channel lifecycle and acknowledged RTP endpoint.
    pub audio_transmit: MediaStreamState,
    pub video: VideoMediaState,
    /// Captured presentation policy for an auto-answered intercom call.
    /// Only one-way calls retain device-microphone ownership after commit.
    auto_answer_mode: Option<AutoAnswerMode>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BargeMode {
    Directed,
    Conference,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BargeRejection {
    Unavailable,
    NotRemote,
    Private,
    Capability,
    Conflict,
    AlreadyBarged,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PickupRejection {
    Unavailable,
    Permission,
    Disabled,
    Conflict,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParkingRejection {
    Unavailable,
    Disabled,
    Conflict,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConferenceRejection {
    Unavailable,
    Disabled,
    Conflict,
    NotConnected,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConferenceDestinationRequest {
    pub device_id: DeviceId,
    pub handset_call_id: CallId,
    pub destination: String,
    pub application_options: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConferenceDestinationRejection {
    Unavailable,
    Conflict,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConferenceParticipantRejection {
    Unavailable,
    NotModerator,
    InvalidParticipant,
    Moderator,
    LastModerator,
    Conflict,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConferenceEndRejection {
    Unavailable,
    NotModerator,
    Conflict,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConferencePhase {
    Consultation,
    Merging,
    Active,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConferenceOrigin {
    Consultation,
    Selection,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConferenceInvite {
    pub moderator_id: ParticipantId,
    pub moderator_call_id: PbxCallId,
    pub music_started: bool,
    pub participant: ConferenceParticipant,
}

/// Normalized media behavior captured when a conference is created. Keeping
/// this policy with the conference makes later invite and participant actions
/// independent of configuration reloads.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ConferenceMediaPolicy {
    pub music_on_hold_class: Option<String>,
    pub mute_on_entry: bool,
    pub play_general_announcements: bool,
    pub play_participant_announcements: bool,
}

#[derive(Clone, Debug)]
pub struct ConferenceConsultationRequest {
    pub original_call_id: CallId,
    pub consultation_call_id: CallId,
    pub binding: LineBinding,
    pub codec: Codec,
    pub now: Instant,
    pub permitted: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConferenceParticipantMutationKind {
    Mute(bool),
    Remove,
    Moderator(bool),
    Hold(bool),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConferenceParticipantMutation {
    pub participant_id: ParticipantId,
    pub call_id: PbxCallId,
    pub kind: ConferenceParticipantMutationKind,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum ConferenceMutationOwner {
    Session(ConferenceId),
    Destination(PbxCallId),
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ConferenceMutationToken {
    owner: ConferenceMutationOwner,
    generation: u64,
}

#[cfg(test)]
impl ConferenceMutationToken {
    pub(crate) const fn for_test(call_id: PbxCallId) -> Self {
        Self {
            owner: ConferenceMutationOwner::Destination(call_id),
            generation: 1,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConferenceSession {
    pub id: ConferenceId,
    pub bridge_id: PbxBridgeId,
    pub device_id: DeviceId,
    pub original_handset_call_id: CallId,
    pub original_call_id: PbxCallId,
    pub consultation_handset_call_id: CallId,
    pub consultation_call_id: PbxCallId,
    pub phase: ConferencePhase,
    pub origin: ConferenceOrigin,
    pub participants: ConferenceParticipantRegistry,
    pub media_policy: ConferenceMediaPolicy,
    pub pending_invite: Option<ConferenceInvite>,
    pub pending_participant_mutation: Option<ConferenceParticipantMutation>,
}

impl ConferenceSession {
    pub fn list_effect(&self, call_id: CallId) -> HandsetEffect {
        HandsetEffect::ShowConferenceList {
            device_id: self.device_id.clone(),
            call_id,
            conference_id: self.id,
            participants: self
                .participants
                .iter()
                .map(conference_list_entry)
                .collect(),
        }
    }

    pub fn participant_actions_effect(
        &self,
        participant_id: ParticipantId,
    ) -> Option<HandsetEffect> {
        let participant = self.participants.get(participant_id)?;
        let moderator_count = self.participants.moderator_count();
        if participant.moderator && moderator_count == 1 {
            return None;
        }
        Some(HandsetEffect::ShowConferenceParticipantActions {
            device_id: self.device_id.clone(),
            call_id: self.original_handset_call_id,
            conference_id: self.id,
            participant: conference_list_entry(participant),
            removable: !participant.moderator && self.participants.iter().len() > 2,
            demotable: participant.moderator && moderator_count > 1,
        })
    }
}

fn conference_list_entry(participant: &ConferenceParticipant) -> ConferenceListEntry {
    ConferenceListEntry {
        participant_id: participant.id,
        name: participant.display_name.clone(),
        number: participant.number.clone(),
        moderator: participant.moderator,
        muted: participant.muted,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BargeSession {
    pub target_call_id: PbxCallId,
    pub barger_call_id: PbxCallId,
    pub bridge_id: PbxBridgeId,
    pub handset_call_id: CallId,
    pub mode: BargeMode,
}

#[derive(Clone, Debug)]
struct BargeGroup {
    bridge_id: PbxBridgeId,
    mode: BargeMode,
    members: Vec<CallId>,
}

#[derive(Clone, Debug, Default)]
struct ConferenceRegistry {
    by_consultation: HashMap<CallId, ConferenceSession>,
    by_pbx: HashMap<PbxCallId, CallId>,
}

#[derive(Clone, Debug, Default)]
struct BargeRegistry {
    groups: HashMap<PbxCallId, BargeGroup>,
    by_handset: HashMap<CallId, BargeSession>,
    by_pbx: HashMap<PbxCallId, CallId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SharedControlClaim {
    Steal(CallAppearanceId),
    Barge(PbxBridgeId),
}

/// One pre-allocated handset identity and configured line appearance that may
/// receive an inbound PBX call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InboundAppearance {
    pub call_id: CallId,
    pub binding: LineBinding,
    pub codec: Codec,
}

/// An eligible inbound handset offer, retained in deterministic configuration
/// order. The adapter is responsible for creating the session call and
/// honoring the configured audible-ring policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InboundOffer {
    pub device_id: DeviceId,
    pub line_instance: u32,
    pub call_id: CallId,
    pub ring_mode: AppearanceRingMode,
    pub state: HandsetCallState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CallWaitingToneSchedule {
    device_id: DeviceId,
    waiting_call_id: CallId,
    active_call_id: CallId,
    tone: Tone,
    interval: Duration,
    next_at: Instant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
struct PendingAutoAnswer {
    generation: u64,
    pbx_id: PbxCallId,
    call_id: CallId,
    deadline: Instant,
    request: AutoAnswerRequest,
    tone: Tone,
}

/// Result of applying per-device shared-line routing policy before a PBX call
/// is presented to handsets.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InboundCallDisposition {
    Offer(Vec<InboundOffer>),
    Forward {
        binding: Box<LineBinding>,
        destination: ForwardingDestination,
        reason: ForwardingRouteReason,
    },
    Unavailable(InboundUnavailableReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InboundUnavailableReason {
    Conflict,
    NoEligibleAppearance,
    DoNotDisturb,
    ForwardingConflict,
    IncomingLimit,
}

/// Controller output for a PBX hangup. Session adapters should publish every
/// availability effect before discarding their handset-side call objects.
#[derive(Clone, Debug)]
pub struct PbxHangupOutcome {
    pub primary: Option<CallSnapshot>,
    pub effects: Vec<DriverEffect>,
}

/// Generation-scoped ownership of one delayed handset cleanup after the PBX
/// channel has already been detached. Tokens never wrap, so a stale timer can
/// neither close a later call nor consume a replacement notification.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
pub(crate) struct RemoteHangupToken(u64);

#[derive(Clone, Debug)]
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
struct PendingRemoteHangup {
    token: RemoteHangupToken,
    device_id: DeviceId,
    call_id: CallId,
    deadline: Instant,
}

/// Immediate PBX teardown plus optional ownership of the short handset tone
/// presentation which remains after native channel cleanup.
#[derive(Clone, Debug)]
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
pub(crate) struct RemoteHangupPlan {
    pub outcome: PbxHangupOutcome,
    pub pending: Option<RemoteHangupToken>,
}

/// One atomically detached conference and the terminal work required to
/// release its backend and handset resources. Call identifiers are retained
/// separately so an adapter can release its owned channel references even if
/// one of the best-effort hangup effects fails.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConferenceCleanupPlan {
    pub conference_id: ConferenceId,
    pub call_ids: Vec<PbxCallId>,
    pub effects: Vec<DriverEffect>,
}

/// Committed cleanup after one handset presentation failed. A surviving
/// session keeps its stable conference, bridge, and participant identities;
/// otherwise the effects contain the complete terminal cleanup sequence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConferenceParticipantFailureOutcome {
    pub conference_id: ConferenceId,
    pub failed_call_id: PbxCallId,
    pub call_ids: Vec<PbxCallId>,
    pub surviving_session: Option<ConferenceSession>,
    pub effects: Vec<DriverEffect>,
}

#[derive(Clone, Debug)]
pub struct TransferConsultationRequest {
    pub source_call_id: CallId,
    pub consultation_call_id: CallId,
    pub binding: LineBinding,
    pub codec: Codec,
    pub complete_on_hangup: bool,
    pub now: Instant,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransferCompletionPlan {
    pub completion: TransferCompletion,
    pub effects: Vec<DriverEffect>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransferTerminalOutcome {
    pub transaction: TransferTransaction,
    pub effects: Vec<DriverEffect>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VoicemailPlan {
    pub transaction: VoicemailTransaction,
    pub effects: Vec<DriverEffect>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VoicemailTerminalOutcome {
    pub transaction: VoicemailTransaction,
    pub effects: Vec<DriverEffect>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VoicemailNativeOutcome {
    Committed(VoicemailTerminalOutcome),
    CallAlreadyEnded,
}

#[derive(Clone, Debug)]
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
pub(crate) struct HotlineCallRequest {
    pub handset_call_id: CallId,
    pub binding: LineBinding,
    pub codec: Codec,
    pub destination: HotlineDestination,
    pub now: Instant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodecPreferenceRejection {
    Unavailable,
    NotPreDial,
    Ambiguous,
}

#[derive(Clone, Debug)]
pub struct RegisteredDevice {
    pub registration: DeviceRegistration,
    pub session_generation: SessionGeneration,
    pub capabilities: StationMediaCapabilities,
    pub audio_encryption: StationEncryptionCapabilities,
    pub selected_line: Option<u32>,
    active_call: Option<CallId>,
    selected_calls: HashSet<CallId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegisterSessionOutcome {
    pub cleanup: Vec<DriverEffect>,
    pub replaced: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DndMode {
    #[default]
    Off,
    Silent,
    Reject,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ForwardingState {
    pub all: Option<ForwardingDestination>,
    pub busy: Option<ForwardingDestination>,
    pub no_answer: Option<ForwardingDestination>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DeviceFeatureState {
    pub dnd: DndMode,
    pub privacy: bool,
    pub forwarding: ForwardingState,
    pub buttons: HashMap<u32, bool>,
}

impl RegisteredDevice {
    pub fn active_call(&self) -> Option<CallId> {
        self.active_call
    }

    pub fn selected_calls(&self) -> impl Iterator<Item = CallId> + '_ {
        self.selected_calls.iter().copied()
    }

    pub fn is_call_selected(&self, call_id: CallId) -> bool {
        self.selected_calls.contains(&call_id)
    }
}

pub struct Controller {
    next_pbx_id: u64,
    next_appearance_id: u64,
    next_bridge_id: u64,
    next_conference_id: u32,
    next_participant_id: u32,
    next_conference_mutation_generation: u64,
    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
    next_call_transition_id: u64,
    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
    next_auto_answer_generation: u64,
    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
    next_remote_hangup_generation: u64,
    first_digit: Duration,
    interdigit: Duration,
    dial_terminator: char,
    simulate_enbloc: bool,
    overlap_devices: HashSet<DeviceId>,
    line_dial_tones: HashMap<String, LineDialToneConfig>,
    line_incoming_limits: HashMap<String, u32>,
    call_waiting_tones: HashMap<CallId, CallWaitingToneSchedule>,
    /// Inbound handset answers which have claimed an appearance but have not
    /// yet been committed to the PBX because OpenReceiveChannel is pending.
    pending_phone_answers: HashMap<CallId, PbxCallId>,
    /// Outbound coupled ORC/SMT transactions which have not received an ORC
    /// acknowledgement yet. The transmit acknowledgement is independent.
    pending_route_media: HashSet<CallId>,
    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
    pending_call_transitions: HashMap<CallTransitionId, PendingCallTransition>,
    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
    auto_answer_requests: HashMap<PbxCallId, AutoAnswerRequest>,
    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
    pending_auto_answers: HashMap<CallId, PendingAutoAnswer>,
    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
    pending_remote_hangups: HashMap<CallId, PendingRemoteHangup>,
    devices: HashMap<DeviceId, RegisteredDevice>,
    features: HashMap<DeviceId, DeviceFeatureState>,
    call_registry: CallRegistry,
    shared_control_claims: HashMap<PbxCallId, SharedControlClaim>,
    barges: BargeRegistry,
    conferences: ConferenceRegistry,
    conference_mutations: HashMap<ConferenceMutationOwner, u64>,
    transfers: TransferRegistry,
    voicemail: VoicemailRegistry,
    redirect_claims: HashSet<PbxCallId>,
}

/// Runs one pure controller transition and drops the mutex guard before
/// returning its owned result to adapter code. Adapter I/O belongs after this
/// function returns.
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
pub(crate) fn controller_step<T>(
    controller: &Mutex<Controller>,
    step: impl FnOnce(&mut Controller) -> T,
) -> T {
    let mut controller = controller.lock().expect("SCCP controller lock poisoned");
    step(&mut controller)
}

impl Controller {
    pub fn new(interdigit: Duration) -> Self {
        Self::with_digit_timeouts(interdigit, interdigit)
    }

    pub fn with_digit_timeouts(first_digit: Duration, interdigit: Duration) -> Self {
        Self {
            next_pbx_id: 1,
            next_appearance_id: 1,
            next_bridge_id: 1,
            next_conference_id: 1,
            next_participant_id: 1,
            next_conference_mutation_generation: 1,
            #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
            next_call_transition_id: 1,
            #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
            next_auto_answer_generation: 1,
            #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
            next_remote_hangup_generation: 1,
            first_digit,
            interdigit,
            dial_terminator: '#',
            simulate_enbloc: true,
            overlap_devices: HashSet::new(),
            line_dial_tones: HashMap::new(),
            line_incoming_limits: HashMap::new(),
            call_waiting_tones: HashMap::new(),
            pending_phone_answers: HashMap::new(),
            pending_route_media: HashSet::new(),
            #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
            pending_call_transitions: HashMap::new(),
            #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
            auto_answer_requests: HashMap::new(),
            #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
            pending_auto_answers: HashMap::new(),
            #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
            pending_remote_hangups: HashMap::new(),
            devices: HashMap::new(),
            features: HashMap::new(),
            call_registry: CallRegistry::default(),
            shared_control_claims: HashMap::new(),
            barges: BargeRegistry::default(),
            conferences: ConferenceRegistry::default(),
            conference_mutations: HashMap::new(),
            transfers: TransferRegistry::default(),
            voicemail: VoicemailRegistry::default(),
            redirect_claims: HashSet::new(),
        }
    }

    /// Applies to future digit collection updates. Existing absolute
    /// deadlines and active call state are intentionally left unchanged.
    pub fn set_interdigit_timeout(&mut self, interdigit: Duration) {
        self.interdigit = interdigit;
    }

    /// Applies to future calls that have not collected their first digit.
    /// Existing absolute deadlines and active call state are unchanged.
    pub fn set_first_digit_timeout(&mut self, first_digit: Duration) {
        self.first_digit = first_digit;
    }

    /// Applies to future digits. A terminator already collected into a call is
    /// not rewritten when this policy changes.
    pub fn set_dial_terminator(&mut self, character: char) {
        self.dial_terminator = character;
    }

    /// Applies the fast-keypad deadline policy to future digits. Disabling it
    /// leaves existing absolute deadlines unchanged.
    pub fn set_simulated_enbloc(&mut self, enabled: bool) {
        self.simulate_enbloc = enabled;
    }

    pub fn set_overlap_devices(&mut self, devices: impl IntoIterator<Item = DeviceId>) {
        self.overlap_devices = devices.into_iter().collect();
    }

    /// Replaces the normalized logical-line dial-tone policy used by future
    /// off-hook and digit-collection transitions.
    pub fn set_line_dial_tones(
        &mut self,
        lines: impl IntoIterator<Item = (String, LineDialToneConfig)>,
    ) {
        self.line_dial_tones = lines.into_iter().collect();
    }

    /// Replaces the logical-line limit used for future inbound admission.
    /// Existing calls are never evicted when a reload lowers a limit.
    pub fn set_line_incoming_limits(&mut self, lines: impl IntoIterator<Item = (String, u32)>) {
        self.line_incoming_limits = lines.into_iter().collect();
    }

    /// Installs a newly accepted station session.
    ///
    /// A newer session atomically retires the prior session's call state. Its
    /// cleanup is returned to the adapter. Handset effects for the replaced
    /// connection are discarded, while effects for surviving devices remain.
    pub fn register_session(
        &mut self,
        session_generation: SessionGeneration,
        registration: DeviceRegistration,
    ) -> Option<RegisterSessionOutcome> {
        let device = registration.id.clone();
        if self
            .devices
            .get(&device)
            .is_some_and(|current| session_generation <= current.session_generation)
        {
            return None;
        }

        let replaced = self.devices.contains_key(&device);
        let mut cleanup = if replaced {
            self.disconnected(&device)
        } else {
            Vec::new()
        };
        cleanup.retain(|effect| {
            !matches!(effect, DriverEffect::Handset(effect) if effect.device_id() == &device)
        });
        self.devices.insert(
            device,
            RegisteredDevice {
                registration,
                session_generation,
                capabilities: StationMediaCapabilities::default(),
                audio_encryption: StationEncryptionCapabilities::default(),
                selected_line: None,
                active_call: None,
                selected_calls: HashSet::new(),
            },
        );
        Some(RegisterSessionOutcome { cleanup, replaced })
    }

    /// Replaces media capabilities only for the connection that advertised
    /// them. A late update from a replaced connection has no effect.
    pub fn update_capabilities(
        &mut self,
        device: &DeviceId,
        session_generation: SessionGeneration,
        capabilities: StationMediaCapabilities,
    ) -> bool {
        let Some(state) = self
            .devices
            .get_mut(device)
            .filter(|state| state.session_generation == session_generation)
        else {
            return false;
        };
        state.capabilities = capabilities;
        true
    }

    /// Replaces audio-encryption advertisements only for the connection that
    /// supplied them.
    pub fn update_audio_encryption_capabilities(
        &mut self,
        device: &DeviceId,
        session_generation: SessionGeneration,
        capabilities: StationEncryptionCapabilities,
    ) -> bool {
        let Some(state) = self
            .devices
            .get_mut(device)
            .filter(|state| state.session_generation == session_generation)
        else {
            return false;
        };
        state.audio_encryption = capabilities;
        true
    }

    pub fn session_is_current(
        &self,
        device: &DeviceId,
        session_generation: SessionGeneration,
    ) -> bool {
        self.devices
            .get(device)
            .is_some_and(|state| state.session_generation == session_generation)
    }

    #[cfg(test)]
    pub fn registered(&mut self, registration: DeviceRegistration) {
        let next = self.devices.get(&registration.id).map_or(1, |state| {
            state
                .session_generation
                .get()
                .checked_add(1)
                .expect("test session generation is available")
        });
        let generation = SessionGeneration::new(next).expect("test session generation is nonzero");
        let _ = self.register_session(generation, registration);
    }

    #[cfg(test)]
    pub fn capabilities(
        &mut self,
        device: &DeviceId,
        capabilities: Vec<sccp_protocol::MediaCapability>,
    ) {
        let generation = self
            .devices
            .get(device)
            .expect("test device must be registered")
            .session_generation;
        assert!(self.update_capabilities(device, generation, capabilities.into()));
    }

    pub fn disconnected(&mut self, device: &DeviceId) -> Vec<DriverEffect> {
        #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
        self.pending_call_transitions
            .retain(|_, pending| &pending.transition.device_id != device);
        #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
        {
            let removed = self
                .appearances_for_device(device)
                .map(|appearance| appearance.sccp_id)
                .collect::<HashSet<_>>();
            self.pending_auto_answers
                .retain(|call_id, _| !removed.contains(call_id));
        }
        #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
        self.pending_remote_hangups
            .retain(|_, pending| &pending.device_id != device);
        let mut pending_answers = self
            .appearances_for_device(device)
            .filter(|appearance| {
                self.pending_phone_answers.get(&appearance.sccp_id) == Some(&appearance.pbx_id)
            })
            .map(|appearance| appearance.sccp_id)
            .collect::<Vec<_>>();
        pending_answers.sort_unstable_by_key(|call_id| call_id.0);
        let transfer = self.transfers.get(device).cloned();
        let conferences: Vec<_> = self
            .conferences
            .by_consultation
            .values()
            .filter(|session| {
                session
                    .participants
                    .iter()
                    .any(|participant| &participant.device_id == device)
            })
            .cloned()
            .collect();
        let barges: Vec<_> = self
            .call_registry
            .by_device
            .get(device)
            .into_iter()
            .flatten()
            .filter_map(|appearance_id| self.call_registry.appearances.get(appearance_id))
            .map(|appearance| appearance.sccp_id)
            .filter(|call_id| self.barges.by_handset.contains_key(call_id))
            .collect();
        let mut actions = Vec::new();
        if let Some(transfer) = transfer
            && let Ok(outcome) = self.abort_transfer(
                device,
                transfer.id,
                TransferCancellationReason::DeviceDisconnect,
            )
        {
            actions.extend(outcome.effects);
        }
        for session in conferences {
            if session.phase == ConferencePhase::Active {
                let departing_calls = session
                    .participants
                    .iter()
                    .filter(|participant| &participant.device_id == device)
                    .map(|participant| participant.pbx_call_id)
                    .collect::<Vec<_>>();
                for pbx_id in departing_calls {
                    let Some(current) = self.conference_session_by_pbx(pbx_id).cloned() else {
                        continue;
                    };
                    actions.extend(self.active_conference_departure(current, pbx_id, None, false));
                }
            } else {
                let bridge_created = matches!(session.phase, ConferencePhase::Merging);
                actions.extend(self.end_conference_internal(session, bridge_created, None));
            }
        }
        for call_id in barges {
            actions.extend(self.end_barge_internal(call_id, true, true, false));
        }
        // A handset answer does not answer the Asterisk channel until its
        // receive-channel acknowledgement commits media. Losing that exact
        // owner is therefore an answer failure, not an established shared
        // call that another appearance can steal.
        for call_id in pending_answers {
            actions.extend(self.terminate(call_id));
        }
        self.devices.remove(device);
        let appearance_ids: Vec<_> = self
            .call_registry
            .by_device
            .get(device)
            .into_iter()
            .flatten()
            .copied()
            .collect();
        let mut empty_calls = HashSet::new();
        for appearance_id in appearance_ids {
            if let Some(appearance) = self.remove_appearance(appearance_id)
                && self
                    .call_registry
                    .pbx
                    .get(&appearance.pbx_id)
                    .is_some_and(|call| call.appearance_ids.is_empty())
            {
                empty_calls.insert(appearance.pbx_id);
            }
        }
        for pbx_id in empty_calls {
            actions.extend(self.end_barges_for_target(pbx_id));
            if self.remove_pbx_call(pbx_id).is_some() {
                actions.push(PbxEffect::Hangup { call_id: pbx_id }.into());
            }
        }
        debug_assert!(self.invariant_error().is_none());
        actions
    }

    pub fn is_registered(&self, device: &DeviceId) -> bool {
        self.devices.contains_key(device)
    }

    pub fn registered_device(&self, device: &DeviceId) -> Option<&RegisteredDevice> {
        self.devices.get(device)
    }

    pub fn registered_devices(&self) -> impl Iterator<Item = (&DeviceId, &RegisteredDevice)> {
        self.devices.iter()
    }

    pub fn feature_state(&self, device: &DeviceId) -> Option<&DeviceFeatureState> {
        self.features.get(device)
    }

    pub fn feature_state_mut(&mut self, device: &DeviceId) -> &mut DeviceFeatureState {
        self.features.entry(device.clone()).or_default()
    }

    pub fn set_feature_state(&mut self, device: &DeviceId, state: DeviceFeatureState) {
        self.features.insert(device.clone(), state);
    }

    pub fn replace_feature_states(&mut self, states: HashMap<DeviceId, DeviceFeatureState>) {
        self.features = states;
    }

    pub fn set_dnd(&mut self, device: &DeviceId, mode: DndMode) {
        self.feature_state_mut(device).dnd = mode;
    }

    pub fn set_privacy(&mut self, device: &DeviceId, enabled: bool) {
        self.feature_state_mut(device).privacy = enabled;
    }

    pub fn set_forwarding(&mut self, device: &DeviceId, forwarding: ForwardingState) {
        self.feature_state_mut(device).forwarding = forwarding;
    }

    pub fn set_feature_button(&mut self, device: &DeviceId, instance: u32, enabled: bool) {
        self.feature_state_mut(device)
            .buttons
            .insert(instance, enabled);
    }

    /// Change privacy for the call owned by `call_id`. Remote appearances
    /// cannot change another handset's privacy policy.
    pub fn set_call_privacy(&mut self, call_id: CallId, enabled: bool) -> bool {
        let Some(appearance) = self.appearance_for_call(call_id) else {
            return false;
        };
        let pbx_id = appearance.pbx_id;
        let appearance_id = appearance.id;
        let Some(call) = self.call_registry.pbx.get_mut(&pbx_id) else {
            return false;
        };
        if call.active_appearance != Some(appearance_id) {
            return false;
        }
        call.privacy = enabled;
        self.refresh_conference_participant_identity(pbx_id);
        true
    }

    pub fn call_privacy(&self, call_id: CallId) -> Option<bool> {
        let appearance = self.appearance_for_call(call_id)?;
        self.call_registry
            .pbx
            .get(&appearance.pbx_id)
            .map(|call| call.privacy)
    }

    pub fn select_line(&mut self, device: &DeviceId, line_instance: u32) -> bool {
        let Some(state) = self.devices.get_mut(device) else {
            return false;
        };
        state.selected_line = Some(line_instance);
        true
    }

    fn set_active_call(&mut self, device: &DeviceId, call_id: Option<CallId>) -> bool {
        if call_id.is_some_and(|call_id| {
            self.appearance_for_call(call_id)
                .is_none_or(|appearance| &appearance.device_id != device)
        }) {
            return false;
        }
        let Some(state) = self.devices.get_mut(device) else {
            return false;
        };
        state.active_call = call_id;
        true
    }

    pub fn set_call_selected(
        &mut self,
        device: &DeviceId,
        call_id: CallId,
        selected: bool,
    ) -> bool {
        if self
            .appearance_for_call(call_id)
            .is_none_or(|appearance| &appearance.device_id != device)
        {
            return false;
        }
        let Some(state) = self.devices.get_mut(device) else {
            return false;
        };
        if selected {
            state.selected_calls.insert(call_id);
        } else {
            state.selected_calls.remove(&call_id);
        }
        true
    }

    /// Toggle a handset's explicit selection marker for an appearance owned
    /// by that handset. The returned value is the new selection state.
    pub fn toggle_call_selected(&mut self, device: &DeviceId, call_id: CallId) -> Option<bool> {
        if self
            .appearance_for_call(call_id)
            .is_none_or(|appearance| &appearance.device_id != device)
        {
            return None;
        }
        let state = self.devices.get_mut(device)?;
        let selected = !state.selected_calls.contains(&call_id);
        if selected {
            state.selected_calls.insert(call_id);
        } else {
            state.selected_calls.remove(&call_id);
        }
        Some(selected)
    }

    /// Move one handset's active call plane to an exact local presentation.
    ///
    /// The previous connected leg is held before the target is answered or
    /// resumed. Validation is completed before either transition so a stale,
    /// remote, or conference-owned target cannot disturb the current call.
    pub fn switch_active_call(
        &mut self,
        device_id: &DeviceId,
        call_id: CallId,
    ) -> Result<Vec<DriverEffect>, CallSwitchRejection> {
        let target = self
            .appearance_for_call(call_id)
            .filter(|appearance| &appearance.device_id == device_id)
            .cloned()
            .ok_or(CallSwitchRejection::Unavailable)?;
        if self.conferences.by_pbx.contains_key(&target.pbx_id) {
            return Err(CallSwitchRejection::Conflict);
        }
        if !matches!(
            target.state,
            CallState::Ringing | CallState::Connected | CallState::Held | CallState::SharedHeld
        ) {
            return Err(CallSwitchRejection::Conflict);
        }
        let previous = self
            .devices
            .get(device_id)
            .ok_or(CallSwitchRejection::Unavailable)?
            .active_call;
        if previous == Some(call_id) {
            return Ok(Vec::new());
        }
        if let Some(previous) = previous
            && let Some(previous_state) = self.call_state(previous)
            && matches!(
                previous_state,
                CallState::Collecting
                    | CallState::Calling
                    | CallState::Connected
                    | CallState::TransferCollecting
            )
            && (self.conference_session(previous).is_some()
                || self.barges.by_handset.contains_key(&previous))
        {
            return Err(CallSwitchRejection::Conflict);
        }

        let mut effects = Vec::new();
        if let Some(previous) = previous
            && let Some(previous_state) = self.call_state(previous)
            && matches!(
                previous_state,
                CallState::Collecting
                    | CallState::Calling
                    | CallState::Connected
                    | CallState::TransferCollecting
            )
        {
            let hold = self.hold(previous);
            if hold.is_empty() {
                return Err(CallSwitchRejection::Conflict);
            }
            effects.extend(hold);
        }
        let activate = match target.state {
            CallState::Ringing => self.phone_answer(call_id),
            CallState::Held | CallState::SharedHeld => self.resume(call_id),
            CallState::Connected => {
                self.set_active_call(device_id, Some(call_id));
                self.select_line(device_id, target.line_instance);
                vec![
                    HandsetEffect::SetCallState {
                        device_id: device_id.clone(),
                        call_id,
                        state: HandsetCallState::Connected,
                        stop_media: false,
                    }
                    .into(),
                ]
            }
            _ => Vec::new(),
        };
        if activate.is_empty() && target.state != CallState::Connected {
            return Err(CallSwitchRejection::Conflict);
        }
        effects.extend(activate);
        debug_assert!(self.invariant_error().is_none());
        Ok(effects)
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
    pub(crate) fn begin_active_call_switch_transaction(
        &mut self,
        device_id: &DeviceId,
        call_id: CallId,
    ) -> Result<CallTransition, CallSwitchRejection> {
        if self
            .pending_call_transitions
            .values()
            .any(|pending| &pending.transition.device_id == device_id)
        {
            return Err(CallSwitchRejection::Conflict);
        }
        let previous_call_id = self
            .devices
            .get(device_id)
            .and_then(|device| device.active_call);
        let previous_pbx_id = previous_call_id
            .and_then(|previous| self.appearance_for_call(previous))
            .map(|appearance| appearance.pbx_id);
        let target = self
            .appearance_for_call(call_id)
            .cloned()
            .ok_or(CallSwitchRejection::Unavailable)?;
        let snapshot = self.call_domain_snapshot();
        let effects = self.switch_active_call(device_id, call_id)?;
        let transition = CallTransition {
            id: self.allocate_call_transition_id(),
            effects,
            device_id: device_id.clone(),
            target_call_id: call_id,
            target_pbx_id: target.pbx_id,
            previous_call_id,
            previous_pbx_id,
            kind: CallTransitionKind::Switch(target.state),
            auto_answer_mode: None,
        };
        self.pending_call_transitions.insert(
            transition.id,
            PendingCallTransition {
                transition: transition.clone(),
                snapshot,
                progress: CallTransitionProgress::default(),
            },
        );
        Ok(transition)
    }

    /// Resolve a hook flash against the exact active handset identity without
    /// mutating call state. A waiting inbound call takes precedence over
    /// starting a consultation transfer; existing transfer consultations use
    /// the same action so a second flash can complete that transaction.
    pub fn hook_flash_action(&self, device_id: &DeviceId, call_id: CallId) -> HookFlashAction {
        let Some(device) = self.devices.get(device_id) else {
            return HookFlashAction::Ignore;
        };
        if device.active_call != Some(call_id) {
            return HookFlashAction::Ignore;
        }
        if let Some(transfer) = self.transfers.get(device_id) {
            return if transfer
                .consultation
                .is_some_and(|leg| leg.handset_call_id == call_id)
            {
                HookFlashAction::Transfer
            } else {
                HookFlashAction::Ignore
            };
        }
        let Some(active) = self.appearance_for_call(call_id) else {
            return HookFlashAction::Ignore;
        };
        if active.state != CallState::Connected
            || self.conferences.by_pbx.contains_key(&active.pbx_id)
            || self.barges.by_handset.contains_key(&call_id)
        {
            return HookFlashAction::Ignore;
        }
        let mut waiting = self
            .appearances_for_device(device_id)
            .filter(|appearance| {
                appearance.sccp_id != call_id && appearance.state == CallState::Ringing
            })
            .map(|appearance| appearance.sccp_id)
            .collect::<Vec<_>>();
        waiting.sort_by_key(|waiting_call_id| waiting_call_id.0);
        waiting
            .first()
            .copied()
            .map_or(HookFlashAction::Transfer, HookFlashAction::AnswerWaiting)
    }

    pub fn begin_phone_call(
        &mut self,
        sccp_id: CallId,
        binding: LineBinding,
        codec: Codec,
        now: Instant,
    ) -> Vec<DriverEffect> {
        if self.call_registry.by_sccp.contains_key(&sccp_id) {
            return Vec::new();
        }
        let device_id = binding.device_id.clone();
        let line_instance = binding.line_instance;
        let pbx_id = self.allocate_pbx_id();
        let privacy = binding.appearance.privacy
            || self
                .features
                .get(&device_id)
                .is_some_and(|state| state.privacy);
        let appearance_id = self.allocate_appearance_id();
        let info = CallInfo {
            direction: CallDirection::Outbound,
            calling_name: binding.line.caller_name.clone(),
            calling_number: binding.line.caller_number.clone(),
            ..CallInfo::default()
        };
        let pbx_call = PbxCall {
            id: pbx_id,
            line: binding.line.number.clone(),
            context: binding.line.context.clone(),
            direction: CallDirection::Outbound,
            state: CallState::Collecting,
            outbound_phase: Some(OutboundCallPhase::Collecting),
            outbound_identity_stage: OutboundIdentityStage::Awaiting,
            digits: String::new(),
            privacy,
            metadata: CallMetadata::default(),
            pending_pickup: None,
            appearance_ids: Vec::new(),
            active_appearance: Some(appearance_id),
            digit_deadline: Some(now + self.first_digit),
            last_digit_at: None,
            simulated_enbloc_eligible: self.simulate_enbloc,
            overlap_enabled: self.overlap_devices.contains(&device_id),
        };
        let appearance = CallAppearance {
            id: appearance_id,
            sccp_id,
            pbx_id,
            device_id: device_id.clone(),
            line_instance: binding.line_instance,
            state: CallState::Collecting,
            ring_mode: binding.appearance.ring_mode,
            privacy: binding.appearance.privacy,
            info,
            codec,
            audio: MediaStreamState::Closed,
            audio_transmit: MediaStreamState::Closed,
            video: VideoMediaState::default(),
            auto_answer_mode: None,
        };
        if !self.insert_pbx_call(pbx_call, appearance) {
            return Vec::new();
        }
        self.select_line(&device_id, line_instance);
        self.set_active_call(&device_id, Some(sccp_id));
        self.set_call_selected(&device_id, sccp_id, true);
        debug_assert!(self.invariant_error().is_none());
        vec![
            PbxEffect::CreateChannel {
                handset_call_id: sccp_id,
                call_id: pbx_id,
                binding: Box::new(binding),
                codec,
            }
            .into(),
        ]
    }

    /// Begin an additional handset call, holding the exact active ordinary
    /// call first. Conference and barge legs reject the transition so a new
    /// call cannot silently detach their active presentation.
    pub fn begin_additional_phone_call(
        &mut self,
        sccp_id: CallId,
        binding: LineBinding,
        codec: Codec,
        now: Instant,
    ) -> Result<Vec<DriverEffect>, CallSwitchRejection> {
        if self.call_registry.by_sccp.contains_key(&sccp_id)
            || !self.devices.contains_key(&binding.device_id)
        {
            return Err(CallSwitchRejection::Unavailable);
        }
        let active = self
            .devices
            .get(&binding.device_id)
            .and_then(|device| device.active_call);
        if active.is_some_and(|call_id| {
            self.conference_session(call_id).is_some()
                || self.barges.by_handset.contains_key(&call_id)
        }) {
            return Err(CallSwitchRejection::Conflict);
        }
        let mut effects = active
            .filter(|call_id| {
                self.call_state(*call_id).is_some_and(|state| {
                    matches!(
                        state,
                        CallState::Collecting
                            | CallState::Calling
                            | CallState::Connected
                            | CallState::TransferCollecting
                    )
                })
            })
            .map_or_else(Vec::new, |call_id| self.hold(call_id));
        let created = self.begin_phone_call(sccp_id, binding, codec, now);
        if created.is_empty() {
            return Err(CallSwitchRejection::Unavailable);
        }
        effects.extend(created);
        Ok(effects)
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
    pub(crate) fn begin_additional_phone_call_transaction(
        &mut self,
        sccp_id: CallId,
        binding: LineBinding,
        codec: Codec,
        now: Instant,
    ) -> Result<CallTransition, CallSwitchRejection> {
        if self
            .pending_call_transitions
            .values()
            .any(|pending| pending.transition.device_id == binding.device_id)
        {
            return Err(CallSwitchRejection::Conflict);
        }
        let previous_call_id = self
            .devices
            .get(&binding.device_id)
            .and_then(|device| device.active_call);
        if previous_call_id.is_some_and(|previous| {
            self.call_state(previous)
                .is_none_or(|state| state != CallState::Connected)
        }) {
            return Err(CallSwitchRejection::Conflict);
        }
        let previous_pbx_id = previous_call_id
            .and_then(|previous| self.appearance_for_call(previous))
            .map(|appearance| appearance.pbx_id);
        let device_id = binding.device_id.clone();
        let snapshot = self.call_domain_snapshot();
        let effects = self.begin_additional_phone_call(sccp_id, binding, codec, now)?;
        let target_pbx_id = self
            .appearance_for_call(sccp_id)
            .map(|appearance| appearance.pbx_id)
            .ok_or(CallSwitchRejection::Unavailable)?;
        let transition = CallTransition {
            id: self.allocate_call_transition_id(),
            effects,
            device_id,
            target_call_id: sccp_id,
            target_pbx_id,
            previous_call_id,
            previous_pbx_id,
            kind: CallTransitionKind::Additional,
            auto_answer_mode: None,
        };
        self.pending_call_transitions.insert(
            transition.id,
            PendingCallTransition {
                transition: transition.clone(),
                snapshot,
                progress: CallTransitionProgress::default(),
            },
        );
        Ok(transition)
    }

    /// Begin a configured PLAR/hotline call as the same rollback-safe
    /// additional-call transaction used by ordinary NewCall, but route the
    /// captured destination immediately without exposing digit collection or
    /// a dial tone to the handset.
    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
    pub(crate) fn begin_hotline_call_transaction(
        &mut self,
        request: HotlineCallRequest,
    ) -> Result<CallTransition, CallSwitchRejection> {
        let destination = request.destination.as_str().to_owned();
        let mut transition = self.begin_additional_phone_call_transaction(
            request.handset_call_id,
            request.binding,
            request.codec,
            request.now,
        )?;
        let routing = self.enbloc(request.handset_call_id, destination);
        if !matches!(
            routing.last(),
            Some(DriverEffect::Backend(PbxEffect::StartRouting { .. }))
        ) {
            let _ = self.abort_call_transition(transition.id, &CallTransitionProgress::default());
            return Err(CallSwitchRejection::Conflict);
        }
        transition.effects.retain(|effect| {
            !matches!(
                effect,
                DriverEffect::Handset(HandsetEffect::StartTone { call_id, .. })
                    if *call_id == request.handset_call_id
            )
        });
        transition.effects.extend(routing);
        let Some(pending) = self.pending_call_transitions.get_mut(&transition.id) else {
            return Err(CallSwitchRejection::Conflict);
        };
        pending.transition.effects.clone_from(&transition.effects);
        debug_assert!(self.invariant_error().is_none());
        Ok(transition)
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
    pub(crate) fn commit_call_transition(&mut self, id: CallTransitionId) -> bool {
        let Some(pending) = self.pending_call_transitions.remove(&id) else {
            return false;
        };
        if let Some(mode) = pending.transition.auto_answer_mode
            && let Some(appearance_id) = self
                .call_registry
                .by_sccp
                .get(&pending.transition.target_call_id)
                .copied()
            && let Some(appearance) = self.call_registry.appearances.get_mut(&appearance_id)
        {
            appearance.auto_answer_mode = Some(mode);
        }
        true
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
    pub(crate) fn record_call_transition_success(
        &mut self,
        id: CallTransitionId,
        effect: &DriverEffect,
    ) -> bool {
        let Some(pending) = self.pending_call_transitions.get_mut(&id) else {
            return false;
        };
        pending.progress.record_success(&pending.transition, effect);
        true
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
    pub(crate) fn abort_call_transition(
        &mut self,
        id: CallTransitionId,
        progress: &CallTransitionProgress,
    ) -> Vec<DriverEffect> {
        let Some(pending) = self.pending_call_transitions.remove(&id) else {
            return Vec::new();
        };
        let transition = pending.transition;
        if !self.restore_call_domain(&pending.snapshot, &transition) {
            return Vec::new();
        }
        let mut effects = Vec::new();
        match transition.kind {
            CallTransitionKind::Additional => {
                if progress.completed(CallTransitionMilestone::TargetBackendStarted) {
                    effects.push(
                        PbxEffect::Hangup {
                            call_id: transition.target_pbx_id,
                        }
                        .into(),
                    );
                }
                effects.push(
                    HandsetEffect::SetCallState {
                        device_id: transition.device_id.clone(),
                        call_id: transition.target_call_id,
                        state: HandsetCallState::OnHook,
                        stop_media: true,
                    }
                    .into(),
                );
            }
            CallTransitionKind::Switch(CallState::Ringing)
                if progress.completed(CallTransitionMilestone::TargetBackendStarted) =>
            {
                effects.push(
                    PbxEffect::Hangup {
                        call_id: transition.target_pbx_id,
                    }
                    .into(),
                );
                if let Some(mut outcome) = self.pbx_hangup_with_effects(transition.target_pbx_id) {
                    effects.append(&mut outcome.effects);
                }
            }
            CallTransitionKind::Switch(CallState::Held | CallState::SharedHeld)
                if progress.completed(CallTransitionMilestone::TargetBackendStarted) =>
            {
                effects.push(
                    PbxEffect::Hold {
                        call_id: transition.target_pbx_id,
                    }
                    .into(),
                );
                if progress.completed(CallTransitionMilestone::TargetHandsetChanged) {
                    effects.push(
                        HandsetEffect::SetCallState {
                            device_id: transition.device_id.clone(),
                            call_id: transition.target_call_id,
                            state: HandsetCallState::Hold,
                            stop_media: true,
                        }
                        .into(),
                    );
                }
            }
            CallTransitionKind::Switch(_) => {}
        }
        if progress.completed(CallTransitionMilestone::TargetMicrophoneDisabled) {
            effects.push(
                HandsetEffect::SetMicrophoneMode {
                    device_id: transition.device_id.clone(),
                    call_id: transition.target_call_id,
                    enabled: true,
                }
                .into(),
            );
        }
        if progress.completed(CallTransitionMilestone::PreviousBackendHeld)
            && let Some(call_id) = transition.previous_pbx_id
        {
            effects.push(PbxEffect::Resume { call_id }.into());
        }
        if progress.completed(CallTransitionMilestone::PreviousHandsetHeld)
            && let Some(previous_call_id) = transition.previous_call_id
            && let Some(previous) = self.appearance_for_call(previous_call_id).cloned()
        {
            effects.push(appearance_state_effect(
                &previous,
                HandsetCallState::Connected,
                false,
            ));
            if previous.state == CallState::Connected {
                effects.push(
                    HandsetEffect::BeginMedia {
                        device_id: previous.device_id,
                        call_id: previous.sccp_id,
                        codec: previous.codec,
                    }
                    .into(),
                );
            }
        }
        debug_assert!(self.invariant_error().is_none());
        effects
    }

    /// Compensate one external effect that completed after a lifecycle event
    /// had already cancelled its transition. The controller has already
    /// restored or removed the affected calls, so this applies only the exact
    /// inverse still meaningful against the surviving call domain.
    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
    pub(crate) fn compensate_unrecorded_call_transition_effect(
        &mut self,
        transition: &CallTransition,
        effect: &DriverEffect,
    ) -> CallTransitionCompensation {
        if self.pending_call_transitions.contains_key(&transition.id) {
            return CallTransitionCompensation::default();
        }

        let mut compensation = CallTransitionCompensation::default();
        match effect {
            DriverEffect::Backend(PbxEffect::Hold { call_id })
                if Some(*call_id) == transition.previous_pbx_id
                    && self.call_registry.pbx.contains_key(call_id) =>
            {
                compensation
                    .effects
                    .push(PbxEffect::Resume { call_id: *call_id }.into());
            }
            DriverEffect::Backend(PbxEffect::CreateChannel { call_id, .. })
                if *call_id == transition.target_pbx_id =>
            {
                compensation
                    .effects
                    .push(PbxEffect::Hangup { call_id: *call_id }.into());
                compensation.remove_target_channel = true;
            }
            DriverEffect::Backend(PbxEffect::StartRouting { call_id, .. })
                if *call_id == transition.target_pbx_id =>
            {
                compensation
                    .effects
                    .push(PbxEffect::Hangup { call_id: *call_id }.into());
                compensation.remove_target_channel = true;
            }
            DriverEffect::Backend(PbxEffect::Answer { call_id })
                if *call_id == transition.target_pbx_id
                    && self.call_registry.pbx.contains_key(call_id) =>
            {
                compensation
                    .effects
                    .push(PbxEffect::Hangup { call_id: *call_id }.into());
                if let Some(mut outcome) = self.pbx_hangup_with_effects(*call_id) {
                    compensation.effects.append(&mut outcome.effects);
                }
                compensation.remove_target_channel = true;
            }
            DriverEffect::Backend(PbxEffect::Resume { call_id })
                if *call_id == transition.target_pbx_id
                    && self.call_registry.pbx.contains_key(call_id) =>
            {
                compensation
                    .effects
                    .push(PbxEffect::Hold { call_id: *call_id }.into());
            }
            DriverEffect::Handset(HandsetEffect::SetCallState {
                call_id,
                state: HandsetCallState::Hold,
                ..
            }) if Some(*call_id) == transition.previous_call_id => {
                if let Some(previous) = self.appearance_for_call(*call_id).cloned() {
                    compensation.effects.push(appearance_state_effect(
                        &previous,
                        HandsetCallState::Connected,
                        false,
                    ));
                    if previous.state == CallState::Connected {
                        compensation.effects.push(
                            HandsetEffect::BeginMedia {
                                device_id: previous.device_id,
                                call_id: previous.sccp_id,
                                codec: previous.codec,
                            }
                            .into(),
                        );
                    }
                }
            }
            DriverEffect::Handset(HandsetEffect::SetMicrophoneMode {
                device_id,
                call_id,
                enabled: false,
            }) if *call_id == transition.target_call_id => {
                compensation.effects.push(
                    HandsetEffect::SetMicrophoneMode {
                        device_id: device_id.clone(),
                        call_id: *call_id,
                        enabled: true,
                    }
                    .into(),
                );
            }
            DriverEffect::Handset(handset)
                if handset.transition_call_id() == Some(transition.target_call_id) =>
            {
                compensation
                    .effects
                    .extend(self.restored_target_handset_effects(transition));
            }
            _ => {}
        }
        compensation
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
    fn restored_target_handset_effects(&self, transition: &CallTransition) -> Vec<DriverEffect> {
        let Some(appearance) = self.appearance_for_call(transition.target_call_id).cloned() else {
            return vec![
                HandsetEffect::SetCallState {
                    device_id: transition.device_id.clone(),
                    call_id: transition.target_call_id,
                    state: HandsetCallState::OnHook,
                    stop_media: true,
                }
                .into(),
            ];
        };
        let (state, stop_media) = match appearance.state {
            CallState::Ringing => (self.inbound_offer_for_appearance(&appearance).state, false),
            CallState::Held => (HandsetCallState::Hold, true),
            CallState::SharedHeld => (HandsetCallState::HoldRed, true),
            CallState::Connected => (HandsetCallState::Connected, false),
            CallState::RemoteInUse => (HandsetCallState::RemoteMultiline, true),
            _ => (HandsetCallState::OnHook, true),
        };
        vec![appearance_state_effect(&appearance, state, stop_media)]
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
    fn abort_call_transitions_for_pbx(&mut self, pbx_id: PbxCallId) -> Vec<DriverEffect> {
        let pending = self
            .pending_call_transitions
            .iter()
            .filter(|(_, pending)| {
                pending.transition.target_pbx_id == pbx_id
                    || pending.transition.previous_pbx_id == Some(pbx_id)
            })
            .map(|(id, pending)| (*id, pending.progress.clone()))
            .collect::<Vec<_>>();
        let mut effects = Vec::new();
        for (id, progress) in pending {
            effects.extend(
                self.abort_call_transition(id, &progress)
                    .into_iter()
                    .filter(|effect| {
                        !matches!(
                            effect,
                            DriverEffect::Backend(
                                PbxEffect::Hold { call_id }
                                    | PbxEffect::Resume { call_id }
                                    | PbxEffect::Answer { call_id }
                                    | PbxEffect::Hangup { call_id }
                            )
                                if *call_id == pbx_id
                        )
                    }),
            );
        }
        effects
    }

    /// Commit a configured destination-based conference request for an
    /// existing pre-dial handset call. Any ordinary connected call on the same
    /// handset is held first; an active ad-hoc conference is never modified.
    pub fn begin_conference_destination(
        &mut self,
        request: ConferenceDestinationRequest,
    ) -> Result<Vec<DriverEffect>, ConferenceDestinationRejection> {
        if request.destination.trim().is_empty() {
            return Err(ConferenceDestinationRejection::Unavailable);
        }
        let target = self
            .appearance_for_call(request.handset_call_id)
            .cloned()
            .ok_or(ConferenceDestinationRejection::Unavailable)?;
        if target.device_id != request.device_id {
            return Err(ConferenceDestinationRejection::Unavailable);
        }
        let target_call = self
            .call_registry
            .pbx
            .get(&target.pbx_id)
            .ok_or(ConferenceDestinationRejection::Unavailable)?;
        if target.state != CallState::Collecting
            || target_call.state != CallState::Collecting
            || !target_call.digits.is_empty()
            || target_call.active_appearance != Some(target.id)
        {
            return Err(ConferenceDestinationRejection::Conflict);
        }
        if self.conferences.by_consultation.values().any(|session| {
            session
                .participants
                .iter()
                .any(|participant| participant.device_id == target.device_id)
        }) {
            return Err(ConferenceDestinationRejection::Conflict);
        }

        let mut ordinary_connected = self
            .call_registry
            .by_device
            .get(&target.device_id)
            .into_iter()
            .flatten()
            .filter_map(|appearance_id| self.call_registry.appearances.get(appearance_id))
            .filter(|appearance| appearance.id != target.id)
            .filter(|appearance| appearance.state == CallState::Connected)
            .filter(|appearance| !self.conferences.by_pbx.contains_key(&appearance.pbx_id))
            .filter(|appearance| {
                self.call_registry
                    .pbx
                    .get(&appearance.pbx_id)
                    .is_some_and(|call| call.active_appearance == Some(appearance.id))
            })
            .map(|appearance| (appearance.pbx_id, appearance.sccp_id))
            .collect::<Vec<_>>();
        ordinary_connected.sort_by_key(|(pbx_id, call_id)| (pbx_id.0, call_id.0));
        ordinary_connected.dedup_by_key(|(pbx_id, _)| *pbx_id);
        if ordinary_connected
            .iter()
            .any(|(pbx_id, _)| self.redirect_claims.contains(pbx_id))
        {
            return Err(ConferenceDestinationRejection::Conflict);
        }

        let mutation = self
            .allocate_conference_mutation(ConferenceMutationOwner::Destination(target.pbx_id))
            .ok_or(ConferenceDestinationRejection::Conflict)?;

        let held_calls = ordinary_connected
            .iter()
            .map(|(pbx_id, _)| *pbx_id)
            .collect::<Vec<_>>();
        let mut effects = Vec::new();
        for (_, call_id) in ordinary_connected {
            effects.extend(self.hold(call_id));
        }

        let info = {
            let appearance = self
                .call_registry
                .appearances
                .get_mut(&target.id)
                .ok_or(ConferenceDestinationRejection::Unavailable)?;
            appearance.state = CallState::Calling;
            appearance.info.called_name = "Conference".into();
            appearance
                .info
                .called_number
                .clone_from(&request.destination);
            appearance.info.clone()
        };
        let call = self
            .call_registry
            .pbx
            .get_mut(&target.pbx_id)
            .ok_or(ConferenceDestinationRejection::Unavailable)?;
        call.state = CallState::Calling;
        call.digit_deadline = None;
        call.last_digit_at = None;
        debug_assert!(self.invariant_error().is_none());

        effects.extend([
            HandsetEffect::SetCallInfo {
                device_id: target.device_id.clone(),
                call_id: request.handset_call_id,
                info,
            }
            .into(),
            HandsetEffect::StartTone {
                device_id: target.device_id.clone(),
                call_id: request.handset_call_id,
                tone: Tone::Silence,
            }
            .into(),
            HandsetEffect::SetCallState {
                device_id: target.device_id,
                call_id: request.handset_call_id,
                state: HandsetCallState::Proceed,
                stop_media: false,
            }
            .into(),
            PbxEffect::StartConferenceDestination {
                operation: ConferenceDestinationOperation {
                    call_id: target.pbx_id,
                    destination: request.destination,
                    application_options: request.application_options,
                    handset_call_id: request.handset_call_id,
                    held_calls,
                    mutation,
                },
            }
            .into(),
        ]);
        Ok(effects)
    }

    /// Roll back a destination-conference launch after effect execution
    /// failed. Calls whose PBX hold completed are resumed externally; calls
    /// whose hold was never executed are restored only in controller state.
    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
    pub(crate) fn conference_destination_failed(
        &mut self,
        mutation: ConferenceMutationToken,
        handset_call_id: CallId,
        held_calls: &[PbxCallId],
        completed_holds: &[PbxCallId],
    ) -> Vec<DriverEffect> {
        if !self.complete_conference_mutation(mutation) {
            return Vec::new();
        }
        let Some(target) = self.appearance_for_call(handset_call_id).cloned() else {
            return Vec::new();
        };
        if target.state != CallState::Calling {
            return Vec::new();
        }
        let mut effects = self.hangup(handset_call_id);
        let completed = completed_holds.iter().copied().collect::<HashSet<_>>();
        for pbx_id in held_calls {
            let handset_call_id = self.call_registry.pbx.get(pbx_id).and_then(|call| {
                call.active_appearance
                    .and_then(|id| self.call_registry.appearances.get(&id))
                    .map(|appearance| appearance.sccp_id)
            });
            let Some(handset_call_id) = handset_call_id else {
                continue;
            };
            let resume = self.resume(handset_call_id);
            if completed.contains(pbx_id) {
                effects.extend(resume);
            }
        }
        debug_assert!(self.invariant_error().is_none());
        effects
    }

    pub fn begin_asterisk_call(
        &mut self,
        sccp_id: CallId,
        pbx_id: PbxCallId,
        binding: &LineBinding,
        codec: Codec,
    ) {
        if self.call_registry.by_sccp.contains_key(&sccp_id)
            || self.call_registry.pbx.contains_key(&pbx_id)
        {
            return;
        }
        self.next_pbx_id = self.next_pbx_id.max(pbx_id.0.saturating_add(1));
        let appearance_id = self.allocate_appearance_id();
        let info = CallInfo {
            direction: CallDirection::Inbound,
            called_name: binding.appearance.display_label().to_owned(),
            called_number: binding.line.number.clone(),
            ..CallInfo::default()
        };
        let pbx_call = PbxCall {
            id: pbx_id,
            line: binding.line.number.clone(),
            context: binding.line.context.clone(),
            direction: CallDirection::Inbound,
            state: CallState::Ringing,
            outbound_phase: None,
            outbound_identity_stage: OutboundIdentityStage::Awaiting,
            digits: String::new(),
            privacy: false,
            metadata: CallMetadata::default(),
            pending_pickup: None,
            appearance_ids: Vec::new(),
            active_appearance: None,
            digit_deadline: None,
            last_digit_at: None,
            simulated_enbloc_eligible: false,
            overlap_enabled: false,
        };
        let appearance = CallAppearance {
            id: appearance_id,
            sccp_id,
            pbx_id,
            device_id: binding.device_id.clone(),
            line_instance: binding.line_instance,
            state: CallState::Ringing,
            ring_mode: binding.appearance.ring_mode,
            privacy: binding.appearance.privacy,
            info,
            codec,
            audio: MediaStreamState::Closed,
            audio_transmit: MediaStreamState::Closed,
            video: VideoMediaState::default(),
            auto_answer_mode: None,
        };
        let inserted = self.insert_pbx_call(pbx_call, appearance);
        debug_assert!(inserted);
        debug_assert!(self.invariant_error().is_none());
    }

    /// Build every currently eligible handset presentation for one inbound
    /// PBX call. Candidate order is preserved so presentation and fallback
    /// ownership are stable across runs.
    pub fn offer_inbound_call(
        &mut self,
        pbx_id: PbxCallId,
        candidates: impl IntoIterator<Item = InboundAppearance>,
    ) -> Vec<InboundOffer> {
        match self.offer_inbound_call_with_policy(pbx_id, candidates) {
            InboundCallDisposition::Offer(offers) => offers,
            InboundCallDisposition::Forward { .. } | InboundCallDisposition::Unavailable(_) => {
                Vec::new()
            }
        }
    }

    /// Apply per-device DND and forwarding before creating the shared handset
    /// presentations for an inbound PBX call.
    ///
    /// A forwarding or rejecting appearance never suppresses a different
    /// appearance that can still ring. A PBX-level forward is selected only
    /// when no handset remains and every forwarding appearance agrees on the
    /// destination.
    pub fn offer_inbound_call_with_policy(
        &mut self,
        pbx_id: PbxCallId,
        candidates: impl IntoIterator<Item = InboundAppearance>,
    ) -> InboundCallDisposition {
        if self.call_registry.pbx.contains_key(&pbx_id) {
            return InboundCallDisposition::Unavailable(InboundUnavailableReason::Conflict);
        }
        let candidates: Vec<_> = candidates.into_iter().collect();
        let Some(line) = candidates
            .first()
            .map(|candidate| candidate.binding.line.number.clone())
        else {
            return InboundCallDisposition::Unavailable(
                InboundUnavailableReason::NoEligibleAppearance,
            );
        };
        let mut seen_calls = HashSet::new();
        let mut seen_buttons = HashSet::new();
        let mut structural_exclusions = 0_usize;
        let mut eligible = Vec::new();
        for candidate in candidates {
            let button = (
                candidate.binding.device_id.clone(),
                candidate.binding.line_instance,
            );
            if candidate.binding.line.number != line
                || !self.devices.contains_key(&candidate.binding.device_id)
                || candidate.binding.appearance.ring_mode == AppearanceRingMode::Disabled
                || self.call_registry.by_sccp.contains_key(&candidate.call_id)
                || !seen_calls.insert(candidate.call_id)
                || !seen_buttons.insert(button)
            {
                structural_exclusions += 1;
                continue;
            }
            eligible.push(candidate);
        }
        let mut ringable = Vec::new();
        let mut forwarded = Vec::new();
        let eligible_count = eligible.len();
        let mut dnd_rejected = 0_usize;
        for mut candidate in eligible {
            let features = self
                .features
                .get(&candidate.binding.device_id)
                .cloned()
                .unwrap_or_default();
            match features.dnd {
                DndMode::Reject => {
                    dnd_rejected += 1;
                    continue;
                }
                DndMode::Silent => {
                    candidate.binding.appearance.ring_mode = AppearanceRingMode::Silent;
                }
                DndMode::Off => {}
            }
            let route = features
                .forwarding
                .all
                .map(|destination| (destination, ForwardingRouteReason::Unconditional))
                .or_else(|| {
                    self.device_is_busy(&candidate.binding.device_id)
                        .then_some(features.forwarding.busy)
                        .flatten()
                        .map(|destination| (destination, ForwardingRouteReason::Busy))
                });
            if let Some((destination, reason)) = route {
                forwarded.push((candidate, destination, reason));
            } else {
                ringable.push(candidate);
            }
        }

        let incoming_limit = self.line_incoming_limits.get(&line).copied().unwrap_or(6);
        let incoming_calls = self
            .call_registry
            .pbx
            .values()
            .filter(|call| call.direction == CallDirection::Inbound && call.line == line)
            .count();

        if ringable.is_empty() {
            let Some((first, destination, reason)) = forwarded.first() else {
                let dnd_only = structural_exclusions == 0
                    && eligible_count != 0
                    && dnd_rejected == eligible_count;
                let reason = if dnd_only && incoming_calls >= incoming_limit as usize {
                    InboundUnavailableReason::IncomingLimit
                } else if dnd_only {
                    InboundUnavailableReason::DoNotDisturb
                } else {
                    InboundUnavailableReason::NoEligibleAppearance
                };
                return InboundCallDisposition::Unavailable(reason);
            };
            if forwarded
                .iter()
                .any(|(_, candidate_destination, candidate_reason)| {
                    candidate_destination != destination || candidate_reason != reason
                })
            {
                return InboundCallDisposition::Unavailable(
                    InboundUnavailableReason::ForwardingConflict,
                );
            }
            return InboundCallDisposition::Forward {
                binding: Box::new(first.binding.clone()),
                destination: destination.clone(),
                reason: *reason,
            };
        }

        if incoming_calls >= incoming_limit as usize {
            return InboundCallDisposition::Unavailable(InboundUnavailableReason::IncomingLimit);
        }

        let Some(first) = ringable.first() else {
            return InboundCallDisposition::Unavailable(
                InboundUnavailableReason::NoEligibleAppearance,
            );
        };

        self.next_pbx_id = self.next_pbx_id.max(pbx_id.0.saturating_add(1));
        let first_appearance_id = self.allocate_appearance_id();
        let call = PbxCall {
            id: pbx_id,
            line,
            context: first.binding.line.context.clone(),
            direction: CallDirection::Inbound,
            state: CallState::Ringing,
            outbound_phase: None,
            outbound_identity_stage: OutboundIdentityStage::Awaiting,
            digits: String::new(),
            privacy: false,
            metadata: CallMetadata::default(),
            pending_pickup: None,
            appearance_ids: Vec::new(),
            active_appearance: None,
            digit_deadline: None,
            last_digit_at: None,
            simulated_enbloc_eligible: false,
            overlap_enabled: false,
        };
        let first_appearance = inbound_call_appearance(first_appearance_id, pbx_id, first);
        if !self.insert_pbx_call(call, first_appearance) {
            return InboundCallDisposition::Unavailable(InboundUnavailableReason::Conflict);
        }

        let mut offers = vec![self.inbound_offer(first)];
        for candidate in &ringable[1..] {
            let appearance_id = self.allocate_appearance_id();
            if self.attach_appearance(inbound_call_appearance(appearance_id, pbx_id, candidate)) {
                offers.push(self.inbound_offer(candidate));
            }
        }
        debug_assert!(self.invariant_error().is_none());
        InboundCallDisposition::Offer(offers)
    }

    /// Starts the configured call-waiting tone for one successfully queued
    /// inbound presentation. Repeats retain the policy captured here so a
    /// reload affects only later waiting calls.
    pub fn start_call_waiting_tone(
        &mut self,
        waiting_call_id: CallId,
        tone: Option<Tone>,
        interval: Duration,
        now: Instant,
    ) -> Vec<DriverEffect> {
        self.call_waiting_tones.remove(&waiting_call_id);
        let Some(tone) = tone else {
            return Vec::new();
        };
        let Some(waiting) = self.appearance_for_call(waiting_call_id) else {
            return Vec::new();
        };
        if waiting.state != CallState::Ringing || waiting.ring_mode != AppearanceRingMode::Normal {
            return Vec::new();
        }
        let device_id = waiting.device_id.clone();
        let Some(active_call_id) = self
            .devices
            .get(&device_id)
            .and_then(|device| device.active_call)
            .filter(|active| *active != waiting_call_id)
        else {
            return Vec::new();
        };
        if !self
            .appearance_for_call(active_call_id)
            .is_some_and(|active| {
                matches!(
                    active.state,
                    CallState::Collecting
                        | CallState::Calling
                        | CallState::Connected
                        | CallState::TransferCollecting
                )
            })
        {
            return Vec::new();
        }
        if !interval.is_zero() {
            self.call_waiting_tones.insert(
                waiting_call_id,
                CallWaitingToneSchedule {
                    device_id: device_id.clone(),
                    waiting_call_id,
                    active_call_id,
                    tone,
                    interval,
                    next_at: now + interval,
                },
            );
        }
        vec![
            HandsetEffect::StartTone {
                device_id,
                call_id: active_call_id,
                tone,
            }
            .into(),
        ]
    }

    /// Emits every due repeat in deterministic waiting-call order. Invalid or
    /// completed schedules are discarded before any handset effect escapes.
    pub fn expire_call_waiting_tones(&mut self, now: Instant) -> Vec<DriverEffect> {
        let mut call_ids = self.call_waiting_tones.keys().copied().collect::<Vec<_>>();
        call_ids.sort_by_key(|call_id| call_id.0);
        let mut effects = Vec::new();
        for waiting_call_id in call_ids {
            let Some(schedule) = self.call_waiting_tones.get(&waiting_call_id).cloned() else {
                continue;
            };
            let valid = self
                .appearance_for_call(schedule.waiting_call_id)
                .is_some_and(|appearance| appearance.state == CallState::Ringing)
                && self
                    .devices
                    .get(&schedule.device_id)
                    .is_some_and(|device| device.active_call == Some(schedule.active_call_id))
                && self
                    .appearance_for_call(schedule.active_call_id)
                    .is_some_and(|appearance| {
                        matches!(
                            appearance.state,
                            CallState::Collecting
                                | CallState::Calling
                                | CallState::Connected
                                | CallState::TransferCollecting
                        )
                    });
            if !valid {
                self.call_waiting_tones.remove(&waiting_call_id);
                continue;
            }
            if schedule.next_at > now {
                continue;
            }
            effects.push(
                HandsetEffect::StartTone {
                    device_id: schedule.device_id,
                    call_id: schedule.active_call_id,
                    tone: schedule.tone,
                }
                .into(),
            );
            if let Some(active) = self.call_waiting_tones.get_mut(&waiting_call_id) {
                active.next_at = now + active.interval;
            }
        }
        effects
    }

    pub fn cancel_call_waiting_tone(&mut self, waiting_call_id: CallId) -> bool {
        self.call_waiting_tones.remove(&waiting_call_id).is_some()
    }

    /// Attach another handset presentation to an existing PBX call.
    ///
    /// Current call setup creates one appearance. Shared-line routing can use
    /// this operation to fan the same PBX identity out to additional devices.
    pub fn add_call_appearance(
        &mut self,
        pbx_id: PbxCallId,
        sccp_id: CallId,
        binding: &LineBinding,
        codec: Codec,
    ) -> Option<CallAppearanceId> {
        let (state, active_appearance, direction, mut info) = self
            .call_registry
            .pbx
            .get(&pbx_id)
            .filter(|call| call.line == binding.line.number)
            .and_then(|call| {
                let info = call
                    .appearance_ids
                    .first()
                    .and_then(|id| self.call_registry.appearances.get(id))?
                    .info
                    .clone();
                Some((call.state, call.active_appearance, call.direction, info))
            })?;
        if self.call_registry.by_sccp.contains_key(&sccp_id) {
            return None;
        }
        let id = self.allocate_appearance_id();
        let state = shared_appearance_state(state, active_appearance.is_some());
        if direction == CallDirection::Inbound {
            info.called_name = binding.appearance.display_label().to_owned();
            info.called_number.clone_from(&binding.line.number);
        }
        let appearance = CallAppearance {
            id,
            sccp_id,
            pbx_id,
            device_id: binding.device_id.clone(),
            line_instance: binding.line_instance,
            state,
            ring_mode: binding.appearance.ring_mode,
            privacy: binding.appearance.privacy,
            info,
            codec,
            audio: MediaStreamState::Closed,
            audio_transmit: MediaStreamState::Closed,
            video: VideoMediaState::default(),
            auto_answer_mode: None,
        };
        self.attach_appearance(appearance).then(|| {
            debug_assert!(self.invariant_error().is_none());
            id
        })
    }

    pub fn digit(&mut self, call_id: CallId, digit: Digit, now: Instant) -> Vec<DriverEffect> {
        let Some(character) = digit_character(digit) else {
            return Vec::new();
        };
        let Some((appearance_state, appearance_pbx_id, device_id)) =
            self.appearance_for_call(call_id).map(|appearance| {
                (
                    appearance.state,
                    appearance.pbx_id,
                    appearance.device_id.clone(),
                )
            })
        else {
            return Vec::new();
        };
        let pbx_id = self
            .barges
            .by_handset
            .get(&call_id)
            .map_or(appearance_pbx_id, |barge| barge.barger_call_id);
        match appearance_state {
            CallState::Collecting | CallState::PickupCollecting | CallState::TransferCollecting => {
                if character == self.dial_terminator {
                    return self.finish_digits(call_id);
                }
                let (overlap, secondary_tone) = {
                    let Some(call) = self.call_registry.pbx.get_mut(&pbx_id) else {
                        return Vec::new();
                    };
                    if let Some(previous) = call.last_digit_at
                        && now.saturating_duration_since(previous) > Duration::from_millis(400)
                    {
                        call.simulated_enbloc_eligible = false;
                    }
                    call.last_digit_at = Some(now);
                    call.digits.push(character);
                    let overlap = if appearance_state == CallState::Collecting
                        && call.overlap_enabled
                        && call.digits.len() == 1
                    {
                        call.state = CallState::Calling;
                        call.digit_deadline = None;
                        Some((call.context.clone(), call.digits.clone()))
                    } else {
                        let timeout = if call.simulated_enbloc_eligible && call.digits.len() >= 4 {
                            self.interdigit.min(Duration::from_secs(2))
                        } else {
                            self.interdigit
                        };
                        call.digit_deadline = (appearance_state != CallState::PickupCollecting)
                            .then_some(now + timeout);
                        None
                    };
                    let secondary_tone = self
                        .line_dial_tones
                        .get(&call.line)
                        .filter(|dial_tones| {
                            dial_tones.secondary_prefix.as_deref() == Some(call.digits.as_str())
                        })
                        .map(|dial_tones| dial_tones.secondary);
                    (overlap, secondary_tone)
                };
                if let Some((context, destination)) = overlap {
                    if let Some(appearance_id) = self.call_registry.by_sccp.get(&call_id).copied()
                        && let Some(appearance) =
                            self.call_registry.appearances.get_mut(&appearance_id)
                    {
                        appearance.state = CallState::Calling;
                    }
                    debug_assert!(self.invariant_error().is_none());
                    let mut effects = Vec::new();
                    if let Some(tone) = secondary_tone {
                        effects.push(
                            HandsetEffect::StartTone {
                                device_id,
                                call_id,
                                tone,
                            }
                            .into(),
                        );
                    }
                    effects.extend(self.outbound_route_presentation(pbx_id, &destination));
                    effects.push(
                        PbxEffect::StartRouting {
                            call_id: pbx_id,
                            context,
                            destination,
                        }
                        .into(),
                    );
                    return effects;
                }
                debug_assert!(self.invariant_error().is_none());
                secondary_tone
                    .map(|tone| {
                        vec![
                            HandsetEffect::StartTone {
                                device_id,
                                call_id,
                                tone,
                            }
                            .into(),
                        ]
                    })
                    .unwrap_or_default()
            }
            CallState::Calling
                if self
                    .call_registry
                    .pbx
                    .get(&pbx_id)
                    .is_some_and(|call| call.overlap_enabled) =>
            {
                vec![
                    PbxEffect::SendDigit {
                        call_id: pbx_id,
                        digit: character,
                    }
                    .into(),
                ]
            }
            CallState::Connected | CallState::Held => vec![
                PbxEffect::SendDigit {
                    call_id: pbx_id,
                    digit: character,
                }
                .into(),
            ],
            _ => Vec::new(),
        }
    }

    pub fn enbloc(&mut self, call_id: CallId, number: String) -> Vec<DriverEffect> {
        let Some((pbx_id, state)) = self
            .appearance_for_call(call_id)
            .map(|appearance| (appearance.pbx_id, appearance.state))
        else {
            return Vec::new();
        };
        if matches!(state, CallState::Connected | CallState::Held) {
            let digits = number
                .chars()
                .map(|character| match character {
                    '0'..='9' | '*' | '#' | 'A'..='D' => Some(character),
                    'a'..='d' => Some(character.to_ascii_uppercase()),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>();
            return digits.map_or_else(Vec::new, |digits| {
                digits
                    .into_iter()
                    .map(|digit| {
                        PbxEffect::SendDigit {
                            call_id: pbx_id,
                            digit,
                        }
                        .into()
                    })
                    .collect()
            });
        }
        if !matches!(
            state,
            CallState::Collecting | CallState::PickupCollecting | CallState::TransferCollecting
        ) {
            return Vec::new();
        }
        // Some phones send the Dial soft key without repeating the digits
        // already delivered as KeypadButton messages.
        if !number.is_empty()
            && let Some(call) = self.call_registry.pbx.get_mut(&pbx_id)
        {
            call.digits = number;
        }
        self.finish_digits(call_id)
    }

    pub fn expire_digits(&mut self, now: Instant) -> Vec<DriverEffect> {
        let expired: Vec<_> = self
            .call_registry
            .pbx
            .values()
            .filter(|call| call.digit_deadline.is_some_and(|deadline| deadline <= now))
            .filter_map(|call| {
                call.appearance_ids
                    .first()
                    .and_then(|id| self.call_registry.appearances.get(id))
                    .map(|appearance| appearance.sccp_id)
            })
            .collect();
        expired
            .into_iter()
            .flat_map(|call| self.finish_digits(call))
            .collect()
    }

    pub fn phone_answer(&mut self, call_id: CallId) -> Vec<DriverEffect> {
        let Some(winner) = self.appearance_for_call(call_id).cloned() else {
            return Vec::new();
        };
        let Some(call) = self.call_registry.pbx.get(&winner.pbx_id) else {
            return Vec::new();
        };
        if self.redirect_claims.contains(&winner.pbx_id)
            || call.state != CallState::Ringing
            || call.active_appearance.is_some()
            || winner.state != CallState::Ringing
        {
            return Vec::new();
        }
        let appearance_ids = call.appearance_ids.clone();
        #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
        self.cancel_auto_answers_for_pbx(winner.pbx_id);
        self.call_waiting_tones.remove(&call_id);
        let previous_active = self
            .devices
            .get(&winner.device_id)
            .and_then(|device| device.active_call)
            .filter(|active| *active != call_id);
        if previous_active.is_some_and(|active| {
            self.call_state(active).is_some_and(|state| {
                matches!(
                    state,
                    CallState::Collecting
                        | CallState::Calling
                        | CallState::Connected
                        | CallState::TransferCollecting
                )
            }) && (self.conference_session(active).is_some()
                || self.barges.by_handset.contains_key(&active))
        }) {
            return Vec::new();
        }
        let winner_privacy = winner.privacy
            || self
                .features
                .get(&winner.device_id)
                .is_some_and(|state| state.privacy);
        let mut effects = previous_active.map_or_else(Vec::new, |active| self.hold(active));
        if let Some(call) = self.call_registry.pbx.get_mut(&winner.pbx_id) {
            call.state = CallState::Connected;
            call.active_appearance = Some(winner.id);
            call.privacy |= winner_privacy;
        }
        for appearance_id in appearance_ids {
            let Some(appearance) = self.call_registry.appearances.get_mut(&appearance_id) else {
                continue;
            };
            if appearance_id == winner.id {
                appearance.state = CallState::Connected;
            } else {
                appearance.state = CallState::RemoteInUse;
                effects.push(appearance_state_effect(
                    appearance,
                    HandsetCallState::RemoteMultiline,
                    false,
                ));
                if let Some(device) = self.devices.get_mut(&appearance.device_id) {
                    device.selected_calls.remove(&appearance.sccp_id);
                }
            }
        }
        effects.push(
            HandsetEffect::BeginAnswerMedia {
                device_id: winner.device_id.clone(),
                call_id: winner.sccp_id,
                codec: winner.codec,
            }
            .into(),
        );
        self.pending_phone_answers
            .insert(winner.sccp_id, winner.pbx_id);
        self.select_line(&winner.device_id, winner.line_instance);
        self.set_active_call(&winner.device_id, Some(call_id));
        self.set_call_selected(&winner.device_id, call_id, true);
        debug_assert!(self.invariant_error().is_none());
        effects
    }

    pub fn pbx_answer(&mut self, pbx_id: PbxCallId) -> Vec<DriverEffect> {
        if self.redirect_claims.contains(&pbx_id) {
            return Vec::new();
        }
        let Some(call) = self.call_registry.pbx.get(&pbx_id) else {
            return Vec::new();
        };
        if call.state == CallState::Connected && call.active_appearance.is_some()
            || call
                .outbound_phase
                .is_some_and(|phase| phase >= OutboundCallPhase::Answered)
        {
            return Vec::new();
        }
        let appearance_ids = call.appearance_ids.clone();
        let Some(winner_id) = call
            .active_appearance
            .or_else(|| appearance_ids.first().copied())
        else {
            return Vec::new();
        };
        let winner_privacy =
            self.call_registry
                .appearances
                .get(&winner_id)
                .is_some_and(|appearance| {
                    appearance.privacy
                        || self
                            .features
                            .get(&appearance.device_id)
                            .is_some_and(|state| state.privacy)
                });
        let winner_direction = self
            .call_registry
            .appearances
            .get(&winner_id)
            .map(|appearance| appearance.info.direction);
        let coupled_media_pending = self
            .call_registry
            .appearances
            .get(&winner_id)
            .is_some_and(|appearance| self.pending_route_media.contains(&appearance.sccp_id));
        let media_state = self.call_registry.appearances.get(&winner_id).map_or(
            MediaStreamState::Closed,
            |appearance| {
                if self.pending_route_media.contains(&appearance.sccp_id) {
                    MediaStreamState::Opening
                } else {
                    appearance.audio
                }
            },
        );
        if let Some(call) = self.call_registry.pbx.get_mut(&pbx_id) {
            call.state = CallState::Connected;
            call.active_appearance = Some(winner_id);
            call.privacy |= winner_privacy;
            if call.direction == CallDirection::Outbound && call.outbound_phase.is_some() {
                call.outbound_phase = Some(OutboundCallPhase::Answered);
            }
        }
        let mut effects = Vec::new();
        for appearance_id in appearance_ids {
            let Some(appearance) = self.call_registry.appearances.get_mut(&appearance_id) else {
                continue;
            };
            if appearance_id == winner_id {
                appearance.state = CallState::Connected;
                if appearance.audio == MediaStreamState::Closed {
                    appearance.audio = MediaStreamState::Opening;
                }
            } else {
                appearance.state = CallState::RemoteInUse;
                effects.push(appearance_state_effect(
                    appearance,
                    HandsetCallState::RemoteMultiline,
                    false,
                ));
                if let Some(device) = self.devices.get_mut(&appearance.device_id) {
                    device.selected_calls.remove(&appearance.sccp_id);
                }
            }
        }
        let Some(winner) = self.call_registry.appearances.get(&winner_id).cloned() else {
            return Vec::new();
        };
        self.select_line(&winner.device_id, winner.line_instance);
        self.set_active_call(&winner.device_id, Some(winner.sccp_id));
        self.set_call_selected(&winner.device_id, winner.sccp_id, true);
        self.advance_transfer_for_pbx(pbx_id, TransferPhase::Connected);
        debug_assert!(self.invariant_error().is_none());
        match media_state {
            MediaStreamState::Open(_) => effects.push(
                HandsetEffect::SetCallState {
                    device_id: winner.device_id,
                    call_id: winner.sccp_id,
                    state: HandsetCallState::Connected,
                    stop_media: false,
                }
                .into(),
            ),
            MediaStreamState::Opening => {
                if winner_direction == Some(CallDirection::Outbound) && !coupled_media_pending {
                    effects.push(
                        HandsetEffect::SetCallState {
                            device_id: winner.device_id,
                            call_id: winner.sccp_id,
                            state: HandsetCallState::Connected,
                            stop_media: false,
                        }
                        .into(),
                    );
                }
            }
            MediaStreamState::Closed => effects.push(
                if winner_direction == Some(CallDirection::Outbound) {
                    HandsetEffect::BeginMedia {
                        device_id: winner.device_id,
                        call_id: winner.sccp_id,
                        codec: winner.codec,
                    }
                } else {
                    HandsetEffect::BeginAnswerMedia {
                        device_id: winner.device_id,
                        call_id: winner.sccp_id,
                        codec: winner.codec,
                    }
                }
                .into(),
            ),
        }
        effects
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
    pub(crate) fn set_auto_answer_request(
        &mut self,
        pbx_id: PbxCallId,
        request: AutoAnswerRequest,
    ) -> bool {
        if self
            .call_registry
            .pbx
            .get(&pbx_id)
            .is_none_or(|call| call.state != CallState::Ringing)
        {
            return false;
        }
        self.auto_answer_requests.insert(pbx_id, request);
        true
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
    pub(crate) fn has_auto_answer_request(&self, pbx_id: PbxCallId) -> bool {
        self.auto_answer_requests.contains_key(&pbx_id)
    }

    /// Capture the current normalized delay/tone only after the adapter has
    /// successfully queued the inbound presentation. Each eligible shared
    /// appearance receives an independent generation; the first valid due
    /// generation claims the PBX call and cancels its peers.
    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
    pub(crate) fn schedule_auto_answers(
        &mut self,
        pbx_id: PbxCallId,
        policy: AutoAnswerPolicy,
        now: Instant,
    ) -> Result<usize, AutoAnswerScheduleRejection> {
        let Some(request) = self.auto_answer_requests.remove(&pbx_id) else {
            return Err(AutoAnswerScheduleRejection::Unavailable);
        };
        self.cancel_auto_answers_for_pbx(pbx_id);
        let mut call_ids = self
            .appearances_for_pbx(pbx_id)
            .filter(|appearance| appearance.state == CallState::Ringing)
            .filter(|appearance| {
                self.device_can_auto_answer(&appearance.device_id, appearance.sccp_id)
            })
            .map(|appearance| appearance.sccp_id)
            .collect::<Vec<_>>();
        call_ids.sort_by_key(|call_id| call_id.0);
        let count = u64::try_from(call_ids.len())
            .map_err(|_| AutoAnswerScheduleRejection::GenerationExhausted)?;
        let next_generation = self
            .next_auto_answer_generation
            .checked_add(count)
            .ok_or(AutoAnswerScheduleRejection::GenerationExhausted)?;
        for call_id in &call_ids {
            let generation = self.next_auto_answer_generation;
            self.next_auto_answer_generation += 1;
            self.pending_auto_answers.insert(
                *call_id,
                PendingAutoAnswer {
                    generation,
                    pbx_id,
                    call_id: *call_id,
                    deadline: now + policy.delay,
                    request,
                    tone: policy.tone,
                },
            );
        }
        debug_assert_eq!(self.next_auto_answer_generation, next_generation);
        Ok(call_ids.len())
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
    pub(crate) fn expire_auto_answers(&mut self, now: Instant) -> Vec<CallTransition> {
        let mut due = self
            .pending_auto_answers
            .values()
            .copied()
            .filter(|pending| pending.deadline <= now)
            .collect::<Vec<_>>();
        due.sort_by_key(|pending| (pending.deadline, pending.generation, pending.call_id.0));
        let mut transitions = Vec::new();
        for pending in due {
            if self
                .pending_auto_answers
                .get(&pending.call_id)
                .is_none_or(|current| current.generation != pending.generation)
            {
                continue;
            }
            self.pending_auto_answers.remove(&pending.call_id);
            let Ok(mut transition) =
                self.begin_active_call_switch_transaction_for_auto_answer(pending)
            else {
                continue;
            };
            transition.effects.push(
                HandsetEffect::StartTone {
                    device_id: transition.device_id.clone(),
                    call_id: transition.target_call_id,
                    tone: pending.tone,
                }
                .into(),
            );
            if transition.auto_answer_mode == Some(AutoAnswerMode::OneWay) {
                transition.effects.push(
                    HandsetEffect::SetMicrophoneMode {
                        device_id: transition.device_id.clone(),
                        call_id: transition.target_call_id,
                        enabled: false,
                    }
                    .into(),
                );
            }
            if let Some(stored) = self.pending_call_transitions.get_mut(&transition.id) {
                stored.transition.clone_from(&transition);
            }
            transitions.push(transition);
        }
        transitions
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
    fn begin_active_call_switch_transaction_for_auto_answer(
        &mut self,
        pending: PendingAutoAnswer,
    ) -> Result<CallTransition, CallSwitchRejection> {
        let appearance = self
            .appearance_for_call(pending.call_id)
            .ok_or(CallSwitchRejection::Unavailable)?;
        if appearance.pbx_id != pending.pbx_id || appearance.state != CallState::Ringing {
            return Err(CallSwitchRejection::Unavailable);
        }
        let device_id = appearance.device_id.clone();
        if !self.device_can_auto_answer(&device_id, pending.call_id) {
            return Err(CallSwitchRejection::Conflict);
        }
        let mut transition =
            self.begin_active_call_switch_transaction(&device_id, pending.call_id)?;
        transition.auto_answer_mode = Some(pending.request.mode);
        for effect in &mut transition.effects {
            if let DriverEffect::Handset(HandsetEffect::BeginAnswerMedia {
                device_id,
                call_id,
                codec,
            }) = effect
                && *call_id == pending.call_id
            {
                *effect = if pending.request.mode == AutoAnswerMode::OneWay {
                    HandsetEffect::BeginOneWayMedia {
                        device_id: device_id.clone(),
                        call_id: *call_id,
                        codec: *codec,
                    }
                    .into()
                } else {
                    HandsetEffect::BeginMedia {
                        device_id: device_id.clone(),
                        call_id: *call_id,
                        codec: *codec,
                    }
                    .into()
                };
            }
        }
        Ok(transition)
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
    fn device_can_auto_answer(&self, device_id: &DeviceId, target: CallId) -> bool {
        self.devices
            .get(device_id)
            .is_some_and(|device| device.active_call.is_none())
            && !self
                .pending_call_transitions
                .values()
                .any(|pending| &pending.transition.device_id == device_id)
            && self.transfers.get(device_id).is_none()
            && !self.conferences.by_consultation.values().any(|session| {
                session
                    .participants
                    .iter()
                    .any(|participant| &participant.device_id == device_id)
            })
            && !self
                .appearances_for_device(device_id)
                .filter(|appearance| appearance.sccp_id != target)
                .any(|appearance| {
                    self.barges.by_handset.contains_key(&appearance.sccp_id)
                        || matches!(
                            appearance.state,
                            CallState::Collecting
                                | CallState::PickupCollecting
                                | CallState::Calling
                                | CallState::Connected
                                | CallState::Parking
                                | CallState::Retrieving
                                | CallState::Barged
                                | CallState::TransferCollecting
                        )
                })
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
    fn cancel_auto_answers_for_pbx(&mut self, pbx_id: PbxCallId) {
        self.auto_answer_requests.remove(&pbx_id);
        self.pending_auto_answers
            .retain(|_, pending| pending.pbx_id != pbx_id);
    }

    /// Publish pre-answer progress and open media once when policy permits it.
    pub fn pbx_progress(&mut self, pbx_id: PbxCallId, early_media: bool) -> Vec<DriverEffect> {
        self.pbx_progress_with_media_mode(pbx_id, early_media, OutboundMediaMode::Staged)
    }

    pub fn pbx_proceeding(&mut self, pbx_id: PbxCallId) -> Vec<DriverEffect> {
        let Some(appearance) =
            self.advance_outbound_call_phase(pbx_id, OutboundCallPhase::Proceeding)
        else {
            return Vec::new();
        };
        vec![
            HandsetEffect::PresentOutboundProceeding {
                device_id: appearance.device_id,
                call_id: appearance.sccp_id,
                info: appearance.info,
            }
            .into(),
        ]
    }

    pub fn pbx_ringing(&mut self, pbx_id: PbxCallId) -> Vec<DriverEffect> {
        let Some(appearance) = self.advance_outbound_call_phase(pbx_id, OutboundCallPhase::Ringing)
        else {
            return Vec::new();
        };
        self.advance_transfer_for_pbx(pbx_id, TransferPhase::Ringing);
        let mut effects = vec![
            HandsetEffect::PresentOutboundRinging {
                device_id: appearance.device_id.clone(),
                call_id: appearance.sccp_id,
                info: appearance.info,
            }
            .into(),
        ];
        effects.extend(self.publish_outbound_ring_out(pbx_id));
        effects
    }

    fn advance_outbound_call_phase(
        &mut self,
        pbx_id: PbxCallId,
        next: OutboundCallPhase,
    ) -> Option<CallAppearance> {
        let appearance_id = {
            let call = self.call_registry.pbx.get(&pbx_id)?;
            if call.direction != CallDirection::Outbound
                || call.state != CallState::Calling
                || call.outbound_phase.is_none_or(|current| current >= next)
            {
                return None;
            }
            call.active_appearance
                .or_else(|| call.appearance_ids.first().copied())?
        };
        let appearance = self.call_registry.appearances.get(&appearance_id)?.clone();
        self.call_registry.pbx.get_mut(&pbx_id)?.outbound_phase = Some(next);
        Some(appearance)
    }

    /// Publish outbound progress with the station's resolved media-opening
    /// strategy. Coupling is a NAT compatibility operation, not the default
    /// early-media transaction.
    pub fn pbx_progress_with_media_mode(
        &mut self,
        pbx_id: PbxCallId,
        early_media: bool,
        outbound_media_mode: OutboundMediaMode,
    ) -> Vec<DriverEffect> {
        let Some(call) = self.call_registry.pbx.get(&pbx_id) else {
            return Vec::new();
        };
        if call.direction != CallDirection::Outbound
            || call.state != CallState::Calling
            || call
                .outbound_phase
                .is_some_and(|phase| phase > OutboundCallPhase::Progress)
        {
            return Vec::new();
        }
        let publish_proceed = call
            .outbound_phase
            .is_none_or(|phase| phase < OutboundCallPhase::Routing);
        let Some(appearance_id) = call
            .active_appearance
            .or_else(|| call.appearance_ids.first().copied())
        else {
            return Vec::new();
        };
        let Some(appearance) = self.call_registry.appearances.get_mut(&appearance_id) else {
            return Vec::new();
        };
        let device_id = appearance.device_id.clone();
        let call_id = appearance.sccp_id;
        let codec = appearance.codec;
        let begin_media = early_media && appearance.audio == MediaStreamState::Closed;
        let coupled = begin_media && outbound_media_mode == OutboundMediaMode::Coupled;
        if begin_media {
            appearance.audio = MediaStreamState::Opening;
            if coupled {
                appearance.audio_transmit = MediaStreamState::Opening;
                self.pending_route_media.insert(call_id);
            }
        }
        if let Some(call) = self.call_registry.pbx.get_mut(&pbx_id) {
            call.outbound_phase = Some(OutboundCallPhase::Progress);
        }
        self.advance_transfer_for_pbx(pbx_id, TransferPhase::Ringing);
        debug_assert!(self.invariant_error().is_none());
        let mut effects = Vec::new();
        if publish_proceed {
            effects.push(
                HandsetEffect::SetCallState {
                    device_id: device_id.clone(),
                    call_id,
                    state: HandsetCallState::Proceed,
                    stop_media: false,
                }
                .into(),
            );
        }
        if coupled {
            effects.push(
                HandsetEffect::BeginOutboundMedia {
                    device_id,
                    call_id,
                    codec,
                }
                .into(),
            );
        } else if begin_media {
            effects.push(
                HandsetEffect::BeginEarlyMedia {
                    device_id,
                    call_id,
                    codec,
                }
                .into(),
            );
        }
        effects
    }

    /// True only while an exact coupled ORC/SMT generation is awaiting its
    /// receive acknowledgement. Protocol state uses this provenance to settle
    /// the transmit side explicitly for firmware which omits a separate SMT
    /// acknowledgement.
    pub fn coupled_outbound_media_pending(&self, call_id: CallId) -> bool {
        self.pending_route_media.contains(&call_id)
    }

    /// Commit a receive acknowledgement only when it came from the device
    /// that owns the call appearance. Session-local call IDs are validated at
    /// the protocol boundary too; retaining the identity check here keeps a
    /// stale or misrouted runtime event from mutating another appearance.
    pub fn media_opened_for_device(
        &mut self,
        device_id: &DeviceId,
        call_id: CallId,
        endpoint: MediaEndpoint,
    ) -> Vec<DriverEffect> {
        if self
            .appearance_for_call(call_id)
            .is_none_or(|appearance| &appearance.device_id != device_id)
        {
            return Vec::new();
        }
        self.media_opened(call_id, endpoint)
    }

    pub fn media_opened(&mut self, call_id: CallId, endpoint: MediaEndpoint) -> Vec<DriverEffect> {
        let outbound_hole_punch = self.pending_route_media.remove(&call_id);
        let Some(appearance_id) = self.call_registry.by_sccp.get(&call_id).copied() else {
            return Vec::new();
        };
        let Some(appearance) = self.call_registry.appearances.get_mut(&appearance_id) else {
            return Vec::new();
        };
        match endpoint.codec.kind() {
            CodecKind::Audio => {
                appearance.audio = MediaStreamState::Open(endpoint);
                if !matches!(appearance.audio_transmit, MediaStreamState::Open(_)) {
                    appearance.audio_transmit = MediaStreamState::Opening;
                }
            }
            CodecKind::Video => return Vec::new(),
            CodecKind::Text | CodecKind::Data | CodecKind::TelephoneEvent | CodecKind::Unknown => {
                return Vec::new();
            }
        }
        let pbx_id = self
            .barges
            .by_handset
            .get(&call_id)
            .map_or(appearance.pbx_id, |barge| barge.barger_call_id);
        let device_id = appearance.device_id.clone();
        let handset_call_id = appearance.sccp_id;
        let codec = appearance.codec;
        let presentation = appearance.clone();
        let pending_answer = self
            .pending_phone_answers
            .remove(&call_id)
            .filter(|pending_pbx_id| *pending_pbx_id == appearance.pbx_id);
        debug_assert!(self.invariant_error().is_none());
        let configure = if outbound_hole_punch {
            PbxEffect::ConfigureMediaOnly {
                call_id: pbx_id,
                codec,
                remote: endpoint,
            }
        } else {
            PbxEffect::ConfigureMedia {
                call_id: pbx_id,
                device_id,
                handset_call_id,
                codec,
                remote: endpoint,
            }
        };
        let mut effects = Vec::new();
        if presentation.info.direction == CallDirection::Outbound {
            if outbound_hole_punch {
                effects.push(
                    HandsetEffect::StartTone {
                        device_id: presentation.device_id.clone(),
                        call_id: presentation.sccp_id,
                        tone: Tone::Silence,
                    }
                    .into(),
                );
            }
            effects.push(
                HandsetEffect::SetCallInfo {
                    device_id: presentation.device_id.clone(),
                    call_id: presentation.sccp_id,
                    info: presentation.info.clone(),
                }
                .into(),
            );
        }
        effects.push(configure.into());
        if let Some(pbx_id) = pending_answer {
            // ConfigureMedia's immediate handset follow-up sends
            // StartMediaTransmission. Only after that succeeds may Asterisk
            // be answered and the full Connected presentation be published.
            effects.push(PbxEffect::Answer { call_id: pbx_id }.into());
        }
        if presentation.state == CallState::Connected
            && (presentation.info.direction == CallDirection::Inbound
                || (presentation.info.direction == CallDirection::Outbound && outbound_hole_punch))
        {
            effects.push(appearance_state_effect(
                &presentation,
                HandsetCallState::Connected,
                false,
            ));
        }
        effects.extend(self.begin_auto_video(call_id));
        effects
    }

    /// Commit a transmit acknowledgement only for its owning handset.
    pub fn media_transmission_started_for_device(
        &mut self,
        device_id: &DeviceId,
        call_id: CallId,
        endpoint: MediaEndpoint,
    ) -> Vec<DriverEffect> {
        if self
            .appearance_for_call(call_id)
            .is_none_or(|appearance| &appearance.device_id != device_id)
        {
            return Vec::new();
        }
        self.media_transmission_started(call_id, endpoint)
    }

    pub fn media_transmission_started(
        &mut self,
        call_id: CallId,
        endpoint: MediaEndpoint,
    ) -> Vec<DriverEffect> {
        if endpoint.codec.kind() != CodecKind::Audio {
            return Vec::new();
        }
        let Some(appearance_id) = self.call_registry.by_sccp.get(&call_id).copied() else {
            return Vec::new();
        };
        let Some(appearance) = self.call_registry.appearances.get_mut(&appearance_id) else {
            return Vec::new();
        };
        if appearance.audio_transmit != MediaStreamState::Opening {
            return Vec::new();
        }
        appearance.audio_transmit = MediaStreamState::Open(endpoint);
        debug_assert!(self.invariant_error().is_none());
        Vec::new()
    }

    fn begin_auto_video(&mut self, call_id: CallId) -> Vec<DriverEffect> {
        let Some(appearance) = self.appearance_for_call(call_id) else {
            return Vec::new();
        };
        let device_id = appearance.device_id.clone();
        let Some(current_generation) = self
            .devices
            .get(&device_id)
            .filter(|device| device.active_call == Some(call_id))
            .map(|device| device.session_generation)
        else {
            return Vec::new();
        };
        let Some(appearance) = self.appearance_for_call_mut(call_id) else {
            return Vec::new();
        };
        let Some(plan) = appearance.video.plan() else {
            return Vec::new();
        };
        if appearance.state != CallState::Connected
            || plan.mode != VideoMode::Auto
            || plan.session_generation != current_generation
            || !appearance.video.begin_receive()
        {
            return Vec::new();
        }
        vec![
            HandsetEffect::OpenVideoReceive {
                device_id,
                call_id,
                session_generation: current_generation,
            }
            .into(),
        ]
    }

    pub fn install_video_plan_for_device(
        &mut self,
        device_id: &DeviceId,
        call_id: CallId,
        plan: VideoPlan,
        readiness: VideoPlanReadiness,
    ) -> bool {
        if self
            .devices
            .get(device_id)
            .is_none_or(|device| device.session_generation != plan.session_generation)
        {
            return false;
        }
        let Some(appearance) = self.appearance_for_call_mut(call_id) else {
            return false;
        };
        if &appearance.device_id != device_id {
            return false;
        }
        appearance.video = match readiness {
            VideoPlanReadiness::Ready => VideoMediaState::ready(plan),
            VideoPlanReadiness::Blocked(reason) => VideoMediaState::blocked(plan, reason),
        };
        true
    }

    pub fn set_video_audio_only_for_device(
        &mut self,
        device_id: &DeviceId,
        session_generation: SessionGeneration,
        call_id: CallId,
        reason: VideoFallbackReason,
    ) -> bool {
        if self
            .devices
            .get(device_id)
            .is_none_or(|device| device.session_generation != session_generation)
        {
            return false;
        }
        let Some(appearance) = self.appearance_for_call_mut(call_id) else {
            return false;
        };
        if &appearance.device_id != device_id {
            return false;
        }
        appearance.video = VideoMediaState::audio_only(reason);
        true
    }

    pub fn video_mode_for_device(
        &mut self,
        device_id: &DeviceId,
        call_id: CallId,
    ) -> Vec<DriverEffect> {
        let Some(device) = self.devices.get(device_id) else {
            return Vec::new();
        };
        let current_generation = device.session_generation;
        if device.active_call != Some(call_id) {
            return Vec::new();
        }
        let Some(appearance) = self.appearance_for_call_mut(call_id) else {
            return Vec::new();
        };
        if &appearance.device_id != device_id
            || appearance.state != CallState::Connected
            || appearance.video.plan().is_none_or(|plan| {
                plan.mode != VideoMode::User || plan.session_generation != current_generation
            })
            || appearance.video.fallback_reason().is_some()
        {
            return Vec::new();
        }
        let Some(session_generation) = appearance.video.plan().map(|plan| plan.session_generation)
        else {
            return Vec::new();
        };
        let effect = if appearance.video.is_idle() {
            if !appearance.video.begin_receive() {
                return Vec::new();
            }
            HandsetEffect::OpenVideoReceive {
                device_id: device_id.clone(),
                call_id,
                session_generation,
            }
        } else {
            appearance.video.close_streams();
            HandsetEffect::StopVideo {
                device_id: device_id.clone(),
                call_id,
                session_generation,
            }
        };
        vec![effect.into()]
    }

    fn video_plan_for_device_matching(
        &self,
        device_id: &DeviceId,
        session_generation: SessionGeneration,
        call_id: CallId,
        state_matches: impl FnOnce(&VideoMediaState) -> bool,
    ) -> Option<&VideoPlan> {
        let device = self.devices.get(device_id)?;
        if device.session_generation != session_generation {
            return None;
        }
        let appearance = self.appearance_for_call(call_id)?;
        if &appearance.device_id != device_id || appearance.state != CallState::Connected {
            return None;
        }
        if !state_matches(&appearance.video) {
            return None;
        }
        appearance
            .video
            .plan()
            .filter(|plan| plan.session_generation == session_generation)
    }

    /// Returns the plan only while its exact receive-open command is pending.
    pub fn opening_video_receive_plan_for_device(
        &self,
        device_id: &DeviceId,
        session_generation: SessionGeneration,
        call_id: CallId,
    ) -> Option<&VideoPlan> {
        self.video_plan_for_device_matching(device_id, session_generation, call_id, |video| {
            video.receive() == VideoStreamState::Opening
        })
    }

    /// Returns the plan only while its exact transmit-start command is pending.
    pub fn opening_video_transmit_plan_for_device(
        &self,
        device_id: &DeviceId,
        session_generation: SessionGeneration,
        call_id: CallId,
    ) -> Option<&VideoPlan> {
        self.video_plan_for_device_matching(device_id, session_generation, call_id, |video| {
            video.transmit() == VideoStreamState::Opening
        })
    }

    pub fn video_receive_opened_for_device(
        &mut self,
        device_id: &DeviceId,
        session_generation: SessionGeneration,
        call_id: CallId,
        codec: Codec,
        endpoint: MediaEndpointAddress,
    ) -> bool {
        let Some(appearance) = self.appearance_for_call_mut(call_id) else {
            return false;
        };
        &appearance.device_id == device_id
            && appearance
                .video
                .plan()
                .is_some_and(|plan| plan.session_generation == session_generation)
            && appearance.video.opened_receive(codec, endpoint)
    }

    pub fn video_transmit_opened_for_device(
        &mut self,
        device_id: &DeviceId,
        session_generation: SessionGeneration,
        call_id: CallId,
        codec: Codec,
        endpoint: MediaEndpointAddress,
        passthrough_party_id: PassthroughPartyId,
    ) -> bool {
        let Some(appearance) = self.appearance_for_call_mut(call_id) else {
            return false;
        };
        &appearance.device_id == device_id
            && appearance
                .video
                .plan()
                .is_some_and(|plan| plan.session_generation == session_generation)
            && appearance
                .video
                .opened_transmit(codec, endpoint, passthrough_party_id)
    }

    pub fn refresh_video_for_pbx(&self, pbx_id: PbxCallId) -> Vec<DriverEffect> {
        let Some(call) = self.call_by_pbx(pbx_id) else {
            return Vec::new();
        };
        let Some(appearance) = self.appearance_for_call(call.sccp_id) else {
            return Vec::new();
        };
        let VideoMediaState::Ready {
            plan,
            transmit: VideoStreamState::Open { .. },
            transmit_token: Some(passthrough_party_id),
            ..
        } = &appearance.video
        else {
            return Vec::new();
        };
        if appearance.state != CallState::Connected
            || self
                .devices
                .get(&appearance.device_id)
                .is_none_or(|device| {
                    device.session_generation != plan.session_generation
                        || device.active_call != Some(call.sccp_id)
                })
        {
            return Vec::new();
        }
        vec![
            HandsetEffect::RefreshVideo {
                device_id: appearance.device_id.clone(),
                call_id: call.sccp_id,
                session_generation: plan.session_generation,
                passthrough_party_id: *passthrough_party_id,
            }
            .into(),
        ]
    }

    pub fn video_refresh_is_current(
        &self,
        device_id: &DeviceId,
        session_generation: SessionGeneration,
        call_id: CallId,
        passthrough_party_id: PassthroughPartyId,
    ) -> bool {
        self.video_plan_for_device_matching(device_id, session_generation, call_id, |video| {
            matches!(
                video,
                VideoMediaState::Ready {
                    transmit: VideoStreamState::Open { .. },
                    transmit_token: Some(token),
                    ..
                } if *token == passthrough_party_id
            )
        })
        .is_some()
    }

    pub fn begin_video_transmit_for_device(
        &mut self,
        device_id: &DeviceId,
        session_generation: SessionGeneration,
        call_id: CallId,
    ) -> Vec<DriverEffect> {
        if self
            .devices
            .get(device_id)
            .is_none_or(|device| device.session_generation != session_generation)
        {
            return Vec::new();
        }
        let Some(appearance) = self.appearance_for_call_mut(call_id) else {
            return Vec::new();
        };
        if &appearance.device_id != device_id
            || appearance.state != CallState::Connected
            || appearance
                .video
                .plan()
                .is_none_or(|plan| plan.session_generation != session_generation)
            || !appearance.video.begin_transmit()
        {
            return Vec::new();
        }
        vec![
            HandsetEffect::StartVideoTransmit {
                device_id: device_id.clone(),
                call_id,
                session_generation,
            }
            .into(),
        ]
    }

    pub fn video_fallback_for_device(
        &mut self,
        device_id: &DeviceId,
        session_generation: SessionGeneration,
        call_id: CallId,
        reason: VideoFallbackReason,
    ) -> VideoFallbackOutcome {
        let Some(appearance) = self.appearance_for_call_mut(call_id) else {
            return VideoFallbackOutcome::Ignored;
        };
        if &appearance.device_id != device_id
            || appearance
                .video
                .plan()
                .is_none_or(|plan| plan.session_generation != session_generation)
            || !appearance.video.accepts_failure(reason)
        {
            return VideoFallbackOutcome::Ignored;
        }
        let was_active = !appearance.video.is_idle();
        appearance.video = VideoMediaState::audio_only(reason);
        VideoFallbackOutcome::Applied {
            cleanup: was_active.then(|| VideoCleanup {
                device_id: device_id.clone(),
                call_id,
                session_generation,
            }),
        }
    }

    pub fn recover_optional_video_effect_failure(
        &mut self,
        effect: &HandsetEffect,
    ) -> Option<Vec<DriverEffect>> {
        match effect {
            HandsetEffect::OpenVideoReceive {
                device_id,
                call_id,
                session_generation,
            } => Some(
                self.video_fallback_for_device(
                    device_id,
                    *session_generation,
                    *call_id,
                    VideoFallbackReason::ReceiveFailed,
                )
                .into_effects(),
            ),
            HandsetEffect::StartVideoTransmit {
                device_id,
                call_id,
                session_generation,
            } => Some(
                self.video_fallback_for_device(
                    device_id,
                    *session_generation,
                    *call_id,
                    VideoFallbackReason::TransmitFailed,
                )
                .into_effects(),
            ),
            HandsetEffect::StopVideo {
                device_id,
                call_id,
                session_generation,
            } => {
                let Some(appearance) = self.appearance_for_call_mut(*call_id) else {
                    return Some(Vec::new());
                };
                if &appearance.device_id == device_id
                    && appearance
                        .video
                        .plan()
                        .is_some_and(|plan| plan.session_generation == *session_generation)
                {
                    appearance.video =
                        VideoMediaState::audio_only(VideoFallbackReason::TransmitFailed);
                }
                Some(Vec::new())
            }
            _ => None,
        }
    }

    /// Marks an acknowledged handset transmit stream as awaiting a peer
    /// retarget acknowledgement. The prior endpoint is returned so an adapter
    /// can restore it if command enqueueing fails.
    pub fn media_retarget_started(&mut self, call_id: CallId) -> Option<MediaEndpoint> {
        let appearance_id = self.call_registry.by_sccp.get(&call_id).copied()?;
        let appearance = self.call_registry.appearances.get_mut(&appearance_id)?;
        let MediaStreamState::Open(previous) = appearance.audio_transmit else {
            return None;
        };
        appearance.audio_transmit = MediaStreamState::Opening;
        debug_assert!(self.invariant_error().is_none());
        Some(previous)
    }

    pub fn media_retarget_enqueue_failed(
        &mut self,
        call_id: CallId,
        previous: MediaEndpoint,
    ) -> bool {
        let Some(appearance_id) = self.call_registry.by_sccp.get(&call_id).copied() else {
            return false;
        };
        let Some(appearance) = self.call_registry.appearances.get_mut(&appearance_id) else {
            return false;
        };
        if appearance.audio_transmit != MediaStreamState::Opening {
            return false;
        }
        appearance.audio_transmit = MediaStreamState::Open(previous);
        debug_assert!(self.invariant_error().is_none());
        true
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
    pub(crate) fn media_retarget_compensation_started(
        &mut self,
        call_id: CallId,
    ) -> Option<MediaStreamState> {
        let appearance_id = self.call_registry.by_sccp.get(&call_id).copied()?;
        let appearance = self.call_registry.appearances.get_mut(&appearance_id)?;
        let previous = appearance.audio_transmit;
        if matches!(previous, MediaStreamState::Closed) {
            return None;
        }
        appearance.audio_transmit = MediaStreamState::Opening;
        Some(previous)
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
    pub(crate) fn media_retarget_compensation_enqueue_failed(
        &mut self,
        call_id: CallId,
        previous: MediaStreamState,
    ) -> bool {
        let Some(appearance_id) = self.call_registry.by_sccp.get(&call_id).copied() else {
            return false;
        };
        let Some(appearance) = self.call_registry.appearances.get_mut(&appearance_id) else {
            return false;
        };
        if appearance.audio_transmit != MediaStreamState::Opening {
            return false;
        }
        appearance.audio_transmit = previous;
        true
    }

    pub fn hold(&mut self, call_id: CallId) -> Vec<DriverEffect> {
        let Some(owner) = self.appearance_for_call(call_id).cloned() else {
            return Vec::new();
        };
        let Some(call) = self.call_registry.pbx.get(&owner.pbx_id) else {
            return Vec::new();
        };
        if self.redirect_claims.contains(&owner.pbx_id)
            || self.barges.groups.contains_key(&owner.pbx_id)
            || self.pending_phone_answers.contains_key(&call_id)
        {
            return Vec::new();
        }
        if !matches!(
            call.state,
            CallState::Collecting
                | CallState::Calling
                | CallState::Connected
                | CallState::TransferCollecting
        ) || call.active_appearance != Some(owner.id)
        {
            return Vec::new();
        }
        let appearance_ids = call.appearance_ids.clone();
        if let Some(call) = self.call_registry.pbx.get_mut(&owner.pbx_id) {
            call.state = CallState::Held;
        }
        self.shared_control_claims.remove(&owner.pbx_id);
        let mut effects = vec![
            PbxEffect::Hold {
                call_id: owner.pbx_id,
            }
            .into(),
        ];
        for appearance_id in appearance_ids {
            let Some(appearance) = self.call_registry.appearances.get_mut(&appearance_id) else {
                continue;
            };
            appearance.audio = MediaStreamState::Closed;
            appearance.audio_transmit = MediaStreamState::Closed;
            effects.extend(
                appearance
                    .video
                    .cleanup(&appearance.device_id, appearance.sccp_id)
                    .map(HandsetEffect::from)
                    .map(DriverEffect::from),
            );
            appearance.video.close_streams();
            if appearance_id == owner.id {
                appearance.state = CallState::Held;
                effects.push(appearance_state_effect(
                    appearance,
                    HandsetCallState::Hold,
                    true,
                ));
            } else {
                appearance.state = CallState::SharedHeld;
                effects.push(appearance_state_effect(
                    appearance,
                    HandsetCallState::HoldRed,
                    false,
                ));
            }
        }
        if self
            .devices
            .get(&owner.device_id)
            .is_some_and(|device| device.active_call == Some(call_id))
        {
            self.set_active_call(&owner.device_id, None);
        }
        self.set_call_selected(&owner.device_id, call_id, false);
        debug_assert!(self.invariant_error().is_none());
        effects
    }

    pub fn resume(&mut self, call_id: CallId) -> Vec<DriverEffect> {
        let Some(requester) = self.appearance_for_call(call_id).cloned() else {
            return Vec::new();
        };
        if self.redirect_claims.contains(&requester.pbx_id)
            || self.conferences.by_pbx.contains_key(&requester.pbx_id)
        {
            return Vec::new();
        }
        let Some(call) = self.call_registry.pbx.get(&requester.pbx_id) else {
            return Vec::new();
        };
        if call.state != CallState::Held
            || !matches!(requester.state, CallState::Held | CallState::SharedHeld)
            || (call.active_appearance != Some(requester.id)
                && !self.shared_control_eligible(&requester))
        {
            return Vec::new();
        }
        let previous_owner = call.active_appearance;
        let appearance_ids = call.appearance_ids.clone();
        let requester_privacy = requester.privacy
            || self
                .features
                .get(&requester.device_id)
                .is_some_and(|state| state.privacy);
        if let Some(call) = self.call_registry.pbx.get_mut(&requester.pbx_id) {
            call.state = CallState::Connected;
            call.active_appearance = Some(requester.id);
            call.privacy |= requester_privacy;
        }
        if previous_owner != Some(requester.id) {
            self.shared_control_claims
                .insert(requester.pbx_id, SharedControlClaim::Steal(requester.id));
        }
        let mut effects = vec![
            PbxEffect::Resume {
                call_id: requester.pbx_id,
            }
            .into(),
        ];
        for appearance_id in appearance_ids {
            let Some(appearance) = self.call_registry.appearances.get_mut(&appearance_id) else {
                continue;
            };
            if appearance_id == requester.id {
                appearance.state = CallState::Connected;
                appearance.audio = MediaStreamState::Opening;
                appearance.audio_transmit = MediaStreamState::Closed;
            } else {
                appearance.state = CallState::RemoteInUse;
                effects.push(appearance_state_effect(
                    appearance,
                    HandsetCallState::RemoteMultiline,
                    previous_owner == Some(appearance_id),
                ));
                if let Some(device) = self.devices.get_mut(&appearance.device_id) {
                    device.selected_calls.remove(&appearance.sccp_id);
                }
            }
        }
        effects.push(
            HandsetEffect::BeginMedia {
                device_id: requester.device_id.clone(),
                call_id: requester.sccp_id,
                codec: requester.codec,
            }
            .into(),
        );
        self.select_line(&requester.device_id, requester.line_instance);
        self.set_active_call(&requester.device_id, Some(call_id));
        self.set_call_selected(&requester.device_id, call_id, true);
        debug_assert!(self.invariant_error().is_none());
        effects
    }

    /// Put a newly-created handset call into directed-pickup digit collection.
    /// The configured context and answer policy are retained until Dial or `#`
    /// completes the request.
    pub fn begin_directed_pickup(
        &mut self,
        call_id: CallId,
        permitted: bool,
        enabled: bool,
        context: String,
        answer: bool,
    ) -> Result<(), PickupRejection> {
        if !enabled {
            return Err(PickupRejection::Disabled);
        }
        if !permitted {
            return Err(PickupRejection::Permission);
        }
        let appearance = self
            .appearance_for_call(call_id)
            .cloned()
            .ok_or(PickupRejection::Unavailable)?;
        let call = self
            .call_registry
            .pbx
            .get(&appearance.pbx_id)
            .ok_or(PickupRejection::Unavailable)?;
        if call.state != CallState::Collecting
            || appearance.state != CallState::Collecting
            || call.active_appearance != Some(appearance.id)
            || call.pending_pickup.is_some()
            || !self.devices.contains_key(&appearance.device_id)
        {
            return Err(PickupRejection::Conflict);
        }
        if context.trim().is_empty() {
            return Err(PickupRejection::Unavailable);
        }
        if let Some(call) = self.call_registry.pbx.get_mut(&appearance.pbx_id) {
            call.state = CallState::PickupCollecting;
            call.digits.clear();
            call.digit_deadline = None;
            call.last_digit_at = None;
            call.simulated_enbloc_eligible = false;
            call.pending_pickup = Some(PendingDirectedPickup { context, answer });
        }
        if let Some(appearance) = self.call_registry.appearances.get_mut(&appearance.id) {
            appearance.state = CallState::PickupCollecting;
        }
        debug_assert!(self.invariant_error().is_none());
        Ok(())
    }

    /// Attempt the oldest ringing call permitted by this channel's configured
    /// numeric or named pickup groups.
    pub fn group_pickup(
        &mut self,
        call_id: CallId,
        permitted: bool,
        answer: bool,
    ) -> Result<Vec<DriverEffect>, PickupRejection> {
        if !permitted {
            return Err(PickupRejection::Permission);
        }
        let appearance = self
            .appearance_for_call(call_id)
            .cloned()
            .ok_or(PickupRejection::Unavailable)?;
        let call = self
            .call_registry
            .pbx
            .get(&appearance.pbx_id)
            .ok_or(PickupRejection::Unavailable)?;
        if call.state != CallState::Collecting
            || appearance.state != CallState::Collecting
            || call.active_appearance != Some(appearance.id)
            || call.pending_pickup.is_some()
            || !self.devices.contains_key(&appearance.device_id)
        {
            return Err(PickupRejection::Conflict);
        }
        let pbx_id = appearance.pbx_id;
        if let Some(call) = self.call_registry.pbx.get_mut(&pbx_id) {
            call.state = if answer {
                CallState::Connected
            } else {
                CallState::Ringing
            };
            call.active_appearance = answer.then_some(appearance.id);
            call.digit_deadline = None;
        }
        if let Some(stored) = self.call_registry.appearances.get_mut(&appearance.id) {
            stored.state = if answer {
                CallState::Connected
            } else {
                CallState::Ringing
            };
            stored.audio = if answer {
                MediaStreamState::Opening
            } else {
                MediaStreamState::Closed
            };
        }
        debug_assert!(self.invariant_error().is_none());
        Ok(vec![
            PbxEffect::Pickup {
                operation: PickupOperation::Group {
                    call_id: pbx_id,
                    device_id: appearance.device_id,
                    handset_call_id: appearance.sccp_id,
                    codec: appearance.codec,
                    answer,
                },
            }
            .into(),
        ])
    }

    /// Start parking the active PBX call owned by this handset appearance.
    /// The final assigned slot arrives asynchronously from the backend.
    pub fn park(
        &mut self,
        call_id: CallId,
        enabled: bool,
        lot: Option<String>,
    ) -> Result<Vec<DriverEffect>, ParkingRejection> {
        if !enabled {
            return Err(ParkingRejection::Disabled);
        }
        let appearance = self
            .appearance_for_call(call_id)
            .cloned()
            .ok_or(ParkingRejection::Unavailable)?;
        let call = self
            .call_registry
            .pbx
            .get(&appearance.pbx_id)
            .ok_or(ParkingRejection::Unavailable)?;
        if self.redirect_claims.contains(&appearance.pbx_id)
            || call.state != CallState::Connected
            || appearance.state != CallState::Connected
            || call.active_appearance != Some(appearance.id)
        {
            return Err(ParkingRejection::Conflict);
        }
        if let Some(call) = self.call_registry.pbx.get_mut(&appearance.pbx_id) {
            call.state = CallState::Parking;
        }
        if let Some(appearance) = self.call_registry.appearances.get_mut(&appearance.id) {
            appearance.state = CallState::Parking;
        }
        debug_assert!(self.invariant_error().is_none());
        Ok(vec![
            PbxEffect::Parking {
                operation: ParkingOperation::Park {
                    call_id: appearance.pbx_id,
                    lot,
                },
            }
            .into(),
        ])
    }

    /// Roll back a synchronous or timed-out parking attempt without changing
    /// call ownership or allocating another PBX identity.
    pub fn parking_failed(&mut self, call_id: CallId) -> Vec<DriverEffect> {
        let Some(appearance) = self.appearance_for_call(call_id).cloned() else {
            return Vec::new();
        };
        if self
            .call_registry
            .pbx
            .get(&appearance.pbx_id)
            .is_none_or(|call| call.state != CallState::Parking)
        {
            return Vec::new();
        }
        if let Some(call) = self.call_registry.pbx.get_mut(&appearance.pbx_id) {
            call.state = CallState::Connected;
        }
        if let Some(appearance) = self.call_registry.appearances.get_mut(&appearance.id) {
            appearance.state = CallState::Connected;
        }
        debug_assert!(self.invariant_error().is_none());
        vec![
            HandsetEffect::SetCallState {
                device_id: appearance.device_id,
                call_id,
                state: HandsetCallState::Connected,
                stop_media: false,
            }
            .into(),
        ]
    }

    /// Publish the assigned slot before closing the owner's now-parked SCCP
    /// channel. The backend retains the parked peer independently.
    pub fn parking_confirmed(&mut self, call_id: CallId, slot: u32) -> Vec<DriverEffect> {
        let Some(appearance) = self.appearance_for_call(call_id).cloned() else {
            return Vec::new();
        };
        if self
            .call_registry
            .pbx
            .get(&appearance.pbx_id)
            .is_none_or(|call| call.state != CallState::Parking)
        {
            return Vec::new();
        }
        let mut effects = vec![
            HandsetEffect::SetCallInfo {
                device_id: appearance.device_id.clone(),
                call_id,
                info: CallInfo {
                    direction: CallDirection::Outbound,
                    calling_name: String::new(),
                    calling_number: String::new(),
                    called_name: "Parked".into(),
                    called_number: slot.to_string(),
                    ..CallInfo::default()
                },
            }
            .into(),
            HandsetEffect::SetCallState {
                device_id: appearance.device_id,
                call_id,
                state: HandsetCallState::Park,
                stop_media: true,
            }
            .into(),
        ];
        effects.extend(self.hangup(call_id));
        effects
    }

    /// Create the retriever's PBX channel and invoke the backend parking
    /// application. The registry claim is owned by the adapter so competing
    /// handsets cannot create a second retriever.
    pub fn begin_parking_retrieval(
        &mut self,
        call_id: CallId,
        binding: LineBinding,
        codec: Codec,
        lot: Option<String>,
        slot: u32,
        info: CallInfo,
    ) -> Result<Vec<DriverEffect>, ParkingRejection> {
        if self.call_registry.by_sccp.contains_key(&call_id)
            || !self.devices.contains_key(&binding.device_id)
            || slot == 0
        {
            return Err(ParkingRejection::Conflict);
        }
        let pbx_id = self.allocate_pbx_id();
        let appearance_id = self.allocate_appearance_id();
        let device_id = binding.device_id.clone();
        let line_instance = binding.line_instance;
        let pbx_call = PbxCall {
            id: pbx_id,
            line: binding.line.number.clone(),
            context: binding.line.context.clone(),
            direction: CallDirection::Outbound,
            state: CallState::Retrieving,
            outbound_phase: None,
            outbound_identity_stage: OutboundIdentityStage::Awaiting,
            digits: String::new(),
            privacy: true,
            metadata: CallMetadata::default(),
            pending_pickup: None,
            appearance_ids: Vec::new(),
            active_appearance: Some(appearance_id),
            digit_deadline: None,
            last_digit_at: None,
            simulated_enbloc_eligible: false,
            overlap_enabled: false,
        };
        let appearance = CallAppearance {
            id: appearance_id,
            sccp_id: call_id,
            pbx_id,
            device_id: device_id.clone(),
            line_instance,
            state: CallState::Retrieving,
            ring_mode: binding.appearance.ring_mode,
            privacy: true,
            info: info.clone(),
            codec,
            audio: MediaStreamState::Closed,
            audio_transmit: MediaStreamState::Closed,
            video: VideoMediaState::default(),
            auto_answer_mode: None,
        };
        if !self.insert_pbx_call(pbx_call, appearance) {
            return Err(ParkingRejection::Conflict);
        }
        self.select_line(&device_id, line_instance);
        self.set_call_selected(&device_id, call_id, true);
        debug_assert!(self.invariant_error().is_none());
        Ok(vec![
            PbxEffect::CreateChannel {
                handset_call_id: call_id,
                call_id: pbx_id,
                binding: Box::new(binding),
                codec,
            }
            .into(),
            HandsetEffect::SetCallInfo {
                device_id,
                call_id,
                info,
            }
            .into(),
            PbxEffect::Parking {
                operation: ParkingOperation::Retrieve {
                    call_id: pbx_id,
                    lot,
                    slot: slot.to_string(),
                },
            }
            .into(),
        ])
    }

    /// Finish retrieval after the backend reports that this claimant won.
    pub fn parking_retrieved(&mut self, call_id: CallId) -> Vec<DriverEffect> {
        let Some(appearance) = self.appearance_for_call(call_id).cloned() else {
            return Vec::new();
        };
        if self
            .call_registry
            .pbx
            .get(&appearance.pbx_id)
            .is_none_or(|call| call.state != CallState::Retrieving)
        {
            return Vec::new();
        }
        if let Some(call) = self.call_registry.pbx.get_mut(&appearance.pbx_id) {
            call.state = CallState::Connected;
        }
        if let Some(stored) = self.call_registry.appearances.get_mut(&appearance.id) {
            stored.state = CallState::Connected;
            stored.audio = MediaStreamState::Opening;
        }
        debug_assert!(self.invariant_error().is_none());
        vec![
            HandsetEffect::SetCallState {
                device_id: appearance.device_id.clone(),
                call_id,
                state: HandsetCallState::Connected,
                stop_media: false,
            }
            .into(),
            HandsetEffect::BeginMedia {
                device_id: appearance.device_id,
                call_id,
                codec: appearance.codec,
            }
            .into(),
        ]
    }

    pub fn parking_retrieval_failed(&mut self, call_id: CallId) -> Vec<DriverEffect> {
        if self
            .appearance_for_call(call_id)
            .is_some_and(|appearance| appearance.state == CallState::Retrieving)
        {
            self.hangup(call_id)
        } else {
            Vec::new()
        }
    }

    /// Move an active shared call to another registered presentation without
    /// answering or replacing the PBX channel.
    pub fn steal(&mut self, call_id: CallId) -> Vec<DriverEffect> {
        let Some(requester) = self.appearance_for_call(call_id).cloned() else {
            return Vec::new();
        };
        let Some(call) = self.call_registry.pbx.get(&requester.pbx_id) else {
            return Vec::new();
        };
        if self.redirect_claims.contains(&requester.pbx_id)
            || self
                .pending_phone_answers
                .values()
                .any(|pending_pbx_id| *pending_pbx_id == requester.pbx_id)
            || call.state != CallState::Connected
            || requester.state != CallState::RemoteInUse
            || call.active_appearance == Some(requester.id)
            || !self.shared_control_eligible(&requester)
        {
            return Vec::new();
        }
        let previous_owner = call.active_appearance;
        let appearance_ids = call.appearance_ids.clone();
        let requester_privacy = requester.privacy
            || self
                .features
                .get(&requester.device_id)
                .is_some_and(|state| state.privacy);
        if let Some(call) = self.call_registry.pbx.get_mut(&requester.pbx_id) {
            call.active_appearance = Some(requester.id);
            call.privacy |= requester_privacy;
        }
        self.shared_control_claims
            .insert(requester.pbx_id, SharedControlClaim::Steal(requester.id));
        let mut effects = Vec::new();
        for appearance_id in appearance_ids {
            let Some(appearance) = self.call_registry.appearances.get_mut(&appearance_id) else {
                continue;
            };
            if appearance_id == requester.id {
                appearance.state = CallState::Connected;
            } else {
                appearance.state = CallState::RemoteInUse;
                if previous_owner == Some(appearance_id) {
                    effects.push(appearance_state_effect(
                        appearance,
                        HandsetCallState::RemoteMultiline,
                        true,
                    ));
                }
                if let Some(device) = self.devices.get_mut(&appearance.device_id) {
                    device.selected_calls.remove(&appearance.sccp_id);
                }
            }
        }
        effects.push(
            HandsetEffect::BeginMedia {
                device_id: requester.device_id.clone(),
                call_id: requester.sccp_id,
                codec: requester.codec,
            }
            .into(),
        );
        self.select_line(&requester.device_id, requester.line_instance);
        self.set_call_selected(&requester.device_id, call_id, true);
        debug_assert!(self.invariant_error().is_none());
        effects
    }

    /// Attach a remote shared-line appearance to the target call through a
    /// separate PBX media channel. The first serialized steal or barge claim
    /// wins; conference barges may add more participants to the winning
    /// conference bridge.
    pub fn barge(
        &mut self,
        call_id: CallId,
        binding: LineBinding,
        codec: Codec,
        mode: BargeMode,
    ) -> Result<Vec<DriverEffect>, BargeRejection> {
        let requester = self
            .appearance_for_call(call_id)
            .cloned()
            .ok_or(BargeRejection::Unavailable)?;
        let target = self
            .call_registry
            .pbx
            .get(&requester.pbx_id)
            .cloned()
            .ok_or(BargeRejection::Unavailable)?;
        if requester.state != CallState::RemoteInUse
            || target.state != CallState::Connected
            || target.active_appearance == Some(requester.id)
        {
            return Err(BargeRejection::NotRemote);
        }
        if target.privacy {
            return Err(BargeRejection::Private);
        }
        if requester.ring_mode == AppearanceRingMode::Disabled
            || binding.device_id != requester.device_id
            || binding.line_instance != requester.line_instance
            || binding.line.number != target.line
            || !self.devices.contains_key(&requester.device_id)
        {
            return Err(BargeRejection::Unavailable);
        }
        let owner = target
            .active_appearance
            .and_then(|id| self.call_registry.appearances.get(&id))
            .ok_or(BargeRejection::Unavailable)?;
        if !self.device_supports_codec(&requester.device_id, codec)
            || !self.device_supports_codec(&owner.device_id, owner.codec)
        {
            return Err(BargeRejection::Capability);
        }
        if self.barges.by_handset.contains_key(&call_id) {
            return Err(BargeRejection::AlreadyBarged);
        }
        if self.redirect_claims.contains(&requester.pbx_id) {
            return Err(BargeRejection::Conflict);
        }

        let (bridge_id, first_participant) =
            match self.shared_control_claims.get(&target.id).copied() {
                Some(SharedControlClaim::Steal(_)) => return Err(BargeRejection::Conflict),
                Some(SharedControlClaim::Barge(bridge_id)) => {
                    let group = self
                        .barges
                        .groups
                        .get(&target.id)
                        .ok_or(BargeRejection::Conflict)?;
                    if mode != BargeMode::Conference || group.mode != BargeMode::Conference {
                        return Err(BargeRejection::AlreadyBarged);
                    }
                    (bridge_id, false)
                }
                None => (self.allocate_bridge_id(), true),
            };

        let barger_call_id = self.allocate_pbx_id();
        self.call_registry.pbx.insert(
            barger_call_id,
            PbxCall {
                id: barger_call_id,
                line: target.line.clone(),
                context: target.context.clone(),
                direction: CallDirection::Outbound,
                state: CallState::Connected,
                outbound_phase: None,
                outbound_identity_stage: OutboundIdentityStage::Awaiting,
                digits: String::new(),
                privacy: true,
                metadata: CallMetadata::default(),
                pending_pickup: None,
                appearance_ids: Vec::new(),
                active_appearance: None,
                digit_deadline: None,
                last_digit_at: None,
                simulated_enbloc_eligible: false,
                overlap_enabled: false,
            },
        );
        if let Some(appearance) = self.call_registry.appearances.get_mut(&requester.id) {
            appearance.state = CallState::Barged;
            appearance.codec = codec;
            appearance.audio = MediaStreamState::Opening;
            appearance.video.close_streams();
        }
        let session = BargeSession {
            target_call_id: target.id,
            barger_call_id,
            bridge_id,
            handset_call_id: call_id,
            mode,
        };
        self.barges.by_handset.insert(call_id, session);
        self.barges.by_pbx.insert(barger_call_id, call_id);
        if first_participant {
            self.shared_control_claims
                .insert(target.id, SharedControlClaim::Barge(bridge_id));
            self.barges.groups.insert(
                target.id,
                BargeGroup {
                    bridge_id,
                    mode,
                    members: vec![call_id],
                },
            );
        } else if let Some(group) = self.barges.groups.get_mut(&target.id) {
            group.members.push(call_id);
        }
        self.select_line(&requester.device_id, requester.line_instance);
        self.set_call_selected(&requester.device_id, call_id, true);
        debug_assert!(self.invariant_error().is_none());

        Ok(vec![
            PbxEffect::CreateChannel {
                handset_call_id: call_id,
                call_id: barger_call_id,
                binding: Box::new(binding),
                codec,
            }
            .into(),
            PbxEffect::Barge {
                operation: BargeOperation::Join {
                    bridge_id,
                    target_call_id: target.id,
                    barger_call_id,
                },
            }
            .into(),
            HandsetEffect::BeginMedia {
                device_id: requester.device_id,
                call_id,
                codec,
            }
            .into(),
        ])
    }

    pub fn barge_session(&self, call_id: CallId) -> Option<&BargeSession> {
        self.barges.by_handset.get(&call_id)
    }

    pub fn barge_session_by_pbx(&self, pbx_id: PbxCallId) -> Option<&BargeSession> {
        self.barges
            .by_pbx
            .get(&pbx_id)
            .and_then(|call_id| self.barges.by_handset.get(call_id))
    }

    /// Roll back a failed adapter operation. `bridge_joined` and
    /// `channel_created` describe which preceding effects completed.
    pub fn abort_barge(
        &mut self,
        call_id: CallId,
        bridge_joined: bool,
        channel_created: bool,
    ) -> Vec<DriverEffect> {
        self.end_barge(call_id, bridge_joined, channel_created)
    }

    /// Create a conference from locally eligible calls. Two or more selected
    /// calls are an exact set; otherwise every eligible call on the initiating
    /// handset is used. The initiating call is always the moderator.
    pub fn join_calls_with_media(
        &mut self,
        device_id: &DeviceId,
        initiating_call_id: CallId,
        permitted: bool,
        media_policy: ConferenceMediaPolicy,
    ) -> Result<Vec<DriverEffect>, ConferenceRejection> {
        let mut effects = self.join_calls(device_id, initiating_call_id, permitted)?;
        if !self.configure_conference_media(initiating_call_id, media_policy) {
            self.abort_join_conference(initiating_call_id, false, &[]);
            return Err(ConferenceRejection::Conflict);
        }
        if let Some(session) = self.conference_session(initiating_call_id) {
            effects.extend(Self::conference_mute_on_entry_effects(
                session,
                session.participants.iter(),
            ));
        }
        Ok(effects)
    }

    pub fn join_calls(
        &mut self,
        device_id: &DeviceId,
        initiating_call_id: CallId,
        permitted: bool,
    ) -> Result<Vec<DriverEffect>, ConferenceRejection> {
        if !permitted {
            return Err(ConferenceRejection::Disabled);
        }
        let Some(device) = self.devices.get(device_id) else {
            return Err(ConferenceRejection::Unavailable);
        };
        let selected = device.selected_calls.clone();
        let mut eligible: Vec<_> = self
            .call_registry
            .appearances
            .values()
            .filter(|appearance| {
                &appearance.device_id == device_id
                    && matches!(appearance.state, CallState::Connected | CallState::Held)
                    && self
                        .call_registry
                        .pbx
                        .get(&appearance.pbx_id)
                        .is_some_and(|call| {
                            matches!(call.state, CallState::Connected | CallState::Held)
                                && call.active_appearance == Some(appearance.id)
                        })
                    && !self.conferences.by_pbx.contains_key(&appearance.pbx_id)
            })
            .cloned()
            .collect();
        eligible.sort_by_key(|appearance| appearance.sccp_id.0);
        if !eligible
            .iter()
            .any(|appearance| appearance.sccp_id == initiating_call_id)
        {
            return Err(ConferenceRejection::NotConnected);
        }

        let selected_eligible: Vec<_> = eligible
            .iter()
            .filter(|appearance| selected.contains(&appearance.sccp_id))
            .cloned()
            .collect();
        let mut chosen = if selected_eligible.len() >= 2 {
            selected_eligible
        } else {
            eligible
        };
        if chosen.len() < 2 {
            return Err(ConferenceRejection::NotConnected);
        }
        if chosen.len() > MAX_CONFERENCE_PARTICIPANTS {
            return Err(ConferenceRejection::Conflict);
        }
        let Some(moderator_index) = chosen
            .iter()
            .position(|appearance| appearance.sccp_id == initiating_call_id)
        else {
            return Err(ConferenceRejection::Conflict);
        };
        let moderator = chosen.remove(moderator_index);
        chosen.insert(0, moderator);

        let participants = ConferenceParticipantRegistry::new(
            chosen
                .iter()
                .enumerate()
                .map(|(index, appearance)| self.conference_participant(appearance, index == 0))
                .collect::<Vec<_>>(),
        )
        .expect("eligible conference calls have unique identities");
        let original = &chosen[0];
        let consultation = &chosen[1];
        let session = ConferenceSession {
            id: self.allocate_conference_id(),
            bridge_id: self.allocate_bridge_id(),
            device_id: device_id.clone(),
            original_handset_call_id: original.sccp_id,
            original_call_id: original.pbx_id,
            consultation_handset_call_id: consultation.sccp_id,
            consultation_call_id: consultation.pbx_id,
            phase: ConferencePhase::Merging,
            origin: ConferenceOrigin::Selection,
            participants,
            media_policy: ConferenceMediaPolicy::default(),
            pending_invite: None,
            pending_participant_mutation: None,
        };
        let key = session.consultation_handset_call_id;
        for participant in session.participants.iter() {
            self.conferences.by_pbx.insert(participant.pbx_call_id, key);
        }
        let resumed: Vec<_> = chosen
            .iter()
            .filter(|appearance| appearance.state == CallState::Held)
            .map(|appearance| appearance.pbx_id)
            .collect();
        let call_ids = session
            .participants
            .iter()
            .map(|participant| participant.pbx_call_id)
            .collect();
        let bridge_id = session.bridge_id;
        self.conferences.by_consultation.insert(key, session);

        let mut effects = vec![
            PbxEffect::Bridge {
                operation: crate::runtime::backend::BridgeOperation::Create { bridge_id },
            }
            .into(),
        ];
        effects.extend(
            resumed
                .into_iter()
                .map(|call_id| PbxEffect::Resume { call_id }.into()),
        );
        effects.push(
            PbxEffect::Bridge {
                operation: crate::runtime::backend::BridgeOperation::MergeCalls {
                    bridge_id,
                    call_ids,
                },
            }
            .into(),
        );
        debug_assert!(self.invariant_error().is_none());
        Ok(effects)
    }

    /// Start an outbound consultation from the active moderator leg while the
    /// existing conference bridge remains live for its other participants.
    pub fn begin_conference_invite(
        &mut self,
        moderator_call_id: CallId,
        invite_call_id: CallId,
        binding: LineBinding,
        codec: Codec,
        now: Instant,
    ) -> Result<Vec<DriverEffect>, ConferenceRejection> {
        if self.call_registry.by_sccp.contains_key(&invite_call_id) {
            return Err(ConferenceRejection::Conflict);
        }
        let session = self
            .conference_session(moderator_call_id)
            .cloned()
            .ok_or(ConferenceRejection::Unavailable)?;
        if session.phase != ConferencePhase::Active
            || session.pending_invite.is_some()
            || session.pending_participant_mutation.is_some()
            || session.participants.iter().len() >= MAX_CONFERENCE_PARTICIPANTS
        {
            return Err(ConferenceRejection::Conflict);
        }
        let moderator = session
            .participants
            .iter()
            .find(|participant| {
                participant.moderator && participant.handset_call_id == moderator_call_id
            })
            .ok_or(ConferenceRejection::Disabled)?;
        let moderator_appearance = self
            .appearance_for_call(moderator_call_id)
            .cloned()
            .ok_or(ConferenceRejection::Unavailable)?;
        if self
            .call_registry
            .pbx
            .get(&moderator.pbx_call_id)
            .is_none_or(|call| call.state != CallState::Connected)
            || moderator_appearance.state != CallState::Connected
            || binding.device_id != session.device_id
            || binding.line_instance != moderator_appearance.line_instance
        {
            return Err(ConferenceRejection::NotConnected);
        }

        let music_started = session.participants.active_moderator_count() == 1;
        let moderator_id = moderator.id;
        let moderator_pbx_call_id = moderator.pbx_call_id;
        let mut effects = self.hold(moderator_call_id);
        if effects.is_empty() {
            return Err(ConferenceRejection::Conflict);
        }
        if music_started {
            effects.extend(Self::conference_music_effects(&session, true));
        }
        let invite_device_id = binding.device_id.clone();
        let invite_line_instance = binding.line_instance;
        let mut invite_effects = self.begin_phone_call(invite_call_id, binding, codec, now);
        invite_effects.insert(
            0,
            HandsetEffect::BeginCall {
                device_id: invite_device_id,
                line_instance: invite_line_instance,
                call_id: invite_call_id,
                codec,
            }
            .into(),
        );
        effects.extend(invite_effects);
        let Some(invite) = self.appearance_for_call(invite_call_id).cloned() else {
            let _ = self.resume(moderator_call_id);
            return Err(ConferenceRejection::Conflict);
        };
        let participant = self.conference_participant(&invite, false);
        let key = session.consultation_handset_call_id;
        let Some(stored) = self.conferences.by_consultation.get_mut(&key) else {
            let _ = self.resume(moderator_call_id);
            self.remove_pbx_call(invite.pbx_id);
            return Err(ConferenceRejection::Conflict);
        };
        stored.pending_invite = Some(ConferenceInvite {
            moderator_id,
            moderator_call_id: moderator_pbx_call_id,
            music_started,
            participant: participant.clone(),
        });
        self.conferences.by_pbx.insert(participant.pbx_call_id, key);
        debug_assert!(self.invariant_error().is_none());
        Ok(effects)
    }

    pub fn confirm_conference_invite(
        &self,
        invite_call_id: CallId,
    ) -> Result<Vec<DriverEffect>, ConferenceRejection> {
        let session = self
            .conference_session(invite_call_id)
            .ok_or(ConferenceRejection::Unavailable)?;
        let invite = session
            .pending_invite
            .as_ref()
            .filter(|invite| invite.participant.handset_call_id == invite_call_id)
            .ok_or(ConferenceRejection::Conflict)?;
        let moderator = session
            .participants
            .get(invite.moderator_id)
            .filter(|moderator| {
                moderator.moderator && moderator.pbx_call_id == invite.moderator_call_id
            })
            .ok_or(ConferenceRejection::Unavailable)?;
        if self
            .call_registry
            .pbx
            .get(&invite.participant.pbx_call_id)
            .is_none_or(|call| call.state != CallState::Connected)
            || self
                .call_registry
                .pbx
                .get(&moderator.pbx_call_id)
                .is_none_or(|call| call.state != CallState::Held)
        {
            return Err(ConferenceRejection::NotConnected);
        }
        let mut effects = if invite.music_started {
            Self::conference_music_effects(session, false)
        } else {
            Vec::new()
        };
        effects.extend([
            PbxEffect::Resume {
                call_id: moderator.pbx_call_id,
            }
            .into(),
            PbxEffect::Bridge {
                operation: crate::runtime::backend::BridgeOperation::MergeParticipant {
                    bridge_id: session.bridge_id,
                    call_id: invite.participant.pbx_call_id,
                },
            }
            .into(),
        ]);
        effects.extend(Self::conference_mute_on_entry_effects(
            session,
            std::iter::once(&invite.participant),
        ));
        Ok(effects)
    }

    pub fn conference_invite_merged(&mut self, invite_call_id: CallId) -> bool {
        let Some(key) = self
            .conference_session(invite_call_id)
            .map(|session| session.consultation_handset_call_id)
        else {
            return false;
        };
        let Some(session) = self.conferences.by_consultation.get_mut(&key) else {
            return false;
        };
        let Some(invite) = session.pending_invite.take() else {
            return false;
        };
        let participant_id = invite.participant.id;
        if invite.participant.handset_call_id != invite_call_id
            || session
                .participants
                .insert(invite.participant.clone())
                .is_err()
        {
            session.pending_invite = Some(invite);
            return false;
        }
        if session.media_policy.mute_on_entry
            && !session.participants.set_muted(participant_id, true)
        {
            return false;
        }
        let participant_calls: Vec<_> = session
            .participants
            .iter()
            .map(|participant| (participant.pbx_call_id, participant.handset_call_id))
            .collect();
        for (pbx_id, handset_call_id) in participant_calls {
            if let Some(call) = self.call_registry.pbx.get_mut(&pbx_id) {
                call.state = CallState::Connected;
            }
            if let Some(appearance_id) = self.call_registry.by_sccp.get(&handset_call_id).copied()
                && let Some(appearance) = self.call_registry.appearances.get_mut(&appearance_id)
            {
                appearance.state = CallState::Connected;
            }
        }
        debug_assert!(self.invariant_error().is_none());
        true
    }

    pub fn abort_conference_invite(
        &mut self,
        invite_call_id: CallId,
        invite_channel_created: bool,
        moderator_needs_resume: bool,
        restore_moderator_media: bool,
    ) -> Vec<DriverEffect> {
        let Some(key) = self
            .conference_session(invite_call_id)
            .map(|session| session.consultation_handset_call_id)
        else {
            return Vec::new();
        };
        let Some(session) = self.conferences.by_consultation.get_mut(&key) else {
            return Vec::new();
        };
        let Some(invite) = session.pending_invite.take() else {
            return Vec::new();
        };
        if invite.participant.handset_call_id != invite_call_id {
            session.pending_invite = Some(invite);
            return Vec::new();
        }
        let moderator = session
            .participants
            .get(invite.moderator_id)
            .filter(|moderator| {
                moderator.moderator && moderator.pbx_call_id == invite.moderator_call_id
            })
            .cloned();
        let music_effects = if invite.music_started {
            Self::conference_music_effects(session, false)
        } else {
            Vec::new()
        };
        self.conferences
            .by_pbx
            .remove(&invite.participant.pbx_call_id);
        self.remove_pbx_call(invite.participant.pbx_call_id);
        if let Some(moderator) = moderator.as_ref() {
            if let Some(call) = self.call_registry.pbx.get_mut(&moderator.pbx_call_id) {
                call.state = CallState::Connected;
            }
            if let Some(appearance_id) = self
                .call_registry
                .by_sccp
                .get(&moderator.handset_call_id)
                .copied()
                && let Some(appearance) = self.call_registry.appearances.get_mut(&appearance_id)
            {
                appearance.state = CallState::Connected;
                if restore_moderator_media {
                    appearance.audio = MediaStreamState::Opening;
                }
            }
        }

        let mut effects = music_effects;
        if invite_channel_created {
            effects.push(
                PbxEffect::Hangup {
                    call_id: invite.participant.pbx_call_id,
                }
                .into(),
            );
        }
        if moderator_needs_resume && let Some(moderator) = moderator.as_ref() {
            effects.push(
                PbxEffect::Resume {
                    call_id: moderator.pbx_call_id,
                }
                .into(),
            );
        }
        effects.push(
            HandsetEffect::SetCallState {
                device_id: invite.participant.device_id,
                call_id: invite.participant.handset_call_id,
                state: HandsetCallState::OnHook,
                stop_media: true,
            }
            .into(),
        );
        if restore_moderator_media && let Some(moderator) = moderator {
            let codec = self
                .appearance_for_call(moderator.handset_call_id)
                .map_or(Codec::Pcmu, |appearance| appearance.codec);
            effects.push(
                HandsetEffect::BeginMedia {
                    device_id: moderator.device_id,
                    call_id: moderator.handset_call_id,
                    codec,
                }
                .into(),
            );
        }
        debug_assert!(self.invariant_error().is_none());
        effects
    }

    /// Hold an active call and create a second outbound call for an attended
    /// conference consultation. The handset call identifier is reserved by
    /// the session adapter before this transition.
    pub fn begin_conference_with_media(
        &mut self,
        request: ConferenceConsultationRequest,
        media_policy: ConferenceMediaPolicy,
    ) -> Result<Vec<DriverEffect>, ConferenceRejection> {
        let effects = self.begin_conference(
            request.original_call_id,
            request.consultation_call_id,
            request.binding,
            request.codec,
            request.now,
            request.permitted,
        )?;
        if !self.configure_conference_media(request.consultation_call_id, media_policy) {
            self.abort_conference(request.consultation_call_id, false, false, false, false);
            return Err(ConferenceRejection::Conflict);
        }
        Ok(effects)
    }

    pub fn begin_conference(
        &mut self,
        original_call_id: CallId,
        consultation_call_id: CallId,
        binding: LineBinding,
        codec: Codec,
        now: Instant,
        permitted: bool,
    ) -> Result<Vec<DriverEffect>, ConferenceRejection> {
        if !permitted {
            return Err(ConferenceRejection::Disabled);
        }
        if self
            .call_registry
            .by_sccp
            .contains_key(&consultation_call_id)
        {
            return Err(ConferenceRejection::Conflict);
        }
        let original = self
            .appearance_for_call(original_call_id)
            .cloned()
            .ok_or(ConferenceRejection::Unavailable)?;
        let original_pbx = self
            .call_registry
            .pbx
            .get(&original.pbx_id)
            .ok_or(ConferenceRejection::Unavailable)?;
        if original_pbx.state != CallState::Connected
            || original.state != CallState::Connected
            || original_pbx.active_appearance != Some(original.id)
        {
            return Err(ConferenceRejection::NotConnected);
        }
        if binding.device_id != original.device_id
            || binding.line_instance != original.line_instance
            || binding.line.number != original_pbx.line
            || !self.devices.contains_key(&original.device_id)
        {
            return Err(ConferenceRejection::Unavailable);
        }
        if self.redirect_claims.contains(&original.pbx_id)
            || self.conferences.by_pbx.contains_key(&original.pbx_id)
        {
            return Err(ConferenceRejection::Conflict);
        }

        let mut effects = self.hold(original_call_id);
        if effects.is_empty() {
            return Err(ConferenceRejection::Conflict);
        }
        let consultation_device_id = binding.device_id.clone();
        let consultation_line_instance = binding.line_instance;
        let mut consultation_effects =
            self.begin_phone_call(consultation_call_id, binding, codec, now);
        consultation_effects.insert(
            0,
            HandsetEffect::BeginCall {
                device_id: consultation_device_id,
                line_instance: consultation_line_instance,
                call_id: consultation_call_id,
                codec,
            }
            .into(),
        );
        let Some(consultation) = self.appearance_for_call(consultation_call_id).cloned() else {
            let _ = self.resume(original_call_id);
            return Err(ConferenceRejection::Conflict);
        };
        let participants = ConferenceParticipantRegistry::new([
            self.conference_participant(&original, true),
            self.conference_participant(&consultation, false),
        ])
        .expect("fresh conference participant identities are unique");
        effects.extend(consultation_effects);
        let session = ConferenceSession {
            id: self.allocate_conference_id(),
            bridge_id: self.allocate_bridge_id(),
            device_id: original.device_id,
            original_handset_call_id: original_call_id,
            original_call_id: original.pbx_id,
            consultation_handset_call_id: consultation_call_id,
            consultation_call_id: consultation.pbx_id,
            phase: ConferencePhase::Consultation,
            origin: ConferenceOrigin::Consultation,
            participants,
            media_policy: ConferenceMediaPolicy::default(),
            pending_invite: None,
            pending_participant_mutation: None,
        };
        self.conferences
            .by_pbx
            .insert(session.original_call_id, consultation_call_id);
        self.conferences
            .by_pbx
            .insert(session.consultation_call_id, consultation_call_id);
        self.conferences
            .by_consultation
            .insert(consultation_call_id, session);
        debug_assert!(self.invariant_error().is_none());
        Ok(effects)
    }

    pub fn conference_session(&self, call_id: CallId) -> Option<&ConferenceSession> {
        let pbx_id = self.appearance_for_call(call_id)?.pbx_id;
        self.conference_session_by_pbx(pbx_id)
    }

    pub fn conference_session_by_pbx(&self, pbx_id: PbxCallId) -> Option<&ConferenceSession> {
        let consultation = self.conferences.by_pbx.get(&pbx_id)?;
        self.conferences.by_consultation.get(consultation)
    }

    pub fn conference_session_by_id(
        &self,
        conference_id: ConferenceId,
    ) -> Option<&ConferenceSession> {
        self.conferences
            .by_consultation
            .values()
            .find(|session| session.id == conference_id)
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
    pub(crate) fn claim_conference_mutation(
        &mut self,
        call_id: CallId,
    ) -> Option<ConferenceMutationToken> {
        let owner = ConferenceMutationOwner::Session(self.conference_session(call_id)?.id);
        self.allocate_conference_mutation(owner)
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
    pub(crate) fn claim_conference_mutation_by_id(
        &mut self,
        conference_id: ConferenceId,
    ) -> Option<ConferenceMutationToken> {
        self.conference_session_by_id(conference_id)?;
        self.allocate_conference_mutation(ConferenceMutationOwner::Session(conference_id))
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
    pub(crate) fn conference_mutation_is_active(&self, token: ConferenceMutationToken) -> bool {
        if self.conference_mutations.get(&token.owner) != Some(&token.generation) {
            return false;
        }
        match token.owner {
            ConferenceMutationOwner::Session(conference_id) => {
                self.conference_session_by_id(conference_id).is_some()
            }
            ConferenceMutationOwner::Destination(call_id) => {
                self.call_registry.pbx.contains_key(&call_id)
            }
        }
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
    pub(crate) fn complete_conference_mutation(&mut self, token: ConferenceMutationToken) -> bool {
        if self.conference_mutations.get(&token.owner) != Some(&token.generation) {
            return false;
        }
        self.conference_mutations.remove(&token.owner);
        true
    }

    #[cfg(test)]
    fn conference_destination_mutation(
        &self,
        handset_call_id: CallId,
    ) -> Option<ConferenceMutationToken> {
        let owner =
            ConferenceMutationOwner::Destination(self.appearance_for_call(handset_call_id)?.pbx_id);
        self.conference_mutations
            .get(&owner)
            .copied()
            .map(|generation| ConferenceMutationToken { owner, generation })
    }

    fn allocate_conference_mutation(
        &mut self,
        owner: ConferenceMutationOwner,
    ) -> Option<ConferenceMutationToken> {
        if self.conference_mutations.contains_key(&owner) {
            return None;
        }
        let generation = self.next_conference_mutation_generation;
        self.next_conference_mutation_generation = generation.checked_add(1)?;
        self.conference_mutations.insert(owner, generation);
        Some(ConferenceMutationToken { owner, generation })
    }

    /// Bind normalized media policy before a conference becomes active. An
    /// active session keeps its captured policy across configuration reloads.
    pub fn configure_conference_media(
        &mut self,
        call_id: CallId,
        policy: ConferenceMediaPolicy,
    ) -> bool {
        let Some(key) = self
            .conference_session(call_id)
            .map(|session| session.consultation_handset_call_id)
        else {
            return false;
        };
        let Some(session) = self.conferences.by_consultation.get_mut(&key) else {
            return false;
        };
        if session.phase == ConferencePhase::Active {
            return false;
        }
        session.media_policy = policy;
        true
    }

    /// Build one typed PBX announcement from committed conference state.
    /// Callers invoke this only after the associated bridge mutation succeeds.
    pub fn conference_announcement_effects(
        &self,
        conference_id: ConferenceId,
        announcement: ConferenceAnnouncement,
    ) -> Vec<DriverEffect> {
        let Some(session) = self.conference_session_by_id(conference_id) else {
            return Vec::new();
        };
        Self::conference_announcement_effects_for_session(session, announcement)
    }

    fn conference_announcement_effects_for_session(
        session: &ConferenceSession,
        announcement: ConferenceAnnouncement,
    ) -> Vec<DriverEffect> {
        if session.phase != ConferencePhase::Active {
            return Vec::new();
        }
        let enabled = match announcement {
            ConferenceAnnouncement::Connected
            | ConferenceAnnouncement::ParticipantJoined(_)
            | ConferenceAnnouncement::ParticipantRemoved(_)
            | ConferenceAnnouncement::ModeratorDeparted(_) => {
                session.media_policy.play_general_announcements
            }
            ConferenceAnnouncement::ParticipantMuted(_)
            | ConferenceAnnouncement::ParticipantUnmuted(_) => {
                session.media_policy.play_participant_announcements
            }
        };
        if !enabled {
            return Vec::new();
        }
        let participant_ids = match announcement {
            ConferenceAnnouncement::ParticipantMuted(participant_id)
            | ConferenceAnnouncement::ParticipantUnmuted(participant_id) => session
                .participants
                .get(participant_id)
                .map(|_| vec![participant_id])
                .unwrap_or_default(),
            ConferenceAnnouncement::Connected
            | ConferenceAnnouncement::ParticipantJoined(_)
            | ConferenceAnnouncement::ParticipantRemoved(_) => session
                .participants
                .iter()
                .map(|participant| participant.id)
                .collect(),
            ConferenceAnnouncement::ModeratorDeparted(participant_id) => session
                .participants
                .iter()
                .filter(|participant| participant.id != participant_id)
                .map(|participant| participant.id)
                .collect(),
        };
        if participant_ids.is_empty() {
            return Vec::new();
        }
        let targets = participant_ids
            .iter()
            .filter_map(|participant_id| {
                session
                    .participants
                    .get(*participant_id)
                    .map(|participant| ConferenceAnnouncementTarget {
                        participant_id: *participant_id,
                        call_id: participant.pbx_call_id,
                    })
            })
            .collect();
        vec![
            PbxEffect::ConferenceAnnouncement {
                operation: ConferenceAnnouncementOperation {
                    conference_id: session.id,
                    targets,
                    announcement,
                },
            }
            .into(),
        ]
    }

    fn conference_music_effects(session: &ConferenceSession, enabled: bool) -> Vec<DriverEffect> {
        let Some(class) = session.media_policy.music_on_hold_class.as_ref() else {
            return Vec::new();
        };
        session
            .participants
            .iter()
            .filter(|participant| !participant.moderator)
            .map(|participant| {
                PbxEffect::Bridge {
                    operation:
                        crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold {
                            bridge_id: session.bridge_id,
                            participant_id: participant.id,
                            call_id: participant.pbx_call_id,
                            class: class.clone(),
                            enabled,
                        },
                }
                .into()
            })
            .collect()
    }

    fn conference_mute_on_entry_effects<'a>(
        session: &ConferenceSession,
        participants: impl IntoIterator<Item = &'a ConferenceParticipant>,
    ) -> Vec<DriverEffect> {
        if !session.media_policy.mute_on_entry {
            return Vec::new();
        }
        participants
            .into_iter()
            .filter(|participant| !participant.moderator)
            .map(|participant| {
                PbxEffect::Bridge {
                    operation: crate::runtime::backend::BridgeOperation::SetParticipantMuted {
                        bridge_id: session.bridge_id,
                        participant_id: participant.id,
                        call_id: participant.pbx_call_id,
                        muted: true,
                    },
                }
                .into()
            })
            .collect()
    }

    /// Plan the atomic native merge after the consultation party answers.
    pub fn confirm_conference(
        &mut self,
        call_id: CallId,
    ) -> Result<Vec<DriverEffect>, ConferenceRejection> {
        let consultation = self
            .conference_session(call_id)
            .map(|session| session.consultation_handset_call_id)
            .ok_or(ConferenceRejection::Unavailable)?;
        if consultation != call_id {
            return Err(ConferenceRejection::Conflict);
        }
        let session = self
            .conferences
            .by_consultation
            .get(&consultation)
            .cloned()
            .ok_or(ConferenceRejection::Unavailable)?;
        if session.phase != ConferencePhase::Consultation {
            return Err(ConferenceRejection::Conflict);
        }
        if self
            .call_registry
            .pbx
            .get(&session.consultation_call_id)
            .is_none_or(|call| call.state != CallState::Connected)
            || self
                .call_registry
                .pbx
                .get(&session.original_call_id)
                .is_none_or(|call| call.state != CallState::Held)
        {
            return Err(ConferenceRejection::NotConnected);
        }
        if let Some(stored) = self.conferences.by_consultation.get_mut(&consultation) {
            stored.phase = ConferencePhase::Merging;
        }
        debug_assert!(self.invariant_error().is_none());
        let mut effects = vec![
            PbxEffect::Bridge {
                operation: crate::runtime::backend::BridgeOperation::Create {
                    bridge_id: session.bridge_id,
                },
            }
            .into(),
            PbxEffect::Resume {
                call_id: session.original_call_id,
            }
            .into(),
            PbxEffect::Bridge {
                operation: crate::runtime::backend::BridgeOperation::MergeConsultation {
                    bridge_id: session.bridge_id,
                    original_call_id: session.original_call_id,
                    consultation_call_id: session.consultation_call_id,
                },
            }
            .into(),
        ];
        effects.extend(Self::conference_mute_on_entry_effects(
            &session,
            session.participants.iter(),
        ));
        Ok(effects)
    }

    pub fn conference_merged(&mut self, call_id: CallId) -> bool {
        let Some(consultation) = self
            .conference_session(call_id)
            .map(|session| session.consultation_handset_call_id)
        else {
            return false;
        };
        let Some(session) = self.conferences.by_consultation.get_mut(&consultation) else {
            return false;
        };
        if session.phase != ConferencePhase::Merging {
            return false;
        }
        session.phase = ConferencePhase::Active;
        if session.media_policy.mute_on_entry {
            let participant_ids = session
                .participants
                .iter()
                .filter(|participant| !participant.moderator)
                .map(|participant| participant.id)
                .collect::<Vec<_>>();
            for participant_id in participant_ids {
                if !session.participants.set_muted(participant_id, true) {
                    return false;
                }
            }
        }
        let participant_calls: Vec<_> = session
            .participants
            .iter()
            .map(|participant| (participant.pbx_call_id, participant.handset_call_id))
            .collect();
        for (pbx_id, handset_call_id) in participant_calls {
            if let Some(call) = self.call_registry.pbx.get_mut(&pbx_id) {
                call.state = CallState::Connected;
            }
            if let Some(appearance_id) = self.call_registry.by_sccp.get(&handset_call_id).copied()
                && let Some(appearance) = self.call_registry.appearances.get_mut(&appearance_id)
            {
                appearance.state = CallState::Connected;
            }
        }
        debug_assert!(self.invariant_error().is_none());
        true
    }

    pub fn conference_json(&self, call_id: CallId) -> Option<String> {
        let session = self.conference_session(call_id)?;
        session.participants.to_json(session.id).ok()
    }

    /// Reserve a moderator-leg hold or resume while leaving the PBX channel
    /// and live conference bridge connected. Music changes only when this
    /// transition crosses the boundary between at least one listening
    /// moderator and none.
    pub fn begin_conference_moderator_leg_transition(
        &mut self,
        call_id: CallId,
        held: bool,
    ) -> Result<Vec<DriverEffect>, ConferenceParticipantRejection> {
        let session = self
            .conference_session(call_id)
            .ok_or(ConferenceParticipantRejection::Unavailable)?;
        if session.phase != ConferencePhase::Active {
            return Err(ConferenceParticipantRejection::Unavailable);
        }
        if session.pending_participant_mutation.is_some() || session.pending_invite.is_some() {
            return Err(ConferenceParticipantRejection::Conflict);
        }
        let participant = session
            .participants
            .iter()
            .find(|participant| participant.handset_call_id == call_id)
            .filter(|participant| participant.moderator)
            .cloned()
            .ok_or(ConferenceParticipantRejection::NotModerator)?;
        if participant.held == held {
            return Err(ConferenceParticipantRejection::Conflict);
        }
        let appearance = self
            .appearance_for_call(call_id)
            .cloned()
            .ok_or(ConferenceParticipantRejection::Unavailable)?;
        let expected_state = if held {
            CallState::Connected
        } else {
            CallState::Held
        };
        if appearance.state != expected_state
            || self
                .call_registry
                .pbx
                .get(&participant.pbx_call_id)
                .is_none_or(|call| call.state != CallState::Connected)
        {
            return Err(ConferenceParticipantRejection::Conflict);
        }

        let change_music = if held {
            session.participants.active_moderator_count() == 1
        } else {
            session.participants.active_moderator_count() == 0
        };
        let mut effects = Vec::new();
        if held {
            effects.push(appearance_state_effect(
                &appearance,
                HandsetCallState::Hold,
                true,
            ));
        }
        if change_music {
            effects.extend(Self::conference_music_effects(session, held));
        }
        if !held {
            effects.push(
                HandsetEffect::BeginMedia {
                    device_id: participant.device_id.clone(),
                    call_id: participant.handset_call_id,
                    codec: appearance.codec,
                }
                .into(),
            );
        }

        let conference_id = session.id;
        let mutation = ConferenceParticipantMutation {
            participant_id: participant.id,
            call_id: participant.pbx_call_id,
            kind: ConferenceParticipantMutationKind::Hold(held),
        };
        self.conferences
            .by_consultation
            .values_mut()
            .find(|session| session.id == conference_id)
            .expect("conference validated above")
            .pending_participant_mutation = Some(mutation);
        if !held && let Some(appearance) = self.call_registry.appearances.get_mut(&appearance.id) {
            appearance.audio = MediaStreamState::Opening;
        }
        debug_assert!(self.invariant_error().is_none());
        Ok(effects)
    }

    /// Commit a handset/native-confirmed moderator-leg transition without
    /// changing the participant, PBX-call, or bridge identities.
    pub fn conference_moderator_leg_transitioned(
        &mut self,
        conference_id: ConferenceId,
        participant_id: ParticipantId,
        held: bool,
    ) -> bool {
        let Some(key) = self
            .conferences
            .by_consultation
            .iter()
            .find_map(|(key, session)| (session.id == conference_id).then_some(*key))
        else {
            return false;
        };
        let Some(session) = self.conferences.by_consultation.get(&key) else {
            return false;
        };
        let Some(pending) = session.pending_participant_mutation else {
            return false;
        };
        let Some(participant) = session.participants.get(participant_id).cloned() else {
            return false;
        };
        if pending.participant_id != participant_id
            || pending.call_id != participant.pbx_call_id
            || pending.kind != ConferenceParticipantMutationKind::Hold(held)
            || participant.held == held
        {
            return false;
        }
        let Some(appearance_id) = self
            .call_registry
            .by_sccp
            .get(&participant.handset_call_id)
            .copied()
            .filter(|appearance_id| self.call_registry.appearances.contains_key(appearance_id))
        else {
            return false;
        };
        let session = self
            .conferences
            .by_consultation
            .get_mut(&key)
            .expect("conference key validated above");
        if !session.participants.set_held(participant_id, held) {
            return false;
        }
        session.pending_participant_mutation = None;

        let appearance = self
            .call_registry
            .appearances
            .get_mut(&appearance_id)
            .expect("appearance validated above");
        appearance.state = if held {
            appearance.audio = MediaStreamState::Closed;
            appearance.audio_transmit = MediaStreamState::Closed;
            appearance.video.close_streams();
            CallState::Held
        } else {
            appearance.audio = MediaStreamState::Opening;
            CallState::Connected
        };
        let device_id = appearance.device_id.clone();
        let line_instance = appearance.line_instance;
        let handset_call_id = appearance.sccp_id;
        self.select_line(&device_id, line_instance);
        self.set_call_selected(&device_id, handset_call_id, !held);
        debug_assert!(self.invariant_error().is_none());
        true
    }

    /// Release a failed transition and describe only the inverse operations
    /// required for handset/native work that may already have completed.
    pub fn abort_conference_moderator_leg_transition(
        &mut self,
        conference_id: ConferenceId,
        participant_id: ParticipantId,
        held: bool,
        completed_music: &[ParticipantId],
        handset_attempted: bool,
    ) -> Vec<DriverEffect> {
        let Some(session) = self
            .conferences
            .by_consultation
            .values()
            .find(|session| session.id == conference_id)
            .cloned()
        else {
            return Vec::new();
        };
        if session.pending_participant_mutation.is_none_or(|pending| {
            pending.participant_id != participant_id
                || pending.kind != ConferenceParticipantMutationKind::Hold(held)
        }) {
            return Vec::new();
        }
        let Some(participant) = session.participants.get(participant_id).cloned() else {
            return Vec::new();
        };
        if let Some(stored) = self
            .conferences
            .by_consultation
            .values_mut()
            .find(|stored| stored.id == conference_id)
        {
            stored.pending_participant_mutation = None;
        }

        let music = session.media_policy.music_on_hold_class.as_ref();
        let mut effects = Vec::new();
        if !held {
            if let Some(appearance_id) = self
                .call_registry
                .by_sccp
                .get(&participant.handset_call_id)
                .copied()
                && let Some(appearance) = self.call_registry.appearances.get_mut(&appearance_id)
            {
                appearance.audio = MediaStreamState::Closed;
            }
            if handset_attempted {
                effects.push(
                    HandsetEffect::SetCallState {
                        device_id: participant.device_id.clone(),
                        call_id: participant.handset_call_id,
                        state: HandsetCallState::Hold,
                        stop_media: true,
                    }
                    .into(),
                );
            }
        }
        if let Some(class) = music {
            effects.extend(completed_music.iter().filter_map(|completed| {
                session.participants.get(*completed).map(|target| {
                    PbxEffect::Bridge {
                        operation:
                            crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold {
                                bridge_id: session.bridge_id,
                                participant_id: target.id,
                                call_id: target.pbx_call_id,
                                class: class.clone(),
                                enabled: !held,
                            },
                    }
                    .into()
                })
            }));
        }
        if held && handset_attempted {
            if let Some(appearance_id) = self
                .call_registry
                .by_sccp
                .get(&participant.handset_call_id)
                .copied()
                && let Some(appearance) = self.call_registry.appearances.get_mut(&appearance_id)
            {
                appearance.audio = MediaStreamState::Opening;
            }
            let codec = self
                .appearance_for_call(participant.handset_call_id)
                .map_or(Codec::Pcmu, |appearance| appearance.codec);
            effects.push(
                HandsetEffect::BeginMedia {
                    device_id: participant.device_id,
                    call_id: participant.handset_call_id,
                    codec,
                }
                .into(),
            );
        }
        debug_assert!(self.invariant_error().is_none());
        effects
    }

    /// Reserve a moderator-authorized participant mute transition. Participant
    /// state is committed only after the backend confirms that the live bridge
    /// channel was updated.
    pub fn begin_conference_participant_mute(
        &mut self,
        requester: &DeviceId,
        conference_id: ConferenceId,
        participant_id: ParticipantId,
        muted: bool,
    ) -> Result<Vec<DriverEffect>, ConferenceParticipantRejection> {
        let session = self
            .conferences
            .by_consultation
            .values()
            .find(|session| session.id == conference_id)
            .ok_or(ConferenceParticipantRejection::Unavailable)?;
        if session.phase != ConferencePhase::Active {
            return Err(ConferenceParticipantRejection::Unavailable);
        }
        if &session.device_id != requester
            || session
                .participants
                .moderator()
                .is_none_or(|moderator| &moderator.device_id != requester)
        {
            return Err(ConferenceParticipantRejection::NotModerator);
        }
        if session.pending_participant_mutation.is_some() || session.pending_invite.is_some() {
            return Err(ConferenceParticipantRejection::Conflict);
        }
        let participant = session
            .participants
            .get(participant_id)
            .ok_or(ConferenceParticipantRejection::InvalidParticipant)?;
        if participant.moderator {
            return Err(ConferenceParticipantRejection::Moderator);
        }
        if participant.muted == muted {
            return Err(ConferenceParticipantRejection::Conflict);
        }
        let mutation = ConferenceParticipantMutation {
            participant_id,
            call_id: participant.pbx_call_id,
            kind: ConferenceParticipantMutationKind::Mute(muted),
        };
        let bridge_id = session.bridge_id;
        let session = self
            .conferences
            .by_consultation
            .values_mut()
            .find(|session| session.id == conference_id)
            .expect("conference validated above");
        session.pending_participant_mutation = Some(mutation);
        debug_assert!(self.invariant_error().is_none());
        Ok(vec![
            PbxEffect::Bridge {
                operation: crate::runtime::backend::BridgeOperation::SetParticipantMuted {
                    bridge_id,
                    participant_id,
                    call_id: mutation.call_id,
                    muted,
                },
            }
            .into(),
        ])
    }

    /// Commit a participant mute transition after the backend succeeds.
    pub fn conference_participant_muted(
        &mut self,
        conference_id: ConferenceId,
        participant_id: ParticipantId,
        muted: bool,
    ) -> bool {
        let Some(session) = self
            .conferences
            .by_consultation
            .values_mut()
            .find(|session| session.id == conference_id)
        else {
            return false;
        };
        let Some(pending) = session.pending_participant_mutation else {
            return false;
        };
        if pending.participant_id != participant_id
            || pending.kind != ConferenceParticipantMutationKind::Mute(muted)
        {
            return false;
        }
        let Some(participant) = session.participants.get(participant_id) else {
            return false;
        };
        if participant.pbx_call_id != pending.call_id
            || participant.moderator
            || participant.muted == muted
        {
            return false;
        }
        if !session.participants.set_muted(participant_id, muted) {
            return false;
        }
        session.pending_participant_mutation = None;
        debug_assert!(self.invariant_error().is_none());
        true
    }

    /// Release a reserved mute transition after any backend failure. The
    /// published participant state remains unchanged.
    pub fn abort_conference_participant_mute(
        &mut self,
        conference_id: ConferenceId,
        participant_id: ParticipantId,
        muted: bool,
    ) -> bool {
        let Some(session) = self
            .conferences
            .by_consultation
            .values_mut()
            .find(|session| session.id == conference_id)
        else {
            return false;
        };
        if session.pending_participant_mutation.is_none_or(|pending| {
            pending.participant_id != participant_id
                || pending.kind != ConferenceParticipantMutationKind::Mute(muted)
        }) {
            return false;
        }
        session.pending_participant_mutation = None;
        debug_assert!(self.invariant_error().is_none());
        true
    }

    /// Reserve removal of one non-moderator while retaining at least two live
    /// conference members. Registry and UI state remain unchanged until the
    /// backend validates and clears the exact bridge member.
    pub fn begin_conference_participant_removal(
        &mut self,
        requester: &DeviceId,
        conference_id: ConferenceId,
        participant_id: ParticipantId,
    ) -> Result<Vec<DriverEffect>, ConferenceParticipantRejection> {
        let session = self
            .conferences
            .by_consultation
            .values()
            .find(|session| session.id == conference_id)
            .ok_or(ConferenceParticipantRejection::Unavailable)?;
        if session.phase != ConferencePhase::Active {
            return Err(ConferenceParticipantRejection::Unavailable);
        }
        if &session.device_id != requester
            || session
                .participants
                .moderator()
                .is_none_or(|moderator| &moderator.device_id != requester)
        {
            return Err(ConferenceParticipantRejection::NotModerator);
        }
        if session.pending_participant_mutation.is_some() || session.pending_invite.is_some() {
            return Err(ConferenceParticipantRejection::Conflict);
        }
        let participant = session
            .participants
            .get(participant_id)
            .ok_or(ConferenceParticipantRejection::InvalidParticipant)?;
        if participant.moderator {
            return Err(ConferenceParticipantRejection::Moderator);
        }
        if session.participants.iter().len() <= 2 {
            return Err(ConferenceParticipantRejection::Conflict);
        }
        let mutation = ConferenceParticipantMutation {
            participant_id,
            call_id: participant.pbx_call_id,
            kind: ConferenceParticipantMutationKind::Remove,
        };
        let bridge_id = session.bridge_id;
        self.conferences
            .by_consultation
            .values_mut()
            .find(|session| session.id == conference_id)
            .expect("conference validated above")
            .pending_participant_mutation = Some(mutation);
        debug_assert!(self.invariant_error().is_none());
        Ok(vec![
            PbxEffect::Bridge {
                operation: crate::runtime::backend::BridgeOperation::RemoveConferenceParticipant {
                    bridge_id,
                    participant_id,
                    call_id: mutation.call_id,
                },
            }
            .into(),
        ])
    }

    /// Commit a backend-confirmed participant removal, re-keying the internal
    /// conference session if its historical consultation leg was removed.
    pub fn conference_participant_removed(
        &mut self,
        conference_id: ConferenceId,
        participant_id: ParticipantId,
    ) -> Option<Vec<DriverEffect>> {
        let key = self
            .conferences
            .by_consultation
            .iter()
            .find_map(|(key, session)| (session.id == conference_id).then_some(*key))?;
        let mut session = self.conferences.by_consultation.remove(&key)?;
        let Some(pending) = session.pending_participant_mutation else {
            self.conferences.by_consultation.insert(key, session);
            return None;
        };
        if pending.participant_id != participant_id
            || pending.kind != ConferenceParticipantMutationKind::Remove
            || session.participants.iter().len() <= 2
            || session
                .participants
                .get(participant_id)
                .is_none_or(|participant| {
                    participant.moderator || participant.pbx_call_id != pending.call_id
                })
        {
            self.conferences.by_consultation.insert(key, session);
            return None;
        }

        let removed = session
            .participants
            .remove(participant_id)
            .expect("participant validated above");
        session.pending_participant_mutation = None;
        self.conferences.by_pbx.remove(&removed.pbx_call_id);
        if session.consultation_call_id == removed.pbx_call_id {
            let replacement = session
                .participants
                .iter()
                .find(|participant| participant.pbx_call_id != session.original_call_id)
                .expect("a removable conference retains a secondary participant");
            session.consultation_call_id = replacement.pbx_call_id;
            session.consultation_handset_call_id = replacement.handset_call_id;
        }
        let new_key = session.consultation_handset_call_id;
        for participant in session.participants.iter() {
            self.conferences
                .by_pbx
                .insert(participant.pbx_call_id, new_key);
        }
        let appearance = self.appearance_for_call(removed.handset_call_id).cloned();
        self.remove_pbx_call(removed.pbx_call_id);
        self.conferences.by_consultation.insert(new_key, session);

        let effects = appearance
            .as_ref()
            .map(|appearance| {
                vec![appearance_state_effect(
                    appearance,
                    HandsetCallState::OnHook,
                    true,
                )]
            })
            .unwrap_or_default();
        debug_assert!(self.invariant_error().is_none());
        Some(effects)
    }

    pub fn abort_conference_participant_removal(
        &mut self,
        conference_id: ConferenceId,
        participant_id: ParticipantId,
    ) -> bool {
        let Some(session) = self
            .conferences
            .by_consultation
            .values_mut()
            .find(|session| session.id == conference_id)
        else {
            return false;
        };
        if session.pending_participant_mutation.is_none_or(|pending| {
            pending.participant_id != participant_id
                || pending.kind != ConferenceParticipantMutationKind::Remove
        }) {
            return false;
        }
        session.pending_participant_mutation = None;
        debug_assert!(self.invariant_error().is_none());
        true
    }

    /// Reserve a moderator role transition. When the role change crosses the
    /// boundary between a listening moderator and no listening moderators,
    /// conference music is changed before the new role is committed.
    pub fn begin_conference_participant_role_change(
        &mut self,
        requester: &DeviceId,
        conference_id: ConferenceId,
        participant_id: ParticipantId,
        moderator: bool,
    ) -> Result<Vec<DriverEffect>, ConferenceParticipantRejection> {
        let session = self
            .conferences
            .by_consultation
            .values()
            .find(|session| session.id == conference_id)
            .ok_or(ConferenceParticipantRejection::Unavailable)?;
        if session.phase != ConferencePhase::Active {
            return Err(ConferenceParticipantRejection::Unavailable);
        }
        if &session.device_id != requester
            || !session
                .participants
                .iter()
                .any(|participant| participant.moderator && &participant.device_id == requester)
        {
            return Err(ConferenceParticipantRejection::NotModerator);
        }
        if session.pending_participant_mutation.is_some() || session.pending_invite.is_some() {
            return Err(ConferenceParticipantRejection::Conflict);
        }
        let participant = session
            .participants
            .get(participant_id)
            .ok_or(ConferenceParticipantRejection::InvalidParticipant)?;
        if participant.moderator == moderator {
            return Err(ConferenceParticipantRejection::Conflict);
        }
        if participant.held || (moderator && participant.muted) {
            return Err(ConferenceParticipantRejection::Conflict);
        }
        if !moderator && session.participants.moderator_count() == 1 {
            return Err(ConferenceParticipantRejection::LastModerator);
        }
        let effects = if moderator && session.participants.active_moderator_count() == 0 {
            Self::conference_music_effects(session, false)
        } else if !moderator && session.participants.active_moderator_count() == 1 {
            let Some(class) = session.media_policy.music_on_hold_class.as_ref() else {
                return self.reserve_conference_participant_role_change(
                    conference_id,
                    participant_id,
                    participant.pbx_call_id,
                    moderator,
                    Vec::new(),
                );
            };
            session
                .participants
                .iter()
                .filter(|candidate| !candidate.moderator || candidate.id == participant_id)
                .map(|candidate| {
                    PbxEffect::Bridge {
                        operation:
                            crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold {
                                bridge_id: session.bridge_id,
                                participant_id: candidate.id,
                                call_id: candidate.pbx_call_id,
                                class: class.clone(),
                                enabled: true,
                            },
                    }
                    .into()
                })
                .collect()
        } else {
            Vec::new()
        };
        self.reserve_conference_participant_role_change(
            conference_id,
            participant_id,
            participant.pbx_call_id,
            moderator,
            effects,
        )
    }

    fn reserve_conference_participant_role_change(
        &mut self,
        conference_id: ConferenceId,
        participant_id: ParticipantId,
        call_id: PbxCallId,
        moderator: bool,
        effects: Vec<DriverEffect>,
    ) -> Result<Vec<DriverEffect>, ConferenceParticipantRejection> {
        let mutation = ConferenceParticipantMutation {
            participant_id,
            call_id,
            kind: ConferenceParticipantMutationKind::Moderator(moderator),
        };
        self.conferences
            .by_consultation
            .values_mut()
            .find(|session| session.id == conference_id)
            .expect("conference validated above")
            .pending_participant_mutation = Some(mutation);
        debug_assert!(self.invariant_error().is_none());
        Ok(effects)
    }

    pub fn conference_participant_role_changed(
        &mut self,
        conference_id: ConferenceId,
        participant_id: ParticipantId,
        moderator: bool,
    ) -> bool {
        let Some(session) = self
            .conferences
            .by_consultation
            .values_mut()
            .find(|session| session.id == conference_id)
        else {
            return false;
        };
        let Some(pending) = session.pending_participant_mutation else {
            return false;
        };
        if pending.participant_id != participant_id
            || pending.kind != ConferenceParticipantMutationKind::Moderator(moderator)
            || session
                .participants
                .get(participant_id)
                .is_none_or(|participant| {
                    participant.pbx_call_id != pending.call_id
                        || participant.moderator == moderator
                        || participant.held
                        || (moderator && participant.muted)
                })
        {
            return false;
        }
        if session
            .participants
            .set_moderator(participant_id, moderator)
            .is_err()
        {
            session.pending_participant_mutation = None;
            return false;
        }
        session.pending_participant_mutation = None;
        debug_assert!(self.invariant_error().is_none());
        true
    }

    pub fn abort_conference_participant_role_change(
        &mut self,
        conference_id: ConferenceId,
        participant_id: ParticipantId,
        moderator: bool,
    ) -> bool {
        let Some(session) = self
            .conferences
            .by_consultation
            .values_mut()
            .find(|session| session.id == conference_id)
        else {
            return false;
        };
        if session.pending_participant_mutation.is_none_or(|pending| {
            pending.participant_id != participant_id
                || pending.kind != ConferenceParticipantMutationKind::Moderator(moderator)
        }) {
            return false;
        }
        session.pending_participant_mutation = None;
        debug_assert!(self.invariant_error().is_none());
        true
    }

    /// Restore selected calls after a native multi-call merge fails. Native
    /// merge is atomic, so only calls resumed before the merge need re-hold.
    pub fn abort_join_conference(
        &mut self,
        call_id: CallId,
        bridge_created: bool,
        resumed_call_ids: &[PbxCallId],
    ) -> Vec<DriverEffect> {
        let Some(key) = self
            .conference_session(call_id)
            .filter(|session| session.origin == ConferenceOrigin::Selection)
            .map(|session| session.consultation_handset_call_id)
        else {
            return Vec::new();
        };
        let Some(session) = self.conferences.by_consultation.remove(&key) else {
            return Vec::new();
        };
        self.conference_mutations
            .remove(&ConferenceMutationOwner::Session(session.id));
        for participant in session.participants.iter() {
            self.conferences.by_pbx.remove(&participant.pbx_call_id);
        }
        let mut effects = Vec::new();
        if bridge_created {
            effects.push(
                PbxEffect::Bridge {
                    operation: crate::runtime::backend::BridgeOperation::Destroy {
                        bridge_id: session.bridge_id,
                    },
                }
                .into(),
            );
        }
        effects.extend(
            resumed_call_ids
                .iter()
                .copied()
                .map(|call_id| PbxEffect::Hold { call_id }.into()),
        );
        debug_assert!(self.invariant_error().is_none());
        effects
    }

    pub fn cancel_conference(&mut self, call_id: CallId) -> Vec<DriverEffect> {
        let Some(session) = self.conference_session(call_id) else {
            return Vec::new();
        };
        if session.phase != ConferencePhase::Consultation
            || session.origin != ConferenceOrigin::Consultation
        {
            return Vec::new();
        }
        self.abort_conference(call_id, false, true, true, true)
    }

    pub fn end_conference(&mut self, call_id: CallId) -> Vec<DriverEffect> {
        let Some(session) = self.conference_session(call_id).cloned() else {
            return Vec::new();
        };
        if session.phase != ConferencePhase::Active {
            return Vec::new();
        }
        self.end_conference_internal(session, true, None)
    }

    /// Commit the loss of one handset conference presentation under the same
    /// controller lock as every other conference mutation. Pending work makes
    /// the failure terminal; otherwise the normal departure policy may retain
    /// the remaining participants and their stable identities.
    pub fn conference_participant_failed(
        &mut self,
        call_id: CallId,
    ) -> Option<ConferenceParticipantFailureOutcome> {
        let session = self.conference_session(call_id)?.clone();
        let failed_call_id = self.appearance_for_call(call_id)?.pbx_id;
        let mut owned_call_ids = session
            .participants
            .iter()
            .map(|participant| participant.pbx_call_id)
            .collect::<Vec<_>>();
        if let Some(invite) = &session.pending_invite {
            owned_call_ids.push(invite.participant.pbx_call_id);
        }
        let effects = self.hangup(call_id);
        let surviving_session = self.conference_session_by_id(session.id).cloned();
        let call_ids = if surviving_session.is_some() {
            vec![failed_call_id]
        } else {
            owned_call_ids
        };
        debug_assert!(self.invariant_error().is_none());
        Some(ConferenceParticipantFailureOutcome {
            conference_id: session.id,
            failed_call_id,
            call_ids,
            surviving_session,
            effects,
        })
    }

    /// Atomically detach every conference before module shutdown. The
    /// deterministic plans remain valid after the controller lock is released
    /// and a second drain is an explicit no-op.
    pub fn drain_conferences_for_shutdown(&mut self) -> Vec<ConferenceCleanupPlan> {
        let mut sessions = self
            .conferences
            .by_consultation
            .values()
            .cloned()
            .collect::<Vec<_>>();
        sessions.sort_by_key(|session| session.id);

        let plans = sessions
            .into_iter()
            .map(|session| {
                debug_assert!(self.conference_session_by_id(session.id).is_some());
                let mut call_ids = session
                    .participants
                    .iter()
                    .map(|participant| participant.pbx_call_id)
                    .collect::<Vec<_>>();
                if let Some(invite) = &session.pending_invite {
                    call_ids.push(invite.participant.pbx_call_id);
                }
                let bridge_created = session.phase != ConferencePhase::Consultation;
                let conference_id = session.id;
                let effects = self.end_conference_internal(session, bridge_created, None);
                ConferenceCleanupPlan {
                    conference_id,
                    call_ids,
                    effects,
                }
            })
            .collect();
        debug_assert!(self.invariant_error().is_none());
        plans
    }

    /// Authorize and claim an explicit handset conference termination. The
    /// controller removes the complete conference atomically so a concurrent
    /// action or PBX callback cannot schedule a second cleanup sequence.
    pub fn end_conference_by_moderator(
        &mut self,
        requester: &DeviceId,
        conference_id: ConferenceId,
    ) -> Result<Vec<DriverEffect>, ConferenceEndRejection> {
        let session = self
            .conferences
            .by_consultation
            .values()
            .find(|session| session.id == conference_id)
            .cloned()
            .ok_or(ConferenceEndRejection::Unavailable)?;
        if session.phase != ConferencePhase::Active {
            return Err(ConferenceEndRejection::Unavailable);
        }
        if &session.device_id != requester
            || !session
                .participants
                .iter()
                .any(|participant| participant.moderator && &participant.device_id == requester)
        {
            return Err(ConferenceEndRejection::NotModerator);
        }
        if session.pending_participant_mutation.is_some() || session.pending_invite.is_some() {
            return Err(ConferenceEndRejection::Conflict);
        }
        Ok(self.end_conference_internal(session, true, None))
    }

    /// Roll back a failed consultation start or bridge merge. The flags
    /// describe backend/handset work that completed before the failure.
    pub fn abort_conference(
        &mut self,
        call_id: CallId,
        bridge_created: bool,
        consultation_channel_created: bool,
        original_needs_resume: bool,
        restore_original_media: bool,
    ) -> Vec<DriverEffect> {
        let Some(consultation) = self
            .conference_session(call_id)
            .map(|session| session.consultation_handset_call_id)
        else {
            return Vec::new();
        };
        let Some(session) = self.conferences.by_consultation.remove(&consultation) else {
            return Vec::new();
        };
        if session.origin != ConferenceOrigin::Consultation {
            self.conferences
                .by_consultation
                .insert(consultation, session);
            return Vec::new();
        }
        self.conference_mutations
            .remove(&ConferenceMutationOwner::Session(session.id));
        for participant in session.participants.iter() {
            self.conferences.by_pbx.remove(&participant.pbx_call_id);
        }
        self.remove_pbx_call(session.consultation_call_id);

        let original_appearance = self
            .appearance_for_call(session.original_handset_call_id)
            .cloned();
        if let Some(call) = self.call_registry.pbx.get_mut(&session.original_call_id) {
            call.state = CallState::Connected;
        }
        if let Some(appearance) = original_appearance.as_ref()
            && let Some(stored) = self.call_registry.appearances.get_mut(&appearance.id)
        {
            stored.state = CallState::Connected;
            stored.audio = if restore_original_media {
                MediaStreamState::Opening
            } else {
                appearance.audio
            };
        }
        self.select_line(
            &session.device_id,
            original_appearance
                .as_ref()
                .map_or(0, |call| call.line_instance),
        );
        self.set_call_selected(&session.device_id, session.original_handset_call_id, true);

        let mut effects = Vec::new();
        if bridge_created {
            effects.push(
                PbxEffect::Bridge {
                    operation: crate::runtime::backend::BridgeOperation::Destroy {
                        bridge_id: session.bridge_id,
                    },
                }
                .into(),
            );
        }
        if consultation_channel_created {
            effects.push(
                PbxEffect::Hangup {
                    call_id: session.consultation_call_id,
                }
                .into(),
            );
        }
        if original_needs_resume {
            effects.push(
                PbxEffect::Resume {
                    call_id: session.original_call_id,
                }
                .into(),
            );
        }
        effects.push(
            HandsetEffect::SetCallState {
                device_id: session.device_id.clone(),
                call_id: session.consultation_handset_call_id,
                state: HandsetCallState::OnHook,
                stop_media: true,
            }
            .into(),
        );
        if restore_original_media && let Some(original) = original_appearance {
            effects.push(
                HandsetEffect::BeginMedia {
                    device_id: original.device_id,
                    call_id: original.sccp_id,
                    codec: original.codec,
                }
                .into(),
            );
        }
        debug_assert!(self.invariant_error().is_none());
        effects
    }

    fn active_conference_departure(
        &mut self,
        session: ConferenceSession,
        pbx_id: PbxCallId,
        already_hung_up: Option<PbxCallId>,
        announce: bool,
    ) -> Vec<DriverEffect> {
        let Some(departing) = session
            .participants
            .iter()
            .find(|participant| participant.pbx_call_id == pbx_id)
            .cloned()
        else {
            return self.end_conference_internal(session, true, already_hung_up);
        };
        let remaining_participants = session.participants.iter().len().saturating_sub(1);
        let remaining_moderators = session
            .participants
            .moderator_count()
            .saturating_sub(usize::from(departing.moderator));
        let must_end = remaining_participants < 2
            || remaining_moderators == 0
            || session.pending_invite.is_some()
            || session.pending_participant_mutation.is_some();
        if must_end {
            let announcement = if departing.moderator {
                ConferenceAnnouncement::ModeratorDeparted(departing.id)
            } else {
                ConferenceAnnouncement::ParticipantRemoved(departing.id)
            };
            let mut effects = if announce {
                Self::conference_announcement_effects_for_session(&session, announcement)
            } else {
                Vec::new()
            };
            effects.extend(self.end_conference_internal(session, true, already_hung_up));
            return effects;
        }

        let old_key = session.consultation_handset_call_id;
        let mut session = self
            .conferences
            .by_consultation
            .remove(&old_key)
            .expect("active conference departure has a live session");
        let removed = session
            .participants
            .remove(departing.id)
            .expect("departing participant was validated above");
        self.conferences.by_pbx.remove(&removed.pbx_call_id);
        let appearance = self.appearance_for_call(removed.handset_call_id).cloned();
        self.remove_pbx_call(removed.pbx_call_id);

        if session.original_call_id == removed.pbx_call_id {
            let moderator = session
                .participants
                .moderator()
                .expect("a preserved conference retains a moderator");
            session.original_call_id = moderator.pbx_call_id;
            session.original_handset_call_id = moderator.handset_call_id;
            session.device_id = moderator.device_id.clone();
        }
        if session.consultation_call_id == removed.pbx_call_id
            || session.consultation_call_id == session.original_call_id
        {
            let replacement = session
                .participants
                .iter()
                .find(|participant| participant.pbx_call_id != session.original_call_id)
                .expect("a preserved conference retains a secondary participant");
            session.consultation_call_id = replacement.pbx_call_id;
            session.consultation_handset_call_id = replacement.handset_call_id;
        }
        let new_key = session.consultation_handset_call_id;
        for participant in session.participants.iter() {
            self.conferences
                .by_pbx
                .insert(participant.pbx_call_id, new_key);
        }
        let announcement = if announce {
            Self::conference_announcement_effects_for_session(
                &session,
                if departing.moderator {
                    ConferenceAnnouncement::ModeratorDeparted(departing.id)
                } else {
                    ConferenceAnnouncement::ParticipantRemoved(departing.id)
                },
            )
        } else {
            Vec::new()
        };
        self.conferences.by_consultation.insert(new_key, session);

        let mut effects = already_hung_up
            .is_none()
            .then_some(
                PbxEffect::Hangup {
                    call_id: removed.pbx_call_id,
                }
                .into(),
            )
            .into_iter()
            .collect::<Vec<_>>();
        effects.extend(
            appearance
                .as_ref()
                .map(|appearance| {
                    vec![appearance_state_effect(
                        appearance,
                        HandsetCallState::OnHook,
                        true,
                    )]
                })
                .unwrap_or_default(),
        );
        effects.extend(announcement);
        debug_assert!(self.invariant_error().is_none());
        effects
    }

    fn end_conference_internal(
        &mut self,
        session: ConferenceSession,
        bridge_created: bool,
        already_hung_up: Option<PbxCallId>,
    ) -> Vec<DriverEffect> {
        self.conference_mutations
            .remove(&ConferenceMutationOwner::Session(session.id));
        self.conferences
            .by_consultation
            .remove(&session.consultation_handset_call_id);
        let mut participants: Vec<_> = session.participants.iter().cloned().collect();
        if let Some(invite) = session.pending_invite {
            participants.push(invite.participant);
        }
        for participant in &participants {
            self.conferences.by_pbx.remove(&participant.pbx_call_id);
        }
        let appearances: Vec<_> = participants
            .iter()
            .filter_map(|participant| {
                self.appearance_for_call(participant.handset_call_id)
                    .cloned()
            })
            .collect();
        for participant in &participants {
            self.remove_pbx_call(participant.pbx_call_id);
        }

        let mut effects = Vec::new();
        if bridge_created {
            effects.push(
                PbxEffect::Bridge {
                    operation: crate::runtime::backend::BridgeOperation::Destroy {
                        bridge_id: session.bridge_id,
                    },
                }
                .into(),
            );
        }
        for participant in &participants {
            if already_hung_up != Some(participant.pbx_call_id) {
                effects.push(
                    PbxEffect::Hangup {
                        call_id: participant.pbx_call_id,
                    }
                    .into(),
                );
            }
        }
        for appearance in appearances {
            effects.push(appearance_state_effect(
                &appearance,
                HandsetCallState::OnHook,
                true,
            ));
        }
        debug_assert!(self.invariant_error().is_none());
        effects
    }

    pub fn begin_immediate_divert(
        &mut self,
        device_id: &DeviceId,
        call_id: CallId,
        target: VoicemailTarget,
    ) -> Result<VoicemailPlan, VoicemailRejection> {
        let appearance = self
            .appearance_for_call(call_id)
            .filter(|appearance| &appearance.device_id == device_id)
            .cloned()
            .ok_or(VoicemailRejection::Conflict)?;
        let call = self
            .call_registry
            .pbx
            .get(&appearance.pbx_id)
            .ok_or(VoicemailRejection::Conflict)?;
        if appearance.state != CallState::Ringing
            || call.state != CallState::Ringing
            || call.active_appearance.is_some()
        {
            return Err(VoicemailRejection::InvalidPhase);
        }
        self.begin_voicemail_claim(&appearance, VoicemailAction::ImmediateDivert, target)
    }

    pub fn begin_selected_voicemail_transfer(
        &mut self,
        device_id: &DeviceId,
        target: VoicemailTarget,
    ) -> Result<VoicemailPlan, VoicemailRejection> {
        let selected = self
            .devices
            .get(device_id)
            .ok_or(VoicemailRejection::Conflict)?
            .selected_calls
            .iter()
            .copied()
            .collect::<Vec<_>>();
        if selected.len() != 1 {
            return Err(VoicemailRejection::Conflict);
        }
        let appearance = self
            .appearance_for_call(selected[0])
            .filter(|appearance| &appearance.device_id == device_id)
            .cloned()
            .ok_or(VoicemailRejection::Conflict)?;
        let call = self
            .call_registry
            .pbx
            .get(&appearance.pbx_id)
            .ok_or(VoicemailRejection::Conflict)?;
        let eligible = match appearance.state {
            CallState::Connected => {
                call.state == CallState::Connected && call.active_appearance == Some(appearance.id)
            }
            CallState::Held => {
                call.state == CallState::Held && call.active_appearance == Some(appearance.id)
            }
            _ => false,
        };
        if !eligible {
            return Err(VoicemailRejection::InvalidPhase);
        }
        self.begin_voicemail_claim(&appearance, VoicemailAction::TransferSelected, target)
    }

    fn begin_voicemail_claim(
        &mut self,
        appearance: &CallAppearance,
        action: VoicemailAction,
        target: VoicemailTarget,
    ) -> Result<VoicemailPlan, VoicemailRejection> {
        if self.redirect_claims.contains(&appearance.pbx_id)
            || self.conferences.by_pbx.contains_key(&appearance.pbx_id)
            || self.barges.by_handset.contains_key(&appearance.sccp_id)
            || self
                .transfers
                .for_leg(TransferLeg {
                    handset_call_id: appearance.sccp_id,
                    pbx_call_id: appearance.pbx_id,
                })
                .is_some()
        {
            return Err(VoicemailRejection::Conflict);
        }
        let mut transaction = self.voicemail.claim(
            appearance.device_id.clone(),
            appearance.sccp_id,
            appearance.pbx_id,
            action,
            target,
        )?;
        if !self.redirect_claims.insert(appearance.pbx_id) {
            let _ = self.voicemail.cancel(&appearance.device_id, transaction.id);
            return Err(VoicemailRejection::Conflict);
        }
        let operation = match self
            .voicemail
            .begin_execution(&appearance.device_id, transaction.id)
        {
            Ok(operation) => operation,
            Err(error) => {
                self.redirect_claims.remove(&appearance.pbx_id);
                let _ = self.voicemail.cancel(&appearance.device_id, transaction.id);
                return Err(error);
            }
        };
        transaction.phase = VoicemailPhase::Executing;
        debug_assert!(self.invariant_error().is_none());
        Ok(VoicemailPlan {
            transaction,
            effects: vec![PbxEffect::Voicemail { operation }.into()],
        })
    }

    pub fn voicemail_generation_is_active(
        &self,
        device_id: &DeviceId,
        transaction_id: VoicemailTransactionId,
    ) -> bool {
        self.voicemail
            .get(device_id)
            .is_some_and(|transaction| transaction.id == transaction_id)
    }

    pub fn abort_voicemail(
        &mut self,
        device_id: &DeviceId,
        transaction_id: VoicemailTransactionId,
    ) -> Result<VoicemailTransaction, VoicemailRejection> {
        let transaction = self.voicemail.cancel(device_id, transaction_id)?;
        self.redirect_claims.remove(&transaction.pbx_call_id);
        debug_assert!(self.invariant_error().is_none());
        Ok(transaction)
    }

    pub fn voicemail_succeeded(
        &mut self,
        device_id: &DeviceId,
        transaction_id: VoicemailTransactionId,
    ) -> Result<VoicemailTerminalOutcome, VoicemailRejection> {
        let transaction = self
            .voicemail
            .get(device_id)
            .filter(|transaction| transaction.id == transaction_id)
            .cloned()
            .ok_or(VoicemailRejection::Conflict)?;
        let appearance_is_owned = self
            .appearance_for_call(transaction.handset_call_id)
            .is_some_and(|appearance| appearance.pbx_id == transaction.pbx_call_id);
        let owner_disconnected = !self.devices.contains_key(&transaction.device_id);
        if !self.redirect_claims.contains(&transaction.pbx_call_id)
            || (!appearance_is_owned && !owner_disconnected)
        {
            return Err(VoicemailRejection::Conflict);
        }
        let effects = self
            .call_registry
            .pbx
            .get(&transaction.pbx_call_id)
            .ok_or(VoicemailRejection::Conflict)?
            .appearance_ids
            .iter()
            .filter_map(|appearance_id| self.call_registry.appearances.get(appearance_id))
            .map(|appearance| appearance_state_effect(appearance, HandsetCallState::OnHook, true))
            .collect();
        let transaction = self.voicemail.commit(device_id, transaction_id)?;
        self.redirect_claims.remove(&transaction.pbx_call_id);
        self.remove_pbx_call(transaction.pbx_call_id);
        debug_assert!(self.invariant_error().is_none());
        Ok(VoicemailTerminalOutcome {
            transaction,
            effects,
        })
    }

    pub fn complete_voicemail_native(
        &mut self,
        device_id: &DeviceId,
        transaction_id: VoicemailTransactionId,
        pbx_call_id: PbxCallId,
    ) -> Result<VoicemailNativeOutcome, VoicemailRejection> {
        if self.voicemail.get(device_id).is_some_and(|transaction| {
            transaction.id == transaction_id && transaction.pbx_call_id == pbx_call_id
        }) {
            return self
                .voicemail_succeeded(device_id, transaction_id)
                .map(VoicemailNativeOutcome::Committed);
        }
        if !self.call_registry.pbx.contains_key(&pbx_call_id)
            && self.voicemail.for_pbx(pbx_call_id).is_none()
            && !self.redirect_claims.contains(&pbx_call_id)
        {
            return Ok(VoicemailNativeOutcome::CallAlreadyEnded);
        }
        Err(VoicemailRejection::Conflict)
    }

    /// Start an isolated consultation call for one connected source. The
    /// caller executes the returned hold/create/handset effects in order and
    /// records each completed setup milestone before it can continue.
    pub fn begin_transfer(
        &mut self,
        request: TransferConsultationRequest,
    ) -> Result<Vec<DriverEffect>, TransferRejection> {
        if request.source_call_id == request.consultation_call_id
            || request.binding.device_id
                != self
                    .appearance_for_call(request.source_call_id)
                    .map(|appearance| appearance.device_id.clone())
                    .ok_or(TransferRejection::WrongCall)?
        {
            return Err(TransferRejection::WrongCall);
        }
        let source = self
            .appearance_for_call(request.source_call_id)
            .cloned()
            .ok_or(TransferRejection::WrongCall)?;
        let source_call = self
            .call_registry
            .pbx
            .get(&source.pbx_id)
            .ok_or(TransferRejection::WrongCall)?;
        if source.state != CallState::Connected
            || source_call.state != CallState::Connected
            || source_call.active_appearance != Some(source.id)
            || self
                .devices
                .get(&source.device_id)
                .is_none_or(|device| device.active_call != Some(request.source_call_id))
        {
            return Err(TransferRejection::InvalidPhase);
        }
        if self.redirect_claims.contains(&source.pbx_id)
            || self.conferences.by_pbx.contains_key(&source.pbx_id)
            || self.barges.by_handset.contains_key(&request.source_call_id)
            || self.transfers.get(&source.device_id).is_some()
            || self
                .call_registry
                .by_sccp
                .contains_key(&request.consultation_call_id)
        {
            return Err(TransferRejection::Conflict);
        }

        let transaction_id = self.transfers.allocate_id();
        let mut effects = self
            .begin_additional_phone_call(
                request.consultation_call_id,
                request.binding,
                request.codec,
                request.now,
            )
            .map_err(|_| TransferRejection::Conflict)?;
        let consultation = self
            .appearance_for_call(request.consultation_call_id)
            .cloned()
            .ok_or(TransferRejection::Conflict)?;
        if let Some(appearance_id) = self
            .call_registry
            .by_sccp
            .get(&request.consultation_call_id)
            .copied()
            && let Some(appearance) = self.call_registry.appearances.get_mut(&appearance_id)
        {
            appearance.state = CallState::TransferCollecting;
        }
        if let Some(call) = self.call_registry.pbx.get_mut(&consultation.pbx_id) {
            call.state = CallState::TransferCollecting;
        }
        for effect in &mut effects {
            if let DriverEffect::Backend(PbxEffect::CreateChannel {
                handset_call_id,
                call_id,
                binding,
                codec,
            }) = effect
                && *call_id == consultation.pbx_id
            {
                *effect = PbxEffect::CreateConsultationChannel {
                    source_call_id: source.pbx_id,
                    handset_call_id: *handset_call_id,
                    call_id: *call_id,
                    binding: binding.clone(),
                    codec: *codec,
                }
                .into();
            }
        }
        let mut transaction = TransferTransaction::consultation(
            transaction_id,
            source.device_id.clone(),
            TransferLeg {
                handset_call_id: source.sccp_id,
                pbx_call_id: source.pbx_id,
            },
            TransferSourceState::Connected,
            request.complete_on_hangup,
        );
        transaction.attach_consultation(TransferLeg {
            handset_call_id: consultation.sccp_id,
            pbx_call_id: consultation.pbx_id,
        })?;
        if let Err(error) = self.transfers.insert(transaction) {
            self.remove_pbx_call(consultation.pbx_id);
            let _ = self.resume(source.sccp_id);
            return Err(error);
        }
        if !effects.iter().any(|effect| {
            matches!(
                effect,
                DriverEffect::Backend(PbxEffect::Hold { call_id }) if *call_id == source.pbx_id
            )
        }) {
            let _ = self.transfers.cancel(
                &source.device_id,
                transaction_id,
                TransferCancellationReason::ConsultationFailure,
                None,
            );
            self.remove_pbx_call(consultation.pbx_id);
            let _ = self.resume(source.sccp_id);
            return Err(TransferRejection::Conflict);
        }
        self.set_call_selected(&source.device_id, request.source_call_id, true);
        self.set_call_selected(&source.device_id, request.consultation_call_id, true);
        effects.push(
            HandsetEffect::BeginTransfer {
                device_id: source.device_id,
                source_call_id: request.source_call_id,
                consultation_call_id: request.consultation_call_id,
                consultation_line_instance: consultation.line_instance,
                codec: request.codec,
            }
            .into(),
        );
        debug_assert!(self.invariant_error().is_none());
        Ok(effects)
    }

    pub fn transfer_transaction(&self, call_id: CallId) -> Option<&TransferTransaction> {
        let appearance = self.appearance_for_call(call_id)?;
        self.transfers.for_leg(TransferLeg {
            handset_call_id: call_id,
            pbx_call_id: appearance.pbx_id,
        })
    }

    pub fn transfer_transaction_for_device(
        &self,
        device_id: &DeviceId,
    ) -> Option<&TransferTransaction> {
        self.transfers.get(device_id)
    }

    pub fn transfer_generation_is_active(
        &self,
        device_id: &DeviceId,
        transaction_id: TransferId,
    ) -> bool {
        self.transfers
            .get(device_id)
            .is_some_and(|transaction| transaction.id == transaction_id)
    }

    pub fn transfer_setup_completed(
        &mut self,
        device_id: &DeviceId,
        transaction_id: TransferId,
        milestone: TransferSetupMilestone,
    ) -> Result<(), TransferRejection> {
        self.transfers
            .record_setup_milestone(device_id, transaction_id, milestone)
    }

    pub fn defer_transfer_action(
        &mut self,
        device_id: &DeviceId,
        transaction_id: TransferId,
        action: DeferredTransferAction,
    ) -> Result<(), TransferRejection> {
        self.transfers
            .defer_action(device_id, transaction_id, action)
    }

    fn advance_transfer_for_pbx(&mut self, pbx_id: PbxCallId, phase: TransferPhase) {
        let Some(appearance) = self
            .call_registry
            .pbx
            .get(&pbx_id)
            .and_then(|call| call.appearance_ids.first())
            .and_then(|id| self.call_registry.appearances.get(id))
            .cloned()
        else {
            return;
        };
        let leg = TransferLeg {
            handset_call_id: appearance.sccp_id,
            pbx_call_id: pbx_id,
        };
        let Some(device_id) = self
            .transfers
            .for_leg(leg)
            .filter(|transaction| transaction.consultation == Some(leg))
            .map(|transaction| transaction.device_id.clone())
        else {
            return;
        };
        let _ = self
            .transfers
            .get_mut(&device_id)
            .expect("transfer found by device")
            .advance_consultation(leg, phase);
    }

    pub fn complete_transfer(
        &mut self,
        device_id: &DeviceId,
        consultation_call_id: CallId,
        trigger: TransferTrigger,
    ) -> Result<TransferCompletionPlan, TransferRejection> {
        let consultation = self
            .appearance_for_call(consultation_call_id)
            .filter(|appearance| &appearance.device_id == device_id)
            .ok_or(TransferRejection::WrongCall)?;
        let completion = self.transfers.begin_completion(
            device_id,
            trigger,
            TransferLeg {
                handset_call_id: consultation.sccp_id,
                pbx_call_id: consultation.pbx_id,
            },
        )?;
        Ok(TransferCompletionPlan {
            effects: vec![
                PbxEffect::Transfer {
                    operation: completion.clone(),
                }
                .into(),
            ],
            completion,
        })
    }

    pub fn complete_device_transfer(
        &mut self,
        device_id: &DeviceId,
        reported_call_id: Option<CallId>,
        trigger: TransferTrigger,
    ) -> Result<TransferCompletionPlan, TransferRejection> {
        let transaction = self
            .transfers
            .get(device_id)
            .cloned()
            .ok_or(TransferRejection::WrongCall)?;
        if let Some(reported_call_id) = reported_call_id.filter(|call_id| call_id.0 != 0) {
            let reported = self
                .appearance_for_call(reported_call_id)
                .filter(|appearance| &appearance.device_id == device_id)
                .map(|appearance| TransferLeg {
                    handset_call_id: appearance.sccp_id,
                    pbx_call_id: appearance.pbx_id,
                })
                .ok_or(TransferRejection::WrongCall)?;
            if !transaction.contains(reported) {
                return Err(TransferRejection::WrongCall);
            }
        }
        let consultation = transaction
            .consultation
            .ok_or(TransferRejection::ConsultationMissing)?;
        self.complete_transfer(device_id, consultation.handset_call_id, trigger)
    }

    /// Claim one native transfer for exactly two selected local calls. A held
    /// call is always the source; otherwise the active call is the target and
    /// call identifiers provide a stable fallback order.
    pub fn direct_transfer(
        &mut self,
        device_id: &DeviceId,
    ) -> Result<TransferCompletionPlan, TransferRejection> {
        if self.transfers.get(device_id).is_some() {
            return Err(TransferRejection::Conflict);
        }
        let device = self
            .devices
            .get(device_id)
            .ok_or(TransferRejection::WrongCall)?;
        if device.selected_calls.len() != 2 {
            return Err(TransferRejection::InvalidSelection);
        }
        let active_call = device.active_call;
        let mut selected = device.selected_calls.iter().copied().collect::<Vec<_>>();
        selected.sort_by_key(|call_id| call_id.0);
        let mut appearances = selected
            .iter()
            .map(|call_id| {
                self.appearance_for_call(*call_id)
                    .filter(|appearance| &appearance.device_id == device_id)
                    .cloned()
                    .ok_or(TransferRejection::WrongCall)
            })
            .collect::<Result<Vec<_>, _>>()?;
        if appearances.iter().any(|appearance| {
            !matches!(appearance.state, CallState::Connected | CallState::Held)
                || self.redirect_claims.contains(&appearance.pbx_id)
                || self.conferences.by_pbx.contains_key(&appearance.pbx_id)
                || self.barges.by_handset.contains_key(&appearance.sccp_id)
        }) || appearances[0].pbx_id == appearances[1].pbx_id
        {
            return Err(TransferRejection::InvalidSelection);
        }
        appearances.sort_by_key(|appearance| {
            let held_rank = u8::from(appearance.state != CallState::Held);
            let active_rank = u8::from(Some(appearance.sccp_id) == active_call);
            (held_rank, active_rank, appearance.sccp_id.0)
        });
        let source = appearances.remove(0);
        let consultation = appearances.remove(0);
        let source_state = match source.state {
            CallState::Connected => TransferSourceState::Connected,
            CallState::Held => TransferSourceState::Held,
            _ => return Err(TransferRejection::InvalidSelection),
        };
        let transaction_id = self.transfers.allocate_id();
        let transaction = TransferTransaction::direct(
            transaction_id,
            device_id.clone(),
            TransferLeg {
                handset_call_id: source.sccp_id,
                pbx_call_id: source.pbx_id,
            },
            source_state,
            TransferLeg {
                handset_call_id: consultation.sccp_id,
                pbx_call_id: consultation.pbx_id,
            },
        )?;
        self.transfers.insert(transaction)?;
        self.complete_transfer(
            device_id,
            consultation.sccp_id,
            TransferTrigger::TransferKey,
        )
    }

    pub fn transfer_succeeded(
        &mut self,
        device_id: &DeviceId,
        transaction_id: TransferId,
    ) -> Option<TransferTerminalOutcome> {
        let transaction = self.transfers.commit(device_id, transaction_id)?;
        let mut appearances = Vec::new();
        for pbx_id in [
            transaction.source.pbx_call_id,
            transaction.consultation?.pbx_call_id,
        ] {
            let ids = self.call_registry.pbx.get(&pbx_id)?.appearance_ids.clone();
            appearances.extend(
                ids.into_iter()
                    .filter_map(|id| self.call_registry.appearances.get(&id).cloned()),
            );
        }
        let effects = appearances
            .iter()
            .map(|appearance| appearance_state_effect(appearance, HandsetCallState::OnHook, true))
            .collect();
        self.remove_pbx_call(transaction.source.pbx_call_id);
        self.remove_pbx_call(
            transaction
                .consultation
                .expect("committed transfer has a consultation leg")
                .pbx_call_id,
        );
        debug_assert!(self.invariant_error().is_none());
        Some(TransferTerminalOutcome {
            transaction,
            effects,
        })
    }

    pub fn abort_transfer(
        &mut self,
        device_id: &DeviceId,
        transaction_id: TransferId,
        reason: TransferCancellationReason,
    ) -> Result<TransferTerminalOutcome, TransferRejection> {
        let cancellation = self
            .transfers
            .cancel(device_id, transaction_id, reason, None)?;
        let transaction = cancellation.transaction;
        let progress = transaction.execution_progress.clone();
        if transaction.mode == TransferMode::Direct {
            let mut effects = Vec::new();
            for (terminated, leg) in [
                (transaction.source_terminated, transaction.source),
                (
                    transaction.consultation_terminated,
                    transaction
                        .consultation
                        .expect("direct transfer has a consultation leg"),
                ),
            ] {
                if !terminated {
                    continue;
                }
                if let Some(call) = self.call_registry.pbx.get(&leg.pbx_call_id) {
                    effects.extend(
                        call.appearance_ids
                            .iter()
                            .filter_map(|id| self.call_registry.appearances.get(id))
                            .map(|appearance| {
                                appearance_state_effect(appearance, HandsetCallState::OnHook, true)
                            }),
                    );
                }
                self.remove_pbx_call(leg.pbx_call_id);
            }
            return Ok(TransferTerminalOutcome {
                transaction,
                effects,
            });
        }
        let mut effects = Vec::new();
        if let Some(consultation) = transaction.consultation {
            if progress.completed(TransferSetupMilestone::ConsultationChannelCreated)
                && !transaction.consultation_terminated
            {
                effects.push(
                    PbxEffect::Hangup {
                        call_id: consultation.pbx_call_id,
                    }
                    .into(),
                );
            }
            if progress.completed(TransferSetupMilestone::ConsultationHandsetStarted)
                && let Some(appearance) = self
                    .appearance_for_call(consultation.handset_call_id)
                    .cloned()
            {
                effects.push(appearance_state_effect(
                    &appearance,
                    HandsetCallState::OnHook,
                    true,
                ));
            }
            self.remove_pbx_call(consultation.pbx_call_id);
        }
        if cancellation.source_recovery == TransferSourceRecovery::RestoreConnected
            && self
                .appearance_for_call(transaction.source.handset_call_id)
                .is_some()
        {
            effects.extend(
                self.resume(transaction.source.handset_call_id)
                    .into_iter()
                    .filter(|effect| match effect {
                        DriverEffect::Backend(PbxEffect::Resume { .. }) => {
                            progress.completed(TransferSetupMilestone::SourceBackendHeld)
                        }
                        DriverEffect::Handset(_) => {
                            progress.completed(TransferSetupMilestone::SourceHandsetHeld)
                        }
                        _ => true,
                    }),
            );
        }
        if cancellation.source_recovery == TransferSourceRecovery::SourceGone
            && transaction.source_terminated
        {
            if let Some(call) = self.call_registry.pbx.get(&transaction.source.pbx_call_id) {
                effects.extend(
                    call.appearance_ids
                        .iter()
                        .filter_map(|id| self.call_registry.appearances.get(id))
                        .map(|appearance| {
                            appearance_state_effect(appearance, HandsetCallState::OnHook, true)
                        }),
                );
            }
            self.remove_pbx_call(transaction.source.pbx_call_id);
        }
        debug_assert!(self.invariant_error().is_none());
        Ok(TransferTerminalOutcome {
            transaction,
            effects,
        })
    }

    pub fn hangup(&mut self, call_id: CallId) -> Vec<DriverEffect> {
        // The protocol's immediate OnHook presentation is not sufficient to
        // retire its indexed SessionCall. Emit the idempotent terminal effect
        // as well so CloseCall removes wire/controller ownership together.
        self.hangup_internal(call_id, true)
    }

    /// Terminate a call whose failure did not originate from a physical
    /// handset OnHook. The active appearance still needs explicit terminal UI
    /// and media cleanup before controller ownership is removed.
    pub fn terminate(&mut self, call_id: CallId) -> Vec<DriverEffect> {
        self.hangup_internal(call_id, true)
    }

    fn hangup_internal(
        &mut self,
        call_id: CallId,
        cleanup_current_appearance: bool,
    ) -> Vec<DriverEffect> {
        #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
        if let Some(effect) = self.complete_remote_hangup(call_id) {
            return vec![effect];
        }
        if let Some(session) = self.conference_session(call_id).cloned() {
            if session
                .pending_invite
                .as_ref()
                .is_some_and(|invite| invite.participant.handset_call_id == call_id)
            {
                return self.abort_conference_invite(call_id, true, true, true);
            }
            if session.phase == ConferencePhase::Consultation
                && session.consultation_handset_call_id == call_id
            {
                return self.cancel_conference(call_id);
            }
            if session.phase == ConferencePhase::Active
                || (session.phase == ConferencePhase::Merging
                    && session.origin == ConferenceOrigin::Selection)
            {
                if session.phase == ConferencePhase::Active {
                    let Some(pbx_id) = self
                        .appearance_for_call(call_id)
                        .map(|appearance| appearance.pbx_id)
                    else {
                        return Vec::new();
                    };
                    return self.active_conference_departure(session, pbx_id, None, true);
                }
                return self.end_conference_internal(session, true, None);
            }
        }
        if self.barges.by_handset.contains_key(&call_id) {
            return self.end_barge(call_id, true, true);
        }
        let Some(appearance) = self.appearance_for_call(call_id).cloned() else {
            return Vec::new();
        };
        let Some(call) = self.call_registry.pbx.get(&appearance.pbx_id) else {
            return Vec::new();
        };
        let appearance_ids = call.appearance_ids.clone();
        if call
            .active_appearance
            .is_some_and(|owner| owner != appearance.id)
        {
            return Vec::new();
        }
        if call.active_appearance.is_none()
            && call.state == CallState::Ringing
            && appearance_ids.len() > 1
        {
            self.remove_appearance(appearance.id);
            debug_assert!(self.invariant_error().is_none());
            return Vec::new();
        }
        let mut effects = vec![
            PbxEffect::Hangup {
                call_id: appearance.pbx_id,
            }
            .into(),
        ];
        if appearance.auto_answer_mode == Some(AutoAnswerMode::OneWay) {
            effects.push(
                HandsetEffect::SetMicrophoneMode {
                    device_id: appearance.device_id.clone(),
                    call_id: appearance.sccp_id,
                    enabled: true,
                }
                .into(),
            );
        }
        effects.extend(
            appearance_ids
                .into_iter()
                .filter(|id| cleanup_current_appearance || *id != appearance.id)
                .filter_map(|id| self.call_registry.appearances.get(&id))
                .flat_map(appearance_terminal_effects),
        );
        self.remove_pbx_call(appearance.pbx_id);
        debug_assert!(self.invariant_error().is_none());
        effects
    }

    pub fn pbx_hangup(&mut self, pbx_id: PbxCallId) -> Option<CallSnapshot> {
        self.pbx_hangup_with_effects(pbx_id)?.primary
    }

    /// Detach one PBX call immediately while optionally leaving the exact
    /// active handset presentation up for a bounded remote-hangup tone.
    /// Conference, transfer, barge, held/ringing and in-flight switch state
    /// always take the ordinary immediate cleanup path.
    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
    pub(crate) fn begin_remote_hangup(
        &mut self,
        pbx_id: PbxCallId,
        tone: Option<Tone>,
        delay: Duration,
        now: Instant,
    ) -> Option<RemoteHangupPlan> {
        let eligible =
            tone.is_some() && !delay.is_zero() && self.remote_hangup_owner(pbx_id).is_some();
        let pending = eligible
            .then(|| self.allocate_remote_hangup_token())
            .flatten();
        let owner = pending.zip(self.remote_hangup_owner(pbx_id));
        let mut outcome = self.pbx_hangup_with_effects(pbx_id)?;
        if let (Some(tone), Some((token, owner))) = (tone, owner) {
            outcome.effects.retain(|effect| {
                !matches!(
                    effect,
                    DriverEffect::Handset(HandsetEffect::SetCallState {
                        device_id,
                        call_id,
                        ..
                    }) if device_id == &owner.device_id && *call_id == owner.sccp_id
                )
            });
            outcome.effects.push(
                HandsetEffect::SetCallState {
                    device_id: owner.device_id.clone(),
                    call_id: owner.sccp_id,
                    state: HandsetCallState::Connected,
                    stop_media: true,
                }
                .into(),
            );
            outcome.effects.push(
                HandsetEffect::StartTone {
                    device_id: owner.device_id.clone(),
                    call_id: owner.sccp_id,
                    tone,
                }
                .into(),
            );
            self.pending_remote_hangups.insert(
                owner.sccp_id,
                PendingRemoteHangup {
                    token,
                    device_id: owner.device_id,
                    call_id: owner.sccp_id,
                    deadline: now + delay,
                },
            );
            return Some(RemoteHangupPlan {
                outcome,
                pending: Some(token),
            });
        }
        Some(RemoteHangupPlan {
            outcome,
            pending: None,
        })
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
    pub(crate) fn expire_remote_hangups(&mut self, now: Instant) -> Vec<DriverEffect> {
        let mut due = self
            .pending_remote_hangups
            .values()
            .filter(|pending| pending.deadline <= now)
            .map(|pending| (pending.deadline, pending.token, pending.call_id))
            .collect::<Vec<_>>();
        due.sort_by_key(|(deadline, token, call_id)| (*deadline, token.0, call_id.0));
        due.into_iter()
            .filter_map(|(_, token, _)| self.complete_remote_hangup_token(token))
            .collect()
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
    pub(crate) fn complete_remote_hangup_token(
        &mut self,
        token: RemoteHangupToken,
    ) -> Option<DriverEffect> {
        let call_id = self
            .pending_remote_hangups
            .iter()
            .find_map(|(call_id, pending)| (pending.token == token).then_some(*call_id))?;
        self.complete_remote_hangup(call_id)
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
    pub(crate) fn complete_remote_hangup(&mut self, call_id: CallId) -> Option<DriverEffect> {
        let pending = self.pending_remote_hangups.remove(&call_id)?;
        Some(
            HandsetEffect::SetCallState {
                device_id: pending.device_id,
                call_id: pending.call_id,
                state: HandsetCallState::OnHook,
                stop_media: true,
            }
            .into(),
        )
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
    pub(crate) fn drain_remote_hangups(&mut self) -> Vec<DriverEffect> {
        let mut pending = self
            .pending_remote_hangups
            .values()
            .map(|pending| (pending.token.0, pending.call_id))
            .collect::<Vec<_>>();
        pending.sort_by_key(|(generation, call_id)| (*generation, call_id.0));
        pending
            .into_iter()
            .filter_map(|(_, call_id)| self.complete_remote_hangup(call_id))
            .collect()
    }

    /// Restore every device microphone still owned by a committed one-way
    /// auto-answer before the handset server is stopped. Clearing the marker
    /// while producing the effects makes repeated shutdown/drain calls exact.
    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
    pub(crate) fn drain_one_way_microphones(&mut self) -> Vec<DriverEffect> {
        let mut owned = self
            .call_registry
            .appearances
            .values()
            .filter(|appearance| appearance.auto_answer_mode == Some(AutoAnswerMode::OneWay))
            .map(|appearance| {
                (
                    appearance.device_id.clone(),
                    appearance.sccp_id,
                    appearance.id,
                )
            })
            .collect::<Vec<_>>();
        owned.sort_by_key(|(device_id, call_id, _)| (device_id.clone(), call_id.0));
        let mut effects = Vec::with_capacity(owned.len());
        for (device_id, call_id, appearance_id) in owned {
            let Some(appearance) = self.call_registry.appearances.get_mut(&appearance_id) else {
                continue;
            };
            appearance.auto_answer_mode = None;
            effects.push(
                HandsetEffect::SetMicrophoneMode {
                    device_id,
                    call_id,
                    enabled: true,
                }
                .into(),
            );
        }
        effects
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
    fn remote_hangup_owner(&self, pbx_id: PbxCallId) -> Option<CallAppearance> {
        let call = self.call_registry.pbx.get(&pbx_id)?;
        let owner_id = call.active_appearance?;
        let owner = self.call_registry.appearances.get(&owner_id)?;
        let transfer_owned = self.transfers.transactions().any(|transaction| {
            transaction.source.pbx_call_id == pbx_id
                || transaction
                    .consultation
                    .is_some_and(|leg| leg.pbx_call_id == pbx_id)
        });
        let transition_owned = self.pending_call_transitions.values().any(|pending| {
            pending.transition.target_pbx_id == pbx_id
                || pending.transition.previous_pbx_id == Some(pbx_id)
        });
        if call.state != CallState::Connected
            || owner.state != CallState::Connected
            || self
                .devices
                .get(&owner.device_id)
                .is_none_or(|device| device.active_call != Some(owner.sccp_id))
            || transfer_owned
            || transition_owned
            || self.conferences.by_pbx.contains_key(&pbx_id)
            || self.barges.by_pbx.contains_key(&pbx_id)
            || self.barges.groups.contains_key(&pbx_id)
        {
            return None;
        }
        Some(owner.clone())
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
    fn allocate_remote_hangup_token(&mut self) -> Option<RemoteHangupToken> {
        let next = self.next_remote_hangup_generation.checked_add(1)?;
        let token = RemoteHangupToken(self.next_remote_hangup_generation);
        self.next_remote_hangup_generation = next;
        Some(token)
    }

    pub fn pbx_hangup_with_effects(&mut self, pbx_id: PbxCallId) -> Option<PbxHangupOutcome> {
        let transition_primary = self.call_by_pbx(pbx_id);
        #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
        let transition_effects = self.abort_call_transitions_for_pbx(pbx_id);
        #[cfg(not(any(test, feature = "asterisk-22", feature = "asterisk-23")))]
        let transition_effects = Vec::new();
        if !transition_effects.is_empty() && !self.call_registry.pbx.contains_key(&pbx_id) {
            return Some(PbxHangupOutcome {
                primary: transition_primary,
                effects: transition_effects,
            });
        }
        let transfer = {
            let mut transactions = self.transfers.transactions();
            transactions
                .find(|transaction| {
                    transaction.source.pbx_call_id == pbx_id
                        || transaction
                            .consultation
                            .is_some_and(|leg| leg.pbx_call_id == pbx_id)
                })
                .cloned()
        };
        if let Some(transaction) = transfer {
            let primary = self.call_by_pbx(pbx_id);
            if transaction.phase == TransferPhase::Completing {
                let terminated_leg = if transaction.source.pbx_call_id == pbx_id {
                    transaction.source
                } else {
                    transaction
                        .consultation
                        .expect("indexed transfer has a consultation leg")
                };
                let _ = self.transfers.note_completing_hangup(terminated_leg);
                return Some(PbxHangupOutcome {
                    primary,
                    effects: Vec::new(),
                });
            }
            let source_hung_up = transaction.source.pbx_call_id == pbx_id;
            let reason = if source_hung_up {
                TransferCancellationReason::SourceHangup
            } else {
                TransferCancellationReason::ConsultationHangup
            };
            let transfer = self
                .abort_transfer(&transaction.device_id, transaction.id, reason)
                .ok()?;
            if source_hung_up {
                let mut outcome = self.pbx_hangup_with_effects(pbx_id)?;
                let mut effects = transfer.effects;
                effects.append(&mut outcome.effects);
                outcome.effects = effects;
                return Some(outcome);
            }
            return Some(PbxHangupOutcome {
                primary,
                effects: transfer.effects,
            });
        }
        if let Some(session) = self.conference_session_by_pbx(pbx_id).cloned() {
            self.conference_mutations
                .remove(&ConferenceMutationOwner::Session(session.id));
            let primary = self.call_by_pbx(pbx_id);
            if let Some(pending) = session.pending_participant_mutation
                && pending.kind == ConferenceParticipantMutationKind::Remove
                && pending.call_id == pbx_id
            {
                let mut effects = self
                    .conference_participant_removed(session.id, pending.participant_id)
                    .unwrap_or_default();
                effects.extend(self.conference_announcement_effects(
                    session.id,
                    ConferenceAnnouncement::ParticipantRemoved(pending.participant_id),
                ));
                debug_assert!(self.invariant_error().is_none());
                return Some(PbxHangupOutcome { primary, effects });
            }
            if session
                .pending_invite
                .as_ref()
                .is_some_and(|invite| invite.participant.pbx_call_id == pbx_id)
            {
                let effects = self.abort_conference_invite(
                    session
                        .pending_invite
                        .as_ref()
                        .expect("checked pending invite")
                        .participant
                        .handset_call_id,
                    false,
                    true,
                    true,
                );
                debug_assert!(self.invariant_error().is_none());
                return Some(PbxHangupOutcome { primary, effects });
            }
            let effects = match session.phase {
                ConferencePhase::Consultation if pbx_id == session.consultation_call_id => self
                    .abort_conference(
                        session.consultation_handset_call_id,
                        false,
                        false,
                        true,
                        true,
                    ),
                ConferencePhase::Consultation => {
                    self.end_conference_internal(session, false, Some(pbx_id))
                }
                ConferencePhase::Merging => {
                    self.end_conference_internal(session, true, Some(pbx_id))
                }
                ConferencePhase::Active => {
                    self.active_conference_departure(session, pbx_id, Some(pbx_id), true)
                }
            };
            debug_assert!(self.invariant_error().is_none());
            return Some(PbxHangupOutcome { primary, effects });
        }
        if let Some(handset_call_id) = self.barges.by_pbx.get(&pbx_id).copied() {
            let primary = self.call(handset_call_id);
            let effects = self.end_barge_internal(handset_call_id, true, false, true);
            debug_assert!(self.invariant_error().is_none());
            return Some(PbxHangupOutcome { primary, effects });
        }
        let mut effects = transition_effects;
        effects.extend(self.end_barges_for_target(pbx_id));
        effects.extend(
            self.call_registry
                .pbx
                .get(&pbx_id)?
                .appearance_ids
                .iter()
                .filter_map(|id| self.call_registry.appearances.get(id))
                .flat_map(appearance_terminal_effects),
        );
        let (_, primary) = self.remove_pbx_call(pbx_id)?;
        debug_assert!(self.invariant_error().is_none());
        Some(PbxHangupOutcome { primary, effects })
    }

    pub fn call(&self, call_id: CallId) -> Option<CallSnapshot> {
        let appearance_id = self.call_registry.by_sccp.get(&call_id)?;
        self.call_snapshot(*appearance_id)
    }

    pub fn call_state(&self, call_id: CallId) -> Option<CallState> {
        self.appearance_for_call(call_id)
            .map(|appearance| appearance.state)
    }

    pub fn call_pbx_id(&self, call_id: CallId) -> Option<PbxCallId> {
        self.appearance_for_call(call_id)
            .map(|appearance| appearance.pbx_id)
    }

    pub fn call_device_id(&self, call_id: CallId) -> Option<&DeviceId> {
        self.appearance_for_call(call_id)
            .map(|appearance| &appearance.device_id)
    }

    pub fn call_line_instance(&self, call_id: CallId) -> Option<u32> {
        self.appearance_for_call(call_id)
            .map(|appearance| appearance.line_instance)
    }

    /// Changes the codec for one outbound channel before either media stream
    /// has started. The previous codec is returned so an adapter can restore
    /// controller state if its native channel update fails.
    pub fn set_pre_dial_codec(
        &mut self,
        pbx_id: PbxCallId,
        codec: Codec,
    ) -> Result<Codec, CodecPreferenceRejection> {
        let call = self
            .call_registry
            .pbx
            .get(&pbx_id)
            .ok_or(CodecPreferenceRejection::Unavailable)?;
        if call.direction != CallDirection::Outbound
            || !matches!(call.state, CallState::Collecting | CallState::Calling)
        {
            return Err(CodecPreferenceRejection::NotPreDial);
        }
        let [appearance_id] = call.appearance_ids.as_slice() else {
            return Err(CodecPreferenceRejection::Ambiguous);
        };
        let appearance = self
            .call_registry
            .appearances
            .get(appearance_id)
            .ok_or(CodecPreferenceRejection::Unavailable)?;
        if appearance.audio != MediaStreamState::Closed
            || appearance.audio_transmit != MediaStreamState::Closed
            || !appearance.video.is_idle()
        {
            return Err(CodecPreferenceRejection::NotPreDial);
        }
        let previous = appearance.codec;
        self.call_registry
            .appearances
            .get_mut(appearance_id)
            .expect("validated call appearance")
            .codec = codec;
        debug_assert!(self.invariant_error().is_none());
        Ok(previous)
    }

    pub fn call_by_pbx(&self, pbx_id: PbxCallId) -> Option<CallSnapshot> {
        self.call_registry
            .pbx
            .get(&pbx_id)
            .and_then(|call| call.appearance_ids.first())
            .and_then(|appearance_id| self.call_snapshot(*appearance_id))
    }

    pub fn calls(&self) -> impl Iterator<Item = CallSnapshot> + '_ {
        self.call_registry
            .appearances
            .keys()
            .filter_map(|appearance_id| self.call_snapshot(*appearance_id))
    }

    pub fn pbx_call(&self, pbx_id: PbxCallId) -> Option<&PbxCall> {
        self.call_registry.pbx.get(&pbx_id)
    }

    pub fn call_metadata(&self, pbx_id: PbxCallId) -> Option<&CallMetadata> {
        self.call_registry
            .pbx
            .get(&pbx_id)
            .map(|call| &call.metadata)
    }

    /// Atomically replaces PBX-owned channel metadata after the complete value
    /// validates.
    pub fn set_call_metadata(
        &mut self,
        pbx_id: PbxCallId,
        metadata: CallMetadata,
    ) -> Result<bool, MetadataError> {
        metadata.validate()?;
        let Some(call) = self.call_registry.pbx.get_mut(&pbx_id) else {
            return Ok(false);
        };
        if call.metadata == metadata {
            return Ok(true);
        }
        call.metadata = metadata;
        Ok(true)
    }

    pub fn active_call_id(&self, pbx_id: PbxCallId) -> Option<CallId> {
        let call = self.call_registry.pbx.get(&pbx_id)?;
        self.call_registry
            .appearances
            .get(&call.active_appearance?)
            .map(|appearance| appearance.sccp_id)
    }

    pub fn call_appearance(&self, appearance_id: CallAppearanceId) -> Option<&CallAppearance> {
        self.call_registry.appearances.get(&appearance_id)
    }

    pub fn appearance_for_call(&self, call_id: CallId) -> Option<&CallAppearance> {
        self.call_registry
            .by_sccp
            .get(&call_id)
            .and_then(|appearance_id| self.call_registry.appearances.get(appearance_id))
    }

    fn appearance_for_call_mut(&mut self, call_id: CallId) -> Option<&mut CallAppearance> {
        let appearance_id = self.call_registry.by_sccp.get(&call_id)?;
        self.call_registry.appearances.get_mut(appearance_id)
    }

    pub fn call_info(&self, call_id: CallId) -> Option<&CallInfo> {
        self.appearance_for_call(call_id)
            .map(|appearance| &appearance.info)
    }

    /// Replaces one appearance's party metadata and returns its handset update.
    pub fn set_call_info(&mut self, call_id: CallId, info: CallInfo) -> Vec<DriverEffect> {
        let Some(appearance_id) = self.call_registry.by_sccp.get(&call_id).copied() else {
            return Vec::new();
        };
        let Some(appearance) = self.call_registry.appearances.get_mut(&appearance_id) else {
            return Vec::new();
        };
        if appearance.info == info {
            return Vec::new();
        }
        appearance.info = info.clone();
        let device_id = appearance.device_id.clone();
        let pbx_id = appearance.pbx_id;
        self.refresh_conference_participant_identity(pbx_id);
        vec![
            HandsetEffect::SetCallInfo {
                device_id,
                call_id,
                info,
            }
            .into(),
        ]
    }

    /// Updates every presentation of one PBX call in stable appearance order.
    pub fn update_call_info_by_pbx(
        &mut self,
        pbx_id: PbxCallId,
        mut update: impl FnMut(&CallInfo) -> CallInfo,
    ) -> Vec<DriverEffect> {
        let appearance_ids = self
            .call_registry
            .pbx
            .get(&pbx_id)
            .map(|call| call.appearance_ids.clone())
            .unwrap_or_default();
        let mut effects = Vec::new();
        for appearance_id in appearance_ids {
            let Some(appearance) = self.call_registry.appearances.get_mut(&appearance_id) else {
                continue;
            };
            let info = update(&appearance.info);
            if appearance.info == info {
                continue;
            }
            appearance.info = info.clone();
            effects.push(
                HandsetEffect::SetCallInfo {
                    device_id: appearance.device_id.clone(),
                    call_id: appearance.sccp_id,
                    info,
                }
                .into(),
            );
        }
        self.refresh_conference_participant_identity(pbx_id);
        effects
    }

    /// Publish RingOut only after an outbound remote-identity update has
    /// established the called party. Repeated or late identity callbacks may
    /// refresh CallInfo but cannot regress an answered call.
    pub fn pbx_remote_identity_ready(&mut self, pbx_id: PbxCallId) -> Vec<DriverEffect> {
        let Some(call) = self.call_registry.pbx.get_mut(&pbx_id) else {
            return Vec::new();
        };
        if call.direction != CallDirection::Outbound || call.state != CallState::Calling {
            return Vec::new();
        }
        if call.outbound_identity_stage != OutboundIdentityStage::RingOutPublished {
            call.outbound_identity_stage = OutboundIdentityStage::Ready;
        }
        self.publish_outbound_ring_out(pbx_id)
    }

    fn publish_outbound_ring_out(&mut self, pbx_id: PbxCallId) -> Vec<DriverEffect> {
        let appearance_id = {
            let Some(call) = self.call_registry.pbx.get(&pbx_id) else {
                return Vec::new();
            };
            if call.direction != CallDirection::Outbound
                || call.state != CallState::Calling
                || call.outbound_identity_stage != OutboundIdentityStage::Ready
                || call.outbound_phase != Some(OutboundCallPhase::Ringing)
            {
                return Vec::new();
            }
            let Some(appearance_id) = call
                .active_appearance
                .or_else(|| call.appearance_ids.first().copied())
            else {
                return Vec::new();
            };
            appearance_id
        };
        let Some(appearance) = self.call_registry.appearances.get(&appearance_id).cloned() else {
            return Vec::new();
        };
        if let Some(call) = self.call_registry.pbx.get_mut(&pbx_id) {
            call.outbound_identity_stage = OutboundIdentityStage::RingOutPublished;
        }
        vec![
            HandsetEffect::SetCallState {
                device_id: appearance.device_id.clone(),
                call_id: appearance.sccp_id,
                state: HandsetCallState::RingOut,
                stop_media: false,
            }
            .into(),
            HandsetEffect::SetCallInfo {
                device_id: appearance.device_id,
                call_id: appearance.sccp_id,
                info: appearance.info,
            }
            .into(),
        ]
    }

    pub fn appearances_for_device(
        &self,
        device: &DeviceId,
    ) -> impl Iterator<Item = &CallAppearance> {
        self.call_registry
            .by_device
            .get(device)
            .into_iter()
            .flatten()
            .filter_map(|appearance_id| self.call_registry.appearances.get(appearance_id))
    }

    pub fn appearances_for_pbx(&self, pbx_id: PbxCallId) -> impl Iterator<Item = &CallAppearance> {
        self.call_registry
            .pbx
            .get(&pbx_id)
            .into_iter()
            .flat_map(|call| call.appearance_ids.iter())
            .filter_map(|appearance_id| self.call_registry.appearances.get(appearance_id))
    }

    pub fn inbound_offers_for_pbx(&self, pbx_id: PbxCallId) -> Vec<InboundOffer> {
        self.call_registry
            .pbx
            .get(&pbx_id)
            .filter(|call| call.direction == CallDirection::Inbound)
            .into_iter()
            .flat_map(|call| call.appearance_ids.iter())
            .filter_map(|appearance_id| self.call_registry.appearances.get(appearance_id))
            .filter(|appearance| appearance.state == CallState::Ringing)
            .map(|appearance| self.inbound_offer_for_appearance(appearance))
            .collect()
    }

    fn inbound_offer(&self, candidate: &InboundAppearance) -> InboundOffer {
        let call_waiting = self.device_has_active_call(&candidate.binding.device_id);
        InboundOffer {
            device_id: candidate.binding.device_id.clone(),
            line_instance: candidate.binding.line_instance,
            call_id: candidate.call_id,
            ring_mode: candidate.binding.appearance.ring_mode,
            state: if call_waiting {
                HandsetCallState::CallWaiting
            } else {
                HandsetCallState::RingIn
            },
        }
    }

    fn inbound_offer_for_appearance(&self, appearance: &CallAppearance) -> InboundOffer {
        InboundOffer {
            device_id: appearance.device_id.clone(),
            line_instance: appearance.line_instance,
            call_id: appearance.sccp_id,
            ring_mode: appearance.ring_mode,
            state: if self.device_has_active_call(&appearance.device_id) {
                HandsetCallState::CallWaiting
            } else {
                HandsetCallState::RingIn
            },
        }
    }

    fn device_has_active_call(&self, device_id: &DeviceId) -> bool {
        self.devices
            .get(device_id)
            .and_then(|device| device.active_call)
            .and_then(|call_id| self.appearance_for_call(call_id))
            .is_some_and(|appearance| {
                matches!(
                    appearance.state,
                    CallState::Collecting
                        | CallState::Calling
                        | CallState::Connected
                        | CallState::TransferCollecting
                )
            })
    }

    pub fn cancel_inbound_offer(&mut self, call_id: CallId) -> bool {
        let Some(appearance) = self.appearance_for_call(call_id).cloned() else {
            return false;
        };
        let Some(call) = self.call_registry.pbx.get(&appearance.pbx_id) else {
            return false;
        };
        if self.redirect_claims.contains(&appearance.pbx_id)
            || call.state != CallState::Ringing
            || call.active_appearance.is_some()
        {
            return false;
        }
        #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
        self.pending_auto_answers.remove(&call_id);
        self.remove_appearance(appearance.id);
        if self
            .call_registry
            .pbx
            .get(&appearance.pbx_id)
            .is_some_and(|call| call.appearance_ids.is_empty())
        {
            self.call_registry.pbx.remove(&appearance.pbx_id);
        }
        debug_assert!(self.invariant_error().is_none());
        true
    }

    /// Remove a still-ringing PBX call from every handset before the adapter
    /// continues that same PBX channel at a forwarding destination.
    pub fn forward_ringing_call(&mut self, pbx_id: PbxCallId) -> Vec<DriverEffect> {
        let Some(call) = self.call_registry.pbx.get(&pbx_id) else {
            return Vec::new();
        };
        if call.state != CallState::Ringing || call.active_appearance.is_some() {
            return Vec::new();
        }
        let effects = call
            .appearance_ids
            .iter()
            .filter_map(|id| self.call_registry.appearances.get(id))
            .map(|appearance| appearance_state_effect(appearance, HandsetCallState::OnHook, false))
            .collect();
        self.remove_pbx_call(pbx_id);
        debug_assert!(self.invariant_error().is_none());
        effects
    }

    /// Reserve one still-ringing logical call for a no-answer redirect without
    /// mutating handset-visible state. Answer/hold/steal transitions reject the
    /// call until the exact adapter claim completes or rolls back.
    pub fn claim_ringing_forward(&mut self, pbx_id: PbxCallId) -> bool {
        if self.redirect_claims.contains(&pbx_id)
            || self.call_registry.pbx.get(&pbx_id).is_none_or(|call| {
                call.state != CallState::Ringing || call.active_appearance.is_some()
            })
        {
            return false;
        }
        self.redirect_claims.insert(pbx_id)
    }

    pub fn complete_ringing_forward(&mut self, pbx_id: PbxCallId) -> Vec<DriverEffect> {
        if !self.redirect_claims.remove(&pbx_id) {
            return Vec::new();
        }
        self.forward_ringing_call(pbx_id)
    }

    pub fn rollback_ringing_forward(&mut self, pbx_id: PbxCallId) -> bool {
        self.redirect_claims.remove(&pbx_id)
    }

    fn device_is_busy(&self, device: &DeviceId) -> bool {
        self.appearances_for_device(device).any(|appearance| {
            matches!(
                appearance.state,
                CallState::Collecting
                    | CallState::PickupCollecting
                    | CallState::Calling
                    | CallState::Connected
                    | CallState::Parking
                    | CallState::Retrieving
                    | CallState::Held
                    | CallState::TransferCollecting
            )
        })
    }

    fn shared_control_eligible(&self, appearance: &CallAppearance) -> bool {
        appearance.ring_mode != AppearanceRingMode::Disabled
            && self.devices.contains_key(&appearance.device_id)
            && !self.shared_control_claims.contains_key(&appearance.pbx_id)
            && self
                .call_registry
                .pbx
                .get(&appearance.pbx_id)
                .is_some_and(|call| !call.privacy)
    }

    fn outbound_route_presentation(
        &mut self,
        pbx_id: PbxCallId,
        destination: &str,
    ) -> Vec<DriverEffect> {
        let Some(appearance_id) = self.call_registry.pbx.get(&pbx_id).and_then(|call| {
            (call.direction == CallDirection::Outbound && call.state == CallState::Calling)
                .then(|| {
                    call.active_appearance
                        .or_else(|| call.appearance_ids.first().copied())
                })
                .flatten()
        }) else {
            return Vec::new();
        };
        let Some(appearance) = self.call_registry.appearances.get_mut(&appearance_id) else {
            return Vec::new();
        };
        appearance.info.called_number = destination.to_owned();
        let device_id = appearance.device_id.clone();
        let call_id = appearance.sccp_id;
        let info = appearance.info.clone();
        if let Some(call) = self.call_registry.pbx.get_mut(&pbx_id) {
            call.outbound_phase = Some(OutboundCallPhase::Routing);
        }
        self.refresh_conference_participant_identity(pbx_id);
        vec![
            HandsetEffect::CommitOutboundCall {
                device_id,
                call_id,
                info,
            }
            .into(),
        ]
    }

    fn finish_digits(&mut self, call_id: CallId) -> Vec<DriverEffect> {
        let Some((appearance_id, pbx_id, appearance_state, device_id, codec)) =
            self.appearance_for_call(call_id).map(|appearance| {
                (
                    appearance.id,
                    appearance.pbx_id,
                    appearance.state,
                    appearance.device_id.clone(),
                    appearance.codec,
                )
            })
        else {
            return Vec::new();
        };
        if !matches!(
            appearance_state,
            CallState::Collecting | CallState::PickupCollecting | CallState::TransferCollecting
        ) {
            return Vec::new();
        }
        let Some(call) = self.call_registry.pbx.get_mut(&pbx_id) else {
            return Vec::new();
        };
        if call.digits.is_empty() {
            call.digit_deadline = None;
            return Vec::new();
        }
        call.digit_deadline = None;
        let destination = call.digits.clone();
        if let Some(pickup) = call.pending_pickup.take() {
            let next_state = if pickup.answer {
                CallState::Connected
            } else {
                CallState::Ringing
            };
            call.state = next_state;
            call.active_appearance = pickup.answer.then_some(appearance_id);
            if let Some(appearance) = self.call_registry.appearances.get_mut(&appearance_id) {
                appearance.state = next_state;
                appearance.audio = if pickup.answer {
                    MediaStreamState::Opening
                } else {
                    MediaStreamState::Closed
                };
            }
            debug_assert!(self.invariant_error().is_none());
            return vec![
                PbxEffect::Pickup {
                    operation: PickupOperation::Directed {
                        call_id: pbx_id,
                        device_id,
                        handset_call_id: call_id,
                        codec,
                        extension: destination,
                        context: pickup.context,
                        answer: pickup.answer,
                    },
                }
                .into(),
            ];
        }
        let context = call.context.clone();
        let consultation_transfer = self
            .transfers
            .for_leg(TransferLeg {
                handset_call_id: call_id,
                pbx_call_id: pbx_id,
            })
            .is_some_and(|transaction| {
                transaction.consultation
                    == Some(TransferLeg {
                        handset_call_id: call_id,
                        pbx_call_id: pbx_id,
                    })
            });
        let next_state = CallState::Calling;
        call.state = next_state;
        if let Some(appearance) = self.call_registry.appearances.get_mut(&appearance_id) {
            appearance.state = next_state;
        }
        if consultation_transfer {
            self.advance_transfer_for_pbx(pbx_id, TransferPhase::Routing);
        }
        debug_assert!(self.invariant_error().is_none());
        let mut effects = self.outbound_route_presentation(pbx_id, &destination);
        effects.push(
            PbxEffect::StartRouting {
                call_id: pbx_id,
                context,
                destination,
            }
            .into(),
        );
        effects
    }

    fn device_supports_codec(&self, device: &DeviceId, codec: Codec) -> bool {
        codec.kind() == CodecKind::Audio
            && self.devices.get(device).is_some_and(|state| {
                state
                    .capabilities
                    .audio()
                    .iter()
                    .any(|capability| capability.codec == codec)
            })
    }

    fn end_barge(
        &mut self,
        call_id: CallId,
        bridge_joined: bool,
        channel_created: bool,
    ) -> Vec<DriverEffect> {
        self.end_barge_internal(call_id, bridge_joined, channel_created, true)
    }

    fn end_barge_internal(
        &mut self,
        call_id: CallId,
        bridge_joined: bool,
        channel_created: bool,
        restore_handset: bool,
    ) -> Vec<DriverEffect> {
        let Some(session) = self.barges.by_handset.remove(&call_id) else {
            return Vec::new();
        };
        self.barges.by_pbx.remove(&session.barger_call_id);
        self.call_registry.pbx.remove(&session.barger_call_id);

        let last_participant =
            if let Some(group) = self.barges.groups.get_mut(&session.target_call_id) {
                group.members.retain(|member| *member != call_id);
                group.members.is_empty()
            } else {
                true
            };
        if last_participant {
            self.barges.groups.remove(&session.target_call_id);
            if self.shared_control_claims.get(&session.target_call_id)
                == Some(&SharedControlClaim::Barge(session.bridge_id))
            {
                self.shared_control_claims.remove(&session.target_call_id);
            }
        }

        let mut effects = Vec::new();
        if bridge_joined {
            effects.push(
                PbxEffect::Barge {
                    operation: BargeOperation::Leave {
                        bridge_id: session.bridge_id,
                        barger_call_id: session.barger_call_id,
                        last_participant,
                    },
                }
                .into(),
            );
        }
        if channel_created {
            effects.push(
                PbxEffect::Hangup {
                    call_id: session.barger_call_id,
                }
                .into(),
            );
        }

        let target_exists = self.call_registry.pbx.contains_key(&session.target_call_id);
        let appearance_id = self.call_registry.by_sccp.get(&call_id).copied();
        if let Some(appearance_id) = appearance_id {
            if target_exists {
                if let Some(appearance) = self.call_registry.appearances.get_mut(&appearance_id) {
                    appearance.state = CallState::RemoteInUse;
                    appearance.audio = MediaStreamState::Closed;
                    appearance.audio_transmit = MediaStreamState::Closed;
                    appearance.video.close_streams();
                }
                if restore_handset
                    && let Some(appearance) = self.call_registry.appearances.get(&appearance_id)
                {
                    effects.push(appearance_state_effect(
                        appearance,
                        HandsetCallState::RemoteMultiline,
                        true,
                    ));
                }
            } else if let Some(appearance) =
                self.call_registry.appearances.get(&appearance_id).cloned()
            {
                if restore_handset {
                    effects.push(appearance_state_effect(
                        &appearance,
                        HandsetCallState::OnHook,
                        true,
                    ));
                }
                self.remove_appearance(appearance_id);
            }
        }
        debug_assert!(self.invariant_error().is_none());
        effects
    }

    fn end_barges_for_target(&mut self, target: PbxCallId) -> Vec<DriverEffect> {
        let members = self
            .barges
            .groups
            .get(&target)
            .map(|group| group.members.clone())
            .unwrap_or_default();
        let mut effects = Vec::new();
        for call_id in members {
            effects.extend(self.end_barge_internal(call_id, true, true, false));
        }
        effects
    }

    fn allocate_pbx_id(&mut self) -> PbxCallId {
        loop {
            let id = self.next_pbx_id.into();
            self.next_pbx_id = self.next_pbx_id.wrapping_add(1).max(1);
            if !self.call_registry.pbx.contains_key(&id) {
                return id;
            }
        }
    }

    fn allocate_bridge_id(&mut self) -> PbxBridgeId {
        loop {
            let id = self.next_bridge_id.into();
            self.next_bridge_id = self.next_bridge_id.wrapping_add(1).max(1);
            if !self
                .barges
                .groups
                .values()
                .any(|group| group.bridge_id == id)
                && !self
                    .conferences
                    .by_consultation
                    .values()
                    .any(|conference| conference.bridge_id == id)
            {
                return id;
            }
        }
    }

    fn allocate_conference_id(&mut self) -> ConferenceId {
        loop {
            let id = ConferenceId::new(self.next_conference_id);
            self.next_conference_id = self.next_conference_id.wrapping_add(1).max(1);
            if !self
                .conferences
                .by_consultation
                .values()
                .any(|conference| conference.id == id)
            {
                return id;
            }
        }
    }

    fn allocate_participant_id(&mut self) -> ParticipantId {
        loop {
            let id = ParticipantId::new(self.next_participant_id);
            self.next_participant_id = self.next_participant_id.wrapping_add(1).max(1);
            if !self
                .conferences
                .by_consultation
                .values()
                .any(|conference| conference.participants.get(id).is_some())
            {
                return id;
            }
        }
    }

    fn conference_participant(
        &mut self,
        appearance: &CallAppearance,
        moderator: bool,
    ) -> ConferenceParticipant {
        let identity = self
            .call_registry
            .pbx
            .get(&appearance.pbx_id)
            .map(|call| conference_participant_identity(call, appearance))
            .unwrap_or_default();
        ConferenceParticipant {
            id: self.allocate_participant_id(),
            pbx_call_id: appearance.pbx_id,
            handset_call_id: appearance.sccp_id,
            device_id: appearance.device_id.clone(),
            display_name: identity.display_name,
            number: identity.number,
            moderator,
            muted: false,
            held: false,
        }
    }

    fn refresh_conference_participant_identity(&mut self, pbx_id: PbxCallId) -> bool {
        let Some(conference_key) = self.conferences.by_pbx.get(&pbx_id).copied() else {
            return false;
        };
        let Some(handset_call_id) = self
            .conferences
            .by_consultation
            .get(&conference_key)
            .and_then(|session| {
                session
                    .participants
                    .by_pbx(pbx_id)
                    .map(|participant| participant.handset_call_id)
                    .or_else(|| {
                        session
                            .pending_invite
                            .as_ref()
                            .filter(|invite| invite.participant.pbx_call_id == pbx_id)
                            .map(|invite| invite.participant.handset_call_id)
                    })
            })
        else {
            return false;
        };
        let Some(identity) = self
            .call_registry
            .pbx
            .get(&pbx_id)
            .zip(self.appearance_for_call(handset_call_id))
            .map(|(call, appearance)| conference_participant_identity(call, appearance))
        else {
            return false;
        };
        let Some(session) = self.conferences.by_consultation.get_mut(&conference_key) else {
            return false;
        };
        if session
            .participants
            .update_identity(pbx_id, identity.clone())
        {
            return true;
        }
        let Some(invite) = session
            .pending_invite
            .as_mut()
            .filter(|invite| invite.participant.pbx_call_id == pbx_id)
        else {
            return false;
        };
        invite.participant.display_name = identity.display_name;
        invite.participant.number = identity.number;
        true
    }

    fn allocate_appearance_id(&mut self) -> CallAppearanceId {
        loop {
            let id = CallAppearanceId(self.next_appearance_id);
            self.next_appearance_id = self.next_appearance_id.wrapping_add(1).max(1);
            if !self.call_registry.appearances.contains_key(&id) {
                return id;
            }
        }
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
    fn allocate_call_transition_id(&mut self) -> CallTransitionId {
        loop {
            let id = CallTransitionId(self.next_call_transition_id);
            self.next_call_transition_id = self.next_call_transition_id.wrapping_add(1).max(1);
            if !self.pending_call_transitions.contains_key(&id) {
                return id;
            }
        }
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
    fn call_domain_snapshot(&self) -> CallDomainSnapshot {
        CallDomainSnapshot {
            devices: self.devices.clone(),
            pbx_calls: self.call_registry.pbx.clone(),
            appearances: self.call_registry.appearances.clone(),
            appearance_by_sccp: self.call_registry.by_sccp.clone(),
            shared_control_claims: self.shared_control_claims.clone(),
            call_waiting_tones: self.call_waiting_tones.clone(),
            pending_phone_answers: self.pending_phone_answers.clone(),
            pending_route_media: self.pending_route_media.clone(),
        }
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
    fn restore_call_domain(
        &mut self,
        snapshot: &CallDomainSnapshot,
        transition: &CallTransition,
    ) -> bool {
        let affected_pbx = [transition.previous_pbx_id, Some(transition.target_pbx_id)]
            .into_iter()
            .flatten()
            .collect::<HashSet<_>>();
        let current_active = self
            .devices
            .get(&transition.device_id)
            .and_then(|device| device.active_call);
        if current_active != Some(transition.target_call_id)
            || self
                .appearance_for_call(transition.target_call_id)
                .is_none_or(|appearance| appearance.pbx_id != transition.target_pbx_id)
            || transition.previous_call_id.is_some_and(|previous| {
                self.appearance_for_call(previous)
                    .is_none_or(|appearance| Some(appearance.pbx_id) != transition.previous_pbx_id)
            })
        {
            return false;
        }

        let mut affected_calls = self
            .call_registry
            .appearances
            .values()
            .filter(|appearance| affected_pbx.contains(&appearance.pbx_id))
            .map(|appearance| appearance.sccp_id)
            .collect::<HashSet<_>>();
        affected_calls.extend(
            snapshot
                .appearances
                .values()
                .filter(|appearance| affected_pbx.contains(&appearance.pbx_id))
                .map(|appearance| appearance.sccp_id),
        );
        affected_calls.insert(transition.target_call_id);
        if let Some(previous) = transition.previous_call_id {
            affected_calls.insert(previous);
        }

        self.call_registry
            .pbx
            .retain(|pbx_id, _| !affected_pbx.contains(pbx_id));
        self.call_registry.pbx.extend(
            snapshot
                .pbx_calls
                .iter()
                .filter(|(pbx_id, _)| affected_pbx.contains(pbx_id))
                .map(|(pbx_id, call)| (*pbx_id, call.clone())),
        );
        self.call_registry
            .appearances
            .retain(|_, appearance| !affected_pbx.contains(&appearance.pbx_id));
        self.call_registry.appearances.extend(
            snapshot
                .appearances
                .iter()
                .filter(|(_, appearance)| affected_pbx.contains(&appearance.pbx_id))
                .map(|(id, appearance)| (*id, appearance.clone())),
        );
        self.call_registry
            .by_sccp
            .retain(|call_id, _| !affected_calls.contains(call_id));
        self.call_registry.by_sccp.extend(
            snapshot
                .appearance_by_sccp
                .iter()
                .filter(|(call_id, _)| affected_calls.contains(call_id))
                .map(|(call_id, appearance_id)| (*call_id, *appearance_id)),
        );
        self.call_registry.by_device.clear();
        for (appearance_id, appearance) in &self.call_registry.appearances {
            self.call_registry
                .by_device
                .entry(appearance.device_id.clone())
                .or_default()
                .insert(*appearance_id);
        }
        for (device_id, before) in &snapshot.devices {
            let Some(device) = self.devices.get_mut(device_id) else {
                continue;
            };
            if device_id == &transition.device_id {
                device.active_call = before.active_call;
            }
            for call_id in &affected_calls {
                if before.selected_calls.contains(call_id) {
                    device.selected_calls.insert(*call_id);
                } else {
                    device.selected_calls.remove(call_id);
                }
            }
        }
        for pbx_id in &affected_pbx {
            match snapshot.shared_control_claims.get(pbx_id) {
                Some(claim) => {
                    self.shared_control_claims.insert(*pbx_id, *claim);
                }
                None => {
                    self.shared_control_claims.remove(pbx_id);
                }
            }
        }
        self.call_waiting_tones.retain(|call_id, schedule| {
            !affected_calls.contains(call_id) && !affected_calls.contains(&schedule.active_call_id)
        });
        self.call_waiting_tones.extend(
            snapshot
                .call_waiting_tones
                .iter()
                .filter(|(call_id, schedule)| {
                    affected_calls.contains(call_id)
                        || affected_calls.contains(&schedule.active_call_id)
                })
                .map(|(call_id, schedule)| (*call_id, schedule.clone())),
        );
        self.pending_phone_answers
            .retain(|call_id, _| !affected_calls.contains(call_id));
        self.pending_phone_answers.extend(
            snapshot
                .pending_phone_answers
                .iter()
                .filter(|(call_id, _)| affected_calls.contains(call_id))
                .map(|(call_id, pbx_id)| (*call_id, *pbx_id)),
        );
        self.pending_route_media
            .retain(|call_id| !affected_calls.contains(call_id));
        self.pending_route_media.extend(
            snapshot
                .pending_route_media
                .iter()
                .filter(|call_id| affected_calls.contains(call_id))
                .copied(),
        );
        true
    }

    fn insert_pbx_call(&mut self, call: PbxCall, appearance: CallAppearance) -> bool {
        if call.id != appearance.pbx_id
            || !call.appearance_ids.is_empty()
            || self.call_registry.pbx.contains_key(&call.id)
        {
            return false;
        }
        let pbx_id = call.id;
        self.call_registry.pbx.insert(pbx_id, call);
        if self.attach_appearance(appearance) {
            true
        } else {
            self.call_registry.pbx.remove(&pbx_id);
            false
        }
    }

    fn attach_appearance(&mut self, appearance: CallAppearance) -> bool {
        if !self.call_registry.pbx.contains_key(&appearance.pbx_id)
            || self.call_registry.appearances.contains_key(&appearance.id)
            || self.call_registry.by_sccp.contains_key(&appearance.sccp_id)
        {
            return false;
        }
        let appearance_id = appearance.id;
        let pbx_id = appearance.pbx_id;
        let call_id = appearance.sccp_id;
        let device_id = appearance.device_id.clone();
        self.call_registry
            .appearances
            .insert(appearance_id, appearance);
        self.call_registry.by_sccp.insert(call_id, appearance_id);
        self.call_registry
            .by_device
            .entry(device_id)
            .or_default()
            .insert(appearance_id);
        self.call_registry
            .pbx
            .get_mut(&pbx_id)
            .expect("PBX call checked above")
            .appearance_ids
            .push(appearance_id);
        true
    }

    fn remove_appearance(&mut self, appearance_id: CallAppearanceId) -> Option<CallAppearance> {
        let appearance = self.call_registry.appearances.remove(&appearance_id)?;
        self.call_waiting_tones.retain(|_, schedule| {
            schedule.waiting_call_id != appearance.sccp_id
                && schedule.active_call_id != appearance.sccp_id
        });
        self.pending_phone_answers.remove(&appearance.sccp_id);
        self.pending_route_media.remove(&appearance.sccp_id);
        self.call_registry.by_sccp.remove(&appearance.sccp_id);
        if let Some(device_appearances) =
            self.call_registry.by_device.get_mut(&appearance.device_id)
        {
            device_appearances.remove(&appearance_id);
            if device_appearances.is_empty() {
                self.call_registry.by_device.remove(&appearance.device_id);
            }
        }
        if let Some(call) = self.call_registry.pbx.get_mut(&appearance.pbx_id) {
            call.appearance_ids.retain(|id| *id != appearance_id);
            if call.active_appearance == Some(appearance_id) {
                call.active_appearance = None;
            }
        }
        if self.shared_control_claims.get(&appearance.pbx_id)
            == Some(&SharedControlClaim::Steal(appearance_id))
        {
            self.shared_control_claims.remove(&appearance.pbx_id);
        }
        if let Some(device) = self.devices.get_mut(&appearance.device_id) {
            device.selected_calls.remove(&appearance.sccp_id);
            if device.active_call == Some(appearance.sccp_id) {
                device.active_call = None;
            }
        }
        Some(appearance)
    }

    fn remove_pbx_call(&mut self, pbx_id: PbxCallId) -> Option<(PbxCall, Option<CallSnapshot>)> {
        self.conference_mutations
            .remove(&ConferenceMutationOwner::Destination(pbx_id));
        if let Some(transaction) = self.voicemail.for_pbx(pbx_id).cloned() {
            let _ = self
                .voicemail
                .cancel(&transaction.device_id, transaction.id);
        }
        self.redirect_claims.remove(&pbx_id);
        self.shared_control_claims.remove(&pbx_id);
        #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
        self.cancel_auto_answers_for_pbx(pbx_id);
        let appearance_ids = self.call_registry.pbx.get(&pbx_id)?.appearance_ids.clone();
        let mut primary = appearance_ids
            .first()
            .and_then(|appearance_id| self.call_snapshot(*appearance_id));
        for appearance_id in appearance_ids {
            self.remove_appearance(appearance_id);
        }
        let mut call = self.call_registry.pbx.remove(&pbx_id)?;
        call.state = CallState::Ended;
        call.digit_deadline = None;
        if let Some(primary) = &mut primary {
            primary.state = CallState::Ended;
            primary.digit_deadline = None;
        }
        Some((call, primary))
    }

    fn call_snapshot(&self, appearance_id: CallAppearanceId) -> Option<CallSnapshot> {
        let appearance = self.call_registry.appearances.get(&appearance_id)?;
        let call = self.call_registry.pbx.get(&appearance.pbx_id)?;
        Some(CallSnapshot {
            sccp_id: appearance.sccp_id,
            pbx_id: call.id,
            device_id: appearance.device_id.clone(),
            line_instance: appearance.line_instance,
            line: call.line.clone(),
            direction: call.direction,
            state: appearance.state,
            digits: call.digits.clone(),
            info: appearance.info.clone(),
            metadata: call.metadata.clone(),
            codec: appearance.codec,
            audio: appearance.audio,
            audio_transmit: appearance.audio_transmit,
            video: appearance.video.clone(),
            digit_deadline: call.digit_deadline,
        })
    }

    fn invariant_error(&self) -> Option<String> {
        if let Some(pbx_id) = self
            .redirect_claims
            .iter()
            .find(|pbx_id| !self.call_registry.pbx.contains_key(pbx_id))
        {
            return Some(format!("redirect claim for missing PBX call {pbx_id:?}"));
        }
        for transaction in self.voicemail.transactions() {
            let appearance_is_owned = self
                .appearance_for_call(transaction.handset_call_id)
                .is_some_and(|appearance| {
                    appearance.pbx_id == transaction.pbx_call_id
                        && appearance.device_id == transaction.device_id
                });
            let owner_disconnected = !self.devices.contains_key(&transaction.device_id);
            if !self.redirect_claims.contains(&transaction.pbx_call_id)
                || !self
                    .call_registry
                    .pbx
                    .contains_key(&transaction.pbx_call_id)
                || (!appearance_is_owned && !owner_disconnected)
            {
                return Some(format!(
                    "voicemail transaction {:?} has inconsistent ownership",
                    transaction.id
                ));
            }
        }
        for (device_id, device) in &self.devices {
            if let Some(active_call) = device.active_call
                && self
                    .appearance_for_call(active_call)
                    .is_none_or(|appearance| &appearance.device_id != device_id)
            {
                return Some(format!(
                    "device {device_id} has invalid active call {active_call:?}"
                ));
            }
            if device.selected_calls.iter().any(|call_id| {
                self.appearance_for_call(*call_id)
                    .is_none_or(|appearance| &appearance.device_id != device_id)
            }) {
                return Some(format!("device {device_id} has an invalid selected call"));
            }
        }
        for (pbx_id, call) in &self.call_registry.pbx {
            if pbx_id != &call.id {
                return Some(format!("PBX call key {pbx_id:?} does not match record"));
            }
            if call.pending_pickup.is_some() != (call.state == CallState::PickupCollecting) {
                return Some(format!(
                    "PBX call {pbx_id:?} has inconsistent directed-pickup state"
                ));
            }
            let mut unique = HashSet::new();
            for appearance_id in &call.appearance_ids {
                if !unique.insert(*appearance_id) {
                    return Some(format!("PBX call {pbx_id:?} repeats an appearance"));
                }
                if self
                    .call_registry
                    .appearances
                    .get(appearance_id)
                    .is_none_or(|appearance| appearance.pbx_id != *pbx_id)
                {
                    return Some(format!("PBX call {pbx_id:?} has a dangling appearance"));
                }
            }
            if let Some(active) = call.active_appearance {
                let Some(appearance) = self.call_registry.appearances.get(&active) else {
                    return Some(format!(
                        "PBX call {pbx_id:?} has a dangling active appearance"
                    ));
                };
                if appearance.pbx_id != *pbx_id
                    || !matches!(
                        appearance.state,
                        CallState::Collecting
                            | CallState::PickupCollecting
                            | CallState::Calling
                            | CallState::Connected
                            | CallState::Parking
                            | CallState::Retrieving
                            | CallState::Held
                            | CallState::TransferCollecting
                    )
                {
                    return Some(format!(
                        "PBX call {pbx_id:?} has an invalid active appearance"
                    ));
                }
            }
            let active_count = call
                .appearance_ids
                .iter()
                .filter_map(|id| self.call_registry.appearances.get(id))
                .filter(|appearance| {
                    matches!(
                        appearance.state,
                        CallState::Collecting
                            | CallState::PickupCollecting
                            | CallState::Calling
                            | CallState::Connected
                            | CallState::Parking
                            | CallState::Retrieving
                            | CallState::Held
                            | CallState::TransferCollecting
                    )
                })
                .count();
            if active_count > usize::from(call.active_appearance.is_some()) {
                return Some(format!(
                    "PBX call {pbx_id:?} has multiple active appearances"
                ));
            }
        }
        for (appearance_id, appearance) in &self.call_registry.appearances {
            if appearance_id != &appearance.id {
                return Some(format!(
                    "appearance key {appearance_id:?} does not match record"
                ));
            }
            if self.call_registry.by_sccp.get(&appearance.sccp_id) != Some(appearance_id) {
                return Some(format!(
                    "call {:?} does not index appearance {appearance_id:?}",
                    appearance.sccp_id
                ));
            }
            if !self
                .call_registry
                .by_device
                .get(&appearance.device_id)
                .is_some_and(|ids| ids.contains(appearance_id))
            {
                return Some(format!(
                    "device {} does not index appearance {appearance_id:?}",
                    appearance.device_id
                ));
            }
            let Some(call) = self.call_registry.pbx.get(&appearance.pbx_id) else {
                return Some(format!("appearance {appearance_id:?} has no PBX call"));
            };
            if !call.appearance_ids.contains(appearance_id) {
                return Some(format!(
                    "appearance {appearance_id:?} is not owned by its PBX call"
                ));
            }
        }
        for (call_id, appearance_id) in &self.call_registry.by_sccp {
            if self
                .call_registry
                .appearances
                .get(appearance_id)
                .is_none_or(|appearance| appearance.sccp_id != *call_id)
            {
                return Some(format!("call {call_id:?} has a dangling index"));
            }
        }
        for (device_id, appearance_ids) in &self.call_registry.by_device {
            for appearance_id in appearance_ids {
                if self
                    .call_registry
                    .appearances
                    .get(appearance_id)
                    .is_none_or(|appearance| appearance.device_id != *device_id)
                {
                    return Some(format!("device {device_id} has a dangling appearance"));
                }
            }
        }
        for (target_id, claim) in &self.shared_control_claims {
            let Some(target) = self.call_registry.pbx.get(target_id) else {
                return Some(format!("shared-control claim has no target {target_id:?}"));
            };
            match claim {
                SharedControlClaim::Steal(winner) => {
                    if target.active_appearance != Some(*winner) {
                        return Some(format!(
                            "steal claim for {target_id:?} does not match its active appearance"
                        ));
                    }
                }
                SharedControlClaim::Barge(bridge_id) => {
                    if self
                        .barges
                        .groups
                        .get(target_id)
                        .is_none_or(|group| group.bridge_id != *bridge_id)
                    {
                        return Some(format!(
                            "barge claim for {target_id:?} has no matching group"
                        ));
                    }
                }
            }
        }
        for (target_id, group) in &self.barges.groups {
            if group.members.is_empty() {
                return Some(format!("barge group for {target_id:?} is empty"));
            }
            for call_id in &group.members {
                if self.barges.by_handset.get(call_id).is_none_or(|session| {
                    session.target_call_id != *target_id
                        || session.bridge_id != group.bridge_id
                        || session.mode != group.mode
                }) {
                    return Some(format!(
                        "barge group for {target_id:?} has dangling member {call_id:?}"
                    ));
                }
            }
        }
        for (call_id, session) in &self.barges.by_handset {
            if session.handset_call_id != *call_id
                || self.barges.by_pbx.get(&session.barger_call_id) != Some(call_id)
                || self
                    .call_registry
                    .pbx
                    .get(&session.barger_call_id)
                    .is_none_or(|call| {
                        !call.appearance_ids.is_empty() || call.state != CallState::Connected
                    })
                || self.appearance_for_call(*call_id).is_none_or(|appearance| {
                    appearance.pbx_id != session.target_call_id
                        || appearance.state != CallState::Barged
                })
            {
                return Some(format!("barge session for {call_id:?} is inconsistent"));
            }
        }
        for (pbx_id, call_id) in &self.barges.by_pbx {
            if self
                .barges
                .by_handset
                .get(call_id)
                .is_none_or(|session| session.barger_call_id != *pbx_id)
            {
                return Some(format!("barge PBX index {pbx_id:?} is dangling"));
            }
        }
        for (consultation, session) in &self.conferences.by_consultation {
            if consultation != &session.consultation_handset_call_id
                || session.participants.iter().len() < 2
                || session.participants.moderator_count() == 0
                || session
                    .participants
                    .by_pbx(session.original_call_id)
                    .is_none_or(|participant| {
                        participant.pbx_call_id != session.original_call_id
                            || participant.handset_call_id != session.original_handset_call_id
                    })
                || session.participants.iter().any(|participant| {
                    self.conferences.by_pbx.get(&participant.pbx_call_id) != Some(consultation)
                })
                || session.pending_invite.as_ref().is_some_and(|invite| {
                    self.conferences.by_pbx.get(&invite.participant.pbx_call_id)
                        != Some(consultation)
                })
            {
                return Some(format!(
                    "conference {:?} has inconsistent indexes",
                    session.id
                ));
            }
            let Some(original) = self.call_registry.pbx.get(&session.original_call_id) else {
                return Some(format!("conference {:?} has no original call", session.id));
            };
            let Some(consultation_call) = self.call_registry.pbx.get(&session.consultation_call_id)
            else {
                return Some(format!(
                    "conference {:?} has no consultation call",
                    session.id
                ));
            };
            if self
                .appearance_for_call(session.original_handset_call_id)
                .is_none_or(|appearance| {
                    appearance.pbx_id != session.original_call_id
                        || appearance.device_id != session.device_id
                })
                || self
                    .appearance_for_call(session.consultation_handset_call_id)
                    .is_none_or(|appearance| {
                        appearance.pbx_id != session.consultation_call_id
                            || appearance.device_id != session.device_id
                    })
            {
                return Some(format!(
                    "conference {:?} has inconsistent handset appearances",
                    session.id
                ));
            }
            if session.participants.iter().any(|participant| {
                !self
                    .call_registry
                    .pbx
                    .contains_key(&participant.pbx_call_id)
                    || self
                        .appearance_for_call(participant.handset_call_id)
                        .is_none_or(|appearance| {
                            appearance.pbx_id != participant.pbx_call_id
                                || appearance.device_id != participant.device_id
                        })
            }) {
                return Some(format!(
                    "conference {:?} has an inconsistent participant",
                    session.id
                ));
            }
            if session.pending_invite.as_ref().is_some_and(|invite| {
                !self
                    .call_registry
                    .pbx
                    .contains_key(&invite.participant.pbx_call_id)
                    || self
                        .appearance_for_call(invite.participant.handset_call_id)
                        .is_none_or(|appearance| {
                            appearance.pbx_id != invite.participant.pbx_call_id
                                || appearance.device_id != invite.participant.device_id
                        })
                    || session
                        .participants
                        .get(invite.moderator_id)
                        .is_none_or(|moderator| {
                            !moderator.moderator
                                || moderator.pbx_call_id != invite.moderator_call_id
                                || self
                                    .call_registry
                                    .pbx
                                    .get(&moderator.pbx_call_id)
                                    .is_none_or(|call| call.state != CallState::Held)
                        })
            }) {
                return Some(format!(
                    "conference {:?} has an inconsistent pending invite",
                    session.id
                ));
            }
            if session
                .pending_participant_mutation
                .is_some_and(|mutation| {
                    session.phase != ConferencePhase::Active
                        || session
                            .participants
                            .get(mutation.participant_id)
                            .is_none_or(|participant| {
                                participant.pbx_call_id != mutation.call_id
                                    || match mutation.kind {
                                        ConferenceParticipantMutationKind::Mute(muted) => {
                                            participant.moderator || participant.muted == muted
                                        }
                                        ConferenceParticipantMutationKind::Remove => {
                                            participant.moderator
                                                || session.participants.iter().len() <= 2
                                        }
                                        ConferenceParticipantMutationKind::Moderator(moderator) => {
                                            participant.moderator == moderator
                                                || participant.held
                                                || (moderator && participant.muted)
                                                || (!moderator
                                                    && session.participants.moderator_count() == 1)
                                        }
                                        ConferenceParticipantMutationKind::Hold(held) => {
                                            !participant.moderator || participant.held == held
                                        }
                                    }
                            })
                })
            {
                return Some(format!(
                    "conference {:?} has an inconsistent participant mutation",
                    session.id
                ));
            }
            let states_are_valid = match session.phase {
                ConferencePhase::Consultation => {
                    session.origin == ConferenceOrigin::Consultation
                        && original.state == CallState::Held
                        && matches!(
                            consultation_call.state,
                            CallState::Collecting | CallState::Calling | CallState::Connected
                        )
                }
                ConferencePhase::Merging if session.origin == ConferenceOrigin::Consultation => {
                    original.state == CallState::Held
                        && consultation_call.state == CallState::Connected
                }
                ConferencePhase::Merging => session.participants.iter().all(|participant| {
                    self.call_registry
                        .pbx
                        .get(&participant.pbx_call_id)
                        .is_some_and(|call| {
                            matches!(call.state, CallState::Connected | CallState::Held)
                        })
                }),
                ConferencePhase::Active => {
                    session.participants.iter().all(|participant| {
                        self.call_registry
                            .pbx
                            .get(&participant.pbx_call_id)
                            .is_some_and(|call| {
                                matches!(call.state, CallState::Connected | CallState::Held)
                            })
                    }) && session.pending_invite.as_ref().is_none_or(|invite| {
                        self.call_registry
                            .pbx
                            .get(&invite.participant.pbx_call_id)
                            .is_some_and(|call| {
                                matches!(
                                    call.state,
                                    CallState::Collecting
                                        | CallState::Calling
                                        | CallState::Connected
                                )
                            })
                    })
                }
            };
            if !states_are_valid {
                return Some(format!(
                    "conference {:?} has inconsistent call states",
                    session.id
                ));
            }
        }
        for (pbx_id, consultation) in &self.conferences.by_pbx {
            if self
                .conferences
                .by_consultation
                .get(consultation)
                .is_none_or(|session| {
                    session.participants.by_pbx(*pbx_id).is_none()
                        && session
                            .pending_invite
                            .as_ref()
                            .is_none_or(|invite| invite.participant.pbx_call_id != *pbx_id)
                })
            {
                return Some(format!("conference PBX index {pbx_id:?} is dangling"));
            }
        }
        for transaction in self.transfers.transactions() {
            if !self.devices.contains_key(&transaction.device_id)
                || self
                    .appearance_for_call(transaction.source.handset_call_id)
                    .is_none_or(|appearance| {
                        appearance.pbx_id != transaction.source.pbx_call_id
                            || appearance.device_id != transaction.device_id
                    })
                || transaction.consultation.is_none_or(|consultation| {
                    self.appearance_for_call(consultation.handset_call_id)
                        .is_none_or(|appearance| {
                            appearance.pbx_id != consultation.pbx_call_id
                                || appearance.device_id != transaction.device_id
                        })
                })
            {
                return Some(format!(
                    "transfer {:?} has inconsistent call identities",
                    transaction.id
                ));
            }
            if transaction.mode == TransferMode::Consultation
                && transaction.phase != TransferPhase::Completing
                && (self
                    .call_registry
                    .pbx
                    .get(&transaction.source.pbx_call_id)
                    .is_none_or(|call| call.state != CallState::Held)
                    || transaction.consultation.is_none_or(|consultation| {
                        self.call_registry
                            .pbx
                            .get(&consultation.pbx_call_id)
                            .is_none_or(|call| {
                                !matches!(
                                    call.state,
                                    CallState::Collecting
                                        | CallState::TransferCollecting
                                        | CallState::Calling
                                        | CallState::Ringing
                                        | CallState::Connected
                                )
                            })
                    }))
            {
                return Some(format!(
                    "transfer {:?} has inconsistent call states",
                    transaction.id
                ));
            }
        }
        None
    }
}

fn conference_participant_identity(
    call: &PbxCall,
    appearance: &CallAppearance,
) -> ConferenceParticipantIdentity {
    if call.privacy || appearance.privacy || appearance.info.party_restrictions != 0 {
        return ConferenceParticipantIdentity::default();
    }
    let (display_name, number) = match appearance.info.direction {
        CallDirection::Inbound => (
            &appearance.info.calling_name,
            &appearance.info.calling_number,
        ),
        CallDirection::Outbound => (&appearance.info.called_name, &appearance.info.called_number),
    };
    ConferenceParticipantIdentity {
        display_name: display_name.clone(),
        number: number.clone(),
    }
}

fn inbound_call_appearance(
    id: CallAppearanceId,
    pbx_id: PbxCallId,
    candidate: &InboundAppearance,
) -> CallAppearance {
    CallAppearance {
        id,
        sccp_id: candidate.call_id,
        pbx_id,
        device_id: candidate.binding.device_id.clone(),
        line_instance: candidate.binding.line_instance,
        state: CallState::Ringing,
        ring_mode: candidate.binding.appearance.ring_mode,
        privacy: candidate.binding.appearance.privacy,
        info: CallInfo {
            direction: CallDirection::Inbound,
            called_name: candidate.binding.appearance.display_label().to_owned(),
            called_number: candidate.binding.line.number.clone(),
            ..CallInfo::default()
        },
        codec: candidate.codec,
        audio: MediaStreamState::Closed,
        audio_transmit: MediaStreamState::Closed,
        video: VideoMediaState::default(),
        auto_answer_mode: None,
    }
}

fn shared_appearance_state(state: CallState, has_active_appearance: bool) -> CallState {
    match state {
        CallState::Ringing => CallState::Ringing,
        CallState::Held => CallState::SharedHeld,
        CallState::Connected => CallState::RemoteInUse,
        CallState::Collecting
        | CallState::PickupCollecting
        | CallState::Calling
        | CallState::Parking
        | CallState::Retrieving
        | CallState::TransferCollecting
            if has_active_appearance =>
        {
            CallState::RemoteInUse
        }
        state => state,
    }
}

fn appearance_state_effect(
    appearance: &CallAppearance,
    state: HandsetCallState,
    stop_media: bool,
) -> DriverEffect {
    HandsetEffect::SetCallState {
        device_id: appearance.device_id.clone(),
        call_id: appearance.sccp_id,
        state,
        stop_media,
    }
    .into()
}

fn appearance_terminal_effects(appearance: &CallAppearance) -> Vec<DriverEffect> {
    let mut effects = Vec::with_capacity(2);
    if appearance.auto_answer_mode == Some(AutoAnswerMode::OneWay) {
        effects.push(
            HandsetEffect::SetMicrophoneMode {
                device_id: appearance.device_id.clone(),
                call_id: appearance.sccp_id,
                enabled: true,
            }
            .into(),
        );
    }
    effects.push(appearance_state_effect(
        appearance,
        HandsetCallState::OnHook,
        true,
    ));
    effects
}

fn digit_character(digit: Digit) -> Option<char> {
    match digit {
        Digit::Number(number @ 0..=9) => Some(char::from(b'0' + number)),
        Digit::Number(_) => None,
        Digit::Star => Some('*'),
        Digit::Pound => Some('#'),
        Digit::A => Some('A'),
        Digit::B => Some('B'),
        Digit::C => Some('C'),
        Digit::D => Some('D'),
        Digit::Unknown(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::call::transfer::TransferExecutionProgress;
    use crate::config::LineConfig;
    use crate::media::formats::{PbxVideoFormat, negotiate_video_owned};
    use sccp_protocol::{IpAddressType, ReceiveTransmit, VideoCapability, VideoLevelPreference};

    fn binding() -> LineBinding {
        binding_for("SEP001122334455", 1)
    }

    fn assert_outbound_route(effects: &[DriverEffect], expected_destination: &str) {
        assert!(matches!(
            effects,
            [
                DriverEffect::Handset(HandsetEffect::CommitOutboundCall { info, .. }),
                DriverEffect::Backend(PbxEffect::StartRouting { destination, .. })
            ] if info.called_number == expected_destination
                && destination == expected_destination
        ));
    }

    fn test_media_endpoint(codec: Codec) -> MediaEndpoint {
        MediaEndpoint {
            address: "192.0.2.20".parse().unwrap(),
            rtp_port: 20_000,
            rtcp_port: 20_001,
            codec,
            packet_ms: 20,
            max_frames_per_packet: 1,
            telephone_event_payload: 101,
        }
    }

    fn test_video_endpoint(port: u16) -> MediaEndpointAddress {
        MediaEndpointAddress {
            address: "192.0.2.30".parse().unwrap(),
            port,
        }
    }

    fn test_video_plan(controller: &Controller, mode: VideoMode) -> VideoPlan {
        let station = controller.registered_device(&binding().device_id).unwrap();
        let session_generation = station.session_generation;
        let protocol = station.registration.protocol;
        let capabilities = StationMediaCapabilities::new(
            Vec::new(),
            vec![VideoCapability {
                codec: Codec::H264,
                direction: ReceiveTransmit::RECEIVE | ReceiveTransmit::TRANSMIT,
                level_preferences: vec![VideoLevelPreference {
                    transmit_preference: 1,
                    format: 4,
                    max_bit_rate: 384,
                    min_bit_rate: 64,
                    minimum_picture_interval: 1,
                    service_number: 0,
                }],
                codec_parameters: vec![64, 43, 40_500, 1_620, 8_100, 10_000],
                encryption_capability: None,
                address_type: Some(IpAddressType::Ipv4),
            }],
        );
        let negotiated = negotiate_video_owned(
            &[Codec::H264],
            capabilities,
            &[PbxVideoFormat::H264],
            ReceiveTransmit::RECEIVE | ReceiveTransmit::TRANSMIT,
        )
        .unwrap();
        let payload = negotiated
            .multimedia_payload(PbxVideoFormat::H264.payload_type().unwrap())
            .unwrap();
        VideoPlan {
            session_generation,
            protocol,
            mode,
            negotiated,
            payload,
            local_endpoint: test_video_endpoint(30_000),
        }
    }

    fn forwarding(value: &str) -> ForwardingDestination {
        ForwardingDestination::new(value).unwrap()
    }

    fn voicemail_target(value: &str) -> VoicemailTarget {
        VoicemailTarget::new("from-sccp", value).unwrap()
    }

    fn binding_for(device: &str, line_instance: u32) -> LineBinding {
        binding_with_ring(device, line_instance, AppearanceRingMode::Normal)
    }

    fn binding_with_ring(
        device: &str,
        line_instance: u32,
        ring_mode: AppearanceRingMode,
    ) -> LineBinding {
        let mut binding = LineBinding {
            device_id: DeviceId::new(device).unwrap(),
            line_instance,
            appearance: sccp_protocol::LineAppearance::new(
                line_instance,
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
        };
        binding.appearance.ring_mode = ring_mode;
        binding
    }

    fn registration() -> DeviceRegistration {
        registration_for("SEP001122334455")
    }

    fn registration_for(device: &str) -> DeviceRegistration {
        DeviceRegistration {
            id: DeviceId::new(device).unwrap(),
            peer: "192.0.2.10:2000".parse().unwrap(),
            transport: sccp_protocol::StationTransport::Clear,
            reported_address: Some("192.0.2.10".parse().unwrap()),
            reported_ipv6_address: None,
            device_type: sccp_protocol::DeviceType::Cisco7962,
            protocol: sccp_protocol::ProtocolVersion::V22,
            firmware: "SCCP-test".into(),
        }
    }

    fn shared_inbound_controller() -> Controller {
        let mut controller = Controller::new(Duration::from_secs(1));
        controller.registered(registration_for("SEP001122334455"));
        controller.registered(registration_for("SEP112233445566"));
        assert_eq!(
            controller
                .offer_inbound_call(
                    PbxCallId(8),
                    [
                        InboundAppearance {
                            call_id: CallId(2),
                            binding: binding_for("SEP001122334455", 1),
                            codec: Codec::Pcma,
                        },
                        InboundAppearance {
                            call_id: CallId(3),
                            binding: binding_for("SEP112233445566", 2),
                            codec: Codec::Pcmu,
                        },
                    ],
                )
                .len(),
            2
        );
        controller
    }

    fn enable_barge_capabilities(controller: &mut Controller, device: &str, codec: Codec) {
        controller.capabilities(
            &DeviceId::new(device).unwrap(),
            vec![MediaCapability {
                codec,
                max_frames_per_packet: 4,
                codec_parameters: [0; 8],
            }],
        );
    }

    #[derive(Default)]
    struct FakeHandsets {
        effects: Vec<HandsetEffect>,
        announcements: Vec<ConferenceAnnouncementOperation>,
    }

    impl FakeHandsets {
        fn apply(&mut self, effects: &[DriverEffect]) {
            for effect in effects {
                match effect {
                    DriverEffect::Handset(effect) => self.effects.push(effect.clone()),
                    DriverEffect::Backend(PbxEffect::ConferenceAnnouncement { operation }) => {
                        self.announcements.push(operation.clone());
                    }
                    DriverEffect::Backend(_) => {}
                }
            }
        }

        fn media_winners(&self) -> Vec<CallId> {
            self.effects
                .iter()
                .filter_map(|effect| match effect {
                    HandsetEffect::BeginMedia { call_id, .. }
                    | HandsetEffect::BeginAnswerMedia { call_id, .. } => Some(*call_id),
                    _ => None,
                })
                .collect()
        }

        fn call_states(&self) -> Vec<(CallId, HandsetCallState, bool)> {
            self.effects
                .iter()
                .filter_map(|effect| match effect {
                    HandsetEffect::SetCallState {
                        call_id,
                        state,
                        stop_media,
                        ..
                    } => Some((*call_id, *state, *stop_media)),
                    _ => None,
                })
                .collect()
        }

        fn call_info(&self, call_id: CallId) -> Vec<CallInfo> {
            self.effects
                .iter()
                .filter_map(|effect| match effect {
                    HandsetEffect::SetCallInfo {
                        call_id: actual,
                        info,
                        ..
                    } if *actual == call_id => Some(info.clone()),
                    _ => None,
                })
                .collect()
        }

        fn tones(&self, call_id: CallId) -> Vec<Tone> {
            self.effects
                .iter()
                .filter_map(|effect| match effect {
                    HandsetEffect::StartTone {
                        call_id: actual,
                        tone,
                        ..
                    } if *actual == call_id => Some(*tone),
                    _ => None,
                })
                .collect()
        }

        fn announcements(
            &self,
        ) -> Vec<(
            ConferenceId,
            Vec<ParticipantId>,
            Vec<PbxCallId>,
            ConferenceAnnouncement,
        )> {
            self.announcements
                .iter()
                .map(|operation| {
                    (
                        operation.conference_id,
                        operation
                            .targets
                            .iter()
                            .map(|target| target.participant_id)
                            .collect(),
                        operation
                            .targets
                            .iter()
                            .map(|target| target.call_id)
                            .collect(),
                        operation.announcement,
                    )
                })
                .collect()
        }

        fn clear(&mut self) {
            self.effects.clear();
            self.announcements.clear();
        }
    }

    #[test]
    fn adapter_callback_steps_release_the_controller_before_external_work() {
        let controller = Mutex::new(Controller::new(Duration::from_secs(1)));
        let observed = Mutex::new(Vec::new());
        let probe = |phase: &'static str| {
            assert!(
                controller.try_lock().is_ok(),
                "{phase} external work observed a held controller lock"
            );
            observed.lock().unwrap().push(phase);
        };

        controller_step(&controller, |controller| {
            controller.registered(registration_for("SEP001122334455"));
            controller.registered(registration_for("SEP112233445566"));
        });
        probe("registration");
        probe("blf callback");

        controller_step(&controller, |controller| {
            controller.begin_phone_call(
                CallId(10),
                binding_for("SEP001122334455", 1),
                Codec::Pcmu,
                Instant::now(),
            )
        });
        probe("phone event");
        controller_step(&controller, |controller| controller.hangup(CallId(10)));
        probe("phone effect execution");

        controller_step(&controller, |controller| {
            controller.disconnected(&DeviceId::new("SEP001122334455").unwrap())
        });
        probe("disconnect");
        controller_step(&controller, |controller| {
            controller.registered(registration_for("SEP001122334455"))
        });

        let offers = controller_step(&controller, |controller| {
            controller.offer_inbound_call(
                PbxCallId(20),
                [
                    InboundAppearance {
                        call_id: CallId(21),
                        binding: binding_for("SEP001122334455", 1),
                        codec: Codec::Pcmu,
                    },
                    InboundAppearance {
                        call_id: CallId(22),
                        binding: binding_for("SEP112233445566", 2),
                        codec: Codec::Pcmu,
                    },
                ],
            )
        });
        assert_eq!(offers.len(), 2);
        probe("inbound request and fanout");

        controller_step(&controller, |controller| {
            controller.pbx_answer(PbxCallId(20))
        });
        probe("PBX indication");
        controller_step(&controller, |controller| {
            controller.pbx_hangup_with_effects(PbxCallId(20))
        });
        probe("PBX hangup");

        // Reload does not enter the controller at all; the probe guards that
        // its phone reconfiguration and subscription work starts unlocked.
        probe("reload");
        let recording_callback = || probe("recording callback");
        recording_callback();

        assert_eq!(
            *observed.lock().unwrap(),
            [
                "registration",
                "blf callback",
                "phone event",
                "phone effect execution",
                "disconnect",
                "inbound request and fanout",
                "PBX indication",
                "PBX hangup",
                "reload",
                "recording callback",
            ]
        );
    }

    #[test]
    fn asterisk_adapter_uses_the_owned_result_lock_scope_for_every_controller_access() {
        let source = concat!(
            include_str!("../asterisk/mod.rs"),
            include_str!("../asterisk/runtime/management.rs"),
            include_str!("../asterisk/runtime/lifecycle.rs"),
            include_str!("../asterisk/runtime/services.rs"),
            include_str!("../asterisk/phone/calls.rs"),
            include_str!("../asterisk/phone/parking.rs"),
            include_str!("../asterisk/phone/features.rs"),
            include_str!("../asterisk/phone/conference.rs"),
            include_str!("../asterisk/runtime/backend.rs"),
            include_str!("../asterisk/runtime/channel.rs"),
            include_str!("../asterisk/runtime/media.rs"),
            include_str!("../asterisk/runtime/presence.rs"),
            include_str!("../asterisk/runtime/native_support.rs"),
            include_str!("../asterisk/exports.rs"),
        );
        let compact: String = source
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect();

        assert!(compact.contains("controller_step(&access.shared.controller"));
        assert!(
            !compact.contains(".controller.lock("),
            "adapter code bypassed controller_step and acquired the mutex directly"
        );
    }

    #[test]
    fn phone_call_collects_digits_then_starts_dialplan() {
        let now = Instant::now();
        let mut controller = Controller::new(Duration::from_secs(1));
        let actions = controller.begin_phone_call(CallId(7), binding(), Codec::Pcmu, now);
        assert!(matches!(
            actions[0],
            DriverEffect::Backend(PbxEffect::CreateChannel { .. })
        ));
        assert_eq!(actions.len(), 1);
        assert!(
            controller
                .digit(CallId(7), Digit::Number(1), now)
                .is_empty()
        );
        assert!(
            controller
                .digit(CallId(7), Digit::Number(2), now)
                .is_empty()
        );
        let actions = controller.digit(CallId(7), Digit::Pound, now);
        assert_outbound_route(&actions, "12");
        assert_eq!(controller.pbx_call(PbxCallId(1)).unwrap().digits, "12");
    }

    #[test]
    fn configured_conference_destination_commits_one_typed_application_effect() {
        let now = Instant::now();
        let mut controller = Controller::new(Duration::from_secs(1));
        controller.registered(registration());
        controller.begin_phone_call(CallId(7), binding(), Codec::Pcmu, now);

        let effects = controller
            .begin_conference_destination(ConferenceDestinationRequest {
                device_id: binding().device_id,
                handset_call_id: CallId(7),
                destination: "700".into(),
                application_options: "Mac".into(),
            })
            .unwrap();
        assert!(matches!(
            effects.as_slice(),
            [
                DriverEffect::Handset(HandsetEffect::SetCallInfo {
                    call_id: CallId(7),
                    info,
                    ..
                }),
                DriverEffect::Handset(HandsetEffect::StartTone {
                    call_id: CallId(7),
                    tone: Tone::Silence,
                    ..
                }),
                DriverEffect::Handset(HandsetEffect::SetCallState {
                    call_id: CallId(7),
                    state: HandsetCallState::Proceed,
                    stop_media: false,
                    ..
                }),
                DriverEffect::Backend(PbxEffect::StartConferenceDestination {
                    operation: ConferenceDestinationOperation {
                        call_id: PbxCallId(1),
                        destination,
                        application_options,
                        ..
                    },
                }),
            ] if info.called_name == "Conference"
                && info.called_number == "700"
                && destination == "700"
                && application_options == "Mac"
        ));
        assert_eq!(
            controller.call(CallId(7)).unwrap().state,
            CallState::Calling
        );
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn conference_mutation_tokens_reject_stale_and_repeated_completion() {
        let mut first = active_three_party_conference();
        let conference_id = first.conference_session(CallId(4)).unwrap().id;
        let token = first.claim_conference_mutation(CallId(4)).unwrap();
        assert!(first.conference_mutation_is_active(token));
        assert!(first.pbx_hangup_with_effects(PbxCallId(10)).is_some());
        assert!(!first.conference_mutation_is_active(token));
        assert!(!first.complete_conference_mutation(token));

        let mut second = active_three_party_conference();
        let second_id = second.conference_session(CallId(4)).unwrap().id;
        let second_token = second.claim_conference_mutation_by_id(second_id).unwrap();
        assert!(second.conference_mutation_is_active(second_token));
        assert!(second.complete_conference_mutation(second_token));
        assert!(!second.complete_conference_mutation(second_token));
        assert_eq!(conference_id, second_id);
    }

    #[test]
    fn conference_destination_holds_an_ordinary_call_and_rejects_reentry() {
        let now = Instant::now();
        let mut controller = Controller::new(Duration::from_secs(1));
        controller.registered(registration());
        controller.begin_phone_call(CallId(7), binding(), Codec::Pcmu, now);
        controller.enbloc(CallId(7), "2000".into());
        controller.pbx_answer(PbxCallId(1));
        controller.begin_phone_call(CallId(8), binding(), Codec::Pcmu, now);

        let effects = controller
            .begin_conference_destination(ConferenceDestinationRequest {
                device_id: binding().device_id,
                handset_call_id: CallId(8),
                destination: "701".into(),
                application_options: String::new(),
            })
            .unwrap();
        let mutation = controller
            .conference_destination_mutation(CallId(8))
            .unwrap();
        assert!(matches!(
            effects.first(),
            Some(DriverEffect::Backend(PbxEffect::Hold {
                call_id: PbxCallId(1)
            }))
        ));
        assert_eq!(controller.call(CallId(7)).unwrap().state, CallState::Held);
        assert_eq!(
            controller.call(CallId(8)).unwrap().state,
            CallState::Calling
        );
        assert_eq!(
            controller.begin_conference_destination(ConferenceDestinationRequest {
                device_id: binding().device_id,
                handset_call_id: CallId(8),
                destination: "702".into(),
                application_options: "Mac".into(),
            }),
            Err(ConferenceDestinationRejection::Conflict)
        );
        assert_eq!(
            controller.call(CallId(8)).unwrap().info.called_number,
            "701"
        );
        let rollback = controller.conference_destination_failed(
            mutation,
            CallId(8),
            &[PbxCallId(1)],
            &[PbxCallId(1)],
        );
        assert!(matches!(
            rollback.first(),
            Some(DriverEffect::Backend(PbxEffect::Hangup {
                call_id: PbxCallId(2)
            }))
        ));
        assert!(rollback.iter().any(|effect| matches!(
            effect,
            DriverEffect::Backend(PbxEffect::Resume {
                call_id: PbxCallId(1)
            })
        )));
        assert!(controller.call(CallId(8)).is_none());
        assert_eq!(
            controller.call(CallId(7)).unwrap().state,
            CallState::Connected
        );
        assert!(
            controller
                .conference_destination_failed(
                    mutation,
                    CallId(8),
                    &[PbxCallId(1)],
                    &[PbxCallId(1)],
                )
                .is_empty()
        );
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn conference_destination_rejects_missing_or_non_collecting_calls_without_mutation() {
        let now = Instant::now();
        let mut controller = Controller::new(Duration::from_secs(1));
        controller.registered(registration());
        controller.begin_phone_call(CallId(7), binding(), Codec::Pcmu, now);
        let before_pbx_id = controller.call(CallId(7)).unwrap().pbx_id;
        assert_eq!(
            controller.begin_conference_destination(ConferenceDestinationRequest {
                device_id: DeviceId::new("SEP112233445566").unwrap(),
                handset_call_id: CallId(7),
                destination: "700".into(),
                application_options: "Mac".into(),
            }),
            Err(ConferenceDestinationRejection::Unavailable)
        );
        let unchanged = controller.call(CallId(7)).unwrap();
        assert_eq!(unchanged.pbx_id, before_pbx_id);
        assert_eq!(unchanged.state, CallState::Collecting);
        assert!(unchanged.digits.is_empty());
        assert_eq!(
            controller.begin_conference_destination(ConferenceDestinationRequest {
                device_id: binding().device_id,
                handset_call_id: CallId(7),
                destination: String::new(),
                application_options: "Mac".into(),
            }),
            Err(ConferenceDestinationRejection::Unavailable)
        );
        controller.digit(CallId(7), Digit::Number(1), now);
        assert_eq!(
            controller.begin_conference_destination(ConferenceDestinationRequest {
                device_id: binding().device_id,
                handset_call_id: CallId(7),
                destination: "700".into(),
                application_options: "Mac".into(),
            }),
            Err(ConferenceDestinationRejection::Conflict)
        );
        assert_eq!(controller.call(CallId(7)).unwrap().digits, "1");
        assert_eq!(
            controller.call(CallId(7)).unwrap().state,
            CallState::Collecting
        );
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn conference_destination_failed_hold_restores_state_without_an_unexecuted_resume() {
        let now = Instant::now();
        let mut controller = Controller::new(Duration::from_secs(1));
        controller.registered(registration());
        controller.begin_phone_call(CallId(7), binding(), Codec::Pcmu, now);
        controller.enbloc(CallId(7), "2000".into());
        controller.pbx_answer(PbxCallId(1));
        controller.begin_phone_call(CallId(8), binding(), Codec::Pcmu, now);
        controller
            .begin_conference_destination(ConferenceDestinationRequest {
                device_id: binding().device_id,
                handset_call_id: CallId(8),
                destination: "700".into(),
                application_options: "Mac".into(),
            })
            .unwrap();
        let mutation = controller
            .conference_destination_mutation(CallId(8))
            .unwrap();

        let rollback =
            controller.conference_destination_failed(mutation, CallId(8), &[PbxCallId(1)], &[]);
        assert!(
            rollback
                .iter()
                .all(|effect| !matches!(effect, DriverEffect::Backend(PbxEffect::Resume { .. })))
        );
        assert!(matches!(
            rollback.as_slice(),
            [
                DriverEffect::Backend(PbxEffect::Hangup {
                    call_id: PbxCallId(2)
                }),
                DriverEffect::Handset(HandsetEffect::SetCallState {
                    call_id: CallId(8),
                    state: HandsetCallState::OnHook,
                    stop_media: true,
                    ..
                })
            ]
        ));
        assert_eq!(
            controller.call(CallId(7)).unwrap().state,
            CallState::Connected
        );
        assert!(controller.call(CallId(8)).is_none());
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn configured_initial_and_secondary_dial_tones_follow_exact_prefixes() {
        let now = Instant::now();
        let mut controller = Controller::new(Duration::from_secs(1));
        controller.set_line_dial_tones([(
            "1001".into(),
            LineDialToneConfig {
                initial: Tone::RecallDial,
                secondary_prefix: Some("9".into()),
                secondary: Tone::OutsideDial,
            },
        )]);

        let effects = controller.begin_phone_call(CallId(7), binding(), Codec::Pcmu, now);
        assert!(matches!(
            effects.as_slice(),
            [DriverEffect::Backend(PbxEffect::CreateChannel { .. })]
        ));

        assert_eq!(
            controller.digit(CallId(7), Digit::Number(9), now),
            [DriverEffect::Handset(HandsetEffect::StartTone {
                device_id: binding().device_id,
                call_id: CallId(7),
                tone: Tone::OutsideDial,
            })]
        );
        assert!(
            controller
                .digit(CallId(7), Digit::Number(1), now)
                .is_empty(),
            "the secondary tone must only start on the exact configured prefix"
        );
    }

    #[test]
    fn configured_dial_terminator_routes_without_entering_the_destination() {
        let now = Instant::now();
        let mut controller = Controller::new(Duration::from_secs(1));
        controller.set_dial_terminator('*');
        controller.begin_phone_call(CallId(7), binding(), Codec::Pcmu, now);

        assert!(
            controller
                .digit(CallId(7), Digit::Number(1), now)
                .is_empty()
        );
        assert!(controller.digit(CallId(7), Digit::Pound, now).is_empty());
        assert_outbound_route(&controller.digit(CallId(7), Digit::Star, now), "1#");
    }

    #[test]
    fn first_digit_deadline_is_independent_from_subsequent_digits() {
        let now = Instant::now();
        let mut controller =
            Controller::with_digit_timeouts(Duration::from_secs(10), Duration::from_secs(2));
        controller.begin_phone_call(CallId(7), binding(), Codec::Pcmu, now);
        assert_eq!(
            controller.pbx_call(PbxCallId(1)).unwrap().digit_deadline,
            Some(now + Duration::from_secs(10))
        );
        assert!(
            controller
                .expire_digits(now + Duration::from_secs(9))
                .is_empty()
        );

        controller.digit(CallId(7), Digit::Number(1), now + Duration::from_secs(9));
        assert_eq!(
            controller.pbx_call(PbxCallId(1)).unwrap().digit_deadline,
            Some(now + Duration::from_secs(11))
        );
        assert!(
            controller
                .expire_digits(now + Duration::from_secs(10))
                .is_empty()
        );
        assert_outbound_route(
            &controller.expire_digits(now + Duration::from_secs(12)),
            "1",
        );
    }

    #[test]
    fn simulated_enbloc_accelerates_fast_keypad_entry_but_not_slow_entry() {
        let now = Instant::now();
        let mut fast =
            Controller::with_digit_timeouts(Duration::from_secs(10), Duration::from_secs(5));
        fast.begin_phone_call(CallId(7), binding(), Codec::Pcmu, now);
        for (index, digit) in [1, 2, 3, 4].into_iter().enumerate() {
            fast.digit(
                CallId(7),
                Digit::Number(digit),
                now + Duration::from_millis(index as u64 * 100),
            );
        }
        assert_eq!(
            fast.pbx_call(PbxCallId(1)).unwrap().digit_deadline,
            Some(now + Duration::from_millis(2_300))
        );

        let mut slow =
            Controller::with_digit_timeouts(Duration::from_secs(10), Duration::from_secs(5));
        slow.begin_phone_call(CallId(7), binding(), Codec::Pcmu, now);
        slow.digit(CallId(7), Digit::Number(1), now);
        slow.digit(
            CallId(7),
            Digit::Number(2),
            now + Duration::from_millis(500),
        );
        slow.digit(
            CallId(7),
            Digit::Number(3),
            now + Duration::from_millis(600),
        );
        slow.digit(
            CallId(7),
            Digit::Number(4),
            now + Duration::from_millis(700),
        );
        assert_eq!(
            slow.pbx_call(PbxCallId(1)).unwrap().digit_deadline,
            Some(now + Duration::from_millis(5_700))
        );
    }

    #[test]
    fn simulated_enbloc_can_be_disabled_without_changing_direct_enbloc_routing() {
        let now = Instant::now();
        let mut controller =
            Controller::with_digit_timeouts(Duration::from_secs(10), Duration::from_secs(5));
        controller.set_simulated_enbloc(false);
        controller.begin_phone_call(CallId(7), binding(), Codec::Pcmu, now);
        for (index, digit) in [1, 2, 3, 4].into_iter().enumerate() {
            controller.digit(
                CallId(7),
                Digit::Number(digit),
                now + Duration::from_millis(index as u64 * 100),
            );
        }
        assert_eq!(
            controller.pbx_call(PbxCallId(1)).unwrap().digit_deadline,
            Some(now + Duration::from_millis(5_300))
        );
        assert_outbound_route(&controller.enbloc(CallId(7), "8675309".into()), "8675309");
    }

    #[test]
    fn explicit_overlap_starts_on_the_first_digit_and_forwards_the_remainder() {
        let now = Instant::now();
        let binding = binding();
        let mut controller = Controller::new(Duration::from_secs(5));
        controller.set_overlap_devices([binding.device_id.clone()]);
        controller.begin_phone_call(CallId(7), binding, Codec::Pcmu, now);

        assert_outbound_route(&controller.digit(CallId(7), Digit::Number(1), now), "1");
        assert_eq!(
            controller.digit(CallId(7), Digit::Number(2), now),
            [DriverEffect::Backend(PbxEffect::SendDigit {
                call_id: PbxCallId(1),
                digit: '2',
            })]
        );
        assert_eq!(
            controller.digit(CallId(7), Digit::Pound, now),
            [DriverEffect::Backend(PbxEffect::SendDigit {
                call_id: PbxCallId(1),
                digit: '#',
            })]
        );
        assert_eq!(
            controller.pbx_call(PbxCallId(1)).unwrap().state,
            CallState::Calling
        );
        assert_eq!(controller.pbx_call(PbxCallId(1)).unwrap().digits, "1");
    }

    #[test]
    fn overlap_disabled_keeps_collecting_until_an_explicit_completion() {
        let now = Instant::now();
        let mut controller = Controller::new(Duration::from_secs(5));
        controller.begin_phone_call(CallId(7), binding(), Codec::Pcmu, now);

        assert!(
            controller
                .digit(CallId(7), Digit::Number(1), now)
                .is_empty()
        );
        assert_eq!(
            controller.pbx_call(PbxCallId(1)).unwrap().state,
            CallState::Collecting
        );
    }

    #[test]
    fn pre_dial_codec_change_is_guarded_and_updates_the_snapshot() {
        let now = Instant::now();
        let mut controller = Controller::new(Duration::from_secs(1));
        controller.begin_phone_call(CallId(7), binding(), Codec::Pcmu, now);

        assert_eq!(
            controller.set_pre_dial_codec(PbxCallId(1), Codec::G72264k),
            Ok(Codec::Pcmu)
        );
        assert_eq!(controller.call(CallId(7)).unwrap().codec, Codec::G72264k);

        controller.digit(CallId(7), Digit::Number(1), now);
        controller.digit(CallId(7), Digit::Pound, now);
        assert_eq!(
            controller.set_pre_dial_codec(PbxCallId(1), Codec::Pcma),
            Ok(Codec::G72264k)
        );
        assert_eq!(controller.call(CallId(7)).unwrap().codec, Codec::Pcma);

        controller.pbx_progress(PbxCallId(1), true);
        assert_eq!(
            controller.set_pre_dial_codec(PbxCallId(1), Codec::Pcmu),
            Err(CodecPreferenceRejection::NotPreDial)
        );
        assert_eq!(controller.call(CallId(7)).unwrap().codec, Codec::Pcma);
        assert_eq!(
            controller.set_pre_dial_codec(PbxCallId(999), Codec::Pcmu),
            Err(CodecPreferenceRejection::Unavailable)
        );
    }

    #[test]
    fn call_snapshots_are_derived_from_current_call_and_appearance_state() {
        let now = Instant::now();
        let mut controller = Controller::new(Duration::from_secs(1));
        controller.begin_phone_call(CallId(7), binding(), Codec::Pcmu, now);
        let before = controller.call(CallId(7)).unwrap();

        assert!(
            controller
                .digit(CallId(7), Digit::Number(4), now)
                .is_empty()
        );
        let mut info = controller.call_info(CallId(7)).unwrap().clone();
        info.called_name = "Destination".into();
        info.called_number = "4001".into();
        controller.set_call_info(CallId(7), info.clone());
        let metadata = CallMetadata {
            account_code: Some("sales".into()),
            ..CallMetadata::default()
        };
        assert_eq!(
            controller.set_call_metadata(PbxCallId(1), metadata.clone()),
            Ok(true)
        );
        assert_eq!(
            controller.set_pre_dial_codec(PbxCallId(1), Codec::G72264k),
            Ok(Codec::Pcmu)
        );

        let after = controller.call(CallId(7)).unwrap();
        assert_eq!(before.digits, "");
        assert_eq!(before.info.called_number, "");
        assert!(before.metadata.account_code.is_none());
        assert_eq!(before.codec, Codec::Pcmu);
        assert_eq!(after.digits, "4");
        assert_eq!(after.info, info);
        assert!(after.metadata == metadata);
        assert_eq!(after.codec, Codec::G72264k);

        let by_pbx = controller.call_by_pbx(PbxCallId(1)).unwrap();
        assert_eq!(by_pbx.digits, after.digits);
        assert_eq!(by_pbx.info, after.info);
        assert!(by_pbx.metadata == after.metadata);
        assert_eq!(controller.calls().next().unwrap().codec, after.codec);
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn early_media_modes_are_explicit_and_answer_reuses_the_stream() {
        let now = Instant::now();
        let mut controller = Controller::new(Duration::from_secs(1));
        controller.begin_phone_call(CallId(7), binding(), Codec::Pcmu, now);
        controller.digit(CallId(7), Digit::Number(1), now);
        controller.digit(CallId(7), Digit::Pound, now);

        assert!(controller.pbx_progress(PbxCallId(1), false).is_empty());
        assert_eq!(
            controller.call(CallId(7)).unwrap().audio,
            MediaStreamState::Closed
        );
        assert_eq!(
            controller
                .pbx_progress_with_media_mode(PbxCallId(1), true, OutboundMediaMode::Coupled,),
            [DriverEffect::Handset(HandsetEffect::BeginOutboundMedia {
                device_id: DeviceId::new("SEP001122334455").unwrap(),
                call_id: CallId(7),
                codec: Codec::Pcmu,
            })]
        );
        assert_eq!(
            controller.call(CallId(7)).unwrap().audio,
            MediaStreamState::Opening
        );
        assert_eq!(
            controller.call(CallId(7)).unwrap().audio_transmit,
            MediaStreamState::Opening
        );
        assert!(
            controller
                .pbx_progress_with_media_mode(PbxCallId(1), true, OutboundMediaMode::Coupled,)
                .is_empty()
        );

        let endpoint = MediaEndpoint {
            address: "192.0.2.20".parse().unwrap(),
            rtp_port: 20_000,
            rtcp_port: 20_001,
            codec: Codec::Pcmu,
            packet_ms: 20,
            max_frames_per_packet: 1,
            telephone_event_payload: 101,
        };
        assert!(matches!(
            controller.media_opened(CallId(7), endpoint).as_slice(),
            [
                DriverEffect::Handset(HandsetEffect::StartTone {
                    tone: Tone::Silence,
                    ..
                }),
                DriverEffect::Handset(HandsetEffect::SetCallInfo { .. }),
                DriverEffect::Backend(PbxEffect::ConfigureMediaOnly { .. })
            ]
        ));
        assert_eq!(
            controller.call(CallId(7)).unwrap().audio_transmit,
            MediaStreamState::Opening
        );
        let transmit_endpoint = MediaEndpoint {
            address: "192.0.2.21".parse().unwrap(),
            rtp_port: 20_002,
            rtcp_port: 20_003,
            ..endpoint
        };
        assert!(
            controller
                .media_transmission_started(CallId(7), transmit_endpoint)
                .is_empty()
        );
        assert_eq!(
            controller.call(CallId(7)).unwrap().audio_transmit,
            MediaStreamState::Open(transmit_endpoint)
        );
        let stale_endpoint = MediaEndpoint {
            rtp_port: 20_004,
            ..transmit_endpoint
        };
        controller.media_transmission_started(CallId(7), stale_endpoint);
        assert_eq!(
            controller.call(CallId(7)).unwrap().audio_transmit,
            MediaStreamState::Open(transmit_endpoint)
        );

        assert_eq!(
            controller.media_retarget_started(CallId(7)),
            Some(transmit_endpoint)
        );
        assert_eq!(
            controller.call(CallId(7)).unwrap().audio_transmit,
            MediaStreamState::Opening
        );
        assert!(controller.media_retarget_enqueue_failed(CallId(7), transmit_endpoint));
        assert_eq!(
            controller.call(CallId(7)).unwrap().audio_transmit,
            MediaStreamState::Open(transmit_endpoint)
        );

        assert_eq!(
            controller.media_retarget_started(CallId(7)),
            Some(transmit_endpoint)
        );
        let retargeted_endpoint = MediaEndpoint {
            address: "192.0.2.22".parse().unwrap(),
            rtp_port: 20_006,
            rtcp_port: 20_007,
            ..transmit_endpoint
        };
        controller.media_transmission_started(CallId(7), retargeted_endpoint);
        assert_eq!(
            controller.call(CallId(7)).unwrap().audio_transmit,
            MediaStreamState::Open(retargeted_endpoint)
        );
        let previous = controller
            .media_retarget_compensation_started(CallId(7))
            .unwrap();
        assert_eq!(previous, MediaStreamState::Open(retargeted_endpoint));
        assert_eq!(
            controller.call(CallId(7)).unwrap().audio_transmit,
            MediaStreamState::Opening
        );
        assert!(controller.media_retarget_compensation_enqueue_failed(CallId(7), previous));
        assert_eq!(
            controller.call(CallId(7)).unwrap().audio_transmit,
            MediaStreamState::Open(retargeted_endpoint)
        );
        assert!(!controller.media_retarget_enqueue_failed(CallId(7), transmit_endpoint));
        assert_eq!(
            controller.pbx_answer(PbxCallId(1)),
            [DriverEffect::Handset(HandsetEffect::SetCallState {
                device_id: DeviceId::new("SEP001122334455").unwrap(),
                call_id: CallId(7),
                state: HandsetCallState::Connected,
                stop_media: false,
            })]
        );
        assert_eq!(
            controller.call(CallId(7)).unwrap().audio,
            MediaStreamState::Open(endpoint)
        );
        assert!(controller.pbx_progress(PbxCallId(1), true).is_empty());
        assert!(controller.invariant_error().is_none());

        let mut answer_race = Controller::new(Duration::from_secs(1));
        answer_race.begin_phone_call(CallId(8), binding(), Codec::Pcmu, now);
        answer_race.digit(CallId(8), Digit::Number(2), now);
        answer_race.digit(CallId(8), Digit::Pound, now);
        answer_race.pbx_progress_with_media_mode(PbxCallId(1), true, OutboundMediaMode::Coupled);
        assert!(answer_race.pbx_answer(PbxCallId(1)).is_empty());
        assert_eq!(
            answer_race.call(CallId(8)).unwrap().audio,
            MediaStreamState::Opening
        );
        assert_eq!(
            answer_race.call(CallId(8)).unwrap().audio_transmit,
            MediaStreamState::Opening
        );
        assert!(matches!(
            answer_race.media_opened(CallId(8), endpoint).as_slice(),
            [
                DriverEffect::Handset(HandsetEffect::StartTone {
                    tone: Tone::Silence,
                    ..
                }),
                DriverEffect::Handset(HandsetEffect::SetCallInfo { .. }),
                DriverEffect::Backend(PbxEffect::ConfigureMediaOnly { .. }),
                DriverEffect::Handset(HandsetEffect::SetCallState {
                    state: HandsetCallState::Connected,
                    ..
                })
            ]
        ));
        assert!(answer_race.invariant_error().is_none());

        let mut no_early_media = Controller::new(Duration::from_secs(1));
        no_early_media.begin_phone_call(CallId(9), binding(), Codec::Pcmu, now);
        no_early_media.digit(CallId(9), Digit::Number(2), now);
        no_early_media.digit(CallId(9), Digit::Pound, now);
        assert!(matches!(
            no_early_media.pbx_answer(PbxCallId(1)).as_slice(),
            [DriverEffect::Handset(HandsetEffect::BeginMedia {
                call_id: CallId(9),
                ..
            })]
        ));
        assert!(matches!(
            no_early_media.media_opened(CallId(9), endpoint).as_slice(),
            [
                DriverEffect::Handset(HandsetEffect::SetCallInfo { .. }),
                DriverEffect::Backend(PbxEffect::ConfigureMedia { .. })
            ]
        ));
        assert!(no_early_media.invariant_error().is_none());

        let mut staged = Controller::new(Duration::from_secs(1));
        staged.begin_phone_call(CallId(10), binding(), Codec::Pcmu, now);
        staged.digit(CallId(10), Digit::Number(2), now);
        staged.digit(CallId(10), Digit::Pound, now);
        assert!(matches!(
            staged
                .pbx_progress_with_media_mode(PbxCallId(1), true, OutboundMediaMode::Staged,)
                .as_slice(),
            [DriverEffect::Handset(HandsetEffect::BeginEarlyMedia {
                call_id: CallId(10),
                ..
            })]
        ));
        assert_eq!(
            staged.call(CallId(10)).unwrap().audio_transmit,
            MediaStreamState::Closed
        );
        assert!(matches!(
            staged.pbx_answer(PbxCallId(1)).as_slice(),
            [DriverEffect::Handset(HandsetEffect::SetCallState {
                state: HandsetCallState::Connected,
                ..
            })]
        ));
        assert!(matches!(
            staged.media_opened(CallId(10), endpoint).as_slice(),
            [
                DriverEffect::Handset(HandsetEffect::SetCallInfo { .. }),
                DriverEffect::Backend(PbxEffect::ConfigureMedia { .. })
            ]
        ));
        assert_eq!(
            staged.call(CallId(10)).unwrap().audio_transmit,
            MediaStreamState::Opening
        );

        let mut staged_answer = Controller::new(Duration::from_secs(1));
        staged_answer.begin_phone_call(CallId(11), binding(), Codec::Pcmu, now);
        staged_answer.digit(CallId(11), Digit::Number(2), now);
        staged_answer.digit(CallId(11), Digit::Pound, now);
        assert!(matches!(
            staged_answer.pbx_answer(PbxCallId(1)).as_slice(),
            [DriverEffect::Handset(HandsetEffect::BeginMedia {
                call_id: CallId(11),
                ..
            })]
        ));
        assert!(!staged_answer.coupled_outbound_media_pending(CallId(11)));
        let wrong_device = DeviceId::new("SEP112233445566").unwrap();
        assert!(
            staged_answer
                .media_opened_for_device(&wrong_device, CallId(11), endpoint)
                .is_empty()
        );
        assert_eq!(
            staged_answer.call(CallId(11)).unwrap().audio,
            MediaStreamState::Opening
        );
        let owner = binding().device_id;
        assert!(matches!(
            staged_answer
                .media_opened_for_device(&owner, CallId(11), endpoint)
                .as_slice(),
            [
                DriverEffect::Handset(HandsetEffect::SetCallInfo { .. }),
                DriverEffect::Backend(PbxEffect::ConfigureMedia { .. })
            ]
        ));
        assert!(
            staged_answer
                .media_transmission_started_for_device(&wrong_device, CallId(11), endpoint)
                .is_empty()
        );
        assert_eq!(
            staged_answer.call(CallId(11)).unwrap().audio_transmit,
            MediaStreamState::Opening
        );
        staged_answer.media_transmission_started_for_device(&owner, CallId(11), endpoint);
        assert_eq!(
            staged_answer.call(CallId(11)).unwrap().audio_transmit,
            MediaStreamState::Open(endpoint)
        );
        assert!(staged_answer.invariant_error().is_none());
    }

    #[test]
    fn coupled_media_keeps_an_explicit_transmit_ack_open_when_receive_ack_arrives_later() {
        let now = Instant::now();
        let mut controller = Controller::new(Duration::from_secs(1));
        controller.begin_phone_call(CallId(7), binding(), Codec::Pcmu, now);
        controller.digit(CallId(7), Digit::Number(2), now);
        controller.digit(CallId(7), Digit::Pound, now);
        assert!(matches!(
            controller
                .pbx_progress_with_media_mode(PbxCallId(1), true, OutboundMediaMode::Coupled)
                .as_slice(),
            [DriverEffect::Handset(
                HandsetEffect::BeginOutboundMedia { .. }
            )]
        ));

        let transmit_endpoint = MediaEndpoint {
            address: "192.0.2.21".parse().unwrap(),
            rtp_port: 20_002,
            rtcp_port: 20_003,
            codec: Codec::Pcmu,
            packet_ms: 20,
            max_frames_per_packet: 1,
            telephone_event_payload: 101,
        };
        assert!(
            controller
                .media_transmission_started(CallId(7), transmit_endpoint)
                .is_empty()
        );
        assert_eq!(
            controller.call(CallId(7)).unwrap().audio_transmit,
            MediaStreamState::Open(transmit_endpoint)
        );

        let receive_endpoint = MediaEndpoint {
            address: "192.0.2.20".parse().unwrap(),
            rtp_port: 20_000,
            rtcp_port: 20_001,
            ..transmit_endpoint
        };
        assert!(matches!(
            controller
                .media_opened(CallId(7), receive_endpoint)
                .as_slice(),
            [
                DriverEffect::Handset(HandsetEffect::StartTone {
                    tone: Tone::Silence,
                    ..
                }),
                DriverEffect::Handset(HandsetEffect::SetCallInfo { .. }),
                DriverEffect::Backend(PbxEffect::ConfigureMediaOnly { .. })
            ]
        ));
        assert_eq!(
            controller.call(CallId(7)).unwrap().audio,
            MediaStreamState::Open(receive_endpoint)
        );
        assert_eq!(
            controller.call(CallId(7)).unwrap().audio_transmit,
            MediaStreamState::Open(transmit_endpoint)
        );
        assert!(!controller.coupled_outbound_media_pending(CallId(7)));
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn outbound_signalling_advances_monotonically_without_regressing_proceed() {
        let now = Instant::now();
        let mut controller = Controller::new(Duration::from_secs(1));
        controller.begin_phone_call(CallId(7), binding(), Codec::Pcmu, now);
        assert_outbound_route(&controller.enbloc(CallId(7), "2200".into()), "2200");

        assert!(matches!(
            controller.pbx_proceeding(PbxCallId(1)).as_slice(),
            [DriverEffect::Handset(
                HandsetEffect::PresentOutboundProceeding { info, .. }
            )] if info.called_number == "2200"
        ));
        controller.update_call_info_by_pbx(PbxCallId(1), |info| {
            let mut info = info.clone();
            info.called_name = "Remote Party".into();
            info
        });
        assert!(
            controller
                .pbx_remote_identity_ready(PbxCallId(1))
                .is_empty()
        );
        assert!(matches!(
            controller.pbx_ringing(PbxCallId(1)).as_slice(),
            [
                DriverEffect::Handset(HandsetEffect::PresentOutboundRinging { info, .. }),
                DriverEffect::Handset(HandsetEffect::SetCallState {
                    state: HandsetCallState::RingOut,
                    ..
                }),
                DriverEffect::Handset(HandsetEffect::SetCallInfo { .. })
            ] if info.called_name == "Remote Party" && info.called_number == "2200"
        ));
        assert!(controller.pbx_ringing(PbxCallId(1)).is_empty());
        assert!(controller.pbx_proceeding(PbxCallId(1)).is_empty());

        assert!(
            controller
                .pbx_remote_identity_ready(PbxCallId(1))
                .is_empty()
        );

        assert!(controller.pbx_progress(PbxCallId(1), false).is_empty());
        assert!(controller.pbx_ringing(PbxCallId(1)).is_empty());
        assert!(controller.pbx_proceeding(PbxCallId(1)).is_empty());
        assert!(matches!(
            controller.pbx_answer(PbxCallId(1)).as_slice(),
            [DriverEffect::Handset(HandsetEffect::BeginMedia {
                call_id: CallId(7),
                ..
            })]
        ));
        assert!(controller.pbx_answer(PbxCallId(1)).is_empty());
        assert!(controller.pbx_progress(PbxCallId(1), true).is_empty());
        assert!(controller.pbx_ringing(PbxCallId(1)).is_empty());
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn outbound_remote_identity_cannot_regress_progress_to_ring_out() {
        let now = Instant::now();
        let mut controller = Controller::new(Duration::from_secs(1));
        controller.begin_phone_call(CallId(7), binding(), Codec::Pcmu, now);
        controller.enbloc(CallId(7), "2200".into());
        controller.pbx_ringing(PbxCallId(1));
        controller.pbx_progress(PbxCallId(1), false);

        assert!(
            controller
                .pbx_remote_identity_ready(PbxCallId(1))
                .is_empty()
        );
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn connected_digits_are_forwarded_without_collection() {
        let mut controller = Controller::new(Duration::from_secs(1));
        controller.begin_asterisk_call(CallId(2), PbxCallId(8), &binding(), Codec::Pcma);
        controller.phone_answer(CallId(2));
        assert_eq!(
            controller.digit(CallId(2), Digit::Number(5), Instant::now()),
            [DriverEffect::Backend(PbxEffect::SendDigit {
                call_id: PbxCallId(8),
                digit: '5'
            })]
        );
        assert_eq!(
            controller.enbloc(CallId(2), "12#d".into()),
            [
                DriverEffect::Backend(PbxEffect::SendDigit {
                    call_id: PbxCallId(8),
                    digit: '1',
                }),
                DriverEffect::Backend(PbxEffect::SendDigit {
                    call_id: PbxCallId(8),
                    digit: '2',
                }),
                DriverEffect::Backend(PbxEffect::SendDigit {
                    call_id: PbxCallId(8),
                    digit: '#',
                }),
                DriverEffect::Backend(PbxEffect::SendDigit {
                    call_id: PbxCallId(8),
                    digit: 'D',
                }),
            ]
        );
        assert!(controller.enbloc(CallId(2), "12x".into()).is_empty());
        assert_eq!(
            controller.pbx_call(PbxCallId(8)).unwrap().state,
            CallState::Connected,
            "connected en-bloc DTMF must not restart dialplan routing"
        );
    }

    #[test]
    fn party_updates_fan_out_in_appearance_order_and_preserve_local_identity() {
        let mut controller = shared_inbound_controller();
        let effects = controller.update_call_info_by_pbx(PbxCallId(8), |current| {
            let mut info = current.clone();
            info.calling_name = "Updated caller".into();
            info.calling_number = "2100".into();
            info.original_called_number = "2000".into();
            info.last_redirecting_number = "2050".into();
            info.last_redirect_reason = 4;
            info
        });

        assert!(matches!(
            effects.as_slice(),
            [
                DriverEffect::Handset(HandsetEffect::SetCallInfo {
                    call_id: CallId(2),
                    ..
                }),
                DriverEffect::Handset(HandsetEffect::SetCallInfo {
                    call_id: CallId(3),
                    ..
                })
            ]
        ));
        for call_id in [CallId(2), CallId(3)] {
            let info = controller.call_info(call_id).unwrap();
            assert_eq!(info.calling_name, "Updated caller");
            assert_eq!(info.calling_number, "2100");
            assert_eq!(info.called_name, "Desk");
            assert_eq!(info.called_number, "1001");
            assert_eq!(info.original_called_number, "2000");
            assert_eq!(info.last_redirecting_number, "2050");
            assert_eq!(info.last_redirect_reason, 4);
        }
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn group_pickup_requires_permission_and_serializes_one_attempt() {
        let mut controller = Controller::new(Duration::from_secs(1));
        controller.registered(registration());
        controller.begin_phone_call(CallId(7), binding(), Codec::Pcmu, Instant::now());

        assert_eq!(
            controller.group_pickup(CallId(7), false, true),
            Err(PickupRejection::Permission)
        );
        assert_eq!(
            controller.group_pickup(CallId(7), true, true).unwrap(),
            [DriverEffect::Backend(PbxEffect::Pickup {
                operation: PickupOperation::Group {
                    call_id: PbxCallId(1),
                    device_id: DeviceId::new("SEP001122334455").unwrap(),
                    handset_call_id: CallId(7),
                    codec: Codec::Pcmu,
                    answer: true,
                },
            })]
        );
        assert_eq!(
            controller.call(CallId(7)).unwrap().state,
            CallState::Connected
        );
        assert_eq!(
            controller.group_pickup(CallId(7), true, true),
            Err(PickupRejection::Conflict)
        );
        assert!(controller.invariant_error().is_none());

        let mut ringing = Controller::new(Duration::from_secs(1));
        ringing.registered(registration());
        ringing.begin_phone_call(CallId(8), binding(), Codec::Pcmu, Instant::now());
        ringing.group_pickup(CallId(8), true, false).unwrap();
        assert_eq!(ringing.call(CallId(8)).unwrap().state, CallState::Ringing);
        assert!(
            ringing
                .pbx_call(PbxCallId(1))
                .unwrap()
                .active_appearance()
                .is_none()
        );
        assert!(!ringing.phone_answer(CallId(8)).is_empty());
        assert!(ringing.invariant_error().is_none());
    }

    #[test]
    fn parking_requires_the_active_connected_owner_and_rolls_back_cleanly() {
        let mut controller = Controller::new(Duration::from_secs(1));
        controller.registered(registration());
        controller.begin_asterisk_call(CallId(2), PbxCallId(8), &binding(), Codec::Pcma);
        controller.phone_answer(CallId(2));

        assert_eq!(
            controller.park(CallId(2), false, Some("executive".into())),
            Err(ParkingRejection::Disabled)
        );
        assert_eq!(
            controller
                .park(CallId(2), true, Some("executive".into()))
                .unwrap(),
            [DriverEffect::Backend(PbxEffect::Parking {
                operation: ParkingOperation::Park {
                    call_id: PbxCallId(8),
                    lot: Some("executive".into()),
                },
            })]
        );
        assert_eq!(
            controller.call(CallId(2)).unwrap().state,
            CallState::Parking
        );
        assert_eq!(
            controller.park(CallId(2), true, None),
            Err(ParkingRejection::Conflict)
        );
        assert_eq!(
            controller.parking_failed(CallId(2)),
            [DriverEffect::Handset(HandsetEffect::SetCallState {
                device_id: DeviceId::new("SEP001122334455").unwrap(),
                call_id: CallId(2),
                state: HandsetCallState::Connected,
                stop_media: false,
            })]
        );
        assert_eq!(
            controller.call(CallId(2)).unwrap().state,
            CallState::Connected
        );
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn assigned_parking_slot_is_published_before_owner_channel_cleanup() {
        let mut controller = Controller::new(Duration::from_secs(1));
        controller.registered(registration());
        controller.begin_asterisk_call(CallId(2), PbxCallId(8), &binding(), Codec::Pcma);
        controller.phone_answer(CallId(2));
        controller.park(CallId(2), true, None).unwrap();

        let effects = controller.parking_confirmed(CallId(2), 701);
        assert!(matches!(
            &effects[0],
            DriverEffect::Handset(HandsetEffect::SetCallInfo { info, .. })
                if info.called_number == "701" && info.called_name == "Parked"
        ));
        assert!(matches!(
            effects[1],
            DriverEffect::Handset(HandsetEffect::SetCallState {
                state: HandsetCallState::Park,
                ..
            })
        ));
        assert_eq!(
            effects[2],
            DriverEffect::Backend(PbxEffect::Hangup {
                call_id: PbxCallId(8)
            })
        );
        assert!(controller.call(CallId(2)).is_none());
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn retrieval_has_one_call_identity_and_failure_cleans_every_index() {
        let mut controller = Controller::new(Duration::from_secs(1));
        controller.registered(registration());
        let info = CallInfo {
            direction: CallDirection::Inbound,
            calling_name: "Caller".into(),
            calling_number: "2100".into(),
            called_name: "Park 701".into(),
            called_number: "701".into(),
            ..CallInfo::default()
        };

        let effects = controller
            .begin_parking_retrieval(
                CallId(22),
                binding(),
                Codec::Pcmu,
                Some("main".into()),
                701,
                info.clone(),
            )
            .unwrap();
        assert!(matches!(
            effects.as_slice(),
            [
                DriverEffect::Backend(PbxEffect::CreateChannel { .. }),
                DriverEffect::Handset(HandsetEffect::SetCallInfo { info: actual, .. }),
                DriverEffect::Backend(PbxEffect::Parking {
                    operation: ParkingOperation::Retrieve { slot, .. }
                })
            ] if actual == &info && slot == "701"
        ));
        assert_eq!(
            controller.call(CallId(22)).unwrap().state,
            CallState::Retrieving
        );
        assert_eq!(
            controller.begin_parking_retrieval(
                CallId(22),
                binding(),
                Codec::Pcmu,
                Some("main".into()),
                701,
                info,
            ),
            Err(ParkingRejection::Conflict)
        );
        let cleanup = controller.parking_retrieval_failed(CallId(22));
        assert_eq!(
            cleanup,
            [
                DriverEffect::Backend(PbxEffect::Hangup {
                    call_id: PbxCallId(1)
                }),
                DriverEffect::Handset(HandsetEffect::SetCallState {
                    device_id: binding().device_id,
                    call_id: CallId(22),
                    state: HandsetCallState::OnHook,
                    stop_media: true,
                })
            ]
        );
        assert!(controller.call(CallId(22)).is_none());
        assert!(controller.call_by_pbx(PbxCallId(1)).is_none());
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn retrieval_confirmation_enters_connected_media_once() {
        let mut controller = Controller::new(Duration::from_secs(1));
        controller.registered(registration());
        controller
            .begin_parking_retrieval(
                CallId(22),
                binding(),
                Codec::Pcma,
                None,
                701,
                CallInfo {
                    direction: CallDirection::Inbound,
                    calling_name: "Caller".into(),
                    calling_number: "2100".into(),
                    called_name: "Park 701".into(),
                    called_number: "701".into(),
                    ..CallInfo::default()
                },
            )
            .unwrap();

        let effects = controller.parking_retrieved(CallId(22));
        assert!(matches!(
            effects.as_slice(),
            [
                DriverEffect::Handset(HandsetEffect::SetCallState {
                    state: HandsetCallState::Connected,
                    ..
                }),
                DriverEffect::Handset(HandsetEffect::BeginMedia {
                    codec: Codec::Pcma,
                    ..
                })
            ]
        ));
        assert_eq!(
            controller.call(CallId(22)).unwrap().state,
            CallState::Connected
        );
        assert!(controller.parking_retrieved(CallId(22)).is_empty());
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn directed_pickup_collects_extension_and_preserves_context_and_answer_policy() {
        let mut controller = Controller::new(Duration::from_secs(1));
        controller.registered(registration());
        controller.begin_phone_call(CallId(7), binding(), Codec::Pcma, Instant::now());

        assert_eq!(
            controller.begin_directed_pickup(
                CallId(7),
                true,
                false,
                "pickup-context".into(),
                false,
            ),
            Err(PickupRejection::Disabled)
        );
        assert_eq!(
            controller.begin_directed_pickup(
                CallId(7),
                false,
                true,
                "pickup-context".into(),
                false,
            ),
            Err(PickupRejection::Permission)
        );
        controller
            .begin_directed_pickup(CallId(7), true, true, "pickup-context".into(), false)
            .unwrap();
        assert_eq!(
            controller.call(CallId(7)).unwrap().state,
            CallState::PickupCollecting
        );
        assert_eq!(
            controller
                .begin_directed_pickup(CallId(7), true, true, "pickup-context".into(), false,),
            Err(PickupRejection::Conflict)
        );
        controller.digit(CallId(7), Digit::Number(2), Instant::now());
        controller.digit(CallId(7), Digit::Number(1), Instant::now());
        controller.digit(CallId(7), Digit::Number(0), Instant::now());
        assert_eq!(
            controller.digit(CallId(7), Digit::Pound, Instant::now()),
            [DriverEffect::Backend(PbxEffect::Pickup {
                operation: PickupOperation::Directed {
                    call_id: PbxCallId(1),
                    device_id: DeviceId::new("SEP001122334455").unwrap(),
                    handset_call_id: CallId(7),
                    codec: Codec::Pcma,
                    extension: "210".into(),
                    context: "pickup-context".into(),
                    answer: false,
                },
            })]
        );
        assert_eq!(
            controller.call(CallId(7)).unwrap().state,
            CallState::Ringing
        );
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn dial_softkey_finishes_previously_collected_digits() {
        let now = Instant::now();
        let mut controller = Controller::new(Duration::from_secs(1));
        controller.begin_phone_call(CallId(7), binding(), Codec::Pcmu, now);
        controller.digit(CallId(7), Digit::Number(1), now);
        controller.digit(CallId(7), Digit::Number(2), now);

        assert_outbound_route(&controller.enbloc(CallId(7), String::new()), "12");
    }

    #[test]
    fn disconnect_hangs_up_device_calls() {
        let mut controller = Controller::new(Duration::from_secs(1));
        let device = binding().device_id;
        controller.registered(registration());
        controller.begin_asterisk_call(CallId(2), PbxCallId(8), &binding(), Codec::Pcma);
        assert_eq!(
            controller.disconnected(&device),
            [DriverEffect::Backend(PbxEffect::Hangup {
                call_id: PbxCallId(8)
            })]
        );
        assert!(controller.call(CallId(2)).is_none());
    }

    #[test]
    fn inbound_offer_fans_out_in_order_to_registered_ringable_appearances() {
        let mut controller = Controller::new(Duration::from_secs(1));
        for device in ["SEP001122334455", "SEP112233445566", "SEP223344556677"] {
            controller.registered(registration_for(device));
        }
        let offers = controller.offer_inbound_call(
            PbxCallId(8),
            [
                InboundAppearance {
                    call_id: CallId(30),
                    binding: binding_with_ring("SEP112233445566", 2, AppearanceRingMode::Silent),
                    codec: Codec::Pcmu,
                },
                InboundAppearance {
                    call_id: CallId(20),
                    binding: binding_for("SEP001122334455", 1),
                    codec: Codec::Pcma,
                },
                InboundAppearance {
                    call_id: CallId(40),
                    binding: binding_with_ring("SEP223344556677", 3, AppearanceRingMode::Disabled),
                    codec: Codec::G72264k,
                },
                InboundAppearance {
                    call_id: CallId(50),
                    binding: binding_for("SEP334455667788", 4),
                    codec: Codec::Pcma,
                },
            ],
        );

        assert_eq!(
            offers,
            [
                InboundOffer {
                    device_id: DeviceId::new("SEP112233445566").unwrap(),
                    line_instance: 2,
                    call_id: CallId(30),
                    ring_mode: AppearanceRingMode::Silent,
                    state: HandsetCallState::RingIn,
                },
                InboundOffer {
                    device_id: DeviceId::new("SEP001122334455").unwrap(),
                    line_instance: 1,
                    call_id: CallId(20),
                    ring_mode: AppearanceRingMode::Normal,
                    state: HandsetCallState::RingIn,
                },
            ]
        );
        assert_eq!(controller.inbound_offers_for_pbx(PbxCallId(8)), offers);
        assert_eq!(controller.appearances_for_pbx(PbxCallId(8)).count(), 2);
        assert!(
            controller
                .appearances_for_pbx(PbxCallId(8))
                .all(|appearance| appearance.state == CallState::Ringing)
        );
        assert_eq!(
            controller
                .pbx_call(PbxCallId(8))
                .unwrap()
                .active_appearance(),
            None
        );
        assert!(controller.invariant_error().is_none());
        assert!(controller.cancel_inbound_offer(CallId(30)));
        assert_eq!(controller.appearances_for_pbx(PbxCallId(8)).count(), 1);
        assert!(controller.cancel_inbound_offer(CallId(20)));
        assert!(controller.pbx_call(PbxCallId(8)).is_none());
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn per_appearance_ring_policy_is_independent_across_concurrent_offer_snapshots() {
        let first = DeviceId::new("SEP001122334455").unwrap();
        let second = DeviceId::new("SEP112233445566").unwrap();
        let mut controller = Controller::new(Duration::from_secs(1));
        controller.registered(registration_for(first.as_str()));
        controller.registered(registration_for(second.as_str()));

        let first_offers = controller.offer_inbound_call(
            PbxCallId(8),
            [
                InboundAppearance {
                    call_id: CallId(20),
                    binding: binding_with_ring(first.as_str(), 1, AppearanceRingMode::Normal),
                    codec: Codec::Pcma,
                },
                InboundAppearance {
                    call_id: CallId(30),
                    binding: binding_with_ring(second.as_str(), 2, AppearanceRingMode::Silent),
                    codec: Codec::Pcmu,
                },
            ],
        );
        assert_eq!(
            first_offers
                .iter()
                .map(|offer| (offer.call_id, offer.ring_mode))
                .collect::<Vec<_>>(),
            [
                (CallId(20), AppearanceRingMode::Normal),
                (CallId(30), AppearanceRingMode::Silent),
            ]
        );

        let second_offers = controller.offer_inbound_call(
            PbxCallId(9),
            [
                InboundAppearance {
                    call_id: CallId(21),
                    binding: binding_with_ring(first.as_str(), 1, AppearanceRingMode::Silent),
                    codec: Codec::Pcma,
                },
                InboundAppearance {
                    call_id: CallId(31),
                    binding: binding_with_ring(second.as_str(), 2, AppearanceRingMode::Normal),
                    codec: Codec::Pcmu,
                },
            ],
        );
        assert_eq!(
            second_offers
                .iter()
                .map(|offer| (offer.call_id, offer.ring_mode))
                .collect::<Vec<_>>(),
            [
                (CallId(21), AppearanceRingMode::Silent),
                (CallId(31), AppearanceRingMode::Normal),
            ]
        );
        assert_eq!(
            controller
                .appearance_for_call(CallId(20))
                .unwrap()
                .ring_mode,
            AppearanceRingMode::Normal
        );
        assert_eq!(
            controller
                .appearance_for_call(CallId(30))
                .unwrap()
                .ring_mode,
            AppearanceRingMode::Silent
        );

        assert!(controller.cancel_inbound_offer(CallId(21)));
        assert!(controller.appearance_for_call(CallId(31)).is_some());
        assert!(controller.appearance_for_call(CallId(20)).is_some());
        assert!(controller.appearance_for_call(CallId(30)).is_some());
        assert!(!controller.phone_answer(CallId(30)).is_empty());
        assert_eq!(
            controller.appearance_for_call(CallId(20)).unwrap().state,
            CallState::RemoteInUse
        );
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn delayed_auto_answer_has_one_generation_per_idle_shared_appearance() {
        let now = Instant::now();
        let mut controller = shared_inbound_controller();
        assert!(controller.set_auto_answer_request(
            PbxCallId(8),
            AutoAnswerRequest {
                mode: crate::call::auto_answer::AutoAnswerMode::TwoWay,
                unavailable_cause: None,
            },
        ));
        assert!(controller.has_auto_answer_request(PbxCallId(8)));
        assert_eq!(
            controller.schedule_auto_answers(
                PbxCallId(8),
                AutoAnswerPolicy {
                    delay: Duration::from_secs(2),
                    tone: Tone::Zip,
                },
                now,
            ),
            Ok(2)
        );
        assert!(!controller.has_auto_answer_request(PbxCallId(8)));
        assert_eq!(controller.pending_auto_answers.len(), 2);
        assert!(controller.pending_auto_answers.values().all(|pending| {
            pending.request
                == AutoAnswerRequest {
                    mode: crate::call::auto_answer::AutoAnswerMode::TwoWay,
                    unavailable_cause: None,
                }
        }));
        assert!(
            controller
                .expire_auto_answers(now + Duration::from_millis(1999))
                .is_empty()
        );

        let transitions = controller.expire_auto_answers(now + Duration::from_secs(2));
        assert_eq!(transitions.len(), 1);
        let transition = &transitions[0];
        assert_eq!(transition.target_call_id, CallId(2));
        assert!(matches!(
            transition.effects.last(),
            Some(DriverEffect::Handset(HandsetEffect::StartTone {
                call_id: CallId(2),
                tone: Tone::Zip,
                ..
            }))
        ));
        assert!(controller.pending_auto_answers.is_empty());
        assert_eq!(
            controller.call(CallId(2)).unwrap().state,
            CallState::Connected
        );
        assert_eq!(
            controller.call(CallId(3)).unwrap().state,
            CallState::RemoteInUse
        );
    }

    #[test]
    fn auto_answer_replacement_captures_new_policy_and_disconnect_cancels_it() {
        let now = Instant::now();
        let request = AutoAnswerRequest {
            mode: crate::call::auto_answer::AutoAnswerMode::TwoWay,
            unavailable_cause: None,
        };
        let mut controller = shared_inbound_controller();
        assert!(controller.set_auto_answer_request(PbxCallId(8), request));
        assert_eq!(
            controller.schedule_auto_answers(
                PbxCallId(8),
                AutoAnswerPolicy {
                    delay: Duration::from_secs(10),
                    tone: Tone::Zip,
                },
                now,
            ),
            Ok(2)
        );
        let old_generations = controller
            .pending_auto_answers
            .values()
            .map(|pending| pending.generation)
            .collect::<HashSet<_>>();

        assert!(controller.set_auto_answer_request(PbxCallId(8), request));
        assert_eq!(
            controller.schedule_auto_answers(
                PbxCallId(8),
                AutoAnswerPolicy {
                    delay: Duration::from_secs(2),
                    tone: Tone::ZipZip,
                },
                now + Duration::from_secs(1),
            ),
            Ok(2)
        );
        assert!(controller.pending_auto_answers.values().all(|pending| {
            !old_generations.contains(&pending.generation)
                && pending.deadline == now + Duration::from_secs(3)
                && pending.tone == Tone::ZipZip
        }));
        assert!(
            controller
                .expire_auto_answers(now + Duration::from_secs(2))
                .is_empty()
        );
        controller.disconnected(&DeviceId::new("SEP001122334455").unwrap());
        assert_eq!(controller.pending_auto_answers.len(), 1);
        controller.disconnected(&DeviceId::new("SEP112233445566").unwrap());
        assert!(controller.pending_auto_answers.is_empty());
        assert!(
            controller
                .expire_auto_answers(now + Duration::from_secs(30))
                .is_empty()
        );
    }

    #[test]
    fn manual_answer_remote_hangup_and_active_call_cancel_auto_answer() {
        let now = Instant::now();
        let request = AutoAnswerRequest {
            mode: crate::call::auto_answer::AutoAnswerMode::TwoWay,
            unavailable_cause: None,
        };
        let policy = AutoAnswerPolicy {
            delay: Duration::from_secs(1),
            tone: Tone::Zip,
        };

        let mut manual = shared_inbound_controller();
        assert!(manual.set_auto_answer_request(PbxCallId(8), request));
        assert_eq!(
            manual.schedule_auto_answers(PbxCallId(8), policy, now),
            Ok(2)
        );
        assert!(!manual.phone_answer(CallId(3)).is_empty());
        assert!(manual.pending_auto_answers.is_empty());
        assert!(
            manual
                .expire_auto_answers(now + Duration::from_secs(2))
                .is_empty()
        );

        let mut remote = shared_inbound_controller();
        assert!(remote.set_auto_answer_request(PbxCallId(8), request));
        assert_eq!(
            remote.schedule_auto_answers(PbxCallId(8), policy, now),
            Ok(2)
        );
        assert!(remote.pbx_hangup_with_effects(PbxCallId(8)).is_some());
        assert!(remote.pending_auto_answers.is_empty());
        assert!(
            remote
                .expire_auto_answers(now + Duration::from_secs(2))
                .is_empty()
        );

        let mut busy = connected_outbound_controller();
        busy.offer_inbound_call(
            PbxCallId(8),
            [InboundAppearance {
                call_id: CallId(2),
                binding: binding(),
                codec: Codec::Pcmu,
            }],
        );
        assert!(busy.set_auto_answer_request(PbxCallId(8), request));
        assert_eq!(busy.schedule_auto_answers(PbxCallId(8), policy, now), Ok(0));
        assert!(busy.pending_auto_answers.is_empty());
        assert_eq!(busy.call(CallId(1)).unwrap().state, CallState::Connected);
        assert_eq!(busy.call(CallId(2)).unwrap().state, CallState::Ringing);
    }

    #[test]
    fn auto_answer_rollback_never_resurrects_cancelled_peer_generations() {
        let now = Instant::now();
        let mut controller = shared_inbound_controller();
        assert!(controller.set_auto_answer_request(
            PbxCallId(8),
            AutoAnswerRequest {
                mode: crate::call::auto_answer::AutoAnswerMode::OneWay,
                unavailable_cause: Some(crate::call::auto_answer::AutoAnswerCause::Unavailable),
            },
        ));
        assert_eq!(
            controller.schedule_auto_answers(
                PbxCallId(8),
                AutoAnswerPolicy {
                    delay: Duration::ZERO,
                    tone: Tone::ZipZip,
                },
                now,
            ),
            Ok(2)
        );
        let transition = controller.expire_auto_answers(now).pop().unwrap();
        assert!(controller.pending_auto_answers.is_empty());
        let cleanup =
            controller.abort_call_transition(transition.id, &CallTransitionProgress::default());
        assert!(cleanup.is_empty());
        assert_eq!(
            controller.call(CallId(2)).unwrap().state,
            CallState::Ringing
        );
        assert_eq!(
            controller.call(CallId(3)).unwrap().state,
            CallState::Ringing
        );
        assert!(
            controller
                .expire_auto_answers(now + Duration::from_secs(30))
                .is_empty()
        );

        assert!(controller.set_auto_answer_request(
            PbxCallId(8),
            AutoAnswerRequest {
                mode: crate::call::auto_answer::AutoAnswerMode::OneWay,
                unavailable_cause: None,
            },
        ));
        assert_eq!(
            controller.schedule_auto_answers(
                PbxCallId(8),
                AutoAnswerPolicy {
                    delay: Duration::ZERO,
                    tone: Tone::ZipZip,
                },
                now,
            ),
            Ok(2)
        );
        let transition = controller.expire_auto_answers(now).pop().unwrap();
        let cleanup = controller.abort_call_transition(
            transition.id,
            &CallTransitionProgress::with_completed([
                CallTransitionMilestone::TargetBackendStarted,
                CallTransitionMilestone::TargetHandsetChanged,
            ]),
        );
        assert!(
            cleanup.iter().any(|effect| matches!(
                effect,
                DriverEffect::Backend(PbxEffect::Hangup {
                    call_id: PbxCallId(8)
                })
            )),
            "{cleanup:?}"
        );
        assert!(controller.pending_auto_answers.is_empty());
        assert!(controller.expire_auto_answers(now).is_empty());

        let mut exhausted = shared_inbound_controller();
        exhausted.next_auto_answer_generation = u64::MAX;
        assert!(exhausted.set_auto_answer_request(
            PbxCallId(8),
            AutoAnswerRequest {
                mode: crate::call::auto_answer::AutoAnswerMode::TwoWay,
                unavailable_cause: None,
            },
        ));
        assert_eq!(
            exhausted.schedule_auto_answers(
                PbxCallId(8),
                AutoAnswerPolicy {
                    delay: Duration::ZERO,
                    tone: Tone::Zip,
                },
                now,
            ),
            Err(AutoAnswerScheduleRejection::GenerationExhausted)
        );
        assert!(exhausted.pending_auto_answers.is_empty());
    }

    #[test]
    fn auto_answer_mode_controls_intercom_media_microphone_and_terminal_restore() {
        let now = Instant::now();
        for (mode, one_way) in [
            (crate::call::auto_answer::AutoAnswerMode::OneWay, true),
            (crate::call::auto_answer::AutoAnswerMode::TwoWay, false),
        ] {
            let mut controller = shared_inbound_controller();
            assert!(controller.set_auto_answer_request(
                PbxCallId(8),
                AutoAnswerRequest {
                    mode,
                    unavailable_cause: None,
                },
            ));
            assert_eq!(
                controller.schedule_auto_answers(
                    PbxCallId(8),
                    AutoAnswerPolicy {
                        delay: Duration::ZERO,
                        tone: Tone::ZipZip,
                    },
                    now,
                ),
                Ok(2)
            );
            let transition = controller.expire_auto_answers(now).pop().unwrap();
            assert_eq!(transition.auto_answer_mode, Some(mode));
            assert_eq!(
                transition.effects.iter().any(|effect| matches!(
                    effect,
                    DriverEffect::Handset(HandsetEffect::BeginOneWayMedia {
                        call_id: CallId(2),
                        ..
                    })
                )),
                one_way,
                "{transition:?}"
            );
            assert_eq!(
                transition.effects.iter().any(|effect| matches!(
                    effect,
                    DriverEffect::Handset(HandsetEffect::BeginMedia {
                        call_id: CallId(2),
                        ..
                    })
                )),
                !one_way,
                "{transition:?}"
            );
            assert!(matches!(
                transition.effects.iter().rev().find(|effect| matches!(
                    effect,
                    DriverEffect::Handset(HandsetEffect::StartTone { .. })
                )),
                Some(DriverEffect::Handset(HandsetEffect::StartTone {
                    call_id: CallId(2),
                    tone: Tone::ZipZip,
                    ..
                }))
            ));
            assert_eq!(
                transition.effects.iter().any(|effect| matches!(
                    effect,
                    DriverEffect::Handset(HandsetEffect::SetMicrophoneMode {
                        call_id: CallId(2),
                        enabled: false,
                        ..
                    })
                )),
                one_way,
                "{transition:?}"
            );

            for effect in &transition.effects {
                assert!(controller.record_call_transition_success(transition.id, effect));
            }
            assert!(controller.commit_call_transition(transition.id));
            assert_eq!(
                controller
                    .appearance_for_call(CallId(2))
                    .unwrap()
                    .auto_answer_mode,
                Some(mode)
            );
            let hangup = controller.pbx_hangup_with_effects(PbxCallId(8)).unwrap();
            assert_eq!(
                hangup.effects.iter().any(|effect| matches!(
                    effect,
                    DriverEffect::Handset(HandsetEffect::SetMicrophoneMode {
                        call_id: CallId(2),
                        enabled: true,
                        ..
                    })
                )),
                one_way,
                "{hangup:?}"
            );
        }
    }

    #[test]
    fn one_way_microphone_is_compensated_on_abort_and_late_completion() {
        let now = Instant::now();
        let prepare = || {
            let mut controller = shared_inbound_controller();
            assert!(controller.set_auto_answer_request(
                PbxCallId(8),
                AutoAnswerRequest {
                    mode: crate::call::auto_answer::AutoAnswerMode::OneWay,
                    unavailable_cause: None,
                },
            ));
            assert_eq!(
                controller.schedule_auto_answers(
                    PbxCallId(8),
                    AutoAnswerPolicy {
                        delay: Duration::ZERO,
                        tone: Tone::Zip,
                    },
                    now,
                ),
                Ok(2)
            );
            let transition = controller.expire_auto_answers(now).pop().unwrap();
            (controller, transition)
        };

        let (mut aborted, transition) = prepare();
        let cleanup = aborted.abort_call_transition(
            transition.id,
            &CallTransitionProgress::with_completed([
                CallTransitionMilestone::TargetBackendStarted,
                CallTransitionMilestone::TargetHandsetChanged,
                CallTransitionMilestone::TargetMicrophoneDisabled,
            ]),
        );
        assert!(cleanup.iter().any(|effect| matches!(
            effect,
            DriverEffect::Handset(HandsetEffect::SetMicrophoneMode {
                call_id: CallId(2),
                enabled: true,
                ..
            })
        )));

        let (mut late, transition) = prepare();
        assert!(
            late.abort_call_transition(transition.id, &CallTransitionProgress::default())
                .is_empty()
        );
        let completed = transition
            .effects
            .iter()
            .find(|effect| {
                matches!(
                    effect,
                    DriverEffect::Handset(HandsetEffect::SetMicrophoneMode { enabled: false, .. })
                )
            })
            .unwrap();
        let compensation =
            late.compensate_unrecorded_call_transition_effect(&transition, completed);
        assert!(matches!(
            compensation.effects.as_slice(),
            [DriverEffect::Handset(HandsetEffect::SetMicrophoneMode {
                call_id: CallId(2),
                enabled: true,
                ..
            })]
        ));

        let (mut shutdown, transition) = prepare();
        for effect in &transition.effects {
            assert!(shutdown.record_call_transition_success(transition.id, effect));
        }
        assert!(shutdown.commit_call_transition(transition.id));
        assert!(matches!(
            shutdown.drain_one_way_microphones().as_slice(),
            [DriverEffect::Handset(HandsetEffect::SetMicrophoneMode {
                call_id: CallId(2),
                enabled: true,
                ..
            })]
        ));
        assert!(shutdown.drain_one_way_microphones().is_empty());
    }

    #[test]
    fn delayed_two_way_auto_answer_cancels_when_the_device_becomes_active() {
        let now = Instant::now();
        let mut controller = Controller::new(Duration::from_secs(1));
        controller.registered(registration());
        controller.begin_asterisk_call(CallId(2), PbxCallId(8), &binding(), Codec::Pcma);
        assert!(controller.set_auto_answer_request(
            PbxCallId(8),
            AutoAnswerRequest {
                mode: crate::call::auto_answer::AutoAnswerMode::TwoWay,
                unavailable_cause: None,
            },
        ));
        assert_eq!(
            controller.schedule_auto_answers(
                PbxCallId(8),
                AutoAnswerPolicy {
                    delay: Duration::from_secs(2),
                    tone: Tone::Zip,
                },
                now,
            ),
            Ok(1)
        );
        assert!(
            !controller
                .begin_phone_call(CallId(9), binding(), Codec::Pcmu, now)
                .is_empty()
        );
        assert!(
            controller
                .expire_auto_answers(now + Duration::from_secs(2))
                .is_empty()
        );
        assert_eq!(
            controller.call(CallId(2)).unwrap().state,
            CallState::Ringing
        );
        assert_eq!(
            controller.call(CallId(9)).unwrap().state,
            CallState::Collecting
        );
        assert_eq!(
            controller
                .registered_device(&binding().device_id)
                .unwrap()
                .active_call(),
            Some(CallId(9))
        );
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn local_one_way_hangup_restores_microphone_before_removing_call() {
        let now = Instant::now();
        let mut controller = shared_inbound_controller();
        assert!(controller.set_auto_answer_request(
            PbxCallId(8),
            AutoAnswerRequest {
                mode: crate::call::auto_answer::AutoAnswerMode::OneWay,
                unavailable_cause: None,
            },
        ));
        assert_eq!(
            controller.schedule_auto_answers(
                PbxCallId(8),
                AutoAnswerPolicy {
                    delay: Duration::ZERO,
                    tone: Tone::Zip,
                },
                now,
            ),
            Ok(2)
        );
        let transition = controller.expire_auto_answers(now).pop().unwrap();
        for effect in &transition.effects {
            assert!(controller.record_call_transition_success(transition.id, effect));
        }
        assert!(controller.commit_call_transition(transition.id));
        let effects = controller.hangup(CallId(2));
        assert!(effects.iter().any(|effect| matches!(
            effect,
            DriverEffect::Handset(HandsetEffect::SetMicrophoneMode {
                call_id: CallId(2),
                enabled: true,
                ..
            })
        )));
        assert!(controller.call(CallId(2)).is_none());
        assert!(controller.call(CallId(3)).is_none());
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn answer_hangup_transfer_and_timeout_threads_have_one_serialized_winner() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        for _ in 0..16 {
            let controller = Arc::new(Mutex::new(shared_inbound_controller()));
            let gate = Arc::new(Barrier::new(2));
            let answer = {
                let controller = Arc::clone(&controller);
                let gate = Arc::clone(&gate);
                thread::spawn(move || {
                    gate.wait();
                    controller_step(&controller, |controller| controller.phone_answer(CallId(2)))
                })
            };
            let hangup = {
                let controller = Arc::clone(&controller);
                let gate = Arc::clone(&gate);
                thread::spawn(move || {
                    gate.wait();
                    controller_step(&controller, |controller| {
                        controller.pbx_hangup_with_effects(PbxCallId(8))
                    })
                })
            };
            let answer_effects = answer.join().unwrap();
            let hangup_outcome = hangup.join().unwrap();
            assert!(hangup_outcome.is_some());
            assert!(
                answer_effects.is_empty()
                    || answer_effects.iter().any(|effect| matches!(
                        effect,
                        DriverEffect::Handset(HandsetEffect::BeginAnswerMedia {
                            call_id: CallId(2),
                            ..
                        })
                    ))
            );
            let controller = controller.lock().unwrap();
            assert!(controller.pbx_call(PbxCallId(8)).is_none());
            assert!(controller.invariant_error().is_none());
        }

        let now = Instant::now();
        for _ in 0..16 {
            let mut prepared = shared_inbound_controller();
            assert!(prepared.set_auto_answer_request(
                PbxCallId(8),
                AutoAnswerRequest {
                    mode: crate::call::auto_answer::AutoAnswerMode::TwoWay,
                    unavailable_cause: None,
                },
            ));
            assert_eq!(
                prepared.schedule_auto_answers(
                    PbxCallId(8),
                    AutoAnswerPolicy {
                        delay: Duration::ZERO,
                        tone: Tone::Zip,
                    },
                    now,
                ),
                Ok(2)
            );
            let controller = Arc::new(Mutex::new(prepared));
            let gate = Arc::new(Barrier::new(2));
            let timer = {
                let controller = Arc::clone(&controller);
                let gate = Arc::clone(&gate);
                thread::spawn(move || {
                    gate.wait();
                    controller_step(&controller, |controller| {
                        controller.expire_auto_answers(now)
                    })
                })
            };
            let manual = {
                let controller = Arc::clone(&controller);
                let gate = Arc::clone(&gate);
                thread::spawn(move || {
                    gate.wait();
                    controller_step(&controller, |controller| controller.phone_answer(CallId(3)))
                })
            };
            let transitions = timer.join().unwrap();
            let manual_effects = manual.join().unwrap();
            let mut controller = controller.lock().unwrap();
            if let Some(transition) = transitions.first() {
                assert!(manual_effects.is_empty());
                assert!(controller.commit_call_transition(transition.id));
            } else {
                assert!(!manual_effects.is_empty());
            }
            assert!(controller.pending_auto_answers.is_empty());
            assert_eq!(
                controller
                    .appearances_for_pbx(PbxCallId(8))
                    .filter(|appearance| appearance.state == CallState::Connected)
                    .count(),
                1
            );
            assert!(controller.invariant_error().is_none());
        }

        for _ in 0..16 {
            let mut prepared = connected_outbound_controller();
            let device_id = binding().device_id;
            let (transaction_id, _) = begin_test_transfer(&mut prepared, false);
            prepared.enbloc(CallId(2), "2200".into());
            prepared.pbx_progress(PbxCallId(2), false);
            let controller = Arc::new(Mutex::new(prepared));
            let gate = Arc::new(Barrier::new(2));
            let completion = {
                let controller = Arc::clone(&controller);
                let gate = Arc::clone(&gate);
                let device_id = device_id.clone();
                thread::spawn(move || {
                    gate.wait();
                    controller_step(&controller, |controller| {
                        controller.complete_transfer(
                            &device_id,
                            CallId(2),
                            TransferTrigger::TransferKey,
                        )
                    })
                })
            };
            let hangup = {
                let controller = Arc::clone(&controller);
                let gate = Arc::clone(&gate);
                thread::spawn(move || {
                    gate.wait();
                    controller_step(&controller, |controller| {
                        controller.pbx_hangup_with_effects(PbxCallId(2))
                    })
                })
            };
            let completion = completion.join().unwrap();
            assert!(hangup.join().unwrap().is_some());
            let mut controller = controller.lock().unwrap();
            if completion.is_ok() {
                controller
                    .abort_transfer(
                        &device_id,
                        transaction_id,
                        TransferCancellationReason::BackendFailure,
                    )
                    .unwrap();
            }
            assert_eq!(
                controller.call(CallId(1)).unwrap().state,
                CallState::Connected
            );
            assert!(controller.call(CallId(2)).is_none());
            assert!(controller.transfer_transaction(CallId(1)).is_none());
            assert!(controller.invariant_error().is_none());
        }
    }

    #[test]
    fn call_waiting_tone_targets_active_call_repeats_and_cancels_on_answer() {
        let now = Instant::now();
        let mut controller = connected_outbound_controller();
        let offers = controller.offer_inbound_call(
            PbxCallId(8),
            [InboundAppearance {
                call_id: CallId(2),
                binding: binding(),
                codec: Codec::Pcma,
            }],
        );
        assert_eq!(offers[0].state, HandsetCallState::CallWaiting);
        assert_eq!(
            controller.start_call_waiting_tone(
                CallId(2),
                Some(Tone::PriorityCallWaiting),
                Duration::from_secs(5),
                now,
            ),
            [DriverEffect::Handset(HandsetEffect::StartTone {
                device_id: binding().device_id,
                call_id: CallId(1),
                tone: Tone::PriorityCallWaiting,
            })]
        );
        assert!(
            controller
                .expire_call_waiting_tones(now + Duration::from_secs(4))
                .is_empty()
        );
        assert_eq!(
            controller.expire_call_waiting_tones(now + Duration::from_secs(5)),
            [DriverEffect::Handset(HandsetEffect::StartTone {
                device_id: binding().device_id,
                call_id: CallId(1),
                tone: Tone::PriorityCallWaiting,
            })]
        );

        let answer = controller.phone_answer(CallId(2));
        assert!(matches!(
            answer.first(),
            Some(DriverEffect::Backend(PbxEffect::Hold {
                call_id: PbxCallId(1)
            }))
        ));
        assert!(
            controller
                .expire_call_waiting_tones(now + Duration::from_secs(10))
                .is_empty()
        );
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn disabled_or_nonwaiting_tone_policy_never_schedules_an_effect() {
        let now = Instant::now();
        let mut idle = Controller::new(Duration::from_secs(1));
        idle.registered(registration());
        idle.offer_inbound_call(
            PbxCallId(8),
            [InboundAppearance {
                call_id: CallId(2),
                binding: binding(),
                codec: Codec::Pcma,
            }],
        );
        assert!(
            idle.start_call_waiting_tone(
                CallId(2),
                Some(Tone::CallWaiting),
                Duration::from_secs(5),
                now,
            )
            .is_empty()
        );

        let mut waiting = connected_outbound_controller();
        waiting.offer_inbound_call(
            PbxCallId(8),
            [InboundAppearance {
                call_id: CallId(2),
                binding: binding(),
                codec: Codec::Pcma,
            }],
        );
        assert!(
            waiting
                .start_call_waiting_tone(CallId(2), None, Duration::from_secs(5), now)
                .is_empty()
        );
        assert!(
            waiting
                .expire_call_waiting_tones(now + Duration::from_secs(5))
                .is_empty()
        );

        let mut silent = connected_outbound_controller();
        silent.set_dnd(&binding().device_id, DndMode::Silent);
        silent.offer_inbound_call(
            PbxCallId(8),
            [InboundAppearance {
                call_id: CallId(2),
                binding: binding(),
                codec: Codec::Pcma,
            }],
        );
        assert!(
            silent
                .start_call_waiting_tone(
                    CallId(2),
                    Some(Tone::CallWaiting),
                    Duration::from_secs(5),
                    now,
                )
                .is_empty()
        );
        assert!(silent.cancel_inbound_offer(CallId(2)));
        silent.set_dnd(&binding().device_id, DndMode::Off);
        silent.offer_inbound_call(
            PbxCallId(9),
            [InboundAppearance {
                call_id: CallId(3),
                binding: binding(),
                codec: Codec::Pcma,
            }],
        );
        assert_eq!(
            silent
                .start_call_waiting_tone(
                    CallId(3),
                    Some(Tone::CallWaiting),
                    Duration::from_secs(5),
                    now,
                )
                .len(),
            1
        );
        assert!(silent.invariant_error().is_none());
    }

    #[test]
    fn call_waiting_timer_cleans_up_cancel_hangup_and_active_leg_changes() {
        let now = Instant::now();
        for cleanup in 0..3 {
            let mut controller = connected_outbound_controller();
            controller.offer_inbound_call(
                PbxCallId(8),
                [InboundAppearance {
                    call_id: CallId(2),
                    binding: binding(),
                    codec: Codec::Pcma,
                }],
            );
            assert_eq!(
                controller
                    .start_call_waiting_tone(
                        CallId(2),
                        Some(Tone::CallWaiting),
                        Duration::from_secs(3),
                        now,
                    )
                    .len(),
                1
            );
            match cleanup {
                0 => assert!(controller.cancel_inbound_offer(CallId(2))),
                1 => assert!(controller.pbx_hangup_with_effects(PbxCallId(8)).is_some()),
                2 => assert!(!controller.hold(CallId(1)).is_empty()),
                _ => unreachable!(),
            }
            assert!(
                controller
                    .expire_call_waiting_tones(now + Duration::from_secs(3))
                    .is_empty()
            );
            assert!(controller.invariant_error().is_none());
        }
    }

    #[test]
    fn call_waiting_policy_reload_is_captured_per_waiting_call() {
        let now = Instant::now();
        let mut controller = connected_outbound_controller();
        for (pbx_id, call_id) in [(PbxCallId(8), CallId(2)), (PbxCallId(9), CallId(3))] {
            controller.offer_inbound_call(
                pbx_id,
                [InboundAppearance {
                    call_id,
                    binding: binding(),
                    codec: Codec::Pcma,
                }],
            );
        }
        controller.start_call_waiting_tone(
            CallId(2),
            Some(Tone::CallWaiting),
            Duration::from_secs(5),
            now,
        );
        controller.start_call_waiting_tone(
            CallId(3),
            Some(Tone::PriorityCallWaiting),
            Duration::from_secs(2),
            now,
        );

        assert_eq!(
            controller.expire_call_waiting_tones(now + Duration::from_secs(2)),
            [DriverEffect::Handset(HandsetEffect::StartTone {
                device_id: binding().device_id,
                call_id: CallId(1),
                tone: Tone::PriorityCallWaiting,
            })]
        );
        assert!(controller.cancel_inbound_offer(CallId(3)));
        assert_eq!(
            controller.expire_call_waiting_tones(now + Duration::from_secs(5)),
            [DriverEffect::Handset(HandsetEffect::StartTone {
                device_id: binding().device_id,
                call_id: CallId(1),
                tone: Tone::CallWaiting,
            })]
        );
    }

    #[test]
    fn incoming_limit_counts_logical_calls_and_reopens_after_cleanup() {
        let mut controller = Controller::new(Duration::from_secs(1));
        for device in ["SEP001122334455", "SEP112233445566"] {
            controller.registered(registration_for(device));
        }
        controller.set_line_incoming_limits([("1001".into(), 1)]);
        let shared = [
            InboundAppearance {
                call_id: CallId(2),
                binding: binding_for("SEP001122334455", 1),
                codec: Codec::Pcma,
            },
            InboundAppearance {
                call_id: CallId(3),
                binding: binding_for("SEP112233445566", 2),
                codec: Codec::Pcmu,
            },
        ];
        assert_eq!(controller.offer_inbound_call(PbxCallId(8), shared).len(), 2);
        assert_eq!(
            controller.offer_inbound_call_with_policy(
                PbxCallId(9),
                [InboundAppearance {
                    call_id: CallId(4),
                    binding: binding_for("SEP001122334455", 1),
                    codec: Codec::Pcma,
                }],
            ),
            InboundCallDisposition::Unavailable(InboundUnavailableReason::IncomingLimit)
        );
        controller.pbx_hangup_with_effects(PbxCallId(8));
        assert!(matches!(
            controller.offer_inbound_call_with_policy(
                PbxCallId(9),
                [InboundAppearance {
                    call_id: CallId(4),
                    binding: binding_for("SEP001122334455", 1),
                    codec: Codec::Pcma,
                }],
            ),
            InboundCallDisposition::Offer(offers) if offers.len() == 1
        ));
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn default_incoming_limit_serializes_the_sixth_and_seventh_offer_boundary() {
        let mut controller = Controller::new(Duration::from_secs(1));
        controller.registered(registration());
        for offset in 0..6 {
            assert!(matches!(
                controller.offer_inbound_call_with_policy(
                    (20 + offset).into(),
                    [InboundAppearance {
                        call_id: CallId(20 + offset),
                        binding: binding(),
                        codec: Codec::Pcma,
                    }],
                ),
                InboundCallDisposition::Offer(offers) if offers.len() == 1
            ));
        }
        assert_eq!(
            controller.offer_inbound_call_with_policy(
                PbxCallId(26),
                [InboundAppearance {
                    call_id: CallId(26),
                    binding: binding(),
                    codec: Codec::Pcma,
                }],
            ),
            InboundCallDisposition::Unavailable(InboundUnavailableReason::IncomingLimit)
        );
        controller.pbx_hangup_with_effects(PbxCallId(22));
        assert!(matches!(
            controller.offer_inbound_call_with_policy(
                PbxCallId(26),
                [InboundAppearance {
                    call_id: CallId(26),
                    binding: binding(),
                    codec: Codec::Pcma,
                }],
            ),
            InboundCallDisposition::Offer(_)
        ));
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn forwarding_is_resolved_before_zero_incoming_limit_rejects_ringing() {
        let mut controller = Controller::new(Duration::from_secs(1));
        let device = binding().device_id;
        controller.registered(registration());
        controller.set_line_incoming_limits([("1001".into(), 0)]);
        controller.set_forwarding(
            &device,
            ForwardingState {
                all: Some(forwarding("2200")),
                ..ForwardingState::default()
            },
        );

        assert!(matches!(
            controller.offer_inbound_call_with_policy(
                PbxCallId(8),
                [InboundAppearance {
                    call_id: CallId(2),
                    binding: binding(),
                    codec: Codec::Pcma,
                }],
            ),
            InboundCallDisposition::Forward { destination, .. } if destination.as_str() == "2200"
        ));
        assert!(controller.pbx_call(PbxCallId(8)).is_none());

        controller.set_forwarding(&device, ForwardingState::default());
        controller.set_dnd(&device, DndMode::Reject);
        assert_eq!(
            controller.offer_inbound_call_with_policy(
                PbxCallId(9),
                [InboundAppearance {
                    call_id: CallId(3),
                    binding: binding(),
                    codec: Codec::Pcma,
                }],
            ),
            InboundCallDisposition::Unavailable(InboundUnavailableReason::IncomingLimit)
        );
        assert!(controller.pbx_call(PbxCallId(9)).is_none());
    }

    #[test]
    fn per_device_dnd_filters_or_silences_only_its_own_shared_appearance() {
        let first = DeviceId::new("SEP001122334455").unwrap();
        let second = DeviceId::new("SEP112233445566").unwrap();
        let mut controller = Controller::new(Duration::from_secs(1));
        controller.registered(registration_for(first.as_str()));
        controller.registered(registration_for(second.as_str()));
        controller.set_dnd(&first, DndMode::Reject);
        controller.set_dnd(&second, DndMode::Silent);

        let disposition = controller.offer_inbound_call_with_policy(
            PbxCallId(8),
            [
                InboundAppearance {
                    call_id: CallId(2),
                    binding: binding_for(first.as_str(), 1),
                    codec: Codec::Pcma,
                },
                InboundAppearance {
                    call_id: CallId(3),
                    binding: binding_for(second.as_str(), 2),
                    codec: Codec::Pcmu,
                },
            ],
        );

        let InboundCallDisposition::Offer(offers) = disposition else {
            panic!("the silent appearance must remain eligible");
        };
        assert_eq!(offers.len(), 1);
        assert_eq!(offers[0].device_id, second);
        assert_eq!(offers[0].ring_mode, AppearanceRingMode::Silent);
        assert!(controller.appearance_for_call(CallId(2)).is_none());
        assert_eq!(
            controller.appearance_for_call(CallId(3)).unwrap().ring_mode,
            AppearanceRingMode::Silent
        );
        assert!(
            controller
                .phone_answer(CallId(3))
                .iter()
                .all(|effect| !matches!(effect, DriverEffect::Backend(PbxEffect::Answer { .. })))
        );
        assert!(
            controller
                .media_opened(CallId(3), test_media_endpoint(Codec::Pcmu))
                .iter()
                .any(|effect| matches!(
                    effect,
                    DriverEffect::Backend(PbxEffect::Answer {
                        call_id: PbxCallId(8)
                    })
                ))
        );
        assert_eq!(
            controller.appearance_for_call(CallId(3)).unwrap().state,
            CallState::Connected
        );
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn changing_dnd_preserves_active_and_selected_call_state() {
        let device = binding().device_id;
        let mut controller = connected_outbound_controller();
        controller.set_call_selected(&device, CallId(1), true);

        for mode in [DndMode::Silent, DndMode::Reject, DndMode::Off] {
            controller.set_dnd(&device, mode);
            let registered = controller.registered_device(&device).unwrap();
            assert_eq!(registered.active_call(), Some(CallId(1)));
            assert!(registered.is_call_selected(CallId(1)));
            assert_eq!(
                controller.call(CallId(1)).unwrap().state,
                CallState::Connected
            );
            assert_eq!(controller.feature_state(&device).unwrap().dnd, mode);
            assert!(controller.invariant_error().is_none());
        }
    }

    #[test]
    fn rejected_offer_leaves_no_state_and_can_be_retried_after_dnd_is_disabled() {
        let device = binding().device_id;
        let mut controller = Controller::new(Duration::from_secs(1));
        controller.registered(registration());
        controller.set_dnd(&device, DndMode::Reject);
        let candidate = || InboundAppearance {
            call_id: CallId(2),
            binding: binding(),
            codec: Codec::Pcma,
        };

        assert_eq!(
            controller.offer_inbound_call_with_policy(PbxCallId(8), [candidate()]),
            InboundCallDisposition::Unavailable(InboundUnavailableReason::DoNotDisturb)
        );
        assert!(controller.pbx_call(PbxCallId(8)).is_none());
        assert!(controller.appearance_for_call(CallId(2)).is_none());

        controller.set_dnd(&device, DndMode::Off);
        assert!(matches!(
            controller.offer_inbound_call_with_policy(PbxCallId(8), [candidate()]),
            InboundCallDisposition::Offer(offers)
                if offers.len() == 1 && offers[0].ring_mode == AppearanceRingMode::Normal
        ));
        assert_eq!(
            controller.offer_inbound_call_with_policy(PbxCallId(8), [candidate()]),
            InboundCallDisposition::Unavailable(InboundUnavailableReason::Conflict)
        );
        assert_eq!(
            controller
                .pbx_call(PbxCallId(8))
                .unwrap()
                .appearance_ids
                .len(),
            1
        );
        assert!(controller.pbx_hangup_with_effects(PbxCallId(8)).is_some());

        controller.set_dnd(&device, DndMode::Silent);
        assert!(matches!(
            controller.offer_inbound_call_with_policy(
                PbxCallId(9),
                [InboundAppearance {
                    call_id: CallId(3),
                    binding: binding(),
                    codec: Codec::Pcma,
                }],
            ),
            InboundCallDisposition::Offer(offers)
                if offers.len() == 1 && offers[0].ring_mode == AppearanceRingMode::Silent
        ));
        assert!(controller.pbx_hangup_with_effects(PbxCallId(9)).is_some());

        controller.set_dnd(&device, DndMode::Off);
        assert!(matches!(
            controller.offer_inbound_call_with_policy(
                PbxCallId(10),
                [InboundAppearance {
                    call_id: CallId(4),
                    binding: binding(),
                    codec: Codec::Pcma,
                }],
            ),
            InboundCallDisposition::Offer(offers)
                if offers.len() == 1 && offers[0].ring_mode == AppearanceRingMode::Normal
        ));
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn mixed_structural_exclusions_do_not_report_a_dnd_only_rejection() {
        let first = DeviceId::new("SEP001122334455").unwrap();
        let second = DeviceId::new("SEP112233445566").unwrap();
        let mut controller = Controller::new(Duration::from_secs(1));
        controller.registered(registration_for(first.as_str()));
        controller.set_dnd(&first, DndMode::Reject);

        assert_eq!(
            controller.offer_inbound_call_with_policy(
                PbxCallId(8),
                [
                    InboundAppearance {
                        call_id: CallId(2),
                        binding: binding_for(first.as_str(), 1),
                        codec: Codec::Pcma,
                    },
                    InboundAppearance {
                        call_id: CallId(3),
                        binding: binding_for(second.as_str(), 2),
                        codec: Codec::Pcmu,
                    },
                ],
            ),
            InboundCallDisposition::Unavailable(InboundUnavailableReason::NoEligibleAppearance)
        );

        controller.registered(registration_for(second.as_str()));
        let mut disabled = binding_for(second.as_str(), 2);
        disabled.appearance.ring_mode = AppearanceRingMode::Disabled;
        assert_eq!(
            controller.offer_inbound_call_with_policy(
                PbxCallId(8),
                [
                    InboundAppearance {
                        call_id: CallId(4),
                        binding: binding_for(first.as_str(), 1),
                        codec: Codec::Pcma,
                    },
                    InboundAppearance {
                        call_id: CallId(5),
                        binding: disabled,
                        codec: Codec::Pcmu,
                    },
                ],
            ),
            InboundCallDisposition::Unavailable(InboundUnavailableReason::NoEligibleAppearance)
        );

        let duplicate = InboundAppearance {
            call_id: CallId(6),
            binding: binding_for(first.as_str(), 1),
            codec: Codec::Pcma,
        };
        assert_eq!(
            controller
                .offer_inbound_call_with_policy(PbxCallId(8), [duplicate.clone(), duplicate],),
            InboundCallDisposition::Unavailable(InboundUnavailableReason::NoEligibleAppearance)
        );
        assert!(controller.pbx_call(PbxCallId(8)).is_none());
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn simultaneous_reject_and_duplicate_offer_paths_leave_one_consistent_result() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        let device = binding().device_id;
        let mut rejected = Controller::new(Duration::from_secs(1));
        rejected.registered(registration());
        rejected.set_dnd(&device, DndMode::Reject);
        let rejected = Arc::new(Mutex::new(rejected));
        let gate = Arc::new(Barrier::new(2));
        let results = [(PbxCallId(8), CallId(2)), (PbxCallId(9), CallId(3))]
            .into_iter()
            .map(|(pbx_id, call_id)| {
                let controller = Arc::clone(&rejected);
                let gate = Arc::clone(&gate);
                thread::spawn(move || {
                    gate.wait();
                    controller_step(&controller, |controller| {
                        controller.offer_inbound_call_with_policy(
                            pbx_id,
                            [InboundAppearance {
                                call_id,
                                binding: binding(),
                                codec: Codec::Pcma,
                            }],
                        )
                    })
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();
        assert!(results.iter().all(|result| {
            *result == InboundCallDisposition::Unavailable(InboundUnavailableReason::DoNotDisturb)
        }));
        let rejected = rejected.lock().unwrap();
        assert!(rejected.pbx_call(PbxCallId(8)).is_none());
        assert!(rejected.pbx_call(PbxCallId(9)).is_none());
        assert!(rejected.invariant_error().is_none());
        drop(rejected);

        let mut available = Controller::new(Duration::from_secs(1));
        available.registered(registration());
        let available = Arc::new(Mutex::new(available));
        let gate = Arc::new(Barrier::new(2));
        let results = [CallId(2), CallId(3)]
            .into_iter()
            .map(|call_id| {
                let controller = Arc::clone(&available);
                let gate = Arc::clone(&gate);
                thread::spawn(move || {
                    gate.wait();
                    controller_step(&controller, |controller| {
                        controller.offer_inbound_call_with_policy(
                            PbxCallId(8),
                            [InboundAppearance {
                                call_id,
                                binding: binding(),
                                codec: Codec::Pcma,
                            }],
                        )
                    })
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, InboundCallDisposition::Offer(_)))
                .count(),
            1
        );
        assert_eq!(
            results
                .iter()
                .filter(|result| {
                    **result
                        == InboundCallDisposition::Unavailable(InboundUnavailableReason::Conflict)
                })
                .count(),
            1
        );
        let available = available.lock().unwrap();
        assert_eq!(
            available
                .pbx_call(PbxCallId(8))
                .unwrap()
                .appearance_ids
                .len(),
            1
        );
        assert!(available.invariant_error().is_none());
    }

    #[test]
    fn shared_forwarding_rings_remaining_devices_and_requires_one_destination() {
        let first = DeviceId::new("SEP001122334455").unwrap();
        let second = DeviceId::new("SEP112233445566").unwrap();
        let candidates = || {
            [
                InboundAppearance {
                    call_id: CallId(2),
                    binding: binding_for(first.as_str(), 1),
                    codec: Codec::Pcma,
                },
                InboundAppearance {
                    call_id: CallId(3),
                    binding: binding_for(second.as_str(), 2),
                    codec: Codec::Pcmu,
                },
            ]
        };

        let mut controller = Controller::new(Duration::from_secs(1));
        controller.registered(registration_for(first.as_str()));
        controller.registered(registration_for(second.as_str()));
        controller.set_forwarding(
            &first,
            ForwardingState {
                all: Some(forwarding("9000")),
                ..ForwardingState::default()
            },
        );
        let InboundCallDisposition::Offer(offers) =
            controller.offer_inbound_call_with_policy(PbxCallId(8), candidates())
        else {
            panic!("one unforwarded shared appearance must still ring");
        };
        assert_eq!(offers.len(), 1);
        assert_eq!(offers[0].device_id, second);

        let mut forwarded = Controller::new(Duration::from_secs(1));
        forwarded.registered(registration_for(first.as_str()));
        forwarded.registered(registration_for(second.as_str()));
        for device in [&first, &second] {
            forwarded.set_forwarding(
                device,
                ForwardingState {
                    all: Some(forwarding("9000")),
                    ..ForwardingState::default()
                },
            );
        }
        assert!(matches!(
            forwarded.offer_inbound_call_with_policy(PbxCallId(9), candidates()),
            InboundCallDisposition::Forward { destination, .. } if destination.as_str() == "9000"
        ));
        assert!(forwarded.pbx_call(PbxCallId(9)).is_none());

        forwarded.set_forwarding(
            &second,
            ForwardingState {
                all: Some(forwarding("9001")),
                ..ForwardingState::default()
            },
        );
        assert_eq!(
            forwarded.offer_inbound_call_with_policy(PbxCallId(10), candidates()),
            InboundCallDisposition::Unavailable(InboundUnavailableReason::ForwardingConflict)
        );
    }

    #[test]
    fn forward_busy_is_evaluated_per_device_before_shared_fanout() {
        let first = DeviceId::new("SEP001122334455").unwrap();
        let second = DeviceId::new("SEP112233445566").unwrap();
        let mut controller = Controller::new(Duration::from_secs(1));
        controller.registered(registration_for(first.as_str()));
        controller.registered(registration_for(second.as_str()));
        controller.begin_phone_call(
            CallId(50),
            binding_for(first.as_str(), 1),
            Codec::Pcmu,
            Instant::now(),
        );
        controller.set_forwarding(
            &first,
            ForwardingState {
                busy: Some(forwarding("9000")),
                ..ForwardingState::default()
            },
        );
        controller.set_dnd(&second, DndMode::Reject);

        assert!(matches!(
            controller.offer_inbound_call_with_policy(
                PbxCallId(8),
                [
                    InboundAppearance {
                        call_id: CallId(2),
                        binding: binding_for(first.as_str(), 2),
                        codec: Codec::Pcma,
                    },
                    InboundAppearance {
                        call_id: CallId(3),
                        binding: binding_for(second.as_str(), 1),
                        codec: Codec::Pcmu,
                    },
                ],
            ),
            InboundCallDisposition::Forward { destination, .. } if destination.as_str() == "9000"
        ));

        let mut free_peer = Controller::new(Duration::from_secs(1));
        free_peer.registered(registration_for(first.as_str()));
        free_peer.registered(registration_for(second.as_str()));
        free_peer.begin_phone_call(
            CallId(50),
            binding_for(first.as_str(), 1),
            Codec::Pcmu,
            Instant::now(),
        );
        free_peer.set_forwarding(
            &first,
            ForwardingState {
                busy: Some(forwarding("9000")),
                ..ForwardingState::default()
            },
        );
        assert!(matches!(
            free_peer.offer_inbound_call_with_policy(
                PbxCallId(8),
                [
                    InboundAppearance {
                        call_id: CallId(2),
                        binding: binding_for(first.as_str(), 2),
                        codec: Codec::Pcma,
                    },
                    InboundAppearance {
                        call_id: CallId(3),
                        binding: binding_for(second.as_str(), 1),
                        codec: Codec::Pcmu,
                    },
                ],
            ),
            InboundCallDisposition::Offer(offers)
                if offers.len() == 1 && offers[0].device_id == second
        ));

        let mut disagreement = Controller::new(Duration::from_secs(1));
        disagreement.registered(registration_for(first.as_str()));
        disagreement.registered(registration_for(second.as_str()));
        for (call_id, device) in [(CallId(50), &first), (CallId(51), &second)] {
            disagreement.begin_phone_call(
                call_id,
                binding_for(device.as_str(), 1),
                Codec::Pcmu,
                Instant::now(),
            );
        }
        disagreement.set_forwarding(
            &first,
            ForwardingState {
                busy: Some(forwarding("9000")),
                ..ForwardingState::default()
            },
        );
        disagreement.set_forwarding(
            &second,
            ForwardingState {
                busy: Some(forwarding("9001")),
                ..ForwardingState::default()
            },
        );
        assert_eq!(
            disagreement.offer_inbound_call_with_policy(
                PbxCallId(8),
                [
                    InboundAppearance {
                        call_id: CallId(2),
                        binding: binding_for(first.as_str(), 2),
                        codec: Codec::Pcma,
                    },
                    InboundAppearance {
                        call_id: CallId(3),
                        binding: binding_for(second.as_str(), 2),
                        codec: Codec::Pcmu,
                    },
                ],
            ),
            InboundCallDisposition::Unavailable(InboundUnavailableReason::ForwardingConflict)
        );
    }

    #[test]
    fn privacy_from_call_appearance_and_device_blocks_remote_shared_control() {
        let first = DeviceId::new("SEP001122334455").unwrap();
        let second = DeviceId::new("SEP112233445566").unwrap();
        let mut first_binding = binding_for(first.as_str(), 1);
        first_binding.appearance.privacy = true;
        let mut controller = Controller::new(Duration::from_secs(1));
        controller.registered(registration_for(first.as_str()));
        controller.registered(registration_for(second.as_str()));
        controller.offer_inbound_call(
            PbxCallId(8),
            [
                InboundAppearance {
                    call_id: CallId(2),
                    binding: first_binding,
                    codec: Codec::Pcma,
                },
                InboundAppearance {
                    call_id: CallId(3),
                    binding: binding_for(second.as_str(), 2),
                    codec: Codec::Pcmu,
                },
            ],
        );
        controller.phone_answer(CallId(2));
        controller.media_opened(CallId(2), test_media_endpoint(Codec::Pcma));

        assert_eq!(controller.call_privacy(CallId(2)), Some(true));
        assert!(controller.steal(CallId(3)).is_empty());
        controller.hold(CallId(2));
        assert!(controller.resume(CallId(3)).is_empty());
        assert!(!controller.set_call_privacy(CallId(3), false));
        assert!(controller.set_call_privacy(CallId(2), false));
        assert!(!controller.resume(CallId(3)).is_empty());

        let mut outbound = Controller::new(Duration::from_secs(1));
        outbound.set_privacy(&first, true);
        outbound.begin_phone_call(
            CallId(20),
            binding_for(first.as_str(), 1),
            Codec::Pcmu,
            Instant::now(),
        );
        assert_eq!(outbound.call_privacy(CallId(20)), Some(true));
    }

    #[test]
    fn no_answer_forward_closes_every_ringing_appearance_without_hanging_up_pbx() {
        let mut controller = shared_inbound_controller();
        let effects = controller.forward_ringing_call(PbxCallId(8));

        assert_eq!(effects.len(), 2);
        assert!(effects.iter().all(|effect| matches!(
            effect,
            DriverEffect::Handset(HandsetEffect::SetCallState {
                state: HandsetCallState::OnHook,
                stop_media: false,
                ..
            })
        )));
        assert!(
            effects
                .iter()
                .all(|effect| !matches!(effect, DriverEffect::Backend(_)))
        );
        assert!(controller.pbx_call(PbxCallId(8)).is_none());

        let mut answered = shared_inbound_controller();
        answered.phone_answer(CallId(2));
        assert!(answered.forward_ringing_call(PbxCallId(8)).is_empty());
        assert!(answered.pbx_call(PbxCallId(8)).is_some());
    }

    #[test]
    fn no_answer_claim_serializes_shared_answers_rollback_and_pbx_hangup() {
        let mut controller = shared_inbound_controller();
        assert!(controller.claim_ringing_forward(PbxCallId(8)));
        assert!(!controller.claim_ringing_forward(PbxCallId(8)));
        assert!(controller.phone_answer(CallId(2)).is_empty());
        assert!(controller.phone_answer(CallId(3)).is_empty());
        assert!(controller.rollback_ringing_forward(PbxCallId(8)));
        assert!(!controller.phone_answer(CallId(3)).is_empty());

        let mut hung_up = shared_inbound_controller();
        assert!(hung_up.claim_ringing_forward(PbxCallId(8)));
        assert!(hung_up.pbx_hangup_with_effects(PbxCallId(8)).is_some());
        assert!(!hung_up.rollback_ringing_forward(PbxCallId(8)));
        assert!(hung_up.complete_ringing_forward(PbxCallId(8)).is_empty());
        assert!(hung_up.invariant_error().is_none());
    }

    #[test]
    fn first_serialized_answer_wins_and_later_answers_are_noops() {
        let mut controller = shared_inbound_controller();
        let effects = controller.phone_answer(CallId(3));

        assert_eq!(
            effects,
            [
                DriverEffect::Handset(HandsetEffect::SetCallState {
                    device_id: DeviceId::new("SEP001122334455").unwrap(),
                    call_id: CallId(2),
                    state: HandsetCallState::RemoteMultiline,
                    stop_media: false,
                }),
                DriverEffect::Handset(HandsetEffect::BeginAnswerMedia {
                    device_id: DeviceId::new("SEP112233445566").unwrap(),
                    call_id: CallId(3),
                    codec: Codec::Pcmu,
                }),
            ]
        );
        assert!(controller.phone_answer(CallId(2)).is_empty());
        assert_eq!(
            controller.media_opened(CallId(3), test_media_endpoint(Codec::Pcmu)),
            [
                DriverEffect::Backend(PbxEffect::ConfigureMedia {
                    call_id: PbxCallId(8),
                    device_id: DeviceId::new("SEP112233445566").unwrap(),
                    handset_call_id: CallId(3),
                    codec: Codec::Pcmu,
                    remote: test_media_endpoint(Codec::Pcmu),
                }),
                DriverEffect::Backend(PbxEffect::Answer {
                    call_id: PbxCallId(8),
                }),
                DriverEffect::Handset(HandsetEffect::SetCallState {
                    device_id: DeviceId::new("SEP112233445566").unwrap(),
                    call_id: CallId(3),
                    state: HandsetCallState::Connected,
                    stop_media: false,
                }),
            ]
        );
        let winner = controller
            .pbx_call(PbxCallId(8))
            .unwrap()
            .active_appearance()
            .unwrap();
        assert_eq!(
            controller.call_appearance(winner).unwrap().sccp_id,
            CallId(3)
        );
        assert_eq!(
            controller.appearance_for_call(CallId(2)).unwrap().state,
            CallState::RemoteInUse
        );
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, DriverEffect::Backend(PbxEffect::Hangup { .. })))
        );
        assert!(controller.invariant_error().is_none());

        let mut reverse = shared_inbound_controller();
        assert!(!reverse.phone_answer(CallId(2)).is_empty());
        assert!(reverse.phone_answer(CallId(3)).is_empty());
        let winner = reverse
            .pbx_call(PbxCallId(8))
            .unwrap()
            .active_appearance()
            .unwrap();
        assert_eq!(reverse.call_appearance(winner).unwrap().sccp_id, CallId(2));
        assert!(reverse.invariant_error().is_none());
    }

    #[test]
    fn media_timeout_terminates_pending_answer_and_late_ack_cannot_answer() {
        let mut controller = shared_inbound_controller();
        let opening = controller.phone_answer(CallId(2));
        assert!(opening.iter().any(|effect| matches!(
            effect,
            DriverEffect::Handset(HandsetEffect::BeginAnswerMedia {
                call_id: CallId(2),
                ..
            })
        )));
        assert!(
            opening
                .iter()
                .all(|effect| !matches!(effect, DriverEffect::Backend(PbxEffect::Answer { .. })))
        );

        let cleanup = controller.terminate(CallId(2));
        assert!(cleanup.iter().any(|effect| matches!(
            effect,
            DriverEffect::Backend(PbxEffect::Hangup {
                call_id: PbxCallId(8)
            })
        )));
        assert!(cleanup.iter().any(|effect| matches!(
            effect,
            DriverEffect::Handset(HandsetEffect::SetCallState {
                call_id: CallId(2),
                state: HandsetCallState::OnHook,
                stop_media: true,
                ..
            })
        )));
        assert!(
            controller
                .media_opened(CallId(2), test_media_endpoint(Codec::Pcma))
                .is_empty()
        );
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn pending_answer_rejects_hold_switch_and_shared_steal_until_media_commits() {
        let mut controller = shared_inbound_controller();
        let device = DeviceId::new("SEP001122334455").unwrap();
        controller.offer_inbound_call(
            PbxCallId(9),
            [InboundAppearance {
                call_id: CallId(4),
                binding: binding_for(device.as_str(), 3),
                codec: Codec::Pcma,
            }],
        );

        assert!(!controller.phone_answer(CallId(2)).is_empty());
        assert!(controller.hold(CallId(2)).is_empty());
        assert!(matches!(
            controller.begin_active_call_switch_transaction(&device, CallId(4)),
            Err(CallSwitchRejection::Conflict)
        ));
        assert!(controller.steal(CallId(3)).is_empty());
        assert_eq!(
            controller.registered_device(&device).unwrap().active_call(),
            Some(CallId(2))
        );
        assert_eq!(
            controller.call(CallId(2)).unwrap().state,
            CallState::Connected
        );
        assert_eq!(
            controller.call(CallId(4)).unwrap().state,
            CallState::Ringing
        );

        let endpoint = test_media_endpoint(Codec::Pcma);
        assert!(
            controller
                .media_opened(CallId(2), endpoint)
                .iter()
                .any(|effect| matches!(effect, DriverEffect::Backend(PbxEffect::Answer { .. })))
        );
        assert!(!controller.hold(CallId(2)).is_empty());
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn pending_answer_owner_disconnect_terminates_shared_call_and_ignores_late_ack() {
        let mut controller = shared_inbound_controller();
        assert!(!controller.phone_answer(CallId(2)).is_empty());

        let effects = controller.disconnected(&DeviceId::new("SEP001122334455").unwrap());
        assert!(effects.iter().any(|effect| matches!(
            effect,
            DriverEffect::Backend(PbxEffect::Hangup {
                call_id: PbxCallId(8)
            })
        )));
        assert!(effects.iter().any(|effect| matches!(
            effect,
            DriverEffect::Handset(HandsetEffect::SetCallState {
                call_id: CallId(3),
                state: HandsetCallState::OnHook,
                stop_media: true,
                ..
            })
        )));
        assert!(controller.pbx_call(PbxCallId(8)).is_none());
        assert!(controller.call(CallId(2)).is_none());
        assert!(controller.call(CallId(3)).is_none());
        assert!(
            controller
                .media_opened(CallId(2), test_media_endpoint(Codec::Pcma))
                .is_empty()
        );
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn shared_hold_can_be_resumed_from_another_registered_device() {
        let mut controller = shared_inbound_controller();
        controller.phone_answer(CallId(2));
        let first_endpoint = test_media_endpoint(Codec::Pcma);
        controller.media_opened(CallId(2), first_endpoint);
        controller.media_transmission_started(CallId(2), first_endpoint);
        assert_eq!(
            controller.call(CallId(2)).unwrap().audio,
            MediaStreamState::Open(first_endpoint)
        );
        assert_eq!(
            controller.call(CallId(2)).unwrap().audio_transmit,
            MediaStreamState::Open(first_endpoint)
        );
        let held = controller.hold(CallId(2));
        assert_eq!(
            held,
            [
                DriverEffect::Backend(PbxEffect::Hold {
                    call_id: PbxCallId(8),
                }),
                DriverEffect::Handset(HandsetEffect::SetCallState {
                    device_id: DeviceId::new("SEP001122334455").unwrap(),
                    call_id: CallId(2),
                    state: HandsetCallState::Hold,
                    stop_media: true,
                }),
                DriverEffect::Handset(HandsetEffect::SetCallState {
                    device_id: DeviceId::new("SEP112233445566").unwrap(),
                    call_id: CallId(3),
                    state: HandsetCallState::HoldRed,
                    stop_media: false,
                }),
            ]
        );
        assert_eq!(
            controller.appearance_for_call(CallId(3)).unwrap().state,
            CallState::SharedHeld
        );
        for call_id in [CallId(2), CallId(3)] {
            let appearance = controller.call(call_id).unwrap();
            assert_eq!(appearance.audio, MediaStreamState::Closed);
            assert_eq!(appearance.audio_transmit, MediaStreamState::Closed);
        }

        let resumed = controller.resume(CallId(3));
        assert_eq!(
            resumed,
            [
                DriverEffect::Backend(PbxEffect::Resume {
                    call_id: PbxCallId(8),
                }),
                DriverEffect::Handset(HandsetEffect::SetCallState {
                    device_id: DeviceId::new("SEP001122334455").unwrap(),
                    call_id: CallId(2),
                    state: HandsetCallState::RemoteMultiline,
                    stop_media: true,
                }),
                DriverEffect::Handset(HandsetEffect::BeginMedia {
                    device_id: DeviceId::new("SEP112233445566").unwrap(),
                    call_id: CallId(3),
                    codec: Codec::Pcmu,
                }),
            ]
        );
        assert_eq!(
            controller.appearance_for_call(CallId(2)).unwrap().state,
            CallState::RemoteInUse
        );
        assert_eq!(
            controller.appearance_for_call(CallId(3)).unwrap().state,
            CallState::Connected
        );
        assert_eq!(
            controller.call(CallId(3)).unwrap().audio,
            MediaStreamState::Opening
        );
        assert_eq!(
            controller.call(CallId(3)).unwrap().audio_transmit,
            MediaStreamState::Closed
        );

        let resumed_endpoint = test_media_endpoint(Codec::Pcmu);
        assert!(matches!(
            controller
                .media_opened(CallId(3), resumed_endpoint)
                .as_slice(),
            [
                DriverEffect::Backend(PbxEffect::ConfigureMedia { .. }),
                DriverEffect::Handset(HandsetEffect::SetCallState {
                    state: HandsetCallState::Connected,
                    ..
                })
            ]
        ));
        assert_eq!(
            controller.call(CallId(3)).unwrap().audio_transmit,
            MediaStreamState::Opening
        );
        controller.media_transmission_started(CallId(3), resumed_endpoint);
        assert_eq!(
            controller.call(CallId(3)).unwrap().audio_transmit,
            MediaStreamState::Open(resumed_endpoint)
        );

        let terminal = controller.pbx_hangup_with_effects(PbxCallId(8)).unwrap();
        assert_eq!(
            terminal
                .effects
                .iter()
                .filter_map(|effect| match effect {
                    DriverEffect::Handset(HandsetEffect::SetCallState {
                        call_id,
                        state: HandsetCallState::OnHook,
                        stop_media,
                        ..
                    }) => Some((*call_id, *stop_media)),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            [(CallId(2), true), (CallId(3), true)]
        );
        assert!(controller.call(CallId(2)).is_none());
        assert!(controller.call(CallId(3)).is_none());
        assert!(
            controller
                .media_opened(CallId(3), resumed_endpoint)
                .is_empty()
        );
        controller.media_transmission_started(CallId(3), resumed_endpoint);
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn active_call_can_be_stolen_once_by_an_eligible_remote_appearance() {
        let mut controller = shared_inbound_controller();
        controller.phone_answer(CallId(2));
        controller.media_opened(CallId(2), test_media_endpoint(Codec::Pcma));
        let effects = controller.steal(CallId(3));

        assert_eq!(
            effects,
            [
                DriverEffect::Handset(HandsetEffect::SetCallState {
                    device_id: DeviceId::new("SEP001122334455").unwrap(),
                    call_id: CallId(2),
                    state: HandsetCallState::RemoteMultiline,
                    stop_media: true,
                }),
                DriverEffect::Handset(HandsetEffect::BeginMedia {
                    device_id: DeviceId::new("SEP112233445566").unwrap(),
                    call_id: CallId(3),
                    codec: Codec::Pcmu,
                }),
            ]
        );
        assert!(controller.steal(CallId(3)).is_empty());
        let winner = controller
            .pbx_call(PbxCallId(8))
            .unwrap()
            .active_appearance()
            .unwrap();
        assert_eq!(
            controller.call_appearance(winner).unwrap().sccp_id,
            CallId(3)
        );
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn steal_rejects_disabled_and_unregistered_presentations() {
        let mut controller = shared_inbound_controller();
        controller.phone_answer(CallId(2));
        controller.media_opened(CallId(2), test_media_endpoint(Codec::Pcma));
        controller.registered(registration_for("SEP223344556677"));
        controller
            .add_call_appearance(
                PbxCallId(8),
                CallId(4),
                &binding_with_ring("SEP223344556677", 3, AppearanceRingMode::Disabled),
                Codec::Pcma,
            )
            .unwrap();
        controller
            .add_call_appearance(
                PbxCallId(8),
                CallId(5),
                &binding_for("SEP334455667788", 4),
                Codec::Pcma,
            )
            .unwrap();

        assert!(controller.steal(CallId(4)).is_empty());
        assert!(controller.steal(CallId(5)).is_empty());
        assert_eq!(
            controller.appearance_for_call(CallId(2)).unwrap().state,
            CallState::Connected
        );
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn remaining_device_can_claim_a_call_after_the_owner_disconnects() {
        let mut controller = shared_inbound_controller();
        controller.phone_answer(CallId(2));
        controller.media_opened(CallId(2), test_media_endpoint(Codec::Pcma));
        assert!(
            controller
                .disconnected(&DeviceId::new("SEP001122334455").unwrap())
                .is_empty()
        );
        assert_eq!(
            controller
                .pbx_call(PbxCallId(8))
                .unwrap()
                .active_appearance(),
            None
        );

        assert_eq!(
            controller.steal(CallId(3)),
            [DriverEffect::Handset(HandsetEffect::BeginMedia {
                device_id: DeviceId::new("SEP112233445566").unwrap(),
                call_id: CallId(3),
                codec: Codec::Pcmu,
            })]
        );
        assert_eq!(
            controller.appearance_for_call(CallId(3)).unwrap().state,
            CallState::Connected
        );
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn directed_barge_checks_privacy_capabilities_and_restores_remote_appearance() {
        let mut controller = shared_inbound_controller();
        controller.phone_answer(CallId(2));
        assert_eq!(
            controller.barge(
                CallId(3),
                binding_for("SEP112233445566", 2),
                Codec::Pcmu,
                BargeMode::Directed,
            ),
            Err(BargeRejection::Capability)
        );
        enable_barge_capabilities(&mut controller, "SEP001122334455", Codec::Pcma);
        enable_barge_capabilities(&mut controller, "SEP112233445566", Codec::Pcmu);

        controller.set_call_privacy(CallId(2), true);
        assert_eq!(
            controller.barge(
                CallId(3),
                binding_for("SEP112233445566", 2),
                Codec::Pcmu,
                BargeMode::Directed,
            ),
            Err(BargeRejection::Private)
        );
        controller.set_call_privacy(CallId(2), false);

        let effects = controller
            .barge(
                CallId(3),
                binding_for("SEP112233445566", 2),
                Codec::Pcmu,
                BargeMode::Directed,
            )
            .unwrap();
        assert_eq!(
            effects,
            [
                DriverEffect::Backend(PbxEffect::CreateChannel {
                    handset_call_id: CallId(3),
                    call_id: PbxCallId(9),
                    binding: Box::new(binding_for("SEP112233445566", 2)),
                    codec: Codec::Pcmu,
                }),
                DriverEffect::Backend(PbxEffect::Barge {
                    operation: BargeOperation::Join {
                        bridge_id: PbxBridgeId(1),
                        target_call_id: PbxCallId(8),
                        barger_call_id: PbxCallId(9),
                    },
                }),
                DriverEffect::Handset(HandsetEffect::BeginMedia {
                    device_id: DeviceId::new("SEP112233445566").unwrap(),
                    call_id: CallId(3),
                    codec: Codec::Pcmu,
                }),
            ]
        );
        assert_eq!(
            controller.appearance_for_call(CallId(3)).unwrap().state,
            CallState::Barged
        );
        assert_eq!(
            controller
                .pbx_call(PbxCallId(8))
                .unwrap()
                .active_appearance()
                .and_then(|id| controller.call_appearance(id))
                .map(|appearance| appearance.sccp_id),
            Some(CallId(2))
        );
        let endpoint = MediaEndpoint {
            address: "192.0.2.20".parse().unwrap(),
            rtp_port: 20_000,
            rtcp_port: 20_001,
            codec: Codec::Pcmu,
            packet_ms: 20,
            max_frames_per_packet: 1,
            telephone_event_payload: 101,
        };
        assert!(matches!(
            controller.media_opened(CallId(3), endpoint).as_slice(),
            [DriverEffect::Backend(PbxEffect::ConfigureMedia {
                call_id: PbxCallId(9),
                handset_call_id: CallId(3),
                ..
            })]
        ));
        assert!(controller.hold(CallId(2)).is_empty());

        let cleanup = controller.hangup(CallId(3));
        assert_eq!(
            cleanup,
            [
                DriverEffect::Backend(PbxEffect::Barge {
                    operation: BargeOperation::Leave {
                        bridge_id: PbxBridgeId(1),
                        barger_call_id: PbxCallId(9),
                        last_participant: true,
                    },
                }),
                DriverEffect::Backend(PbxEffect::Hangup {
                    call_id: PbxCallId(9),
                }),
                DriverEffect::Handset(HandsetEffect::SetCallState {
                    device_id: DeviceId::new("SEP112233445566").unwrap(),
                    call_id: CallId(3),
                    state: HandsetCallState::RemoteMultiline,
                    stop_media: true,
                }),
            ]
        );
        assert!(controller.pbx_call(PbxCallId(8)).is_some());
        assert!(controller.pbx_call(PbxCallId(9)).is_none());
        assert_eq!(
            controller.appearance_for_call(CallId(3)).unwrap().state,
            CallState::RemoteInUse
        );
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn conference_barge_reuses_one_bridge_and_cleans_every_participant() {
        let mut controller = shared_inbound_controller();
        controller.registered(registration_for("SEP223344556677"));
        controller
            .add_call_appearance(
                PbxCallId(8),
                CallId(4),
                &binding_for("SEP223344556677", 3),
                Codec::Pcmu,
            )
            .unwrap();
        controller.phone_answer(CallId(2));
        enable_barge_capabilities(&mut controller, "SEP001122334455", Codec::Pcma);
        enable_barge_capabilities(&mut controller, "SEP112233445566", Codec::Pcmu);
        enable_barge_capabilities(&mut controller, "SEP223344556677", Codec::Pcmu);

        let first = controller
            .barge(
                CallId(3),
                binding_for("SEP112233445566", 2),
                Codec::Pcmu,
                BargeMode::Conference,
            )
            .unwrap();
        let second = controller
            .barge(
                CallId(4),
                binding_for("SEP223344556677", 3),
                Codec::Pcmu,
                BargeMode::Conference,
            )
            .unwrap();
        assert!(matches!(
            first.get(1),
            Some(DriverEffect::Backend(PbxEffect::Barge {
                operation: BargeOperation::Join {
                    bridge_id: PbxBridgeId(1),
                    barger_call_id: PbxCallId(9),
                    ..
                }
            }))
        ));
        assert!(matches!(
            second.get(1),
            Some(DriverEffect::Backend(PbxEffect::Barge {
                operation: BargeOperation::Join {
                    bridge_id: PbxBridgeId(1),
                    barger_call_id: PbxCallId(10),
                    ..
                }
            }))
        ));
        assert!(matches!(
            controller.hangup(CallId(3)).first(),
            Some(DriverEffect::Backend(PbxEffect::Barge {
                operation: BargeOperation::Leave {
                    last_participant: false,
                    ..
                }
            }))
        ));
        assert!(matches!(
            controller.hangup(CallId(4)).first(),
            Some(DriverEffect::Backend(PbxEffect::Barge {
                operation: BargeOperation::Leave {
                    last_participant: true,
                    ..
                }
            }))
        ));
        assert!(controller.pbx_call(PbxCallId(8)).is_some());
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn first_serialized_steal_or_barge_claim_wins_without_target_hangup() {
        fn prepared() -> Controller {
            let mut controller = shared_inbound_controller();
            controller.registered(registration_for("SEP223344556677"));
            controller
                .add_call_appearance(
                    PbxCallId(8),
                    CallId(4),
                    &binding_for("SEP223344556677", 3),
                    Codec::Pcmu,
                )
                .unwrap();
            controller.phone_answer(CallId(2));
            controller.media_opened(CallId(2), test_media_endpoint(Codec::Pcma));
            enable_barge_capabilities(&mut controller, "SEP001122334455", Codec::Pcma);
            enable_barge_capabilities(&mut controller, "SEP112233445566", Codec::Pcmu);
            enable_barge_capabilities(&mut controller, "SEP223344556677", Codec::Pcmu);
            controller
        }

        let mut steal_first = prepared();
        assert!(!steal_first.steal(CallId(3)).is_empty());
        assert_eq!(
            steal_first.barge(
                CallId(4),
                binding_for("SEP223344556677", 3),
                Codec::Pcmu,
                BargeMode::Directed,
            ),
            Err(BargeRejection::Conflict)
        );
        assert!(steal_first.pbx_call(PbxCallId(8)).is_some());

        let mut barge_first = prepared();
        let effects = barge_first
            .barge(
                CallId(4),
                binding_for("SEP223344556677", 3),
                Codec::Pcmu,
                BargeMode::Directed,
            )
            .unwrap();
        assert!(barge_first.steal(CallId(3)).is_empty());
        assert!(!effects.iter().any(|effect| matches!(
            effect,
            DriverEffect::Backend(PbxEffect::Hangup {
                call_id: PbxCallId(8)
            })
        )));
        assert!(barge_first.invariant_error().is_none());
    }

    #[test]
    fn fake_handset_races_serialize_answer_hold_steal_and_barge() {
        fn prepared_for_barge() -> Controller {
            let mut controller = shared_inbound_controller();
            controller.registered(registration_for("SEP223344556677"));
            controller
                .add_call_appearance(
                    PbxCallId(8),
                    CallId(4),
                    &binding_for("SEP223344556677", 3),
                    Codec::Pcmu,
                )
                .unwrap();
            enable_barge_capabilities(&mut controller, "SEP001122334455", Codec::Pcma);
            enable_barge_capabilities(&mut controller, "SEP112233445566", Codec::Pcmu);
            enable_barge_capabilities(&mut controller, "SEP223344556677", Codec::Pcmu);
            controller
        }

        // Two handsets answer the same offer: the first serialized answer is
        // the only one that reaches either the backend or handset media.
        let mut answer = shared_inbound_controller();
        let first = answer.phone_answer(CallId(2));
        let second = answer.phone_answer(CallId(3));
        let mut handsets = FakeHandsets::default();
        handsets.apply(&first);
        handsets.apply(&second);
        assert_eq!(handsets.media_winners(), [CallId(2)]);
        assert_eq!(
            first
                .iter()
                .chain(&second)
                .filter(|effect| matches!(effect, DriverEffect::Backend(PbxEffect::Answer { .. })))
                .count(),
            0
        );
        assert_eq!(
            answer
                .media_opened(CallId(2), test_media_endpoint(Codec::Pcma))
                .iter()
                .filter(|effect| matches!(effect, DriverEffect::Backend(PbxEffect::Answer { .. })))
                .count(),
            1
        );
        assert!(answer.invariant_error().is_none());

        // Holding first makes a concurrent steal ineligible. Stealing first
        // transfers ownership and makes the former owner's hold a no-op.
        let mut hold_first = shared_inbound_controller();
        hold_first.phone_answer(CallId(2));
        hold_first.media_opened(CallId(2), test_media_endpoint(Codec::Pcma));
        assert!(!hold_first.hold(CallId(2)).is_empty());
        assert!(hold_first.steal(CallId(3)).is_empty());
        assert!(hold_first.invariant_error().is_none());

        let mut steal_first = shared_inbound_controller();
        steal_first.phone_answer(CallId(2));
        steal_first.media_opened(CallId(2), test_media_endpoint(Codec::Pcma));
        let steal = steal_first.steal(CallId(3));
        assert!(!steal.is_empty());
        assert!(steal_first.hold(CallId(2)).is_empty());
        assert!(steal_first.invariant_error().is_none());

        // Directed barge has one winner. Reversing request order reverses the
        // winner without ever hanging up the shared target call.
        for (winner, loser, winner_device) in [
            (CallId(3), CallId(4), "SEP112233445566"),
            (CallId(4), CallId(3), "SEP223344556677"),
        ] {
            let mut controller = prepared_for_barge();
            controller.phone_answer(CallId(2));
            let winning_effects = controller
                .barge(
                    winner,
                    binding_for(winner_device, if winner == CallId(3) { 2 } else { 3 }),
                    Codec::Pcmu,
                    BargeMode::Directed,
                )
                .unwrap();
            let losing_device = if loser == CallId(3) {
                "SEP112233445566"
            } else {
                "SEP223344556677"
            };
            assert_eq!(
                controller.barge(
                    loser,
                    binding_for(losing_device, if loser == CallId(3) { 2 } else { 3 }),
                    Codec::Pcmu,
                    BargeMode::Directed,
                ),
                Err(BargeRejection::AlreadyBarged)
            );
            let mut handsets = FakeHandsets::default();
            handsets.apply(&winning_effects);
            assert_eq!(handsets.media_winners(), [winner]);
            assert!(!winning_effects.iter().any(|effect| matches!(
                effect,
                DriverEffect::Backend(PbxEffect::Hangup {
                    call_id: PbxCallId(8)
                })
            )));
            assert!(controller.pbx_call(PbxCallId(8)).is_some());
            assert!(controller.invariant_error().is_none());
        }
    }

    #[test]
    fn barge_abort_and_target_hangup_have_exact_cleanup_without_double_hangup() {
        let mut controller = shared_inbound_controller();
        controller.phone_answer(CallId(2));
        enable_barge_capabilities(&mut controller, "SEP001122334455", Codec::Pcma);
        enable_barge_capabilities(&mut controller, "SEP112233445566", Codec::Pcmu);
        controller
            .barge(
                CallId(3),
                binding_for("SEP112233445566", 2),
                Codec::Pcmu,
                BargeMode::Directed,
            )
            .unwrap();
        let failed_join = controller.abort_barge(CallId(3), false, true);
        assert!(failed_join.iter().any(|effect| matches!(
            effect,
            DriverEffect::Backend(PbxEffect::Hangup {
                call_id: PbxCallId(9)
            })
        )));
        assert!(
            !failed_join
                .iter()
                .any(|effect| matches!(effect, DriverEffect::Backend(PbxEffect::Barge { .. })))
        );

        controller
            .barge(
                CallId(3),
                binding_for("SEP112233445566", 2),
                Codec::Pcmu,
                BargeMode::Directed,
            )
            .unwrap();
        let outcome = controller.pbx_hangup_with_effects(PbxCallId(8)).unwrap();
        assert_eq!(
            outcome
                .effects
                .iter()
                .filter(|effect| matches!(
                    effect,
                    DriverEffect::Backend(PbxEffect::Barge {
                        operation: BargeOperation::Leave { .. }
                    })
                ))
                .count(),
            1
        );
        assert_eq!(
            outcome
                .effects
                .iter()
                .filter(|effect| matches!(
                    effect,
                    DriverEffect::Backend(PbxEffect::Hangup {
                        call_id: PbxCallId(10)
                    })
                ))
                .count(),
            1
        );
        assert!(!outcome.effects.iter().any(|effect| matches!(
            effect,
            DriverEffect::Backend(PbxEffect::Hangup {
                call_id: PbxCallId(8)
            })
        )));
        assert_eq!(controller.calls().count(), 0);
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn pbx_hangup_publishes_available_to_every_shared_appearance() {
        let mut controller = shared_inbound_controller();
        controller.phone_answer(CallId(2));
        let outcome = controller.pbx_hangup_with_effects(PbxCallId(8)).unwrap();

        assert_eq!(outcome.effects.len(), 2);
        assert!(outcome.effects.iter().all(|effect| matches!(
            effect,
            DriverEffect::Handset(HandsetEffect::SetCallState {
                state: HandsetCallState::OnHook,
                stop_media: true,
                ..
            })
        )));
        assert!(controller.pbx_call(PbxCallId(8)).is_none());
        assert_eq!(controller.calls().count(), 0);
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn immediate_divert_claim_is_exact_and_failure_preserves_the_ringing_call() {
        let mut controller = shared_inbound_controller();
        let device = DeviceId::new("SEP001122334455").unwrap();
        let plan = controller
            .begin_immediate_divert(&device, CallId(2), voicemail_target("61001"))
            .unwrap();

        assert_eq!(plan.transaction.action, VoicemailAction::ImmediateDivert);
        assert_eq!(plan.transaction.phase, VoicemailPhase::Executing);
        assert!(matches!(
            plan.effects.as_slice(),
            [DriverEffect::Backend(PbxEffect::Voicemail { operation })]
                if operation.transaction_id == plan.transaction.id
                    && operation.device_id == device
                    && operation.handset_call_id == CallId(2)
                    && operation.pbx_call_id == PbxCallId(8)
                    && operation.action == VoicemailAction::ImmediateDivert
                    && operation.target.destination() == "61001"
        ));
        assert!(controller.phone_answer(CallId(2)).is_empty());
        assert_eq!(
            controller.begin_immediate_divert(&device, CallId(2), voicemail_target("61001")),
            Err(VoicemailRejection::Conflict)
        );

        let aborted = controller
            .abort_voicemail(&device, plan.transaction.id)
            .unwrap();
        assert_eq!(aborted.id, plan.transaction.id);
        assert_eq!(
            controller.call(CallId(2)).unwrap().state,
            CallState::Ringing
        );
        assert_eq!(
            controller.call(CallId(3)).unwrap().state,
            CallState::Ringing
        );
        assert!(!controller.phone_answer(CallId(2)).is_empty());
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn immediate_divert_success_ends_every_shared_appearance_once() {
        let mut controller = shared_inbound_controller();
        let device = DeviceId::new("SEP001122334455").unwrap();
        let plan = controller
            .begin_immediate_divert(&device, CallId(2), voicemail_target("61001"))
            .unwrap();
        let outcome = controller
            .voicemail_succeeded(&device, plan.transaction.id)
            .unwrap();

        assert_eq!(outcome.transaction.id, plan.transaction.id);
        assert_eq!(outcome.effects.len(), 2);
        assert!(outcome.effects.iter().all(|effect| matches!(
            effect,
            DriverEffect::Handset(HandsetEffect::SetCallState {
                state: HandsetCallState::OnHook,
                stop_media: true,
                ..
            })
        )));
        assert!(controller.pbx_call(PbxCallId(8)).is_none());
        assert!(controller.call(CallId(2)).is_none());
        assert!(controller.call(CallId(3)).is_none());
        assert!(!controller.voicemail_generation_is_active(&device, plan.transaction.id));
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn voicemail_claim_is_cancelled_by_pbx_hangup_and_last_appearance_disconnect() {
        let device = DeviceId::new("SEP001122334455").unwrap();
        let mut hung_up = shared_inbound_controller();
        let hangup_plan = hung_up
            .begin_immediate_divert(&device, CallId(2), voicemail_target("61001"))
            .unwrap();
        let outcome = hung_up.pbx_hangup_with_effects(PbxCallId(8)).unwrap();
        assert_eq!(outcome.effects.len(), 2);
        assert!(!hung_up.voicemail_generation_is_active(&device, hangup_plan.transaction.id));
        assert_eq!(
            hung_up
                .complete_voicemail_native(
                    &device,
                    hangup_plan.transaction.id,
                    hangup_plan.transaction.pbx_call_id,
                )
                .unwrap(),
            VoicemailNativeOutcome::CallAlreadyEnded
        );

        let mut disconnected = connected_outbound_controller();
        let disconnect_plan = disconnected
            .begin_selected_voicemail_transfer(&device, voicemail_target("61001"))
            .unwrap();
        let _ = disconnected.disconnected(&device);
        assert!(
            !disconnected.voicemail_generation_is_active(&device, disconnect_plan.transaction.id)
        );
        assert!(
            disconnected
                .pbx_call(disconnect_plan.transaction.pbx_call_id)
                .is_none()
        );
        assert_eq!(
            disconnected
                .complete_voicemail_native(
                    &device,
                    disconnect_plan.transaction.id,
                    disconnect_plan.transaction.pbx_call_id,
                )
                .unwrap(),
            VoicemailNativeOutcome::CallAlreadyEnded
        );
        assert!(disconnected.invariant_error().is_none());
    }

    #[test]
    fn native_voicemail_success_survives_shared_owner_disconnect() {
        let first = DeviceId::new("SEP001122334455").unwrap();

        let mut immediate = shared_inbound_controller();
        let immediate_plan = immediate
            .begin_immediate_divert(&first, CallId(2), voicemail_target("61001"))
            .unwrap();
        let _ = immediate.disconnected(&first);
        assert!(immediate.voicemail_generation_is_active(&first, immediate_plan.transaction.id));
        let immediate_outcome = immediate
            .complete_voicemail_native(
                &first,
                immediate_plan.transaction.id,
                immediate_plan.transaction.pbx_call_id,
            )
            .unwrap();
        assert!(matches!(
            immediate_outcome,
            VoicemailNativeOutcome::Committed(VoicemailTerminalOutcome { ref effects, .. })
                if effects.len() == 1
                    && matches!(
                        effects[0],
                        DriverEffect::Handset(HandsetEffect::SetCallState {
                            call_id: CallId(3),
                            state: HandsetCallState::OnHook,
                            ..
                        })
                    )
        ));
        assert!(immediate.pbx_call(PbxCallId(8)).is_none());

        let mut selected = shared_inbound_controller();
        selected.phone_answer(CallId(2));
        selected.media_opened(CallId(2), test_media_endpoint(Codec::Pcma));
        assert!(selected.set_call_selected(&first, CallId(2), true));
        let selected_plan = selected
            .begin_selected_voicemail_transfer(&first, voicemail_target("61001"))
            .unwrap();
        let _ = selected.disconnected(&first);
        assert!(selected.voicemail_generation_is_active(&first, selected_plan.transaction.id));
        assert!(matches!(
            selected
                .complete_voicemail_native(
                    &first,
                    selected_plan.transaction.id,
                    selected_plan.transaction.pbx_call_id,
                )
                .unwrap(),
            VoicemailNativeOutcome::Committed(_)
        ));
        assert!(selected.pbx_call(PbxCallId(8)).is_none());
        assert!(selected.invariant_error().is_none());
    }

    #[test]
    fn selected_voicemail_requires_exactly_one_owned_connected_or_held_call() {
        let device = binding().device_id;
        let mut none = connected_outbound_controller();
        none.set_call_selected(&device, CallId(1), false);
        assert_eq!(
            none.begin_selected_voicemail_transfer(&device, voicemail_target("61001")),
            Err(VoicemailRejection::Conflict)
        );

        let mut multiple = connected_outbound_controller();
        multiple
            .begin_additional_phone_call(CallId(2), binding(), Codec::Pcmu, Instant::now())
            .unwrap();
        multiple.set_call_selected(&device, CallId(1), true);
        multiple.set_call_selected(&device, CallId(2), true);
        assert_eq!(
            multiple.begin_selected_voicemail_transfer(&device, voicemail_target("61001")),
            Err(VoicemailRejection::Conflict)
        );

        let wrong_device = DeviceId::new("SEP112233445566").unwrap();
        let mut wrong = connected_outbound_controller();
        assert_eq!(
            wrong.begin_selected_voicemail_transfer(&wrong_device, voicemail_target("61001")),
            Err(VoicemailRejection::Conflict)
        );
        let mut ringing = shared_inbound_controller();
        assert!(ringing.set_call_selected(&device, CallId(2), true));
        assert_eq!(
            ringing.begin_selected_voicemail_transfer(&device, voicemail_target("61001")),
            Err(VoicemailRejection::InvalidPhase)
        );
        let mut remote = shared_inbound_controller();
        remote.phone_answer(CallId(3));
        assert!(remote.set_call_selected(&device, CallId(2), true));
        assert_eq!(
            remote.begin_selected_voicemail_transfer(&device, voicemail_target("61001")),
            Err(VoicemailRejection::InvalidPhase)
        );
        let mut held = connected_outbound_controller();
        held.hold(CallId(1));
        assert!(held.set_call_selected(&device, CallId(1), true));
        assert!(
            held.begin_selected_voicemail_transfer(&device, voicemail_target("61001"))
                .is_ok()
        );
        assert!(none.invariant_error().is_none());
        assert!(multiple.invariant_error().is_none());
        assert!(wrong.invariant_error().is_none());
        assert!(ringing.invariant_error().is_none());
        assert!(remote.invariant_error().is_none());
        assert!(held.invariant_error().is_none());
    }

    #[test]
    fn selected_voicemail_serializes_against_park_transfer_and_conference() {
        let mut controller = connected_outbound_controller();
        let device = binding().device_id;
        let plan = controller
            .begin_selected_voicemail_transfer(&device, voicemail_target("61001"))
            .unwrap();

        assert_eq!(plan.transaction.action, VoicemailAction::TransferSelected);
        assert_eq!(
            controller.park(CallId(1), true, None),
            Err(ParkingRejection::Conflict)
        );
        assert_eq!(
            controller.begin_transfer(TransferConsultationRequest {
                source_call_id: CallId(1),
                consultation_call_id: CallId(2),
                binding: binding(),
                codec: Codec::Pcmu,
                complete_on_hangup: false,
                now: Instant::now(),
            }),
            Err(TransferRejection::Conflict)
        );
        assert_eq!(
            controller.begin_conference(
                CallId(1),
                CallId(2),
                binding(),
                Codec::Pcmu,
                Instant::now(),
                true,
            ),
            Err(ConferenceRejection::Conflict)
        );
        assert_eq!(
            controller.call(CallId(1)).unwrap().state,
            CallState::Connected
        );

        controller
            .abort_voicemail(&device, plan.transaction.id)
            .unwrap();
        let retry = controller
            .begin_selected_voicemail_transfer(&device, voicemail_target("61001"))
            .unwrap();
        assert!(retry.transaction.id > plan.transaction.id);
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn registered_device_runtime_tracks_capabilities_and_selection() {
        let now = Instant::now();
        let mut controller = Controller::new(Duration::from_secs(1));
        let registration = registration();
        let device = registration.id.clone();
        controller.registered(registration);
        controller.capabilities(
            &device,
            vec![MediaCapability {
                codec: Codec::Pcma,
                max_frames_per_packet: 4,
                codec_parameters: [0; 8],
            }],
        );

        controller.begin_phone_call(CallId(12), binding(), Codec::Pcma, now);
        let state = controller.registered_device(&device).unwrap();
        assert_eq!(state.registration.firmware, "SCCP-test");
        assert_eq!(state.capabilities.audio()[0].codec, Codec::Pcma);
        assert_eq!(state.selected_line, Some(1));
        assert!(state.is_call_selected(CallId(12)));

        controller.hold(CallId(12));
        assert!(
            !controller
                .registered_device(&device)
                .unwrap()
                .is_call_selected(CallId(12))
        );
        controller.resume(CallId(12));
        assert!(
            controller
                .registered_device(&device)
                .unwrap()
                .is_call_selected(CallId(12))
        );
        controller.hangup(CallId(12));
        assert_eq!(
            controller
                .registered_device(&device)
                .unwrap()
                .selected_calls()
                .count(),
            0
        );
    }

    #[test]
    fn newer_session_retires_old_calls_and_rejects_late_session_state() {
        let mut controller = connected_outbound_controller();
        let device = DeviceId::new("SEP001122334455").unwrap();
        let old_generation = controller
            .registered_device(&device)
            .unwrap()
            .session_generation;
        let old_capabilities = StationMediaCapabilities::new(
            vec![MediaCapability {
                codec: Codec::Pcmu,
                max_frames_per_packet: 4,
                codec_parameters: [0; 8],
            }],
            vec![VideoCapability {
                codec: Codec::H264,
                direction: ReceiveTransmit::RECEIVE | ReceiveTransmit::TRANSMIT,
                level_preferences: Vec::new(),
                codec_parameters: vec![1, 2, 3],
                encryption_capability: None,
                address_type: None,
            }],
        );
        assert!(controller.update_capabilities(&device, old_generation, old_capabilities.clone(),));
        let old_encryption = StationEncryptionCapabilities::Supported(vec![
            crate::media::encryption::AdvertisedEncryptionProfile {
                algorithm: sccp_protocol::EncryptionMethod::Aes128HmacSha1_80,
                master_key_bits: 128,
            },
        ]);
        assert!(controller.update_audio_encryption_capabilities(
            &device,
            old_generation,
            old_encryption.clone(),
        ));

        let new_generation = SessionGeneration::new(old_generation.get() + 1).unwrap();
        let outcome = controller
            .register_session(new_generation, registration())
            .unwrap();

        assert!(outcome.replaced);
        assert_eq!(
            outcome.cleanup,
            vec![DriverEffect::Backend(PbxEffect::Hangup {
                call_id: PbxCallId(1),
            })]
        );
        assert!(controller.call(CallId(1)).is_none());
        assert!(controller.pbx_call(PbxCallId(1)).is_none());
        let state = controller.registered_device(&device).unwrap();
        assert_eq!(state.session_generation, new_generation);
        assert!(state.capabilities.is_empty());
        assert_eq!(
            state.audio_encryption,
            StationEncryptionCapabilities::NotReported
        );

        assert!(!controller.session_is_current(&device, old_generation));
        assert!(
            !controller.update_capabilities(&device, old_generation, old_capabilities.clone(),)
        );
        assert!(!controller.update_audio_encryption_capabilities(
            &device,
            old_generation,
            old_encryption.clone(),
        ));
        assert!(
            controller
                .register_session(old_generation, registration())
                .is_none()
        );
        assert!(
            controller
                .register_session(new_generation, registration())
                .is_none()
        );
        assert!(
            controller
                .registered_device(&device)
                .unwrap()
                .capabilities
                .is_empty()
        );

        assert!(controller.session_is_current(&device, new_generation));
        assert!(controller.update_capabilities(&device, new_generation, old_capabilities.clone(),));
        assert!(controller.update_audio_encryption_capabilities(
            &device,
            new_generation,
            old_encryption.clone(),
        ));
        let state = controller.registered_device(&device).unwrap();
        assert_eq!(state.capabilities.audio(), old_capabilities.audio());
        assert_eq!(state.capabilities.video(), old_capabilities.video());
        assert_eq!(state.audio_encryption, old_encryption);
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn session_replacement_retains_cleanup_for_surviving_handsets() {
        let mut controller = shared_inbound_controller();
        let replaced = DeviceId::new("SEP001122334455").unwrap();
        let survivor = DeviceId::new("SEP112233445566").unwrap();
        assert!(!controller.phone_answer(CallId(2)).is_empty());
        let generation = controller
            .registered_device(&replaced)
            .unwrap()
            .session_generation;

        let outcome = controller
            .register_session(
                SessionGeneration::new(generation.get() + 1).unwrap(),
                registration_for(replaced.as_str()),
            )
            .unwrap();

        assert!(outcome.replaced);
        assert!(outcome.cleanup.iter().any(|effect| matches!(
            effect,
            DriverEffect::Backend(PbxEffect::Hangup {
                call_id: PbxCallId(8)
            })
        )));
        assert!(outcome.cleanup.iter().any(|effect| matches!(
            effect,
            DriverEffect::Handset(HandsetEffect::SetCallState {
                device_id,
                call_id: CallId(3),
                state: HandsetCallState::OnHook,
                stop_media: true,
            }) if device_id == &survivor
        )));
        assert!(!outcome.cleanup.iter().any(
            |effect| matches!(effect, DriverEffect::Handset(effect) if effect.device_id() == &replaced)
        ));
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn additional_calls_keep_independent_identity_and_switch_in_exact_order() {
        let now = Instant::now();
        let mut controller = connected_outbound_controller();
        let device = binding().device_id;

        let created = controller
            .begin_additional_phone_call(CallId(2), binding(), Codec::Pcma, now)
            .unwrap();
        assert!(matches!(
            created.as_slice(),
            [
                DriverEffect::Backend(PbxEffect::Hold {
                    call_id: PbxCallId(1)
                }),
                DriverEffect::Handset(HandsetEffect::SetCallState {
                    call_id: CallId(1),
                    state: HandsetCallState::Hold,
                    stop_media: true,
                    ..
                }),
                DriverEffect::Backend(PbxEffect::CreateChannel {
                    call_id: PbxCallId(2),
                    handset_call_id: CallId(2),
                    ..
                }),
                ..
            ]
        ));
        assert_eq!(controller.call(CallId(1)).unwrap().state, CallState::Held);
        assert_eq!(
            controller.call(CallId(2)).unwrap().state,
            CallState::Collecting
        );
        assert_eq!(
            controller.registered_device(&device).unwrap().active_call(),
            Some(CallId(2))
        );
        assert_ne!(
            controller.call(CallId(1)).unwrap().pbx_id,
            controller.call(CallId(2)).unwrap().pbx_id
        );

        let switched = controller.switch_active_call(&device, CallId(1)).unwrap();
        assert!(matches!(
            switched.first(),
            Some(DriverEffect::Backend(PbxEffect::Hold {
                call_id: PbxCallId(2)
            }))
        ));
        assert!(switched.iter().any(|effect| matches!(
            effect,
            DriverEffect::Backend(PbxEffect::Resume {
                call_id: PbxCallId(1)
            })
        )));
        assert_eq!(
            controller.call(CallId(1)).unwrap().state,
            CallState::Connected
        );
        assert_eq!(controller.call(CallId(2)).unwrap().state, CallState::Held);
        assert_eq!(
            controller.registered_device(&device).unwrap().active_call(),
            Some(CallId(1))
        );
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn answering_waiting_call_holds_active_call_and_stale_switch_is_non_mutating() {
        let mut controller = connected_outbound_controller();
        let device = binding().device_id;
        let offers = controller.offer_inbound_call(
            PbxCallId(8),
            [InboundAppearance {
                call_id: CallId(2),
                binding: binding(),
                codec: Codec::Pcma,
            }],
        );
        assert_eq!(offers.len(), 1);

        let switched = controller.switch_active_call(&device, CallId(2)).unwrap();
        let backend = switched
            .iter()
            .filter_map(|effect| match effect {
                DriverEffect::Backend(effect) => Some(effect),
                DriverEffect::Handset(_) => None,
            })
            .collect::<Vec<_>>();
        assert!(matches!(
            backend.as_slice(),
            [PbxEffect::Hold {
                call_id: PbxCallId(1)
            }]
        ));
        assert!(
            controller
                .media_opened(CallId(2), test_media_endpoint(Codec::Pcma))
                .iter()
                .any(|effect| matches!(
                    effect,
                    DriverEffect::Backend(PbxEffect::Answer {
                        call_id: PbxCallId(8)
                    })
                ))
        );
        assert_eq!(controller.call(CallId(1)).unwrap().state, CallState::Held);
        assert_eq!(
            controller.call(CallId(2)).unwrap().state,
            CallState::Connected
        );
        let snapshot = (
            controller.call(CallId(1)).unwrap().state,
            controller.call(CallId(2)).unwrap().state,
            controller.registered_device(&device).unwrap().active_call(),
        );
        assert_eq!(
            controller.switch_active_call(&device, CallId(999)),
            Err(CallSwitchRejection::Unavailable)
        );
        assert_eq!(
            snapshot,
            (
                controller.call(CallId(1)).unwrap().state,
                controller.call(CallId(2)).unwrap().state,
                controller.registered_device(&device).unwrap().active_call(),
            )
        );
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn selection_is_device_scoped_and_removed_with_each_independent_call() {
        let now = Instant::now();
        let mut controller = Controller::new(Duration::from_secs(1));
        let device = binding().device_id;
        controller.registered(registration());
        controller.begin_phone_call(CallId(1), binding(), Codec::Pcmu, now);
        controller.begin_phone_call(CallId(2), binding(), Codec::Pcma, now);

        assert_eq!(
            controller.toggle_call_selected(&device, CallId(1)),
            Some(false)
        );
        assert_eq!(
            controller.toggle_call_selected(&device, CallId(1)),
            Some(true)
        );
        assert_eq!(
            controller.toggle_call_selected(&DeviceId::new("SEP112233445566").unwrap(), CallId(1)),
            None
        );
        controller.hangup(CallId(2));
        assert_eq!(
            controller.registered_device(&device).unwrap().active_call(),
            None
        );
        assert!(controller.call(CallId(1)).is_some());
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn three_call_switch_and_cleanup_preserve_unrelated_selection_and_identity() {
        let mut controller = connected_outbound_controller();
        let device = binding().device_id;
        for (pbx_id, call_id) in [(PbxCallId(8), CallId(2)), (PbxCallId(9), CallId(3))] {
            controller.offer_inbound_call(
                pbx_id,
                [InboundAppearance {
                    call_id,
                    binding: binding(),
                    codec: Codec::Pcma,
                }],
            );
            assert!(controller.set_call_selected(&device, call_id, true));
        }

        let transition = controller
            .begin_active_call_switch_transaction(&device, CallId(2))
            .unwrap();
        for effect in &transition.effects {
            assert!(controller.record_call_transition_success(transition.id, effect));
        }
        assert!(controller.commit_call_transition(transition.id));
        controller
            .pbx_hangup_with_effects(PbxCallId(9))
            .expect("third call is independently addressable");

        assert_eq!(controller.call(CallId(1)).unwrap().state, CallState::Held);
        assert_eq!(
            controller.call(CallId(2)).unwrap().state,
            CallState::Connected
        );
        assert!(controller.call(CallId(3)).is_none());
        let registered = controller.registered_device(&device).unwrap();
        assert_eq!(registered.active_call(), Some(CallId(2)));
        assert!(registered.is_call_selected(CallId(2)));
        assert!(!registered.is_call_selected(CallId(3)));
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn hook_flash_resolves_connected_waiting_held_conference_and_duplicate_states() {
        let device = binding().device_id;
        let mut idle = Controller::new(Duration::from_secs(1));
        idle.registered(registration());
        assert_eq!(
            idle.hook_flash_action(&device, CallId(1)),
            HookFlashAction::Ignore
        );
        let mut connected = connected_outbound_controller();
        assert_eq!(
            connected.hook_flash_action(&device, CallId(1)),
            HookFlashAction::Transfer
        );
        assert_eq!(
            connected.hook_flash_action(&device, CallId(99)),
            HookFlashAction::Ignore
        );

        connected.offer_inbound_call(
            PbxCallId(8),
            [InboundAppearance {
                call_id: CallId(2),
                binding: binding(),
                codec: Codec::Pcma,
            }],
        );
        assert_eq!(
            connected.hook_flash_action(&device, CallId(1)),
            HookFlashAction::AnswerWaiting(CallId(2))
        );
        assert!(!connected.hold(CallId(1)).is_empty());
        assert_eq!(
            connected.hook_flash_action(&device, CallId(1)),
            HookFlashAction::Ignore
        );

        let mut duplicate = connected_outbound_controller();
        duplicate
            .begin_transfer(TransferConsultationRequest {
                source_call_id: CallId(1),
                consultation_call_id: CallId(2),
                binding: binding(),
                codec: Codec::Pcma,
                complete_on_hangup: false,
                now: Instant::now(),
            })
            .unwrap();
        assert_eq!(
            duplicate.hook_flash_action(&device, CallId(2)),
            HookFlashAction::Transfer
        );

        let conference = active_three_party_conference();
        assert_eq!(
            conference.hook_flash_action(&device, CallId(4)),
            HookFlashAction::Ignore
        );
        assert!(connected.invariant_error().is_none());
        assert!(duplicate.invariant_error().is_none());
        assert!(conference.invariant_error().is_none());
    }

    #[test]
    fn additional_call_transaction_rolls_back_every_effect_boundary() {
        for fail_at in 0..5 {
            let mut controller = connected_outbound_controller();
            let device = binding().device_id;
            let transition = controller
                .begin_additional_phone_call_transaction(
                    CallId(2),
                    binding(),
                    Codec::Pcma,
                    Instant::now(),
                )
                .unwrap();
            let mut progress = CallTransitionProgress::default();
            for effect in transition.effects.iter().take(fail_at) {
                progress.record_success(&transition, effect);
                assert!(controller.record_call_transition_success(transition.id, effect));
            }
            let cleanup = controller.abort_call_transition(transition.id, &progress);
            assert_eq!(
                controller.call(CallId(1)).unwrap().state,
                CallState::Connected,
                "failed at {fail_at}"
            );
            assert_eq!(
                controller.registered_device(&device).unwrap().active_call(),
                Some(CallId(1)),
                "failed at {fail_at}"
            );
            assert!(controller.call(CallId(2)).is_none(), "failed at {fail_at}");
            assert_eq!(
                cleanup.iter().any(|effect| matches!(
                    effect,
                    DriverEffect::Backend(PbxEffect::Resume {
                        call_id: PbxCallId(1)
                    })
                )),
                fail_at > 0,
                "failed at {fail_at}"
            );
            assert_eq!(
                cleanup.iter().any(|effect| matches!(
                    effect,
                    DriverEffect::Backend(PbxEffect::Hangup {
                        call_id: PbxCallId(2)
                    })
                )),
                fail_at > 2,
                "failed at {fail_at}"
            );
            assert!(cleanup.iter().any(|effect| matches!(
                effect,
                DriverEffect::Handset(HandsetEffect::SetCallState {
                    call_id: CallId(2),
                    state: HandsetCallState::OnHook,
                    ..
                })
            )));
            assert!(controller.invariant_error().is_none());
        }
    }

    #[test]
    fn hotline_routes_immediately_without_digit_collection_and_rolls_back_every_boundary() {
        for fail_at in 0..5 {
            let mut controller = connected_outbound_controller();
            let destination = HotlineDestination::new("9911").unwrap();
            let transition = controller
                .begin_hotline_call_transaction(HotlineCallRequest {
                    handset_call_id: CallId(2),
                    binding: binding(),
                    codec: Codec::Pcma,
                    destination,
                    now: Instant::now(),
                })
                .unwrap();
            assert_eq!(transition.effects.len(), 5);
            assert!(!transition.effects.iter().any(|effect| matches!(
                effect,
                DriverEffect::Handset(HandsetEffect::StartTone {
                    call_id: CallId(2),
                    tone,
                    ..
                })
                    if *tone != Tone::Silence
            )));
            assert!(matches!(
                transition.effects.last(),
                Some(DriverEffect::Backend(PbxEffect::StartRouting {
                    call_id: PbxCallId(2),
                    context,
                    destination,
                })) if context == "from-sccp" && destination == "9911"
            ));
            let call = controller.pbx_call(PbxCallId(2)).unwrap();
            assert_eq!(call.state, CallState::Calling);
            assert_eq!(call.digits, "9911");
            assert_eq!(call.digit_deadline, None);

            let mut progress = CallTransitionProgress::default();
            for effect in transition.effects.iter().take(fail_at) {
                progress.record_success(&transition, effect);
                assert!(controller.record_call_transition_success(transition.id, effect));
            }
            let cleanup = controller.abort_call_transition(transition.id, &progress);
            assert_eq!(
                controller.call(CallId(1)).unwrap().state,
                CallState::Connected
            );
            assert!(controller.call(CallId(2)).is_none());
            assert_eq!(
                cleanup.iter().any(|effect| matches!(
                    effect,
                    DriverEffect::Backend(PbxEffect::Hangup {
                        call_id: PbxCallId(2)
                    })
                )),
                fail_at > 2,
                "failed at {fail_at}"
            );
            assert!(controller.invariant_error().is_none());
        }
    }

    #[test]
    fn hotline_routing_completed_after_disconnect_is_compensated_exactly_once() {
        let mut controller = connected_outbound_controller();
        let device = binding().device_id;
        let transition = controller
            .begin_hotline_call_transaction(HotlineCallRequest {
                handset_call_id: CallId(2),
                binding: binding(),
                codec: Codec::Pcma,
                destination: HotlineDestination::new("9911").unwrap(),
                now: Instant::now(),
            })
            .unwrap();
        for effect in transition.effects.iter().take(transition.effects.len() - 1) {
            assert!(controller.record_call_transition_success(transition.id, effect));
        }
        controller.disconnected(&device);
        let completed = transition.effects.last().unwrap();
        assert!(!controller.record_call_transition_success(transition.id, completed));
        let compensation =
            controller.compensate_unrecorded_call_transition_effect(&transition, completed);
        assert!(compensation.remove_target_channel);
        assert_eq!(
            compensation
                .effects
                .iter()
                .filter(|effect| matches!(
                    effect,
                    DriverEffect::Backend(PbxEffect::Hangup {
                        call_id: PbxCallId(2)
                    })
                ))
                .count(),
            1
        );
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn active_switch_transaction_rolls_back_answer_and_preserves_unrelated_offer() {
        for fail_at in 0..=3 {
            let mut controller = connected_outbound_controller();
            let device = binding().device_id;
            controller.offer_inbound_call(
                PbxCallId(8),
                [InboundAppearance {
                    call_id: CallId(2),
                    binding: binding(),
                    codec: Codec::Pcma,
                }],
            );
            let transition = controller
                .begin_active_call_switch_transaction(&device, CallId(2))
                .unwrap();
            let mut progress = CallTransitionProgress::default();
            for effect in transition.effects.iter().take(fail_at) {
                progress.record_success(&transition, effect);
                assert!(controller.record_call_transition_success(transition.id, effect));
            }
            if fail_at == 2 {
                controller.offer_inbound_call(
                    PbxCallId(20),
                    [InboundAppearance {
                        call_id: CallId(20),
                        binding: binding(),
                        codec: Codec::Pcmu,
                    }],
                );
            }
            let cleanup = controller.abort_call_transition(transition.id, &progress);
            assert_eq!(
                controller.call(CallId(1)).unwrap().state,
                CallState::Connected
            );
            assert_eq!(
                controller.registered_device(&device).unwrap().active_call(),
                Some(CallId(1))
            );
            assert_eq!(
                controller.call(CallId(2)).unwrap().state,
                CallState::Ringing
            );
            assert!(cleanup.iter().all(|effect| !matches!(
                effect,
                DriverEffect::Backend(PbxEffect::Hangup {
                    call_id: PbxCallId(8)
                })
            )));
            if fail_at == 2 {
                assert!(controller.call(CallId(20)).is_some());
            }
            assert!(controller.invariant_error().is_none());
        }
    }

    #[test]
    fn successful_call_transition_commit_rejects_late_abort() {
        let mut controller = connected_outbound_controller();
        let transition = controller
            .begin_additional_phone_call_transaction(
                CallId(2),
                binding(),
                Codec::Pcma,
                Instant::now(),
            )
            .unwrap();
        assert!(controller.commit_call_transition(transition.id));
        assert!(
            controller
                .abort_call_transition(transition.id, &CallTransitionProgress::default())
                .is_empty()
        );
        assert_eq!(controller.call(CallId(1)).unwrap().state, CallState::Held);
        assert_eq!(
            controller.call(CallId(2)).unwrap().state,
            CallState::Collecting
        );
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn call_transition_pbx_hangup_races_abort_without_resurrection() {
        for hung_up in [PbxCallId(1), PbxCallId(2)] {
            let mut controller = connected_outbound_controller();
            let transition = controller
                .begin_additional_phone_call_transaction(
                    CallId(2),
                    binding(),
                    Codec::Pcma,
                    Instant::now(),
                )
                .unwrap();
            let mut progress = CallTransitionProgress::default();
            for effect in transition.effects.iter().take(3) {
                progress.record_success(&transition, effect);
                assert!(controller.record_call_transition_success(transition.id, effect));
            }

            let outcome = controller
                .pbx_hangup_with_effects(hung_up)
                .expect("the racing PBX hangup is claimed");
            assert!(controller.call_by_pbx(hung_up).is_none());
            assert!(
                controller
                    .abort_call_transition(transition.id, &progress)
                    .is_empty()
            );
            assert!(!outcome.effects.iter().any(|effect| matches!(
                effect,
                DriverEffect::Backend(
                    PbxEffect::Hold { call_id }
                        | PbxEffect::Resume { call_id }
                        | PbxEffect::Answer { call_id }
                        | PbxEffect::Hangup { call_id }
                ) if *call_id == hung_up
            )));
            if hung_up == PbxCallId(2) {
                assert_eq!(
                    controller.call(CallId(1)).unwrap().state,
                    CallState::Connected
                );
            } else {
                assert!(controller.call(CallId(1)).is_none());
                assert!(controller.call(CallId(2)).is_none());
                assert!(outcome.effects.iter().any(|effect| matches!(
                    effect,
                    DriverEffect::Backend(PbxEffect::Hangup {
                        call_id: PbxCallId(2)
                    })
                )));
            }
            assert!(controller.invariant_error().is_none());
        }
    }

    #[test]
    fn additional_call_compensates_each_effect_completed_after_cancellation() {
        for completed_at in 0..3 {
            let mut controller = connected_outbound_controller();
            let transition = controller
                .begin_additional_phone_call_transaction(
                    CallId(2),
                    binding(),
                    Codec::Pcma,
                    Instant::now(),
                )
                .unwrap();
            for effect in transition.effects.iter().take(completed_at) {
                assert!(controller.record_call_transition_success(transition.id, effect));
            }

            controller
                .pbx_hangup_with_effects(PbxCallId(1))
                .expect("previous-leg hangup cancels the transition");
            let completed = transition.effects[completed_at].clone();
            assert!(!controller.record_call_transition_success(transition.id, &completed));
            let compensation =
                controller.compensate_unrecorded_call_transition_effect(&transition, &completed);

            assert!(controller.call(CallId(1)).is_none());
            assert!(controller.call(CallId(2)).is_none());
            assert_eq!(
                compensation.remove_target_channel,
                completed_at == 2,
                "completed effect {completed_at}"
            );
            assert_eq!(
                compensation.effects.iter().any(|effect| matches!(
                    effect,
                    DriverEffect::Backend(PbxEffect::Hangup {
                        call_id: PbxCallId(2)
                    })
                )),
                completed_at == 2,
                "completed effect {completed_at}"
            );
            assert!(!compensation.effects.iter().any(|effect| matches!(
                effect,
                DriverEffect::Backend(
                    PbxEffect::Hold { call_id }
                        | PbxEffect::Resume { call_id }
                        | PbxEffect::Answer { call_id }
                        | PbxEffect::Hangup { call_id }
                ) if *call_id == PbxCallId(1)
            )));
            assert!(controller.invariant_error().is_none());
        }
    }

    #[test]
    fn active_switch_compensates_each_effect_completed_after_target_hangup() {
        for completed_at in 0..3 {
            let mut controller = connected_outbound_controller();
            let device = binding().device_id;
            controller.offer_inbound_call(
                PbxCallId(8),
                [InboundAppearance {
                    call_id: CallId(2),
                    binding: binding(),
                    codec: Codec::Pcma,
                }],
            );
            let transition = controller
                .begin_active_call_switch_transaction(&device, CallId(2))
                .unwrap();
            for effect in transition.effects.iter().take(completed_at) {
                assert!(controller.record_call_transition_success(transition.id, effect));
            }

            controller
                .pbx_hangup_with_effects(PbxCallId(8))
                .expect("target-leg hangup cancels the transition");
            let completed = transition.effects[completed_at].clone();
            assert!(!controller.record_call_transition_success(transition.id, &completed));
            let compensation =
                controller.compensate_unrecorded_call_transition_effect(&transition, &completed);

            assert_eq!(
                controller.call(CallId(1)).unwrap().state,
                CallState::Connected
            );
            assert!(controller.call(CallId(2)).is_none());
            assert!(!compensation.remove_target_channel);
            assert!(!compensation.effects.iter().any(|effect| matches!(
                effect,
                DriverEffect::Backend(
                    PbxEffect::Hold { call_id }
                        | PbxEffect::Resume { call_id }
                        | PbxEffect::Answer { call_id }
                        | PbxEffect::Hangup { call_id }
                ) if *call_id == PbxCallId(8)
            )));
            assert_eq!(
                compensation.effects.iter().any(|effect| matches!(
                    effect,
                    DriverEffect::Backend(PbxEffect::Resume {
                        call_id: PbxCallId(1)
                    })
                )),
                completed_at == 0,
                "completed effect {completed_at}"
            );
            assert!(controller.invariant_error().is_none());
        }
    }

    #[test]
    fn active_switch_compensates_each_effect_completed_after_previous_hangup() {
        for completed_at in 0..3 {
            let mut controller = connected_outbound_controller();
            let device = binding().device_id;
            controller.offer_inbound_call(
                PbxCallId(8),
                [InboundAppearance {
                    call_id: CallId(2),
                    binding: binding(),
                    codec: Codec::Pcma,
                }],
            );
            let transition = controller
                .begin_active_call_switch_transaction(&device, CallId(2))
                .unwrap();
            for effect in transition.effects.iter().take(completed_at) {
                assert!(controller.record_call_transition_success(transition.id, effect));
            }

            controller
                .pbx_hangup_with_effects(PbxCallId(1))
                .expect("previous-leg hangup cancels the transition");
            let completed = transition.effects[completed_at].clone();
            assert!(!controller.record_call_transition_success(transition.id, &completed));
            let compensation =
                controller.compensate_unrecorded_call_transition_effect(&transition, &completed);

            assert!(controller.call(CallId(1)).is_none());
            assert!(!compensation.remove_target_channel);
            assert!(compensation.effects.iter().all(|effect| !matches!(
                effect,
                DriverEffect::Backend(PbxEffect::Hangup {
                    call_id: PbxCallId(8)
                })
            )));
            assert_eq!(
                controller.call(CallId(2)).unwrap().state,
                CallState::Ringing
            );
            assert!(!compensation.effects.iter().any(|effect| matches!(
                effect,
                DriverEffect::Backend(
                    PbxEffect::Hold { call_id }
                        | PbxEffect::Resume { call_id }
                        | PbxEffect::Answer { call_id }
                        | PbxEffect::Hangup { call_id }
                ) if *call_id == PbxCallId(1)
            )));
            assert!(controller.invariant_error().is_none());
        }
    }

    #[test]
    fn held_switch_never_discards_the_existing_target_channel_on_abort() {
        let mut controller = connected_outbound_controller();
        let device = binding().device_id;
        controller
            .begin_additional_phone_call(CallId(2), binding(), Codec::Pcma, Instant::now())
            .unwrap();
        controller.enbloc(CallId(2), "2200".into());
        controller.pbx_answer(PbxCallId(2));
        let transition = controller
            .begin_active_call_switch_transaction(&device, CallId(1))
            .unwrap();
        let mut progress = CallTransitionProgress::default();
        for effect in transition.effects.iter().take(3) {
            progress.record_success(&transition, effect);
            assert!(controller.record_call_transition_success(transition.id, effect));
        }

        assert!(!transition.remove_target_channel_on_abort(&progress));
        let cleanup = controller.abort_call_transition(transition.id, &progress);
        assert!(cleanup.iter().any(|effect| matches!(
            effect,
            DriverEffect::Backend(PbxEffect::Hold {
                call_id: PbxCallId(1)
            })
        )));
        assert!(controller.call(CallId(1)).is_some());
        assert!(controller.call(CallId(2)).is_some());
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn disconnect_invalidates_pending_transition_and_late_abort_is_idempotent() {
        let mut controller = connected_outbound_controller();
        let device = binding().device_id;
        let transition = controller
            .begin_additional_phone_call_transaction(
                CallId(2),
                binding(),
                Codec::Pcma,
                Instant::now(),
            )
            .unwrap();
        let cleanup = controller.disconnected(&device);
        assert_eq!(
            cleanup
                .iter()
                .filter(|effect| matches!(effect, DriverEffect::Backend(PbxEffect::Hangup { .. })))
                .count(),
            2
        );
        assert!(
            controller
                .abort_call_transition(transition.id, &CallTransitionProgress::default())
                .is_empty()
        );
        assert!(controller.call(CallId(1)).is_none());
        assert!(controller.call(CallId(2)).is_none());
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn disconnect_compensates_each_effect_completed_after_transition_cancellation() {
        for completed_at in 0..3 {
            let mut controller = connected_outbound_controller();
            let device = binding().device_id;
            let transition = controller
                .begin_additional_phone_call_transaction(
                    CallId(2),
                    binding(),
                    Codec::Pcma,
                    Instant::now(),
                )
                .unwrap();
            for effect in transition.effects.iter().take(completed_at) {
                assert!(controller.record_call_transition_success(transition.id, effect));
            }

            controller.disconnected(&device);
            let completed = transition.effects[completed_at].clone();
            assert!(!controller.record_call_transition_success(transition.id, &completed));
            let compensation =
                controller.compensate_unrecorded_call_transition_effect(&transition, &completed);

            assert_eq!(
                compensation.remove_target_channel,
                completed_at == 2,
                "completed effect {completed_at}"
            );
            assert!(!compensation.effects.iter().any(|effect| matches!(
                effect,
                DriverEffect::Backend(
                    PbxEffect::Hold { call_id }
                        | PbxEffect::Resume { call_id }
                        | PbxEffect::Answer { call_id }
                        | PbxEffect::Hangup { call_id }
                ) if *call_id == PbxCallId(1)
            )));
            assert!(controller.call(CallId(1)).is_none());
            assert!(controller.call(CallId(2)).is_none());
            assert!(controller.invariant_error().is_none());
        }
    }

    #[test]
    fn device_features_are_independent_of_registration_and_calls() {
        let mut controller = Controller::new(Duration::from_secs(1));
        let device = binding().device_id;
        controller.set_dnd(&device, DndMode::Silent);
        controller.set_privacy(&device, true);
        controller.set_forwarding(
            &device,
            ForwardingState {
                all: Some(forwarding("2000")),
                busy: None,
                no_answer: Some(forwarding("2001")),
            },
        );
        controller.set_feature_button(&device, 4, true);

        controller.registered(registration());
        controller.begin_asterisk_call(CallId(2), PbxCallId(8), &binding(), Codec::Pcma);
        controller.disconnected(&device);

        assert_eq!(
            controller.feature_state(&device),
            Some(&DeviceFeatureState {
                dnd: DndMode::Silent,
                privacy: true,
                forwarding: ForwardingState {
                    all: Some(forwarding("2000")),
                    busy: None,
                    no_answer: Some(forwarding("2001")),
                },
                buttons: HashMap::from([(4, true)]),
            })
        );
    }

    #[test]
    fn complete_feature_reload_candidate_removes_stale_device_state() {
        let mut controller = Controller::new(Duration::from_secs(1));
        let removed = DeviceId::new("SEP001122334455").unwrap();
        let retained = DeviceId::new("SEP112233445566").unwrap();
        controller.set_dnd(&removed, DndMode::Reject);
        controller.set_privacy(&retained, true);
        let retained_state = controller.feature_state(&retained).unwrap().clone();

        controller
            .replace_feature_states(HashMap::from([(retained.clone(), retained_state.clone())]));

        assert_eq!(controller.feature_state(&removed), None);
        assert_eq!(controller.feature_state(&retained), Some(&retained_state));
    }

    #[test]
    fn audio_acknowledgements_cannot_mutate_typed_video_state() {
        let mut controller = Controller::new(Duration::from_secs(1));
        controller.begin_asterisk_call(CallId(2), PbxCallId(8), &binding(), Codec::Pcma);
        let audio = MediaEndpoint {
            address: "192.0.2.20".parse().unwrap(),
            rtp_port: 20000,
            rtcp_port: 20001,
            codec: Codec::Pcma,
            packet_ms: 20,
            max_frames_per_packet: 1,
            telephone_event_payload: 101,
        };
        let video = MediaEndpoint {
            address: "192.0.2.20".parse().unwrap(),
            rtp_port: 21000,
            rtcp_port: 21001,
            codec: Codec::H264,
            packet_ms: 0,
            max_frames_per_packet: 1,
            telephone_event_payload: 0,
        };

        controller.media_opened(CallId(2), audio);
        controller.media_opened(CallId(2), video);

        let call = controller.call(CallId(2)).unwrap();
        assert_eq!(call.audio, MediaStreamState::Open(audio));
        assert_eq!(
            call.video,
            VideoMediaState::AudioOnly(VideoFallbackReason::NotNegotiated)
        );
    }

    #[test]
    fn video_lifecycle_requires_the_current_session_and_owned_codec() {
        let mut controller = connected_outbound_controller();
        let device_id = binding().device_id;
        let plan = test_video_plan(&controller, VideoMode::User);
        let session_generation = plan.session_generation;
        let stale_generation = SessionGeneration::new(session_generation.get() + 1).unwrap();
        assert!(controller.install_video_plan_for_device(
            &device_id,
            CallId(1),
            plan,
            VideoPlanReadiness::Ready,
        ));

        assert_eq!(
            controller.video_mode_for_device(&device_id, CallId(1)),
            [DriverEffect::Handset(HandsetEffect::OpenVideoReceive {
                device_id: device_id.clone(),
                call_id: CallId(1),
                session_generation,
            })]
        );
        assert!(!controller.video_receive_opened_for_device(
            &device_id,
            stale_generation,
            CallId(1),
            Codec::H264,
            test_video_endpoint(30_002),
        ));
        assert!(!controller.video_receive_opened_for_device(
            &device_id,
            session_generation,
            CallId(1),
            Codec::H263,
            test_video_endpoint(30_002),
        ));
        assert!(controller.video_receive_opened_for_device(
            &device_id,
            session_generation,
            CallId(1),
            Codec::H264,
            test_video_endpoint(30_002),
        ));
        assert_eq!(
            controller.begin_video_transmit_for_device(&device_id, session_generation, CallId(1),),
            [DriverEffect::Handset(HandsetEffect::StartVideoTransmit {
                device_id: device_id.clone(),
                call_id: CallId(1),
                session_generation,
            })]
        );
        assert!(controller.video_transmit_opened_for_device(
            &device_id,
            session_generation,
            CallId(1),
            Codec::H264,
            test_video_endpoint(30_004),
            PassthroughPartyId::new(41),
        ));
        let pbx_id = controller.call(CallId(1)).unwrap().pbx_id;
        assert_eq!(
            controller.refresh_video_for_pbx(pbx_id),
            [DriverEffect::Handset(HandsetEffect::RefreshVideo {
                device_id: device_id.clone(),
                call_id: CallId(1),
                session_generation,
                passthrough_party_id: PassthroughPartyId::new(41),
            })]
        );
        assert!(!controller.video_refresh_is_current(
            &device_id,
            session_generation,
            CallId(1),
            PassthroughPartyId::new(42),
        ));
        assert_eq!(
            controller.call(CallId(1)).unwrap().video,
            VideoMediaState::Ready {
                plan: test_video_plan(&controller, VideoMode::User),
                receive: VideoStreamState::Open {
                    codec: Codec::H264,
                    endpoint: test_video_endpoint(30_002),
                },
                transmit: VideoStreamState::Open {
                    codec: Codec::H264,
                    endpoint: test_video_endpoint(30_004),
                },
                transmit_token: Some(PassthroughPartyId::new(41)),
            }
        );
        assert_eq!(
            controller.video_mode_for_device(&device_id, CallId(1)),
            [DriverEffect::Handset(HandsetEffect::StopVideo {
                device_id,
                call_id: CallId(1),
                session_generation,
            })]
        );
        let state = &controller.call(CallId(1)).unwrap().video;
        assert_eq!(state.receive(), VideoStreamState::Closed);
        assert_eq!(state.transmit(), VideoStreamState::Closed);
        assert!(controller.refresh_video_for_pbx(pbx_id).is_empty());
    }

    #[test]
    fn automatic_video_starts_only_after_audio_for_the_active_connected_call() {
        let mut controller = connected_outbound_controller();
        let device_id = binding().device_id;
        let plan = test_video_plan(&controller, VideoMode::Auto);
        let session_generation = plan.session_generation;
        assert!(controller.install_video_plan_for_device(
            &device_id,
            CallId(1),
            plan,
            VideoPlanReadiness::Ready,
        ));
        assert_eq!(
            controller.call(CallId(1)).unwrap().video.receive(),
            VideoStreamState::Closed
        );

        let effects = controller.media_opened(CallId(1), test_media_endpoint(Codec::Pcmu));
        assert!(
            effects.contains(&DriverEffect::Handset(HandsetEffect::OpenVideoReceive {
                device_id: device_id.clone(),
                call_id: CallId(1),
                session_generation,
            }))
        );
        let video = &controller.call(CallId(1)).unwrap().video;
        assert_eq!(video.receive(), VideoStreamState::Opening);
        assert_eq!(video.transmit(), VideoStreamState::Closed);
        assert!(
            controller
                .video_mode_for_device(&device_id, CallId(1))
                .is_empty()
        );
    }

    #[test]
    fn user_video_rejects_foreign_calls_and_stale_pending_effects() {
        let mut controller = connected_outbound_controller();
        let device_id = binding().device_id;
        let plan = test_video_plan(&controller, VideoMode::User);
        let session_generation = plan.session_generation;
        assert!(controller.install_video_plan_for_device(
            &device_id,
            CallId(1),
            plan,
            VideoPlanReadiness::Ready,
        ));
        assert!(
            controller
                .video_mode_for_device(&device_id, CallId(99))
                .is_empty()
        );

        let open = HandsetEffect::OpenVideoReceive {
            device_id: device_id.clone(),
            call_id: CallId(1),
            session_generation,
        };
        assert_eq!(
            controller.video_mode_for_device(&device_id, CallId(1)),
            [DriverEffect::Handset(open.clone())]
        );
        assert!(
            controller
                .opening_video_receive_plan_for_device(&device_id, session_generation, CallId(1),)
                .is_some()
        );
        assert!(matches!(
            controller
                .video_mode_for_device(&device_id, CallId(1))
                .as_slice(),
            [DriverEffect::Handset(HandsetEffect::StopVideo { .. })]
        ));
        assert!(
            controller
                .opening_video_receive_plan_for_device(&device_id, session_generation, CallId(1),)
                .is_none()
        );
        assert_eq!(
            controller.recover_optional_video_effect_failure(&open),
            Some(Vec::new())
        );
        assert!(matches!(
            controller.call(CallId(1)).unwrap().video,
            VideoMediaState::Ready { .. }
        ));
    }

    #[test]
    fn hold_closes_generation_owned_video_before_changing_presentation() {
        let mut controller = connected_outbound_controller();
        let device_id = binding().device_id;
        let plan = test_video_plan(&controller, VideoMode::User);
        let session_generation = plan.session_generation;
        assert!(controller.install_video_plan_for_device(
            &device_id,
            CallId(1),
            plan,
            VideoPlanReadiness::Ready,
        ));
        assert!(
            !controller
                .video_mode_for_device(&device_id, CallId(1))
                .is_empty()
        );

        let effects = controller.hold(CallId(1));
        assert!(
            effects.contains(&DriverEffect::Handset(HandsetEffect::StopVideo {
                device_id,
                call_id: CallId(1),
                session_generation,
            }))
        );
        let call = controller.call(CallId(1)).unwrap();
        assert_eq!(call.state, CallState::Held);
        assert_eq!(call.video.receive(), VideoStreamState::Closed);
        assert_eq!(call.video.transmit(), VideoStreamState::Closed);
    }

    #[test]
    fn optional_video_failure_falls_back_without_terminating_audio() {
        let mut controller = connected_outbound_controller();
        let device_id = binding().device_id;
        let audio = test_media_endpoint(Codec::Pcmu);
        controller.media_opened(CallId(1), audio);
        let plan = test_video_plan(&controller, VideoMode::User);
        let session_generation = plan.session_generation;
        assert!(controller.install_video_plan_for_device(
            &device_id,
            CallId(1),
            plan,
            VideoPlanReadiness::Ready,
        ));
        assert!(
            !controller
                .video_mode_for_device(&device_id, CallId(1))
                .is_empty()
        );

        let begin = HandsetEffect::OpenVideoReceive {
            device_id: device_id.clone(),
            call_id: CallId(1),
            session_generation,
        };
        let stop = HandsetEffect::StopVideo {
            device_id: device_id.clone(),
            call_id: CallId(1),
            session_generation,
        };
        assert_eq!(
            controller.recover_optional_video_effect_failure(&begin),
            Some(vec![DriverEffect::Handset(HandsetEffect::StopVideo {
                device_id: device_id.clone(),
                call_id: CallId(1),
                session_generation,
            })])
        );
        let call = controller.call(CallId(1)).unwrap();
        assert_eq!(call.audio, MediaStreamState::Open(audio));
        assert_eq!(call.pbx_id, PbxCallId(1));
        assert_eq!(
            call.video,
            VideoMediaState::AudioOnly(VideoFallbackReason::ReceiveFailed)
        );
        assert!(controller.pbx_call(PbxCallId(1)).is_some());
        assert_eq!(
            controller.recover_optional_video_effect_failure(&stop),
            Some(Vec::new())
        );
        assert_eq!(
            controller.recover_optional_video_effect_failure(&HandsetEffect::StartTone {
                device_id,
                call_id: CallId(1),
                tone: Tone::Silence,
            }),
            None
        );
        assert!(controller.call(CallId(1)).is_some());
    }

    #[test]
    fn blocked_video_mode_is_an_audio_only_noop() {
        let mut controller = connected_outbound_controller();
        let device_id = binding().device_id;
        let plan = test_video_plan(&controller, VideoMode::User);
        assert!(controller.install_video_plan_for_device(
            &device_id,
            CallId(1),
            plan.clone(),
            VideoPlanReadiness::Blocked(VideoFallbackReason::DescriptorUnavailable),
        ));

        assert!(
            controller
                .video_mode_for_device(&device_id, CallId(1))
                .is_empty()
        );
        assert_eq!(
            controller.call(CallId(1)).unwrap().video,
            VideoMediaState::Blocked {
                plan,
                reason: VideoFallbackReason::DescriptorUnavailable,
            }
        );
    }

    #[test]
    fn direct_transfer_pairs_exact_selected_held_and_connected_calls() {
        let mut controller = Controller::new(Duration::from_secs(1));
        controller.registered(registration());
        controller.begin_asterisk_call(CallId(2), PbxCallId(8), &binding(), Codec::Pcma);
        controller.phone_answer(CallId(2));
        controller.media_opened(CallId(2), test_media_endpoint(Codec::Pcma));
        controller.hold(CallId(2));
        controller.begin_asterisk_call(CallId(3), PbxCallId(9), &binding(), Codec::Pcma);
        controller.phone_answer(CallId(3));
        controller.media_opened(CallId(3), test_media_endpoint(Codec::Pcma));
        controller.set_call_selected(&binding().device_id, CallId(2), true);
        controller.set_call_selected(&binding().device_id, CallId(3), true);
        let plan = controller.direct_transfer(&binding().device_id).unwrap();
        assert_eq!(
            plan.effects,
            [DriverEffect::Backend(PbxEffect::Transfer {
                operation: plan.completion.clone(),
            })]
        );
        assert_eq!(plan.completion.source.pbx_call_id, PbxCallId(8));
        assert_eq!(plan.completion.consultation.pbx_call_id, PbxCallId(9));
        assert_eq!(
            plan.completion.kind,
            crate::call::transfer::TransferCompletionKind::Direct
        );
    }

    fn begin_test_transfer(
        controller: &mut Controller,
        complete_on_hangup: bool,
    ) -> (TransferId, Vec<DriverEffect>) {
        let effects = controller
            .begin_transfer(TransferConsultationRequest {
                source_call_id: CallId(1),
                consultation_call_id: CallId(2),
                binding: binding(),
                codec: Codec::Pcmu,
                complete_on_hangup,
                now: Instant::now(),
            })
            .unwrap();
        let transaction_id = controller.transfer_transaction(CallId(1)).unwrap().id;
        (transaction_id, effects)
    }

    fn record_test_transfer_progress(
        controller: &mut Controller,
        device_id: &DeviceId,
        transaction_id: TransferId,
        progress: &TransferExecutionProgress,
    ) {
        for milestone in [
            TransferSetupMilestone::SourceBackendHeld,
            TransferSetupMilestone::SourceHandsetHeld,
            TransferSetupMilestone::ConsultationChannelCreated,
            TransferSetupMilestone::ConsultationHandsetStarted,
        ] {
            if progress.completed(milestone) {
                controller
                    .transfer_setup_completed(device_id, transaction_id, milestone)
                    .unwrap();
            }
        }
    }

    fn completed_transfer_progress() -> TransferExecutionProgress {
        TransferExecutionProgress::with_completed([
            TransferSetupMilestone::SourceBackendHeld,
            TransferSetupMilestone::SourceHandsetHeld,
            TransferSetupMilestone::ConsultationChannelCreated,
            TransferSetupMilestone::ConsultationHandsetStarted,
        ])
    }

    #[test]
    fn consultation_transfer_keeps_distinct_identities_and_exact_setup_order() {
        let mut controller = connected_outbound_controller();
        let (_, effects) = begin_test_transfer(&mut controller, true);
        assert!(matches!(
            effects.as_slice(),
            [
                DriverEffect::Backend(PbxEffect::Hold {
                    call_id: PbxCallId(1)
                }),
                DriverEffect::Handset(HandsetEffect::SetCallState {
                    call_id: CallId(1),
                    state: HandsetCallState::Hold,
                    stop_media: true,
                    ..
                }),
                DriverEffect::Backend(PbxEffect::CreateConsultationChannel {
                    source_call_id: PbxCallId(1),
                    handset_call_id: CallId(2),
                    call_id: PbxCallId(2),
                    ..
                }),
                DriverEffect::Handset(HandsetEffect::BeginTransfer {
                    source_call_id: CallId(1),
                    consultation_call_id: CallId(2),
                    ..
                }),
            ]
        ));
        let transaction = controller.transfer_transaction(CallId(2)).unwrap();
        assert_eq!(transaction.source.handset_call_id, CallId(1));
        assert_eq!(transaction.source.pbx_call_id, PbxCallId(1));
        assert_eq!(transaction.consultation.unwrap().handset_call_id, CallId(2));
        assert!(transaction.complete_on_hangup);
        assert_eq!(controller.call(CallId(1)).unwrap().state, CallState::Held);
        assert_eq!(
            controller.call(CallId(2)).unwrap().state,
            CallState::TransferCollecting
        );
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn consultation_setup_failure_rolls_back_only_completed_effects_once() {
        for progress in [
            TransferExecutionProgress::default(),
            TransferExecutionProgress::with_completed([TransferSetupMilestone::SourceBackendHeld]),
            completed_transfer_progress(),
        ] {
            let mut controller = connected_outbound_controller();
            let device_id = binding().device_id;
            let (transaction_id, _) = begin_test_transfer(&mut controller, false);
            record_test_transfer_progress(&mut controller, &device_id, transaction_id, &progress);
            let outcome = controller
                .abort_transfer(
                    &device_id,
                    transaction_id,
                    TransferCancellationReason::ConsultationFailure,
                )
                .unwrap();
            assert_eq!(
                outcome.effects.iter().any(|effect| matches!(
                    effect,
                    DriverEffect::Backend(PbxEffect::Resume {
                        call_id: PbxCallId(1)
                    })
                )),
                progress.completed(TransferSetupMilestone::SourceBackendHeld)
            );
            assert_eq!(
                outcome.effects.iter().any(|effect| matches!(
                    effect,
                    DriverEffect::Backend(PbxEffect::Hangup {
                        call_id: PbxCallId(2)
                    })
                )),
                progress.completed(TransferSetupMilestone::ConsultationChannelCreated)
            );
            assert_eq!(
                outcome.effects.iter().any(|effect| matches!(
                    effect,
                    DriverEffect::Handset(HandsetEffect::SetCallState {
                        call_id: CallId(2),
                        state: HandsetCallState::OnHook,
                        ..
                    })
                )),
                progress.completed(TransferSetupMilestone::ConsultationHandsetStarted)
            );
            assert_eq!(
                controller.call(CallId(1)).unwrap().state,
                CallState::Connected
            );
            assert!(controller.call(CallId(2)).is_none());
            assert!(controller.transfer_transaction(CallId(1)).is_none());
            assert_eq!(
                controller.abort_transfer(
                    &device_id,
                    transaction_id,
                    TransferCancellationReason::ConsultationFailure,
                ),
                Err(TransferRejection::Conflict)
            );
            assert!(controller.invariant_error().is_none());
        }
    }

    #[test]
    fn blind_transfer_commits_only_after_exact_backend_completion_identity() {
        let mut controller = connected_outbound_controller();
        let device_id = binding().device_id;
        let (transaction_id, _) = begin_test_transfer(&mut controller, false);
        let consultation = TransferLeg {
            handset_call_id: CallId(2),
            pbx_call_id: PbxCallId(2),
        };
        controller
            .transfers
            .get_mut(&device_id)
            .unwrap()
            .advance_consultation(consultation, TransferPhase::Routing)
            .unwrap();
        controller
            .transfers
            .get_mut(&device_id)
            .unwrap()
            .advance_consultation(consultation, TransferPhase::Ringing)
            .unwrap();
        let plan = controller
            .complete_transfer(&device_id, CallId(2), TransferTrigger::TransferKey)
            .unwrap();
        assert_eq!(plan.completion.transaction_id, transaction_id);
        assert_eq!(
            plan.completion.kind,
            crate::call::transfer::TransferCompletionKind::Blind
        );
        assert!(controller.call(CallId(1)).is_some());
        assert!(controller.call(CallId(2)).is_some());
        assert!(
            controller
                .transfer_succeeded(&device_id, TransferId(transaction_id.0 + 1))
                .is_none()
        );

        let outcome = controller
            .transfer_succeeded(&device_id, transaction_id)
            .unwrap();
        assert_eq!(outcome.effects.len(), 2);
        assert!(outcome.effects.iter().all(|effect| matches!(
            effect,
            DriverEffect::Handset(HandsetEffect::SetCallState {
                state: HandsetCallState::OnHook,
                stop_media: true,
                ..
            })
        )));
        assert!(controller.call(CallId(1)).is_none());
        assert!(controller.call(CallId(2)).is_none());
        assert!(
            controller
                .transfer_succeeded(&device_id, transaction_id)
                .is_none()
        );
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn device_transfer_completion_accepts_either_leg_or_an_omitted_reference() {
        for reported in [Some(CallId(1)), Some(CallId(2)), Some(CallId(0)), None] {
            let mut controller = connected_outbound_controller();
            let device_id = binding().device_id;
            let (transaction_id, _) = begin_test_transfer(&mut controller, false);
            let consultation = TransferLeg {
                handset_call_id: CallId(2),
                pbx_call_id: PbxCallId(2),
            };
            let transaction = controller.transfers.get_mut(&device_id).unwrap();
            transaction
                .advance_consultation(consultation, TransferPhase::Routing)
                .unwrap();
            transaction
                .advance_consultation(consultation, TransferPhase::Ringing)
                .unwrap();

            let plan = controller
                .complete_device_transfer(&device_id, reported, TransferTrigger::TransferKey)
                .unwrap();
            assert_eq!(plan.completion.transaction_id, transaction_id);
            assert_eq!(plan.completion.consultation, consultation);
        }

        let mut controller = connected_outbound_controller();
        let device_id = binding().device_id;
        let (_, _) = begin_test_transfer(&mut controller, false);
        assert_eq!(
            controller.complete_device_transfer(
                &device_id,
                Some(CallId(99)),
                TransferTrigger::TransferKey,
            ),
            Err(TransferRejection::WrongCall)
        );
        assert_eq!(
            controller
                .transfer_transaction_for_device(&device_id)
                .unwrap()
                .phase,
            TransferPhase::Collecting
        );
    }

    #[test]
    fn transfer_destination_progress_and_answer_choose_blind_then_attended_kind() {
        let device_id = binding().device_id;

        let mut blind = connected_outbound_controller();
        begin_test_transfer(&mut blind, false);
        assert_outbound_route(&blind.enbloc(CallId(2), "2200".into()), "2200");
        assert_eq!(
            blind.transfer_transaction(CallId(2)).unwrap().phase,
            TransferPhase::Routing
        );
        blind.pbx_progress(PbxCallId(2), false);
        assert_eq!(
            blind
                .complete_transfer(&device_id, CallId(2), TransferTrigger::TransferKey)
                .unwrap()
                .completion
                .kind,
            crate::call::transfer::TransferCompletionKind::Blind
        );

        let mut attended = connected_outbound_controller();
        begin_test_transfer(&mut attended, false);
        attended.enbloc(CallId(2), "2200".into());
        attended.pbx_answer(PbxCallId(2));
        assert_eq!(
            attended
                .complete_transfer(&device_id, CallId(2), TransferTrigger::TransferKey)
                .unwrap()
                .completion
                .kind,
            crate::call::transfer::TransferCompletionKind::Attended
        );
    }

    #[test]
    fn transfer_consultation_pbx_hangup_restores_source_once() {
        let mut controller = connected_outbound_controller();
        let device_id = binding().device_id;
        let (transaction_id, _) = begin_test_transfer(&mut controller, true);
        record_test_transfer_progress(
            &mut controller,
            &device_id,
            transaction_id,
            &completed_transfer_progress(),
        );
        controller.enbloc(CallId(2), "2200".into());
        let outcome = controller.pbx_hangup_with_effects(PbxCallId(2)).unwrap();
        assert_eq!(outcome.primary.unwrap().sccp_id, CallId(2));
        assert!(outcome.effects.iter().any(|effect| matches!(
            effect,
            DriverEffect::Backend(PbxEffect::Resume {
                call_id: PbxCallId(1)
            })
        )));
        assert!(
            !outcome
                .effects
                .iter()
                .any(|effect| matches!(effect, DriverEffect::Backend(PbxEffect::Transfer { .. })))
        );
        assert_eq!(
            controller.call(CallId(1)).unwrap().state,
            CallState::Connected
        );
        assert!(controller.call(CallId(2)).is_none());
        assert!(controller.transfer_transaction(CallId(1)).is_none());
        assert!(controller.pbx_hangup_with_effects(PbxCallId(2)).is_none());
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn transfer_source_pbx_hangup_cancels_consultation_without_restoring_source() {
        let mut controller = connected_outbound_controller();
        let device_id = binding().device_id;
        let (transaction_id, _) = begin_test_transfer(&mut controller, false);
        record_test_transfer_progress(
            &mut controller,
            &device_id,
            transaction_id,
            &completed_transfer_progress(),
        );
        let outcome = controller.pbx_hangup_with_effects(PbxCallId(1)).unwrap();
        assert_eq!(outcome.primary.unwrap().sccp_id, CallId(1));
        assert!(outcome.effects.iter().any(|effect| matches!(
            effect,
            DriverEffect::Backend(PbxEffect::Hangup {
                call_id: PbxCallId(2)
            })
        )));
        assert!(
            !outcome
                .effects
                .iter()
                .any(|effect| matches!(effect, DriverEffect::Backend(PbxEffect::Resume { .. })))
        );
        assert!(controller.call(CallId(1)).is_none());
        assert!(controller.call(CallId(2)).is_none());
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn transfer_completion_claim_defers_late_pbx_hangup_until_commit() {
        let mut controller = connected_outbound_controller();
        let device_id = binding().device_id;
        let (transaction_id, _) = begin_test_transfer(&mut controller, false);
        controller.enbloc(CallId(2), "2200".into());
        controller.pbx_progress(PbxCallId(2), false);
        controller
            .complete_transfer(&device_id, CallId(2), TransferTrigger::TransferKey)
            .unwrap();

        let late = controller.pbx_hangup_with_effects(PbxCallId(2)).unwrap();
        assert!(late.effects.is_empty());
        assert!(controller.call(CallId(1)).is_some());
        assert!(controller.call(CallId(2)).is_some());
        assert_eq!(
            controller.transfer_transaction(CallId(2)).unwrap().phase,
            TransferPhase::Completing
        );
        assert!(
            controller
                .transfer_transaction(CallId(2))
                .unwrap()
                .consultation_terminated
        );
        assert!(
            controller
                .transfer_succeeded(&device_id, transaction_id)
                .is_some()
        );
        assert!(controller.call(CallId(1)).is_none());
        assert!(controller.call(CallId(2)).is_none());
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn consultation_hangup_during_completion_is_not_hung_up_twice_on_backend_failure() {
        let mut controller = connected_outbound_controller();
        let device_id = binding().device_id;
        let (transaction_id, _) = begin_test_transfer(&mut controller, false);
        record_test_transfer_progress(
            &mut controller,
            &device_id,
            transaction_id,
            &completed_transfer_progress(),
        );
        controller.enbloc(CallId(2), "2200".into());
        controller.pbx_progress(PbxCallId(2), false);
        controller
            .complete_transfer(&device_id, CallId(2), TransferTrigger::TransferKey)
            .unwrap();
        assert!(
            controller
                .pbx_hangup_with_effects(PbxCallId(2))
                .unwrap()
                .effects
                .is_empty()
        );

        let outcome = controller
            .abort_transfer(
                &device_id,
                transaction_id,
                TransferCancellationReason::BackendFailure,
            )
            .unwrap();
        assert!(outcome.effects.iter().any(|effect| matches!(
            effect,
            DriverEffect::Backend(PbxEffect::Resume {
                call_id: PbxCallId(1)
            })
        )));
        assert!(!outcome.effects.iter().any(|effect| matches!(
            effect,
            DriverEffect::Backend(PbxEffect::Hangup {
                call_id: PbxCallId(2)
            })
        )));
        assert_eq!(
            controller.call(CallId(1)).unwrap().state,
            CallState::Connected
        );
        assert!(controller.call(CallId(2)).is_none());
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn source_hangup_during_completion_removes_source_on_backend_failure() {
        let mut controller = connected_outbound_controller();
        let device_id = binding().device_id;
        let (transaction_id, _) = begin_test_transfer(&mut controller, false);
        record_test_transfer_progress(
            &mut controller,
            &device_id,
            transaction_id,
            &completed_transfer_progress(),
        );
        controller.enbloc(CallId(2), "2200".into());
        controller.pbx_progress(PbxCallId(2), false);
        controller
            .complete_transfer(&device_id, CallId(2), TransferTrigger::TransferKey)
            .unwrap();
        assert!(
            controller
                .pbx_hangup_with_effects(PbxCallId(1))
                .unwrap()
                .effects
                .is_empty()
        );

        let outcome = controller
            .abort_transfer(
                &device_id,
                transaction_id,
                TransferCancellationReason::BackendFailure,
            )
            .unwrap();
        assert!(
            !outcome
                .effects
                .iter()
                .any(|effect| matches!(effect, DriverEffect::Backend(PbxEffect::Resume { .. })))
        );
        assert!(outcome.effects.iter().any(|effect| matches!(
            effect,
            DriverEffect::Backend(PbxEffect::Hangup {
                call_id: PbxCallId(2)
            })
        )));
        assert!(controller.call(CallId(1)).is_none());
        assert!(controller.call(CallId(2)).is_none());
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn transfer_on_hangup_policy_is_snapshotted_and_phase_gated() {
        let device_id = binding().device_id;

        let mut disabled = connected_outbound_controller();
        let (disabled_id, _) = begin_test_transfer(&mut disabled, false);
        disabled.enbloc(CallId(2), "2200".into());
        disabled.pbx_progress(PbxCallId(2), false);
        assert_eq!(
            disabled.complete_transfer(&device_id, CallId(2), TransferTrigger::ConsultationHangup,),
            Err(TransferRejection::HangupCompletionDisabled)
        );
        assert_eq!(
            disabled.transfer_transaction(CallId(2)).unwrap().phase,
            TransferPhase::Ringing
        );
        disabled
            .abort_transfer(
                &device_id,
                disabled_id,
                TransferCancellationReason::ConsultationHangup,
            )
            .unwrap();

        let mut ineligible = connected_outbound_controller();
        let (ineligible_id, _) = begin_test_transfer(&mut ineligible, true);
        assert_eq!(
            ineligible.complete_transfer(
                &device_id,
                CallId(2),
                TransferTrigger::ConsultationHangup,
            ),
            Err(TransferRejection::InvalidPhase)
        );
        ineligible
            .abort_transfer(
                &device_id,
                ineligible_id,
                TransferCancellationReason::ConsultationHangup,
            )
            .unwrap();

        for (answered, expected) in [
            (false, crate::call::transfer::TransferCompletionKind::Blind),
            (
                true,
                crate::call::transfer::TransferCompletionKind::Attended,
            ),
        ] {
            let mut enabled = connected_outbound_controller();
            begin_test_transfer(&mut enabled, true);
            enabled.enbloc(CallId(2), "2200".into());
            if answered {
                enabled.pbx_answer(PbxCallId(2));
            } else {
                enabled.pbx_progress(PbxCallId(2), false);
            }
            assert_eq!(
                enabled
                    .complete_transfer(&device_id, CallId(2), TransferTrigger::ConsultationHangup,)
                    .unwrap()
                    .completion
                    .kind,
                expected
            );
        }
    }

    #[test]
    fn transfer_end_call_and_source_resume_cancel_and_restore_source() {
        for reason in [
            TransferCancellationReason::EndCall,
            TransferCancellationReason::SourceResume,
        ] {
            let mut controller = connected_outbound_controller();
            let device_id = binding().device_id;
            let (transaction_id, _) = begin_test_transfer(&mut controller, true);
            controller.enbloc(CallId(2), "2200".into());
            record_test_transfer_progress(
                &mut controller,
                &device_id,
                transaction_id,
                &completed_transfer_progress(),
            );
            let outcome = controller
                .abort_transfer(&device_id, transaction_id, reason)
                .unwrap();
            assert!(outcome.effects.iter().any(|effect| matches!(
                effect,
                DriverEffect::Backend(PbxEffect::Resume {
                    call_id: PbxCallId(1)
                })
            )));
            assert!(outcome.effects.iter().any(|effect| matches!(
                effect,
                DriverEffect::Backend(PbxEffect::Hangup {
                    call_id: PbxCallId(2)
                })
            )));
            assert_eq!(
                controller.call(CallId(1)).unwrap().state,
                CallState::Connected
            );
            assert!(controller.call(CallId(2)).is_none());
            assert!(controller.invariant_error().is_none());
        }
    }

    #[test]
    fn direct_transfer_rejects_non_exact_selection_without_mutation() {
        let mut controller = connected_outbound_controller();
        let device_id = binding().device_id;
        controller.set_call_selected(&device_id, CallId(1), false);
        assert_eq!(
            controller.direct_transfer(&device_id),
            Err(TransferRejection::InvalidSelection)
        );
        assert_eq!(
            controller.call(CallId(1)).unwrap().state,
            CallState::Connected
        );
        controller.set_call_selected(&device_id, CallId(1), true);
        assert_eq!(
            controller.direct_transfer(&device_id),
            Err(TransferRejection::InvalidSelection)
        );
        assert!(controller.transfer_transaction(CallId(1)).is_none());
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn direct_transfer_rejects_three_or_cross_device_selections_without_mutation() {
        let mut controller = connected_outbound_controller();
        let first_device = binding().device_id;
        controller.registered(registration_for("SEP112233445566"));
        controller.begin_asterisk_call(
            CallId(2),
            PbxCallId(8),
            &binding_for("SEP001122334455", 1),
            Codec::Pcma,
        );
        controller.phone_answer(CallId(2));
        controller.hold(CallId(2));
        controller.begin_asterisk_call(
            CallId(3),
            PbxCallId(9),
            &binding_for("SEP001122334455", 1),
            Codec::Pcma,
        );
        controller.phone_answer(CallId(3));
        for call_id in [CallId(1), CallId(2), CallId(3)] {
            controller.set_call_selected(&first_device, call_id, true);
        }
        let before = [
            controller.call(CallId(1)).unwrap().state,
            controller.call(CallId(2)).unwrap().state,
            controller.call(CallId(3)).unwrap().state,
        ];
        assert_eq!(
            controller.direct_transfer(&first_device),
            Err(TransferRejection::InvalidSelection)
        );
        assert_eq!(
            [
                controller.call(CallId(1)).unwrap().state,
                controller.call(CallId(2)).unwrap().state,
                controller.call(CallId(3)).unwrap().state,
            ],
            before
        );

        controller.set_call_selected(&first_device, CallId(3), false);
        controller.begin_asterisk_call(
            CallId(4),
            PbxCallId(10),
            &binding_for("SEP112233445566", 2),
            Codec::Pcma,
        );
        controller.phone_answer(CallId(4));
        controller.set_call_selected(&first_device, CallId(2), false);
        assert!(!controller.set_call_selected(&first_device, CallId(4), true));
        assert_eq!(
            controller.direct_transfer(&first_device),
            Err(TransferRejection::InvalidSelection)
        );
        assert!(controller.transfer_transaction(CallId(1)).is_none());
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn direct_transfer_backend_failure_preserves_selection_and_allows_retry() {
        let mut controller = Controller::new(Duration::from_secs(1));
        controller.registered(registration());
        controller.begin_asterisk_call(CallId(2), PbxCallId(8), &binding(), Codec::Pcma);
        controller.phone_answer(CallId(2));
        controller.media_opened(CallId(2), test_media_endpoint(Codec::Pcma));
        controller.hold(CallId(2));
        controller.begin_asterisk_call(CallId(3), PbxCallId(9), &binding(), Codec::Pcma);
        controller.phone_answer(CallId(3));
        controller.media_opened(CallId(3), test_media_endpoint(Codec::Pcma));
        let device_id = binding().device_id;
        controller.set_call_selected(&device_id, CallId(2), true);
        controller.set_call_selected(&device_id, CallId(3), true);

        let first = controller.direct_transfer(&device_id).unwrap();
        let outcome = controller
            .abort_transfer(
                &device_id,
                first.completion.transaction_id,
                TransferCancellationReason::BackendFailure,
            )
            .unwrap();
        assert!(outcome.effects.is_empty());
        assert_eq!(controller.call(CallId(2)).unwrap().state, CallState::Held);
        assert_eq!(
            controller.call(CallId(3)).unwrap().state,
            CallState::Connected
        );
        assert!(
            controller
                .registered_device(&device_id)
                .unwrap()
                .is_call_selected(CallId(2))
        );
        assert!(
            controller
                .registered_device(&device_id)
                .unwrap()
                .is_call_selected(CallId(3))
        );
        assert!(controller.direct_transfer(&device_id).is_ok());
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn direct_transfer_hangup_race_removes_only_the_terminated_leg_on_failure() {
        let mut controller = Controller::new(Duration::from_secs(1));
        controller.registered(registration());
        controller.begin_asterisk_call(CallId(2), PbxCallId(8), &binding(), Codec::Pcma);
        controller.phone_answer(CallId(2));
        controller.hold(CallId(2));
        controller.begin_asterisk_call(CallId(3), PbxCallId(9), &binding(), Codec::Pcma);
        controller.phone_answer(CallId(3));
        let device_id = binding().device_id;
        controller.set_call_selected(&device_id, CallId(2), true);
        controller.set_call_selected(&device_id, CallId(3), true);

        let plan = controller.direct_transfer(&device_id).unwrap();
        assert!(
            controller
                .pbx_hangup_with_effects(PbxCallId(8))
                .unwrap()
                .effects
                .is_empty()
        );
        let outcome = controller
            .abort_transfer(
                &device_id,
                plan.completion.transaction_id,
                TransferCancellationReason::BackendFailure,
            )
            .unwrap();
        assert!(outcome.effects.iter().any(|effect| matches!(
            effect,
            DriverEffect::Handset(HandsetEffect::SetCallState {
                call_id: CallId(2),
                state: HandsetCallState::OnHook,
                ..
            })
        )));
        assert!(controller.call(CallId(2)).is_none());
        assert!(controller.call(CallId(3)).is_some());
        assert!(
            controller
                .registered_device(&device_id)
                .unwrap()
                .is_call_selected(CallId(3))
        );
        assert!(controller.transfer_transaction(CallId(3)).is_none());
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn pbx_state_and_device_appearance_states_transition_separately() {
        let mut controller = Controller::new(Duration::from_secs(1));
        let first = binding_for("SEP001122334455", 1);
        let second = binding_for("SEP112233445566", 2);
        controller.begin_asterisk_call(CallId(2), PbxCallId(8), &first, Codec::Pcma);
        let second_appearance = controller
            .add_call_appearance(PbxCallId(8), CallId(3), &second, Codec::Pcmu)
            .unwrap();

        let pbx_call = controller.pbx_call(PbxCallId(8)).unwrap();
        assert_eq!(pbx_call.appearance_ids().count(), 2);
        assert_eq!(controller.appearances_for_pbx(PbxCallId(8)).count(), 2);
        assert_eq!(
            controller.appearances_for_device(&first.device_id).count(),
            1
        );
        assert_eq!(
            controller.appearances_for_device(&second.device_id).count(),
            1
        );
        assert_eq!(
            controller
                .call_appearance(second_appearance)
                .unwrap()
                .sccp_id,
            CallId(3)
        );

        controller.phone_answer(CallId(3));
        controller.media_opened(CallId(3), test_media_endpoint(Codec::Pcmu));
        assert_eq!(
            controller.pbx_call(PbxCallId(8)).unwrap().state,
            CallState::Connected
        );
        assert_eq!(
            controller.appearance_for_call(CallId(2)).unwrap().state,
            CallState::RemoteInUse
        );
        assert_eq!(
            controller.appearance_for_call(CallId(3)).unwrap().state,
            CallState::Connected
        );

        assert!(controller.pbx_answer(PbxCallId(8)).is_empty());
        controller.hold(CallId(3));
        assert_eq!(
            controller.pbx_call(PbxCallId(8)).unwrap().state,
            CallState::Held
        );
        assert_eq!(
            controller.appearance_for_call(CallId(2)).unwrap().state,
            CallState::SharedHeld
        );
        assert_eq!(
            controller.appearance_for_call(CallId(3)).unwrap().state,
            CallState::Held
        );
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn appearance_and_pbx_indexes_are_cleaned_at_their_own_lifetimes() {
        let mut controller = Controller::new(Duration::from_secs(1));
        let first = binding_for("SEP001122334455", 1);
        let second = binding_for("SEP112233445566", 2);
        controller.registered(registration_for(first.device_id.as_str()));
        controller.registered(registration_for(second.device_id.as_str()));
        controller.begin_asterisk_call(CallId(2), PbxCallId(8), &first, Codec::Pcma);
        controller
            .add_call_appearance(PbxCallId(8), CallId(3), &second, Codec::Pcmu)
            .unwrap();
        controller.set_call_selected(&first.device_id, CallId(2), true);
        controller.set_call_selected(&second.device_id, CallId(3), true);

        assert!(controller.disconnected(&second.device_id).is_empty());
        assert!(controller.pbx_call(PbxCallId(8)).is_some());
        assert!(controller.call(CallId(2)).is_some());
        assert!(controller.call(CallId(3)).is_none());
        assert!(controller.appearance_for_call(CallId(3)).is_none());
        assert_eq!(controller.appearances_for_pbx(PbxCallId(8)).count(), 1);
        assert_eq!(
            controller.appearances_for_device(&second.device_id).count(),
            0
        );
        assert!(controller.invariant_error().is_none());

        let removed = controller.pbx_hangup(PbxCallId(8)).unwrap();
        assert_eq!(removed.state, CallState::Ended);
        assert!(controller.pbx_call(PbxCallId(8)).is_none());
        assert!(controller.call(CallId(2)).is_none());
        assert_eq!(controller.calls().count(), 0);
        assert_eq!(
            controller.appearances_for_device(&first.device_id).count(),
            0
        );
        assert_eq!(
            controller
                .registered_device(&first.device_id)
                .unwrap()
                .selected_calls()
                .count(),
            0
        );
        assert!(controller.invariant_error().is_none());
    }

    fn connected_outbound_controller() -> Controller {
        let mut controller = Controller::new(Duration::from_secs(1));
        controller.registered(registration());
        controller.begin_phone_call(CallId(1), binding(), Codec::Pcmu, Instant::now());
        controller.enbloc(CallId(1), "2100".into());
        controller.pbx_answer(PbxCallId(1));
        assert_eq!(
            controller.call(CallId(1)).unwrap().state,
            CallState::Connected
        );
        controller
    }

    #[test]
    fn conference_identity_uses_the_remote_party_for_each_direction() {
        let mut outbound = connected_outbound_controller();
        let outbound_info = CallInfo {
            direction: CallDirection::Outbound,
            calling_name: "Local desk".into(),
            calling_number: "1001".into(),
            called_name: "Remote destination".into(),
            called_number: "2200".into(),
            ..CallInfo::default()
        };
        outbound.set_call_info(CallId(1), outbound_info);
        let appearance = outbound.appearance_for_call(CallId(1)).unwrap().clone();
        let participant = outbound.conference_participant(&appearance, true);
        assert_eq!(participant.display_name, "Remote destination");
        assert_eq!(participant.number, "2200");

        let mut inbound = shared_inbound_controller();
        let inbound_info = CallInfo {
            direction: CallDirection::Inbound,
            calling_name: "Remote caller".into(),
            calling_number: "3300".into(),
            called_name: "Local desk".into(),
            called_number: "1001".into(),
            ..CallInfo::default()
        };
        inbound.set_call_info(CallId(2), inbound_info);
        let appearance = inbound.appearance_for_call(CallId(2)).unwrap().clone();
        let participant = inbound.conference_participant(&appearance, true);
        assert_eq!(participant.display_name, "Remote caller");
        assert_eq!(participant.number, "3300");
    }

    #[test]
    fn conference_identity_is_empty_for_each_presentation_restriction() {
        let mut controller = connected_outbound_controller();
        controller.set_call_info(
            CallId(1),
            CallInfo {
                direction: CallDirection::Outbound,
                called_name: "Private destination".into(),
                called_number: "4400".into(),
                ..CallInfo::default()
            },
        );
        let call = controller.pbx_call(PbxCallId(1)).unwrap().clone();
        let appearance = controller.appearance_for_call(CallId(1)).unwrap().clone();

        let mut private_call = call.clone();
        private_call.privacy = true;
        assert_eq!(
            conference_participant_identity(&private_call, &appearance),
            ConferenceParticipantIdentity::default()
        );

        let mut private_appearance = appearance.clone();
        private_appearance.privacy = true;
        assert_eq!(
            conference_participant_identity(&call, &private_appearance),
            ConferenceParticipantIdentity::default()
        );

        let mut restricted = appearance;
        restricted.info.party_restrictions = 1;
        assert_eq!(
            conference_participant_identity(&call, &restricted),
            ConferenceParticipantIdentity::default()
        );
    }

    #[test]
    fn connected_line_updates_refresh_identity_without_reopening_the_list() {
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
        let initial_ids = controller
            .conference_session(CallId(2))
            .unwrap()
            .participants
            .iter()
            .map(|participant| participant.id)
            .collect::<Vec<_>>();

        let effects = controller.set_call_info(
            CallId(2),
            CallInfo {
                direction: CallDirection::Outbound,
                called_name: "Consulted party".into(),
                called_number: "2200".into(),
                ..CallInfo::default()
            },
        );
        assert!(matches!(
            effects.as_slice(),
            [DriverEffect::Handset(HandsetEffect::SetCallInfo {
                call_id: CallId(2),
                ..
            })]
        ));

        let effects = controller.update_call_info_by_pbx(PbxCallId(1), |current| {
            let mut updated = current.clone();
            updated.called_name = "Updated original party".into();
            updated.called_number = "2101".into();
            updated
        });
        assert!(matches!(
            effects.as_slice(),
            [DriverEffect::Handset(HandsetEffect::SetCallInfo {
                call_id: CallId(1),
                ..
            })]
        ));

        let session = controller.conference_session(CallId(2)).unwrap();
        assert_eq!(
            session
                .participants
                .iter()
                .map(|participant| participant.id)
                .collect::<Vec<_>>(),
            initial_ids
        );
        let original = session.participants.by_pbx(PbxCallId(1)).unwrap();
        assert_eq!(original.display_name, "Updated original party");
        assert_eq!(original.number, "2101");
        let consultation = session.participants.by_pbx(PbxCallId(2)).unwrap();
        assert_eq!(consultation.display_name, "Consulted party");
        assert_eq!(consultation.number, "2200");

        assert!(controller.set_call_privacy(CallId(1), true));
        let hidden = controller
            .conference_session(CallId(2))
            .unwrap()
            .participants
            .by_pbx(PbxCallId(1))
            .unwrap();
        assert!(hidden.display_name.is_empty());
        assert!(hidden.number.is_empty());
        assert!(controller.set_call_privacy(CallId(1), false));
        let restored = controller
            .conference_session(CallId(2))
            .unwrap()
            .participants
            .by_pbx(PbxCallId(1))
            .unwrap();
        assert_eq!(restored.display_name, "Updated original party");
        assert_eq!(restored.number, "2101");

        controller.update_call_info_by_pbx(PbxCallId(2), |current| {
            let mut restricted = current.clone();
            restricted.party_restrictions = 1;
            restricted
        });
        let hidden = controller
            .conference_session(CallId(2))
            .unwrap()
            .participants
            .by_pbx(PbxCallId(2))
            .unwrap();
        assert!(hidden.display_name.is_empty());
        assert!(hidden.number.is_empty());
        assert_eq!(hidden.id, initial_ids[1]);
    }

    #[test]
    fn pending_invite_keeps_its_identity_update_when_merged() {
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
            .begin_conference_invite(CallId(1), CallId(3), binding(), Codec::Pcmu, Instant::now())
            .unwrap();
        let participant_id = controller
            .conference_session(CallId(3))
            .unwrap()
            .pending_invite
            .as_ref()
            .unwrap()
            .participant
            .id;

        controller.set_call_info(
            CallId(3),
            CallInfo {
                direction: CallDirection::Outbound,
                called_name: "Invited party".into(),
                called_number: "3300".into(),
                ..CallInfo::default()
            },
        );
        controller.pbx_answer(PbxCallId(3));
        controller.confirm_conference_invite(CallId(3)).unwrap();
        assert!(controller.conference_invite_merged(CallId(3)));

        let participant = controller
            .conference_session(CallId(3))
            .unwrap()
            .participants
            .by_pbx(PbxCallId(3))
            .unwrap();
        assert_eq!(participant.id, participant_id);
        assert_eq!(participant.display_name, "Invited party");
        assert_eq!(participant.number, "3300");
    }

    #[test]
    fn passive_remote_hangup_detaches_pbx_before_bounded_tone_cleanup() {
        let mut controller = connected_outbound_controller();
        let now = Instant::now();
        let plan = controller
            .begin_remote_hangup(PbxCallId(1), Some(Tone::Zip), Duration::from_secs(15), now)
            .unwrap();

        assert!(plan.pending.is_some());
        assert!(controller.pbx_call(PbxCallId(1)).is_none());
        assert!(controller.call(CallId(1)).is_none());
        assert_eq!(
            plan.outcome.effects,
            vec![
                HandsetEffect::SetCallState {
                    device_id: DeviceId::new("SEP001122334455").unwrap(),
                    call_id: CallId(1),
                    state: HandsetCallState::Connected,
                    stop_media: true,
                }
                .into(),
                HandsetEffect::StartTone {
                    device_id: DeviceId::new("SEP001122334455").unwrap(),
                    call_id: CallId(1),
                    tone: Tone::Zip,
                }
                .into(),
            ]
        );
        assert!(
            controller
                .expire_remote_hangups(now + Duration::from_secs(14))
                .is_empty()
        );
        assert_eq!(
            controller.expire_remote_hangups(now + Duration::from_secs(15)),
            vec![
                HandsetEffect::SetCallState {
                    device_id: DeviceId::new("SEP001122334455").unwrap(),
                    call_id: CallId(1),
                    state: HandsetCallState::OnHook,
                    stop_media: true,
                }
                .into()
            ]
        );
        assert!(
            controller
                .expire_remote_hangups(now + Duration::from_secs(16))
                .is_empty()
        );
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn passive_remote_hangup_delays_only_the_exact_shared_active_owner() {
        let mut controller = shared_inbound_controller();
        controller.phone_answer(CallId(3));
        let plan = controller
            .begin_remote_hangup(
                PbxCallId(8),
                Some(Tone::Zip),
                Duration::from_secs(15),
                Instant::now(),
            )
            .unwrap();

        assert!(plan.pending.is_some());
        assert!(plan.outcome.effects.iter().any(|effect| matches!(
            effect,
            DriverEffect::Handset(HandsetEffect::SetCallState {
                call_id: CallId(2),
                state: HandsetCallState::OnHook,
                stop_media: true,
                ..
            })
        )));
        assert!(!plan.outcome.effects.iter().any(|effect| matches!(
            effect,
            DriverEffect::Handset(HandsetEffect::SetCallState {
                call_id: CallId(3),
                state: HandsetCallState::OnHook,
                ..
            })
        )));
        assert!(plan.outcome.effects.iter().any(|effect| matches!(
            effect,
            DriverEffect::Handset(HandsetEffect::StartTone {
                call_id: CallId(3),
                tone: Tone::Zip,
                ..
            })
        )));
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn passive_remote_hangup_is_immediate_when_disabled_held_or_generation_exhausted() {
        let now = Instant::now();
        let mut disabled = connected_outbound_controller();
        let disabled = disabled
            .begin_remote_hangup(PbxCallId(1), None, Duration::from_secs(15), now)
            .unwrap();
        assert_eq!(disabled.pending, None);
        assert!(disabled.outcome.effects.iter().any(|effect| matches!(
            effect,
            DriverEffect::Handset(HandsetEffect::SetCallState {
                call_id: CallId(1),
                state: HandsetCallState::OnHook,
                ..
            })
        )));

        let mut held = connected_outbound_controller();
        held.hold(CallId(1));
        let held = held
            .begin_remote_hangup(PbxCallId(1), Some(Tone::Zip), Duration::from_secs(15), now)
            .unwrap();
        assert_eq!(held.pending, None);

        let mut ringing = shared_inbound_controller();
        let ringing = ringing
            .begin_remote_hangup(PbxCallId(8), Some(Tone::Zip), Duration::from_secs(15), now)
            .unwrap();
        assert_eq!(ringing.pending, None);

        let mut waiting = connected_outbound_controller();
        waiting.offer_inbound_call(
            PbxCallId(8),
            [InboundAppearance {
                call_id: CallId(2),
                binding: binding(),
                codec: Codec::Pcmu,
            }],
        );
        let waiting_plan = waiting
            .begin_remote_hangup(PbxCallId(8), Some(Tone::Zip), Duration::from_secs(15), now)
            .unwrap();
        assert_eq!(waiting_plan.pending, None);
        assert_eq!(
            waiting_plan.outcome.primary.unwrap().state,
            CallState::Ended
        );
        assert_eq!(waiting.call(CallId(1)).unwrap().state, CallState::Connected);

        let mut transfer = connected_outbound_controller();
        transfer
            .begin_transfer(TransferConsultationRequest {
                source_call_id: CallId(1),
                consultation_call_id: CallId(2),
                binding: binding(),
                codec: Codec::Pcmu,
                complete_on_hangup: false,
                now,
            })
            .unwrap();
        let transfer = transfer
            .begin_remote_hangup(PbxCallId(1), Some(Tone::Zip), Duration::from_secs(15), now)
            .unwrap();
        assert_eq!(transfer.pending, None);

        let mut conference = connected_outbound_controller();
        conference
            .begin_conference(CallId(1), CallId(2), binding(), Codec::Pcmu, now, true)
            .unwrap();
        conference.enbloc(CallId(2), "2200".into());
        conference.pbx_answer(PbxCallId(2));
        conference.confirm_conference(CallId(2)).unwrap();
        let conference = conference
            .begin_remote_hangup(PbxCallId(1), Some(Tone::Zip), Duration::from_secs(15), now)
            .unwrap();
        assert_eq!(conference.pending, None);

        let mut exhausted = connected_outbound_controller();
        exhausted.next_remote_hangup_generation = u64::MAX;
        let exhausted_plan = exhausted
            .begin_remote_hangup(PbxCallId(1), Some(Tone::Zip), Duration::from_secs(15), now)
            .unwrap();
        assert_eq!(exhausted_plan.pending, None);
        assert_eq!(exhausted.next_remote_hangup_generation, u64::MAX);
    }

    #[test]
    fn passive_remote_hangup_cancel_disconnect_and_unload_are_exactly_once() {
        let now = Instant::now();
        let mut physical = connected_outbound_controller();
        let token = physical
            .begin_remote_hangup(PbxCallId(1), Some(Tone::Zip), Duration::from_secs(15), now)
            .unwrap()
            .pending
            .unwrap();
        assert_eq!(physical.hangup(CallId(1)).len(), 1);
        assert!(physical.complete_remote_hangup_token(token).is_none());
        assert!(physical.hangup(CallId(1)).is_empty());

        let mut disconnected = connected_outbound_controller();
        let token = disconnected
            .begin_remote_hangup(PbxCallId(1), Some(Tone::Zip), Duration::from_secs(15), now)
            .unwrap()
            .pending
            .unwrap();
        assert!(
            disconnected
                .disconnected(&DeviceId::new("SEP001122334455").unwrap())
                .is_empty()
        );
        assert!(disconnected.complete_remote_hangup_token(token).is_none());

        let mut shutdown = connected_outbound_controller();
        shutdown
            .begin_remote_hangup(PbxCallId(1), Some(Tone::Zip), Duration::from_secs(15), now)
            .unwrap();
        assert_eq!(shutdown.drain_remote_hangups().len(), 1);
        assert!(shutdown.drain_remote_hangups().is_empty());

        let mut presentation_failure = connected_outbound_controller();
        let token = presentation_failure
            .begin_remote_hangup(PbxCallId(1), Some(Tone::Zip), Duration::from_secs(15), now)
            .unwrap()
            .pending
            .unwrap();
        assert!(matches!(
            presentation_failure.complete_remote_hangup_token(token),
            Some(DriverEffect::Handset(HandsetEffect::SetCallState {
                call_id: CallId(1),
                state: HandsetCallState::OnHook,
                stop_media: true,
                ..
            }))
        ));
        assert!(
            presentation_failure
                .complete_remote_hangup_token(token)
                .is_none()
        );
        assert!(presentation_failure.drain_remote_hangups().is_empty());
    }

    #[test]
    fn consultation_conference_holds_original_and_creates_one_typed_call() {
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

        assert!(matches!(
            effects.first(),
            Some(DriverEffect::Backend(PbxEffect::Hold {
                call_id: PbxCallId(1)
            }))
        ));
        assert!(effects.iter().any(|effect| matches!(
            effect,
            DriverEffect::Backend(PbxEffect::CreateChannel {
                handset_call_id: CallId(2),
                call_id: PbxCallId(2),
                ..
            })
        )));
        let session = controller.conference_session(CallId(2)).unwrap();
        assert_eq!(session.id, ConferenceId::new(1));
        assert_eq!(session.bridge_id, PbxBridgeId(1));
        assert_eq!(session.phase, ConferencePhase::Consultation);
        assert_eq!(controller.call(CallId(1)).unwrap().state, CallState::Held);
        assert_eq!(
            controller.call(CallId(2)).unwrap().state,
            CallState::Collecting
        );
        assert_eq!(
            controller.begin_conference(
                CallId(1),
                CallId(3),
                binding(),
                Codec::Pcmu,
                Instant::now(),
                true,
            ),
            Err(ConferenceRejection::NotConnected)
        );
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn conference_confirm_merges_both_call_bridges_and_destroys_once_on_end() {
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

        let effects = controller.confirm_conference(CallId(2)).unwrap();
        assert_eq!(
            effects,
            [
                DriverEffect::Backend(PbxEffect::Bridge {
                    operation: crate::runtime::backend::BridgeOperation::Create {
                        bridge_id: PbxBridgeId(1),
                    },
                }),
                DriverEffect::Backend(PbxEffect::Resume {
                    call_id: PbxCallId(1),
                }),
                DriverEffect::Backend(PbxEffect::Bridge {
                    operation: crate::runtime::backend::BridgeOperation::MergeConsultation {
                        bridge_id: PbxBridgeId(1),
                        original_call_id: PbxCallId(1),
                        consultation_call_id: PbxCallId(2),
                    },
                }),
            ]
        );
        assert!(controller.conference_merged(CallId(2)));
        assert_eq!(
            controller.conference_session(CallId(2)).unwrap().phase,
            ConferencePhase::Active
        );

        let cleanup = controller.end_conference(CallId(2));
        assert_eq!(
            cleanup
                .iter()
                .filter(|effect| matches!(
                    effect,
                    DriverEffect::Backend(PbxEffect::Bridge {
                        operation: crate::runtime::backend::BridgeOperation::Destroy { .. }
                    })
                ))
                .count(),
            1
        );
        assert_eq!(
            cleanup
                .iter()
                .filter(|effect| matches!(effect, DriverEffect::Backend(PbxEffect::Hangup { .. })))
                .count(),
            2
        );
        assert!(controller.calls().next().is_none());
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn destination_dialing_does_not_mutate_an_active_adhoc_conference() {
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
        let before = controller.conference_session(CallId(1)).unwrap().clone();
        controller.begin_phone_call(CallId(3), binding(), Codec::Pcmu, Instant::now());

        assert_eq!(
            controller.begin_conference_destination(ConferenceDestinationRequest {
                device_id: binding().device_id,
                handset_call_id: CallId(3),
                destination: "700".into(),
                application_options: "Mac".into(),
            }),
            Err(ConferenceDestinationRejection::Conflict)
        );
        let after = controller.conference_session(CallId(1)).unwrap();
        assert_eq!(after.id, before.id);
        assert_eq!(after.bridge_id, before.bridge_id);
        assert_eq!(after.participants, before.participants);
        assert_eq!(
            controller.call(CallId(3)).unwrap().state,
            CallState::Collecting
        );
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn cancelling_or_failing_consultation_restores_original_without_hangup() {
        for bridge_created in [false, true] {
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
            let cleanup = if bridge_created {
                controller.abort_conference(CallId(2), true, true, true, true)
            } else {
                controller.hangup(CallId(2))
            };
            assert!(cleanup.iter().any(|effect| matches!(
                effect,
                DriverEffect::Backend(PbxEffect::Resume {
                    call_id: PbxCallId(1)
                })
            )));
            assert!(!cleanup.iter().any(|effect| matches!(
                effect,
                DriverEffect::Backend(PbxEffect::Hangup {
                    call_id: PbxCallId(1)
                })
            )));
            assert_eq!(
                cleanup
                    .iter()
                    .filter(|effect| matches!(
                        effect,
                        DriverEffect::Backend(PbxEffect::Bridge {
                            operation: crate::runtime::backend::BridgeOperation::Destroy { .. }
                        })
                    ))
                    .count(),
                usize::from(bridge_created)
            );
            assert_eq!(
                controller.call(CallId(1)).unwrap().state,
                CallState::Connected
            );
            assert!(controller.call(CallId(2)).is_none());
            assert!(controller.conference_session(CallId(1)).is_none());
            assert!(controller.invariant_error().is_none());
        }
    }

    #[test]
    fn disabled_conference_does_not_mutate_the_connected_call() {
        let mut controller = connected_outbound_controller();
        assert_eq!(
            controller.begin_conference(
                CallId(1),
                CallId(2),
                binding(),
                Codec::Pcmu,
                Instant::now(),
                false,
            ),
            Err(ConferenceRejection::Disabled)
        );
        assert_eq!(
            controller.call(CallId(1)).unwrap().state,
            CallState::Connected
        );
        assert!(controller.call(CallId(2)).is_none());
        assert!(controller.invariant_error().is_none());
    }

    fn three_call_conference_controller() -> Controller {
        let mut controller = Controller::new(Duration::from_secs(1));
        controller.registered(registration());
        for (handset, pbx) in [(2, 8), (3, 9), (4, 10)] {
            controller.begin_asterisk_call(CallId(handset), pbx.into(), &binding(), Codec::Pcma);
            controller.phone_answer(CallId(handset));
            controller.media_opened(CallId(handset), test_media_endpoint(Codec::Pcma));
            if handset != 4 {
                controller.hold(CallId(handset));
            }
        }
        controller
    }

    #[test]
    fn join_uses_exact_multi_selection_with_stable_participant_ids() {
        let mut controller = three_call_conference_controller();
        let device = binding().device_id;
        controller.set_call_selected(&device, CallId(2), true);

        let effects = controller.join_calls(&device, CallId(4), true).unwrap();
        assert_eq!(
            effects,
            [
                DriverEffect::Backend(PbxEffect::Bridge {
                    operation: crate::runtime::backend::BridgeOperation::Create {
                        bridge_id: PbxBridgeId(1),
                    },
                }),
                DriverEffect::Backend(PbxEffect::Resume {
                    call_id: PbxCallId(8),
                }),
                DriverEffect::Backend(PbxEffect::Bridge {
                    operation: crate::runtime::backend::BridgeOperation::MergeCalls {
                        bridge_id: PbxBridgeId(1),
                        call_ids: vec![PbxCallId(10), PbxCallId(8)],
                    },
                }),
            ]
        );
        let session = controller.conference_session(CallId(4)).unwrap();
        assert_eq!(session.origin, ConferenceOrigin::Selection);
        assert_eq!(
            session
                .participants
                .iter()
                .map(|participant| {
                    (
                        participant.id,
                        participant.pbx_call_id,
                        participant.moderator,
                    )
                })
                .collect::<Vec<_>>(),
            [
                (ParticipantId::new(1), PbxCallId(10), true),
                (ParticipantId::new(2), PbxCallId(8), false),
            ]
        );
        assert!(controller.conference_session(CallId(3)).is_none());
        assert!(controller.conference_merged(CallId(4)));
        assert_eq!(
            controller
                .conference_session(CallId(2))
                .unwrap()
                .participants
                .moderator()
                .unwrap()
                .handset_call_id,
            CallId(4)
        );
        let json: serde_json::Value =
            serde_json::from_str(&controller.conference_json(CallId(2)).unwrap()).unwrap();
        assert_eq!(json["moderator_id"], 1);
        assert_eq!(json["participants"].as_array().unwrap().len(), 2);
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn join_without_multi_selection_uses_all_eligible_calls_and_rolls_back() {
        let mut controller = three_call_conference_controller();
        let device = binding().device_id;

        let effects = controller.join_calls(&device, CallId(4), true).unwrap();
        assert!(matches!(
            effects.last(),
            Some(DriverEffect::Backend(PbxEffect::Bridge {
                operation: crate::runtime::backend::BridgeOperation::MergeCalls { call_ids, .. },
            })) if call_ids == &[PbxCallId(10), PbxCallId(8), PbxCallId(9)]
        ));
        assert_eq!(
            effects
                .iter()
                .filter(|effect| matches!(effect, DriverEffect::Backend(PbxEffect::Resume { .. })))
                .count(),
            2
        );
        let rollback =
            controller.abort_join_conference(CallId(4), true, &[PbxCallId(8), PbxCallId(9)]);
        assert_eq!(
            rollback,
            [
                DriverEffect::Backend(PbxEffect::Bridge {
                    operation: crate::runtime::backend::BridgeOperation::Destroy {
                        bridge_id: PbxBridgeId(1),
                    },
                }),
                DriverEffect::Backend(PbxEffect::Hold {
                    call_id: PbxCallId(8),
                }),
                DriverEffect::Backend(PbxEffect::Hold {
                    call_id: PbxCallId(9),
                }),
            ]
        );
        assert!(controller.conference_session(CallId(4)).is_none());
        assert_eq!(
            controller.pbx_call(PbxCallId(8)).unwrap().state,
            CallState::Held
        );
        assert_eq!(
            controller.pbx_call(PbxCallId(10)).unwrap().state,
            CallState::Connected
        );
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn selection_toggle_rejects_cross_device_and_is_deterministic() {
        let mut controller = three_call_conference_controller();
        let device = binding().device_id;
        let other = DeviceId::new("SEP112233445566").unwrap();
        controller.registered(registration_for(other.as_str()));

        assert_eq!(
            controller.toggle_call_selected(&device, CallId(4)),
            Some(false)
        );
        assert_eq!(
            controller.toggle_call_selected(&device, CallId(4)),
            Some(true)
        );
        assert_eq!(controller.toggle_call_selected(&other, CallId(4)), None);
    }

    fn active_three_party_conference() -> Controller {
        let mut controller = three_call_conference_controller();
        let device = binding().device_id;
        controller.join_calls(&device, CallId(4), true).unwrap();
        assert!(controller.conference_merged(CallId(4)));
        controller
    }

    fn active_three_party_conference_with_media() -> Controller {
        let mut controller = three_call_conference_controller();
        let device = binding().device_id;
        controller.join_calls(&device, CallId(4), true).unwrap();
        assert!(controller.configure_conference_media(
            CallId(4),
            ConferenceMediaPolicy {
                music_on_hold_class: Some("office".into()),
                mute_on_entry: false,
                play_general_announcements: true,
                play_participant_announcements: true,
            },
        ));
        assert!(controller.conference_merged(CallId(4)));
        controller
    }

    fn mute_on_entry_policy(enabled: bool) -> ConferenceMediaPolicy {
        ConferenceMediaPolicy {
            music_on_hold_class: None,
            mute_on_entry: enabled,
            play_general_announcements: false,
            play_participant_announcements: false,
        }
    }

    fn participant_muted(controller: &Controller, call_id: CallId, participant_id: u32) -> bool {
        let json: serde_json::Value =
            serde_json::from_str(&controller.conference_json(call_id).unwrap()).unwrap();
        json["participants"]
            .as_array()
            .unwrap()
            .iter()
            .find(|participant| participant["id"] == participant_id)
            .unwrap()["muted"]
            .as_bool()
            .unwrap()
    }

    #[test]
    fn mute_on_entry_consultation_is_ordered_and_commits_only_after_all_effects() {
        for enabled in [false, true] {
            let mut controller = connected_outbound_controller();
            controller
                .begin_conference_with_media(
                    ConferenceConsultationRequest {
                        original_call_id: CallId(1),
                        consultation_call_id: CallId(2),
                        binding: binding(),
                        codec: Codec::Pcmu,
                        now: Instant::now(),
                        permitted: true,
                    },
                    mute_on_entry_policy(enabled),
                )
                .unwrap();
            controller.enbloc(CallId(2), "2200".into());
            controller.pbx_answer(PbxCallId(2));

            assert!(!participant_muted(&controller, CallId(2), 2));
            let effects = controller.confirm_conference(CallId(2)).unwrap();
            assert!(matches!(
                effects.get(2),
                Some(DriverEffect::Backend(PbxEffect::Bridge {
                    operation: crate::runtime::backend::BridgeOperation::MergeConsultation { .. }
                }))
            ));
            assert_eq!(
                effects.get(3),
                enabled.then_some(&DriverEffect::Backend(PbxEffect::Bridge {
                    operation: crate::runtime::backend::BridgeOperation::SetParticipantMuted {
                        bridge_id: PbxBridgeId(1),
                        participant_id: ParticipantId::new(2),
                        call_id: PbxCallId(2),
                        muted: true,
                    },
                }))
            );
            assert!(!participant_muted(&controller, CallId(2), 2));
            assert!(controller.conference_merged(CallId(2)));
            assert_eq!(participant_muted(&controller, CallId(2), 2), enabled);
            assert!(controller.invariant_error().is_none());
        }

        let mut failed = connected_outbound_controller();
        failed
            .begin_conference_with_media(
                ConferenceConsultationRequest {
                    original_call_id: CallId(1),
                    consultation_call_id: CallId(2),
                    binding: binding(),
                    codec: Codec::Pcmu,
                    now: Instant::now(),
                    permitted: true,
                },
                mute_on_entry_policy(true),
            )
            .unwrap();
        failed.enbloc(CallId(2), "2200".into());
        failed.pbx_answer(PbxCallId(2));
        let effects = failed.confirm_conference(CallId(2)).unwrap();
        assert_eq!(effects.len(), 4);
        assert!(!participant_muted(&failed, CallId(2), 2));
        let cleanup = failed.abort_conference(CallId(2), true, true, true, true);
        assert!(cleanup.iter().any(|effect| matches!(
            effect,
            DriverEffect::Backend(PbxEffect::Bridge {
                operation: crate::runtime::backend::BridgeOperation::Destroy { .. }
            })
        )));
        assert!(failed.conference_session(CallId(1)).is_none());
        assert_eq!(failed.call(CallId(1)).unwrap().state, CallState::Connected);
        assert!(failed.invariant_error().is_none());
    }

    #[test]
    fn mute_on_entry_selection_covers_exact_and_all_members_with_rollback() {
        let device = binding().device_id;
        let mut selected = three_call_conference_controller();
        selected.set_call_selected(&device, CallId(2), true);
        let effects = selected
            .join_calls_with_media(&device, CallId(4), true, mute_on_entry_policy(true))
            .unwrap();
        assert!(matches!(
            effects.get(effects.len() - 2),
            Some(DriverEffect::Backend(PbxEffect::Bridge {
                operation: crate::runtime::backend::BridgeOperation::MergeCalls { .. }
            }))
        ));
        assert!(matches!(
            effects.last(),
            Some(DriverEffect::Backend(PbxEffect::Bridge {
                operation: crate::runtime::backend::BridgeOperation::SetParticipantMuted {
                    participant_id,
                    call_id: PbxCallId(8),
                    muted: true,
                    ..
                }
            })) if *participant_id == ParticipantId::new(2)
        ));
        assert!(!participant_muted(&selected, CallId(4), 2));
        assert!(selected.conference_merged(CallId(4)));
        assert!(participant_muted(&selected, CallId(4), 2));

        let mut all = three_call_conference_controller();
        let effects = all
            .join_calls_with_media(&device, CallId(4), true, mute_on_entry_policy(true))
            .unwrap();
        let merge_index = effects
            .iter()
            .position(|effect| {
                matches!(
                    effect,
                    DriverEffect::Backend(PbxEffect::Bridge {
                        operation: crate::runtime::backend::BridgeOperation::MergeCalls { .. }
                    })
                )
            })
            .unwrap();
        assert_eq!(
            &effects[merge_index + 1..],
            [
                DriverEffect::Backend(PbxEffect::Bridge {
                    operation: crate::runtime::backend::BridgeOperation::SetParticipantMuted {
                        bridge_id: PbxBridgeId(1),
                        participant_id: ParticipantId::new(2),
                        call_id: PbxCallId(8),
                        muted: true,
                    },
                }),
                DriverEffect::Backend(PbxEffect::Bridge {
                    operation: crate::runtime::backend::BridgeOperation::SetParticipantMuted {
                        bridge_id: PbxBridgeId(1),
                        participant_id: ParticipantId::new(3),
                        call_id: PbxCallId(9),
                        muted: true,
                    },
                }),
            ]
        );
        assert!(!participant_muted(&all, CallId(4), 2));
        assert!(!participant_muted(&all, CallId(4), 3));
        let rollback = all.abort_join_conference(CallId(4), true, &[PbxCallId(8), PbxCallId(9)]);
        assert!(rollback.iter().any(|effect| matches!(
            effect,
            DriverEffect::Backend(PbxEffect::Bridge {
                operation: crate::runtime::backend::BridgeOperation::Destroy { .. }
            })
        )));
        assert!(all.conference_session(CallId(4)).is_none());
        assert_eq!(all.call(CallId(2)).unwrap().state, CallState::Held);
        assert_eq!(all.call(CallId(3)).unwrap().state, CallState::Held);
        assert!(all.invariant_error().is_none());
    }

    #[test]
    fn mute_on_entry_invite_is_deferred_and_abort_preserves_published_state() {
        fn pending_invite() -> Controller {
            let device = binding().device_id;
            let mut controller = three_call_conference_controller();
            controller
                .join_calls_with_media(&device, CallId(4), true, mute_on_entry_policy(true))
                .unwrap();
            assert!(controller.conference_merged(CallId(4)));
            controller
                .begin_conference_invite(
                    CallId(4),
                    CallId(5),
                    binding(),
                    Codec::Pcma,
                    Instant::now(),
                )
                .unwrap();
            controller.enbloc(CallId(5), "2300".into());
            controller.pbx_answer(PbxCallId(11));
            controller
        }

        let mut failed = pending_invite();
        let before = failed.conference_json(CallId(4)).unwrap();
        let effects = failed.confirm_conference_invite(CallId(5)).unwrap();
        assert!(matches!(
            effects.get(effects.len() - 2),
            Some(DriverEffect::Backend(PbxEffect::Bridge {
                operation: crate::runtime::backend::BridgeOperation::MergeParticipant {
                    call_id: PbxCallId(11),
                    ..
                }
            }))
        ));
        assert!(matches!(
            effects.last(),
            Some(DriverEffect::Backend(PbxEffect::Bridge {
                operation: crate::runtime::backend::BridgeOperation::SetParticipantMuted {
                    participant_id,
                    call_id: PbxCallId(11),
                    muted: true,
                    ..
                }
            })) if *participant_id == ParticipantId::new(4)
        ));
        assert_eq!(failed.conference_json(CallId(4)).unwrap(), before);
        failed.abort_conference_invite(CallId(5), true, true, true);
        assert_eq!(failed.conference_json(CallId(4)).unwrap(), before);
        assert!(
            failed
                .conference_session(CallId(4))
                .unwrap()
                .pending_invite
                .is_none()
        );

        let mut succeeded = pending_invite();
        let before = succeeded.conference_json(CallId(4)).unwrap();
        succeeded.confirm_conference_invite(CallId(5)).unwrap();
        assert_eq!(succeeded.conference_json(CallId(4)).unwrap(), before);
        assert!(succeeded.conference_invite_merged(CallId(5)));
        assert!(participant_muted(&succeeded, CallId(4), 4));
        assert_eq!(
            succeeded
                .conference_session(CallId(4))
                .unwrap()
                .participants
                .iter()
                .len(),
            4
        );
        assert!(failed.invariant_error().is_none());
        assert!(succeeded.invariant_error().is_none());
    }

    #[test]
    fn fake_handset_consultation_confirm_cancel_and_invite_transcripts_are_exact() {
        let mut cancelled = connected_outbound_controller();
        let mut handset = FakeHandsets::default();
        let start = cancelled
            .begin_conference(
                CallId(1),
                CallId(2),
                binding(),
                Codec::Pcmu,
                Instant::now(),
                true,
            )
            .unwrap();
        handset.apply(&start);
        assert_eq!(
            handset.call_states(),
            [(CallId(1), HandsetCallState::Hold, true)]
        );
        assert!(handset.call_info(CallId(2)).is_empty());
        assert!(handset.tones(CallId(2)).is_empty());
        assert_eq!(
            cancelled.confirm_conference(CallId(2)),
            Err(ConferenceRejection::NotConnected)
        );

        handset.clear();
        handset.apply(&cancelled.cancel_conference(CallId(2)));
        assert_eq!(
            handset.call_states(),
            [(CallId(2), HandsetCallState::OnHook, true)]
        );
        assert_eq!(handset.media_winners(), [CallId(1)]);
        assert_eq!(
            cancelled.call(CallId(1)).unwrap().state,
            CallState::Connected
        );
        assert!(cancelled.call(CallId(2)).is_none());
        assert!(cancelled.invariant_error().is_none());

        let mut merged = connected_outbound_controller();
        merged
            .begin_conference(
                CallId(1),
                CallId(2),
                binding(),
                Codec::Pcmu,
                Instant::now(),
                true,
            )
            .unwrap();
        merged.enbloc(CallId(2), "2200".into());
        assert_eq!(
            merged.confirm_conference(CallId(2)),
            Err(ConferenceRejection::NotConnected)
        );
        merged.pbx_answer(PbxCallId(2));
        let effects = merged.confirm_conference(CallId(2)).unwrap();
        handset.clear();
        handset.apply(&effects);
        assert!(handset.effects.is_empty());
        assert!(merged.conference_merged(CallId(2)));
        let stable_ids = merged
            .conference_session(CallId(2))
            .unwrap()
            .participants
            .iter()
            .map(|participant| participant.id)
            .collect::<Vec<_>>();
        handset.apply(&merged.end_conference(CallId(2)));
        assert_eq!(
            handset.call_states(),
            [
                (CallId(1), HandsetCallState::OnHook, true),
                (CallId(2), HandsetCallState::OnHook, true),
            ]
        );
        assert_eq!(stable_ids, [ParticipantId::new(1), ParticipantId::new(2)]);
        assert!(merged.invariant_error().is_none());

        let mut invited = active_three_party_conference_with_media();
        let conference_id = invited.conference_session(CallId(4)).unwrap().id;
        handset.clear();
        let invite = invited
            .begin_conference_invite(CallId(4), CallId(5), binding(), Codec::Pcma, Instant::now())
            .unwrap();
        handset.apply(&invite);
        assert_eq!(
            handset.call_states(),
            [(CallId(4), HandsetCallState::Hold, true)]
        );
        assert!(handset.call_info(CallId(5)).is_empty());
        assert!(handset.tones(CallId(5)).is_empty());
        assert_eq!(
            invited.confirm_conference_invite(CallId(5)),
            Err(ConferenceRejection::NotConnected)
        );
        handset.clear();
        handset.apply(&invited.abort_conference_invite(CallId(5), true, true, true));
        assert_eq!(
            handset.call_states(),
            [(CallId(5), HandsetCallState::OnHook, true)]
        );
        assert_eq!(handset.media_winners(), [CallId(4)]);
        let restored = invited.conference_session_by_id(conference_id).unwrap();
        assert_eq!(restored.participants.iter().len(), 3);
        assert!(restored.pending_invite.is_none());
        assert!(invited.invariant_error().is_none());

        let mut completed_invite = active_three_party_conference_with_media();
        let completed_id = completed_invite.conference_session(CallId(4)).unwrap().id;
        completed_invite
            .begin_conference_invite(CallId(4), CallId(5), binding(), Codec::Pcma, Instant::now())
            .unwrap();
        completed_invite.enbloc(CallId(5), "2300".into());
        completed_invite.pbx_answer(PbxCallId(11));
        handset.clear();
        handset.apply(
            &completed_invite
                .confirm_conference_invite(CallId(5))
                .unwrap(),
        );
        assert!(handset.effects.is_empty());
        assert!(completed_invite.conference_invite_merged(CallId(5)));
        handset.apply(&completed_invite.conference_announcement_effects(
            completed_id,
            ConferenceAnnouncement::ParticipantJoined(ParticipantId::new(4)),
        ));
        assert_eq!(
            handset.announcements(),
            [(
                completed_id,
                vec![
                    ParticipantId::new(1),
                    ParticipantId::new(2),
                    ParticipantId::new(3),
                    ParticipantId::new(4),
                ],
                vec![PbxCallId(10), PbxCallId(8), PbxCallId(9), PbxCallId(11)],
                ConferenceAnnouncement::ParticipantJoined(ParticipantId::new(4)),
            )]
        );
        let completed = completed_invite
            .conference_session_by_id(completed_id)
            .unwrap();
        assert_eq!(completed.participants.iter().len(), 4);
        assert_eq!(
            completed
                .participants
                .iter()
                .map(|participant| participant.id)
                .collect::<Vec<_>>(),
            [
                ParticipantId::new(1),
                ParticipantId::new(2),
                ParticipantId::new(3),
                ParticipantId::new(4),
            ]
        );
        assert!(completed_invite.invariant_error().is_none());
    }

    #[test]
    fn fake_handset_selected_and_all_call_join_paths_preserve_exact_membership() {
        let device = binding().device_id;
        let mut selected = three_call_conference_controller();
        selected.set_call_selected(&device, CallId(2), true);
        let effects = selected.join_calls(&device, CallId(4), true).unwrap();
        let mut handset = FakeHandsets::default();
        handset.apply(&effects);
        assert!(handset.effects.is_empty());
        assert!(selected.conference_merged(CallId(4)));
        let session = selected.conference_session(CallId(4)).unwrap().clone();
        assert_eq!(
            session
                .participants
                .iter()
                .map(|participant| (participant.id, participant.handset_call_id))
                .collect::<Vec<_>>(),
            [
                (ParticipantId::new(1), CallId(4)),
                (ParticipantId::new(2), CallId(2)),
            ]
        );
        handset.apply(&selected.end_conference(CallId(4)));
        assert_eq!(
            handset.call_states(),
            [
                (CallId(4), HandsetCallState::OnHook, true),
                (CallId(2), HandsetCallState::OnHook, true),
            ]
        );
        assert!(selected.call(CallId(3)).is_some());
        assert!(selected.invariant_error().is_none());

        let mut all = three_call_conference_controller();
        let effects = all.join_calls(&device, CallId(4), true).unwrap();
        handset.clear();
        handset.apply(&effects);
        assert!(handset.effects.is_empty());
        let session = all.conference_session(CallId(4)).unwrap();
        assert_eq!(
            session
                .participants
                .iter()
                .map(|participant| (participant.id, participant.handset_call_id))
                .collect::<Vec<_>>(),
            [
                (ParticipantId::new(1), CallId(4)),
                (ParticipantId::new(2), CallId(2)),
                (ParticipantId::new(3), CallId(3)),
            ]
        );
        handset.apply(&all.abort_join_conference(CallId(4), true, &[PbxCallId(8), PbxCallId(9)]));
        assert!(handset.effects.is_empty());
        assert!(all.conference_session(CallId(4)).is_none());
        assert_eq!(all.call(CallId(2)).unwrap().state, CallState::Held);
        assert_eq!(all.call(CallId(3)).unwrap().state, CallState::Held);
        assert_eq!(all.call(CallId(4)).unwrap().state, CallState::Connected);
        assert!(all.invariant_error().is_none());
    }

    #[test]
    fn fake_handset_participant_controls_commit_typed_ui_only_after_success() {
        let mut controller = active_three_party_conference_with_media();
        let device = binding().device_id;
        let other_device = DeviceId::new("SEP112233445566").unwrap();
        let conference_id = controller.conference_session(CallId(4)).unwrap().id;
        let original_ids = controller
            .conference_session(CallId(4))
            .unwrap()
            .participants
            .iter()
            .map(|participant| participant.id)
            .collect::<Vec<_>>();
        let initial_json = controller.conference_json(CallId(4)).unwrap();
        let mut handset = FakeHandsets::default();

        for rejection in [
            controller.begin_conference_participant_mute(
                &other_device,
                conference_id,
                ParticipantId::new(2),
                true,
            ),
            controller.begin_conference_participant_mute(
                &device,
                conference_id,
                ParticipantId::new(99),
                true,
            ),
            controller.begin_conference_participant_mute(
                &device,
                conference_id,
                ParticipantId::new(1),
                true,
            ),
        ] {
            assert!(rejection.is_err());
        }
        assert_eq!(controller.conference_json(CallId(4)).unwrap(), initial_json);

        let mute = controller
            .begin_conference_participant_mute(&device, conference_id, ParticipantId::new(2), true)
            .unwrap();
        handset.apply(&mute);
        assert!(handset.effects.is_empty());
        assert_eq!(controller.conference_json(CallId(4)).unwrap(), initial_json);
        assert!(controller.conference_participant_muted(
            conference_id,
            ParticipantId::new(2),
            true,
        ));
        handset.apply(&controller.conference_announcement_effects(
            conference_id,
            ConferenceAnnouncement::ParticipantMuted(ParticipantId::new(2)),
        ));
        assert_eq!(
            handset.announcements(),
            [(
                conference_id,
                vec![ParticipantId::new(2)],
                vec![PbxCallId(8)],
                ConferenceAnnouncement::ParticipantMuted(ParticipantId::new(2)),
            )]
        );

        handset.clear();
        controller
            .begin_conference_participant_mute(&device, conference_id, ParticipantId::new(2), false)
            .unwrap();
        assert!(controller.conference_participant_muted(
            conference_id,
            ParticipantId::new(2),
            false,
        ));
        handset.apply(&controller.conference_announcement_effects(
            conference_id,
            ConferenceAnnouncement::ParticipantUnmuted(ParticipantId::new(2)),
        ));
        assert_eq!(
            handset.announcements(),
            [(
                conference_id,
                vec![ParticipantId::new(2)],
                vec![PbxCallId(8)],
                ConferenceAnnouncement::ParticipantUnmuted(ParticipantId::new(2)),
            )]
        );

        assert!(
            controller
                .begin_conference_participant_role_change(
                    &device,
                    conference_id,
                    ParticipantId::new(2),
                    true,
                )
                .unwrap()
                .is_empty()
        );
        assert!(controller.conference_participant_role_changed(
            conference_id,
            ParticipantId::new(2),
            true,
        ));
        assert!(
            controller
                .begin_conference_participant_role_change(
                    &device,
                    conference_id,
                    ParticipantId::new(1),
                    false,
                )
                .unwrap()
                .is_empty()
        );
        assert!(controller.conference_participant_role_changed(
            conference_id,
            ParticipantId::new(1),
            false,
        ));
        let role_json: serde_json::Value =
            serde_json::from_str(&controller.conference_json(CallId(4)).unwrap()).unwrap();
        assert_eq!(role_json["moderator_id"], 2);
        assert_eq!(role_json["participants"][0]["moderator"], false);
        assert_eq!(role_json["participants"][1]["moderator"], true);
        assert_eq!(
            controller
                .conference_session(CallId(4))
                .unwrap()
                .participants
                .iter()
                .map(|participant| participant.id)
                .collect::<Vec<_>>(),
            original_ids
        );

        controller
            .begin_conference_participant_removal(&device, conference_id, ParticipantId::new(3))
            .unwrap();
        handset.clear();
        handset.apply(
            &controller
                .conference_participant_removed(conference_id, ParticipantId::new(3))
                .unwrap(),
        );
        assert_eq!(
            handset.call_states(),
            [(CallId(3), HandsetCallState::OnHook, true)]
        );
        let removed_json: serde_json::Value =
            serde_json::from_str(&controller.conference_json(CallId(4)).unwrap()).unwrap();
        assert_eq!(removed_json["participants"].as_array().unwrap().len(), 2);
        assert_eq!(removed_json["participants"][0]["id"], 1);
        assert_eq!(removed_json["participants"][1]["id"], 2);

        handset.clear();
        handset.apply(
            &controller
                .end_conference_by_moderator(&device, conference_id)
                .unwrap(),
        );
        assert_eq!(
            handset.call_states(),
            [
                (CallId(4), HandsetCallState::OnHook, true),
                (CallId(2), HandsetCallState::OnHook, true),
            ]
        );
        assert!(controller.conference_json(CallId(4)).is_none());
        assert_eq!(
            controller.end_conference_by_moderator(&device, conference_id),
            Err(ConferenceEndRejection::Unavailable)
        );
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn fake_handset_hold_departure_failure_destination_and_shutdown_are_idempotent() {
        let mut handset = FakeHandsets::default();
        let mut held = active_three_party_conference_with_media();
        let conference_id = held.conference_session(CallId(4)).unwrap().id;
        let stable = held
            .conference_session(CallId(4))
            .map(|session| (session.bridge_id, session.participants.clone()))
            .unwrap();
        handset.apply(
            &held
                .begin_conference_moderator_leg_transition(CallId(4), true)
                .unwrap(),
        );
        assert_eq!(
            handset.call_states(),
            [(CallId(4), HandsetCallState::Hold, true)]
        );
        assert!(held.conference_moderator_leg_transitioned(
            conference_id,
            ParticipantId::new(1),
            true,
        ));
        handset.clear();
        handset.apply(
            &held
                .begin_conference_moderator_leg_transition(CallId(4), false)
                .unwrap(),
        );
        assert_eq!(handset.media_winners(), [CallId(4)]);
        assert!(held.conference_moderator_leg_transitioned(
            conference_id,
            ParticipantId::new(1),
            false,
        ));
        let resumed = held.conference_session_by_id(conference_id).unwrap();
        assert_eq!(resumed.bridge_id, stable.0);
        assert_eq!(resumed.participants, stable.1);

        let device = binding().device_id;
        held.begin_conference_participant_role_change(
            &device,
            conference_id,
            ParticipantId::new(2),
            true,
        )
        .unwrap();
        assert!(held.conference_participant_role_changed(
            conference_id,
            ParticipantId::new(2),
            true,
        ));
        handset.clear();
        let departure = held.pbx_hangup_with_effects(PbxCallId(10)).unwrap();
        handset.apply(&departure.effects);
        assert_eq!(
            handset.call_states(),
            [(CallId(4), HandsetCallState::OnHook, true)]
        );
        assert_eq!(
            handset.announcements(),
            [(
                conference_id,
                vec![ParticipantId::new(2), ParticipantId::new(3)],
                vec![PbxCallId(8), PbxCallId(9)],
                ConferenceAnnouncement::ModeratorDeparted(ParticipantId::new(1)),
            )]
        );
        let survivors = held.conference_session_by_id(conference_id).unwrap();
        assert_eq!(survivors.bridge_id, stable.0);
        assert_eq!(
            survivors
                .participants
                .iter()
                .map(|participant| participant.id)
                .collect::<Vec<_>>(),
            [ParticipantId::new(2), ParticipantId::new(3)]
        );
        assert!(held.pbx_hangup_with_effects(PbxCallId(10)).is_none());

        let mut destination = connected_outbound_controller();
        destination.begin_phone_call(CallId(2), binding(), Codec::Pcmu, Instant::now());
        handset.clear();
        let effects = destination
            .begin_conference_destination(ConferenceDestinationRequest {
                device_id: binding().device_id,
                handset_call_id: CallId(2),
                destination: "700".into(),
                application_options: "Mac".into(),
            })
            .unwrap();
        let mutation = destination
            .conference_destination_mutation(CallId(2))
            .unwrap();
        handset.apply(&effects);
        assert_eq!(
            handset.call_states(),
            [
                (CallId(1), HandsetCallState::Hold, true),
                (CallId(2), HandsetCallState::Proceed, false),
            ]
        );
        assert_eq!(handset.tones(CallId(2)), [Tone::Silence]);
        let info = handset.call_info(CallId(2));
        assert_eq!(info.last().unwrap().called_name, "Conference");
        assert_eq!(info.last().unwrap().called_number, "700");
        handset.clear();
        handset.apply(&destination.conference_destination_failed(
            mutation,
            CallId(2),
            &[PbxCallId(1)],
            &[PbxCallId(1)],
        ));
        assert_eq!(
            handset.call_states(),
            [(CallId(2), HandsetCallState::OnHook, true)]
        );
        assert_eq!(handset.media_winners(), [CallId(1)]);
        assert!(destination.call(CallId(2)).is_none());
        assert_eq!(
            destination.call(CallId(1)).unwrap().state,
            CallState::Connected
        );

        let mut shutdown = active_three_party_conference_with_media();
        shutdown
            .begin_conference_invite(CallId(4), CallId(5), binding(), Codec::Pcma, Instant::now())
            .unwrap();
        handset.clear();
        let plans = shutdown.drain_conferences_for_shutdown();
        assert_eq!(plans.len(), 1);
        handset.apply(&plans[0].effects);
        assert_eq!(
            handset.call_states(),
            [
                (CallId(4), HandsetCallState::OnHook, true),
                (CallId(2), HandsetCallState::OnHook, true),
                (CallId(3), HandsetCallState::OnHook, true),
                (CallId(5), HandsetCallState::OnHook, true),
            ]
        );
        assert!(shutdown.drain_conferences_for_shutdown().is_empty());
        assert!(shutdown.calls().next().is_none());
        assert!(held.invariant_error().is_none());
        assert!(destination.invariant_error().is_none());
        assert!(shutdown.invariant_error().is_none());
    }

    #[test]
    fn fake_handset_partial_failures_and_disconnect_release_only_owned_presentations() {
        let mut handset = FakeHandsets::default();

        let mut consultation = connected_outbound_controller();
        consultation
            .begin_conference(
                CallId(1),
                CallId(2),
                binding(),
                Codec::Pcmu,
                Instant::now(),
                true,
            )
            .unwrap();
        handset.apply(&consultation.abort_conference(CallId(2), true, true, true, true));
        assert_eq!(
            handset.call_states(),
            [(CallId(2), HandsetCallState::OnHook, true)]
        );
        assert_eq!(handset.media_winners(), [CallId(1)]);
        assert!(consultation.conference_session(CallId(1)).is_none());

        let mut mutation = active_three_party_conference_with_media();
        let device = binding().device_id;
        let conference_id = mutation.conference_session(CallId(4)).unwrap().id;
        let initial_json = mutation.conference_json(CallId(4)).unwrap();
        let effects = mutation
            .begin_conference_participant_mute(&device, conference_id, ParticipantId::new(2), true)
            .unwrap();
        handset.clear();
        handset.apply(&effects);
        assert!(handset.effects.is_empty());
        assert!(mutation.abort_conference_participant_mute(
            conference_id,
            ParticipantId::new(2),
            true,
        ));
        assert_eq!(mutation.conference_json(CallId(4)).unwrap(), initial_json);

        mutation
            .begin_conference_participant_removal(&device, conference_id, ParticipantId::new(2))
            .unwrap();
        assert!(
            mutation.abort_conference_participant_removal(conference_id, ParticipantId::new(2))
        );
        assert_eq!(mutation.conference_json(CallId(4)).unwrap(), initial_json);

        mutation
            .begin_conference_participant_role_change(
                &device,
                conference_id,
                ParticipantId::new(2),
                true,
            )
            .unwrap();
        assert!(mutation.abort_conference_participant_role_change(
            conference_id,
            ParticipantId::new(2),
            true,
        ));
        assert_eq!(mutation.conference_json(CallId(4)).unwrap(), initial_json);

        let hold = mutation
            .begin_conference_moderator_leg_transition(CallId(4), true)
            .unwrap();
        handset.apply(&hold);
        let rollback = mutation.abort_conference_moderator_leg_transition(
            conference_id,
            ParticipantId::new(1),
            true,
            &[ParticipantId::new(2)],
            true,
        );
        handset.apply(&rollback);
        assert_eq!(
            handset.call_states(),
            [(CallId(4), HandsetCallState::Hold, true)]
        );
        assert_eq!(handset.media_winners(), [CallId(4)]);
        assert_eq!(mutation.conference_json(CallId(4)).unwrap(), initial_json);

        let mut failed = active_three_party_conference_with_media();
        let failed_id = failed.conference_session(CallId(4)).unwrap().id;
        handset.clear();
        let outcome = failed.conference_participant_failed(CallId(2)).unwrap();
        handset.apply(&outcome.effects);
        assert_eq!(
            handset.call_states(),
            [(CallId(2), HandsetCallState::OnHook, true)]
        );
        assert_eq!(outcome.call_ids, [PbxCallId(8)]);
        assert_eq!(
            outcome
                .surviving_session
                .unwrap()
                .participants
                .iter()
                .map(|participant| participant.id)
                .collect::<Vec<_>>(),
            [ParticipantId::new(1), ParticipantId::new(3)]
        );
        assert!(failed.conference_participant_failed(CallId(2)).is_none());
        assert!(failed.conference_session_by_id(failed_id).is_some());

        let mut disconnected = active_three_party_conference_with_media();
        handset.clear();
        handset.apply(&disconnected.disconnected(&device));
        assert_eq!(
            handset.call_states(),
            [
                (CallId(4), HandsetCallState::OnHook, true),
                (CallId(2), HandsetCallState::OnHook, true),
                (CallId(3), HandsetCallState::OnHook, true),
            ]
        );
        assert!(handset.announcements().is_empty());
        assert!(disconnected.calls().next().is_none());
        assert!(consultation.invariant_error().is_none());
        assert!(mutation.invariant_error().is_none());
        assert!(failed.invariant_error().is_none());
        assert!(disconnected.invariant_error().is_none());

        let mut raced = active_three_party_conference_with_media();
        let raced_id = raced.conference_session(CallId(4)).unwrap().id;
        raced
            .begin_conference_participant_mute(&device, raced_id, ParticipantId::new(3), true)
            .unwrap();
        assert_eq!(
            raced.begin_conference_participant_removal(&device, raced_id, ParticipantId::new(2),),
            Err(ConferenceParticipantRejection::Conflict)
        );
        assert_eq!(
            raced.begin_conference_invite(
                CallId(4),
                CallId(5),
                binding(),
                Codec::Pcma,
                Instant::now(),
            ),
            Err(ConferenceRejection::Conflict)
        );
        assert_eq!(
            raced.end_conference_by_moderator(&device, raced_id),
            Err(ConferenceEndRejection::Conflict)
        );
        handset.clear();
        let outcome = raced.conference_participant_failed(CallId(2)).unwrap();
        handset.apply(&outcome.effects);
        assert!(outcome.surviving_session.is_none());
        assert_eq!(
            handset.call_states(),
            [
                (CallId(4), HandsetCallState::OnHook, true),
                (CallId(2), HandsetCallState::OnHook, true),
                (CallId(3), HandsetCallState::OnHook, true),
            ]
        );
        assert!(!raced.conference_participant_muted(raced_id, ParticipantId::new(3), true,));
        assert!(raced.calls().next().is_none());
        assert!(raced.invariant_error().is_none());
    }

    #[test]
    fn conference_media_policy_is_captured_and_announcements_follow_category_flags() {
        let mut controller = active_three_party_conference_with_media();
        let session = controller.conference_session(CallId(4)).unwrap().clone();
        assert_eq!(
            controller
                .conference_announcement_effects(session.id, ConferenceAnnouncement::Connected,),
            [DriverEffect::Backend(PbxEffect::ConferenceAnnouncement {
                operation: ConferenceAnnouncementOperation {
                    conference_id: session.id,
                    targets: vec![
                        ConferenceAnnouncementTarget {
                            participant_id: ParticipantId::new(1),
                            call_id: PbxCallId(10)
                        },
                        ConferenceAnnouncementTarget {
                            participant_id: ParticipantId::new(2),
                            call_id: PbxCallId(8)
                        },
                        ConferenceAnnouncementTarget {
                            participant_id: ParticipantId::new(3),
                            call_id: PbxCallId(9)
                        },
                    ],
                    announcement: ConferenceAnnouncement::Connected,
                },
            })]
        );
        assert_eq!(
            controller.conference_announcement_effects(
                session.id,
                ConferenceAnnouncement::ParticipantMuted(ParticipantId::new(2)),
            ),
            [DriverEffect::Backend(PbxEffect::ConferenceAnnouncement {
                operation: ConferenceAnnouncementOperation {
                    conference_id: session.id,
                    targets: vec![ConferenceAnnouncementTarget {
                        participant_id: ParticipantId::new(2),
                        call_id: PbxCallId(8)
                    }],
                    announcement: ConferenceAnnouncement::ParticipantMuted(ParticipantId::new(2)),
                },
            })]
        );
        assert!(
            !controller.configure_conference_media(CallId(4), ConferenceMediaPolicy::default(),)
        );

        let disabled = active_three_party_conference();
        let disabled_id = disabled.conference_session(CallId(4)).unwrap().id;
        assert!(
            disabled
                .conference_announcement_effects(disabled_id, ConferenceAnnouncement::Connected)
                .is_empty()
        );
        assert!(controller.invariant_error().is_none());
        assert!(disabled.invariant_error().is_none());
    }

    #[test]
    fn moderator_leg_hold_and_resume_preserve_bridge_calls_ids_and_json() {
        let mut controller = active_three_party_conference_with_media();
        let session = controller.conference_session(CallId(4)).unwrap().clone();
        let bridge_id = session.bridge_id;
        let participant_ids = session
            .participants
            .iter()
            .map(|participant| participant.id)
            .collect::<Vec<_>>();
        assert_eq!(
            controller.begin_conference_moderator_leg_transition(CallId(2), true),
            Err(ConferenceParticipantRejection::NotModerator)
        );
        assert!(controller.resume(CallId(4)).is_empty());

        let hold = controller
            .begin_conference_moderator_leg_transition(CallId(4), true)
            .unwrap();
        assert!(matches!(
            hold.first(),
            Some(DriverEffect::Handset(HandsetEffect::SetCallState {
                call_id: CallId(4),
                state: HandsetCallState::Hold,
                stop_media: true,
                ..
            }))
        ));
        assert_eq!(
            hold.iter()
                .filter_map(|effect| match effect {
                    DriverEffect::Backend(PbxEffect::Bridge {
                        operation:
                            crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold {
                                participant_id,
                                enabled: true,
                                ..
                            },
                    }) => Some(*participant_id),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            [ParticipantId::new(2), ParticipantId::new(3)]
        );
        assert!(
            !hold
                .iter()
                .any(|effect| matches!(effect, DriverEffect::Backend(PbxEffect::Hold { .. })))
        );
        assert_eq!(
            controller.pbx_call(PbxCallId(10)).unwrap().state,
            CallState::Connected
        );
        assert!(controller.conference_moderator_leg_transitioned(
            session.id,
            ParticipantId::new(1),
            true,
        ));
        assert_eq!(controller.call(CallId(4)).unwrap().state, CallState::Held);
        assert_eq!(
            controller.pbx_call(PbxCallId(10)).unwrap().state,
            CallState::Connected
        );
        assert_eq!(
            controller
                .conference_session_by_id(session.id)
                .unwrap()
                .participants
                .active_moderator_count(),
            0
        );
        let held_json: serde_json::Value =
            serde_json::from_str(&controller.conference_json(CallId(4)).unwrap()).unwrap();
        assert_eq!(held_json["participants"][0]["held"], true);

        let resume = controller
            .begin_conference_moderator_leg_transition(CallId(4), false)
            .unwrap();
        assert_eq!(
            resume
                .iter()
                .filter_map(|effect| match effect {
                    DriverEffect::Backend(PbxEffect::Bridge {
                        operation:
                            crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold {
                                participant_id,
                                enabled: false,
                                ..
                            },
                    }) => Some(*participant_id),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            [ParticipantId::new(2), ParticipantId::new(3)]
        );
        assert!(matches!(
            resume.last(),
            Some(DriverEffect::Handset(HandsetEffect::BeginMedia {
                call_id: CallId(4),
                ..
            }))
        ));
        assert!(
            !resume
                .iter()
                .any(|effect| matches!(effect, DriverEffect::Backend(PbxEffect::Resume { .. })))
        );
        assert!(controller.conference_moderator_leg_transitioned(
            session.id,
            ParticipantId::new(1),
            false,
        ));
        let resumed = controller.conference_session_by_id(session.id).unwrap();
        assert_eq!(resumed.bridge_id, bridge_id);
        assert_eq!(
            resumed
                .participants
                .iter()
                .map(|participant| participant.id)
                .collect::<Vec<_>>(),
            participant_ids
        );
        assert_eq!(
            controller.call(CallId(4)).unwrap().state,
            CallState::Connected
        );
        assert_eq!(
            controller.call(CallId(4)).unwrap().audio,
            MediaStreamState::Opening
        );
        let resumed_json: serde_json::Value =
            serde_json::from_str(&controller.conference_json(CallId(4)).unwrap()).unwrap();
        assert_eq!(resumed_json["participants"][0]["held"], false);
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn moderator_leg_failure_rolls_back_only_completed_handset_and_music_work() {
        let mut controller = active_three_party_conference_with_media();
        let conference_id = controller.conference_session(CallId(4)).unwrap().id;
        controller
            .begin_conference_moderator_leg_transition(CallId(4), true)
            .unwrap();
        let rollback = controller.abort_conference_moderator_leg_transition(
            conference_id,
            ParticipantId::new(1),
            true,
            &[ParticipantId::new(2)],
            true,
        );
        assert!(matches!(
            rollback.as_slice(),
            [
                DriverEffect::Backend(PbxEffect::Bridge {
                    operation:
                        crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold {
                            participant_id,
                            enabled: false,
                            ..
                        }
                }),
                DriverEffect::Handset(HandsetEffect::BeginMedia {
                    call_id: CallId(4),
                    ..
                })
            ] if *participant_id == ParticipantId::new(2)
        ));
        assert!(
            !controller
                .conference_session_by_id(conference_id)
                .unwrap()
                .participants
                .get(ParticipantId::new(1))
                .unwrap()
                .held
        );
        assert_eq!(
            controller.call(CallId(4)).unwrap().state,
            CallState::Connected
        );

        controller
            .begin_conference_moderator_leg_transition(CallId(4), true)
            .unwrap();
        assert!(controller.conference_moderator_leg_transitioned(
            conference_id,
            ParticipantId::new(1),
            true,
        ));
        controller
            .begin_conference_moderator_leg_transition(CallId(4), false)
            .unwrap();
        let rollback = controller.abort_conference_moderator_leg_transition(
            conference_id,
            ParticipantId::new(1),
            false,
            &[ParticipantId::new(2)],
            true,
        );
        assert!(matches!(
            rollback.as_slice(),
            [
                DriverEffect::Handset(HandsetEffect::SetCallState {
                    call_id: CallId(4),
                    state: HandsetCallState::Hold,
                    stop_media: true,
                    ..
                }),
                DriverEffect::Backend(PbxEffect::Bridge {
                    operation:
                        crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold {
                            participant_id,
                            enabled: true,
                            ..
                        }
                })
            ] if *participant_id == ParticipantId::new(2)
        ));
        assert!(
            controller
                .conference_session_by_id(conference_id)
                .unwrap()
                .participants
                .get(ParticipantId::new(1))
                .unwrap()
                .held
        );
        assert_eq!(controller.call(CallId(4)).unwrap().state, CallState::Held);
        controller
            .begin_conference_moderator_leg_transition(CallId(4), false)
            .unwrap();
        assert!(
            controller
                .abort_conference_moderator_leg_transition(
                    conference_id,
                    ParticipantId::new(1),
                    false,
                    &[],
                    false,
                )
                .is_empty()
        );
        assert_eq!(
            controller.call(CallId(4)).unwrap().audio,
            MediaStreamState::Closed
        );
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn multiple_moderator_legs_change_music_only_at_the_listening_boundary() {
        let mut controller = active_three_party_conference_with_media();
        let device = binding().device_id;
        let conference_id = controller.conference_session(CallId(4)).unwrap().id;
        controller
            .begin_conference_participant_role_change(
                &device,
                conference_id,
                ParticipantId::new(2),
                true,
            )
            .unwrap();
        assert!(controller.conference_participant_role_changed(
            conference_id,
            ParticipantId::new(2),
            true,
        ));

        let first_hold = controller
            .begin_conference_moderator_leg_transition(CallId(4), true)
            .unwrap();
        assert!(
            !first_hold.iter().any(|effect| matches!(
                effect,
                DriverEffect::Backend(PbxEffect::Bridge {
                    operation:
                        crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold { .. }
                })
            ))
        );
        assert!(controller.conference_moderator_leg_transitioned(
            conference_id,
            ParticipantId::new(1),
            true,
        ));

        let last_hold = controller
            .begin_conference_moderator_leg_transition(CallId(2), true)
            .unwrap();
        assert_eq!(
            last_hold
                .iter()
                .filter(|effect| matches!(
                    effect,
                    DriverEffect::Backend(PbxEffect::Bridge {
                        operation:
                            crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold {
                                enabled: true,
                                ..
                            }
                    })
                ))
                .count(),
            1
        );
        assert!(controller.conference_moderator_leg_transitioned(
            conference_id,
            ParticipantId::new(2),
            true,
        ));

        let first_resume = controller
            .begin_conference_moderator_leg_transition(CallId(4), false)
            .unwrap();
        assert_eq!(
            first_resume
                .iter()
                .filter(|effect| matches!(
                    effect,
                    DriverEffect::Backend(PbxEffect::Bridge {
                        operation:
                            crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold {
                                enabled: false,
                                ..
                            }
                    })
                ))
                .count(),
            1
        );
        assert!(controller.conference_moderator_leg_transitioned(
            conference_id,
            ParticipantId::new(1),
            false,
        ));
        let session = controller.conference_session_by_id(conference_id).unwrap();
        assert_eq!(session.bridge_id, PbxBridgeId(1));
        assert_eq!(session.participants.moderator_count(), 2);
        assert_eq!(session.participants.active_moderator_count(), 1);
        assert!(
            session
                .participants
                .get(ParticipantId::new(2))
                .unwrap()
                .held
        );
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn moderator_leg_with_disabled_music_changes_only_the_handset_leg() {
        let mut controller = active_three_party_conference();
        let conference_id = controller.conference_session(CallId(4)).unwrap().id;
        let hold = controller
            .begin_conference_moderator_leg_transition(CallId(4), true)
            .unwrap();
        assert!(matches!(
            hold.as_slice(),
            [DriverEffect::Handset(HandsetEffect::SetCallState {
                call_id: CallId(4),
                state: HandsetCallState::Hold,
                ..
            })]
        ));
        assert!(controller.conference_moderator_leg_transitioned(
            conference_id,
            ParticipantId::new(1),
            true,
        ));
        let resume = controller
            .begin_conference_moderator_leg_transition(CallId(4), false)
            .unwrap();
        assert!(matches!(
            resume.as_slice(),
            [DriverEffect::Handset(HandsetEffect::BeginMedia {
                call_id: CallId(4),
                ..
            })]
        ));
        assert!(controller.conference_moderator_leg_transitioned(
            conference_id,
            ParticipantId::new(1),
            false,
        ));
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn moderator_leg_transition_serializes_mutation_end_and_departure_races() {
        let mut controller = active_three_party_conference_with_media();
        let device = binding().device_id;
        let conference_id = controller.conference_session(CallId(4)).unwrap().id;
        controller
            .begin_conference_moderator_leg_transition(CallId(4), true)
            .unwrap();
        assert_eq!(
            controller.begin_conference_participant_mute(
                &device,
                conference_id,
                ParticipantId::new(2),
                true,
            ),
            Err(ConferenceParticipantRejection::Conflict)
        );
        assert_eq!(
            controller.end_conference_by_moderator(&device, conference_id),
            Err(ConferenceEndRejection::Conflict)
        );

        let outcome = controller
            .pbx_hangup_with_effects(PbxCallId(8))
            .expect("departure is consumed while hold is pending");
        assert!(outcome.effects.iter().any(|effect| matches!(
            effect,
            DriverEffect::Backend(PbxEffect::Bridge {
                operation: crate::runtime::backend::BridgeOperation::Destroy { .. }
            })
        )));
        assert!(controller.conference_session_by_id(conference_id).is_none());
        assert!(!controller.conference_moderator_leg_transitioned(
            conference_id,
            ParticipantId::new(1),
            true,
        ));
        assert!(
            controller
                .abort_conference_moderator_leg_transition(
                    conference_id,
                    ParticipantId::new(1),
                    true,
                    &[],
                    true,
                )
                .is_empty()
        );
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn general_and_personal_conference_announcements_have_exact_audiences() {
        let device = binding().device_id;
        let mut general = three_call_conference_controller();
        general
            .join_calls_with_media(
                &device,
                CallId(4),
                true,
                ConferenceMediaPolicy {
                    music_on_hold_class: None,
                    mute_on_entry: false,
                    play_general_announcements: true,
                    play_participant_announcements: false,
                },
            )
            .unwrap();
        assert!(general.conference_merged(CallId(4)));
        let general_session = general.conference_session(CallId(4)).unwrap().clone();
        general
            .begin_conference_participant_removal(
                &device,
                general_session.id,
                ParticipantId::new(2),
            )
            .unwrap();
        assert!(
            general
                .conference_participant_removed(general_session.id, ParticipantId::new(2))
                .is_some()
        );
        assert_eq!(
            general.conference_announcement_effects(
                general_session.id,
                ConferenceAnnouncement::ParticipantRemoved(ParticipantId::new(2)),
            ),
            [DriverEffect::Backend(PbxEffect::ConferenceAnnouncement {
                operation: ConferenceAnnouncementOperation {
                    conference_id: general_session.id,
                    targets: vec![
                        ConferenceAnnouncementTarget {
                            participant_id: ParticipantId::new(1),
                            call_id: PbxCallId(10)
                        },
                        ConferenceAnnouncementTarget {
                            participant_id: ParticipantId::new(3),
                            call_id: PbxCallId(9)
                        },
                    ],
                    announcement: ConferenceAnnouncement::ParticipantRemoved(ParticipantId::new(2),),
                },
            })]
        );
        assert!(
            general
                .conference_announcement_effects(
                    general_session.id,
                    ConferenceAnnouncement::ParticipantMuted(ParticipantId::new(2)),
                )
                .is_empty()
        );

        let mut personal = three_call_conference_controller();
        personal
            .join_calls_with_media(
                &device,
                CallId(4),
                true,
                ConferenceMediaPolicy {
                    music_on_hold_class: None,
                    mute_on_entry: false,
                    play_general_announcements: false,
                    play_participant_announcements: true,
                },
            )
            .unwrap();
        assert!(personal.conference_merged(CallId(4)));
        let personal_id = personal.conference_session(CallId(4)).unwrap().id;
        assert!(
            personal
                .conference_announcement_effects(
                    personal_id,
                    ConferenceAnnouncement::ParticipantJoined(ParticipantId::new(2)),
                )
                .is_empty()
        );
        assert_eq!(
            personal
                .conference_announcement_effects(
                    personal_id,
                    ConferenceAnnouncement::ParticipantUnmuted(ParticipantId::new(3)),
                )
                .len(),
            1
        );
        assert!(general.invariant_error().is_none());
        assert!(personal.invariant_error().is_none());
    }

    #[test]
    fn moderator_invite_starts_and_every_exit_stops_configured_music_exactly() {
        let mut controller = active_three_party_conference_with_media();
        let effects = controller
            .begin_conference_invite(CallId(4), CallId(5), binding(), Codec::Pcma, Instant::now())
            .unwrap();
        let starts: Vec<_> = effects
            .iter()
            .filter_map(|effect| match effect {
                DriverEffect::Backend(PbxEffect::Bridge {
                    operation:
                        crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold {
                            participant_id,
                            call_id,
                            class,
                            enabled: true,
                            ..
                        },
                }) => Some((*participant_id, *call_id, class.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            starts,
            [
                (ParticipantId::new(2), PbxCallId(8), "office"),
                (ParticipantId::new(3), PbxCallId(9), "office"),
            ]
        );

        let cleanup = controller.abort_conference_invite(CallId(5), false, true, false);
        let stops: Vec<_> = cleanup
            .iter()
            .filter_map(|effect| match effect {
                DriverEffect::Backend(PbxEffect::Bridge {
                    operation:
                        crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold {
                            participant_id,
                            enabled: false,
                            ..
                        },
                }) => Some(*participant_id),
                _ => None,
            })
            .collect();
        assert_eq!(stops, [ParticipantId::new(2), ParticipantId::new(3)]);
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn successful_invite_stops_music_before_moderator_resume_and_bridge_merge() {
        let mut controller = active_three_party_conference_with_media();
        controller
            .begin_conference_invite(CallId(4), CallId(5), binding(), Codec::Pcma, Instant::now())
            .unwrap();
        controller.enbloc(CallId(5), "2200".into());
        let invite_pbx = controller.call(CallId(5)).unwrap().pbx_id;
        controller.pbx_answer(invite_pbx);

        let effects = controller.confirm_conference_invite(CallId(5)).unwrap();
        assert!(matches!(
            effects.as_slice(),
            [
                DriverEffect::Backend(PbxEffect::Bridge {
                    operation:
                        crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold {
                            participant_id: first,
                            enabled: false,
                            ..
                        },
                }),
                DriverEffect::Backend(PbxEffect::Bridge {
                    operation:
                        crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold {
                            participant_id: second,
                            enabled: false,
                            ..
                        },
                }),
                DriverEffect::Backend(PbxEffect::Resume {
                    call_id: PbxCallId(10),
                }),
                DriverEffect::Backend(PbxEffect::Bridge {
                    operation: crate::runtime::backend::BridgeOperation::MergeParticipant { .. },
                }),
            ] if *first == ParticipantId::new(2) && *second == ParticipantId::new(3)
        ));
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn explicitly_disabled_conference_music_emits_no_music_operations() {
        let mut controller = active_three_party_conference();
        let effects = controller
            .begin_conference_invite(CallId(4), CallId(5), binding(), Codec::Pcma, Instant::now())
            .unwrap();
        assert!(
            !effects.iter().any(|effect| matches!(
                effect,
                DriverEffect::Backend(PbxEffect::Bridge {
                    operation:
                        crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold { .. },
                })
            ))
        );
        let cleanup = controller.abort_conference_invite(CallId(5), false, true, false);
        assert!(
            !cleanup.iter().any(|effect| matches!(
                effect,
                DriverEffect::Backend(PbxEffect::Bridge {
                    operation:
                        crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold { .. },
                })
            ))
        );
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn moderator_invite_adds_one_stable_participant_to_the_live_bridge() {
        let mut controller = active_three_party_conference();
        assert_eq!(
            controller.begin_conference_invite(
                CallId(2),
                CallId(5),
                binding(),
                Codec::Pcma,
                Instant::now(),
            ),
            Err(ConferenceRejection::Disabled)
        );

        let effects = controller
            .begin_conference_invite(CallId(4), CallId(5), binding(), Codec::Pcma, Instant::now())
            .unwrap();
        assert!(matches!(
            effects.first(),
            Some(DriverEffect::Backend(PbxEffect::Hold {
                call_id: PbxCallId(10)
            }))
        ));
        let pending = controller
            .conference_session(CallId(5))
            .unwrap()
            .pending_invite
            .as_ref()
            .unwrap()
            .participant
            .clone();
        assert_eq!(pending.id, ParticipantId::new(4));
        assert_eq!(pending.handset_call_id, CallId(5));

        controller.enbloc(CallId(5), "2200".into());
        controller.pbx_answer(pending.pbx_call_id);
        assert_eq!(
            controller.confirm_conference_invite(CallId(5)).unwrap(),
            [
                DriverEffect::Backend(PbxEffect::Resume {
                    call_id: PbxCallId(10),
                }),
                DriverEffect::Backend(PbxEffect::Bridge {
                    operation: crate::runtime::backend::BridgeOperation::MergeParticipant {
                        bridge_id: PbxBridgeId(1),
                        call_id: pending.pbx_call_id,
                    },
                }),
            ]
        );
        assert!(controller.conference_invite_merged(CallId(5)));
        let session = controller.conference_session(CallId(5)).unwrap();
        assert!(session.pending_invite.is_none());
        assert_eq!(session.participants.iter().len(), 4);
        assert_eq!(
            session
                .participants
                .by_pbx(pending.pbx_call_id)
                .map(|entry| entry.id),
            Some(ParticipantId::new(4))
        );
        assert_eq!(
            controller
                .conference_session(CallId(4))
                .unwrap()
                .participants
                .moderator()
                .unwrap()
                .id,
            ParticipantId::new(1)
        );
        let cleanup = controller.end_conference(CallId(5));
        assert_eq!(
            cleanup
                .iter()
                .filter(|effect| matches!(effect, DriverEffect::Backend(PbxEffect::Hangup { .. })))
                .count(),
            4
        );
        assert_eq!(
            cleanup
                .iter()
                .filter(|effect| matches!(
                    effect,
                    DriverEffect::Backend(PbxEffect::Bridge {
                        operation: crate::runtime::backend::BridgeOperation::Destroy { .. }
                    })
                ))
                .count(),
            1
        );
        assert!(controller.calls().next().is_none());
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn secondary_moderator_invite_targets_the_exact_initiating_leg() {
        let mut controller = active_three_party_conference_with_media();
        let device = binding().device_id;
        let conference_id = controller.conference_session(CallId(4)).unwrap().id;
        controller
            .begin_conference_participant_role_change(
                &device,
                conference_id,
                ParticipantId::new(2),
                true,
            )
            .unwrap();
        assert!(controller.conference_participant_role_changed(
            conference_id,
            ParticipantId::new(2),
            true,
        ));
        let stable = controller
            .conference_session(CallId(4))
            .unwrap()
            .participants
            .iter()
            .map(|participant| {
                (
                    participant.id,
                    participant.pbx_call_id,
                    participant.handset_call_id,
                    participant.moderator,
                )
            })
            .collect::<Vec<_>>();

        let effects = controller
            .begin_conference_invite(CallId(2), CallId(5), binding(), Codec::Pcma, Instant::now())
            .unwrap();
        assert!(matches!(
            effects.as_slice(),
            [
                DriverEffect::Backend(PbxEffect::Hold {
                    call_id: PbxCallId(8),
                }),
                DriverEffect::Handset(HandsetEffect::SetCallState {
                    call_id: CallId(2),
                    state: HandsetCallState::Hold,
                    stop_media: true,
                    ..
                }),
                DriverEffect::Handset(HandsetEffect::BeginCall {
                    call_id: CallId(5),
                    line_instance: 1,
                    codec: Codec::Pcma,
                    ..
                }),
                DriverEffect::Backend(PbxEffect::CreateChannel { .. }),
            ]
        ));
        assert!(
            !effects.iter().any(|effect| matches!(
                effect,
                DriverEffect::Backend(PbxEffect::Bridge {
                    operation:
                        crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold { .. },
                })
            ))
        );
        let pending = controller
            .conference_session(CallId(5))
            .unwrap()
            .pending_invite
            .as_ref()
            .unwrap();
        assert_eq!(pending.moderator_id, ParticipantId::new(2));
        assert_eq!(pending.moderator_call_id, PbxCallId(8));
        assert!(!pending.music_started);

        controller.enbloc(CallId(5), "2400".into());
        controller.pbx_answer(PbxCallId(11));
        assert_eq!(
            controller.confirm_conference_invite(CallId(5)).unwrap(),
            [
                DriverEffect::Backend(PbxEffect::Resume {
                    call_id: PbxCallId(8),
                }),
                DriverEffect::Backend(PbxEffect::Bridge {
                    operation: crate::runtime::backend::BridgeOperation::MergeParticipant {
                        bridge_id: PbxBridgeId(1),
                        call_id: PbxCallId(11),
                    },
                }),
            ]
        );
        assert!(controller.conference_invite_merged(CallId(5)));
        let session = controller.conference_session(CallId(5)).unwrap();
        assert_eq!(
            session
                .participants
                .iter()
                .take(3)
                .map(|participant| {
                    (
                        participant.id,
                        participant.pbx_call_id,
                        participant.handset_call_id,
                        participant.moderator,
                    )
                })
                .collect::<Vec<_>>(),
            stable
        );
        assert_eq!(session.participants.iter().len(), 4);
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn secondary_moderator_invite_abort_restores_only_its_leg() {
        let mut controller = active_three_party_conference_with_media();
        let device = binding().device_id;
        let conference_id = controller.conference_session(CallId(4)).unwrap().id;
        controller
            .begin_conference_participant_role_change(
                &device,
                conference_id,
                ParticipantId::new(2),
                true,
            )
            .unwrap();
        assert!(controller.conference_participant_role_changed(
            conference_id,
            ParticipantId::new(2),
            true,
        ));
        controller
            .begin_conference_moderator_leg_transition(CallId(4), true)
            .unwrap();
        assert!(controller.conference_moderator_leg_transitioned(
            conference_id,
            ParticipantId::new(1),
            true,
        ));
        let start = controller
            .begin_conference_invite(CallId(2), CallId(5), binding(), Codec::Pcma, Instant::now())
            .unwrap();
        assert!(start.iter().any(|effect| matches!(
            effect,
            DriverEffect::Backend(PbxEffect::Bridge {
                operation: crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold {
                    participant_id,
                    call_id: PbxCallId(9),
                    class,
                    enabled: true,
                    ..
                },
            }) if *participant_id == ParticipantId::new(3) && class == "office"
        )));
        assert_eq!(
            start
                .iter()
                .filter(|effect| matches!(
                    effect,
                    DriverEffect::Backend(PbxEffect::Bridge {
                        operation:
                            crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold { .. },
                    })
                ))
                .count(),
            1
        );

        let cleanup = controller.abort_conference_invite(CallId(5), true, true, true);
        assert!(matches!(
            cleanup.as_slice(),
            [
                DriverEffect::Backend(PbxEffect::Bridge {
                    operation:
                        crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold {
                            participant_id,
                            call_id: PbxCallId(9),
                            class,
                            enabled: false,
                            ..
                        },
                }),
                DriverEffect::Backend(PbxEffect::Hangup {
                    call_id: PbxCallId(11),
                }),
                DriverEffect::Backend(PbxEffect::Resume {
                    call_id: PbxCallId(8),
                }),
                DriverEffect::Handset(HandsetEffect::SetCallState {
                    call_id: CallId(5),
                    state: HandsetCallState::OnHook,
                    stop_media: true,
                    ..
                }),
                DriverEffect::Handset(HandsetEffect::BeginMedia {
                    call_id: CallId(2),
                    ..
                }),
            ] if *participant_id == ParticipantId::new(3) && class == "office"
        ));
        assert!(!cleanup.iter().any(|effect| matches!(
            effect,
            DriverEffect::Handset(HandsetEffect::BeginMedia {
                call_id: CallId(4),
                ..
            })
        )));
        let session = controller.conference_session(CallId(4)).unwrap();
        assert!(session.pending_invite.is_none());
        assert_eq!(session.participants.iter().len(), 3);
        assert_eq!(
            controller.call(CallId(2)).unwrap().state,
            CallState::Connected
        );
        assert!(controller.call(CallId(5)).is_none());
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn failed_or_cancelled_invite_preserves_the_existing_conference() {
        for channel_created in [false, true] {
            let mut controller = active_three_party_conference();
            controller
                .begin_conference_invite(
                    CallId(4),
                    CallId(5),
                    binding(),
                    Codec::Pcma,
                    Instant::now(),
                )
                .unwrap();
            let invite_pbx = controller
                .conference_session(CallId(5))
                .unwrap()
                .pending_invite
                .as_ref()
                .unwrap()
                .participant
                .pbx_call_id;
            let cleanup =
                controller.abort_conference_invite(CallId(5), channel_created, true, true);
            assert_eq!(
                cleanup
                    .iter()
                    .filter(|effect| matches!(
                        effect,
                        DriverEffect::Backend(PbxEffect::Hangup { .. })
                    ))
                    .count(),
                usize::from(channel_created)
            );
            assert!(!cleanup.iter().any(|effect| matches!(
                effect,
                DriverEffect::Backend(PbxEffect::Bridge {
                    operation: crate::runtime::backend::BridgeOperation::Destroy { .. }
                })
            )));
            assert!(controller.pbx_call(invite_pbx).is_none());
            let session = controller.conference_session(CallId(4)).unwrap();
            assert_eq!(session.phase, ConferencePhase::Active);
            assert_eq!(session.participants.iter().len(), 3);
            assert!(session.pending_invite.is_none());
            assert_eq!(
                controller.call(CallId(4)).unwrap().state,
                CallState::Connected
            );
            assert!(controller.invariant_error().is_none());
        }
    }

    #[test]
    fn moderator_mute_commits_only_after_backend_success_and_updates_json() {
        let mut controller = active_three_party_conference();
        let moderator_device = binding().device_id;
        let conference_id = controller.conference_session(CallId(4)).unwrap().id;
        let participant_id = ParticipantId::new(2);

        let effects = controller
            .begin_conference_participant_mute(
                &moderator_device,
                conference_id,
                participant_id,
                true,
            )
            .unwrap();
        assert_eq!(
            effects,
            [DriverEffect::Backend(PbxEffect::Bridge {
                operation: crate::runtime::backend::BridgeOperation::SetParticipantMuted {
                    bridge_id: PbxBridgeId(1),
                    participant_id,
                    call_id: PbxCallId(8),
                    muted: true,
                },
            })]
        );
        let before: serde_json::Value =
            serde_json::from_str(&controller.conference_json(CallId(4)).unwrap()).unwrap();
        assert_eq!(before["participants"][1]["muted"], false);
        assert_eq!(
            controller.begin_conference_participant_mute(
                &moderator_device,
                conference_id,
                participant_id,
                true,
            ),
            Err(ConferenceParticipantRejection::Conflict)
        );

        assert!(controller.abort_conference_participant_mute(conference_id, participant_id, true));
        assert!(
            !controller
                .conference_session(CallId(4))
                .unwrap()
                .participants
                .get(participant_id)
                .unwrap()
                .muted
        );

        controller
            .begin_conference_participant_mute(
                &moderator_device,
                conference_id,
                participant_id,
                true,
            )
            .unwrap();
        assert!(controller.conference_participant_muted(conference_id, participant_id, true));
        let muted: serde_json::Value =
            serde_json::from_str(&controller.conference_json(CallId(4)).unwrap()).unwrap();
        assert_eq!(muted["participants"][1]["muted"], true);

        controller
            .begin_conference_participant_mute(
                &moderator_device,
                conference_id,
                participant_id,
                false,
            )
            .unwrap();
        assert!(controller.conference_participant_muted(conference_id, participant_id, false));
        assert!(
            !controller
                .conference_session(CallId(4))
                .unwrap()
                .participants
                .get(participant_id)
                .unwrap()
                .muted
        );
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn participant_mute_authorization_identity_and_lifecycle_are_deterministic() {
        let mut controller = active_three_party_conference();
        let moderator_device = binding().device_id;
        let other_device = DeviceId::new("SEP112233445566").unwrap();
        let conference_id = controller.conference_session(CallId(4)).unwrap().id;

        assert_eq!(
            controller.begin_conference_participant_mute(
                &other_device,
                conference_id,
                ParticipantId::new(2),
                true,
            ),
            Err(ConferenceParticipantRejection::NotModerator)
        );
        assert_eq!(
            controller.begin_conference_participant_mute(
                &moderator_device,
                conference_id,
                ParticipantId::new(1),
                true,
            ),
            Err(ConferenceParticipantRejection::Moderator)
        );
        assert_eq!(
            controller.begin_conference_participant_mute(
                &moderator_device,
                conference_id,
                ParticipantId::new(99),
                true,
            ),
            Err(ConferenceParticipantRejection::InvalidParticipant)
        );

        controller
            .begin_conference_participant_mute(
                &moderator_device,
                conference_id,
                ParticipantId::new(2),
                true,
            )
            .unwrap();
        let cleanup = controller.end_conference(CallId(4));
        assert!(!cleanup.is_empty());
        assert!(!controller.conference_participant_muted(
            conference_id,
            ParticipantId::new(2),
            true,
        ));
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn moderator_removal_commits_only_after_backend_success_and_rekeys_indexes() {
        let mut controller = active_three_party_conference();
        let moderator_device = binding().device_id;
        let conference_id = controller.conference_session(CallId(4)).unwrap().id;
        let participant_id = ParticipantId::new(2);

        let effects = controller
            .begin_conference_participant_removal(&moderator_device, conference_id, participant_id)
            .unwrap();
        assert_eq!(
            effects,
            [DriverEffect::Backend(PbxEffect::Bridge {
                operation: crate::runtime::backend::BridgeOperation::RemoveConferenceParticipant {
                    bridge_id: PbxBridgeId(1),
                    participant_id,
                    call_id: PbxCallId(8),
                },
            })]
        );
        assert_eq!(
            controller
                .conference_session(CallId(4))
                .unwrap()
                .participants
                .iter()
                .len(),
            3
        );
        let before_abort = controller.conference_json(CallId(4)).unwrap();
        assert_eq!(
            controller.begin_conference_participant_mute(
                &moderator_device,
                conference_id,
                ParticipantId::new(3),
                true,
            ),
            Err(ConferenceParticipantRejection::Conflict)
        );
        assert!(controller.abort_conference_participant_removal(conference_id, participant_id));
        assert_eq!(controller.conference_json(CallId(4)).unwrap(), before_abort);

        controller
            .begin_conference_participant_removal(&moderator_device, conference_id, participant_id)
            .unwrap();
        let cleanup = controller
            .conference_participant_removed(conference_id, participant_id)
            .unwrap();
        assert_eq!(
            cleanup,
            [DriverEffect::Handset(HandsetEffect::SetCallState {
                device_id: moderator_device.clone(),
                call_id: CallId(2),
                state: HandsetCallState::OnHook,
                stop_media: true,
            })]
        );
        let session = controller.conference_session(CallId(4)).unwrap();
        assert_eq!(session.consultation_call_id, PbxCallId(9));
        assert_eq!(session.consultation_handset_call_id, CallId(3));
        assert_eq!(
            session
                .participants
                .iter()
                .map(|participant| participant.id)
                .collect::<Vec<_>>(),
            [ParticipantId::new(1), ParticipantId::new(3)]
        );
        assert!(controller.call(CallId(2)).is_none());
        let json: serde_json::Value =
            serde_json::from_str(&controller.conference_json(CallId(4)).unwrap()).unwrap();
        assert_eq!(json["participants"].as_array().unwrap().len(), 2);
        assert_eq!(json["participants"][1]["id"], 3);
        assert_eq!(
            controller.begin_conference_participant_removal(
                &moderator_device,
                conference_id,
                ParticipantId::new(3),
            ),
            Err(ConferenceParticipantRejection::Conflict)
        );
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn participant_removal_authorization_failure_and_hangup_race_are_exact() {
        let mut controller = active_three_party_conference();
        let moderator_device = binding().device_id;
        let other_device = DeviceId::new("SEP112233445566").unwrap();
        let conference_id = controller.conference_session(CallId(4)).unwrap().id;

        assert_eq!(
            controller.begin_conference_participant_removal(
                &other_device,
                conference_id,
                ParticipantId::new(2),
            ),
            Err(ConferenceParticipantRejection::NotModerator)
        );
        assert_eq!(
            controller.begin_conference_participant_removal(
                &moderator_device,
                conference_id,
                ParticipantId::new(1),
            ),
            Err(ConferenceParticipantRejection::Moderator)
        );
        assert_eq!(
            controller.begin_conference_participant_removal(
                &moderator_device,
                conference_id,
                ParticipantId::new(99),
            ),
            Err(ConferenceParticipantRejection::InvalidParticipant)
        );

        controller
            .begin_conference_participant_removal(
                &moderator_device,
                conference_id,
                ParticipantId::new(2),
            )
            .unwrap();
        let outcome = controller
            .pbx_hangup_with_effects(PbxCallId(8))
            .expect("pending participant hangup is consumed");
        assert_eq!(
            outcome.effects,
            [DriverEffect::Handset(HandsetEffect::SetCallState {
                device_id: moderator_device,
                call_id: CallId(2),
                state: HandsetCallState::OnHook,
                stop_media: true,
            })]
        );
        assert!(controller.conference_session(CallId(4)).is_some());
        assert!(
            controller
                .conference_participant_removed(conference_id, ParticipantId::new(2))
                .is_none()
        );
        assert!(!outcome.effects.iter().any(|effect| matches!(
            effect,
            DriverEffect::Backend(PbxEffect::Bridge {
                operation: crate::runtime::backend::BridgeOperation::Destroy { .. }
            })
        )));
        assert!(
            !outcome.effects.iter().any(|effect| matches!(
                effect,
                DriverEffect::Backend(PbxEffect::Bridge {
                    operation:
                        crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold { .. }
                })
            ))
        );
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn moderator_role_changes_commit_transactionally_and_preserve_stable_identity() {
        let mut controller = active_three_party_conference();
        let device = binding().device_id;
        let conference_id = controller.conference_session(CallId(4)).unwrap().id;
        let participant_id = ParticipantId::new(2);
        let original_moderator_id = ParticipantId::new(1);
        let stable_participants = controller
            .conference_session(CallId(4))
            .unwrap()
            .participants
            .iter()
            .map(|participant| {
                (
                    participant.id,
                    participant.pbx_call_id,
                    participant.handset_call_id,
                )
            })
            .collect::<Vec<_>>();
        let before = controller.conference_json(CallId(4)).unwrap();

        assert!(
            controller
                .begin_conference_participant_role_change(
                    &device,
                    conference_id,
                    participant_id,
                    true,
                )
                .unwrap()
                .is_empty()
        );
        assert_eq!(controller.conference_json(CallId(4)).unwrap(), before);
        assert_eq!(
            controller.begin_conference_participant_mute(
                &device,
                conference_id,
                ParticipantId::new(3),
                true,
            ),
            Err(ConferenceParticipantRejection::Conflict)
        );
        assert_eq!(
            controller.begin_conference_participant_removal(
                &device,
                conference_id,
                ParticipantId::new(3),
            ),
            Err(ConferenceParticipantRejection::Conflict)
        );
        assert!(controller.abort_conference_participant_role_change(
            conference_id,
            participant_id,
            true,
        ));
        assert_eq!(controller.conference_json(CallId(4)).unwrap(), before);

        controller
            .begin_conference_participant_role_change(&device, conference_id, participant_id, true)
            .unwrap();
        assert!(controller.conference_participant_role_changed(
            conference_id,
            participant_id,
            true,
        ));
        let promoted = controller.conference_session(CallId(4)).unwrap();
        assert_eq!(promoted.participants.moderator_count(), 2);
        assert!(promoted.participants.get(participant_id).unwrap().moderator);
        assert_eq!(
            promoted
                .participants
                .iter()
                .map(|participant| {
                    (
                        participant.id,
                        participant.pbx_call_id,
                        participant.handset_call_id,
                    )
                })
                .collect::<Vec<_>>(),
            stable_participants
        );
        let promoted_json: serde_json::Value =
            serde_json::from_str(&controller.conference_json(CallId(4)).unwrap()).unwrap();
        assert_eq!(promoted_json["moderator_id"], 1);
        assert_eq!(promoted_json["participants"][1]["moderator"], true);

        controller
            .begin_conference_participant_role_change(
                &device,
                conference_id,
                original_moderator_id,
                false,
            )
            .unwrap();
        let before_demote: serde_json::Value =
            serde_json::from_str(&controller.conference_json(CallId(4)).unwrap()).unwrap();
        assert_eq!(before_demote["participants"][0]["moderator"], true);
        assert!(controller.conference_participant_role_changed(
            conference_id,
            original_moderator_id,
            false,
        ));
        let demoted = controller.conference_session(CallId(4)).unwrap();
        assert_eq!(demoted.participants.moderator_count(), 1);
        assert!(
            !demoted
                .participants
                .get(original_moderator_id)
                .unwrap()
                .moderator
        );
        let demoted_json: serde_json::Value =
            serde_json::from_str(&controller.conference_json(CallId(4)).unwrap()).unwrap();
        assert_eq!(demoted_json["moderator_id"], 2);
        assert_eq!(demoted_json["participants"][0]["moderator"], false);
        assert_eq!(demoted_json["participants"][1]["moderator"], true);
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn moderator_role_changes_apply_music_at_the_listening_boundary() {
        let device = binding().device_id;

        let mut promotion = active_three_party_conference_with_media();
        let promotion_id = promotion.conference_session(CallId(4)).unwrap().id;
        promotion
            .begin_conference_moderator_leg_transition(CallId(4), true)
            .unwrap();
        assert!(promotion.conference_moderator_leg_transitioned(
            promotion_id,
            ParticipantId::new(1),
            true,
        ));
        let promote = promotion
            .begin_conference_participant_role_change(
                &device,
                promotion_id,
                ParticipantId::new(2),
                true,
            )
            .unwrap();
        assert!(matches!(
            promote.as_slice(),
            [
                DriverEffect::Backend(PbxEffect::Bridge {
                    operation:
                        crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold {
                            participant_id: first,
                            call_id: PbxCallId(8),
                            class,
                            enabled: false,
                            ..
                        },
                }),
                DriverEffect::Backend(PbxEffect::Bridge {
                    operation:
                        crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold {
                            participant_id: second,
                            call_id: PbxCallId(9),
                            enabled: false,
                            ..
                        },
                }),
            ] if *first == ParticipantId::new(2)
                && *second == ParticipantId::new(3)
                && class == "office"
        ));
        assert!(promotion.abort_conference_participant_role_change(
            promotion_id,
            ParticipantId::new(2),
            true,
        ));
        assert!(
            !promotion
                .conference_session_by_id(promotion_id)
                .unwrap()
                .participants
                .get(ParticipantId::new(2))
                .unwrap()
                .moderator
        );

        let mut demotion = active_three_party_conference_with_media();
        let demotion_id = demotion.conference_session(CallId(4)).unwrap().id;
        demotion
            .begin_conference_participant_role_change(
                &device,
                demotion_id,
                ParticipantId::new(2),
                true,
            )
            .unwrap();
        assert!(demotion.conference_participant_role_changed(
            demotion_id,
            ParticipantId::new(2),
            true,
        ));
        demotion
            .begin_conference_moderator_leg_transition(CallId(4), true)
            .unwrap();
        assert!(demotion.conference_moderator_leg_transitioned(
            demotion_id,
            ParticipantId::new(1),
            true,
        ));
        let demote = demotion
            .begin_conference_participant_role_change(
                &device,
                demotion_id,
                ParticipantId::new(2),
                false,
            )
            .unwrap();
        assert!(matches!(
            demote.as_slice(),
            [
                DriverEffect::Backend(PbxEffect::Bridge {
                    operation:
                        crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold {
                            participant_id: first,
                            call_id: PbxCallId(8),
                            class,
                            enabled: true,
                            ..
                        },
                }),
                DriverEffect::Backend(PbxEffect::Bridge {
                    operation:
                        crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold {
                            participant_id: second,
                            call_id: PbxCallId(9),
                            enabled: true,
                            ..
                        },
                }),
            ] if *first == ParticipantId::new(2)
                && *second == ParticipantId::new(3)
                && class == "office"
        ));
        assert!(demotion.abort_conference_participant_role_change(
            demotion_id,
            ParticipantId::new(2),
            false,
        ));
        assert!(
            demotion
                .conference_session_by_id(demotion_id)
                .unwrap()
                .participants
                .get(ParticipantId::new(2))
                .unwrap()
                .moderator
        );
        assert!(promotion.invariant_error().is_none());
        assert!(demotion.invariant_error().is_none());
    }

    #[test]
    fn moderator_role_changes_reject_muted_promotion_and_held_demotion() {
        let device = binding().device_id;

        let mut muted = active_three_party_conference_with_media();
        let muted_id = muted.conference_session(CallId(4)).unwrap().id;
        muted
            .begin_conference_participant_mute(&device, muted_id, ParticipantId::new(2), true)
            .unwrap();
        assert!(muted.conference_participant_muted(muted_id, ParticipantId::new(2), true,));
        assert_eq!(
            muted.begin_conference_participant_role_change(
                &device,
                muted_id,
                ParticipantId::new(2),
                true,
            ),
            Err(ConferenceParticipantRejection::Conflict)
        );

        let mut held = active_three_party_conference_with_media();
        let held_id = held.conference_session(CallId(4)).unwrap().id;
        held.begin_conference_participant_role_change(
            &device,
            held_id,
            ParticipantId::new(2),
            true,
        )
        .unwrap();
        assert!(held.conference_participant_role_changed(held_id, ParticipantId::new(2), true,));
        held.begin_conference_moderator_leg_transition(CallId(2), true)
            .unwrap();
        assert!(held.conference_moderator_leg_transitioned(held_id, ParticipantId::new(2), true,));
        assert_eq!(
            held.begin_conference_participant_role_change(
                &device,
                held_id,
                ParticipantId::new(2),
                false,
            ),
            Err(ConferenceParticipantRejection::Conflict)
        );
        assert!(muted.invariant_error().is_none());
        assert!(held.invariant_error().is_none());
    }

    #[test]
    fn moderator_role_authorization_serialization_and_lifecycle_are_deterministic() {
        let mut controller = active_three_party_conference();
        let device = binding().device_id;
        let other_device = DeviceId::new("SEP112233445566").unwrap();
        let conference_id = controller.conference_session(CallId(4)).unwrap().id;

        assert_eq!(
            controller.begin_conference_participant_role_change(
                &other_device,
                conference_id,
                ParticipantId::new(2),
                true,
            ),
            Err(ConferenceParticipantRejection::NotModerator)
        );
        assert_eq!(
            controller.begin_conference_participant_role_change(
                &device,
                conference_id,
                ParticipantId::new(99),
                true,
            ),
            Err(ConferenceParticipantRejection::InvalidParticipant)
        );
        assert_eq!(
            controller.begin_conference_participant_role_change(
                &device,
                conference_id,
                ParticipantId::new(1),
                true,
            ),
            Err(ConferenceParticipantRejection::Conflict)
        );
        assert_eq!(
            controller.begin_conference_participant_role_change(
                &device,
                conference_id,
                ParticipantId::new(1),
                false,
            ),
            Err(ConferenceParticipantRejection::LastModerator)
        );

        controller
            .begin_conference_participant_mute(&device, conference_id, ParticipantId::new(2), true)
            .unwrap();
        assert_eq!(
            controller.begin_conference_participant_role_change(
                &device,
                conference_id,
                ParticipantId::new(3),
                true,
            ),
            Err(ConferenceParticipantRejection::Conflict)
        );
        assert!(controller.abort_conference_participant_mute(
            conference_id,
            ParticipantId::new(2),
            true,
        ));
        controller
            .begin_conference_participant_removal(&device, conference_id, ParticipantId::new(2))
            .unwrap();
        assert_eq!(
            controller.begin_conference_participant_role_change(
                &device,
                conference_id,
                ParticipantId::new(3),
                true,
            ),
            Err(ConferenceParticipantRejection::Conflict)
        );
        assert!(
            controller.abort_conference_participant_removal(conference_id, ParticipantId::new(2),)
        );

        controller
            .begin_conference_participant_role_change(
                &device,
                conference_id,
                ParticipantId::new(2),
                true,
            )
            .unwrap();
        assert_eq!(
            controller.begin_conference_participant_role_change(
                &device,
                conference_id,
                ParticipantId::new(3),
                true,
            ),
            Err(ConferenceParticipantRejection::Conflict)
        );
        assert_eq!(
            controller.begin_conference_invite(
                CallId(4),
                CallId(5),
                binding(),
                Codec::Pcma,
                Instant::now(),
            ),
            Err(ConferenceRejection::Conflict)
        );
        assert!(!controller.conference_participant_role_changed(
            conference_id,
            ParticipantId::new(3),
            true,
        ));
        assert!(controller.abort_conference_participant_role_change(
            conference_id,
            ParticipantId::new(2),
            true,
        ));

        controller
            .begin_conference_invite(CallId(4), CallId(5), binding(), Codec::Pcma, Instant::now())
            .unwrap();
        assert_eq!(
            controller.begin_conference_participant_role_change(
                &device,
                conference_id,
                ParticipantId::new(2),
                true,
            ),
            Err(ConferenceParticipantRejection::Conflict)
        );
        controller.abort_conference_invite(CallId(5), false, true, false);

        controller
            .begin_conference_participant_role_change(
                &device,
                conference_id,
                ParticipantId::new(2),
                true,
            )
            .unwrap();
        let cleanup = controller
            .pbx_hangup_with_effects(PbxCallId(8))
            .expect("conference participant hangup is consumed");
        assert!(cleanup.effects.iter().any(|effect| matches!(
            effect,
            DriverEffect::Backend(PbxEffect::Bridge {
                operation: crate::runtime::backend::BridgeOperation::Destroy { .. }
            })
        )));
        assert!(controller.conference_session(CallId(4)).is_none());
        assert!(!controller.conference_participant_role_changed(
            conference_id,
            ParticipantId::new(2),
            true,
        ));
        assert_eq!(
            controller.begin_conference_participant_role_change(
                &device,
                conference_id,
                ParticipantId::new(2),
                true,
            ),
            Err(ConferenceParticipantRejection::Unavailable)
        );
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn explicit_moderator_end_removes_registry_and_restores_every_handset_exactly_once() {
        let mut controller = active_three_party_conference();
        let device = binding().device_id;
        let conference_id = controller.conference_session(CallId(4)).unwrap().id;

        let effects = controller
            .end_conference_by_moderator(&device, conference_id)
            .unwrap();
        assert_eq!(
            effects.first(),
            Some(&DriverEffect::Backend(PbxEffect::Bridge {
                operation: crate::runtime::backend::BridgeOperation::Destroy {
                    bridge_id: PbxBridgeId(1),
                },
            }))
        );
        assert_eq!(
            effects
                .iter()
                .filter_map(|effect| match effect {
                    DriverEffect::Backend(PbxEffect::Hangup { call_id }) => Some(*call_id),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            [PbxCallId(10), PbxCallId(8), PbxCallId(9)]
        );
        assert_eq!(
            effects
                .iter()
                .filter_map(|effect| match effect {
                    DriverEffect::Handset(HandsetEffect::SetCallState {
                        call_id,
                        state: HandsetCallState::OnHook,
                        stop_media: true,
                        ..
                    }) => Some(*call_id),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            [CallId(4), CallId(2), CallId(3)]
        );
        assert!(controller.conference_session_by_id(conference_id).is_none());
        assert!(controller.conference_json(CallId(4)).is_none());
        assert!(controller.calls().next().is_none());
        assert_eq!(
            controller.end_conference_by_moderator(&device, conference_id),
            Err(ConferenceEndRejection::Unavailable)
        );
        assert!(controller.pbx_hangup_with_effects(PbxCallId(10)).is_none());
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn explicit_conference_end_authorization_and_action_races_are_deterministic() {
        let mut controller = active_three_party_conference();
        let device = binding().device_id;
        let other_device = DeviceId::new("SEP112233445566").unwrap();
        let conference_id = controller.conference_session(CallId(4)).unwrap().id;

        assert_eq!(
            controller.end_conference_by_moderator(&other_device, conference_id),
            Err(ConferenceEndRejection::NotModerator)
        );
        assert_eq!(
            controller.end_conference_by_moderator(&device, ConferenceId::new(999)),
            Err(ConferenceEndRejection::Unavailable)
        );

        controller
            .begin_conference_participant_mute(&device, conference_id, ParticipantId::new(2), true)
            .unwrap();
        assert_eq!(
            controller.end_conference_by_moderator(&device, conference_id),
            Err(ConferenceEndRejection::Conflict)
        );
        assert!(controller.abort_conference_participant_mute(
            conference_id,
            ParticipantId::new(2),
            true,
        ));

        controller
            .begin_conference_participant_role_change(
                &device,
                conference_id,
                ParticipantId::new(2),
                true,
            )
            .unwrap();
        assert_eq!(
            controller.end_conference_by_moderator(&device, conference_id),
            Err(ConferenceEndRejection::Conflict)
        );
        assert!(controller.abort_conference_participant_role_change(
            conference_id,
            ParticipantId::new(2),
            true,
        ));

        controller
            .begin_conference_invite(CallId(4), CallId(5), binding(), Codec::Pcma, Instant::now())
            .unwrap();
        assert_eq!(
            controller.end_conference_by_moderator(&device, conference_id),
            Err(ConferenceEndRejection::Conflict)
        );
        controller.abort_conference_invite(CallId(5), false, true, false);

        let hangup = controller
            .pbx_hangup_with_effects(PbxCallId(8))
            .expect("PBX hangup wins the serialized cleanup race");
        assert_eq!(
            hangup
                .effects
                .iter()
                .filter(|effect| matches!(
                    effect,
                    DriverEffect::Backend(PbxEffect::Bridge {
                        operation: crate::runtime::backend::BridgeOperation::Destroy { .. }
                    })
                ))
                .count(),
            0
        );
        let end = controller
            .end_conference_by_moderator(&device, conference_id)
            .unwrap();
        assert_eq!(
            end.iter()
                .filter(|effect| matches!(
                    effect,
                    DriverEffect::Backend(PbxEffect::Bridge {
                        operation: crate::runtime::backend::BridgeOperation::Destroy { .. }
                    })
                ))
                .count(),
            1
        );
        assert_eq!(
            controller.end_conference_by_moderator(&device, conference_id),
            Err(ConferenceEndRejection::Unavailable)
        );
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn participant_departure_preserves_bridge_ids_roles_json_and_exact_leave_audience() {
        let mut controller = active_three_party_conference_with_media();
        let conference_id = controller.conference_session(CallId(4)).unwrap().id;
        let outcome = controller
            .pbx_hangup_with_effects(PbxCallId(8))
            .expect("departing participant is consumed");

        assert!(!outcome.effects.iter().any(|effect| matches!(
            effect,
            DriverEffect::Backend(PbxEffect::Bridge {
                operation: crate::runtime::backend::BridgeOperation::Destroy { .. }
            })
        )));
        assert!(outcome.effects.iter().any(|effect| matches!(
            effect,
            DriverEffect::Backend(PbxEffect::ConferenceAnnouncement {
                operation: ConferenceAnnouncementOperation {
                    conference_id: announced,
                    targets,
                    announcement: ConferenceAnnouncement::ParticipantRemoved(participant),
                },
            }) if *announced == conference_id
                && *participant == ParticipantId::new(2)
                && targets == &[
                    ConferenceAnnouncementTarget { participant_id: ParticipantId::new(1), call_id: PbxCallId(10) },
                    ConferenceAnnouncementTarget { participant_id: ParticipantId::new(3), call_id: PbxCallId(9) },
                ]
        )));
        let session = controller.conference_session_by_id(conference_id).unwrap();
        assert_eq!(session.bridge_id, PbxBridgeId(1));
        assert_eq!(session.participants.moderator_count(), 1);
        assert_eq!(
            session
                .participants
                .iter()
                .map(|participant| participant.id)
                .collect::<Vec<_>>(),
            [ParticipantId::new(1), ParticipantId::new(3)]
        );
        let json: serde_json::Value =
            serde_json::from_str(&controller.conference_json(CallId(4)).unwrap()).unwrap();
        assert_eq!(json["moderator_id"], 1);
        assert_eq!(json["participants"].as_array().unwrap().len(), 2);
        assert!(controller.pbx_hangup_with_effects(PbxCallId(8)).is_none());
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn moderator_departure_preserves_conference_only_when_another_moderator_remains() {
        let mut controller = active_three_party_conference_with_media();
        let device = binding().device_id;
        let conference_id = controller.conference_session(CallId(4)).unwrap().id;
        controller
            .begin_conference_participant_role_change(
                &device,
                conference_id,
                ParticipantId::new(2),
                true,
            )
            .unwrap();
        assert!(controller.conference_participant_role_changed(
            conference_id,
            ParticipantId::new(2),
            true,
        ));

        let outcome = controller
            .pbx_hangup_with_effects(PbxCallId(10))
            .expect("departing moderator is consumed");
        assert!(!outcome.effects.iter().any(|effect| matches!(
            effect,
            DriverEffect::Backend(PbxEffect::Bridge {
                operation: crate::runtime::backend::BridgeOperation::Destroy { .. }
            })
        )));
        assert!(outcome.effects.iter().any(|effect| matches!(
            effect,
            DriverEffect::Backend(PbxEffect::ConferenceAnnouncement {
                operation: ConferenceAnnouncementOperation {
                    targets,
                    announcement: ConferenceAnnouncement::ModeratorDeparted(participant),
                    ..
                },
            }) if *participant == ParticipantId::new(1)
                && targets == &[
                    ConferenceAnnouncementTarget { participant_id: ParticipantId::new(2), call_id: PbxCallId(8) },
                    ConferenceAnnouncementTarget { participant_id: ParticipantId::new(3), call_id: PbxCallId(9) },
                ]
        )));
        let session = controller.conference_session_by_id(conference_id).unwrap();
        assert_eq!(session.bridge_id, PbxBridgeId(1));
        assert_eq!(session.original_call_id, PbxCallId(8));
        assert_eq!(session.original_handset_call_id, CallId(2));
        assert_eq!(session.consultation_call_id, PbxCallId(9));
        assert_eq!(session.consultation_handset_call_id, CallId(3));
        assert_eq!(session.participants.moderator_count(), 1);
        assert_eq!(
            session.participants.moderator().unwrap().id,
            ParticipantId::new(2)
        );
        let json: serde_json::Value =
            serde_json::from_str(&controller.conference_json(CallId(2)).unwrap()).unwrap();
        assert_eq!(json["moderator_id"], 2);
        assert!(controller.invariant_error().is_none());

        let mut handset = active_three_party_conference_with_media();
        let handset_id = handset.conference_session(CallId(4)).unwrap().id;
        handset
            .begin_conference_participant_role_change(
                &device,
                handset_id,
                ParticipantId::new(2),
                true,
            )
            .unwrap();
        assert!(handset.conference_participant_role_changed(
            handset_id,
            ParticipantId::new(2),
            true,
        ));
        let effects = handset.hangup(CallId(4));
        assert!(effects.iter().any(|effect| matches!(
            effect,
            DriverEffect::Backend(PbxEffect::Hangup {
                call_id: PbxCallId(10)
            })
        )));
        assert!(handset.conference_session_by_id(handset_id).is_some());
        assert!(handset.invariant_error().is_none());

        let mut secondary = active_three_party_conference_with_media();
        let secondary_id = secondary.conference_session(CallId(4)).unwrap().id;
        secondary
            .begin_conference_participant_role_change(
                &device,
                secondary_id,
                ParticipantId::new(2),
                true,
            )
            .unwrap();
        assert!(secondary.conference_participant_role_changed(
            secondary_id,
            ParticipantId::new(2),
            true,
        ));
        secondary.pbx_hangup_with_effects(PbxCallId(8));
        let session = secondary.conference_session_by_id(secondary_id).unwrap();
        assert_eq!(session.bridge_id, PbxBridgeId(1));
        assert_eq!(session.participants.moderator_count(), 1);
        assert_eq!(
            session.participants.moderator().unwrap().id,
            ParticipantId::new(1)
        );
        assert_eq!(
            session
                .participants
                .iter()
                .map(|participant| participant.id)
                .collect::<Vec<_>>(),
            [ParticipantId::new(1), ParticipantId::new(3)]
        );
        assert!(secondary.invariant_error().is_none());
    }

    #[test]
    fn last_moderator_departure_announces_before_terminal_cleanup_and_is_idempotent() {
        let mut controller = active_three_party_conference_with_media();
        let conference_id = controller.conference_session(CallId(4)).unwrap().id;
        let outcome = controller
            .pbx_hangup_with_effects(PbxCallId(10))
            .expect("last moderator departure is consumed");
        assert!(matches!(
            outcome.effects.as_slice(),
            [
                DriverEffect::Backend(PbxEffect::ConferenceAnnouncement {
                    operation: ConferenceAnnouncementOperation {
                        conference_id: announced,
                        targets,
                        announcement: ConferenceAnnouncement::ModeratorDeparted(participant),
                    },
                }),
                DriverEffect::Backend(PbxEffect::Bridge {
                    operation: crate::runtime::backend::BridgeOperation::Destroy { .. }
                }),
                ..
            ] if *announced == conference_id
                && *participant == ParticipantId::new(1)
                && targets == &[
                    ConferenceAnnouncementTarget { participant_id: ParticipantId::new(2), call_id: PbxCallId(8) },
                    ConferenceAnnouncementTarget { participant_id: ParticipantId::new(3), call_id: PbxCallId(9) },
                ]
        ));
        assert!(!outcome.effects.iter().any(|effect| matches!(
            effect,
            DriverEffect::Backend(PbxEffect::Hangup {
                call_id: PbxCallId(10)
            })
        )));
        assert!(
            !outcome.effects.iter().any(|effect| matches!(
                effect,
                DriverEffect::Backend(PbxEffect::Bridge {
                    operation:
                        crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold { .. }
                })
            ))
        );
        assert!(controller.conference_session_by_id(conference_id).is_none());
        assert!(controller.pbx_hangup_with_effects(PbxCallId(10)).is_none());
        assert!(controller.invariant_error().is_none());

        let mut announcements_disabled = active_three_party_conference();
        let disabled = announcements_disabled
            .pbx_hangup_with_effects(PbxCallId(10))
            .unwrap();
        assert!(!disabled.effects.iter().any(|effect| matches!(
            effect,
            DriverEffect::Backend(PbxEffect::ConferenceAnnouncement { .. })
        )));
        assert!(
            !disabled.effects.iter().any(|effect| matches!(
                effect,
                DriverEffect::Backend(PbxEffect::Bridge {
                    operation:
                        crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold { .. }
                })
            ))
        );
        assert!(matches!(
            disabled.effects.first(),
            Some(DriverEffect::Backend(PbxEffect::Bridge {
                operation: crate::runtime::backend::BridgeOperation::Destroy { .. }
            }))
        ));
        assert!(announcements_disabled.invariant_error().is_none());
    }

    #[test]
    fn moderator_promotion_and_departure_have_one_serialized_winner() {
        let device = binding().device_id;

        let mut departure_first = active_three_party_conference();
        let departure_id = departure_first.conference_session(CallId(4)).unwrap().id;
        departure_first.pbx_hangup_with_effects(PbxCallId(10));
        assert_eq!(
            departure_first.begin_conference_participant_role_change(
                &device,
                departure_id,
                ParticipantId::new(2),
                true,
            ),
            Err(ConferenceParticipantRejection::Unavailable)
        );

        let mut promotion_pending = active_three_party_conference();
        let pending_id = promotion_pending.conference_session(CallId(4)).unwrap().id;
        promotion_pending
            .begin_conference_participant_role_change(
                &device,
                pending_id,
                ParticipantId::new(2),
                true,
            )
            .unwrap();
        let cleanup = promotion_pending
            .pbx_hangup_with_effects(PbxCallId(10))
            .unwrap();
        assert!(cleanup.effects.iter().any(|effect| matches!(
            effect,
            DriverEffect::Backend(PbxEffect::Bridge {
                operation: crate::runtime::backend::BridgeOperation::Destroy { .. }
            })
        )));
        assert!(!promotion_pending.conference_participant_role_changed(
            pending_id,
            ParticipantId::new(2),
            true,
        ));

        let mut promotion_first = active_three_party_conference();
        let promoted_id = promotion_first.conference_session(CallId(4)).unwrap().id;
        promotion_first
            .begin_conference_participant_role_change(
                &device,
                promoted_id,
                ParticipantId::new(2),
                true,
            )
            .unwrap();
        assert!(promotion_first.conference_participant_role_changed(
            promoted_id,
            ParticipantId::new(2),
            true,
        ));
        promotion_first.pbx_hangup_with_effects(PbxCallId(10));
        assert!(
            promotion_first
                .conference_session_by_id(promoted_id)
                .is_some()
        );
        assert!(departure_first.invariant_error().is_none());
        assert!(promotion_pending.invariant_error().is_none());
        assert!(promotion_first.invariant_error().is_none());
    }

    #[test]
    fn conference_owner_disconnect_fails_closed_without_leaking_bridge_or_channels() {
        let mut controller = active_three_party_conference_with_media();
        let device = binding().device_id;
        let effects = controller.disconnected(&device);
        assert_eq!(
            effects
                .iter()
                .filter(|effect| matches!(
                    effect,
                    DriverEffect::Backend(PbxEffect::Bridge {
                        operation: crate::runtime::backend::BridgeOperation::Destroy { .. }
                    })
                ))
                .count(),
            1
        );
        assert_eq!(
            effects
                .iter()
                .filter(|effect| matches!(effect, DriverEffect::Backend(PbxEffect::Hangup { .. })))
                .count(),
            3
        );
        assert!(!effects.iter().any(|effect| matches!(
            effect,
            DriverEffect::Backend(PbxEffect::ConferenceAnnouncement { .. })
        )));
        assert!(controller.calls().next().is_none());
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn removing_a_secondary_participant_after_promotion_keeps_stable_indexes() {
        let mut controller = active_three_party_conference();
        let device = binding().device_id;
        let conference_id = controller.conference_session(CallId(4)).unwrap().id;

        controller
            .begin_conference_participant_role_change(
                &device,
                conference_id,
                ParticipantId::new(3),
                true,
            )
            .unwrap();
        assert!(controller.conference_participant_role_changed(
            conference_id,
            ParticipantId::new(3),
            true,
        ));
        controller
            .begin_conference_participant_removal(&device, conference_id, ParticipantId::new(2))
            .unwrap();
        controller
            .conference_participant_removed(conference_id, ParticipantId::new(2))
            .unwrap();

        let session = controller.conference_session(CallId(4)).unwrap();
        assert_eq!(session.consultation_call_id, PbxCallId(9));
        assert_eq!(session.consultation_handset_call_id, CallId(3));
        assert_eq!(session.participants.moderator_count(), 2);
        assert_eq!(
            session
                .participants
                .iter()
                .map(|participant| participant.id)
                .collect::<Vec<_>>(),
            [ParticipantId::new(1), ParticipantId::new(3)]
        );
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn consultation_and_active_pbx_hangups_have_exact_conference_cleanup() {
        let mut pending = connected_outbound_controller();
        pending
            .begin_conference(
                CallId(1),
                CallId(2),
                binding(),
                Codec::Pcmu,
                Instant::now(),
                true,
            )
            .unwrap();
        let pending_cleanup = pending
            .pbx_hangup_with_effects(PbxCallId(2))
            .unwrap()
            .effects;
        assert!(pending_cleanup.iter().any(|effect| matches!(
            effect,
            DriverEffect::Backend(PbxEffect::Resume {
                call_id: PbxCallId(1)
            })
        )));
        assert!(pending.call(CallId(1)).is_some());
        assert!(pending.call(CallId(2)).is_none());

        let mut active = connected_outbound_controller();
        active
            .begin_conference(
                CallId(1),
                CallId(2),
                binding(),
                Codec::Pcmu,
                Instant::now(),
                true,
            )
            .unwrap();
        active.enbloc(CallId(2), "2200".into());
        active.pbx_answer(PbxCallId(2));
        active.confirm_conference(CallId(2)).unwrap();
        assert!(active.conference_merged(CallId(2)));
        let active_cleanup = active
            .pbx_hangup_with_effects(PbxCallId(1))
            .unwrap()
            .effects;
        assert_eq!(
            active_cleanup
                .iter()
                .filter(|effect| matches!(
                    effect,
                    DriverEffect::Backend(PbxEffect::Bridge {
                        operation: crate::runtime::backend::BridgeOperation::Destroy { .. }
                    })
                ))
                .count(),
            1
        );
        assert!(active_cleanup.iter().any(|effect| matches!(
            effect,
            DriverEffect::Backend(PbxEffect::Hangup {
                call_id: PbxCallId(2)
            })
        )));
        assert!(!active_cleanup.iter().any(|effect| matches!(
            effect,
            DriverEffect::Backend(PbxEffect::Hangup {
                call_id: PbxCallId(1)
            })
        )));
        assert!(active.calls().next().is_none());
        assert!(active.invariant_error().is_none());
    }

    #[test]
    fn failed_participant_preserves_survivors_or_fails_closed_during_a_mutation() {
        let mut preserving = active_three_party_conference_with_media();
        let conference_id = preserving.conference_session(CallId(4)).unwrap().id;
        let outcome = preserving
            .conference_participant_failed(CallId(2))
            .expect("active participant failure is claimed");
        assert_eq!(outcome.conference_id, conference_id);
        assert_eq!(outcome.failed_call_id, PbxCallId(8));
        assert_eq!(outcome.call_ids, [PbxCallId(8)]);
        let survivor = outcome
            .surviving_session
            .expect("two eligible participants preserve the conference");
        assert_eq!(survivor.id, conference_id);
        assert_eq!(survivor.bridge_id, PbxBridgeId(1));
        assert_eq!(
            survivor
                .participants
                .iter()
                .map(|participant| participant.id)
                .collect::<Vec<_>>(),
            [ParticipantId::new(1), ParticipantId::new(3)]
        );
        assert!(!outcome.effects.iter().any(|effect| matches!(
            effect,
            DriverEffect::Backend(PbxEffect::Bridge {
                operation: crate::runtime::backend::BridgeOperation::Destroy { .. }
            })
        )));
        assert_eq!(
            outcome
                .effects
                .iter()
                .filter(|effect| matches!(
                    effect,
                    DriverEffect::Backend(PbxEffect::Hangup {
                        call_id: PbxCallId(8)
                    })
                ))
                .count(),
            1
        );
        assert!(
            preserving
                .conference_participant_failed(CallId(2))
                .is_none()
        );

        let mut pending = active_three_party_conference();
        let device = binding().device_id;
        let pending_id = pending.conference_session(CallId(4)).unwrap().id;
        pending
            .begin_conference_participant_mute(&device, pending_id, ParticipantId::new(3), true)
            .unwrap();
        let terminal = pending
            .conference_participant_failed(CallId(2))
            .expect("failure wins the pending mutation race");
        assert!(terminal.surviving_session.is_none());
        assert_eq!(
            terminal.call_ids,
            [PbxCallId(10), PbxCallId(8), PbxCallId(9)]
        );
        assert_eq!(
            terminal
                .effects
                .iter()
                .filter(|effect| matches!(
                    effect,
                    DriverEffect::Backend(PbxEffect::Bridge {
                        operation: crate::runtime::backend::BridgeOperation::Destroy { .. }
                    })
                ))
                .count(),
            1
        );
        assert_eq!(
            terminal
                .effects
                .iter()
                .filter(|effect| matches!(effect, DriverEffect::Backend(PbxEffect::Hangup { .. })))
                .count(),
            3
        );
        assert!(!pending.conference_participant_muted(pending_id, ParticipantId::new(3), true,));
        assert!(preserving.invariant_error().is_none());
        assert!(pending.invariant_error().is_none());
    }

    #[test]
    fn shutdown_drain_owns_pending_participants_and_is_idempotent() {
        let mut controller = active_three_party_conference();
        let conference_id = controller.conference_session(CallId(4)).unwrap().id;
        controller
            .begin_conference_invite(CallId(4), CallId(5), binding(), Codec::Pcma, Instant::now())
            .unwrap();

        let plans = controller.drain_conferences_for_shutdown();
        assert_eq!(plans.len(), 1);
        let plan = &plans[0];
        assert_eq!(plan.conference_id, conference_id);
        assert_eq!(
            plan.call_ids,
            [PbxCallId(10), PbxCallId(8), PbxCallId(9), PbxCallId(11),]
        );
        assert_eq!(
            plan.effects
                .iter()
                .filter(|effect| matches!(
                    effect,
                    DriverEffect::Backend(PbxEffect::Bridge {
                        operation: crate::runtime::backend::BridgeOperation::Destroy { .. }
                    })
                ))
                .count(),
            1
        );
        assert_eq!(
            plan.effects
                .iter()
                .filter(|effect| matches!(effect, DriverEffect::Backend(PbxEffect::Hangup { .. })))
                .count(),
            4
        );
        assert_eq!(
            plan.effects
                .iter()
                .filter(|effect| matches!(
                    effect,
                    DriverEffect::Handset(HandsetEffect::SetCallState {
                        state: HandsetCallState::OnHook,
                        stop_media: true,
                        ..
                    })
                ))
                .count(),
            4
        );
        assert!(controller.drain_conferences_for_shutdown().is_empty());
        assert!(controller.conference_session_by_id(conference_id).is_none());
        assert!(controller.calls().next().is_none());
        assert!(controller.invariant_error().is_none());

        let mut mutating = active_three_party_conference();
        let device = binding().device_id;
        let mutating_id = mutating.conference_session(CallId(4)).unwrap().id;
        mutating
            .begin_conference_participant_mute(&device, mutating_id, ParticipantId::new(2), true)
            .unwrap();
        let mutation_plan = mutating.drain_conferences_for_shutdown();
        assert_eq!(mutation_plan.len(), 1);
        assert!(!mutating.conference_participant_muted(mutating_id, ParticipantId::new(2), true,));
        assert!(mutating.drain_conferences_for_shutdown().is_empty());
        assert!(mutating.invariant_error().is_none());
    }

    #[test]
    fn shutdown_drain_orders_multiple_conferences_and_destroys_each_once() {
        let mut controller = Controller::new(Duration::from_secs(1));
        for (device, first_call, first_pbx) in [
            ("SEP001122334455", 2_u64, 8_u64),
            ("SEP112233445566", 12_u64, 18_u64),
        ] {
            let device_id = DeviceId::new(device).unwrap();
            controller.registered(registration_for(device));
            for offset in 0..2 {
                let call_id = CallId(first_call + offset);
                controller.begin_asterisk_call(
                    call_id,
                    (first_pbx + offset).into(),
                    &binding_for(device, 1),
                    Codec::Pcma,
                );
                controller.phone_answer(call_id);
                if offset == 0 {
                    controller.hold(call_id);
                }
            }
            let moderator_call_id = CallId(first_call + 1);
            controller
                .join_calls(&device_id, moderator_call_id, true)
                .unwrap();
            assert!(controller.conference_merged(moderator_call_id));
        }

        let plans = controller.drain_conferences_for_shutdown();
        assert_eq!(
            plans
                .iter()
                .map(|plan| plan.conference_id)
                .collect::<Vec<_>>(),
            [ConferenceId::new(1), ConferenceId::new(2)]
        );
        assert!(plans.iter().all(|plan| {
            plan.effects
                .iter()
                .filter(|effect| {
                    matches!(
                        effect,
                        DriverEffect::Backend(PbxEffect::Bridge {
                            operation: crate::runtime::backend::BridgeOperation::Destroy { .. }
                        })
                    )
                })
                .count()
                == 1
        }));
        assert_eq!(
            plans.iter().map(|plan| plan.call_ids.len()).sum::<usize>(),
            4
        );
        assert!(controller.drain_conferences_for_shutdown().is_empty());
        assert!(controller.calls().next().is_none());
        assert!(controller.invariant_error().is_none());
    }

    #[test]
    fn shared_line_operation_sequences_preserve_ownership_invariants() {
        #[derive(Clone, Copy, Debug)]
        enum Operation {
            Answer(CallId),
            Hold(CallId),
            Resume(CallId),
            Steal(CallId),
            DisconnectFirst,
            DisconnectSecond,
            PbxHangup,
        }

        fn apply(controller: &mut Controller, operation: Operation) {
            match operation {
                Operation::Answer(call_id) => {
                    controller.phone_answer(call_id);
                }
                Operation::Hold(call_id) => {
                    controller.hold(call_id);
                }
                Operation::Resume(call_id) => {
                    controller.resume(call_id);
                }
                Operation::Steal(call_id) => {
                    controller.steal(call_id);
                }
                Operation::DisconnectFirst => {
                    controller.disconnected(&DeviceId::new("SEP001122334455").unwrap());
                }
                Operation::DisconnectSecond => {
                    controller.disconnected(&DeviceId::new("SEP112233445566").unwrap());
                }
                Operation::PbxHangup => {
                    controller.pbx_hangup_with_effects(PbxCallId(8));
                }
            }
        }

        fn assert_shared_invariants(controller: &Controller, sequence: &[Operation]) {
            assert_eq!(
                controller.invariant_error(),
                None,
                "invariant failed after {sequence:?}"
            );
            let Some(call) = controller.pbx_call(PbxCallId(8)) else {
                assert_eq!(controller.calls().count(), 0, "after {sequence:?}");
                return;
            };
            let appearances: Vec<_> = controller.appearances_for_pbx(call.id).collect();
            assert!(!appearances.is_empty(), "after {sequence:?}");
            assert_eq!(
                appearances.len(),
                call.appearance_ids().count(),
                "after {sequence:?}"
            );
            assert!(
                appearances
                    .iter()
                    .all(|appearance| appearance.pbx_id == call.id),
                "after {sequence:?}"
            );
            let active: Vec<_> = appearances
                .iter()
                .filter(|appearance| {
                    matches!(
                        appearance.state,
                        CallState::Collecting
                            | CallState::PickupCollecting
                            | CallState::Calling
                            | CallState::Connected
                            | CallState::Held
                            | CallState::TransferCollecting
                    )
                })
                .collect();
            assert!(active.len() <= 1, "after {sequence:?}");
            assert_eq!(
                active.first().map(|appearance| appearance.id),
                call.active_appearance(),
                "after {sequence:?}"
            );
        }

        let operations = [
            Operation::Answer(CallId(2)),
            Operation::Answer(CallId(3)),
            Operation::Hold(CallId(2)),
            Operation::Hold(CallId(3)),
            Operation::Resume(CallId(2)),
            Operation::Resume(CallId(3)),
            Operation::Steal(CallId(2)),
            Operation::Steal(CallId(3)),
            Operation::DisconnectFirst,
            Operation::DisconnectSecond,
            Operation::PbxHangup,
        ];

        for first in operations {
            for second in operations {
                for third in operations {
                    for fourth in operations {
                        let sequence = [first, second, third, fourth];
                        let mut controller = shared_inbound_controller();
                        assert_shared_invariants(&controller, &[]);
                        for (index, operation) in sequence.into_iter().enumerate() {
                            apply(&mut controller, operation);
                            assert_shared_invariants(&controller, &sequence[..=index]);
                        }
                    }
                }
            }
        }
    }
}
