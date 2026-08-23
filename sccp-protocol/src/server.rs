//! Stateful SCCP station-session boundary.
//!
//! A successful socket write or TCP acknowledgement proves only transport
//! delivery. Media and other correlated operations remain provisional until
//! the station sends the matching SCCP response. Every new media transaction
//! gets a fresh nonzero wire token (a deliberately coupled ORC/SMT pair shares
//! one), and the session accepts an acknowledgement only for the exact
//! direction and current request generation. The zero-party acknowledgement
//! fallback is limited to the first generation, where the
//! stable call reference still makes it unambiguous; reopened media fails
//! closed instead of letting an old acknowledgement settle a new request.
//! Deadlines retire that same generation before a late response is considered.
//! Handset presentation, receive media, transmit media, and call ownership are
//! therefore separate states rather than one broad "connected" flag.
//!
//! One explicit wire exception writes OpenReceiveChannel and
//! StartMediaTransmission together for coupled outbound NAT media. A matching
//! successful receive acknowledgement atomically settles both halves and
//! emits [`DeviceEventKind::TransmitChannelImplied`]; ordinary staged media never infers
//! transmit success from a receive acknowledgement.  Close, failure, timeout,
//! disconnect, and replacement paths retire the coupled transaction.
//!
//! # Runtime workflow
//!
//! [`Server::bind`] creates a plain TCP listener, while
//! [`Server::with_ingress`] lets a transport owner inject clear or already
//! decrypted streams through [`ServerIngress`]. Both constructors return a
//! [`ServerHandle`] for commands and a bounded [`Event`] receiver for handset
//! input and session outcomes. The caller must run [`Server::run`] for any of
//! those channels to make progress and should consume events continuously so a
//! full event queue cannot apply backpressure to station sessions.
//!
//! A station becomes addressable by [`Command`] only after registration has
//! selected a configured [`DeviceDefinition`] and emitted
//! [`DeviceEventKind::Registered`]. Call adapters reserve or receive a
//! [`CallId`], send commands through the handle, and react to correlated media
//! acknowledgements delivered as device events. [`ServerHandle::send`] confirms
//! queue admission; [`ServerHandle::send_confirmed`] additionally waits for the
//! complete encoded command to reach the station stream. Neither operation
//! substitutes for a protocol acknowledgement when the command defines one.
//!
//! Reconfiguration replaces definitions atomically and disconnects only the
//! sessions named by [`ReconfigureResult`] or explicitly marked as affected.
//! [`ServerHandle::shutdown`] asks the run loop to disconnect all sessions and
//! finish. Dropping every handle also closes the command channel and causes the
//! same orderly exit.

mod qos;
mod transport;

pub use qos::{
    SignalingSocket, SocketQosFailure, SocketQosMark, SocketQosPolicy, SocketQosReport,
    StationSocketQos, apply_socket_qos,
};
pub use transport::{ServerIngress, StationIo};

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::num::NonZeroU16;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
#[cfg(test)]
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::message::capabilities::StationMediaCapabilities;
use crate::message::values::{
    AlarmSeverity, BusyLampFieldState, ButtonType, CallHistoryDisposition, CallState, Codec,
    CodecKind, DeviceType, Digit, DtmfMode, EchoCancellation, EncryptionCapability, G723BitRate,
    IpAddressType, KeyMode, LampMode, MediaStatus, MicrophoneMode, MiscCommandType,
    NotificationPriority, PhoneFeatures, ProtocolVersion, ReceiveTransmit, ResetType, RingDuration,
    RingerMode, SilenceSuppression, SoftKey, SpeakerMode, StationSessionContext,
    StatisticsProcessing, Stimulus, SubscriptionCause, Tone, ToneDirection,
};
use crate::message::wire::{CodecError, FrameDecoder};
use crate::message::{
    AnnouncementEntry, AudioStreamControl, BoundedBytes, ButtonTemplateEntry, ClientMessage,
    ConnectionStatistics, MediaEncryption, MediaEndpointAddress, MediaRequestIdentity,
    MediaRequestToken, MiscellaneousCommand, MulticastMediaReception, MulticastMediaTransmission,
    MultimediaPayload, MultimediaPayloadDirection, MultimediaStreamControl, OpenMultimediaChannel,
    ServerMessage, SignalingServerEndpoint,
    StartMultimediaTransmission as MultimediaTransmissionStart, UserDataV1Message,
    VideoFlowControl,
};
#[cfg(test)]
use crate::message::{ControlMessage, MediaCapability, XmlAlarmMessage};
use crate::phone::service::{
    PhoneServiceEvent, PhoneServiceExtendedRouting, PhoneServiceMessageKind, PhoneServicePayload,
    PhoneServiceRouting, parse_phone_service_payload,
};
#[cfg(test)]
use crate::phone::xml::{
    self as phone_xml, CiscoIpPhoneGraphicFileMenu, CiscoIpPhoneImageFile, CiscoIpPhoneInputItem,
    CiscoIpPhoneKeyItem, CiscoIpPhoneSoftKeyItem, CiscoIpPhoneStatus, CiscoIpPhoneStatusFile,
    CiscoIpPhoneTouchAreaMenuItem, PHONE_EXECUTE_MAX_ITEMS, PHONE_STATUS_BITMAP_MAX_BYTES,
    PhoneBackgroundHttpUrl, PhoneBitmapData, PhoneExecutePriority, PhoneImageUrl, PhoneInputFlags,
    PhoneInputParameterName, PhoneRingtoneUrl, PhoneTouchArea, PhoneXmlKey,
};
use crate::phone::xml::{
    CiscoIpPhoneExecute, CiscoIpPhoneExecuteItem, CiscoIpPhoneInput, CiscoIpPhoneMenu,
    CiscoIpPhoneMenuItem, CiscoIpPhoneSetBackground, CiscoIpPhoneSetBackgroundPreview,
    CiscoIpPhoneSetRingTone, CiscoIpPhoneText, ConferenceListAction, ConferenceListDocument,
    ConferenceListEntry, ConferenceMenuFamily, ConferenceParticipantActionsDocument,
    PHONE_BACKGROUND_APPLICATION_ID, PHONE_EXECUTE_MAX_BYTES, PHONE_IMAGE_MAX_BYTES,
    PHONE_INPUT_MAX_BYTES, PHONE_RINGTONE_APPLICATION_ID, PHONE_STATUS_MAX_BYTES,
    PHONE_TEXT_APPLICATION_ID, PHONE_TEXT_LEGACY_MAX_CHARS, PhoneAlarmTelemetry,
    PhoneBackgroundControlDocument, PhoneImageDocument, PhoneLocationTelemetry,
    PhoneServicePriority, PhoneStatusDocument, PhoneXmlError, parse_phone_alarm,
    parse_phone_location,
};
use crate::types::SignalingQos;
use crate::types::{
    ApplicationId, AudioProcessingPolicy, BlfCallerInfo, BlfState, ButtonDefinition, CallId,
    CallInfo, CallReference, ConferenceId, DEFAULT_AUDIO_MAX_FRAMES_PER_PACKET,
    DEFAULT_AUDIO_PACKET_MS, DeviceDefinition, DeviceId, DeviceRegistration, LineAppearance,
    LineDefinition, LineInstance, MediaEndpoint, MediaTrafficClass, ParticipantId,
    PassthroughPartyId, SessionGeneration, SoftKeyProfile, StationTransport,
    StationTransportRequirement, TransactionId,
};
use transport::AcceptedStation;

const EVENT_CAPACITY: usize = 1024;
const COMMAND_CAPACITY: usize = 1024;
const SESSION_COMMAND_CAPACITY: usize = 256;
const SESSION_ACCEPT_CAPACITY: usize = 128;
/// Maximum time a phone may leave an ordering-sensitive media command
/// unacknowledged before the call owner is notified and the stale correlation
/// state is retired.
pub const HANDSET_ACKNOWLEDGEMENT_TIMEOUT: Duration = Duration::from_secs(5);
/// Bound for the writer acknowledgement used to serialize commands whose
/// resources must remain owned until their complete frame reaches the socket.
pub const ORDERING_ACKNOWLEDGEMENT_TIMEOUT: Duration = Duration::from_secs(5);
// A timeout only releases pending correlation state; statistics are never polled.
const CONNECTION_STATISTICS_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_PENDING_CONNECTION_STATISTICS: usize = 32;
// Retired references prevent a late reply from binding to a replacement call.
const MAX_STATISTICS_REFERENCES_PER_SESSION: usize = 4096;
const PARKING_APPLICATION_ID: u32 = 9090;
/// Shortest retry delay accepted for a rejected registration token.
pub const MIN_REGISTRATION_BACKOFF: Duration = Duration::from_secs(30);
/// Longest retry delay accepted for a rejected registration token.
pub const MAX_REGISTRATION_BACKOFF: Duration = Duration::from_secs(86_400);
/// Maximum number of parked calls rendered in one station selection menu.
///
/// Higher-level parking state may contain more calls; callers select and order
/// the bounded subset passed to [`CommandAction::ShowParkingMenu`].
pub const PARKING_MENU_MAX_ITEMS: usize = 32;

/// One selectable parked call rendered by the station parking application.
///
/// `slot` is the stable parking-space identifier returned by a subsequent
/// [`DeviceEventKind::ParkingMenuSelection`]. The caller and connected-party
/// fields are presentation text and do not participate in selection identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParkingMenuEntry {
    pub slot: u32,
    pub caller_name: String,
    pub caller_number: String,
    pub connected_name: String,
    pub connected_number: String,
}

/// Audible presentation to apply to a newly offered incoming call.
///
/// Passing `None` to an incoming-offer method presents the call silently;
/// [`IncomingRing::default`] selects an ordinary inside ring.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IncomingRing {
    pub mode: RingerMode,
    pub duration: RingDuration,
}

/// Device-wide do-not-disturb state rendered by configured feature buttons.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DoNotDisturbMode {
    Off,
    Silent,
    Reject,
}

/// Behavior selected for one do-not-disturb feature button.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DoNotDisturbButtonMode {
    #[default]
    Cycle,
    Silent,
    Reject,
}

impl Default for IncomingRing {
    fn default() -> Self {
        Self {
            mode: RingerMode::Inside,
            duration: RingDuration::Normal,
        }
    }
}

/// One fully correlated, privacy-safe station media snapshot retained independently of call
/// teardown. The firmware-specific quality payload remains opaque and is represented only by its
/// bounded byte count.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MediaStatisticsSnapshot {
    /// Monotonic generation assigned to the statistics request.
    ///
    /// A response is retained only when it matches the live request generation,
    /// preventing a delayed response from replacing newer statistics.
    pub request_generation: u64,
    pub call_id: CallId,
    pub line_instance: LineInstance,
    pub codec: Codec,
    pub packet_ms: u32,
    pub max_frames_per_packet: u32,
    pub receive_peer: Option<MediaEndpoint>,
    pub transmit_peer: Option<MediaEndpoint>,
    pub packets_sent: u32,
    pub octets_sent: u32,
    pub packets_received: u32,
    pub octets_received: u32,
    pub packets_lost: u32,
    pub jitter_millis: u32,
    pub latency_millis: u32,
    /// Length of the bounded opaque quality report; its contents are not
    /// retained in management state.
    pub quality_byte_count: usize,
}

/// Device-wide status-line mutation.
///
/// The optional priority selects a protocol-specific notification plane. A
/// clear with a priority removes only that plane; an unqualified clear removes
/// the ordinary status message.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HandsetStatusMessage {
    Display {
        text: String,
        /// Zero keeps the message until another status mutation replaces it.
        timeout_seconds: u8,
        priority: Option<NotificationPriority>,
    },
    Clear {
        priority: Option<NotificationPriority>,
    },
}

/// Immutable policy shared by every session owned by one [`Server`].
///
/// Station definitions are supplied separately to the constructor and may be
/// replaced at runtime through [`ServerHandle::reconfigure`].
#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub bind: SocketAddr,
    /// Baseline marking applied before a station identifies itself. A device
    /// definition may replace it for the remainder of that session.
    pub signaling_qos: SignalingQos,
    /// Configured fallback used only if the accepted socket does not have a
    /// concrete local address. Normal server-list replies use the local
    /// interface selected by the operating system for that connection.
    pub advertised_address: Ipv4Addr,
    /// IPv6 fallback for an unspecified accepted local socket.
    pub advertised_ipv6_address: Option<Ipv6Addr>,
    pub server_name: String,
    /// Keepalive interval advertised to stations; session expiry uses a bounded
    /// multiple of this interval.
    pub keepalive_seconds: u32,
    /// Keepalive interval advertised for sessions using a secondary server.
    pub secondary_keepalive_seconds: u32,
    /// Ordered failover endpoints. An empty list advertises only the endpoint
    /// that accepted the current connection.
    pub signaling_servers: Vec<SignalingServerRoute>,
    /// Admission and retry policy for pre-registration token probes.
    pub registration_tokens: RegistrationTokenPolicy,
    pub firmware_version: String,
    pub dial_terminator: Digit,
    pub record_dial_terminator: bool,
    pub call_answer_order: CallSelectionOrder,
    /// Fixed station wall-clock offset from UTC. SCCP does not carry a named
    /// timezone or daylight-saving transition table.
    pub timezone_offset_minutes: i16,
    pub date_template: crate::types::DateTemplate,
    /// Optional policy-neutral station template for an otherwise unknown
    /// guest-hotline registration. The dial destination remains owned by the
    /// channel adapter and is never exposed in the handset definition.
    pub anonymous_hotline: Option<AnonymousHotlineDefinition>,
}

/// One configured server and the ports available for each signaling transport.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SignalingServerRoute {
    pub priority: u8,
    pub name: String,
    pub address: IpAddr,
    pub clear_port: Option<NonZeroU16>,
    pub secure_port: Option<NonZeroU16>,
}

impl SignalingServerRoute {
    fn endpoint(&self, transport: StationTransport) -> Option<SignalingServerEndpoint> {
        let port = match transport {
            StationTransport::Clear => self.clear_port,
            StationTransport::Secure => self.secure_port,
        }?;
        Some(SignalingServerEndpoint {
            name: self.name.clone(),
            address: self.address,
            port,
        })
    }
}

/// Decision applied to a pre-registration token probe after station and
/// transport eligibility have been established.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RegistrationFallback {
    #[default]
    Reject,
    ReturnToPrimary,
    DeviceIdOdd,
    DeviceIdEven,
}

/// Policy applied when a station probes this server while registered elsewhere.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegistrationTokenPolicy {
    pub fallback: RegistrationFallback,
    pub backoff: Duration,
    pub server_priority: u8,
}

impl Default for RegistrationTokenPolicy {
    fn default() -> Self {
        Self {
            fallback: RegistrationFallback::Reject,
            backoff: Duration::from_secs(60),
            server_priority: 1,
        }
    }
}

impl RegistrationTokenPolicy {
    fn accepts(&self, device_id: &DeviceId) -> bool {
        let last_nibble = device_id
            .as_str()
            .strip_prefix("SEP")
            .filter(|mac| mac.len() == 12 && mac.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .and_then(|mac| mac.as_bytes().last().copied())
            .and_then(|byte| char::from(byte).to_digit(16));
        match self.fallback {
            RegistrationFallback::Reject => false,
            RegistrationFallback::ReturnToPrimary => self.server_priority == 1,
            RegistrationFallback::DeviceIdOdd => last_nibble.is_some_and(|value| value % 2 == 1),
            RegistrationFallback::DeviceIdEven => last_nibble.is_some_and(|value| value % 2 == 0),
        }
    }
}

/// Restricted definition used to admit an otherwise unknown station as a
/// single-line hotline device.
///
/// The server supplies only station-visible policy. Routing and authorization
/// of the resulting off-hook event remain the adapter's responsibility.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnonymousHotlineDefinition {
    label: String,
}

impl AnonymousHotlineDefinition {
    /// Build a hotline template with a validated station-visible label.
    ///
    /// Labels must contain 1 through 79 bytes and no control characters.
    pub fn new(label: impl Into<String>) -> Result<Self, ServerError> {
        let label = label.into();
        if label.is_empty() || label.len() > 79 || label.chars().any(char::is_control) {
            return Err(ServerError::InvalidConfig(
                "anonymous-hotline label must contain 1..=79 non-control bytes".into(),
            ));
        }
        Ok(Self { label })
    }

    fn device_definition(&self, id: DeviceId) -> DeviceDefinition {
        let soft_keys = SoftKeyProfile::new(KeyMode::ALL_KNOWN.iter().copied().map(|mode| {
            let actions = match mode {
                KeyMode::OnHook => vec![SoftKey::NewCall],
                KeyMode::OffHook | KeyMode::RingOut => vec![SoftKey::EndCall],
                _ => Vec::new(),
            };
            (mode, actions)
        }))
        .expect("minimal anonymous-hotline soft keys are valid");
        DeviceDefinition {
            id,
            description: self.label.clone(),
            transport: StationTransportRequirement::Either,
            signaling_qos: None,
            buttons: vec![ButtonDefinition::Line(LineAppearance::new(
                1,
                LineDefinition {
                    number: "hotline".into(),
                    display_name: self.label.clone(),
                },
            ))],
            soft_keys,
            ui: Default::default(),
        }
    }
}

/// Ordering used when a station asks to answer without identifying a call.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CallSelectionOrder {
    #[default]
    OldestFirst,
    LastFirst,
}

/// Network and framing policy for one station multicast audio direction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MulticastMediaRoute {
    pub address: IpAddr,
    pub port: u16,
    pub codec: Codec,
    pub packet_millis: u32,
}

/// Complete application-owned description of one station video receive flow.
///
/// Session-owned call, line, and request identities are deliberately absent.
/// The opaque payload is accepted only when retained from a decoded receive
/// message for the live session's protocol version.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MultimediaReceiveDescriptor {
    pub conference_id: ConferenceId,
    pub payload: MultimediaPayload,
    pub conference_creator: bool,
    pub encryption: Option<MediaEncryption>,
    pub stream_passthrough_id: u32,
    pub associated_stream_id: u32,
    pub source: MediaEndpointAddress,
    pub requested_address_type: IpAddressType,
}

impl MultimediaReceiveDescriptor {
    /// Rejects descriptors whose typed envelope cannot represent a video
    /// receive flow. Session-specific protocol and station capabilities are
    /// checked when the command is dispatched.
    pub fn validate(self) -> Result<Self, ServerError> {
        validate_multimedia_receive_descriptor(&self)?;
        Ok(self)
    }
}

/// Complete application-owned description of one station video transmit flow.
///
/// The opaque payload is accepted only when retained from a decoded transmit
/// message for the live session's protocol version. Call and request identities
/// remain session-owned.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MultimediaTransmitDescriptor {
    pub conference_id: ConferenceId,
    pub endpoint: MediaEndpointAddress,
    pub payload: MultimediaPayload,
    /// Full traffic-class octet; configuration DSCP is shifted left by two.
    pub traffic_class: MediaTrafficClass,
    pub encryption: Option<MediaEncryption>,
    pub stream_passthrough_id: u32,
    pub associated_stream_id: u32,
}

impl MultimediaTransmitDescriptor {
    /// Rejects descriptors whose typed envelope cannot represent a video
    /// transmit flow. Live session policy is checked during dispatch.
    pub fn validate(self) -> Result<Self, ServerError> {
        validate_multimedia_transmit_descriptor(&self)?;
        Ok(self)
    }
}

/// Parameters for one command applied to an exact live station video encoder.
///
/// The server derives the wire command selector and fixed parameter area from
/// these variants. Arbitrary parameter bytes are intentionally unavailable at
/// this stateful command boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MultimediaTransmitControl {
    FreezePicture,
    FastPictureUpdate {
        first_gob: u32,
        gob_count: u32,
    },
    FastGobUpdate {
        first_gob: u32,
        gob_count: u32,
    },
    FastMacroblockUpdate {
        first_gob: u32,
        first_macroblock: u32,
        macroblock_count: u32,
    },
    LostPicture {
        picture_number: u32,
        long_term_picture_index: u32,
    },
    LostPartialPicture {
        picture_number: u32,
        long_term_picture_index: u32,
        first_macroblock: u32,
        macroblock_count: u32,
    },
    /// Requests recovery from at most four prior picture references.
    RecoveryReferencePicture {
        pictures: VideoPictureReferences,
    },
    TemporalSpatialTradeoff {
        value: u32,
    },
}

/// Picture identity carried by recovery feedback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VideoPictureReference {
    pub picture_number: u32,
    pub long_term_picture_index: u32,
}

/// A recovery request's bounded ordered picture-reference list.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VideoPictureReferences(Box<[VideoPictureReference]>);

impl VideoPictureReferences {
    /// Collects at most five items before rejecting a list beyond the wire
    /// capacity, so even an unbounded iterator cannot cause unbounded storage.
    pub fn new(
        pictures: impl IntoIterator<Item = VideoPictureReference>,
    ) -> Result<Self, ServerError> {
        let pictures = pictures.into_iter().take(5).collect::<Vec<_>>();
        if pictures.len() > 4 {
            return Err(ServerError::InvalidMultimediaTransmitControl(
                "recovery picture count exceeds four",
            ));
        }
        Ok(Self(pictures.into_boxed_slice()))
    }

    /// Returns picture identities in wire order.
    pub fn as_slice(&self) -> &[VideoPictureReference] {
        &self.0
    }
}

impl TryFrom<Vec<VideoPictureReference>> for VideoPictureReferences {
    type Error = ServerError;

    fn try_from(pictures: Vec<VideoPictureReference>) -> Result<Self, Self::Error> {
        Self::new(pictures)
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([0, 0, 0, 0], 2000)),
            signaling_qos: SignalingQos::default(),
            advertised_address: Ipv4Addr::LOCALHOST,
            advertised_ipv6_address: None,
            server_name: "sccp-protocol".to_string(),
            keepalive_seconds: 30,
            secondary_keepalive_seconds: 30,
            signaling_servers: Vec::new(),
            registration_tokens: RegistrationTokenPolicy::default(),
            firmware_version: String::new(),
            dial_terminator: Digit::Pound,
            record_dial_terminator: false,
            call_answer_order: CallSelectionOrder::OldestFirst,
            timezone_offset_minutes: 0,
            date_template: Default::default(),
            anonymous_hotline: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Output emitted by the running server to its integration adapter.
///
/// The receiver returned by a server constructor is the sole event stream for
/// every accepted connection. Consumers should drain it continuously and use
/// the device ID carried by [`Event::Device`] rather than inferring ownership
/// from event order. They should also retain the generation from the latest
/// registration and reject later-delivered events from replaced sessions.
pub enum Event {
    SessionError {
        peer: SocketAddr,
        error: String,
    },
    /// A malformed non-registration message was discarded while the session
    /// remained usable.
    ProtocolWarning {
        peer: SocketAddr,
        /// Registered device identity, or `None` before registration.
        device_id: Option<DeviceId>,
        message_id: u32,
        error: String,
    },
    Device(DeviceEvent),
}

impl Event {
    pub fn device(
        device_id: DeviceId,
        session_generation: SessionGeneration,
        event: DeviceEventKind,
    ) -> Self {
        Self::Device(DeviceEvent::new(device_id, session_generation, event))
    }
}

/// One station-scoped item in the server event stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceEvent {
    pub device_id: DeviceId,
    /// Identifies the connection that produced this event across reconnects.
    pub session_generation: SessionGeneration,
    pub event: DeviceEventKind,
}

impl DeviceEvent {
    pub fn new(
        device_id: DeviceId,
        session_generation: SessionGeneration,
        event: DeviceEventKind,
    ) -> Self {
        Self {
            device_id,
            session_generation,
            event,
        }
    }
}

/// State transitions and handset input produced by one station session.
///
/// Call-bearing variants use the server-owned [`CallId`] rather than the raw
/// wire reference. Media success and failure variants are emitted only after
/// correlation against the current request generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeviceEventKind {
    Registered(DeviceRegistration),
    Disconnected {},
    Capabilities {
        capabilities: StationMediaCapabilities,
    },
    OffHook {
        call_id: CallId,
        line_instance: LineInstance,
    },
    OnHook {
        call_id: CallId,
        line_instance: LineInstance,
    },
    Digit {
        call_id: CallId,
        digit: Digit,
    },
    EnblocCall {
        call_id: CallId,
        line_instance: LineInstance,
        number: String,
    },
    SoftKey {
        /// Active call when the key is call-scoped.
        call_id: Option<CallId>,
        line_instance: LineInstance,
        soft_key: SoftKey,
    },
    LineButton {
        line_instance: LineInstance,
        call_id: Option<CallId>,
    },
    HookFlash {
        call_id: Option<CallId>,
        line_instance: LineInstance,
    },
    FeatureButton {
        instance: LineInstance,
    },
    DoNotDisturbButton {
        instance: LineInstance,
    },
    MobilityButton {
        instance: LineInstance,
    },
    /// Press of a configured voicemail button, resolved to an exact line and
    /// addressable handset call before application dispatch.
    VoicemailButton {
        call_id: CallId,
        line_instance: LineInstance,
    },
    ParkingLotButton {
        /// Feature-button instance, distinct from the call line instance.
        instance: LineInstance,
        /// Active call associated with the press, when any.
        call_id: Option<CallId>,
        line_instance: LineInstance,
    },
    ParkingMenuSelection {
        lot: String,
        slot: u32,
    },
    PhoneServiceResponse {
        response: PhoneServiceEvent,
    },
    ConferenceListAction {
        action: ConferenceListAction,
    },
    /// The receive-channel acknowledgement matched the current request.
    ReceiveChannelOpened {
        call_id: CallId,
        status: MediaStatus,
        endpoint: MediaEndpoint,
    },
    MultimediaReceiveChannelOpened {
        call_id: CallId,
        codec: Codec,
        endpoint: MediaEndpointAddress,
        passthrough_party_id: PassthroughPartyId,
    },
    MultimediaReceiveChannelFailed {
        call_id: CallId,
        codec: Codec,
        status: MediaStatus,
        endpoint: MediaEndpointAddress,
        passthrough_party_id: PassthroughPartyId,
    },
    MultimediaReceiveChannelTimedOut {
        call_id: CallId,
        codec: Codec,
        passthrough_party_id: PassthroughPartyId,
    },
    MultimediaTransmitStarted {
        call_id: CallId,
        codec: Codec,
        endpoint: MediaEndpointAddress,
        passthrough_party_id: PassthroughPartyId,
    },
    MultimediaTransmitFailed {
        call_id: CallId,
        codec: Codec,
        status: MediaStatus,
        endpoint: MediaEndpointAddress,
        passthrough_party_id: PassthroughPartyId,
    },
    MultimediaTransmitTimedOut {
        call_id: CallId,
        codec: Codec,
        passthrough_party_id: PassthroughPartyId,
    },
    /// The transmit half of a coupled outbound media transaction became open
    /// when the station acknowledged the matching receive request.
    ///
    /// The adjacent StartMediaTransmission request may omit its separate
    /// acknowledgement. This event is
    /// deliberately distinct from [`DeviceEventKind::TransmitChannelStarted`]: callers
    /// can preserve the acknowledgement relationship while settling
    /// both halves of the one coupled transaction exactly once.
    TransmitChannelImplied {
        call_id: CallId,
        endpoint: MediaEndpoint,
    },
    /// The transmit-channel acknowledgement matched the current request.
    TransmitChannelStarted {
        call_id: CallId,
        status: MediaStatus,
        endpoint: MediaEndpoint,
    },
    HandsetAcknowledgementTimedOut {
        call_id: CallId,
        acknowledgement: HandsetAcknowledgement,
    },
    MediaTransmissionFailed {
        call_id: CallId,
        status: MediaStatus,
        endpoint: MediaEndpoint,
    },
    MulticastReceptionStarted {
        conference_id: ConferenceId,
        call_id: CallId,
        route: MulticastMediaRoute,
    },
    MulticastReceptionFailed {
        conference_id: ConferenceId,
        call_id: CallId,
        status: MediaStatus,
    },
    MulticastReceptionTimedOut {
        conference_id: ConferenceId,
        call_id: CallId,
    },
    MulticastTransmissionStarted {
        conference_id: ConferenceId,
        call_id: CallId,
        route: MulticastMediaRoute,
    },
    MulticastTransmissionFailed {
        conference_id: ConferenceId,
        call_id: CallId,
        status: MediaStatus,
        address: IpAddr,
        port: u16,
    },
    /// A requested statistics response was correlated and retained.
    ConnectionStatisticsCollected {
        snapshot: MediaStatisticsSnapshot,
    },
    Alarm {
        severity: AlarmSeverity,
        text: String,
        parameters: Option<[u32; 2]>,
    },
    XmlAlarm {
        telemetry: PhoneAlarmTelemetry,
    },
    LocationInformation {
        telemetry: PhoneLocationTelemetry,
    },
    HeadsetStatusChanged {
        enabled: bool,
    },
    MediaPathChanged {
        path: crate::message::values::MediaPathId,
        event: crate::message::values::MediaPathEvent,
    },
    /// A well-framed client message has no server-side behavior yet.
    UnhandledMessage {
        message: ClientMessage,
    },
}

/// Station acknowledgement whose correlation deadline expired.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HandsetAcknowledgement {
    OpenReceiveChannel,
    StartMediaTransmission,
}

/// One station-targeted operation submitted through [`ServerHandle`].
///
/// Construction does not consult live session state; target and call
/// availability are checked when the running server dispatches the action.
#[derive(Clone, Debug)]
pub struct Command {
    pub device_id: DeviceId,
    pub action: CommandAction,
}

impl Command {
    pub fn new(device_id: DeviceId, action: CommandAction) -> Self {
        Self { device_id, action }
    }
}

/// Operation applied to the target station session in command-queue order.
///
/// Call-scoped actions resolve the server-owned [`CallId`] to the current wire
/// reference inside the session. Actions for stale calls are ignored, except
/// that [`Self::CloseCall`] also records an early cancellation so it can retire
/// an incoming offer that has not reached the session yet.
///
/// Outbound presentation normally progresses from [`Self::BeginCall`] through
/// proceeding or ringing to [`Self::CommitOutboundCall`]. Media actions allocate
/// a fresh request identity for each generation; close and stop actions retire
/// that identity so a late acknowledgement cannot settle replacement media.
#[derive(Clone, Debug)]
pub enum CommandAction {
    /// Create local station state and off-hook presentation for an outbound
    /// call whose adapter-side identity is already allocated.
    BeginCall {
        line_instance: LineInstance,
        call_id: CallId,
        codec: Codec,
    },
    /// Put a held source call into transfer mode and create its consultation
    /// call as the active off-hook presentation.
    BeginTransfer {
        source_call_id: CallId,
        consultation_line_instance: LineInstance,
        consultation_call_id: CallId,
        codec: Codec,
    },
    SetCallInfo {
        call_id: CallId,
        info: CallInfo,
    },
    CommitOutboundCall {
        call_id: CallId,
        info: CallInfo,
    },
    PresentOutboundProceeding {
        call_id: CallId,
        info: CallInfo,
    },
    PresentOutboundRinging {
        call_id: CallId,
        info: CallInfo,
    },
    SetCallState {
        call_id: CallId,
        state: CallState,
    },
    SetCallSelected {
        call_id: CallId,
        selected: bool,
    },
    DisplayPrompt {
        call_id: CallId,
        timeout_seconds: u32,
        text: String,
    },
    ClearPrompt {
        call_id: CallId,
    },
    SetStatusMessage {
        message: HandsetStatusMessage,
        beep: bool,
    },
    SetMicrophoneMode {
        enabled: bool,
    },
    SetRecordingStatus {
        call_id: CallId,
        active: bool,
    },
    ResetDevice {
        reset_type: ResetType,
    },
    SetMwi {
        line_instance: LineInstance,
        enabled: bool,
    },
    /// Replace all three forwarding destinations displayed for a line.
    ///
    /// `None` clears that forwarding kind; the server retains the complete
    /// triple for subsequent station queries.
    SetForwardStatus {
        line_instance: LineInstance,
        forward_all: Option<String>,
        forward_busy: Option<String>,
        forward_no_answer: Option<String>,
    },
    SetFeatureStatus {
        instance: LineInstance,
        enabled: bool,
    },
    SetDoNotDisturbStatus {
        instance: LineInstance,
        mode: DoNotDisturbMode,
        button_mode: DoNotDisturbButtonMode,
    },
    /// Install or remove the temporary line appearance owned by a mobility
    /// button, rebuilding the station button template atomically.
    SetMobilityAppearance {
        mobility_instance: LineInstance,
        appearance: Option<LineAppearance>,
    },
    SetBlfStatus {
        instance: LineInstance,
        number: String,
        label: String,
        state: BlfState,
        caller: Option<BlfCallerInfo>,
    },
    ShowParkingMenu {
        instance: LineInstance,
        transaction_id: TransactionId,
        lot: String,
        calls: Vec<ParkingMenuEntry>,
    },
    ShowConferenceList {
        call_id: CallId,
        conference_id: ConferenceId,
        participants: Vec<ConferenceListEntry>,
    },
    ShowConferenceParticipantActions {
        call_id: CallId,
        conference_id: ConferenceId,
        participant: ConferenceListEntry,
        removable: bool,
        demotable: bool,
    },
    ShowTextService {
        line_instance: LineInstance,
        call_reference: CallReference,
        transaction_id: TransactionId,
        priority: PhoneServicePriority,
        document: CiscoIpPhoneText,
    },
    /// Send an input-service form whose response is emitted as
    /// [`DeviceEventKind::PhoneServiceResponse`].
    ShowInputService {
        line_instance: LineInstance,
        call_reference: CallReference,
        application_id: ApplicationId,
        transaction_id: TransactionId,
        priority: PhoneServicePriority,
        document: CiscoIpPhoneInput,
    },
    ExecutePhoneActions {
        line_instance: LineInstance,
        call_reference: CallReference,
        application_id: ApplicationId,
        transaction_id: TransactionId,
        priority: PhoneServicePriority,
        document: CiscoIpPhoneExecute,
    },
    ShowImageService {
        line_instance: LineInstance,
        call_reference: CallReference,
        application_id: ApplicationId,
        transaction_id: TransactionId,
        priority: PhoneServicePriority,
        document: PhoneImageDocument,
    },
    ShowStatusService {
        line_instance: LineInstance,
        call_reference: CallReference,
        application_id: ApplicationId,
        transaction_id: TransactionId,
        priority: PhoneServicePriority,
        document: PhoneStatusDocument,
    },
    SetBackgroundImage {
        transaction_id: TransactionId,
        document: CiscoIpPhoneSetBackground,
    },
    PreviewBackgroundImage {
        transaction_id: TransactionId,
        document: CiscoIpPhoneSetBackgroundPreview,
    },
    SetRingtone {
        transaction_id: TransactionId,
        document: CiscoIpPhoneSetRingTone,
    },
    StartTone {
        call_id: CallId,
        tone: Tone,
    },
    /// Start one or more conference announcements with an explicit participant
    /// hearing mask and playback mode.
    StartAnnouncement {
        conference_id: ConferenceId,
        announcements: Vec<AnnouncementEntry>,
        /// Marks the final request in an acknowledgement-delimited sequence.
        end_of_ack: bool,
        participant_ids: Vec<ParticipantId>,
        /// Bit mask selecting which listed participants hear the announcement.
        hearing_participant_mask: u32,
        /// Protocol playback-mode value retained for station interpretation.
        play_mode: u32,
    },
    StopAnnouncement {
        conference_id: ConferenceId,
    },
    AnnouncementFinish {
        conference_id: ConferenceId,
        play_status: u32,
    },
    StartRinging {
        call_id: CallId,
    },
    StopRinging {
        call_id: CallId,
    },
    /// Ask the station to allocate its receive channel and begin a correlated
    /// media transaction.
    OpenReceiveChannel {
        call_id: CallId,
        /// Optional RTP source restriction. `None` accepts media from any
        /// source and is encoded as the SCCP wildcard endpoint `0.0.0.0:0`.
        source: Option<MediaEndpoint>,
        codec: Codec,
        packet_ms: u32,
        max_frames_per_packet: u32,
        dtmf_mode: DtmfMode,
        audio_processing: AudioProcessingPolicy,
    },
    /// Requires a connected call and an exact advertised receive capability;
    /// replacing a live generation writes its close first.
    OpenMultimediaReceiveChannel {
        call_id: CallId,
        descriptor: MultimediaReceiveDescriptor,
    },
    CloseMultimediaReceiveChannel {
        call_id: CallId,
    },
    /// Requires a connected call and an exact advertised transmit capability;
    /// replacing a live generation writes its stop first.
    StartMultimediaTransmission {
        call_id: CallId,
        descriptor: MultimediaTransmitDescriptor,
    },
    StopMultimediaTransmission {
        call_id: CallId,
    },
    /// Limits the exact live station video encoder identified by its current
    /// passthrough token.
    SetMultimediaTransmitBitRate {
        call_id: CallId,
        passthrough_party_id: PassthroughPartyId,
        maximum_bit_rate: u32,
    },
    /// Reports a bit-rate change for the exact live station video encoder.
    NotifyMultimediaTransmitBitRate {
        call_id: CallId,
        passthrough_party_id: PassthroughPartyId,
        maximum_bit_rate: u32,
    },
    /// Applies typed feedback to the exact live station video encoder.
    ControlMultimediaTransmission {
        call_id: CallId,
        passthrough_party_id: PassthroughPartyId,
        control: MultimediaTransmitControl,
    },
    /// Open both directions of an outbound media path in one session-writer
    /// transaction. The two SCCP frames are written ORC then SMT without a
    /// command-queue or acknowledgement boundary between them.
    OpenOutboundMedia {
        call_id: CallId,
        source: Option<MediaEndpoint>,
        endpoint: MediaEndpoint,
        codec: Codec,
        packet_ms: u32,
        max_frames_per_packet: u32,
        dtmf_mode: DtmfMode,
        audio_processing: AudioProcessingPolicy,
        traffic_class: MediaTrafficClass,
    },
    /// Close the station receive leg and retire its pending acknowledgement.
    CloseReceiveChannel {
        call_id: CallId,
    },
    StartMedia {
        call_id: CallId,
        endpoint: MediaEndpoint,
        dtmf_mode: DtmfMode,
        audio_processing: AudioProcessingPolicy,
        traffic_class: MediaTrafficClass,
    },
    StartMulticastReception {
        conference_id: ConferenceId,
        call_id: CallId,
        route: MulticastMediaRoute,
        echo_cancellation: EchoCancellation,
        g723_bitrate: G723BitRate,
    },
    StopMulticastReception {
        conference_id: ConferenceId,
        call_id: CallId,
    },
    StartMulticastTransmission {
        conference_id: ConferenceId,
        call_id: CallId,
        route: MulticastMediaRoute,
        precedence: u32,
        silence_suppression: SilenceSuppression,
        max_frames_per_packet: u32,
        g723_bitrate: G723BitRate,
    },
    StopMulticastTransmission {
        conference_id: ConferenceId,
        call_id: CallId,
    },
    /// Stop the station transmit leg and retire its pending acknowledgement.
    StopMedia {
        call_id: CallId,
    },
    /// Tear down station media and presentation, request final statistics when
    /// applicable, and retire the call identity.
    CloseCall {
        call_id: CallId,
    },
    DisconnectDevice {},
}

/// Failure returned by server construction, command submission, session I/O,
/// or stateful command validation.
///
/// Queue-admission failures do not imply that an earlier command failed.
/// [`Self::CommandWrite`] and [`Self::CommandAcknowledgementTimeout`] are
/// specific to [`ServerHandle::send_confirmed`]; protocol-level media outcomes
/// instead arrive through [`Event`].
#[derive(Debug, Error)]
pub enum ServerError {
    #[error("failed to bind SCCP server: {0}")]
    Bind(#[source] std::io::Error),
    #[error("SCCP server I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("SCCP protocol error: {0}")]
    Protocol(#[from] CodecError),
    #[error("invalid SCCP server configuration: {0}")]
    InvalidConfig(String),
    #[error("phone XML error: {0}")]
    PhoneXml(#[from] PhoneXmlError),
    #[error("device {0} is not connected")]
    DeviceNotConnected(DeviceId),
    #[error("call {0:?} does not exist")]
    UnknownCall(CallId),
    #[error("call {call_id:?} cannot {operation} while in state {state:?}")]
    InvalidCallTransaction {
        call_id: CallId,
        operation: &'static str,
        state: CallState,
    },
    #[error("SCCP server has stopped")]
    Stopped,
    #[error("SCCP server command queue is full")]
    CommandQueueFull,
    #[error("SCCP command could not be written to the device: {0}")]
    CommandWrite(String),
    #[error("SCCP command writer acknowledgement timed out")]
    CommandAcknowledgementTimeout,
    #[error("SCCP media request identity space is exhausted")]
    MediaRequestIdentityExhausted,
    #[error("SCCP station session generation space is exhausted")]
    SessionGenerationExhausted,
    #[error("invalid multicast media policy: {0}")]
    InvalidMulticastMedia(&'static str),
    #[error("station does not advertise the requested multicast codec")]
    UnsupportedMulticastCodec,
    #[error("invalid multimedia receive policy: {0}")]
    InvalidMultimediaReceive(&'static str),
    #[error("station does not advertise the requested video receive capability")]
    UnsupportedMultimediaReceive,
    #[error("invalid multimedia transmit policy: {0}")]
    InvalidMultimediaTransmit(&'static str),
    #[error("station does not advertise the requested video transmit capability")]
    UnsupportedMultimediaTransmit,
    #[error("invalid multimedia transmit control: {0}")]
    InvalidMultimediaTransmitControl(&'static str),
    #[error(
        "call {call_id:?} has no open multimedia transmit stream with passthrough token {passthrough_party_id}"
    )]
    StaleMultimediaTransmitControl {
        call_id: CallId,
        passthrough_party_id: PassthroughPartyId,
    },
    #[error("{message} is a control/service-node message, not a station command")]
    InvalidStationCommand { message: &'static str },
}

impl ServerError {
    const fn is_nonfatal_command_rejection(&self) -> bool {
        matches!(
            self,
            Self::InvalidCallTransaction { .. }
                | Self::InvalidStationCommand { .. }
                | Self::InvalidMulticastMedia(_)
                | Self::UnsupportedMulticastCodec
                | Self::InvalidMultimediaReceive(_)
                | Self::UnsupportedMultimediaReceive
                | Self::InvalidMultimediaTransmit(_)
                | Self::UnsupportedMultimediaTransmit
                | Self::InvalidMultimediaTransmitControl(_)
                | Self::StaleMultimediaTransmitControl { .. }
        )
    }
}

/// Cloneable command and management endpoint for a running [`Server`].
///
/// The handle does not drive I/O itself: [`Server::run`] must remain active.
/// Clones share call-ID allocation, retained media statistics, and the bounded
/// command queue. Dropping the last handle closes that queue and lets the run
/// loop perform its normal session shutdown.
#[derive(Clone, Debug)]
pub struct ServerHandle {
    command_tx: mpsc::Sender<ServerCommand>,
    next_call_id: Arc<AtomicU64>,
    latest_media_statistics: Arc<RwLock<HashMap<DeviceId, MediaStatisticsSnapshot>>>,
    call_answer_order: Arc<RwLock<CallSelectionOrder>>,
}

/// The station definitions changed by one atomic server reconfiguration.
///
/// Only connected devices in `changed` or `removed` are disconnected. Added
/// devices have no session to disrupt, while definitions absent from every
/// list keep their live session and calls.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ReconfigureResult {
    pub added: Vec<DeviceId>,
    pub changed: Vec<DeviceId>,
    pub removed: Vec<DeviceId>,
}

impl ReconfigureResult {
    pub fn is_unchanged(&self) -> bool {
        self.added.is_empty() && self.changed.is_empty() && self.removed.is_empty()
    }

    fn disconnected_devices(&self) -> impl Iterator<Item = &DeviceId> {
        self.changed.iter().chain(&self.removed)
    }
}

impl ServerHandle {
    /// Applies to future answer requests that omit their call reference.
    /// Explicit references and existing session calls are not rewritten.
    pub fn set_call_answer_order(&self, order: CallSelectionOrder) {
        *self
            .call_answer_order
            .write()
            .expect("SCCP call-answer-order lock poisoned") = order;
    }

    /// Return the latest fully correlated statistics response for a device.
    ///
    /// Snapshots survive call and session teardown until a newer response for
    /// that device replaces them or the server is dropped.
    pub fn latest_media_statistics(&self, device_id: &DeviceId) -> Option<MediaStatisticsSnapshot> {
        self.latest_media_statistics
            .read()
            .expect("SCCP media-statistics lock poisoned")
            .get(device_id)
            .cloned()
    }

    /// Clone every retained per-device snapshot, releasing the internal lock before a caller
    /// sorts, filters, or formats management output.
    pub fn media_statistics(&self) -> Vec<(DeviceId, MediaStatisticsSnapshot)> {
        self.latest_media_statistics
            .read()
            .expect("SCCP media-statistics lock poisoned")
            .iter()
            .map(|(device_id, snapshot)| (device_id.clone(), snapshot.clone()))
            .collect()
    }

    /// Enqueue a station command, waiting for capacity in the server queue.
    ///
    /// Success confirms queue admission only. The command may subsequently be
    /// discarded if the target session retired, and any station response is
    /// reported separately through [`Event`]. Use [`Self::send_confirmed`] when
    /// adapter resource lifetime depends on completion of the stream write.
    pub async fn send(&self, command: Command) -> Result<(), ServerError> {
        self.command_tx
            .send(ServerCommand::Public(Box::new(command)))
            .await
            .map_err(|_| ServerError::Stopped)
    }

    /// Send a command and wait until its complete encoded frame has been
    /// written to the registered device's TCP stream.
    ///
    /// This is intentionally stronger than [`Self::send`], whose completion
    /// only means the command entered the server queue. Lifecycle-sensitive
    /// callers use this boundary before releasing resources that protect the
    /// command's on-device operation.
    pub async fn send_confirmed(&self, command: Command) -> Result<(), ServerError> {
        let expires_at = Instant::now() + ORDERING_ACKNOWLEDGEMENT_TIMEOUT;
        tokio::time::timeout_at(expires_at, async {
            let (written_tx, written_rx) = oneshot::channel();
            self.command_tx
                .send(ServerCommand::Confirmed {
                    command: Box::new(command),
                    written: written_tx,
                    expires_at,
                })
                .await
                .map_err(|_| ServerError::Stopped)?;
            written_rx
                .await
                .map_err(|_| ServerError::Stopped)?
                .map_err(ServerError::CommandWrite)
        })
        .await
        .map_err(|_| ServerError::CommandAcknowledgementTimeout)?
    }

    /// Enqueue a command without yielding, preserving the ordering of
    /// synchronous channel-driver callbacks such as call followed by hangup.
    pub fn try_send(&self, command: Command) -> Result<(), ServerError> {
        self.command_tx
            .try_send(ServerCommand::Public(Box::new(command)))
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => ServerError::CommandQueueFull,
                mpsc::error::TrySendError::Closed(_) => ServerError::Stopped,
            })
    }

    /// Allocate a call ID and enqueue an ordinarily ringing incoming offer.
    ///
    /// The returned identity is stable across all later commands and handset
    /// events for the offer. A failed enqueue still consumes the reserved ID.
    pub async fn offer_incoming_call(
        &self,
        device_id: DeviceId,
        line_instance: LineInstance,
        info: CallInfo,
    ) -> Result<CallId, ServerError> {
        let call_id = self.reserve_call_id();
        self.offer_incoming_call_with_id(device_id, line_instance, call_id, info)
            .await?;
        Ok(call_id)
    }

    /// Reserve a call ID before exposing a call to a protocol session.
    ///
    /// Channel-driver adapters use this to install all private channel state
    /// before the handset can answer the subsequent offer.
    pub fn reserve_call_id(&self) -> CallId {
        CallId(self.next_call_id.fetch_add(1, Ordering::Relaxed))
    }

    /// Offer an incoming call using an ID previously returned by
    /// [`Self::reserve_call_id`].
    pub async fn offer_incoming_call_with_id(
        &self,
        device_id: DeviceId,
        line_instance: LineInstance,
        call_id: CallId,
        info: CallInfo,
    ) -> Result<(), ServerError> {
        self.offer_incoming_call_with_id_and_ring(device_id, line_instance, call_id, info, true)
            .await
    }

    pub async fn offer_incoming_call_with_id_and_ring(
        &self,
        device_id: DeviceId,
        line_instance: LineInstance,
        call_id: CallId,
        info: CallInfo,
        audible_ring: bool,
    ) -> Result<(), ServerError> {
        self.offer_incoming_call_with_id_and_ringer(
            device_id,
            line_instance,
            call_id,
            info,
            audible_ring.then_some(IncomingRing::default()),
        )
        .await
    }

    /// Enqueue an incoming offer with explicit audible presentation.
    ///
    /// `None` creates a silent offer. `Some` applies the supplied ring mode and
    /// duration before selecting the incoming-call soft-key state.
    pub async fn offer_incoming_call_with_id_and_ringer(
        &self,
        device_id: DeviceId,
        line_instance: LineInstance,
        call_id: CallId,
        info: CallInfo,
        ringer: Option<IncomingRing>,
    ) -> Result<(), ServerError> {
        self.command_tx
            .send(ServerCommand::OfferIncoming {
                device_id,
                line_instance,
                call_id,
                info,
                ringer,
            })
            .await
            .map_err(|_| ServerError::Stopped)?;
        Ok(())
    }

    /// Enqueue an incoming offer without yielding. Channel drivers should use
    /// this from their synchronous call callback so a following hangup cannot
    /// overtake the offer.
    pub fn try_offer_incoming_call_with_id(
        &self,
        device_id: DeviceId,
        line_instance: LineInstance,
        call_id: CallId,
        info: CallInfo,
    ) -> Result<(), ServerError> {
        self.try_offer_incoming_call_with_id_and_ring(device_id, line_instance, call_id, info, true)
    }

    /// Non-blocking form of [`Self::offer_incoming_call_with_id_and_ring`].
    ///
    /// Returns [`ServerError::CommandQueueFull`] without changing session state
    /// when immediate queue capacity is unavailable.
    pub fn try_offer_incoming_call_with_id_and_ring(
        &self,
        device_id: DeviceId,
        line_instance: LineInstance,
        call_id: CallId,
        info: CallInfo,
        audible_ring: bool,
    ) -> Result<(), ServerError> {
        self.try_offer_incoming_call_with_id_and_ringer(
            device_id,
            line_instance,
            call_id,
            info,
            audible_ring.then_some(IncomingRing::default()),
        )
    }

    pub fn try_offer_incoming_call_with_id_and_ringer(
        &self,
        device_id: DeviceId,
        line_instance: LineInstance,
        call_id: CallId,
        info: CallInfo,
        ringer: Option<IncomingRing>,
    ) -> Result<(), ServerError> {
        self.command_tx
            .try_send(ServerCommand::OfferIncoming {
                device_id,
                line_instance,
                call_id,
                info,
                ringer,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => ServerError::CommandQueueFull,
                mpsc::error::TrySendError::Closed(_) => ServerError::Stopped,
            })
    }

    /// Request orderly server shutdown.
    ///
    /// Success means the request entered the queue. The owner must still await
    /// the [`Server::run`] future to know that it stopped accepting streams and
    /// issued disconnects to every registered session.
    pub async fn shutdown(&self) -> Result<(), ServerError> {
        self.command_tx
            .send(ServerCommand::Shutdown)
            .await
            .map_err(|_| ServerError::Stopped)
    }

    /// Atomically replace the configured station definitions. Only connected
    /// stations whose definition changed or was removed are asked to register
    /// again; unchanged live sessions and calls are preserved. Success means
    /// the replacement was committed and disconnect requests were queued, not
    /// that every affected transport has already closed.
    pub async fn reconfigure(
        &self,
        definitions: impl IntoIterator<Item = DeviceDefinition>,
    ) -> Result<ReconfigureResult, ServerError> {
        self.reconfigure_affected(definitions, []).await
    }

    /// Atomically replaces station definitions and reconnects the explicit
    /// set in addition to stations whose wire definition changed. This lets a
    /// higher-level configuration owner apply line or global policy changes
    /// whose effects are not represented in [`DeviceDefinition`].
    pub async fn reconfigure_affected(
        &self,
        definitions: impl IntoIterator<Item = DeviceDefinition>,
        affected: impl IntoIterator<Item = DeviceId>,
    ) -> Result<ReconfigureResult, ServerError> {
        let mut by_id = HashMap::new();
        for definition in definitions {
            definition.validate()?;
            by_id.insert(definition.id.clone(), definition);
        }
        let (applied_tx, applied_rx) = oneshot::channel();
        self.command_tx
            .send(ServerCommand::Reconfigure {
                definitions: by_id,
                affected: affected.into_iter().collect(),
                applied: applied_tx,
            })
            .await
            .map_err(|_| ServerError::Stopped)?;
        applied_rx.await.map_err(|_| ServerError::Stopped)
    }

    /// Commits station definitions and unknown-device admission as one server
    /// transaction before any affected session is disconnected.
    pub async fn reconfigure_station_policy(
        &self,
        definitions: impl IntoIterator<Item = DeviceDefinition>,
        affected: impl IntoIterator<Item = DeviceId>,
        anonymous_hotline: Option<AnonymousHotlineDefinition>,
    ) -> Result<ReconfigureResult, ServerError> {
        let mut by_id = HashMap::new();
        for definition in definitions {
            definition.validate()?;
            by_id.insert(definition.id.clone(), definition);
        }
        let (applied_tx, applied_rx) = oneshot::channel();
        self.command_tx
            .send(ServerCommand::ReconfigureStationPolicy {
                definitions: by_id,
                affected: affected.into_iter().collect(),
                anonymous_hotline,
                applied: applied_tx,
            })
            .await
            .map_err(|_| ServerError::Stopped)?;
        applied_rx.await.map_err(|_| ServerError::Stopped)
    }

    /// Replace the unknown-device guest template for future registrations.
    /// A changed policy disconnects only sessions that were admitted through
    /// the previous anonymous template; configured sessions are untouched. The
    /// returned count is the number of such sessions asked to disconnect.
    pub async fn reconfigure_anonymous_hotline(
        &self,
        definition: Option<AnonymousHotlineDefinition>,
    ) -> Result<usize, ServerError> {
        let (applied_tx, applied_rx) = oneshot::channel();
        self.command_tx
            .send(ServerCommand::ReconfigureAnonymousHotline {
                definition,
                applied: applied_tx,
            })
            .await
            .map_err(|_| ServerError::Stopped)?;
        applied_rx.await.map_err(|_| ServerError::Stopped)
    }
}

/// Stateful owner of station admission, registration, command dispatch, and
/// event correlation.
///
/// Construction is inert: callers must poll [`Self::run`]. The server owns its
/// listener or injected-ingress receiver and all registered session routing;
/// integration code normally retains only the returned [`ServerHandle`] and
/// event receiver after spawning the run future. Dropping the `Server` future
/// directly is abrupt, so normal shutdown should use [`ServerHandle::shutdown`]
/// and then await `run`.
#[derive(Debug)]
pub struct Server {
    listener: Option<TcpListener>,
    accepted_rx: mpsc::Receiver<AcceptedStation>,
    config: Arc<ServerConfig>,
    anonymous_hotline: Arc<RwLock<Option<AnonymousHotlineDefinition>>>,
    definitions: Arc<RwLock<HashMap<DeviceId, DeviceDefinition>>>,
    sessions: Sessions,
    event_tx: mpsc::Sender<Event>,
    command_rx: mpsc::Receiver<ServerCommand>,
    next_generation: Arc<AtomicU64>,
    next_statistics_generation: Arc<AtomicU64>,
    next_call_id: Arc<AtomicU64>,
    latest_media_statistics: Arc<RwLock<HashMap<DeviceId, MediaStatisticsSnapshot>>>,
    call_answer_order: Arc<RwLock<CallSelectionOrder>>,
}

type Sessions = Arc<Mutex<HashMap<DeviceId, SessionSender>>>;
type CommandWriteConfirmation = oneshot::Sender<Result<(), String>>;

#[derive(Clone, Debug)]
struct SessionSender {
    generation: SessionGeneration,
    anonymous_hotline: bool,
    tx: mpsc::Sender<SessionCommand>,
}

#[derive(Debug)]
enum ServerCommand {
    Public(Box<Command>),
    Confirmed {
        command: Box<Command>,
        written: CommandWriteConfirmation,
        expires_at: Instant,
    },
    OfferIncoming {
        device_id: DeviceId,
        line_instance: LineInstance,
        call_id: CallId,
        info: CallInfo,
        ringer: Option<IncomingRing>,
    },
    Reconfigure {
        definitions: HashMap<DeviceId, DeviceDefinition>,
        affected: HashSet<DeviceId>,
        applied: oneshot::Sender<ReconfigureResult>,
    },
    ReconfigureStationPolicy {
        definitions: HashMap<DeviceId, DeviceDefinition>,
        affected: HashSet<DeviceId>,
        anonymous_hotline: Option<AnonymousHotlineDefinition>,
        applied: oneshot::Sender<ReconfigureResult>,
    },
    ReconfigureAnonymousHotline {
        definition: Option<AnonymousHotlineDefinition>,
        applied: oneshot::Sender<usize>,
    },
    Shutdown,
}

#[derive(Debug)]
enum AnonymousHotlineUpdate {
    Preserve,
    Replace(Option<AnonymousHotlineDefinition>),
}

#[derive(Debug)]
enum SessionCommand {
    Public(Box<Command>),
    Confirmed {
        command: Box<Command>,
        written: CommandWriteConfirmation,
        expires_at: Instant,
    },
    OfferIncoming {
        line_instance: LineInstance,
        call_id: CallId,
        info: Box<CallInfo>,
        ringer: Option<IncomingRing>,
    },
    Disconnect,
}

#[derive(Clone, Debug)]
struct SessionCall {
    call_id: CallId,
    wire_reference: u32,
    line_instance: u32,
    media: CallMedia,
    video_receive: VideoReceive,
    video_transmit: VideoTransmit,
    state: CallState,
    history_disposition: CallHistoryDisposition,
    dialed_number: String,
    statistics_directory_number: String,
    transfer_role: Option<SessionTransferRole>,
}

#[derive(Clone, Debug, Default)]
struct VideoReceive {
    generation: u64,
    leg: Option<VideoReceiveLeg>,
}

#[derive(Clone, Debug)]
struct VideoReceiveLeg {
    request: MediaRequestIdentity,
    conference_id: ConferenceId,
    codec: Codec,
    requested_address_type: IpAddressType,
    state: MediaChannelState,
    deadline: Option<Instant>,
}

#[derive(Debug)]
struct ExpiredVideoReceive {
    call_id: CallId,
    codec: Codec,
    passthrough_party_id: PassthroughPartyId,
    close: ServerMessage,
}

#[derive(Clone, Debug, Default)]
struct VideoTransmit {
    generation: u64,
    leg: Option<VideoTransmitLeg>,
}

#[derive(Clone, Debug)]
struct VideoTransmitLeg {
    request: MediaRequestIdentity,
    conference_id: ConferenceId,
    codec: Codec,
    address_type: IpAddressType,
    state: MediaChannelState,
    deadline: Option<Instant>,
}

#[derive(Debug)]
struct ExpiredVideoTransmit {
    call_id: CallId,
    codec: Codec,
    passthrough_party_id: PassthroughPartyId,
    stop: ServerMessage,
}

#[derive(Clone, Debug)]
struct CallMedia {
    generation: u64,
    codec: Codec,
    packet_ms: u32,
    max_frames_per_packet: u32,
    receive: MediaLeg,
    transmit: MediaLeg,
    /// Exact StartMediaTransmission endpoint paired with an outstanding
    /// OpenReceiveChannel in one outbound NAT compatibility transaction.
    /// A successful matching receive acknowledgement settles both halves.
    coupled_transmit_endpoint: Option<MediaEndpoint>,
    requested: bool,
}

impl CallMedia {
    fn new(codec: Codec) -> Self {
        Self {
            generation: 0,
            codec,
            packet_ms: DEFAULT_AUDIO_PACKET_MS,
            max_frames_per_packet: DEFAULT_AUDIO_MAX_FRAMES_PER_PACKET,
            receive: MediaLeg::default(),
            transmit: MediaLeg::default(),
            coupled_transmit_endpoint: None,
            requested: false,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct MediaLeg {
    request: Option<MediaRequestIdentity>,
    telephone_event_payload: u8,
    peer: Option<MediaEndpoint>,
    state: MediaChannelState,
    deadline: Option<Instant>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionTransferRole {
    Source { consultation_call_id: CallId },
    Consultation { source_call_id: CallId },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum MediaChannelState {
    #[default]
    Closed,
    Opening,
    Open,
}

impl MediaChannelState {
    const fn is_open(self) -> bool {
        matches!(self, Self::Open)
    }
}

fn validate_server_config(config: &ServerConfig) -> Result<(), ServerError> {
    if digit_character(config.dial_terminator).is_none() {
        return Err(ServerError::InvalidConfig(
            "dial terminator must be one DTMF character".into(),
        ));
    }
    if !(-840..=840).contains(&config.timezone_offset_minutes) {
        return Err(ServerError::InvalidConfig(
            "timezone offset must be between -840 and 840 minutes".into(),
        ));
    }
    if config.keepalive_seconds < 5 || config.secondary_keepalive_seconds < 5 {
        return Err(ServerError::InvalidConfig(
            "primary and secondary keepalive intervals must be at least 5 seconds".into(),
        ));
    }
    if config.advertised_address.is_unspecified()
        || config.advertised_address.is_multicast()
        || config
            .advertised_ipv6_address
            .is_some_and(|address| address.is_unspecified() || address.is_multicast())
    {
        return Err(ServerError::InvalidConfig(
            "advertised fallback addresses must be unicast".into(),
        ));
    }
    if !(MIN_REGISTRATION_BACKOFF..=MAX_REGISTRATION_BACKOFF)
        .contains(&config.registration_tokens.backoff)
    {
        return Err(ServerError::InvalidConfig(
            "registration-token backoff must be between 30 and 86400 seconds".into(),
        ));
    }
    if config.registration_tokens.server_priority == 0 {
        return Err(ServerError::InvalidConfig(
            "server priority must be nonzero".into(),
        ));
    }
    if config.signaling_servers.len() > crate::message::MAX_SIGNALING_SERVERS {
        return Err(ServerError::InvalidConfig(format!(
            "at most {} signaling servers may be advertised",
            crate::message::MAX_SIGNALING_SERVERS
        )));
    }
    let mut priorities = HashSet::new();
    for server in &config.signaling_servers {
        if server.priority == 0 || !priorities.insert(server.priority) {
            return Err(ServerError::InvalidConfig(
                "signaling server priorities must be nonzero and unique".into(),
            ));
        }
        if server.name.is_empty()
            || server.name.len() >= 48
            || server.name.chars().any(char::is_control)
            || server.address.is_unspecified()
            || server.address.is_multicast()
            || server.clear_port.is_none() && server.secure_port.is_none()
        {
            return Err(ServerError::InvalidConfig(
                "each signaling server requires a name, unicast address, and at least one port"
                    .into(),
            ));
        }
    }
    if !config.signaling_servers.is_empty()
        && !priorities.contains(&config.registration_tokens.server_priority)
    {
        return Err(ServerError::InvalidConfig(
            "the local server priority must occur in the advertised server list".into(),
        ));
    }
    config
        .signaling_qos
        .validate()
        .map_err(|error| ServerError::InvalidConfig(error.to_string()))
}

impl Server {
    /// Bind the configured plain TCP endpoint and construct a server.
    ///
    /// The returned tuple contains the inert server, its cloneable command
    /// handle, and the sole event receiver. Call [`Self::local_addr`] after
    /// construction when `config.bind` used port zero, then spawn or await
    /// [`Self::run`]. This constructor classifies every accepted connection as
    /// [`StationTransport::Clear`]. Use [`Self::with_ingress`] when transport
    /// negotiation or multiple listeners are owned elsewhere.
    pub async fn bind(
        config: ServerConfig,
        definitions: impl IntoIterator<Item = DeviceDefinition>,
    ) -> Result<(Self, ServerHandle, mpsc::Receiver<Event>), ServerError> {
        validate_server_config(&config)?;
        let listener = TcpListener::bind(config.bind)
            .await
            .map_err(ServerError::Bind)?;
        if let Ok(local) = listener.local_addr() {
            match SignalingSocket::capture(&listener, local) {
                Ok(socket) => report_socket_qos(None, local, socket.apply(config.signaling_qos)),
                Err(error) => {
                    warn!(%local, %error, "unable to retain signaling listener QoS control")
                }
            }
        }
        let (server, handle, events, _) = Self::build(config, definitions, Some(listener))?;
        Ok((server, handle, events))
    }

    /// Construct a server whose ready station streams are supplied externally.
    ///
    /// The additional [`ServerIngress`] value is cloned by clear and secure
    /// listener tasks. Each task completes its transport setup, preserves the
    /// accepted peer and local socket addresses, and submits the stream with an
    /// accurate [`StationTransport`] classification. The returned server has no
    /// bound listener, so [`Self::local_addr`] is unavailable.
    pub fn with_ingress(
        config: ServerConfig,
        definitions: impl IntoIterator<Item = DeviceDefinition>,
    ) -> Result<(Self, ServerHandle, mpsc::Receiver<Event>, ServerIngress), ServerError> {
        Self::build(config, definitions, None)
    }

    fn build(
        config: ServerConfig,
        definitions: impl IntoIterator<Item = DeviceDefinition>,
        listener: Option<TcpListener>,
    ) -> Result<(Self, ServerHandle, mpsc::Receiver<Event>, ServerIngress), ServerError> {
        validate_server_config(&config)?;
        let mut by_id = HashMap::new();
        for definition in definitions {
            definition.validate()?;
            by_id.insert(definition.id.clone(), definition);
        }
        let (event_tx, event_rx) = mpsc::channel(EVENT_CAPACITY);
        let (command_tx, command_rx) = mpsc::channel(COMMAND_CAPACITY);
        let (ingress, accepted_rx) =
            ServerIngress::channel(SESSION_ACCEPT_CAPACITY, config.signaling_qos);
        let next_call_id = Arc::new(AtomicU64::new(1));
        let latest_media_statistics = Arc::new(RwLock::new(HashMap::new()));
        let call_answer_order = Arc::new(RwLock::new(config.call_answer_order));
        let anonymous_hotline = Arc::new(RwLock::new(config.anonymous_hotline.clone()));
        let handle = ServerHandle {
            command_tx,
            next_call_id: Arc::clone(&next_call_id),
            latest_media_statistics: Arc::clone(&latest_media_statistics),
            call_answer_order: Arc::clone(&call_answer_order),
        };
        Ok((
            Self {
                listener,
                accepted_rx,
                config: Arc::new(config),
                anonymous_hotline,
                definitions: Arc::new(RwLock::new(by_id)),
                sessions: Arc::new(Mutex::new(HashMap::new())),
                event_tx,
                command_rx,
                next_generation: Arc::new(AtomicU64::new(1)),
                next_statistics_generation: Arc::new(AtomicU64::new(1)),
                next_call_id,
                latest_media_statistics,
                call_answer_order,
            },
            handle,
            event_rx,
            ingress,
        ))
    }

    /// Return the concrete address owned by [`Self::bind`].
    ///
    /// This exposes an operating-system-assigned port when the requested bind
    /// address used port zero. Servers created by [`Self::with_ingress`] return
    /// [`ServerError::InvalidConfig`] because listener addresses belong to the
    /// external transport owner.
    pub fn local_addr(&self) -> Result<SocketAddr, ServerError> {
        self.listener
            .as_ref()
            .ok_or_else(|| ServerError::InvalidConfig("server has no bound listener".into()))?
            .local_addr()
            .map_err(ServerError::Io)
    }

    /// Drive admission, command dispatch, reconfiguration, and shutdown.
    ///
    /// This consuming future must be polled exactly once. It accepts plain
    /// sockets owned by [`Self::bind`] and streams submitted through
    /// [`ServerIngress`], starts an independent session task for each, and
    /// serializes server-wide commands. It returns normally after an explicit
    /// shutdown request or after every [`ServerHandle`] is dropped; before
    /// returning it asks each registered session to disconnect. Listener or
    /// server-level I/O failures are returned as [`ServerError`], while an
    /// individual session failure is emitted as [`Event::SessionError`].
    pub async fn run(mut self) -> Result<(), ServerError> {
        if let Some(listener) = &self.listener {
            info!(bind = %listener.local_addr()?, "SCCP server listening");
        }
        loop {
            tokio::select! {
                accepted = accept_clear(self.listener.as_ref(), self.config.signaling_qos) => {
                    self.start_session(accepted?);
                }
                accepted = self.accepted_rx.recv(), if !self.accepted_rx.is_closed() => {
                    if let Some(accepted) = accepted {
                        self.start_session(accepted);
                    }
                }
                command = self.command_rx.recv() => {
                    match command {
                        Some(ServerCommand::Public(command)) => {
                            if let Err(error) = self.dispatch_public(*command).await {
                                warn!(%error, "discarding SCCP command for a retired session");
                            }
                        }
                        Some(ServerCommand::Confirmed { command, written, expires_at }) => {
                            self.dispatch_confirmed(command, written, expires_at).await;
                        }
                        Some(ServerCommand::OfferIncoming { device_id, line_instance, call_id, info, ringer }) => {
                            if let Err(error) = self.dispatch(&device_id, SessionCommand::OfferIncoming { line_instance, call_id, info: Box::new(info), ringer }).await {
                                warn!(%error, "discarding incoming offer for a retired session");
                            }
                        }
                        Some(ServerCommand::Reconfigure { definitions, affected, applied }) => {
                            let result = self
                                .apply_station_policy(
                                    definitions,
                                    affected,
                                    AnonymousHotlineUpdate::Preserve,
                                )
                                .await;
                            let _ = applied.send(result);
                        }
                        Some(ServerCommand::ReconfigureStationPolicy {
                            definitions,
                            affected,
                            anonymous_hotline,
                            applied,
                        }) => {
                            let result = self
                                .apply_station_policy(
                                    definitions,
                                    affected,
                                    AnonymousHotlineUpdate::Replace(anonymous_hotline),
                                )
                                .await;
                            let _ = applied.send(result);
                        }
                        Some(ServerCommand::ReconfigureAnonymousHotline { definition, applied }) => {
                            let sessions = self.sessions.lock().await;
                            let changed = {
                                let mut current = self
                                    .anonymous_hotline
                                    .write()
                                    .expect("SCCP anonymous-hotline lock poisoned");
                                if *current == definition {
                                    false
                                } else {
                                    *current = definition;
                                    true
                                }
                            };
                            let affected = if changed {
                                sessions
                                    .values()
                                    .filter(|session| session.anonymous_hotline)
                                    .cloned()
                                    .collect::<Vec<_>>()
                            } else {
                                Vec::new()
                            };
                            drop(sessions);
                            let count = affected.len();
                            for session in affected {
                                let _ = session.tx.send(SessionCommand::Disconnect).await;
                            }
                            let _ = applied.send(count);
                        }
                        Some(ServerCommand::Shutdown) | None => {
                            let sessions: Vec<_> = self.sessions.lock().await.values().cloned().collect();
                            for session in sessions { let _ = session.tx.send(SessionCommand::Disconnect).await; }
                            return Ok(());
                        }
                    }
                }
            }
        }
    }

    fn start_session(&self, accepted: AcceptedStation) {
        let AcceptedStation {
            stream,
            peer,
            local,
            transport,
            socket_qos,
        } = accepted;
        let context = SessionContext {
            peer,
            local,
            transport,
            socket_qos,
            config: Arc::clone(&self.config),
            definitions: Arc::clone(&self.definitions),
            anonymous_hotline: Arc::clone(&self.anonymous_hotline),
            sessions: Arc::clone(&self.sessions),
            event_tx: self.event_tx.clone(),
            next_generation: Arc::clone(&self.next_generation),
            next_statistics_generation: Arc::clone(&self.next_statistics_generation),
            next_call_id: Arc::clone(&self.next_call_id),
            latest_media_statistics: Arc::clone(&self.latest_media_statistics),
            call_answer_order: Arc::clone(&self.call_answer_order),
        };
        let error_tx = self.event_tx.clone();
        tokio::spawn(async move {
            match run_session(stream, context).await {
                Ok(()) => debug!(%peer, "SCCP session ended cleanly"),
                Err(error) => {
                    warn!(%peer, %error, "SCCP session ended with an error");
                    let _ = error_tx
                        .send(Event::SessionError {
                            peer,
                            error: error.to_string(),
                        })
                        .await;
                }
            }
        });
    }

    async fn dispatch_public(&self, command: Command) -> Result<(), ServerError> {
        let device_id = command.device_id.clone();
        self.dispatch(&device_id, SessionCommand::Public(Box::new(command)))
            .await
    }

    async fn dispatch_confirmed(
        &self,
        command: Box<Command>,
        written: CommandWriteConfirmation,
        expires_at: Instant,
    ) {
        let device_id = command.device_id.clone();
        if confirmed_command_expired(&written, expires_at) {
            reject_expired_confirmed_command(written);
            return;
        }
        let tx = self
            .sessions
            .lock()
            .await
            .get(&device_id)
            .map(|session| session.tx.clone());
        let Some(tx) = tx else {
            let _ = written.send(Err(ServerError::DeviceNotConnected(device_id).to_string()));
            return;
        };
        if let Err(error) = tx
            .send(SessionCommand::Confirmed {
                command,
                written,
                expires_at,
            })
            .await
        {
            let SessionCommand::Confirmed { written, .. } = error.0 else {
                unreachable!("confirmed dispatch returned a different command variant")
            };
            let _ = written.send(Err(ServerError::DeviceNotConnected(device_id).to_string()));
        }
    }

    async fn dispatch(
        &self,
        device_id: &DeviceId,
        command: SessionCommand,
    ) -> Result<(), ServerError> {
        let tx = self
            .sessions
            .lock()
            .await
            .get(device_id)
            .map(|s| s.tx.clone())
            .ok_or_else(|| ServerError::DeviceNotConnected(device_id.clone()))?;
        tx.send(command)
            .await
            .map_err(|_| ServerError::DeviceNotConnected(device_id.clone()))
    }

    async fn apply_station_policy(
        &self,
        definitions: HashMap<DeviceId, DeviceDefinition>,
        affected: HashSet<DeviceId>,
        anonymous_hotline: AnonymousHotlineUpdate,
    ) -> ReconfigureResult {
        // Registration takes the session and definition locks in the same
        // order, so it sees either the complete current policy or the complete
        // candidate policy.
        let sessions = self.sessions.lock().await;
        let result = {
            let mut current = self
                .definitions
                .write()
                .expect("SCCP definitions lock poisoned");
            let result = reconfigure_result(&current, &definitions, &affected);
            *current = definitions;
            result
        };
        let anonymous_changed = match anonymous_hotline {
            AnonymousHotlineUpdate::Preserve => false,
            AnonymousHotlineUpdate::Replace(next) => {
                let mut current = self
                    .anonymous_hotline
                    .write()
                    .expect("SCCP anonymous-hotline lock poisoned");
                if *current == next {
                    false
                } else {
                    *current = next;
                    true
                }
            }
        };
        let affected_devices = result
            .disconnected_devices()
            .chain(affected.iter())
            .cloned()
            .collect::<HashSet<_>>();
        let affected_sessions = sessions
            .iter()
            .filter(|(device, session)| {
                affected_devices.contains(*device)
                    || (anonymous_changed && session.anonymous_hotline)
            })
            .map(|(_, session)| session.clone())
            .collect::<Vec<_>>();
        drop(sessions);
        for session in affected_sessions {
            let _ = session.tx.send(SessionCommand::Disconnect).await;
        }
        result
    }
}

fn confirmed_command_expired(written: &CommandWriteConfirmation, expires_at: Instant) -> bool {
    written.is_closed() || Instant::now() >= expires_at
}

fn reject_expired_confirmed_command(written: CommandWriteConfirmation) {
    let _ = written.send(Err(ServerError::CommandAcknowledgementTimeout.to_string()));
}

fn prepare_session_command(
    command: SessionCommand,
) -> Option<(SessionCommand, Option<CommandWriteConfirmation>)> {
    match command {
        SessionCommand::Confirmed {
            command,
            written,
            expires_at,
        } => {
            if confirmed_command_expired(&written, expires_at) {
                reject_expired_confirmed_command(written);
                None
            } else {
                Some((SessionCommand::Public(command), Some(written)))
            }
        }
        command => Some((command, None)),
    }
}

fn reconfigure_result(
    current: &HashMap<DeviceId, DeviceDefinition>,
    next: &HashMap<DeviceId, DeviceDefinition>,
    affected: &HashSet<DeviceId>,
) -> ReconfigureResult {
    let mut result = ReconfigureResult::default();
    for (device, definition) in next {
        match current.get(device) {
            None => result.added.push(device.clone()),
            Some(previous) if previous != definition => result.changed.push(device.clone()),
            Some(_) => {}
        }
    }
    let explicitly_changed: Vec<_> = affected
        .iter()
        .filter(|device| {
            current.contains_key(*device)
                && next.contains_key(*device)
                && !result.changed.contains(*device)
        })
        .cloned()
        .collect();
    result.changed.extend(explicitly_changed);
    result.removed.extend(
        current
            .keys()
            .filter(|device| !next.contains_key(*device))
            .cloned(),
    );
    result.added.sort();
    result.changed.sort();
    result.removed.sort();
    result
}

fn command_call_id(command: &Command) -> Option<CallId> {
    match &command.action {
        CommandAction::BeginCall { call_id, .. }
        | CommandAction::SetCallInfo { call_id, .. }
        | CommandAction::CommitOutboundCall { call_id, .. }
        | CommandAction::PresentOutboundProceeding { call_id, .. }
        | CommandAction::PresentOutboundRinging { call_id, .. }
        | CommandAction::SetCallState { call_id, .. }
        | CommandAction::SetCallSelected { call_id, .. }
        | CommandAction::DisplayPrompt { call_id, .. }
        | CommandAction::ClearPrompt { call_id, .. }
        | CommandAction::SetRecordingStatus { call_id, .. }
        | CommandAction::ShowConferenceParticipantActions { call_id, .. }
        | CommandAction::StartTone { call_id, .. }
        | CommandAction::StartRinging { call_id, .. }
        | CommandAction::StopRinging { call_id, .. }
        | CommandAction::OpenReceiveChannel { call_id, .. }
        | CommandAction::OpenMultimediaReceiveChannel { call_id, .. }
        | CommandAction::CloseMultimediaReceiveChannel { call_id, .. }
        | CommandAction::StartMultimediaTransmission { call_id, .. }
        | CommandAction::StopMultimediaTransmission { call_id, .. }
        | CommandAction::SetMultimediaTransmitBitRate { call_id, .. }
        | CommandAction::NotifyMultimediaTransmitBitRate { call_id, .. }
        | CommandAction::ControlMultimediaTransmission { call_id, .. }
        | CommandAction::OpenOutboundMedia { call_id, .. }
        | CommandAction::CloseReceiveChannel { call_id, .. }
        | CommandAction::StartMedia { call_id, .. }
        | CommandAction::StartMulticastReception { call_id, .. }
        | CommandAction::StopMulticastReception { call_id, .. }
        | CommandAction::StartMulticastTransmission { call_id, .. }
        | CommandAction::StopMulticastTransmission { call_id, .. }
        | CommandAction::StopMedia { call_id, .. }
        | CommandAction::CloseCall { call_id, .. } => Some(*call_id),
        CommandAction::BeginTransfer { source_call_id, .. } => Some(*source_call_id),
        CommandAction::SetMwi { .. }
        | CommandAction::SetStatusMessage { .. }
        | CommandAction::SetMicrophoneMode { .. }
        | CommandAction::ResetDevice { .. }
        | CommandAction::SetForwardStatus { .. }
        | CommandAction::SetFeatureStatus { .. }
        | CommandAction::SetDoNotDisturbStatus { .. }
        | CommandAction::SetMobilityAppearance { .. }
        | CommandAction::SetBlfStatus { .. }
        | CommandAction::ShowParkingMenu { .. }
        | CommandAction::ShowConferenceList { .. }
        | CommandAction::ShowTextService { .. }
        | CommandAction::ShowInputService { .. }
        | CommandAction::ExecutePhoneActions { .. }
        | CommandAction::ShowImageService { .. }
        | CommandAction::ShowStatusService { .. }
        | CommandAction::SetBackgroundImage { .. }
        | CommandAction::PreviewBackgroundImage { .. }
        | CommandAction::SetRingtone { .. }
        | CommandAction::StartAnnouncement { .. }
        | CommandAction::StopAnnouncement { .. }
        | CommandAction::AnnouncementFinish { .. }
        | CommandAction::DisconnectDevice { .. } => None,
    }
}

async fn accept_clear(
    listener: Option<&TcpListener>,
    signaling_qos: SignalingQos,
) -> Result<AcceptedStation, ServerError> {
    let Some(listener) = listener else {
        return std::future::pending().await;
    };
    let (stream, peer) = listener.accept().await?;
    stream.set_nodelay(true)?;
    let local = stream.local_addr()?;
    let socket_qos = match SignalingSocket::capture(&stream, local) {
        Ok(socket) => {
            report_socket_qos(None, peer, socket.apply(signaling_qos));
            Some(Box::new(socket) as Box<dyn StationSocketQos>)
        }
        Err(error) => {
            warn!(%peer, %error, "unable to retain signaling socket QoS control");
            None
        }
    };
    Ok(AcceptedStation {
        stream: Box::new(stream),
        peer,
        local,
        transport: StationTransport::Clear,
        socket_qos,
    })
}

fn report_socket_qos(device_id: Option<&DeviceId>, endpoint: SocketAddr, report: SocketQosReport) {
    for failure in report.failures() {
        match device_id {
            Some(device_id) => {
                warn!(%device_id, %endpoint, %failure, "signaling socket marking unavailable")
            }
            None => warn!(%endpoint, %failure, "signaling socket marking unavailable"),
        }
    }
}

fn allocate_session_generation(
    next_generation: &AtomicU64,
) -> Result<SessionGeneration, ServerError> {
    let generation = next_generation
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            SessionGeneration::new(current).and_then(|_| current.checked_add(1))
        })
        .map_err(|_| ServerError::SessionGenerationExhausted)?;
    SessionGeneration::new(generation).ok_or(ServerError::SessionGenerationExhausted)
}

const fn transport_allowed(
    requirement: StationTransportRequirement,
    transport: StationTransport,
) -> bool {
    matches!(
        (requirement, transport),
        (StationTransportRequirement::Either, _)
            | (StationTransportRequirement::Clear, StationTransport::Clear)
            | (
                StationTransportRequirement::Secure,
                StationTransport::Secure
            )
    )
}

#[derive(Debug)]
struct SessionContext {
    peer: SocketAddr,
    local: SocketAddr,
    transport: StationTransport,
    socket_qos: Option<Box<dyn StationSocketQos>>,
    config: Arc<ServerConfig>,
    definitions: Arc<RwLock<HashMap<DeviceId, DeviceDefinition>>>,
    anonymous_hotline: Arc<RwLock<Option<AnonymousHotlineDefinition>>>,
    sessions: Sessions,
    event_tx: mpsc::Sender<Event>,
    next_generation: Arc<AtomicU64>,
    next_statistics_generation: Arc<AtomicU64>,
    next_call_id: Arc<AtomicU64>,
    latest_media_statistics: Arc<RwLock<HashMap<DeviceId, MediaStatisticsSnapshot>>>,
    call_answer_order: Arc<RwLock<CallSelectionOrder>>,
}

#[derive(Debug)]
struct SessionState {
    device: DeviceDefinition,
    registration: DeviceRegistration,
    features: PhoneFeatures,
    generation: SessionGeneration,
    calls_by_id: HashMap<CallId, SessionCall>,
    calls_by_wire: HashMap<u32, CallId>,
    media_capabilities: StationMediaCapabilities,
    next_media_token: Option<MediaRequestToken>,
    next_multicast_generation: u64,
    multicast: HashMap<MulticastKey, MulticastSession>,
    pending_connection_statistics: HashMap<u32, PendingConnectionStatistics>,
    statistics_references: HashSet<u32>,
    cancelled_calls: HashSet<CallId>,
    last_number_by_line: HashMap<u32, String>,
    forwarding_by_line: HashMap<u32, SessionForwarding>,
    feature_states: HashMap<u32, SessionFeatureState>,
    mwi_by_line: HashMap<u32, bool>,
    mobility_appearances: HashMap<u32, LineAppearance>,
    active_key_mode: KeyMode,
    active_call_id: Option<CallId>,
    pending_parking_menu: Option<PendingParkingMenu>,
    persistent_status_message: bool,
    headset_enabled: bool,
    media_path_states:
        HashMap<crate::message::values::MediaPathId, crate::message::values::MediaPathEvent>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct MulticastKey {
    conference_id: ConferenceId,
    call_id: CallId,
}

#[derive(Clone, Debug)]
struct MulticastSession {
    wire_call_reference: u32,
    receive: Option<MulticastReceive>,
    transmit: Option<MulticastTransmit>,
}

#[derive(Clone, Debug)]
struct MulticastReceive {
    request: MediaRequestIdentity,
    route: MulticastMediaRoute,
    state: MulticastReceiveState,
}

#[derive(Clone, Debug)]
enum MulticastReceiveState {
    AwaitingAcknowledgement { deadline: Instant },
    Open,
}

#[derive(Clone, Debug)]
struct MulticastTransmit {
    request: MediaRequestIdentity,
    route: MulticastMediaRoute,
}

impl SessionState {
    fn station_context(&self) -> StationSessionContext {
        StationSessionContext::new(self.registration.protocol, self.features)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SessionFeatureState {
    button_type: ButtonType,
    state: u32,
}

#[derive(Clone, Debug)]
struct PendingConnectionStatistics {
    session_generation: SessionGeneration,
    request_generation: u64,
    call_id: CallId,
    line_instance: u32,
    codec: Codec,
    packet_ms: u32,
    max_frames_per_packet: u32,
    receive_peer: Option<MediaEndpoint>,
    transmit_peer: Option<MediaEndpoint>,
    directory_number: String,
    processing: StatisticsProcessing,
    expires_at: Instant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingParkingMenu {
    instance: u32,
    transaction_id: u32,
}

#[derive(Clone, Debug, Default)]
struct SessionForwarding {
    all: Option<String>,
    busy: Option<String>,
    no_answer: Option<String>,
}

async fn run_session(
    mut stream: Box<dyn StationIo>,
    context: SessionContext,
) -> Result<(), ServerError> {
    let (session_tx, mut session_rx) = mpsc::channel(SESSION_COMMAND_CAPACITY);
    let mut decoder = FrameDecoder::new();
    let mut read_buffer = [0_u8; 4096];
    let mut state: Option<SessionState> = None;
    let mut last_keepalive = Instant::now();
    let mut session_deadlines = tokio::time::interval(Duration::from_millis(100));
    session_deadlines.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let keepalive_seconds = if context.config.registration_tokens.server_priority == 1 {
        context.config.keepalive_seconds
    } else {
        context.config.secondary_keepalive_seconds
    };
    let keepalive_timeout = Duration::from_secs(u64::from(keepalive_seconds) * 3);

    loop {
        tokio::select! {
            read = stream.read(&mut read_buffer) => {
                let count = read?;
                if count == 0 { break; }
                for frame in decoder.push(&read_buffer[..count])? {
                    let decode_protocol = state
                        .as_ref()
                        .map_or(ProtocolVersion::V3, |state| state.registration.protocol);
                    let message_id = frame.message_id;
                    let message = match ClientMessage::decode_with_version(frame, decode_protocol) {
                        Ok(message) => message,
                        Err(error) if message_id != crate::message::id::REGISTER => {
                            let device_id = state.as_ref().map(|state| state.device.id.clone());
                            warn!(peer = %context.peer, message_id = format_args!("0x{message_id:04x}"), %error, "ignoring malformed SCCP application message");
                            let _ = context.event_tx.send(Event::ProtocolWarning {
                                peer: context.peer,
                                device_id,
                                message_id,
                                error: error.to_string(),
                            }).await;
                            continue;
                        }
                        Err(error) => return Err(error.into()),
                    };
                    if let ClientMessage::Register(registration) = &message {
                        if state.is_some() {
                            return Err(ServerError::Protocol(CodecError::InvalidDefinition("duplicate REGISTER on one TCP session".into())));
                        }
                        let mut sessions = context.sessions.lock().await;
                        let configured = context
                            .definitions
                            .read()
                            .expect("SCCP definitions lock poisoned")
                            .get(&registration.device_id)
                            .cloned();
                        let anonymous_hotline = configured.is_none();
                        let definition = configured.or_else(|| {
                                context
                                    .anonymous_hotline
                                    .read()
                                    .expect("SCCP anonymous-hotline lock poisoned")
                                    .as_ref()
                                    .map(|hotline| {
                                    hotline.device_definition(registration.device_id.clone())
                                })
                            });
                        let Some(definition) = definition else {
                            drop(sessions);
                            send_message(&mut stream, &ServerMessage::RegisterReject { reason: "Device not configured".into() }, ProtocolVersion::V17).await?;
                            return Ok(());
                        };
                        if !transport_allowed(definition.transport, context.transport) {
                            drop(sessions);
                            send_message(
                                &mut stream,
                                &ServerMessage::RegisterReject {
                                    reason: "Device transport not permitted".into(),
                                },
                                ProtocolVersion::V17,
                            )
                            .await?;
                            return Ok(());
                        }
                        let protocol = ProtocolVersion::negotiate(registration.advertised_protocol)?;
                        if canonical_ip_address(context.peer.ip()).is_ipv6()
                            && protocol < ProtocolVersion::V17
                        {
                            drop(sessions);
                            send_message(
                                &mut stream,
                                &ServerMessage::RegisterReject {
                                    reason: "IPv6 requires protocol v17".into(),
                                },
                                protocol,
                            )
                            .await?;
                            return Ok(());
                        }
                        let features = registration.features;
                        let generation = allocate_session_generation(&context.next_generation)?;
                        if let Some(socket_qos) = &context.socket_qos {
                            let signaling_qos = definition
                                .signaling_qos
                                .unwrap_or(context.config.signaling_qos);
                            report_socket_qos(
                                Some(&registration.device_id),
                                context.peer,
                                socket_qos.apply(signaling_qos),
                            );
                        }
                        let device_registration = DeviceRegistration {
                            id: registration.device_id.clone(), peer: context.peer,
                            transport: context.transport,
                            reported_address: registration.reported_address,
                            reported_ipv6_address: registration.reported_ipv6_address,
                            device_type: registration.device_type,
                            protocol, firmware: registration.firmware.clone(),
                        };
                        let previous = sessions.insert(
                            registration.device_id.clone(), SessionSender {
                                generation,
                                anonymous_hotline,
                                tx: session_tx.clone(),
                            }
                        );
                        drop(sessions);
                        if let Some(previous) = previous { let _ = previous.tx.send(SessionCommand::Disconnect).await; }
                        send_message(
                            &mut stream,
                            &ServerMessage::RegisterAck {
                                keepalive_seconds: context.config.keepalive_seconds,
                                secondary_keepalive_seconds: context.config.secondary_keepalive_seconds,
                                protocol,
                                features: PhoneFeatures::empty(),
                                date_template: context.config.date_template.clone(),
                            },
                            protocol,
                        )
                        .await?;
                        send_message(&mut stream, &ServerMessage::CapabilitiesRequest, protocol).await?;
                        context.event_tx.send(Event::device(
                            registration.device_id.clone(),
                            generation,
                            DeviceEventKind::Registered(device_registration.clone()),
                        )).await.map_err(|_| ServerError::Stopped)?;
                        info!(device_id = %registration.device_id, %protocol, peer = %context.peer, "SCCP device registered");
                        state = Some(SessionState { device: definition, registration: device_registration, features, generation, calls_by_id: HashMap::new(), calls_by_wire: HashMap::new(), media_capabilities: StationMediaCapabilities::default(), next_media_token: MediaRequestToken::new(1), next_multicast_generation: 0, multicast: HashMap::new(), pending_connection_statistics: HashMap::new(), statistics_references: HashSet::new(), cancelled_calls: HashSet::new(), last_number_by_line: HashMap::new(), forwarding_by_line: HashMap::new(), feature_states: HashMap::new(), mwi_by_line: HashMap::new(), mobility_appearances: HashMap::new(), active_key_mode: KeyMode::OnHook, active_call_id: None, pending_parking_menu: None, persistent_status_message: false, headset_enabled: false, media_path_states: HashMap::new() });
                        last_keepalive = Instant::now();
                    } else if let Some(state) = state.as_mut() {
                        if matches!(message, ClientMessage::KeepAlive) { last_keepalive = Instant::now(); }
                        handle_client_message(&mut stream, state, message, &context).await?;
                    } else {
                        handle_pre_registration_message(&mut stream, message, &context).await?;
                    }
                }
            }
            command = session_rx.recv() => {
                match command {
                    Some(command) => {
                        let Some(state) = state.as_mut() else { continue };
                        let Some((command, written)) = prepare_session_command(command) else {
                            continue;
                        };
                        match handle_session_command(&mut stream, state, command, &context).await {
                            Ok(disconnect) => {
                                if let Some(written) = written {
                                    let _ = written.send(Ok(()));
                                }
                                if disconnect { break; }
                            }
                            Err(error) => {
                                if let Some(written) = written {
                                    let _ = written.send(Err(error.to_string()));
                                }
                                if error.is_nonfatal_command_rejection() {
                                    warn!(
                                        device_id = %state.device.id,
                                        %error,
                                        "rejected invalid SCCP station command"
                                    );
                                    continue;
                                }
                                return Err(error);
                            }
                        }
                    }
                    None => break,
                }
            }
            _ = session_deadlines.tick(), if state.is_some() => {
                if let Some(state) = state.as_mut() {
                    let now = Instant::now();
                    let timed_out = expire_handset_acknowledgements(
                        &mut state.calls_by_id,
                        now,
                    );
                    for (call_id, acknowledgement) in timed_out {
                        context
                            .event_tx
                            .send(Event::device(
                                state.device.id.clone(),
                                state.generation,
                                DeviceEventKind::HandsetAcknowledgementTimedOut {
                                call_id,
                                acknowledgement,
                            }))
                            .await
                            .map_err(|_| ServerError::Stopped)?;
                    }
                    for (key, stop) in expire_multicast_reception_acknowledgements(state, now) {
                        send_message(&mut stream, &stop, state.registration.protocol).await?;
                        context
                            .event_tx
                            .send(Event::device(
                                state.device.id.clone(),
                                state.generation,
                                DeviceEventKind::MulticastReceptionTimedOut {
                                    conference_id: key.conference_id,
                                    call_id: key.call_id,
                                },
                            ))
                            .await
                            .map_err(|_| ServerError::Stopped)?;
                    }
                    for expired in expire_multimedia_receive_acknowledgements(state, now) {
                        send_message(
                            &mut stream,
                            &expired.close,
                            state.registration.protocol,
                        )
                        .await?;
                        context
                            .event_tx
                            .send(Event::device(
                                state.device.id.clone(),
                                state.generation,
                                DeviceEventKind::MultimediaReceiveChannelTimedOut {
                                    call_id: expired.call_id,
                                    codec: expired.codec,
                                    passthrough_party_id: expired.passthrough_party_id,
                                },
                            ))
                            .await
                            .map_err(|_| ServerError::Stopped)?;
                    }
                    for expired in expire_multimedia_transmit_acknowledgements(state, now) {
                        send_message(
                            &mut stream,
                            &expired.stop,
                            state.registration.protocol,
                        )
                        .await?;
                        context
                            .event_tx
                            .send(Event::device(
                                state.device.id.clone(),
                                state.generation,
                                DeviceEventKind::MultimediaTransmitTimedOut {
                                    call_id: expired.call_id,
                                    codec: expired.codec,
                                    passthrough_party_id: expired.passthrough_party_id,
                                },
                            ))
                            .await
                            .map_err(|_| ServerError::Stopped)?;
                    }
                    prune_connection_statistics(
                        &mut state.pending_connection_statistics,
                        Instant::now(),
                    );
                }
            }
            _ = tokio::time::sleep_until(last_keepalive + keepalive_timeout), if state.is_some() => {
                warn!(peer = %context.peer, "SCCP keepalive timeout");
                break;
            }
        }
    }

    if let Some(mut state) = state {
        drain_session_media(&mut stream, &mut state).await;
        let mut sessions = context.sessions.lock().await;
        let was_current = sessions
            .get(&state.device.id)
            .is_some_and(|entry| entry.generation == state.generation);
        if was_current {
            sessions.remove(&state.device.id);
        }
        drop(sessions);
        if was_current {
            let _ = context
                .event_tx
                .send(Event::device(
                    state.device.id,
                    state.generation,
                    DeviceEventKind::Disconnected {},
                ))
                .await;
        }
    }
    Ok(())
}

fn expire_handset_acknowledgements(
    calls_by_id: &mut HashMap<CallId, SessionCall>,
    now: Instant,
) -> Vec<(CallId, HandsetAcknowledgement)> {
    let mut calls = calls_by_id.keys().copied().collect::<Vec<_>>();
    calls.sort_unstable_by_key(|call_id| call_id.0);
    let mut expired = Vec::new();
    for call_id in calls {
        let call = calls_by_id
            .get_mut(&call_id)
            .expect("call identifier came from session state");
        if call.media.receive.state == MediaChannelState::Opening
            && call
                .media
                .receive
                .deadline
                .is_some_and(|deadline| deadline <= now)
        {
            call.media.receive.state = MediaChannelState::Closed;
            call.media.receive.deadline = None;
            call.media.receive.peer = None;
            if call.media.coupled_transmit_endpoint.take().is_some() {
                call.media.transmit.state = MediaChannelState::Closed;
                call.media.transmit.deadline = None;
                call.media.transmit.peer = None;
            }
            expired.push((call_id, HandsetAcknowledgement::OpenReceiveChannel));
        }
        if call.media.transmit.state == MediaChannelState::Opening
            && call
                .media
                .transmit
                .deadline
                .is_some_and(|deadline| deadline <= now)
        {
            call.media.transmit.state = MediaChannelState::Closed;
            call.media.transmit.deadline = None;
            call.media.transmit.peer = None;
            expired.push((call_id, HandsetAcknowledgement::StartMediaTransmission));
        }
    }
    expired
}

async fn handle_pre_registration_message(
    stream: &mut dyn StationIo,
    message: ClientMessage,
    context: &SessionContext,
) -> Result<(), ServerError> {
    match message {
        ClientMessage::KeepAlive => {
            send_message(stream, &ServerMessage::KeepAliveAck, ProtocolVersion::V3).await?;
        }
        ClientMessage::RegisterToken(token) => {
            let definition = context
                .definitions
                .read()
                .expect("SCCP definitions lock poisoned")
                .get(&token.device_id)
                .cloned();
            let configured = definition.is_some()
                || context
                    .anonymous_hotline
                    .read()
                    .expect("SCCP anonymous-hotline lock poisoned")
                    .is_some();
            let transport_permitted = definition.as_ref().is_none_or(|definition| {
                transport_allowed(definition.transport, context.transport)
            });
            let already_registered = context.sessions.lock().await.contains_key(&token.device_id);
            let accept = configured
                && transport_permitted
                && !already_registered
                && context.config.registration_tokens.accepts(&token.device_id);
            let response = if accept {
                ServerMessage::RegisterTokenAck
            } else {
                ServerMessage::RegisterTokenReject {
                    backoff_seconds: u32::try_from(
                        context.config.registration_tokens.backoff.as_secs(),
                    )
                    .unwrap_or(u32::MAX),
                }
            };
            send_message(stream, &response, ProtocolVersion::V17).await?;
        }
        ClientMessage::Alarm {
            severity,
            text,
            parameters,
        } => {
            debug!(peer = %context.peer, ?severity, %text, ?parameters, "pre-registration SCCP alarm");
        }
        ClientMessage::XmlAlarm(message) => match parse_phone_alarm(message.xml_bytes()) {
            Ok(telemetry) => {
                debug!(
                    peer = %context.peer,
                    payload_len = message.xml_bytes().len(),
                    summary = ?telemetry.summary(),
                    opaque = telemetry.is_opaque(),
                    "pre-registration SCCP XML alarm"
                );
            }
            Err(error) => {
                warn!(
                    peer = %context.peer,
                    payload_len = message.xml_bytes().len(),
                    %error,
                    "rejected pre-registration SCCP XML alarm"
                );
            }
        },
        ClientMessage::LocationInfo { xml } => match parse_phone_location(xml.as_bytes()) {
            Ok(telemetry) => {
                debug!(
                    peer = %context.peer,
                    payload_len = xml.len(),
                    summary = ?telemetry.summary(),
                    opaque = telemetry.is_opaque(),
                    "pre-registration SCCP location information"
                );
            }
            Err(error) => {
                warn!(
                    peer = %context.peer,
                    payload_len = xml.len(),
                    %error,
                    "rejected pre-registration SCCP location information"
                );
            }
        },
        ClientMessage::KnownOpaque(message) => {
            debug!(peer = %context.peer, message = ?message, "pre-registration deferred SCCP message");
        }
        ClientMessage::Unknown(message) => {
            warn!(peer = %context.peer, message = ?message, "pre-registration unknown SCCP message");
        }
        ClientMessage::Register(_)
        | ClientMessage::IpPort { .. }
        | ClientMessage::KeypadButton { .. }
        | ClientMessage::EnblocCall { .. }
        | ClientMessage::Stimulus { .. }
        | ClientMessage::OffHook { .. }
        | ClientMessage::OnHook { .. }
        | ClientMessage::OffHookWithCallingParty { .. }
        | ClientMessage::LineStatRequest { .. }
        | ClientMessage::ConfigStatRequest
        | ClientMessage::TimeDateRequest
        | ClientMessage::ButtonTemplateRequest
        | ClientMessage::VersionRequest
        | ClientMessage::CapabilitiesResponse(_)
        | ClientMessage::CapabilitiesUpdate(_)
        | ClientMessage::OpenMultimediaReceiveChannelAck(_)
        | ClientMessage::ServerRequest
        | ClientMessage::MulticastMediaReceptionAck { .. }
        | ClientMessage::OpenReceiveChannelAck { .. }
        | ClientMessage::SoftKeySetRequest
        | ClientMessage::SoftKeyTemplateRequest
        | ClientMessage::SoftKeyEvent { .. }
        | ClientMessage::Unregister { .. }
        | ClientMessage::HookFlash { .. }
        | ClientMessage::ForwardStatusRequest { .. }
        | ClientMessage::SpeedDialStatusRequest { .. }
        | ClientMessage::ConnectionStatisticsResponse(_)
        | ClientMessage::HeadsetStatus { .. }
        | ClientMessage::MediaResourceNotification(_)
        | ClientMessage::MediaPathEvent { .. }
        | ClientMessage::MediaPathCapability { .. }
        | ClientMessage::MediaTransmissionFailure { .. }
        | ClientMessage::RegisterAvailableLines { .. }
        | ClientMessage::ServiceUrlStatusRequest { .. }
        | ClientMessage::FeatureStatusRequest { .. }
        | ClientMessage::StartMediaTransmissionAck(_)
        | ClientMessage::StartMultimediaTransmissionAck(_)
        | ClientMessage::ExtensionDeviceCapabilities(_)
        | ClientMessage::DeviceToUserData(_)
        | ClientMessage::DeviceToUserDataResponse(_)
        | ClientMessage::DeviceToUserDataV1(_)
        | ClientMessage::DeviceToUserDataResponseV1(_)
        | ClientMessage::PortResponse(_)
        | ClientMessage::SubscriptionStatusRequest(_)
        | ClientMessage::SubscribeDtmfPayloadResponse(_)
        | ClientMessage::UnsubscribeDtmfPayloadResponse(_)
        | ClientMessage::CallCountRequest { .. }
        | ClientMessage::CreateConferenceResponse(_)
        | ClientMessage::DeleteConferenceResponse { .. }
        | ClientMessage::ModifyConferenceResponse(_)
        | ClientMessage::AuditConferenceResponse(_)
        | ClientMessage::AddParticipantResponse(_)
        | ClientMessage::AuditParticipantResponse(_) => {
            warn!(peer = %context.peer, message = ?message, "ignoring SCCP message before registration");
        }
    }
    Ok(())
}

async fn handle_client_message(
    stream: &mut dyn StationIo,
    state: &mut SessionState,
    message: ClientMessage,
    context: &SessionContext,
) -> Result<(), ServerError> {
    let protocol = state.registration.protocol;
    match message {
        ClientMessage::KeepAlive => {
            send_message(stream, &ServerMessage::KeepAliveAck, protocol).await?
        }
        ClientMessage::CapabilitiesResponse(capabilities) => {
            let capabilities = StationMediaCapabilities::from(capabilities);
            state.media_capabilities.clone_from(&capabilities);
            context
                .event_tx
                .send(Event::device(
                    state.device.id.clone(),
                    state.generation,
                    DeviceEventKind::Capabilities { capabilities },
                ))
                .await
                .map_err(|_| ServerError::Stopped)?;
        }
        ClientMessage::CapabilitiesUpdate(update) => {
            let capabilities = update.into_media_capabilities();
            state.media_capabilities.clone_from(&capabilities);
            context
                .event_tx
                .send(Event::device(
                    state.device.id.clone(),
                    state.generation,
                    DeviceEventKind::Capabilities { capabilities },
                ))
                .await
                .map_err(|_| ServerError::Stopped)?;
        }
        ClientMessage::ConfigStatRequest => {
            send_station_ui_message(
                stream,
                state,
                &ServerMessage::ConfigStatus(crate::message::ConfigurationStatus {
                    device_name: state.device.id.as_str().to_owned(),
                    station_user_id: 0,
                    station_instance: 1,
                    user_name: state.device.description.clone(),
                    server_name: context.config.server_name.clone(),
                    line_count: state.device.line_count() as u32,
                    speed_dial_count: 0,
                }),
            )
            .await?;
        }
        ClientMessage::LineStatRequest { line_instance } => {
            if let Some(message) = line_status(&state.device, line_instance) {
                send_station_ui_message(stream, state, &message).await?;
            }
        }
        ClientMessage::ButtonTemplateRequest => {
            send_message(
                stream,
                &ServerMessage::ButtonTemplate {
                    buttons: button_template(&state.device),
                },
                protocol,
            )
            .await?;
        }
        ClientMessage::VersionRequest => {
            send_message(
                stream,
                &ServerMessage::Version {
                    firmware: context.config.firmware_version.clone(),
                },
                protocol,
            )
            .await?;
        }
        ClientMessage::ServerRequest => {
            send_message(
                stream,
                &ServerMessage::ServerResponse {
                    servers: server_response_endpoints(context, protocol)?,
                },
                protocol,
            )
            .await?;
        }
        ClientMessage::TimeDateRequest => {
            send_message(
                stream,
                &time_date_message(context.config.timezone_offset_minutes),
                protocol,
            )
            .await?
        }
        ClientMessage::SoftKeyTemplateRequest => {
            send_message(
                stream,
                &ServerMessage::SoftKeyTemplate {
                    actions: state.device.soft_keys.template_actions(),
                },
                protocol,
            )
            .await?
        }
        ClientMessage::SoftKeySetRequest => {
            send_message(
                stream,
                &ServerMessage::SoftKeySet {
                    profile: state.device.soft_keys.clone(),
                },
                protocol,
            )
            .await?
        }
        ClientMessage::ForwardStatusRequest { line_instance } => {
            let forwarding = state
                .forwarding_by_line
                .get(&line_instance)
                .cloned()
                .unwrap_or_default();
            send_message(
                stream,
                &ServerMessage::ForwardStatus {
                    line_instance,
                    forward_all: forwarding.all,
                    forward_busy: forwarding.busy,
                    forward_no_answer: forwarding.no_answer,
                },
                protocol,
            )
            .await?;
        }
        ClientMessage::SpeedDialStatusRequest {
            speed_dial_instance,
        } => {
            send_station_ui_message(
                stream,
                state,
                &speed_dial_status(&state.device, speed_dial_instance),
            )
            .await?;
        }
        ClientMessage::FeatureStatusRequest {
            index,
            capabilities,
        } => {
            if let Some(mut message) = feature_status(&state.device, index, capabilities) {
                if let ServerMessage::FeatureStatus {
                    button_type,
                    state: feature_state,
                    ..
                } = &mut message
                    && let Some(cached) = state.feature_states.get(&index)
                {
                    *button_type = cached.button_type;
                    *feature_state = cached.state;
                }
                send_station_ui_message(stream, state, &message).await?;
            }
        }
        ClientMessage::ServiceUrlStatusRequest { index } => {
            if let Some(message) = service_url_status(&state.device, index) {
                send_station_ui_message(stream, state, &message).await?;
            }
        }
        ClientMessage::SubscriptionStatusRequest(request) => {
            send_message(
                stream,
                &ServerMessage::SubscriptionStatus {
                    transaction_id: request.transaction_id,
                    feature_id: request.feature_id,
                    timer_seconds: 0,
                    cause: SubscriptionCause::RouteFailure,
                },
                protocol,
            )
            .await?;
        }
        ClientMessage::RegisterAvailableLines { .. } => {
            debug!(device_id = %state.device.id, "phone finished registering available lines");
        }
        ClientMessage::OffHook {
            line_instance,
            call_reference,
        } => {
            let line = normalize_line(state, line_instance);
            let answer = find_answer_call(
                state,
                call_reference,
                line_instance,
                *context
                    .call_answer_order
                    .read()
                    .expect("SCCP call-answer-order lock poisoned"),
            )
            .cloned();
            let answering = answer.is_some();
            let call = answer.unwrap_or_else(|| {
                ensure_phone_call(state, call_reference, line, &context.next_call_id)
            });
            if let Some(stored) = state.calls_by_id.get_mut(&call.call_id) {
                stored.state = CallState::OffHook;
            }
            state.active_call_id = Some(call.call_id);
            if answering {
                begin_answer_ui(stream, &call, protocol).await?;
            } else {
                state.active_key_mode = KeyMode::OffHook;
                begin_phone_call_ui(stream, &call, &state.device, state.station_context()).await?;
            }
            context
                .event_tx
                .send(Event::device(
                    state.device.id.clone(),
                    state.generation,
                    DeviceEventKind::OffHook {
                        call_id: call.call_id,
                        line_instance: LineInstance::new(line),
                    },
                ))
                .await
                .map_err(|_| ServerError::Stopped)?;
        }
        ClientMessage::OnHook {
            line_instance,
            call_reference,
        } => {
            if let Some(call) = find_call(state, call_reference).cloned() {
                let line = if line_instance == 0 {
                    call.line_instance
                } else {
                    line_instance
                };
                context
                    .event_tx
                    .send(Event::device(
                        state.device.id.clone(),
                        state.generation,
                        DeviceEventKind::OnHook {
                            call_id: call.call_id,
                            line_instance: LineInstance::new(line),
                        },
                    ))
                    .await
                    .map_err(|_| ServerError::Stopped)?;
                state.active_key_mode = KeyMode::OnHook;
                stop_call_multicast(stream, state, call.call_id, protocol).await?;
                close_call_media_messages(stream, &call, protocol).await?;
                close_call_messages(
                    stream,
                    &call,
                    &state.device.soft_keys,
                    protocol,
                    context.config.timezone_offset_minutes,
                )
                .await?;
                request_connection_statistics(stream, state, &call, context).await?;
                if let Some(stored) = state.calls_by_id.get_mut(&call.call_id) {
                    stored.state = CallState::OnHook;
                    stored.media.receive.state = MediaChannelState::Closed;
                    stored.media.receive.deadline = None;
                    stored.media.transmit.state = MediaChannelState::Closed;
                    stored.media.transmit.deadline = None;
                    stored.media.coupled_transmit_endpoint = None;
                    stored.video_receive.leg = None;
                    stored.video_transmit.leg = None;
                }
                if state.active_call_id == Some(call.call_id) {
                    state.active_call_id = None;
                }
            }
        }
        ClientMessage::HookFlash {
            line_instance,
            call_reference,
        } => {
            let line_instance = normalize_line(state, line_instance);
            let call_id = find_call(state, call_reference).map(|call| call.call_id);
            context
                .event_tx
                .send(Event::device(
                    state.device.id.clone(),
                    state.generation,
                    DeviceEventKind::HookFlash {
                        call_id,
                        line_instance: LineInstance::new(line_instance),
                    },
                ))
                .await
                .map_err(|_| ServerError::Stopped)?;
        }
        ClientMessage::KeypadButton {
            button,
            call_reference,
            ..
        } => {
            if let Some(call) = find_call(state, call_reference) {
                if matches!(button, Digit::Unknown(_)) {
                    return Ok(());
                }
                let call = call.clone();
                if matches!(
                    call.state,
                    CallState::Connected
                        | CallState::Hold
                        | CallState::HoldYellow
                        | CallState::HoldRed
                ) && call.media.transmit.state.is_open()
                    && call.media.transmit.telephone_event_payload != 0
                {
                    // The handset sends connected-call digits in RTP when a
                    // telephone-event payload was negotiated. Forwarding the
                    // signaling copy would produce duplicate DTMF in the PBX.
                    return Ok(());
                }
                let collecting = matches!(call.state, CallState::OffHook | CallState::Transfer);
                if collecting && state.active_key_mode != KeyMode::DigitsFollowing {
                    state.active_key_mode = KeyMode::DigitsFollowing;
                    send_message(
                        stream,
                        &ServerMessage::StopTone {
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                        },
                        protocol,
                    )
                    .await?;
                    send_message(
                        stream,
                        &ServerMessage::SelectSoftKeys {
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                            set: KeyMode::DigitsFollowing,
                            valid_mask: state.device.soft_keys.valid_mask(KeyMode::DigitsFollowing),
                        },
                        protocol,
                    )
                    .await?;
                }
                if collecting && let Some(character) = digit_character(button) {
                    let number = if let Some(stored) = state.calls_by_id.get_mut(&call.call_id) {
                        stored.dialed_number.push(character);
                        stored.dialed_number.clone()
                    } else {
                        String::new()
                    };
                    if button == context.config.dial_terminator {
                        remember_last_number(state, call.line_instance, &number, &context.config);
                    }
                }
                context
                    .event_tx
                    .send(Event::device(
                        state.device.id.clone(),
                        state.generation,
                        DeviceEventKind::Digit {
                            call_id: call.call_id,
                            digit: button,
                        },
                    ))
                    .await
                    .map_err(|_| ServerError::Stopped)?;
            }
        }
        ClientMessage::EnblocCall {
            called_party,
            line_instance,
            ..
        } => {
            let line = normalize_line(state, line_instance);
            let existing = state
                .calls_by_id
                .values()
                .find(|call| call.line_instance == line && call.state != CallState::OnHook)
                .cloned();
            let created = existing.is_none();
            let call = existing
                .unwrap_or_else(|| ensure_phone_call(state, 0, line, &context.next_call_id));
            if created {
                state.active_key_mode = KeyMode::OffHook;
                begin_phone_call_ui(stream, &call, &state.device, state.station_context()).await?;
                context
                    .event_tx
                    .send(Event::device(
                        state.device.id.clone(),
                        state.generation,
                        DeviceEventKind::OffHook {
                            call_id: call.call_id,
                            line_instance: LineInstance::new(line),
                        },
                    ))
                    .await
                    .map_err(|_| ServerError::Stopped)?;
            }
            if let Some(stored) = state.calls_by_id.get_mut(&call.call_id) {
                stored.dialed_number.clone_from(&called_party);
            }
            remember_last_number(state, call.line_instance, &called_party, &context.config);
            context
                .event_tx
                .send(Event::device(
                    state.device.id.clone(),
                    state.generation,
                    DeviceEventKind::EnblocCall {
                        call_id: call.call_id,
                        line_instance: LineInstance::new(line),
                        number: called_party,
                    },
                ))
                .await
                .map_err(|_| ServerError::Stopped)?;
        }
        ClientMessage::SoftKeyEvent {
            event,
            line_instance,
            call_reference,
        } => {
            let received_soft_key = SoftKey::from(event);
            if !state
                .device
                .soft_keys
                .allows(state.active_key_mode, received_soft_key)
            {
                debug!(
                    device_id = %state.device.id,
                    mode = state.active_key_mode.wire_value(),
                    event,
                    "ignoring unavailable soft-key event"
                );
                return Ok(());
            }
            let line = normalize_line(state, line_instance);
            let mut soft_key = received_soft_key;
            let ringing_call = find_answer_call(
                state,
                call_reference,
                line_instance,
                *context
                    .call_answer_order
                    .read()
                    .expect("SCCP call-answer-order lock poisoned"),
            );
            let mut call_id = if matches!(soft_key, SoftKey::Answer | SoftKey::NewCall)
                && let Some(call) = ringing_call
            {
                soft_key = SoftKey::Answer;
                Some(call.call_id)
            } else {
                find_call(state, call_reference).map(|call| call.call_id)
            };
            if soft_key == SoftKey::MeetMe
                && call_id.is_some_and(|call_id| {
                    state
                        .calls_by_id
                        .get(&call_id)
                        .is_some_and(|call| call.state != CallState::OffHook)
                })
            {
                call_id = None;
            }
            if matches!(
                soft_key,
                SoftKey::NewCall | SoftKey::Pickup | SoftKey::GroupPickup | SoftKey::MeetMe
            ) && call_id.is_some_and(|call_id| {
                state
                    .calls_by_id
                    .get(&call_id)
                    .is_some_and(|call| call.state == CallState::OnHook)
            }) {
                call_id = None;
            }
            if soft_key == SoftKey::Redial {
                begin_redial(stream, state, context, line, call_id).await?;
                return Ok(());
            }
            if call_id.is_none()
                && matches!(
                    soft_key,
                    SoftKey::NewCall | SoftKey::Pickup | SoftKey::GroupPickup | SoftKey::MeetMe
                )
            {
                let call = if soft_key == SoftKey::MeetMe {
                    reserve_phone_call(state, line, &context.next_call_id)
                } else {
                    ensure_phone_call(state, 0, line, &context.next_call_id)
                };
                state.active_call_id = Some(call.call_id);
                state.active_key_mode = KeyMode::OffHook;
                begin_phone_call_ui(stream, &call, &state.device, state.station_context()).await?;
                context
                    .event_tx
                    .send(Event::device(
                        state.device.id.clone(),
                        state.generation,
                        DeviceEventKind::OffHook {
                            call_id: call.call_id,
                            line_instance: LineInstance::new(line),
                        },
                    ))
                    .await
                    .map_err(|_| ServerError::Stopped)?;
                call_id = Some(call.call_id);
            }
            if soft_key == SoftKey::Backspace
                && let Some(call_id) = call_id
                && let Some(call) = state.calls_by_id.get_mut(&call_id)
            {
                call.dialed_number.pop();
                let call = call.clone();
                send_message(
                    stream,
                    &ServerMessage::BackspaceResponse {
                        line_instance: call.line_instance,
                        call_reference: call.wire_reference,
                    },
                    protocol,
                )
                .await?;
            }
            if soft_key == SoftKey::Dial
                && let Some(call) = call_id.and_then(|call_id| state.calls_by_id.get(&call_id))
            {
                let line_instance = call.line_instance;
                let number = call.dialed_number.clone();
                remember_last_number(state, line_instance, &number, &context.config);
            }
            if soft_key == SoftKey::Answer
                && let Some(call) = call_id.and_then(|call_id| state.calls_by_id.get_mut(&call_id))
            {
                call.state = CallState::OffHook;
                let call = call.clone();
                state.active_call_id = Some(call.call_id);
                begin_answer_ui(stream, &call, protocol).await?;
            }
            context
                .event_tx
                .send(Event::device(
                    state.device.id.clone(),
                    state.generation,
                    DeviceEventKind::SoftKey {
                        call_id,
                        line_instance: LineInstance::new(line),
                        soft_key,
                    },
                ))
                .await
                .map_err(|_| ServerError::Stopped)?;
        }
        ClientMessage::Stimulus {
            stimulus,
            instance,
            call_reference,
            ..
        } => {
            let mut call_id = find_call(state, call_reference).map(|call| call.call_id);
            if stimulus == Stimulus::MeetMeConference
                && call_id.is_some_and(|call_id| {
                    state
                        .calls_by_id
                        .get(&call_id)
                        .is_some_and(|call| call.state != CallState::OffHook)
                })
            {
                call_id = None;
            }
            if matches!(
                stimulus,
                Stimulus::Line
                    | Stimulus::NewCall
                    | Stimulus::CallPickup
                    | Stimulus::GroupCallPickup
            ) && call_id.is_some_and(|call_id| {
                state
                    .calls_by_id
                    .get(&call_id)
                    .is_some_and(|call| call.state == CallState::OnHook)
            }) {
                call_id = None;
            }
            if stimulus == Stimulus::Line {
                let line = normalize_line(state, instance);
                if call_id.is_none() {
                    let call = ensure_phone_call(state, 0, line, &context.next_call_id);
                    state.active_key_mode = KeyMode::OffHook;
                    begin_phone_call_ui(stream, &call, &state.device, state.station_context())
                        .await?;
                    context
                        .event_tx
                        .send(Event::device(
                            state.device.id.clone(),
                            state.generation,
                            DeviceEventKind::OffHook {
                                call_id: call.call_id,
                                line_instance: LineInstance::new(line),
                            },
                        ))
                        .await
                        .map_err(|_| ServerError::Stopped)?;
                } else {
                    context
                        .event_tx
                        .send(Event::device(
                            state.device.id.clone(),
                            state.generation,
                            DeviceEventKind::LineButton {
                                line_instance: LineInstance::new(line),
                                call_id,
                            },
                        ))
                        .await
                        .map_err(|_| ServerError::Stopped)?;
                }
            } else if stimulus == Stimulus::ParkingLot {
                let configured = state.device.buttons.iter().any(|button| {
                    matches!(
                        button,
                        ButtonDefinition::Feature(feature)
                            if feature.instance == instance
                                && feature.feature == ButtonType::ParkingLot
                    )
                });
                if !configured {
                    debug!(
                        device_id = %state.device.id,
                        instance,
                        "ignoring unconfigured parking-lot button stimulus"
                    );
                    return Ok(());
                }
                let line_instance = call_id
                    .and_then(|call_id| state.calls_by_id.get(&call_id))
                    .map_or_else(|| normalize_line(state, 0), |call| call.line_instance);
                context
                    .event_tx
                    .send(Event::device(
                        state.device.id.clone(),
                        state.generation,
                        DeviceEventKind::ParkingLotButton {
                            instance: LineInstance::new(instance),
                            call_id,
                            line_instance: LineInstance::new(line_instance),
                        },
                    ))
                    .await
                    .map_err(|_| ServerError::Stopped)?;
            } else if stimulus == Stimulus::Privacy {
                let configured = state.device.buttons.iter().any(|button| {
                    matches!(
                        button,
                        ButtonDefinition::Feature(feature)
                            if feature.instance == instance
                                && feature.feature == ButtonType::Feature
                    )
                });
                if !configured {
                    debug!(
                        device_id = %state.device.id,
                        instance,
                        "ignoring unconfigured generic feature-button stimulus"
                    );
                    return Ok(());
                }
                context
                    .event_tx
                    .send(Event::device(
                        state.device.id.clone(),
                        state.generation,
                        DeviceEventKind::FeatureButton {
                            instance: LineInstance::new(instance),
                        },
                    ))
                    .await
                    .map_err(|_| ServerError::Stopped)?;
            } else if stimulus == Stimulus::DoNotDisturb {
                let configured = state.device.buttons.iter().any(|button| {
                    matches!(
                        button,
                        ButtonDefinition::Feature(feature)
                            if feature.instance == instance
                                && feature.feature == ButtonType::DoNotDisturb
                    )
                });
                if !configured {
                    debug!(
                        device_id = %state.device.id,
                        instance,
                        "ignoring unconfigured do-not-disturb button stimulus"
                    );
                    return Ok(());
                }
                context
                    .event_tx
                    .send(Event::device(
                        state.device.id.clone(),
                        state.generation,
                        DeviceEventKind::DoNotDisturbButton {
                            instance: LineInstance::new(instance),
                        },
                    ))
                    .await
                    .map_err(|_| ServerError::Stopped)?;
            } else if stimulus == Stimulus::Mobility {
                let configured = state.device.buttons.iter().any(|button| {
                    matches!(
                        button,
                        ButtonDefinition::Feature(feature)
                            if feature.instance == instance
                                && feature.feature == ButtonType::Mobility
                    )
                });
                if !configured {
                    debug!(
                        device_id = %state.device.id,
                        instance,
                        "ignoring unconfigured mobility button stimulus"
                    );
                    return Ok(());
                }
                context
                    .event_tx
                    .send(Event::device(
                        state.device.id.clone(),
                        state.generation,
                        DeviceEventKind::MobilityButton {
                            instance: LineInstance::new(instance),
                        },
                    ))
                    .await
                    .map_err(|_| ServerError::Stopped)?;
            } else if stimulus == Stimulus::Voicemail {
                let configured = state.device.buttons.iter().any(|button| {
                    matches!(
                        button,
                        ButtonDefinition::Feature(feature)
                            if feature.instance == instance
                                && feature.feature == ButtonType::Voicemail
                    )
                });
                if !configured {
                    debug!(
                        device_id = %state.device.id,
                        instance,
                        "ignoring unconfigured voicemail button stimulus"
                    );
                    return Ok(());
                }
                let line = call_id
                    .and_then(|call_id| state.calls_by_id.get(&call_id))
                    .map_or_else(
                        || normalize_line(state, instance),
                        |call| call.line_instance,
                    );
                let call = call_id
                    .and_then(|call_id| state.calls_by_id.get(&call_id).cloned())
                    .unwrap_or_else(|| ensure_phone_call(state, 0, line, &context.next_call_id));
                if call_id.is_none() {
                    state.active_call_id = Some(call.call_id);
                    state.active_key_mode = KeyMode::OffHook;
                    begin_phone_call_ui(stream, &call, &state.device, state.station_context())
                        .await?;
                    context
                        .event_tx
                        .send(Event::device(
                            state.device.id.clone(),
                            state.generation,
                            DeviceEventKind::OffHook {
                                call_id: call.call_id,
                                line_instance: LineInstance::new(line),
                            },
                        ))
                        .await
                        .map_err(|_| ServerError::Stopped)?;
                }
                context
                    .event_tx
                    .send(Event::device(
                        state.device.id.clone(),
                        state.generation,
                        DeviceEventKind::VoicemailButton {
                            call_id: call.call_id,
                            line_instance: LineInstance::new(line),
                        },
                    ))
                    .await
                    .map_err(|_| ServerError::Stopped)?;
            } else {
                let line = normalize_line(state, instance);
                let Some(soft_key) = stimulus_soft_key(stimulus) else {
                    debug!(
                        device_id = %state.device.id,
                        stimulus = stimulus.wire_value(),
                        "ignoring stimulus without a soft-key action mapping"
                    );
                    return Ok(());
                };
                if !state
                    .device
                    .soft_keys
                    .allows(state.active_key_mode, soft_key)
                {
                    debug!(
                        device_id = %state.device.id,
                        mode = state.active_key_mode.wire_value(),
                        stimulus = stimulus.wire_value(),
                        "ignoring unavailable soft-key stimulus"
                    );
                    return Ok(());
                }
                if soft_key == SoftKey::Redial {
                    begin_redial(stream, state, context, line, call_id).await?;
                    return Ok(());
                }
                if matches!(
                    soft_key,
                    SoftKey::NewCall | SoftKey::Pickup | SoftKey::GroupPickup | SoftKey::MeetMe
                ) && call_id.is_none()
                {
                    let call = if soft_key == SoftKey::MeetMe {
                        reserve_phone_call(state, line, &context.next_call_id)
                    } else {
                        ensure_phone_call(state, 0, line, &context.next_call_id)
                    };
                    state.active_call_id = Some(call.call_id);
                    state.active_key_mode = KeyMode::OffHook;
                    begin_phone_call_ui(stream, &call, &state.device, state.station_context())
                        .await?;
                    context
                        .event_tx
                        .send(Event::device(
                            state.device.id.clone(),
                            state.generation,
                            DeviceEventKind::OffHook {
                                call_id: call.call_id,
                                line_instance: LineInstance::new(line),
                            },
                        ))
                        .await
                        .map_err(|_| ServerError::Stopped)?;
                    call_id = Some(call.call_id);
                }
                context
                    .event_tx
                    .send(Event::device(
                        state.device.id.clone(),
                        state.generation,
                        DeviceEventKind::SoftKey {
                            call_id,
                            line_instance: LineInstance::new(line),
                            soft_key,
                        },
                    ))
                    .await
                    .map_err(|_| ServerError::Stopped)?;
            }
        }
        ClientMessage::MulticastMediaReceptionAck {
            status,
            passthrough_party_id,
            call_reference,
        } => {
            let Some(key) =
                find_multicast_receive_key(state, call_reference.get(), passthrough_party_id.get())
            else {
                debug!(
                    device_id = %state.device.id,
                    "ignored stale or mismatched multicast reception acknowledgement"
                );
                return Ok(());
            };
            if status == MediaStatus::Ok {
                let route = {
                    let receive = state
                        .multicast
                        .get_mut(&key)
                        .and_then(|session| session.receive.as_mut())
                        .expect("multicast key came from current receive state");
                    receive.state = MulticastReceiveState::Open;
                    receive.route
                };
                context
                    .event_tx
                    .send(Event::device(
                        state.device.id.clone(),
                        state.generation,
                        DeviceEventKind::MulticastReceptionStarted {
                            conference_id: key.conference_id,
                            call_id: key.call_id,
                            route,
                        },
                    ))
                    .await
                    .map_err(|_| ServerError::Stopped)?;
            } else {
                if let Some(stop) = take_multicast_stop(state, key, true) {
                    send_message(stream, &stop, protocol).await?;
                }
                context
                    .event_tx
                    .send(Event::device(
                        state.device.id.clone(),
                        state.generation,
                        DeviceEventKind::MulticastReceptionFailed {
                            conference_id: key.conference_id,
                            call_id: key.call_id,
                            status,
                        },
                    ))
                    .await
                    .map_err(|_| ServerError::Stopped)?;
            }
        }
        ClientMessage::OpenReceiveChannelAck {
            status,
            address,
            port,
            call_reference,
            passthrough_party_id,
        } => {
            if let Some(call_id) =
                find_receive_media_call_id(state, call_reference, passthrough_party_id)
            {
                let call = state
                    .calls_by_id
                    .get(&call_id)
                    .expect("media call identifier came from session state")
                    .clone();
                if call.media.receive.state != MediaChannelState::Opening {
                    debug!(
                        device_id = %state.device.id,
                        call_id = ?call.call_id,
                        state = ?call.media.receive.state,
                        "ignored stale receive-channel acknowledgement"
                    );
                    return Ok(());
                }
                let endpoint = MediaEndpoint {
                    address,
                    rtp_port: port,
                    rtcp_port: port.saturating_add(1),
                    codec: call.media.codec,
                    packet_ms: call.media.packet_ms,
                    max_frames_per_packet: call.media.max_frames_per_packet,
                    telephone_event_payload: call.media.receive.telephone_event_payload,
                };
                let stored = state
                    .calls_by_id
                    .get_mut(&call_id)
                    .expect("media call identifier came from session state");
                let implied_transmit = if status == MediaStatus::Ok {
                    stored.media.receive.state = MediaChannelState::Open;
                    stored.media.receive.peer = Some(endpoint);
                    if let Some(endpoint) = stored.media.coupled_transmit_endpoint.take() {
                        stored.media.transmit.state = MediaChannelState::Open;
                        stored.media.transmit.peer = Some(endpoint);
                        stored.media.transmit.deadline = None;
                        Some(endpoint)
                    } else {
                        None
                    }
                } else {
                    stored.media.receive.state = MediaChannelState::Closed;
                    stored.media.receive.peer = None;
                    if stored.media.coupled_transmit_endpoint.take().is_some() {
                        stored.media.transmit.state = MediaChannelState::Closed;
                        stored.media.transmit.peer = None;
                        stored.media.transmit.deadline = None;
                    }
                    None
                };
                stored.media.receive.deadline = None;
                context
                    .event_tx
                    .send(Event::device(
                        state.device.id.clone(),
                        state.generation,
                        DeviceEventKind::ReceiveChannelOpened {
                            call_id: call.call_id,
                            status,
                            endpoint,
                        },
                    ))
                    .await
                    .map_err(|_| ServerError::Stopped)?;
                if let Some(endpoint) = implied_transmit {
                    context
                        .event_tx
                        .send(Event::device(
                            state.device.id.clone(),
                            state.generation,
                            DeviceEventKind::TransmitChannelImplied {
                                call_id: call.call_id,
                                endpoint,
                            },
                        ))
                        .await
                        .map_err(|_| ServerError::Stopped)?;
                }
            }
        }
        ClientMessage::StartMediaTransmissionAck(ack) => {
            if let Some(call_id) = find_transmit_media_call_id(
                state,
                ack.conference_id,
                ack.call_reference,
                ack.passthrough_party_id,
            ) {
                let call = state
                    .calls_by_id
                    .get(&call_id)
                    .expect("media call identifier came from session state")
                    .clone();
                if call.media.transmit.state != MediaChannelState::Opening {
                    debug!(
                        device_id = %state.device.id,
                        call_id = ?call.call_id,
                        state = ?call.media.transmit.state,
                        "ignored stale transmit-channel acknowledgement"
                    );
                    return Ok(());
                }
                let endpoint = MediaEndpoint {
                    address: ack.address,
                    rtp_port: ack.port,
                    rtcp_port: ack.port.saturating_add(1),
                    codec: call.media.codec,
                    packet_ms: call.media.packet_ms,
                    max_frames_per_packet: call.media.max_frames_per_packet,
                    telephone_event_payload: call.media.transmit.telephone_event_payload,
                };
                let stored = state
                    .calls_by_id
                    .get_mut(&call_id)
                    .expect("media call identifier came from session state");
                let coupled = stored.media.coupled_transmit_endpoint.take().is_some();
                if ack.status == MediaStatus::Ok {
                    stored.media.transmit.state = MediaChannelState::Open;
                    stored.media.transmit.peer = Some(endpoint);
                } else {
                    stored.media.transmit.state = MediaChannelState::Closed;
                    stored.media.transmit.peer = None;
                    if coupled {
                        stored.media.receive.state = MediaChannelState::Closed;
                        stored.media.receive.deadline = None;
                        stored.media.receive.peer = None;
                    }
                }
                stored.media.transmit.deadline = None;
                context
                    .event_tx
                    .send(Event::device(
                        state.device.id.clone(),
                        state.generation,
                        DeviceEventKind::TransmitChannelStarted {
                            call_id: call.call_id,
                            status: ack.status,
                            endpoint,
                        },
                    ))
                    .await
                    .map_err(|_| ServerError::Stopped)?;
            }
        }
        ClientMessage::Alarm {
            severity,
            text,
            parameters,
        } => {
            context
                .event_tx
                .send(Event::device(
                    state.device.id.clone(),
                    state.generation,
                    DeviceEventKind::Alarm {
                        severity,
                        text,
                        parameters,
                    },
                ))
                .await
                .map_err(|_| ServerError::Stopped)?;
        }
        ClientMessage::XmlAlarm(message) => match parse_phone_alarm(message.xml_bytes()) {
            Ok(telemetry) => {
                context
                    .event_tx
                    .send(Event::device(
                        state.device.id.clone(),
                        state.generation,
                        DeviceEventKind::XmlAlarm { telemetry },
                    ))
                    .await
                    .map_err(|_| ServerError::Stopped)?;
            }
            Err(error) => {
                warn!(
                    device_id = %state.device.id,
                    payload_len = message.xml_bytes().len(),
                    %error,
                    "rejected SCCP XML alarm"
                );
            }
        },
        ClientMessage::LocationInfo { xml } => match parse_phone_location(xml.as_bytes()) {
            Ok(telemetry) => {
                context
                    .event_tx
                    .send(Event::device(
                        state.device.id.clone(),
                        state.generation,
                        DeviceEventKind::LocationInformation { telemetry },
                    ))
                    .await
                    .map_err(|_| ServerError::Stopped)?;
            }
            Err(error) => {
                warn!(
                    device_id = %state.device.id,
                    payload_len = xml.len(),
                    %error,
                    "rejected SCCP location information"
                );
            }
        },
        ClientMessage::Unregister { .. } => {
            send_message(stream, &ServerMessage::UnregisterAck, protocol).await?;
        }
        ClientMessage::CallCountRequest { .. } => {
            send_message(stream, &ServerMessage::CallCountResponse, protocol).await?;
        }
        ClientMessage::ConnectionStatisticsResponse(statistics) => {
            collect_connection_statistics(state, statistics, context).await?;
        }
        ClientMessage::MediaTransmissionFailure {
            conference_id,
            passthrough_party_id,
            address,
            port,
            call_reference,
            status,
        } => {
            if let Some(key) = find_multicast_transmit_key(
                state,
                conference_id,
                call_reference,
                passthrough_party_id,
                address,
                port,
            ) {
                if let Some(stop) = take_multicast_stop(state, key, false) {
                    send_message(stream, &stop, protocol).await?;
                }
                context
                    .event_tx
                    .send(Event::device(
                        state.device.id.clone(),
                        state.generation,
                        DeviceEventKind::MulticastTransmissionFailed {
                            conference_id: key.conference_id,
                            call_id: key.call_id,
                            status,
                            address,
                            port,
                        },
                    ))
                    .await
                    .map_err(|_| ServerError::Stopped)?;
                return Ok(());
            }
            let Some(call_id) = find_transmit_media_call_id(
                state,
                conference_id,
                call_reference,
                passthrough_party_id,
            ) else {
                return Ok(());
            };
            let call = state
                .calls_by_id
                .get(&call_id)
                .expect("media call identifier came from session state")
                .clone();
            let Some(endpoint) = call.media.transmit.peer else {
                return Ok(());
            };
            if call.media.transmit.state != MediaChannelState::Open
                || (conference_id != 0 && conference_id != call.wire_reference)
                || endpoint.address != address
                || endpoint.rtp_port != port
            {
                debug!(
                    device_id = %state.device.id,
                    call_id = ?call.call_id,
                    "ignored stale or mismatched media-transmission failure"
                );
                return Ok(());
            }
            let stored = state
                .calls_by_id
                .get_mut(&call_id)
                .expect("media call identifier came from session state");
            stored.media.transmit.state = MediaChannelState::Closed;
            stored.media.transmit.peer = None;
            context
                .event_tx
                .send(Event::device(
                    state.device.id.clone(),
                    state.generation,
                    DeviceEventKind::MediaTransmissionFailed {
                        call_id,
                        status,
                        endpoint,
                    },
                ))
                .await
                .map_err(|_| ServerError::Stopped)?;
        }
        ClientMessage::HeadsetStatus { enabled } => {
            if state.headset_enabled != enabled {
                state.headset_enabled = enabled;
                context
                    .event_tx
                    .send(Event::device(
                        state.device.id.clone(),
                        state.generation,
                        DeviceEventKind::HeadsetStatusChanged { enabled },
                    ))
                    .await
                    .map_err(|_| ServerError::Stopped)?;
            }
        }
        ClientMessage::MediaPathEvent {
            path,
            event: media_path_event,
        } => {
            if state.media_path_states.get(&path) != Some(&media_path_event) {
                state.media_path_states.insert(path, media_path_event);
                context
                    .event_tx
                    .send(Event::device(
                        state.device.id.clone(),
                        state.generation,
                        DeviceEventKind::MediaPathChanged {
                            path,
                            event: media_path_event,
                        },
                    ))
                    .await
                    .map_err(|_| ServerError::Stopped)?;
            }
        }
        ClientMessage::MediaPathCapability { .. } => {}
        message @ (ClientMessage::IpPort { .. }
        | ClientMessage::OffHookWithCallingParty { .. }
        | ClientMessage::MediaResourceNotification(_)
        | ClientMessage::SubscribeDtmfPayloadResponse(_)
        | ClientMessage::UnsubscribeDtmfPayloadResponse(_)
        | ClientMessage::PortResponse(_)) => {
            debug!(device_id = %state.device.id, message = ?message, "consumed SCCP telemetry");
        }
        ClientMessage::DeviceToUserData(message) => {
            handle_phone_service_message(
                state,
                context,
                crate::message::id::DEVICE_TO_USER_DATA,
                PhoneServiceMessageKind::Data,
                PhoneServiceRouting {
                    application_id: ApplicationId::new(message.application_id),
                    line_instance: LineInstance::new(message.line_instance),
                    call_reference: CallReference::new(message.call_reference),
                    transaction_id: TransactionId::new(message.transaction_id),
                },
                None,
                &message.data,
            )
            .await?;
        }
        ClientMessage::DeviceToUserDataResponse(message) => {
            handle_phone_service_message(
                state,
                context,
                crate::message::id::DEVICE_TO_USER_DATA_RESPONSE,
                PhoneServiceMessageKind::Response,
                PhoneServiceRouting {
                    application_id: ApplicationId::new(message.application_id),
                    line_instance: LineInstance::new(message.line_instance),
                    call_reference: CallReference::new(message.call_reference),
                    transaction_id: TransactionId::new(message.transaction_id),
                },
                None,
                &message.data,
            )
            .await?;
        }
        ClientMessage::DeviceToUserDataV1(message) => {
            handle_phone_service_message(
                state,
                context,
                crate::message::id::DEVICE_TO_USER_DATA_V1,
                PhoneServiceMessageKind::Data,
                PhoneServiceRouting {
                    application_id: ApplicationId::new(message.application_id),
                    line_instance: LineInstance::new(message.line_instance),
                    call_reference: CallReference::new(message.call_reference),
                    transaction_id: TransactionId::new(message.transaction_id),
                },
                Some(PhoneServiceExtendedRouting {
                    sequence_flag: message.sequence_flag,
                    display_priority: message.display_priority,
                    conference_id: message.conference_id,
                    application_instance_id: message.application_instance_id,
                    routing: message.routing,
                }),
                &message.data,
            )
            .await?;
        }
        ClientMessage::DeviceToUserDataResponseV1(message) => {
            handle_phone_service_message(
                state,
                context,
                crate::message::id::DEVICE_TO_USER_DATA_RESPONSE_V1,
                PhoneServiceMessageKind::Response,
                PhoneServiceRouting {
                    application_id: ApplicationId::new(message.application_id),
                    line_instance: LineInstance::new(message.line_instance),
                    call_reference: CallReference::new(message.call_reference),
                    transaction_id: TransactionId::new(message.transaction_id),
                },
                Some(PhoneServiceExtendedRouting {
                    sequence_flag: message.sequence_flag,
                    display_priority: message.display_priority,
                    conference_id: message.conference_id,
                    application_instance_id: message.application_instance_id,
                    routing: message.routing,
                }),
                &message.data,
            )
            .await?;
        }
        ClientMessage::OpenMultimediaReceiveChannelAck(ack) => {
            let Some(call_id) = state.calls_by_wire.get(&ack.call_reference.get()).copied() else {
                debug!(device_id = %state.device.id, "ignored video receive acknowledgement for an unknown call");
                return Ok(());
            };
            let Some((request, codec, requested_address_type)) =
                state.calls_by_id.get(&call_id).and_then(|call| {
                    call.video_receive.leg.as_ref().and_then(|leg| {
                        (leg.state == MediaChannelState::Opening
                            && leg.request.token().get() == ack.passthrough_party_id.get())
                        .then_some((leg.request, leg.codec, leg.requested_address_type))
                    })
                })
            else {
                debug!(device_id = %state.device.id, ?call_id, "ignored stale video receive acknowledgement");
                return Ok(());
            };

            let event = if ack.status == MediaStatus::Ok {
                if !endpoint_is_usable(ack.endpoint)
                    || !address_matches_type(ack.endpoint.address, requested_address_type)
                {
                    debug!(device_id = %state.device.id, ?call_id, "ignored unusable video receive endpoint");
                    return Ok(());
                }
                let leg = state
                    .calls_by_id
                    .get_mut(&call_id)
                    .and_then(|call| call.video_receive.leg.as_mut())
                    .expect("correlated video receive leg remains present");
                debug_assert_eq!(leg.request, request);
                leg.state = MediaChannelState::Open;
                leg.deadline = None;
                DeviceEventKind::MultimediaReceiveChannelOpened {
                    call_id,
                    codec,
                    endpoint: ack.endpoint,
                    passthrough_party_id: ack.passthrough_party_id,
                }
            } else {
                let close = take_multimedia_receive_close(state, call_id)
                    .expect("correlated video receive leg remains present");
                send_message(stream, &close, protocol).await?;
                DeviceEventKind::MultimediaReceiveChannelFailed {
                    call_id,
                    codec,
                    status: ack.status,
                    endpoint: ack.endpoint,
                    passthrough_party_id: ack.passthrough_party_id,
                }
            };
            context
                .event_tx
                .send(Event::device(
                    state.device.id.clone(),
                    state.generation,
                    event,
                ))
                .await
                .map_err(|_| ServerError::Stopped)?;
        }
        ClientMessage::StartMultimediaTransmissionAck(ack) => {
            let Some(call_id) = state.calls_by_wire.get(&ack.call_reference.get()).copied() else {
                debug!(device_id = %state.device.id, "ignored video transmit acknowledgement for an unknown call");
                return Ok(());
            };
            let Some((request, codec, address_type)) =
                state.calls_by_id.get(&call_id).and_then(|call| {
                    call.video_transmit.leg.as_ref().and_then(|leg| {
                        (leg.state == MediaChannelState::Opening
                            && leg.request.token().get() == ack.passthrough_party_id.get()
                            && leg.conference_id == ack.conference_id)
                            .then_some((leg.request, leg.codec, leg.address_type))
                    })
                })
            else {
                debug!(device_id = %state.device.id, ?call_id, "ignored stale video transmit acknowledgement");
                return Ok(());
            };

            let event = if ack.status == MediaStatus::Ok {
                if !endpoint_is_usable(ack.endpoint)
                    || !address_matches_type(ack.endpoint.address, address_type)
                {
                    debug!(device_id = %state.device.id, ?call_id, "ignored unusable video transmit endpoint");
                    return Ok(());
                }
                let leg = state
                    .calls_by_id
                    .get_mut(&call_id)
                    .and_then(|call| call.video_transmit.leg.as_mut())
                    .expect("correlated video transmit leg remains present");
                debug_assert_eq!(leg.request, request);
                leg.state = MediaChannelState::Open;
                leg.deadline = None;
                DeviceEventKind::MultimediaTransmitStarted {
                    call_id,
                    codec,
                    endpoint: ack.endpoint,
                    passthrough_party_id: ack.passthrough_party_id,
                }
            } else {
                let stop = take_multimedia_transmit_stop(state, call_id)
                    .expect("correlated video transmit leg remains present");
                send_message(stream, &stop, protocol).await?;
                DeviceEventKind::MultimediaTransmitFailed {
                    call_id,
                    codec,
                    status: ack.status,
                    endpoint: ack.endpoint,
                    passthrough_party_id: ack.passthrough_party_id,
                }
            };
            context
                .event_tx
                .send(Event::device(
                    state.device.id.clone(),
                    state.generation,
                    event,
                ))
                .await
                .map_err(|_| ServerError::Stopped)?;
        }
        message @ (ClientMessage::ExtensionDeviceCapabilities(_)
        | ClientMessage::CreateConferenceResponse(_)
        | ClientMessage::DeleteConferenceResponse { .. }
        | ClientMessage::ModifyConferenceResponse(_)
        | ClientMessage::AuditConferenceResponse(_)
        | ClientMessage::AddParticipantResponse(_)
        | ClientMessage::AuditParticipantResponse(_)) => {
            debug!(device_id = %state.device.id, message = ?message, "deferred SCCP application message");
            context
                .event_tx
                .send(Event::device(
                    state.device.id.clone(),
                    state.generation,
                    DeviceEventKind::UnhandledMessage { message },
                ))
                .await
                .map_err(|_| ServerError::Stopped)?;
        }
        ClientMessage::KnownOpaque(message) => {
            let message = ClientMessage::KnownOpaque(message);
            debug!(device_id = %state.device.id, message = ?message, "unhandled SCCP message");
            context
                .event_tx
                .send(Event::device(
                    state.device.id.clone(),
                    state.generation,
                    DeviceEventKind::UnhandledMessage { message },
                ))
                .await
                .map_err(|_| ServerError::Stopped)?;
        }
        ClientMessage::Unknown(message) => {
            let message = ClientMessage::Unknown(message);
            warn!(device_id = %state.device.id, message = ?message, "unknown SCCP message");
            context
                .event_tx
                .send(Event::device(
                    state.device.id.clone(),
                    state.generation,
                    DeviceEventKind::UnhandledMessage { message },
                ))
                .await
                .map_err(|_| ServerError::Stopped)?;
        }
        ClientMessage::Register(_) | ClientMessage::RegisterToken(_) => {
            warn!(device_id = %state.device.id, "ignoring registration message on registered session");
        }
    }
    Ok(())
}

fn button_template(device: &DeviceDefinition) -> Vec<ButtonTemplateEntry> {
    let mut buttons = Vec::with_capacity(56);
    let mut addon_buttons_remaining = None;
    for button in &device.buttons {
        if let ButtonDefinition::AddonModule(addon) = button {
            buttons.extend(std::iter::repeat_n(
                ButtonTemplateEntry {
                    instance: 0,
                    button_type: ButtonType::Unused,
                },
                addon_buttons_remaining.take().unwrap_or_default(),
            ));
            addon_buttons_remaining = addon.button_capacity();
            continue;
        }
        buttons.push(match button {
            ButtonDefinition::Line(appearance) => ButtonTemplateEntry {
                instance: appearance.instance,
                button_type: ButtonType::Line,
            },
            ButtonDefinition::SpeedDial(speed_dial) => ButtonTemplateEntry {
                instance: speed_dial.instance,
                button_type: ButtonType::SpeedDial,
            },
            ButtonDefinition::BlfSpeedDial(speed_dial) => ButtonTemplateEntry {
                instance: speed_dial.instance,
                button_type: ButtonType::BlfSpeedDial,
            },
            ButtonDefinition::Feature(feature) => ButtonTemplateEntry {
                instance: feature.instance,
                button_type: ButtonType::from(feature.feature.wire_value()),
            },
            ButtonDefinition::Service(service) => ButtonTemplateEntry {
                instance: service.instance,
                button_type: ButtonType::ServiceUrl,
            },
            ButtonDefinition::Unused => ButtonTemplateEntry {
                instance: 0,
                button_type: ButtonType::Unused,
            },
            ButtonDefinition::AddonModule(_) => unreachable!("addon marker handled above"),
        });
        if let Some(remaining) = &mut addon_buttons_remaining {
            *remaining = remaining.saturating_sub(1);
        }
    }
    buttons.extend(std::iter::repeat_n(
        ButtonTemplateEntry {
            instance: 0,
            button_type: ButtonType::Unused,
        },
        addon_buttons_remaining.unwrap_or_default(),
    ));
    buttons
}

fn line_status(device: &DeviceDefinition, instance: u32) -> Option<ServerMessage> {
    device
        .line(instance)
        .map(|appearance| ServerMessage::LineStatus {
            instance: appearance.instance,
            number: appearance.line.number.clone(),
            display_name: appearance.display_label().to_owned(),
        })
}

fn mobility_device_candidate(
    current: &DeviceDefinition,
    current_appearances: &HashMap<u32, LineAppearance>,
    next_appearances: &HashMap<u32, LineAppearance>,
) -> Result<DeviceDefinition, CodecError> {
    let mut candidate = current.clone();
    candidate.buttons.retain(|button| {
        !matches!(
            button,
            ButtonDefinition::Line(line)
                if current_appearances.values().any(|appearance| appearance == line)
        )
    });
    let mut index = 0;
    while index < candidate.buttons.len() {
        let mobility_instance = match &candidate.buttons[index] {
            ButtonDefinition::Feature(feature) if feature.feature == ButtonType::Mobility => {
                Some(feature.instance)
            }
            _ => None,
        };
        if let Some(appearance) = mobility_instance
            .and_then(|instance| next_appearances.get(&instance))
            .cloned()
        {
            candidate
                .buttons
                .insert(index + 1, ButtonDefinition::Line(appearance));
            index += 2;
        } else {
            index += 1;
        }
    }
    candidate.validate()?;
    Ok(candidate)
}

fn speed_dial_status(device: &DeviceDefinition, instance: u32) -> ServerMessage {
    let speed_dial = device.buttons.iter().find_map(|button| match button {
        ButtonDefinition::SpeedDial(speed_dial) if speed_dial.instance == instance => {
            Some((&speed_dial.number, &speed_dial.display_name))
        }
        ButtonDefinition::BlfSpeedDial(speed_dial) if speed_dial.instance == instance => {
            Some((&speed_dial.number, &speed_dial.display_name))
        }
        _ => None,
    });
    ServerMessage::SpeedDialStatus {
        instance,
        number: speed_dial.map_or_else(String::new, |(number, _)| number.clone()),
        display_name: speed_dial.map_or_else(String::new, |(_, display_name)| display_name.clone()),
    }
}

fn feature_status(
    device: &DeviceDefinition,
    instance: u32,
    capabilities: u32,
) -> Option<ServerMessage> {
    device.buttons.iter().find_map(|button| match button {
        ButtonDefinition::BlfSpeedDial(speed_dial)
            if capabilities == 1 && speed_dial.instance == instance =>
        {
            Some(ServerMessage::FeatureStatus {
                instance,
                button_type: ButtonType::BlfSpeedDial,
                label: speed_dial.display_name.clone(),
                state: BusyLampFieldState::UnknownState.wire_value(),
            })
        }
        ButtonDefinition::Feature(feature) if feature.instance == instance => {
            Some(ServerMessage::FeatureStatus {
                instance,
                button_type: ButtonType::from(feature.feature.wire_value()),
                label: feature.label.clone(),
                state: 0,
            })
        }
        _ => None,
    })
}

fn feature_state_messages(
    device: &DeviceDefinition,
    instance: u32,
    enabled: bool,
) -> Option<[ServerMessage; 2]> {
    let feature = device.buttons.iter().find_map(|button| match button {
        ButtonDefinition::Feature(feature) if feature.instance == instance => Some(feature),
        _ => None,
    })?;
    Some([
        ServerMessage::FeatureStatus {
            instance,
            button_type: ButtonType::from(feature.feature.wire_value()),
            label: feature.label.clone(),
            state: u32::from(enabled),
        },
        ServerMessage::SetLamp {
            stimulus: feature.feature,
            instance,
            mode: if enabled { LampMode::On } else { LampMode::Off },
        },
    ])
}

fn do_not_disturb_state_messages(
    device: &DeviceDefinition,
    instance: u32,
    mode: DoNotDisturbMode,
    button_mode: DoNotDisturbButtonMode,
    protocol: ProtocolVersion,
) -> Option<[ServerMessage; 2]> {
    let feature = device.buttons.iter().find_map(|button| match button {
        ButtonDefinition::Feature(feature)
            if feature.instance == instance && feature.feature == ButtonType::DoNotDisturb =>
        {
            Some(feature)
        }
        _ => None,
    })?;
    let exact_enabled = match button_mode {
        DoNotDisturbButtonMode::Cycle => mode != DoNotDisturbMode::Off,
        DoNotDisturbButtonMode::Silent => mode == DoNotDisturbMode::Silent,
        DoNotDisturbButtonMode::Reject => mode == DoNotDisturbMode::Reject,
    };
    let multi_state =
        button_mode == DoNotDisturbButtonMode::Cycle && protocol > ProtocolVersion::V15;
    let (button_type, state) = if multi_state {
        (
            ButtonType::MultiblinkFeature,
            match mode {
                DoNotDisturbMode::Off => 0x010000,
                DoNotDisturbMode::Reject => 0x020202,
                DoNotDisturbMode::Silent => 0x030302,
            },
        )
    } else {
        (ButtonType::DoNotDisturb, u32::from(exact_enabled))
    };
    let lamp = match (exact_enabled, mode) {
        (false, _) | (_, DoNotDisturbMode::Off) => LampMode::Off,
        (true, DoNotDisturbMode::Silent) => LampMode::Blink,
        (true, DoNotDisturbMode::Reject) => LampMode::On,
    };
    Some([
        ServerMessage::FeatureStatus {
            instance,
            button_type,
            label: feature.label.clone(),
            state,
        },
        ServerMessage::SetLamp {
            stimulus: feature.feature,
            instance,
            mode: lamp,
        },
    ])
}

fn blf_status_messages(
    instance: u32,
    number: &str,
    label: &str,
    state: BlfState,
    caller: Option<&BlfCallerInfo>,
) -> [ServerMessage; 3] {
    let caller = caller.map(BlfCallerInfo::display).unwrap_or_default();
    let dynamic_label = if caller.is_empty() {
        label.to_owned()
    } else {
        format!("{label}: {caller}")
    };
    let icon = match state {
        BlfState::Idle => BusyLampFieldState::Idle,
        BlfState::Ringing => BusyLampFieldState::Alerting,
        BlfState::Busy | BlfState::Held => BusyLampFieldState::InUse,
        BlfState::Unavailable | BlfState::Unknown => BusyLampFieldState::UnknownState,
    };
    let lamp = match state {
        BlfState::Idle => LampMode::Off,
        BlfState::Ringing => LampMode::Blink,
        BlfState::Busy => LampMode::On,
        BlfState::Held => LampMode::Hold,
        BlfState::Unavailable => LampMode::Flash,
        BlfState::Unknown => LampMode::Wink,
    };
    [
        ServerMessage::SpeedDialStatus {
            instance,
            number: truncate_utf8(number, 23),
            display_name: truncate_utf8(&dynamic_label, 39),
        },
        ServerMessage::FeatureStatus {
            instance,
            button_type: ButtonType::BlfSpeedDial,
            label: truncate_utf8(&dynamic_label, 39),
            state: icon.wire_value(),
        },
        ServerMessage::SetLamp {
            stimulus: ButtonType::BlfSpeedDial,
            instance,
            mode: lamp,
        },
    ]
}

fn hinted_ringing_notification(
    device: &DeviceDefinition,
    label: &str,
    caller: Option<&BlfCallerInfo>,
    state: BlfState,
) -> Option<HandsetStatusMessage> {
    if !device.ui.hinted_ringing_notification || state != BlfState::Ringing {
        return None;
    }
    let caller = caller.map(BlfCallerInfo::display).unwrap_or_default();
    let text = if caller.is_empty() {
        format!("{label} is ringing")
    } else {
        format!("{label} is ringing: {caller}")
    };
    Some(HandsetStatusMessage::Display {
        text: truncate_utf8(&text, 79),
        timeout_seconds: 5,
        priority: None,
    })
}

fn truncate_utf8(value: &str, maximum_bytes: usize) -> String {
    if value.len() <= maximum_bytes {
        return value.to_owned();
    }
    let end = value
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= maximum_bytes)
        .last()
        .unwrap_or(0);
    value[..end].to_owned()
}

fn service_url_status(device: &DeviceDefinition, index: u32) -> Option<ServerMessage> {
    device.buttons.iter().find_map(|button| match button {
        ButtonDefinition::Service(service) if service.instance == index => {
            Some(ServerMessage::ServiceUrlStatus {
                index,
                url: service.url.clone(),
                label: service.label.clone(),
                extension_text: String::new(),
            })
        }
        _ => None,
    })
}

const fn key_mode_for_call_state(state: CallState) -> KeyMode {
    match state {
        CallState::Connected => KeyMode::Connected,
        CallState::Hold | CallState::HoldYellow | CallState::HoldRed => KeyMode::OnHold,
        CallState::RingIn | CallState::CallWaiting => KeyMode::RingIn,
        CallState::OffHook
        | CallState::Busy
        | CallState::Congestion
        | CallState::InvalidNumber
        | CallState::IntercomOneWay => KeyMode::OffHook,
        CallState::Transfer => KeyMode::ConnectedTransfer,
        CallState::RingOut | CallState::Proceed => KeyMode::RingOut,
        CallState::RemoteMultiline => KeyMode::OnHookStealable,
        CallState::OnHook | CallState::Park | CallState::Unknown(_) => KeyMode::OnHook,
    }
}

fn transfer_key_mode(call: &SessionCall, state: CallState) -> KeyMode {
    if matches!(
        call.transfer_role,
        Some(SessionTransferRole::Consultation { .. })
    ) && matches!(state, CallState::RingOut | CallState::Connected)
    {
        KeyMode::ConnectedTransfer
    } else {
        key_mode_for_call_state(state)
    }
}

fn stimulus_soft_key(stimulus: Stimulus) -> Option<SoftKey> {
    Some(match stimulus {
        Stimulus::LastNumberRedial => SoftKey::Redial,
        Stimulus::Hold => SoftKey::Hold,
        Stimulus::Transfer => SoftKey::Transfer,
        Stimulus::ForwardAll => SoftKey::ForwardAll,
        Stimulus::ForwardBusy => SoftKey::ForwardBusy,
        Stimulus::ForwardNoAnswer => SoftKey::ForwardNoAnswer,
        Stimulus::Conference => SoftKey::Conference,
        Stimulus::MeetMeConference => SoftKey::MeetMe,
        Stimulus::CallPark => SoftKey::Park,
        Stimulus::CallPickup => SoftKey::Pickup,
        Stimulus::GroupCallPickup => SoftKey::GroupPickup,
        Stimulus::DoNotDisturb => SoftKey::DoNotDisturb,
        Stimulus::ConferenceList => SoftKey::ConferenceList,
        Stimulus::NewCall => SoftKey::NewCall,
        Stimulus::EndCall => SoftKey::EndCall,
        _ => return None,
    })
}

fn parking_menu_xml(
    instance: u32,
    transaction_id: u32,
    lot: &str,
    calls: &[ParkingMenuEntry],
) -> Result<String, ServerError> {
    if calls.len() > PARKING_MENU_MAX_ITEMS {
        return Err(PhoneXmlError::LimitExceeded {
            kind: "parking menu",
            actual: calls.len(),
            maximum: PARKING_MENU_MAX_ITEMS,
        }
        .into());
    }
    let items = calls
        .iter()
        .map(|call| {
            let party = if !call.caller_name.trim().is_empty() {
                call.caller_name.trim()
            } else if !call.caller_number.trim().is_empty() {
                call.caller_number.trim()
            } else {
                "Unknown caller"
            };
            let connected = if !call.connected_name.trim().is_empty() {
                format!(" to {}", call.connected_name.trim())
            } else if !call.connected_number.trim().is_empty() {
                format!(" to {}", call.connected_number.trim())
            } else {
                String::new()
            };
            CiscoIpPhoneMenuItem {
                name: Some(format!("{}: {}{}", call.slot, party, connected)),
                url: Some(format!(
                    "UserCallData:{}:{instance}:0:{transaction_id}:retrieve/{}/{}",
                    PARKING_APPLICATION_ID,
                    utf8_percent_encode(lot, NON_ALPHANUMERIC),
                    call.slot,
                )),
            }
        })
        .collect();
    CiscoIpPhoneMenu::new(
        format!("Parked calls - {lot}"),
        if calls.is_empty() {
            "No parked calls"
        } else {
            "Select a call"
        },
        items,
    )?
    .to_xml_with_limit(2_000)
    .map_err(ServerError::from)
}

fn text_service_messages(
    line_instance: LineInstance,
    call_reference: CallReference,
    transaction_id: TransactionId,
    priority: PhoneServicePriority,
    document: &CiscoIpPhoneText,
    protocol: ProtocolVersion,
) -> Result<Vec<ServerMessage>, ServerError> {
    if protocol <= ProtocolVersion::V17
        && document
            .text
            .as_deref()
            .is_some_and(|text| text.chars().count() > PHONE_TEXT_LEGACY_MAX_CHARS)
    {
        return Err(PhoneXmlError::InvalidField {
            field: "legacy phone text body",
            expected: "at most 1024 characters",
        }
        .into());
    }
    let maximum_bytes = if protocol <= ProtocolVersion::V17 {
        2_000
    } else {
        crate::phone::xml::PHONE_TEXT_MAX_BYTES
    };
    let xml = document.to_xml_with_limit(maximum_bytes)?.into_bytes();
    Ok(phone_service_document_messages(
        line_instance,
        call_reference,
        ApplicationId::new(PHONE_TEXT_APPLICATION_ID),
        transaction_id,
        priority,
        &xml,
    ))
}

fn input_service_messages(
    line_instance: LineInstance,
    call_reference: CallReference,
    application_id: ApplicationId,
    transaction_id: TransactionId,
    priority: PhoneServicePriority,
    document: &CiscoIpPhoneInput,
    protocol: ProtocolVersion,
) -> Result<Vec<ServerMessage>, ServerError> {
    let maximum_bytes = if protocol <= ProtocolVersion::V17 {
        2_000
    } else {
        PHONE_INPUT_MAX_BYTES
    };
    let xml = document.to_xml_with_limit(maximum_bytes)?.into_bytes();
    Ok(phone_service_document_messages(
        line_instance,
        call_reference,
        application_id,
        transaction_id,
        priority,
        &xml,
    ))
}

fn execute_phone_action_messages(
    line_instance: LineInstance,
    call_reference: CallReference,
    application_id: ApplicationId,
    transaction_id: TransactionId,
    priority: PhoneServicePriority,
    document: &CiscoIpPhoneExecute,
    protocol: ProtocolVersion,
) -> Result<Vec<ServerMessage>, ServerError> {
    let maximum_bytes = if protocol <= ProtocolVersion::V17 {
        2_000
    } else {
        PHONE_EXECUTE_MAX_BYTES
    };
    let xml = document.to_xml_with_limit(maximum_bytes)?.into_bytes();
    Ok(phone_service_document_messages(
        line_instance,
        call_reference,
        application_id,
        transaction_id,
        priority,
        &xml,
    ))
}

fn image_service_messages(
    line_instance: LineInstance,
    call_reference: CallReference,
    application_id: ApplicationId,
    transaction_id: TransactionId,
    priority: PhoneServicePriority,
    document: &PhoneImageDocument,
    protocol: ProtocolVersion,
) -> Result<Vec<ServerMessage>, ServerError> {
    let maximum_bytes = if protocol <= ProtocolVersion::V17 {
        2_000
    } else {
        PHONE_IMAGE_MAX_BYTES
    };
    let xml = document.to_xml_with_limit(maximum_bytes)?.into_bytes();
    Ok(phone_service_document_messages(
        line_instance,
        call_reference,
        application_id,
        transaction_id,
        priority,
        &xml,
    ))
}

fn status_service_messages(
    line_instance: LineInstance,
    call_reference: CallReference,
    application_id: ApplicationId,
    transaction_id: TransactionId,
    priority: PhoneServicePriority,
    document: &PhoneStatusDocument,
    protocol: ProtocolVersion,
) -> Result<Vec<ServerMessage>, ServerError> {
    let maximum_bytes = if protocol <= ProtocolVersion::V17 {
        2_000
    } else {
        PHONE_STATUS_MAX_BYTES
    };
    let xml = document.to_xml_with_limit(maximum_bytes)?.into_bytes();
    Ok(phone_service_document_messages(
        line_instance,
        call_reference,
        application_id,
        transaction_id,
        priority,
        &xml,
    ))
}

fn background_control_message(
    transaction_id: TransactionId,
    document: &PhoneBackgroundControlDocument,
) -> Result<ServerMessage, ServerError> {
    let xml = document.to_xml()?.into_bytes();
    let [message] = phone_service_document_messages(
        LineInstance::new(0),
        CallReference::new(0),
        ApplicationId::new(PHONE_BACKGROUND_APPLICATION_ID),
        transaction_id,
        PhoneServicePriority::LOW,
        &xml,
    )
    .try_into()
    .map_err(|_| PhoneXmlError::InvalidField {
        field: "background control document",
        expected: "a single application-data frame",
    })?;
    Ok(message)
}

fn ringtone_control_message(
    transaction_id: TransactionId,
    document: &CiscoIpPhoneSetRingTone,
) -> Result<ServerMessage, ServerError> {
    let xml = document.to_xml()?.into_bytes();
    let [message] = phone_service_document_messages(
        LineInstance::new(0),
        CallReference::new(0),
        ApplicationId::new(PHONE_RINGTONE_APPLICATION_ID),
        transaction_id,
        PhoneServicePriority::LOW,
        &xml,
    )
    .try_into()
    .map_err(|_| PhoneXmlError::InvalidField {
        field: "ringtone control document",
        expected: "a single application-data frame",
    })?;
    Ok(message)
}

#[cfg(test)]
fn start_announcement_message(
    conference_id: ConferenceId,
    announcements: Vec<AnnouncementEntry>,
    end_of_ack: bool,
    participant_ids: Vec<ParticipantId>,
    hearing_participant_mask: u32,
    play_mode: u32,
) -> ServerMessage {
    ServerMessage::StartAnnouncement {
        announcements,
        end_of_ack: u32::from(end_of_ack),
        conference_id: conference_id.get(),
        matrix_conference_party_ids: participant_ids
            .into_iter()
            .map(ParticipantId::get)
            .collect(),
        hearing_conference_party_mask: hearing_participant_mask,
        play_mode,
    }
}

fn phone_service_document_messages(
    line_instance: LineInstance,
    call_reference: CallReference,
    application_id: ApplicationId,
    transaction_id: TransactionId,
    priority: PhoneServicePriority,
    xml: &[u8],
) -> Vec<ServerMessage> {
    let chunks = xml.chunks(2_000);
    let chunk_count = chunks.len();
    chunks
        .enumerate()
        .map(|(index, data)| {
            let sequence_flag = if chunk_count == 1 || index + 1 == chunk_count {
                2
            } else if index == 0 {
                0
            } else {
                1
            };
            ServerMessage::UserToDeviceDataV1(UserDataV1Message {
                application_id: application_id.get(),
                line_instance: line_instance.get(),
                call_reference: call_reference.get(),
                transaction_id: transaction_id.get(),
                sequence_flag,
                display_priority: priority.wire(),
                conference_id: call_reference.get(),
                application_instance_id: application_id.get(),
                routing: 1,
                data: data.to_vec(),
            })
        })
        .collect()
}

async fn handle_phone_service_message(
    state: &mut SessionState,
    context: &SessionContext,
    message_id: u32,
    kind: PhoneServiceMessageKind,
    routing: PhoneServiceRouting,
    extended: Option<PhoneServiceExtendedRouting>,
    data: &[u8],
) -> Result<(), ServerError> {
    let payload = match parse_phone_service_payload(data, kind) {
        Ok(payload) => payload,
        Err(error) => {
            warn!(
                device_id = %state.device.id,
                message_id = format_args!("0x{message_id:04x}"),
                %error,
                "ignoring malformed phone-service response"
            );
            context
                .event_tx
                .send(Event::ProtocolWarning {
                    peer: context.peer,
                    device_id: Some(state.device.id.clone()),
                    message_id,
                    error: error.to_string(),
                })
                .await
                .map_err(|_| ServerError::Stopped)?;
            return Ok(());
        }
    };
    let response = PhoneServiceEvent {
        kind,
        routing,
        extended,
        payload,
    };

    if let Some((lot, slot)) = parking_menu_selection(state.pending_parking_menu, &response) {
        state.pending_parking_menu = None;
        context
            .event_tx
            .send(Event::device(
                state.device.id.clone(),
                state.generation,
                DeviceEventKind::ParkingMenuSelection { lot, slot },
            ))
            .await
            .map_err(|_| ServerError::Stopped)?;
    }
    if response.kind == PhoneServiceMessageKind::Data
        && response.routing.application_id.get() == ConferenceListAction::APPLICATION_ID
        && let PhoneServicePayload::Submission(submission) = &response.payload
        && let Some(action) = ConferenceListAction::from_route(&submission.route)
    {
        context
            .event_tx
            .send(Event::device(
                state.device.id.clone(),
                state.generation,
                DeviceEventKind::ConferenceListAction { action },
            ))
            .await
            .map_err(|_| ServerError::Stopped)?;
    }
    context
        .event_tx
        .send(Event::device(
            state.device.id.clone(),
            state.generation,
            DeviceEventKind::PhoneServiceResponse { response },
        ))
        .await
        .map_err(|_| ServerError::Stopped)
}

fn parking_menu_selection(
    pending: Option<PendingParkingMenu>,
    response: &PhoneServiceEvent,
) -> Option<(String, u32)> {
    let pending = pending?;
    if response.kind != PhoneServiceMessageKind::Data
        || response.routing.application_id.get() != PARKING_APPLICATION_ID
        || response.routing.line_instance.get() != pending.instance
        || response.routing.call_reference.get() != 0
        || response.routing.transaction_id.get() != pending.transaction_id
        || response
            .extended
            .is_some_and(|extended| extended.application_instance_id != pending.instance)
    {
        return None;
    }
    let PhoneServicePayload::Submission(submission) = &response.payload else {
        return None;
    };
    let [action, lot, slot] = submission.route.as_slice() else {
        return None;
    };
    if action != "retrieve" || lot.is_empty() || !submission.values.is_empty() {
        return None;
    }
    let slot = slot.parse().ok()?;
    (slot != 0).then(|| (lot.clone(), slot))
}

fn digit_character(digit: Digit) -> Option<char> {
    match digit {
        Digit::Number(number @ 0..=9) => Some(char::from(b'0' + number)),
        Digit::Star => Some('*'),
        Digit::Pound => Some('#'),
        Digit::A => Some('A'),
        Digit::B => Some('B'),
        Digit::C => Some('C'),
        Digit::D => Some('D'),
        Digit::Number(_) | Digit::Unknown(_) => None,
    }
}

fn normalized_last_number(number: &str, config: &ServerConfig) -> Option<String> {
    let number = number.trim();
    let number = if config.record_dial_terminator {
        number
    } else {
        digit_character(config.dial_terminator)
            .map_or(number, |terminator| number.trim_end_matches(terminator))
    };
    (!number.is_empty()).then(|| number.to_owned())
}

fn remember_last_number(
    state: &mut SessionState,
    line_instance: u32,
    number: &str,
    config: &ServerConfig,
) {
    if let Some(number) = normalized_last_number(number, config) {
        state.last_number_by_line.insert(line_instance, number);
    }
}

async fn begin_redial(
    stream: &mut dyn StationIo,
    state: &mut SessionState,
    context: &SessionContext,
    line_instance: u32,
    existing_call_id: Option<CallId>,
) -> Result<(), ServerError> {
    if state.device.ui.placed_calls_redial_menu
        && placed_calls_menu_supported(state.registration.protocol)
    {
        let document = CiscoIpPhoneExecute::new(vec![CiscoIpPhoneExecuteItem::new(
            "Application:PlacedCalls",
        )?])?;
        for message in execute_phone_action_messages(
            LineInstance::new(line_instance),
            CallReference::new(0),
            ApplicationId::new(0),
            TransactionId::new(0),
            PhoneServicePriority::NORMAL,
            &document,
            state.registration.protocol,
        )? {
            send_message(stream, &message, state.registration.protocol).await?;
        }
        return Ok(());
    }

    let Some(number) = state.last_number_by_line.get(&line_instance).cloned() else {
        return Ok(());
    };
    let existing = existing_call_id.and_then(|call_id| {
        state
            .calls_by_id
            .get(&call_id)
            .filter(|call| call.line_instance == line_instance && call.state != CallState::OnHook)
            .cloned()
    });
    let (call, created) = existing.map_or_else(
        || {
            (
                ensure_phone_call(state, 0, line_instance, &context.next_call_id),
                true,
            )
        },
        |call| (call, false),
    );

    if created {
        state.active_key_mode = KeyMode::OffHook;
        begin_phone_call_ui(stream, &call, &state.device, state.station_context()).await?;
        context
            .event_tx
            .send(Event::device(
                state.device.id.clone(),
                state.generation,
                DeviceEventKind::OffHook {
                    call_id: call.call_id,
                    line_instance: LineInstance::new(line_instance),
                },
            ))
            .await
            .map_err(|_| ServerError::Stopped)?;
    }
    if let Some(stored) = state.calls_by_id.get_mut(&call.call_id) {
        stored.dialed_number.clone_from(&number);
    }
    send_message(
        stream,
        &ServerMessage::DialedNumber {
            number: number.clone(),
            line_instance,
            call_reference: call.wire_reference,
        },
        state.registration.protocol,
    )
    .await?;
    context
        .event_tx
        .send(Event::device(
            state.device.id.clone(),
            state.generation,
            DeviceEventKind::EnblocCall {
                call_id: call.call_id,
                line_instance: LineInstance::new(line_instance),
                number,
            },
        ))
        .await
        .map_err(|_| ServerError::Stopped)?;
    Ok(())
}

fn placed_calls_menu_supported(protocol: ProtocolVersion) -> bool {
    protocol >= ProtocolVersion::V8
}

async fn handle_session_command(
    stream: &mut dyn StationIo,
    state: &mut SessionState,
    command: SessionCommand,
    context: &SessionContext,
) -> Result<bool, ServerError> {
    let config = &context.config;
    let protocol = state.registration.protocol;
    match command {
        SessionCommand::Confirmed { .. } => {
            unreachable!("confirmed commands are unwrapped by the session loop")
        }
        SessionCommand::Disconnect => {
            drain_session_media(stream, state).await;
            return Ok(true);
        }
        SessionCommand::OfferIncoming {
            line_instance,
            call_id,
            info,
            ringer,
        } => {
            if state.cancelled_calls.remove(&call_id) {
                debug!(device_id = %state.device.id, ?call_id, "discarding incoming call cancelled before it was offered");
                return Ok(false);
            }
            let line_instance = normalize_line(state, line_instance.get());
            let statistics_directory_number = statistics_directory_for_call_info(&info).to_owned();
            let caller = match (
                info.calling_name.trim().is_empty(),
                info.calling_number.trim().is_empty(),
            ) {
                (false, false) => format!("{} ({})", info.calling_name, info.calling_number),
                (false, true) => info.calling_name.clone(),
                (true, false) => info.calling_number.clone(),
                (true, true) => "Unknown number".to_owned(),
            };
            let incoming_state = if state.calls_by_id.values().any(|call| {
                matches!(
                    call.state,
                    CallState::Connected
                        | CallState::Hold
                        | CallState::HoldYellow
                        | CallState::HoldRed
                )
            }) {
                CallState::CallWaiting
            } else {
                CallState::RingIn
            };
            let call = insert_call(state, call_id, line_instance, Codec::Pcmu, incoming_state);
            if incoming_state == CallState::RingIn && state.active_call_id.is_none() {
                state.active_call_id = Some(call.call_id);
            }
            if let Some(stored) = state.calls_by_id.get_mut(&call.call_id) {
                stored.statistics_directory_number = statistics_directory_number;
            }
            send_message(
                stream,
                &ServerMessage::ClearPrompt {
                    line_instance,
                    call_reference: call.wire_reference,
                },
                protocol,
            )
            .await?;
            send_message(
                stream,
                &ServerMessage::CallState {
                    state: incoming_state,
                    line_instance,
                    call_reference: call.wire_reference,
                },
                protocol,
            )
            .await?;
            send_station_ui_message(
                stream,
                state,
                &ServerMessage::CallInfo {
                    info: *info,
                    line_instance,
                    call_reference: call.wire_reference,
                },
            )
            .await?;
            send_message(
                stream,
                &ServerMessage::SetLamp {
                    stimulus: ButtonType::Line,
                    instance: line_instance,
                    mode: LampMode::Blink,
                },
                protocol,
            )
            .await?;
            if let Some(ringer) = incoming_ringer(ringer, incoming_state) {
                send_message(
                    stream,
                    &ServerMessage::SetRinger {
                        mode: ringer.mode,
                        duration: ringer.duration,
                        line_instance,
                        call_reference: call.wire_reference,
                    },
                    protocol,
                )
                .await?;
            }
            state.active_key_mode = KeyMode::RingIn;
            send_message(
                stream,
                &ServerMessage::SelectSoftKeys {
                    line_instance,
                    call_reference: call.wire_reference,
                    set: KeyMode::RingIn,
                    valid_mask: state.device.soft_keys.valid_mask(KeyMode::RingIn),
                },
                protocol,
            )
            .await?;
            send_station_ui_message(
                stream,
                state,
                &ServerMessage::DisplayPrompt {
                    timeout_seconds: 0,
                    text: format!("From {caller}"),
                    line_instance,
                    call_reference: call.wire_reference,
                },
            )
            .await?;
        }
        SessionCommand::Public(command) => {
            let command = *command;
            if let Some(call_id) = command_call_id(&command)
                && !matches!(
                    &command.action,
                    CommandAction::BeginCall { .. } | CommandAction::CloseCall { .. }
                )
                && !state.calls_by_id.contains_key(&call_id)
            {
                debug!(device_id = %state.device.id, ?call_id, command = ?command, "ignoring stale SCCP call command");
                return Ok(false);
            }
            let action = command.action;
            match action {
                CommandAction::DisconnectDevice { .. } => {
                    drain_session_media(stream, state).await;
                    return Ok(true);
                }
                CommandAction::BeginCall {
                    line_instance,
                    call_id,
                    codec,
                } => {
                    if state.calls_by_id.contains_key(&call_id) {
                        return Ok(false);
                    }
                    let line_instance = normalize_line(state, line_instance.get());
                    let call =
                        insert_call(state, call_id, line_instance, codec, CallState::OffHook);
                    state.active_call_id = Some(call.call_id);
                    state.active_key_mode = KeyMode::OffHook;
                    begin_phone_call_ui(stream, &call, &state.device, state.station_context())
                        .await?;
                }
                CommandAction::BeginTransfer {
                    source_call_id,
                    consultation_line_instance,
                    consultation_call_id,
                    codec,
                } => {
                    let consultation_line_instance = consultation_line_instance.get();
                    if state.calls_by_id.contains_key(&consultation_call_id) {
                        return Ok(false);
                    }
                    let source = require_call_mut(state, source_call_id)?;
                    if !matches!(
                        source.state,
                        CallState::Hold | CallState::HoldYellow | CallState::HoldRed
                    ) {
                        return Err(ServerError::InvalidCallTransaction {
                            call_id: source_call_id,
                            operation: "begin transfer",
                            state: source.state,
                        });
                    }
                    source.state = CallState::Transfer;
                    source.transfer_role = Some(SessionTransferRole::Source {
                        consultation_call_id,
                    });
                    let source = source.clone();
                    send_message(
                        stream,
                        &ServerMessage::CallState {
                            state: CallState::Transfer,
                            line_instance: source.line_instance,
                            call_reference: source.wire_reference,
                        },
                        protocol,
                    )
                    .await?;
                    send_station_ui_message(
                        stream,
                        state,
                        &ServerMessage::DisplayPrompt {
                            timeout_seconds: 0,
                            text: "Call Transfer".into(),
                            line_instance: source.line_instance,
                            call_reference: source.wire_reference,
                        },
                    )
                    .await?;

                    let line_instance = normalize_line(state, consultation_line_instance);
                    let mut consultation = insert_call(
                        state,
                        consultation_call_id,
                        line_instance,
                        codec,
                        CallState::OffHook,
                    );
                    consultation.transfer_role =
                        Some(SessionTransferRole::Consultation { source_call_id });
                    state
                        .calls_by_id
                        .insert(consultation_call_id, consultation.clone());
                    state.active_call_id = Some(consultation.call_id);
                    state.active_key_mode = KeyMode::OffHookFeature;
                    begin_phone_call_ui_with_key_mode(
                        stream,
                        &consultation,
                        &state.device,
                        KeyMode::OffHookFeature,
                        state.station_context(),
                    )
                    .await?;
                    send_message(
                        stream,
                        &ServerMessage::SetLamp {
                            stimulus: ButtonType::Transfer,
                            instance: source.line_instance,
                            mode: LampMode::Flash,
                        },
                        protocol,
                    )
                    .await?;
                }
                CommandAction::SetCallSelected {
                    call_id, selected, ..
                } => {
                    let call = require_call(state, call_id)?.clone();
                    send_message(
                        stream,
                        &ServerMessage::CallSelectStatus {
                            status: u32::from(selected),
                            call_reference: call.wire_reference,
                            line_instance: call.line_instance,
                        },
                        protocol,
                    )
                    .await?;
                }
                CommandAction::SetMwi {
                    line_instance,
                    enabled,
                    ..
                } => {
                    let line_instance = line_instance.get();
                    state.mwi_by_line.insert(line_instance, enabled);
                    send_mwi_lamp(stream, state, line_instance, enabled, protocol).await?;
                }
                CommandAction::SetForwardStatus {
                    line_instance,
                    forward_all,
                    forward_busy,
                    forward_no_answer,
                    ..
                } => {
                    let line_instance = line_instance.get();
                    state.forwarding_by_line.insert(
                        line_instance,
                        SessionForwarding {
                            all: forward_all.clone(),
                            busy: forward_busy.clone(),
                            no_answer: forward_no_answer.clone(),
                        },
                    );
                    send_message(
                        stream,
                        &ServerMessage::ForwardStatus {
                            line_instance,
                            forward_all,
                            forward_busy,
                            forward_no_answer,
                        },
                        protocol,
                    )
                    .await?;
                }
                CommandAction::SetFeatureStatus {
                    instance, enabled, ..
                } => {
                    let instance = instance.get();
                    if let Some(messages) = feature_state_messages(&state.device, instance, enabled)
                    {
                        let ServerMessage::FeatureStatus {
                            button_type,
                            state: feature_state,
                            ..
                        } = &messages[0]
                        else {
                            unreachable!("feature status starts with a feature-state message")
                        };
                        state.feature_states.insert(
                            instance,
                            SessionFeatureState {
                                button_type: *button_type,
                                state: *feature_state,
                            },
                        );
                        for message in messages {
                            send_station_ui_message(stream, state, &message).await?;
                        }
                    }
                }
                CommandAction::SetDoNotDisturbStatus {
                    instance,
                    mode,
                    button_mode,
                    ..
                } => {
                    let instance = instance.get();
                    if let Some(messages) = do_not_disturb_state_messages(
                        &state.device,
                        instance,
                        mode,
                        button_mode,
                        protocol,
                    ) {
                        let ServerMessage::FeatureStatus {
                            button_type,
                            state: feature_state,
                            ..
                        } = &messages[0]
                        else {
                            unreachable!("DND status starts with a feature-state message")
                        };
                        state.feature_states.insert(
                            instance,
                            SessionFeatureState {
                                button_type: *button_type,
                                state: *feature_state,
                            },
                        );
                        for message in messages {
                            send_station_ui_message(stream, state, &message).await?;
                        }
                    }
                }
                CommandAction::SetMobilityAppearance {
                    mobility_instance,
                    appearance,
                    ..
                } => {
                    let mobility_instance = mobility_instance.get();
                    let configured = state.device.buttons.iter().any(|button| {
                        matches!(
                            button,
                            ButtonDefinition::Feature(feature)
                                if feature.instance == mobility_instance
                                    && feature.feature == ButtonType::Mobility
                        )
                    });
                    if !configured {
                        return Err(CodecError::InvalidDefinition(format!(
                            "device {} has no mobility button instance {mobility_instance}",
                            state.device.id
                        ))
                        .into());
                    }
                    let previous = state.mobility_appearances.get(&mobility_instance).cloned();
                    let mut next_appearances = state.mobility_appearances.clone();
                    match &appearance {
                        Some(appearance) => {
                            next_appearances.insert(mobility_instance, appearance.clone());
                        }
                        None => {
                            next_appearances.remove(&mobility_instance);
                        }
                    }
                    let candidate = mobility_device_candidate(
                        &state.device,
                        &state.mobility_appearances,
                        &next_appearances,
                    )?;

                    send_message(
                        stream,
                        &ServerMessage::ButtonTemplate {
                            buttons: button_template(&candidate),
                        },
                        protocol,
                    )
                    .await?;
                    if let Some(appearance) = &appearance {
                        if let Some(message) = line_status(&candidate, appearance.instance) {
                            send_station_ui_message(stream, state, &message).await?;
                        }
                    } else if let Some(previous) = &previous {
                        send_station_ui_message(
                            stream,
                            state,
                            &ServerMessage::LineStatus {
                                instance: previous.instance,
                                number: String::new(),
                                display_name: String::new(),
                            },
                        )
                        .await?;
                    }
                    state.device = candidate;
                    state.mobility_appearances = next_appearances;
                }
                CommandAction::SetBlfStatus {
                    instance,
                    number,
                    label,
                    state: blf_state,
                    caller,
                    ..
                } => {
                    let instance = instance.get();
                    for message in
                        blf_status_messages(instance, &number, &label, blf_state, caller.as_ref())
                    {
                        send_station_ui_message(stream, state, &message).await?;
                    }
                    if let Some(notification) = hinted_ringing_notification(
                        &state.device,
                        &label,
                        caller.as_ref(),
                        blf_state,
                    ) {
                        for message in status_message_frames(
                            notification,
                            state.registration.device_type,
                            &mut state.persistent_status_message,
                        ) {
                            send_station_ui_message(stream, state, &message).await?;
                        }
                    }
                }
                CommandAction::ShowParkingMenu {
                    instance,
                    transaction_id,
                    lot,
                    calls,
                    ..
                } => {
                    let instance = instance.get();
                    let transaction_id = transaction_id.get();
                    send_message(
                        stream,
                        &ServerMessage::UserToDeviceDataV1(UserDataV1Message {
                            application_id: PARKING_APPLICATION_ID,
                            line_instance: instance,
                            call_reference: 0,
                            transaction_id,
                            sequence_flag: 0,
                            display_priority: 2,
                            conference_id: 0,
                            application_instance_id: instance,
                            routing: 0,
                            data: parking_menu_xml(instance, transaction_id, &lot, &calls)?
                                .into_bytes(),
                        }),
                        protocol,
                    )
                    .await?;
                    state.pending_parking_menu = Some(PendingParkingMenu {
                        instance,
                        transaction_id,
                    });
                }
                CommandAction::ShowConferenceList {
                    call_id,
                    conference_id,
                    participants,
                    ..
                } => {
                    let call = require_call(state, call_id)?.clone();
                    let family = if protocol >= ProtocolVersion::V8 {
                        ConferenceMenuFamily::IconMenu
                    } else {
                        ConferenceMenuFamily::Menu
                    };
                    let data = ConferenceListDocument::new(conference_id, &participants, family)?
                        .to_xml()?
                        .into_bytes();
                    send_message(
                        stream,
                        &ServerMessage::UserToDeviceDataV1(UserDataV1Message {
                            application_id: ConferenceListAction::APPLICATION_ID,
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                            transaction_id: conference_id.get(),
                            sequence_flag: 0,
                            display_priority: 2,
                            conference_id: conference_id.get(),
                            application_instance_id: call.line_instance,
                            routing: 0,
                            data,
                        }),
                        protocol,
                    )
                    .await?;
                }
                CommandAction::ShowConferenceParticipantActions {
                    call_id,
                    conference_id,
                    participant,
                    removable,
                    demotable,
                    ..
                } => {
                    let call = require_call(state, call_id)?.clone();
                    let family = if protocol >= ProtocolVersion::V8 {
                        ConferenceMenuFamily::IconMenu
                    } else {
                        ConferenceMenuFamily::Menu
                    };
                    let data = ConferenceParticipantActionsDocument::new(
                        conference_id,
                        &participant,
                        removable,
                        demotable,
                        family,
                    )?
                    .to_xml()?
                    .into_bytes();
                    send_message(
                        stream,
                        &ServerMessage::UserToDeviceDataV1(UserDataV1Message {
                            application_id: ConferenceListAction::APPLICATION_ID,
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                            transaction_id: conference_id.get(),
                            sequence_flag: 0,
                            display_priority: 2,
                            conference_id: conference_id.get(),
                            application_instance_id: call.line_instance,
                            routing: 0,
                            data,
                        }),
                        protocol,
                    )
                    .await?;
                }
                CommandAction::ShowTextService {
                    line_instance,
                    call_reference,
                    transaction_id,
                    priority,
                    document,
                    ..
                } => {
                    for message in text_service_messages(
                        line_instance,
                        call_reference,
                        transaction_id,
                        priority,
                        &document,
                        protocol,
                    )? {
                        send_message(stream, &message, protocol).await?;
                    }
                }
                CommandAction::ShowInputService {
                    line_instance,
                    call_reference,
                    application_id,
                    transaction_id,
                    priority,
                    document,
                    ..
                } => {
                    for message in input_service_messages(
                        line_instance,
                        call_reference,
                        application_id,
                        transaction_id,
                        priority,
                        &document,
                        protocol,
                    )? {
                        send_message(stream, &message, protocol).await?;
                    }
                }
                CommandAction::ExecutePhoneActions {
                    line_instance,
                    call_reference,
                    application_id,
                    transaction_id,
                    priority,
                    document,
                    ..
                } => {
                    for message in execute_phone_action_messages(
                        line_instance,
                        call_reference,
                        application_id,
                        transaction_id,
                        priority,
                        &document,
                        protocol,
                    )? {
                        send_message(stream, &message, protocol).await?;
                    }
                }
                CommandAction::ShowImageService {
                    line_instance,
                    call_reference,
                    application_id,
                    transaction_id,
                    priority,
                    document,
                    ..
                } => {
                    for message in image_service_messages(
                        line_instance,
                        call_reference,
                        application_id,
                        transaction_id,
                        priority,
                        &document,
                        protocol,
                    )? {
                        send_message(stream, &message, protocol).await?;
                    }
                }
                CommandAction::ShowStatusService {
                    line_instance,
                    call_reference,
                    application_id,
                    transaction_id,
                    priority,
                    document,
                    ..
                } => {
                    for message in status_service_messages(
                        line_instance,
                        call_reference,
                        application_id,
                        transaction_id,
                        priority,
                        &document,
                        protocol,
                    )? {
                        send_message(stream, &message, protocol).await?;
                    }
                }
                CommandAction::SetBackgroundImage {
                    transaction_id,
                    document,
                    ..
                } => {
                    let message = background_control_message(
                        transaction_id,
                        &PhoneBackgroundControlDocument::Set(document),
                    )?;
                    send_message(stream, &message, protocol).await?;
                }
                CommandAction::PreviewBackgroundImage {
                    transaction_id,
                    document,
                    ..
                } => {
                    let message = background_control_message(
                        transaction_id,
                        &PhoneBackgroundControlDocument::Preview(document),
                    )?;
                    send_message(stream, &message, protocol).await?;
                }
                CommandAction::SetRingtone {
                    transaction_id,
                    document,
                    ..
                } => {
                    let message = ringtone_control_message(transaction_id, &document)?;
                    send_message(stream, &message, protocol).await?;
                }
                CommandAction::StartTone { call_id, tone, .. } => {
                    let call = require_call(state, call_id)?.clone();
                    let message = if tone == Tone::Silence {
                        ServerMessage::StopTone {
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                        }
                    } else {
                        ServerMessage::StartTone {
                            tone,
                            direction: ToneDirection::User,
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                        }
                    };
                    send_message(stream, &message, protocol).await?;
                }
                CommandAction::StartAnnouncement {
                    conference_id,
                    announcements,
                    end_of_ack,
                    participant_ids,
                    hearing_participant_mask,
                    play_mode,
                    ..
                } => {
                    let _ = (
                        conference_id,
                        announcements,
                        end_of_ack,
                        participant_ids,
                        hearing_participant_mask,
                        play_mode,
                    );
                    return Err(ServerError::InvalidStationCommand {
                        message: "StartAnnouncement",
                    });
                }
                CommandAction::StopAnnouncement { conference_id, .. } => {
                    let _ = conference_id;
                    return Err(ServerError::InvalidStationCommand {
                        message: "StopAnnouncement",
                    });
                }
                CommandAction::AnnouncementFinish {
                    conference_id,
                    play_status,
                    ..
                } => {
                    let _ = (conference_id, play_status);
                    return Err(ServerError::InvalidStationCommand {
                        message: "AnnouncementFinish",
                    });
                }
                CommandAction::SetCallInfo { call_id, info, .. } => {
                    let statistics_directory_number =
                        statistics_directory_for_call_info(&info).to_owned();
                    if let Some(stored) = state.calls_by_id.get_mut(&call_id) {
                        stored.statistics_directory_number = statistics_directory_number;
                    }
                    let call = require_call(state, call_id)?.clone();
                    send_station_ui_message(
                        stream,
                        state,
                        &ServerMessage::CallInfo {
                            info,
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                        },
                    )
                    .await?;
                }
                CommandAction::CommitOutboundCall { call_id, info, .. } => {
                    let statistics_directory_number =
                        statistics_directory_for_call_info(&info).to_owned();
                    let call = require_call_mut(state, call_id)?;
                    call.state = CallState::Proceed;
                    call.history_disposition =
                        updated_history_disposition(call.history_disposition, CallState::Proceed);
                    call.statistics_directory_number = statistics_directory_number;
                    let call = call.clone();
                    let number = digit_character(config.dial_terminator)
                        .and_then(|terminator| call.dialed_number.strip_suffix(terminator))
                        .unwrap_or(&call.dialed_number)
                        .to_owned();
                    remember_last_number(state, call.line_instance, &number, config);
                    state.active_call_id = Some(call.call_id);
                    refresh_mwi_lamps(stream, state, protocol).await?;
                    for message in [
                        ServerMessage::StopTone {
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                        },
                        ServerMessage::SetLamp {
                            stimulus: ButtonType::Line,
                            instance: call.line_instance,
                            mode: LampMode::Blink,
                        },
                        ServerMessage::CallInfo {
                            info,
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                        },
                        ServerMessage::DialedNumber {
                            number,
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                        },
                        ServerMessage::CallState {
                            state: CallState::Proceed,
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                        },
                    ] {
                        send_station_ui_message(stream, state, &message).await?;
                    }
                }
                CommandAction::PresentOutboundProceeding { call_id, info, .. } => {
                    let statistics_directory_number =
                        statistics_directory_for_call_info(&info).to_owned();
                    let call = require_call_mut(state, call_id)?;
                    call.state = CallState::Proceed;
                    call.history_disposition =
                        updated_history_disposition(call.history_disposition, CallState::Proceed);
                    call.statistics_directory_number = statistics_directory_number;
                    let call = call.clone();
                    state.active_call_id = Some(call.call_id);
                    refresh_mwi_lamps(stream, state, protocol).await?;
                    for message in [
                        ServerMessage::StopTone {
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                        },
                        ServerMessage::CallState {
                            state: CallState::Proceed,
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                        },
                        ServerMessage::CallInfo {
                            info,
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                        },
                        ServerMessage::DisplayPrompt {
                            timeout_seconds: 0,
                            text: "Call Proceed".into(),
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                        },
                    ] {
                        send_station_ui_message(stream, state, &message).await?;
                    }
                }
                CommandAction::PresentOutboundRinging { call_id, info, .. } => {
                    let statistics_directory_number =
                        statistics_directory_for_call_info(&info).to_owned();
                    let call = require_call_mut(state, call_id)?;
                    call.state = CallState::Proceed;
                    call.history_disposition =
                        updated_history_disposition(call.history_disposition, CallState::Proceed);
                    call.statistics_directory_number = statistics_directory_number;
                    let call = call.clone();
                    state.active_call_id = Some(call.call_id);
                    let key_mode = transfer_key_mode(&call, CallState::RingOut);
                    state.active_key_mode = key_mode;
                    refresh_mwi_lamps(stream, state, protocol).await?;
                    for message in [
                        ServerMessage::CallState {
                            state: CallState::Proceed,
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                        },
                        ServerMessage::DisplayPrompt {
                            timeout_seconds: 0,
                            text: "Ring out".into(),
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                        },
                        ServerMessage::StartTone {
                            tone: Tone::Alerting,
                            direction: ToneDirection::User,
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                        },
                        ServerMessage::SelectSoftKeys {
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                            set: key_mode,
                            valid_mask: state.device.soft_keys.valid_mask(key_mode),
                        },
                        ServerMessage::CallInfo {
                            info,
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                        },
                    ] {
                        send_station_ui_message(stream, state, &message).await?;
                    }
                }
                CommandAction::SetCallState {
                    call_id,
                    state: call_state,
                    ..
                } => {
                    let transfer_source_to_clear =
                        state
                            .calls_by_id
                            .get(&call_id)
                            .and_then(|call| match call.transfer_role {
                                Some(SessionTransferRole::Source {
                                    consultation_call_id,
                                }) if call_state != CallState::Transfer => {
                                    Some((consultation_call_id, call.line_instance))
                                }
                                _ => None,
                            });
                    let call = require_call_mut(state, call_id)?;
                    call.state = call_state;
                    call.history_disposition =
                        updated_history_disposition(call.history_disposition, call_state);
                    let call = call.clone();
                    if matches!(
                        call_state,
                        CallState::Proceed | CallState::RingOut | CallState::Connected
                    ) {
                        remember_last_number(
                            state,
                            call.line_instance,
                            &call.dialed_number,
                            config,
                        );
                    }
                    prepare_call_state_ui(stream, &call, call_state, protocol).await?;
                    send_message(
                        stream,
                        &ServerMessage::CallState {
                            state: call_state,
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                        },
                        protocol,
                    )
                    .await?;
                    finish_call_state_ui(stream, &call, call_state, state.station_context())
                        .await?;
                    let set = transfer_key_mode(&call, call_state);
                    state.active_key_mode = set;
                    match call_state {
                        CallState::Connected
                        | CallState::OffHook
                        | CallState::Transfer
                        | CallState::RingOut
                        | CallState::Proceed
                        | CallState::IntercomOneWay => {
                            state.active_call_id = Some(call.call_id);
                        }
                        CallState::OnHook
                        | CallState::Hold
                        | CallState::HoldYellow
                        | CallState::HoldRed
                            if state.active_call_id == Some(call.call_id) =>
                        {
                            state.active_call_id = None;
                        }
                        _ => {}
                    }
                    refresh_mwi_lamps(stream, state, protocol).await?;
                    send_message(
                        stream,
                        &ServerMessage::SelectSoftKeys {
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                            set,
                            valid_mask: state.device.soft_keys.valid_mask(set),
                        },
                        protocol,
                    )
                    .await?;
                    if let Some((consultation_call_id, line_instance)) = transfer_source_to_clear {
                        if let Some(source) = state.calls_by_id.get_mut(&call_id) {
                            source.transfer_role = None;
                        }
                        if let Some(consultation) = state.calls_by_id.get_mut(&consultation_call_id)
                        {
                            consultation.transfer_role = None;
                        }
                        send_message(
                            stream,
                            &ServerMessage::SetLamp {
                                stimulus: ButtonType::Transfer,
                                instance: line_instance,
                                mode: LampMode::Off,
                            },
                            protocol,
                        )
                        .await?;
                    }
                }
                CommandAction::DisplayPrompt {
                    call_id,
                    timeout_seconds,
                    text,
                    ..
                } => {
                    let call = require_call(state, call_id)?.clone();
                    send_station_ui_message(
                        stream,
                        state,
                        &ServerMessage::DisplayPrompt {
                            timeout_seconds,
                            text,
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                        },
                    )
                    .await?;
                }
                CommandAction::ClearPrompt { call_id, .. } => {
                    let call = require_call(state, call_id)?.clone();
                    send_message(
                        stream,
                        &ServerMessage::ClearPrompt {
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                        },
                        protocol,
                    )
                    .await?;
                }
                CommandAction::SetStatusMessage { message, beep, .. } => {
                    let frames = status_message_frames(
                        message,
                        state.registration.device_type,
                        &mut state.persistent_status_message,
                    );
                    for frame in frames {
                        send_station_ui_message(stream, state, &frame).await?;
                    }
                    if beep {
                        send_message(
                            stream,
                            &ServerMessage::StartTone {
                                tone: Tone::ZipZip,
                                direction: ToneDirection::User,
                                line_instance: 0,
                                call_reference: 0,
                            },
                            protocol,
                        )
                        .await?;
                    }
                }
                CommandAction::SetMicrophoneMode { enabled, .. } => {
                    send_message(
                        stream,
                        &ServerMessage::SetMicrophoneMode(if enabled {
                            MicrophoneMode::On
                        } else {
                            MicrophoneMode::Off
                        }),
                        protocol,
                    )
                    .await?;
                }
                CommandAction::SetRecordingStatus {
                    call_id, active, ..
                } => {
                    let call = require_call(state, call_id)?.clone();
                    send_message(
                        stream,
                        &ServerMessage::RecordingStatus {
                            call_reference: call.wire_reference,
                            active,
                        },
                        protocol,
                    )
                    .await?;
                }
                CommandAction::ResetDevice { reset_type, .. } => {
                    send_message(stream, &ServerMessage::Reset(reset_type), protocol).await?;
                }
                ringing @ (CommandAction::StartRinging { call_id }
                | CommandAction::StopRinging { call_id }) => {
                    let enabled = matches!(ringing, CommandAction::StartRinging { .. });
                    let call = require_call(state, call_id)?.clone();
                    send_message(
                        stream,
                        &ServerMessage::SetRinger {
                            mode: if enabled {
                                RingerMode::Inside
                            } else {
                                RingerMode::Off
                            },
                            duration: RingDuration::Normal,
                            line_instance: call.line_instance,
                            call_reference: call.wire_reference,
                        },
                        protocol,
                    )
                    .await?;
                }
                CommandAction::OpenReceiveChannel {
                    call_id,
                    source,
                    codec,
                    packet_ms,
                    max_frames_per_packet,
                    dtmf_mode,
                    audio_processing,
                    ..
                } => {
                    let telephone_event_payload = dtmf_mode.telephone_event_payload(state.features);
                    let request = allocate_media_request_identity(state, call_id)?;
                    let call = require_call_mut(state, call_id)?;
                    call.media.requested = true;
                    call.media.codec = codec;
                    call.media.packet_ms = packet_ms;
                    call.media.max_frames_per_packet = max_frames_per_packet;
                    call.media.receive.telephone_event_payload = telephone_event_payload;
                    call.media.receive.peer = None;
                    call.media.receive.state = MediaChannelState::Opening;
                    call.media.receive.deadline =
                        Some(Instant::now() + HANDSET_ACKNOWLEDGEMENT_TIMEOUT);
                    call.media.receive.request = Some(request);
                    if call.media.transmit.state == MediaChannelState::Closed {
                        call.media.transmit.request = None;
                    }
                    call.media.coupled_transmit_endpoint = None;
                    let call = call.clone();
                    send_message(
                        stream,
                        &ServerMessage::OpenReceiveChannel {
                            call_reference: call.wire_reference,
                            passthrough_party_id: request.token().get(),
                            packet_ms,
                            codec,
                            echo_cancellation: audio_processing.echo_cancellation,
                            telephone_event_payload,
                            source_address: source
                                .map(|endpoint| endpoint.address)
                                .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
                            source_port: source.map_or(0, |endpoint| endpoint.rtp_port),
                            encryption: None,
                            wire: None,
                        },
                        protocol,
                    )
                    .await?;
                }
                CommandAction::OpenMultimediaReceiveChannel {
                    call_id,
                    descriptor,
                } => {
                    let call_state = require_call(state, call_id)?.state;
                    if call_state != CallState::Connected {
                        return Err(ServerError::InvalidCallTransaction {
                            call_id,
                            operation: "open video receive media",
                            state: call_state,
                        });
                    }
                    validate_multimedia_receive(state, &descriptor)?;
                    let request = allocate_video_receive_identity(state, call_id)?;
                    let replacement_close = take_multimedia_receive_close(state, call_id);
                    let call = require_call_mut(state, call_id)?;
                    let line_instance = call.line_instance;
                    let call_reference = CallReference::new(call.wire_reference);
                    call.video_receive.leg = Some(VideoReceiveLeg {
                        request,
                        conference_id: descriptor.conference_id,
                        codec: descriptor.payload.codec(),
                        requested_address_type: descriptor.requested_address_type,
                        state: MediaChannelState::Opening,
                        deadline: Some(Instant::now() + HANDSET_ACKNOWLEDGEMENT_TIMEOUT),
                    });

                    if let Some(close) = replacement_close {
                        send_message(stream, &close, protocol).await?;
                    }
                    send_message(
                        stream,
                        &ServerMessage::OpenMultimediaChannel(OpenMultimediaChannel {
                            conference_id: descriptor.conference_id,
                            passthrough_party_id: request.token().get().into(),
                            line_instance,
                            call_reference,
                            payload: descriptor.payload,
                            conference_creator: descriptor.conference_creator,
                            encryption: descriptor.encryption,
                            stream_passthrough_id: descriptor.stream_passthrough_id,
                            associated_stream_id: descriptor.associated_stream_id,
                            source: descriptor.source,
                            requested_address_type: descriptor.requested_address_type,
                        }),
                        protocol,
                    )
                    .await?;
                }
                CommandAction::CloseMultimediaReceiveChannel { call_id } => {
                    if let Some(close) = take_multimedia_receive_close(state, call_id) {
                        send_message(stream, &close, protocol).await?;
                    }
                }
                CommandAction::StartMultimediaTransmission {
                    call_id,
                    descriptor,
                } => {
                    let call_state = require_call(state, call_id)?.state;
                    if call_state != CallState::Connected {
                        return Err(ServerError::InvalidCallTransaction {
                            call_id,
                            operation: "start video transmit media",
                            state: call_state,
                        });
                    }
                    validate_multimedia_transmit(state, &descriptor)?;
                    let request = allocate_video_transmit_identity(state, call_id)?;
                    let replacement_stop = take_multimedia_transmit_stop(state, call_id);
                    let call_reference = {
                        let call = require_call_mut(state, call_id)?;
                        let call_reference = CallReference::new(call.wire_reference);
                        call.video_transmit.leg = Some(VideoTransmitLeg {
                            request,
                            conference_id: descriptor.conference_id,
                            codec: descriptor.payload.codec(),
                            address_type: address_type(descriptor.endpoint.address),
                            state: MediaChannelState::Opening,
                            deadline: Some(Instant::now() + HANDSET_ACKNOWLEDGEMENT_TIMEOUT),
                        });
                        call_reference
                    };

                    if let Some(stop) = replacement_stop {
                        send_message(stream, &stop, protocol).await?;
                    }
                    send_message(
                        stream,
                        &ServerMessage::StartMultimediaTransmission(MultimediaTransmissionStart {
                            conference_id: descriptor.conference_id,
                            passthrough_party_id: request.token().get().into(),
                            endpoint: descriptor.endpoint,
                            call_reference,
                            payload: descriptor.payload,
                            traffic_class: descriptor.traffic_class,
                            encryption: descriptor.encryption,
                            stream_passthrough_id: descriptor.stream_passthrough_id,
                            associated_stream_id: descriptor.associated_stream_id,
                        }),
                        protocol,
                    )
                    .await?;
                }
                CommandAction::StopMultimediaTransmission { call_id } => {
                    if let Some(stop) = take_multimedia_transmit_stop(state, call_id) {
                        send_message(stream, &stop, protocol).await?;
                    }
                }
                flow_action @ (CommandAction::SetMultimediaTransmitBitRate {
                    call_id,
                    passthrough_party_id,
                    maximum_bit_rate,
                }
                | CommandAction::NotifyMultimediaTransmitBitRate {
                    call_id,
                    passthrough_party_id,
                    maximum_bit_rate,
                }) => {
                    if maximum_bit_rate == 0 {
                        return Err(ServerError::InvalidMultimediaTransmitControl(
                            "maximum bit rate must be nonzero",
                        ));
                    }
                    let (conference_id, call_reference) =
                        multimedia_transmit_control_identity(state, call_id, passthrough_party_id)?;
                    let flow = VideoFlowControl {
                        conference_id,
                        passthrough_party_id,
                        call_reference,
                        maximum_bit_rate,
                    };
                    let message = if matches!(
                        flow_action,
                        CommandAction::SetMultimediaTransmitBitRate { .. }
                    ) {
                        ServerMessage::FlowControlCommand(flow)
                    } else {
                        ServerMessage::FlowControlNotify(flow)
                    };
                    send_message(stream, &message, protocol).await?;
                }
                CommandAction::ControlMultimediaTransmission {
                    call_id,
                    passthrough_party_id,
                    control,
                } => {
                    let (conference_id, call_reference) =
                        multimedia_transmit_control_identity(state, call_id, passthrough_party_id)?;
                    let (command, data) = encode_multimedia_transmit_control(control)?;
                    send_message(
                        stream,
                        &ServerMessage::MiscellaneousCommand(MiscellaneousCommand {
                            conference_id,
                            passthrough_party_id,
                            call_reference,
                            command,
                            data,
                        }),
                        protocol,
                    )
                    .await?;
                }
                CommandAction::OpenOutboundMedia {
                    call_id,
                    source,
                    mut endpoint,
                    codec,
                    packet_ms,
                    max_frames_per_packet,
                    dtmf_mode,
                    audio_processing,
                    traffic_class,
                } => {
                    let call_state = require_call(state, call_id)?.state;
                    if !matches!(call_state, CallState::Proceed | CallState::RingOut) {
                        return Err(ServerError::InvalidCallTransaction {
                            call_id,
                            operation: "open coupled outbound media",
                            state: call_state,
                        });
                    }
                    let telephone_event_payload = dtmf_mode.telephone_event_payload(state.features);
                    let source_address = source
                        .map(|source| source.address)
                        .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
                    let source_port = source.map_or(0, |source| source.rtp_port);
                    let request = allocate_media_request_identity(state, call_id)?;
                    let call = require_call_mut(state, call_id)?;
                    call.media.requested = true;
                    call.media.codec = codec;
                    call.media.packet_ms = packet_ms;
                    call.media.max_frames_per_packet = max_frames_per_packet;
                    call.media.receive.telephone_event_payload = telephone_event_payload;
                    call.media.receive.peer = None;
                    call.media.receive.state = MediaChannelState::Opening;
                    call.media.receive.deadline =
                        Some(Instant::now() + HANDSET_ACKNOWLEDGEMENT_TIMEOUT);
                    call.media.receive.request = Some(request);
                    call.media.transmit.telephone_event_payload = telephone_event_payload;
                    call.media.transmit.peer = None;
                    call.media.transmit.state = MediaChannelState::Opening;
                    call.media.transmit.deadline =
                        Some(Instant::now() + HANDSET_ACKNOWLEDGEMENT_TIMEOUT);
                    call.media.transmit.request = Some(request);
                    endpoint.telephone_event_payload = telephone_event_payload;
                    call.media.coupled_transmit_endpoint = Some(endpoint);
                    let call = call.clone();
                    send_message(
                        stream,
                        &ServerMessage::OpenReceiveChannel {
                            call_reference: call.wire_reference,
                            passthrough_party_id: request.token().get(),
                            packet_ms,
                            codec,
                            echo_cancellation: audio_processing.echo_cancellation,
                            telephone_event_payload,
                            source_address,
                            source_port,
                            encryption: None,
                            wire: None,
                        },
                        protocol,
                    )
                    .await?;
                    send_message(
                        stream,
                        &ServerMessage::StartMediaTransmission {
                            call_reference: call.wire_reference,
                            passthrough_party_id: request.token().get(),
                            endpoint,
                            silence_suppression: audio_processing.silence_suppression,
                            traffic_class,
                            encryption: None,
                            wire: None,
                        },
                        protocol,
                    )
                    .await?;
                }
                CommandAction::CloseReceiveChannel { call_id, .. } => {
                    let call = require_call_mut(state, call_id)?;
                    call.media.coupled_transmit_endpoint = None;
                    if call.media.receive.state != MediaChannelState::Closed {
                        call.media.receive.state = MediaChannelState::Closed;
                        call.media.receive.deadline = None;
                        let call = call.clone();
                        send_message(
                            stream,
                            &ServerMessage::CloseReceiveChannel(AudioStreamControl {
                                conference_id: ConferenceId::new(call.wire_reference),
                                call_reference: CallReference::new(call.wire_reference),
                                passthrough_party_id: media_request_party_id(
                                    call.media.receive.request,
                                    call.wire_reference,
                                )
                                .into(),
                                port_handling_flag: 0,
                            }),
                            protocol,
                        )
                        .await?;
                    }
                }
                CommandAction::StartMedia {
                    call_id,
                    mut endpoint,
                    dtmf_mode,
                    audio_processing,
                    traffic_class,
                } => {
                    let telephone_event_payload = dtmf_mode.telephone_event_payload(state.features);
                    let request = {
                        let call = require_call(state, call_id)?;
                        if call.media.transmit.request.is_none() {
                            call.media.receive.request
                        } else {
                            None
                        }
                    };
                    let request = match request {
                        Some(request) => request,
                        None => allocate_media_request_identity(state, call_id)?,
                    };
                    let call = require_call_mut(state, call_id)?;
                    call.media.requested = true;
                    call.media.transmit.telephone_event_payload = telephone_event_payload;
                    call.media.transmit.peer = None;
                    call.media.transmit.state = MediaChannelState::Opening;
                    call.media.transmit.deadline =
                        Some(Instant::now() + HANDSET_ACKNOWLEDGEMENT_TIMEOUT);
                    call.media.transmit.request = Some(request);
                    call.media.coupled_transmit_endpoint = None;
                    let call = call.clone();
                    endpoint.telephone_event_payload = telephone_event_payload;
                    send_message(
                        stream,
                        &ServerMessage::StartMediaTransmission {
                            call_reference: call.wire_reference,
                            passthrough_party_id: request.token().get(),
                            endpoint,
                            silence_suppression: audio_processing.silence_suppression,
                            traffic_class,
                            encryption: None,
                            wire: None,
                        },
                        protocol,
                    )
                    .await?;
                }
                CommandAction::StartMulticastReception {
                    conference_id,
                    call_id,
                    route,
                    echo_cancellation,
                    g723_bitrate,
                } => {
                    validate_multicast_route(state, route, None)?;
                    let wire_call_reference = require_call(state, call_id)?.wire_reference;
                    let request = allocate_multicast_request_identity(state)?;
                    let key = MulticastKey {
                        conference_id,
                        call_id,
                    };
                    if let Some(stop) = take_multicast_stop(state, key, true) {
                        send_message(stream, &stop, protocol).await?;
                    }
                    send_message(
                        stream,
                        &ServerMessage::StartMulticastMediaReception(MulticastMediaReception {
                            conference_id,
                            passthrough_party_id: request.token().get().into(),
                            call_reference: CallReference::new(wire_call_reference),
                            address: route.address,
                            port: route.port,
                            packet_millis: route.packet_millis,
                            codec: route.codec,
                            echo_cancellation,
                            g723_bitrate,
                        }),
                        protocol,
                    )
                    .await?;
                    state
                        .multicast
                        .entry(key)
                        .or_insert_with(|| MulticastSession {
                            wire_call_reference,
                            receive: None,
                            transmit: None,
                        })
                        .receive = Some(MulticastReceive {
                        request,
                        route,
                        state: MulticastReceiveState::AwaitingAcknowledgement {
                            deadline: Instant::now() + HANDSET_ACKNOWLEDGEMENT_TIMEOUT,
                        },
                    });
                }
                CommandAction::StopMulticastReception {
                    conference_id,
                    call_id,
                } => {
                    let key = MulticastKey {
                        conference_id,
                        call_id,
                    };
                    if let Some(stop) = take_multicast_stop(state, key, true) {
                        send_message(stream, &stop, protocol).await?;
                    }
                }
                CommandAction::StartMulticastTransmission {
                    conference_id,
                    call_id,
                    route,
                    precedence,
                    silence_suppression,
                    max_frames_per_packet,
                    g723_bitrate,
                } => {
                    validate_multicast_route(state, route, Some(max_frames_per_packet))?;
                    let wire_call_reference = require_call(state, call_id)?.wire_reference;
                    let request = allocate_multicast_request_identity(state)?;
                    let key = MulticastKey {
                        conference_id,
                        call_id,
                    };
                    if let Some(stop) = take_multicast_stop(state, key, false) {
                        send_message(stream, &stop, protocol).await?;
                    }
                    send_message(
                        stream,
                        &ServerMessage::StartMulticastMediaTransmission(
                            MulticastMediaTransmission {
                                conference_id,
                                passthrough_party_id: request.token().get().into(),
                                call_reference: CallReference::new(wire_call_reference),
                                address: route.address,
                                port: route.port,
                                packet_millis: route.packet_millis,
                                codec: route.codec,
                                precedence,
                                silence_suppression: silence_suppression.wire_value(),
                                max_frames_per_packet,
                                g723_bitrate,
                            },
                        ),
                        protocol,
                    )
                    .await?;
                    state
                        .multicast
                        .entry(key)
                        .or_insert_with(|| MulticastSession {
                            wire_call_reference,
                            receive: None,
                            transmit: None,
                        })
                        .transmit = Some(MulticastTransmit { request, route });
                    context
                        .event_tx
                        .send(Event::device(
                            state.device.id.clone(),
                            state.generation,
                            DeviceEventKind::MulticastTransmissionStarted {
                                conference_id,
                                call_id,
                                route,
                            },
                        ))
                        .await
                        .map_err(|_| ServerError::Stopped)?;
                }
                CommandAction::StopMulticastTransmission {
                    conference_id,
                    call_id,
                } => {
                    let key = MulticastKey {
                        conference_id,
                        call_id,
                    };
                    if let Some(stop) = take_multicast_stop(state, key, false) {
                        send_message(stream, &stop, protocol).await?;
                    }
                }
                CommandAction::StopMedia { call_id, .. } => {
                    if let Some(call) = state
                        .calls_by_id
                        .get_mut(&call_id)
                        .filter(|call| call.media.transmit.state != MediaChannelState::Closed)
                    {
                        call.media.transmit.state = MediaChannelState::Closed;
                        call.media.transmit.deadline = None;
                        call.media.coupled_transmit_endpoint = None;
                        let call = call.clone();
                        send_message(
                            stream,
                            &ServerMessage::StopMediaTransmission(AudioStreamControl {
                                conference_id: ConferenceId::new(call.wire_reference),
                                call_reference: CallReference::new(call.wire_reference),
                                passthrough_party_id: media_request_party_id(
                                    call.media.transmit.request,
                                    call.wire_reference,
                                )
                                .into(),
                                port_handling_flag: 0,
                            }),
                            protocol,
                        )
                        .await?;
                    }
                }
                CommandAction::CloseCall { call_id, .. } => {
                    if let Some(call) = state.calls_by_id.get(&call_id).cloned() {
                        state.active_key_mode = KeyMode::OnHook;
                        stop_call_multicast(stream, state, call_id, protocol).await?;
                        if call.state != CallState::OnHook {
                            close_call_media_messages(stream, &call, protocol).await?;
                            close_call_messages(
                                stream,
                                &call,
                                &state.device.soft_keys,
                                protocol,
                                context.config.timezone_offset_minutes,
                            )
                            .await?;
                            request_connection_statistics(stream, state, &call, context).await?;
                        }
                        remove_call(state, call_id);
                        refresh_mwi_lamps(stream, state, protocol).await?;
                    } else {
                        state.cancelled_calls.insert(call_id);
                    }
                }
            }
        }
    }
    Ok(false)
}

async fn send_mwi_lamp(
    stream: &mut dyn StationIo,
    state: &SessionState,
    line_instance: u32,
    enabled: bool,
    protocol: ProtocolVersion,
) -> Result<(), ServerError> {
    let mode = projected_mwi_lamp(state.device.ui, state.active_call_id.is_some(), enabled);
    send_message(
        stream,
        &ServerMessage::SetLamp {
            stimulus: ButtonType::Voicemail,
            instance: line_instance,
            mode,
        },
        protocol,
    )
    .await
}

fn projected_mwi_lamp(ui: crate::types::StationUiPolicy, on_call: bool, enabled: bool) -> LampMode {
    if enabled && (ui.mwi_on_call || !on_call) {
        ui.mwi_lamp_mode
    } else {
        LampMode::Off
    }
}

fn updated_history_disposition(
    current: CallHistoryDisposition,
    state: CallState,
) -> CallHistoryDisposition {
    if current != CallHistoryDisposition::Missed {
        return current;
    }
    match state {
        CallState::Connected => CallHistoryDisposition::Received,
        CallState::RemoteMultiline => CallHistoryDisposition::Ignore,
        _ => current,
    }
}

async fn refresh_mwi_lamps(
    stream: &mut dyn StationIo,
    state: &SessionState,
    protocol: ProtocolVersion,
) -> Result<(), ServerError> {
    for (&line_instance, &enabled) in &state.mwi_by_line {
        send_mwi_lamp(stream, state, line_instance, enabled, protocol).await?;
    }
    Ok(())
}

fn incoming_ringer(
    ringer: Option<IncomingRing>,
    incoming_state: CallState,
) -> Option<IncomingRing> {
    ringer.map(|mut ringer| {
        if incoming_state == CallState::CallWaiting {
            ringer.duration = RingDuration::Single;
            if ringer.mode != RingerMode::Urgent {
                ringer.mode = RingerMode::Silent;
            }
        }
        ringer
    })
}

async fn request_connection_statistics(
    stream: &mut dyn StationIo,
    state: &mut SessionState,
    call: &SessionCall,
    context: &SessionContext,
) -> Result<(), ServerError> {
    prune_connection_statistics(&mut state.pending_connection_statistics, Instant::now());
    if !call.media.requested
        || state.pending_connection_statistics.len() >= MAX_PENDING_CONNECTION_STATISTICS
        || state.statistics_references.len() >= MAX_STATISTICS_REFERENCES_PER_SESSION
    {
        return Ok(());
    }
    let directory_number = if call.statistics_directory_number.is_empty() {
        call.dialed_number.trim()
    } else {
        call.statistics_directory_number.trim()
    };
    let maximum = if state.registration.protocol >= ProtocolVersion::V19 {
        24
    } else {
        23
    };
    if directory_number.is_empty()
        || directory_number.len() > maximum
        || directory_number.contains(['\0', '\r', '\n'])
    {
        warn!(
            device_id = %state.device.id,
            ?call.call_id,
            byte_count = directory_number.len(),
            "skipping connection-statistics request with unusable directory number"
        );
        return Ok(());
    }
    if !state.statistics_references.insert(call.wire_reference) {
        warn!(
            device_id = %state.device.id,
            ?call.call_id,
            call_reference = call.wire_reference,
            "skipping connection-statistics request for a reused call reference"
        );
        return Ok(());
    }
    let request_generation = context
        .next_statistics_generation
        .fetch_add(1, Ordering::Relaxed);
    let processing = StatisticsProcessing::Clear;
    state.pending_connection_statistics.insert(
        call.wire_reference,
        PendingConnectionStatistics {
            session_generation: state.generation,
            request_generation,
            call_id: call.call_id,
            line_instance: call.line_instance,
            codec: call.media.codec,
            packet_ms: call.media.packet_ms,
            max_frames_per_packet: call.media.max_frames_per_packet,
            receive_peer: call.media.receive.peer,
            transmit_peer: call.media.transmit.peer,
            directory_number: directory_number.to_owned(),
            processing,
            expires_at: Instant::now() + CONNECTION_STATISTICS_TIMEOUT,
        },
    );
    send_message(
        stream,
        &ServerMessage::ConnectionStatisticsRequest {
            directory_number: directory_number.to_owned(),
            call_reference: call.wire_reference,
            processing,
        },
        state.registration.protocol,
    )
    .await
}

fn statistics_directory_for_call_info(info: &CallInfo) -> &str {
    match info.direction {
        crate::types::CallDirection::Inbound => &info.calling_number,
        crate::types::CallDirection::Outbound => &info.called_number,
    }
}

fn prune_connection_statistics(
    pending_statistics: &mut HashMap<u32, PendingConnectionStatistics>,
    now: Instant,
) {
    pending_statistics.retain(|_, pending| pending.expires_at > now);
}

async fn collect_connection_statistics(
    state: &mut SessionState,
    statistics: ConnectionStatistics,
    context: &SessionContext,
) -> Result<(), ServerError> {
    prune_connection_statistics(&mut state.pending_connection_statistics, Instant::now());
    let Some(pending) = state
        .pending_connection_statistics
        .get(&statistics.call_reference)
        .cloned()
    else {
        warn!(
            device_id = %state.device.id,
            call_reference = statistics.call_reference,
            "ignoring unsolicited or expired connection-statistics response"
        );
        return Ok(());
    };
    let current_session = context
        .sessions
        .lock()
        .await
        .get(&state.device.id)
        .is_some_and(|session| session.generation == pending.session_generation);
    if !current_session
        || pending.session_generation != state.generation
        || statistics.processing != pending.processing
        || statistics.directory_number != pending.directory_number
    {
        warn!(
            device_id = %state.device.id,
            call_reference = statistics.call_reference,
            processing = ?statistics.processing,
            "ignoring mismatched connection-statistics response"
        );
        return Ok(());
    }
    state
        .pending_connection_statistics
        .remove(&statistics.call_reference);
    let snapshot = MediaStatisticsSnapshot {
        request_generation: pending.request_generation,
        call_id: pending.call_id,
        line_instance: LineInstance::new(pending.line_instance),
        codec: pending.codec,
        packet_ms: pending.packet_ms,
        max_frames_per_packet: pending.max_frames_per_packet,
        receive_peer: pending.receive_peer,
        transmit_peer: pending.transmit_peer,
        packets_sent: statistics.packets_sent,
        octets_sent: statistics.octets_sent,
        packets_received: statistics.packets_received,
        octets_received: statistics.octets_received,
        packets_lost: statistics.packets_lost,
        jitter_millis: statistics.jitter_millis,
        latency_millis: statistics.latency_millis,
        quality_byte_count: statistics.quality.as_bytes().len(),
    };
    {
        let mut latest = context
            .latest_media_statistics
            .write()
            .expect("SCCP media-statistics lock poisoned");
        let replace = latest
            .get(&state.device.id)
            .is_none_or(|existing| existing.request_generation < snapshot.request_generation);
        if !replace {
            return Ok(());
        }
        latest.insert(state.device.id.clone(), snapshot.clone());
    }
    context
        .event_tx
        .send(Event::device(
            state.device.id.clone(),
            state.generation,
            DeviceEventKind::ConnectionStatisticsCollected { snapshot },
        ))
        .await
        .map_err(|_| ServerError::Stopped)
}

fn status_message_frames(
    message: HandsetStatusMessage,
    device_type: DeviceType,
    persistent: &mut bool,
) -> Vec<ServerMessage> {
    let prompt_for_timed_message = matches!(
        device_type,
        DeviceType::Cisco6901
            | DeviceType::Cisco6921
            | DeviceType::Cisco6941
            | DeviceType::Cisco6945
            | DeviceType::Cisco6961
    );
    match message {
        HandsetStatusMessage::Display {
            text,
            timeout_seconds,
            priority: Some(priority),
        } => vec![ServerMessage::DisplayPriorityNotify {
            timeout_seconds: u32::from(timeout_seconds),
            priority,
            text,
        }],
        HandsetStatusMessage::Clear {
            priority: Some(priority),
        } => vec![ServerMessage::ClearPriorityNotify { priority }],
        HandsetStatusMessage::Display {
            text,
            timeout_seconds,
            priority: None,
        } if timeout_seconds == 0 || prompt_for_timed_message => {
            if timeout_seconds == 0 {
                *persistent = true;
            }
            vec![ServerMessage::DisplayPrompt {
                timeout_seconds: u32::from(timeout_seconds),
                text,
                line_instance: 0,
                call_reference: 0,
            }]
        }
        HandsetStatusMessage::Display {
            text,
            timeout_seconds,
            priority: None,
        } => vec![ServerMessage::DisplayPriorityNotify {
            timeout_seconds: u32::from(timeout_seconds),
            priority: NotificationPriority::Timed,
            text,
        }],
        HandsetStatusMessage::Clear { priority: None } => {
            let clear_prompt = std::mem::take(persistent) || prompt_for_timed_message;
            let mut frames = Vec::with_capacity(2);
            if clear_prompt {
                frames.push(ServerMessage::ClearPrompt {
                    line_instance: 0,
                    call_reference: 0,
                });
            }
            if !prompt_for_timed_message {
                frames.push(ServerMessage::ClearPriorityNotify {
                    priority: NotificationPriority::Timed,
                });
            }
            frames
        }
    }
}

async fn send_message(
    stream: &mut dyn StationIo,
    message: &ServerMessage,
    session: impl Into<StationSessionContext>,
) -> Result<(), ServerError> {
    stream
        .write_all(&message.encode_for_session(session.into())?)
        .await?;
    Ok(())
}

async fn send_station_ui_message(
    stream: &mut dyn StationIo,
    state: &SessionState,
    message: &ServerMessage,
) -> Result<(), ServerError> {
    let session = state.station_context();
    let bytes = if state.features.contains(PhoneFeatures::UTF8) {
        message.encode_for_session(session)?
    } else {
        message.encode_for_legacy_session(session, state.device.ui.legacy_code_page)?
    };
    stream.write_all(&bytes).await?;
    Ok(())
}

async fn begin_phone_call_ui(
    stream: &mut dyn StationIo,
    call: &SessionCall,
    device: &DeviceDefinition,
    session: StationSessionContext,
) -> Result<(), ServerError> {
    begin_phone_call_ui_with_key_mode(stream, call, device, KeyMode::OffHook, session).await
}

async fn begin_phone_call_ui_with_key_mode(
    stream: &mut dyn StationIo,
    call: &SessionCall,
    device: &DeviceDefinition,
    key_mode: KeyMode,
    session: StationSessionContext,
) -> Result<(), ServerError> {
    let initial_tone = device
        .line(call.line_instance)
        .map_or(Tone::InsideDial, |line| line.initial_tone);
    send_message(
        stream,
        &ServerMessage::SetSpeakerMode(SpeakerMode::On),
        session,
    )
    .await?;
    send_message(
        stream,
        &ServerMessage::SetLamp {
            stimulus: ButtonType::Line,
            instance: call.line_instance,
            mode: LampMode::On,
        },
        session,
    )
    .await?;
    send_message(
        stream,
        &ServerMessage::CallState {
            state: CallState::OffHook,
            line_instance: call.line_instance,
            call_reference: call.wire_reference,
        },
        session,
    )
    .await?;
    send_message(
        stream,
        &ServerMessage::ActivateCallPlane {
            line_instance: call.line_instance,
        },
        session,
    )
    .await?;
    send_message(
        stream,
        &ServerMessage::DisplayPrompt {
            timeout_seconds: 0,
            text: "Enter number".into(),
            line_instance: call.line_instance,
            call_reference: call.wire_reference,
        },
        session,
    )
    .await?;
    send_message(
        stream,
        &ServerMessage::StartTone {
            tone: initial_tone,
            direction: ToneDirection::User,
            line_instance: call.line_instance,
            call_reference: call.wire_reference,
        },
        session,
    )
    .await?;
    send_message(
        stream,
        &ServerMessage::SelectSoftKeys {
            line_instance: call.line_instance,
            call_reference: call.wire_reference,
            set: key_mode,
            valid_mask: device.soft_keys.valid_mask(key_mode),
        },
        session,
    )
    .await
}

async fn begin_answer_ui(
    stream: &mut dyn StationIo,
    call: &SessionCall,
    protocol: ProtocolVersion,
) -> Result<(), ServerError> {
    send_message(
        stream,
        &ServerMessage::SetRinger {
            mode: RingerMode::Off,
            duration: RingDuration::Normal,
            line_instance: call.line_instance,
            call_reference: call.wire_reference,
        },
        protocol,
    )
    .await?;
    send_message(
        stream,
        &ServerMessage::CallState {
            state: CallState::OffHook,
            line_instance: call.line_instance,
            call_reference: call.wire_reference,
        },
        protocol,
    )
    .await?;
    send_message(
        stream,
        &ServerMessage::ActivateCallPlane {
            line_instance: call.line_instance,
        },
        protocol,
    )
    .await?;
    send_message(
        stream,
        &ServerMessage::StopTone {
            line_instance: call.line_instance,
            call_reference: call.wire_reference,
        },
        protocol,
    )
    .await?;
    send_message(
        stream,
        &ServerMessage::SetLamp {
            stimulus: ButtonType::Line,
            instance: call.line_instance,
            mode: LampMode::On,
        },
        protocol,
    )
    .await?;
    Ok(())
}

async fn prepare_call_state_ui(
    stream: &mut dyn StationIo,
    call: &SessionCall,
    state: CallState,
    protocol: ProtocolVersion,
) -> Result<(), ServerError> {
    match state {
        CallState::Connected => {
            send_message(
                stream,
                &ServerMessage::SetRinger {
                    mode: RingerMode::Off,
                    duration: RingDuration::Normal,
                    line_instance: call.line_instance,
                    call_reference: call.wire_reference,
                },
                protocol,
            )
            .await?;
            send_message(
                stream,
                &ServerMessage::SetSpeakerMode(SpeakerMode::On),
                protocol,
            )
            .await?;
            send_message(
                stream,
                &ServerMessage::StopTone {
                    line_instance: call.line_instance,
                    call_reference: call.wire_reference,
                },
                protocol,
            )
            .await?;
            send_message(
                stream,
                &ServerMessage::SetLamp {
                    stimulus: ButtonType::Line,
                    instance: call.line_instance,
                    mode: LampMode::On,
                },
                protocol,
            )
            .await?;
        }
        CallState::RemoteMultiline => {
            send_message(
                stream,
                &ServerMessage::SetRinger {
                    mode: RingerMode::Off,
                    duration: RingDuration::Normal,
                    line_instance: call.line_instance,
                    call_reference: call.wire_reference,
                },
                protocol,
            )
            .await?;
            send_message(
                stream,
                &ServerMessage::SetSpeakerMode(SpeakerMode::Off),
                protocol,
            )
            .await?;
            send_message(
                stream,
                &ServerMessage::SetLamp {
                    stimulus: ButtonType::Line,
                    instance: call.line_instance,
                    mode: LampMode::On,
                },
                protocol,
            )
            .await?;
        }
        CallState::OnHook => {
            send_message(
                stream,
                &ServerMessage::SetRinger {
                    mode: RingerMode::Off,
                    duration: RingDuration::Normal,
                    line_instance: call.line_instance,
                    call_reference: call.wire_reference,
                },
                protocol,
            )
            .await?;
        }
        CallState::Hold | CallState::HoldYellow | CallState::HoldRed => {
            send_message(
                stream,
                &ServerMessage::SetLamp {
                    stimulus: ButtonType::Line,
                    instance: call.line_instance,
                    mode: LampMode::Wink,
                },
                protocol,
            )
            .await?;
        }
        CallState::RingOut | CallState::Proceed => {
            send_message(
                stream,
                &ServerMessage::SetLamp {
                    stimulus: ButtonType::Line,
                    instance: call.line_instance,
                    mode: LampMode::Blink,
                },
                protocol,
            )
            .await?;
        }
        _ => {}
    }
    Ok(())
}

async fn finish_call_state_ui(
    stream: &mut dyn StationIo,
    call: &SessionCall,
    state: CallState,
    session: StationSessionContext,
) -> Result<(), ServerError> {
    let prompt = match state {
        CallState::Connected => Some("Connected"),
        CallState::Hold | CallState::HoldYellow | CallState::HoldRed => Some("Hold"),
        CallState::RingOut => Some("Ring out"),
        CallState::Proceed => Some("Call proceeding"),
        CallState::Busy => Some("Busy"),
        CallState::Congestion => Some("Network congestion"),
        CallState::InvalidNumber => Some("Unknown number"),
        _ => None,
    };
    if state == CallState::Connected {
        send_message(
            stream,
            &ServerMessage::ActivateCallPlane {
                line_instance: call.line_instance,
            },
            session,
        )
        .await?;
    } else if matches!(
        state,
        CallState::Hold | CallState::HoldYellow | CallState::HoldRed
    ) {
        send_message(
            stream,
            &ServerMessage::SetSpeakerMode(SpeakerMode::Off),
            session,
        )
        .await?;
    }
    if let Some(text) = prompt {
        send_message(
            stream,
            &ServerMessage::DisplayPrompt {
                timeout_seconds: 0,
                text: text.into(),
                line_instance: call.line_instance,
                call_reference: call.wire_reference,
            },
            session,
        )
        .await?;
    }
    Ok(())
}

fn normalize_line(state: &SessionState, requested: u32) -> u32 {
    if requested != 0 && state.device.line(requested).is_some() {
        requested
    } else {
        state.device.first_line().map_or(1, |line| line.instance)
    }
}

fn ensure_phone_call(
    state: &mut SessionState,
    wire_reference: u32,
    line_instance: u32,
    next: &AtomicU64,
) -> SessionCall {
    let reusable = if wire_reference == 0 {
        state
            .calls_by_id
            .values()
            .filter(|call| call.state != CallState::OnHook)
            .max_by_key(|call| call.call_id.0)
    } else {
        find_call(state, wire_reference).filter(|call| call.state != CallState::OnHook)
    };
    if let Some(call) = reusable {
        return call.clone();
    }
    let mut call = reserve_phone_call(state, line_instance, next);
    if wire_reference != 0
        && wire_reference != call.wire_reference
        && !state.statistics_references.contains(&wire_reference)
    {
        state.calls_by_wire.remove(&call.wire_reference);
        call.wire_reference = wire_reference;
        state.calls_by_wire.insert(wire_reference, call.call_id);
        state.calls_by_id.insert(call.call_id, call.clone());
    }
    call
}

fn reserve_phone_call(
    state: &mut SessionState,
    line_instance: u32,
    next: &AtomicU64,
) -> SessionCall {
    let call_id = CallId(next.fetch_add(1, Ordering::Relaxed));
    insert_call(
        state,
        call_id,
        line_instance,
        Codec::Pcmu,
        CallState::OffHook,
    )
}

fn insert_call(
    state: &mut SessionState,
    call_id: CallId,
    line_instance: u32,
    codec: Codec,
    call_state: CallState,
) -> SessionCall {
    let mut wire_reference = (call_id.0 as u32).max(1);
    while state.calls_by_wire.contains_key(&wire_reference)
        || state.statistics_references.contains(&wire_reference)
    {
        wire_reference = wire_reference.wrapping_add(1).max(1);
    }
    let call = SessionCall {
        call_id,
        wire_reference,
        line_instance,
        media: CallMedia::new(codec),
        video_receive: VideoReceive::default(),
        video_transmit: VideoTransmit::default(),
        state: call_state,
        history_disposition: if matches!(call_state, CallState::RingIn | CallState::CallWaiting) {
            CallHistoryDisposition::Missed
        } else {
            CallHistoryDisposition::Placed
        },
        dialed_number: String::new(),
        statistics_directory_number: String::new(),
        transfer_role: None,
    };
    state.calls_by_wire.insert(wire_reference, call_id);
    state.calls_by_id.insert(call_id, call.clone());
    call
}

fn find_call(state: &SessionState, wire_reference: u32) -> Option<&SessionCall> {
    if wire_reference != 0 {
        state
            .calls_by_wire
            .get(&wire_reference)
            .and_then(|id| state.calls_by_id.get(id))
    } else {
        state
            .active_call_id
            .and_then(|call_id| state.calls_by_id.get(&call_id))
            .or_else(|| {
                (state.calls_by_id.len() == 1)
                    .then(|| state.calls_by_id.values().next())
                    .flatten()
            })
    }
}

fn find_answer_call(
    state: &SessionState,
    wire_reference: u32,
    line_instance: u32,
    order: CallSelectionOrder,
) -> Option<&SessionCall> {
    let matches_line = |call: &&SessionCall| {
        matches!(call.state, CallState::RingIn | CallState::CallWaiting)
            && (line_instance == 0 || call.line_instance == line_instance)
    };
    if wire_reference != 0 {
        return state
            .calls_by_wire
            .get(&wire_reference)
            .and_then(|call_id| state.calls_by_id.get(call_id))
            .filter(matches_line);
    }
    if let Some(active) = state
        .active_call_id
        .and_then(|call_id| state.calls_by_id.get(&call_id))
        .filter(matches_line)
    {
        return Some(active);
    }
    let candidates = state.calls_by_id.values().filter(matches_line);
    match order {
        CallSelectionOrder::OldestFirst => candidates.min_by_key(|call| call.call_id.0),
        CallSelectionOrder::LastFirst => candidates.max_by_key(|call| call.call_id.0),
    }
}

fn find_receive_media_call_id(
    state: &SessionState,
    wire_reference: u32,
    passthrough_party_id: u32,
) -> Option<CallId> {
    find_media_call_id(state, wire_reference, passthrough_party_id, |call| {
        call.media.receive.request
    })
}

fn find_multicast_receive_key(
    state: &SessionState,
    wire_reference: u32,
    passthrough_party_id: u32,
) -> Option<MulticastKey> {
    state.multicast.iter().find_map(|(key, session)| {
        session.receive.as_ref().and_then(|receive| {
            (matches!(
                receive.state,
                MulticastReceiveState::AwaitingAcknowledgement { .. }
            ) && session.wire_call_reference == wire_reference
                && receive.request.token().get() == passthrough_party_id)
                .then_some(*key)
        })
    })
}

fn find_multicast_transmit_key(
    state: &SessionState,
    conference_id: u32,
    wire_reference: u32,
    passthrough_party_id: u32,
    address: IpAddr,
    port: u16,
) -> Option<MulticastKey> {
    state.multicast.iter().find_map(|(key, session)| {
        session.transmit.as_ref().and_then(|transmit| {
            (key.conference_id.get() == conference_id
                && session.wire_call_reference == wire_reference
                && transmit.request.token().get() == passthrough_party_id
                && canonical_ip_address(transmit.route.address) == canonical_ip_address(address)
                && transmit.route.port == port)
                .then_some(*key)
        })
    })
}

fn find_transmit_media_call_id(
    state: &SessionState,
    conference_id: u32,
    wire_reference: u32,
    passthrough_party_id: u32,
) -> Option<CallId> {
    find_media_call_id(state, wire_reference, passthrough_party_id, |call| {
        call.media.transmit.request
    })
    .filter(|call_id| {
        state
            .calls_by_id
            .get(call_id)
            .is_some_and(|call| conference_id == 0 || conference_id == call.wire_reference)
    })
}

fn find_media_call_id(
    state: &SessionState,
    wire_reference: u32,
    passthrough_party_id: u32,
    request: impl Fn(&SessionCall) -> Option<MediaRequestIdentity>,
) -> Option<CallId> {
    state
        .calls_by_id
        .values()
        .find(|call| {
            request(call).is_some_and(|identity| {
                identity.accepts_ack(passthrough_party_id, wire_reference, call.wire_reference)
            })
        })
        .map(|call| call.call_id)
}

fn require_call(state: &SessionState, call_id: CallId) -> Result<&SessionCall, ServerError> {
    state
        .calls_by_id
        .get(&call_id)
        .ok_or(ServerError::UnknownCall(call_id))
}

fn require_call_mut(
    state: &mut SessionState,
    call_id: CallId,
) -> Result<&mut SessionCall, ServerError> {
    state
        .calls_by_id
        .get_mut(&call_id)
        .ok_or(ServerError::UnknownCall(call_id))
}

fn address_matches_type(address: IpAddr, requested: IpAddressType) -> bool {
    match requested {
        IpAddressType::Ipv4 => address.is_ipv4(),
        IpAddressType::Ipv6 => address.is_ipv6(),
        IpAddressType::Ipv4AndIpv6 => true,
        IpAddressType::Invalid | IpAddressType::Unknown(_) => false,
    }
}

fn address_type(address: IpAddr) -> IpAddressType {
    if address.is_ipv4() {
        IpAddressType::Ipv4
    } else {
        IpAddressType::Ipv6
    }
}

fn endpoint_is_usable(endpoint: MediaEndpointAddress) -> bool {
    endpoint.port != 0 && !endpoint.address.is_unspecified() && !endpoint.address.is_multicast()
}

fn capability_supports_address(
    advertised: Option<IpAddressType>,
    requested: IpAddressType,
) -> bool {
    match advertised {
        None => requested == IpAddressType::Ipv4,
        Some(IpAddressType::Ipv4AndIpv6) => true,
        Some(address_type) => address_type == requested,
    }
}

fn validate_multimedia_receive_descriptor(
    descriptor: &MultimediaReceiveDescriptor,
) -> Result<(), ServerError> {
    if !descriptor
        .payload
        .is_direction(MultimediaPayloadDirection::Receive)
    {
        return Err(ServerError::InvalidMultimediaReceive(
            "payload was not decoded from a receive message",
        ));
    }
    if descriptor.payload.codec().kind() != CodecKind::Video {
        return Err(ServerError::InvalidMultimediaReceive("codec is not video"));
    }
    if !address_matches_type(descriptor.source.address, descriptor.requested_address_type) {
        return Err(ServerError::InvalidMultimediaReceive(
            "source address does not match the requested address type",
        ));
    }
    if descriptor.source.address.is_multicast() {
        return Err(ServerError::InvalidMultimediaReceive(
            "source address must not be multicast",
        ));
    }
    Ok(())
}

fn validate_multimedia_receive(
    state: &SessionState,
    descriptor: &MultimediaReceiveDescriptor,
) -> Result<(), ServerError> {
    validate_multimedia_receive_descriptor(descriptor)?;

    if !descriptor.payload.is_valid_for(
        MultimediaPayloadDirection::Receive,
        state.registration.protocol,
    ) {
        return Err(ServerError::InvalidMultimediaReceive(
            "payload protocol does not match the live session",
        ));
    }

    match state.registration.protocol {
        protocol if protocol < ProtocolVersion::V12 => {
            if descriptor.source
                != (MediaEndpointAddress {
                    address: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                    port: 0,
                })
                || descriptor.requested_address_type != IpAddressType::Ipv4
            {
                return Err(ServerError::InvalidMultimediaReceive(
                    "this protocol version cannot carry a source endpoint",
                ));
            }
        }
        protocol
            if protocol < ProtocolVersion::V17
                && (!descriptor.source.address.is_ipv4()
                    || descriptor.requested_address_type != IpAddressType::Ipv4) =>
        {
            return Err(ServerError::InvalidMultimediaReceive(
                "this protocol version carries only IPv4 video endpoints",
            ));
        }
        _ => {}
    }

    let supported = state.media_capabilities.video().iter().any(|capability| {
        let encryption_supported = descriptor.encryption.is_none()
            || capability.encryption_capability == Some(EncryptionCapability::Capable);
        capability.codec == descriptor.payload.codec()
            && capability.direction.contains(ReceiveTransmit::RECEIVE)
            && capability_supports_address(
                capability.address_type,
                descriptor.requested_address_type,
            )
            && encryption_supported
    });
    supported
        .then_some(())
        .ok_or(ServerError::UnsupportedMultimediaReceive)
}

fn validate_multimedia_transmit_descriptor(
    descriptor: &MultimediaTransmitDescriptor,
) -> Result<(), ServerError> {
    if !descriptor
        .payload
        .is_direction(MultimediaPayloadDirection::Transmit)
    {
        return Err(ServerError::InvalidMultimediaTransmit(
            "payload was not decoded from a transmit message",
        ));
    }
    if descriptor.payload.codec().kind() != CodecKind::Video {
        return Err(ServerError::InvalidMultimediaTransmit("codec is not video"));
    }
    if !endpoint_is_usable(descriptor.endpoint) {
        return Err(ServerError::InvalidMultimediaTransmit(
            "destination endpoint must be unicast and nonzero",
        ));
    }
    Ok(())
}

fn validate_multimedia_transmit(
    state: &SessionState,
    descriptor: &MultimediaTransmitDescriptor,
) -> Result<(), ServerError> {
    validate_multimedia_transmit_descriptor(descriptor)?;
    if !descriptor.payload.is_valid_for(
        MultimediaPayloadDirection::Transmit,
        state.registration.protocol,
    ) {
        return Err(ServerError::InvalidMultimediaTransmit(
            "payload protocol does not match the live session",
        ));
    }
    if state.registration.protocol < ProtocolVersion::V17 && descriptor.endpoint.address.is_ipv6() {
        return Err(ServerError::InvalidMultimediaTransmit(
            "this protocol version carries only IPv4 video endpoints",
        ));
    }
    let requested_address = address_type(descriptor.endpoint.address);
    let supported = state.media_capabilities.video().iter().any(|capability| {
        let encryption_supported = descriptor.encryption.is_none()
            || capability.encryption_capability == Some(EncryptionCapability::Capable);
        capability.codec == descriptor.payload.codec()
            && capability.direction.contains(ReceiveTransmit::TRANSMIT)
            && capability_supports_address(capability.address_type, requested_address)
            && encryption_supported
    });
    supported
        .then_some(())
        .ok_or(ServerError::UnsupportedMultimediaTransmit)
}

fn allocate_video_receive_identity(
    state: &mut SessionState,
    call_id: CallId,
) -> Result<MediaRequestIdentity, ServerError> {
    let generation = require_call(state, call_id)?
        .video_receive
        .generation
        .checked_add(1)
        .ok_or(ServerError::MediaRequestIdentityExhausted)?;
    let token = state
        .next_media_token
        .ok_or(ServerError::MediaRequestIdentityExhausted)?;
    let request = MediaRequestIdentity::new(generation, token)
        .ok_or(ServerError::MediaRequestIdentityExhausted)?;
    state.next_media_token = token.checked_next();
    require_call_mut(state, call_id)?.video_receive.generation = generation;
    Ok(request)
}

fn multimedia_receive_close_message(call: &SessionCall, leg: &VideoReceiveLeg) -> ServerMessage {
    ServerMessage::CloseMultimediaReceiveChannel(MultimediaStreamControl {
        conference_id: leg.conference_id,
        passthrough_party_id: leg.request.token().get().into(),
        call_reference: CallReference::new(call.wire_reference),
        port_handling_flag: 0,
    })
}

fn take_multimedia_receive_close(
    state: &mut SessionState,
    call_id: CallId,
) -> Option<ServerMessage> {
    let call = state.calls_by_id.get_mut(&call_id)?;
    let leg = call.video_receive.leg.take()?;
    Some(multimedia_receive_close_message(call, &leg))
}

fn take_all_multimedia_receive_closes(state: &mut SessionState) -> Vec<ServerMessage> {
    let mut call_ids = state.calls_by_id.keys().copied().collect::<Vec<_>>();
    call_ids.sort_unstable_by_key(|call_id| call_id.get());
    call_ids
        .into_iter()
        .filter_map(|call_id| take_multimedia_receive_close(state, call_id))
        .collect()
}

fn expire_multimedia_receive_acknowledgements(
    state: &mut SessionState,
    now: Instant,
) -> Vec<ExpiredVideoReceive> {
    let mut call_ids = state
        .calls_by_id
        .iter()
        .filter_map(|(&call_id, call)| {
            call.video_receive.leg.as_ref().and_then(|leg| {
                (leg.state == MediaChannelState::Opening
                    && leg.deadline.is_some_and(|deadline| deadline <= now))
                .then_some(call_id)
            })
        })
        .collect::<Vec<_>>();
    call_ids.sort_unstable_by_key(|call_id| call_id.get());
    call_ids
        .into_iter()
        .filter_map(|call_id| {
            let leg = state
                .calls_by_id
                .get(&call_id)?
                .video_receive
                .leg
                .as_ref()?;
            let codec = leg.codec;
            let passthrough_party_id = leg.request.token().get().into();
            take_multimedia_receive_close(state, call_id).map(|close| ExpiredVideoReceive {
                call_id,
                codec,
                passthrough_party_id,
                close,
            })
        })
        .collect()
}

fn allocate_video_transmit_identity(
    state: &mut SessionState,
    call_id: CallId,
) -> Result<MediaRequestIdentity, ServerError> {
    let generation = require_call(state, call_id)?
        .video_transmit
        .generation
        .checked_add(1)
        .ok_or(ServerError::MediaRequestIdentityExhausted)?;
    let token = state
        .next_media_token
        .ok_or(ServerError::MediaRequestIdentityExhausted)?;
    let request = MediaRequestIdentity::new(generation, token)
        .ok_or(ServerError::MediaRequestIdentityExhausted)?;
    state.next_media_token = token.checked_next();
    require_call_mut(state, call_id)?.video_transmit.generation = generation;
    Ok(request)
}

fn multimedia_transmit_control_identity(
    state: &SessionState,
    call_id: CallId,
    passthrough_party_id: PassthroughPartyId,
) -> Result<(ConferenceId, CallReference), ServerError> {
    let call = require_call(state, call_id)?;
    if call.state != CallState::Connected {
        return Err(ServerError::InvalidCallTransaction {
            call_id,
            operation: "control video transmit media",
            state: call.state,
        });
    }
    let leg = call
        .video_transmit
        .leg
        .as_ref()
        .filter(|leg| {
            leg.state == MediaChannelState::Open
                && leg.request.token().get() == passthrough_party_id.get()
        })
        .ok_or(ServerError::StaleMultimediaTransmitControl {
            call_id,
            passthrough_party_id,
        })?;
    Ok((leg.conference_id, CallReference::new(call.wire_reference)))
}

fn encode_multimedia_transmit_control(
    control: MultimediaTransmitControl,
) -> Result<(MiscCommandType, BoundedBytes<36>), ServerError> {
    let (command, words) = match control {
        MultimediaTransmitControl::FreezePicture => {
            (MiscCommandType::VideoFreezePicture, Vec::new())
        }
        MultimediaTransmitControl::FastPictureUpdate {
            first_gob,
            gob_count,
        } => (
            MiscCommandType::VideoFastUpdatePicture,
            vec![first_gob, gob_count],
        ),
        MultimediaTransmitControl::FastGobUpdate {
            first_gob,
            gob_count,
        } => (
            MiscCommandType::VideoFastUpdateGob,
            vec![first_gob, gob_count],
        ),
        MultimediaTransmitControl::FastMacroblockUpdate {
            first_gob,
            first_macroblock,
            macroblock_count,
        } => (
            MiscCommandType::VideoFastUpdateMacroblock,
            vec![first_gob, first_macroblock, macroblock_count],
        ),
        MultimediaTransmitControl::LostPicture {
            picture_number,
            long_term_picture_index,
        } => (
            MiscCommandType::LostPicture,
            vec![picture_number, long_term_picture_index],
        ),
        MultimediaTransmitControl::LostPartialPicture {
            picture_number,
            long_term_picture_index,
            first_macroblock,
            macroblock_count,
        } => (
            MiscCommandType::LostPartialPicture,
            vec![
                picture_number,
                long_term_picture_index,
                first_macroblock,
                macroblock_count,
            ],
        ),
        MultimediaTransmitControl::RecoveryReferencePicture { pictures } => {
            let words =
                std::iter::once(pictures.as_slice().len() as u32)
                    .chain(pictures.as_slice().iter().flat_map(|picture| {
                        [picture.picture_number, picture.long_term_picture_index]
                    }))
                    .collect();
            (MiscCommandType::RecoveryReferencePicture, words)
        }
        MultimediaTransmitControl::TemporalSpatialTradeoff { value } => {
            (MiscCommandType::TemporalSpatialTradeoff, vec![value])
        }
    };
    let data = words
        .into_iter()
        .flat_map(u32::to_le_bytes)
        .collect::<Vec<_>>();
    let data = BoundedBytes::new(data.into_boxed_slice()).map_err(|_| {
        ServerError::InvalidMultimediaTransmitControl("parameter area exceeds 36 bytes")
    })?;
    Ok((command, data))
}

fn multimedia_transmit_stop_message(call: &SessionCall, leg: &VideoTransmitLeg) -> ServerMessage {
    ServerMessage::StopMultimediaTransmission(MultimediaStreamControl {
        conference_id: leg.conference_id,
        passthrough_party_id: leg.request.token().get().into(),
        call_reference: CallReference::new(call.wire_reference),
        port_handling_flag: 0,
    })
}

fn take_multimedia_transmit_stop(
    state: &mut SessionState,
    call_id: CallId,
) -> Option<ServerMessage> {
    let call = state.calls_by_id.get_mut(&call_id)?;
    let leg = call.video_transmit.leg.take()?;
    Some(multimedia_transmit_stop_message(call, &leg))
}

fn take_all_multimedia_transmit_stops(state: &mut SessionState) -> Vec<ServerMessage> {
    let mut call_ids = state.calls_by_id.keys().copied().collect::<Vec<_>>();
    call_ids.sort_unstable_by_key(|call_id| call_id.get());
    call_ids
        .into_iter()
        .filter_map(|call_id| take_multimedia_transmit_stop(state, call_id))
        .collect()
}

fn expire_multimedia_transmit_acknowledgements(
    state: &mut SessionState,
    now: Instant,
) -> Vec<ExpiredVideoTransmit> {
    let mut call_ids = state
        .calls_by_id
        .iter()
        .filter_map(|(&call_id, call)| {
            call.video_transmit.leg.as_ref().and_then(|leg| {
                (leg.state == MediaChannelState::Opening
                    && leg.deadline.is_some_and(|deadline| deadline <= now))
                .then_some(call_id)
            })
        })
        .collect::<Vec<_>>();
    call_ids.sort_unstable_by_key(|call_id| call_id.get());
    call_ids
        .into_iter()
        .filter_map(|call_id| {
            let leg = state
                .calls_by_id
                .get(&call_id)?
                .video_transmit
                .leg
                .as_ref()?;
            let codec = leg.codec;
            let passthrough_party_id = leg.request.token().get().into();
            take_multimedia_transmit_stop(state, call_id).map(|stop| ExpiredVideoTransmit {
                call_id,
                codec,
                passthrough_party_id,
                stop,
            })
        })
        .collect()
}

fn validate_multicast_route(
    state: &SessionState,
    route: MulticastMediaRoute,
    max_frames_per_packet: Option<u32>,
) -> Result<(), ServerError> {
    if !route.address.is_multicast() {
        return Err(ServerError::InvalidMulticastMedia(
            "address must be multicast",
        ));
    }
    if route.address.is_ipv6() && state.registration.protocol < ProtocolVersion::V17 {
        return Err(ServerError::InvalidMulticastMedia(
            "IPv6 requires protocol v17 or later",
        ));
    }
    if route.port == 0 {
        return Err(ServerError::InvalidMulticastMedia("port must be nonzero"));
    }
    if route.packet_millis == 0 {
        return Err(ServerError::InvalidMulticastMedia(
            "packet duration must be nonzero",
        ));
    }
    if route.codec.kind() != CodecKind::Audio {
        return Err(ServerError::UnsupportedMulticastCodec);
    }
    let capability = state
        .media_capabilities
        .audio()
        .iter()
        .find(|capability| capability.codec == route.codec)
        .filter(|capability| capability.max_frames_per_packet != 0)
        .ok_or(ServerError::UnsupportedMulticastCodec)?;
    if let Some(requested) = max_frames_per_packet
        && (requested == 0 || requested > capability.max_frames_per_packet)
    {
        return Err(ServerError::InvalidMulticastMedia(
            "packet framing exceeds the advertised capability",
        ));
    }
    Ok(())
}

fn allocate_multicast_request_identity(
    state: &mut SessionState,
) -> Result<MediaRequestIdentity, ServerError> {
    let generation = state
        .next_multicast_generation
        .checked_add(1)
        .ok_or(ServerError::MediaRequestIdentityExhausted)?;
    let token = state
        .next_media_token
        .ok_or(ServerError::MediaRequestIdentityExhausted)?;
    let identity = MediaRequestIdentity::new(generation, token)
        .ok_or(ServerError::MediaRequestIdentityExhausted)?;
    state.next_multicast_generation = generation;
    state.next_media_token = token.checked_next();
    Ok(identity)
}

fn multicast_stop_message(
    key: MulticastKey,
    wire_call_reference: u32,
    request: MediaRequestIdentity,
    receive: bool,
) -> ServerMessage {
    if receive {
        ServerMessage::StopMulticastMediaReception {
            conference_id: key.conference_id,
            passthrough_party_id: request.token().get().into(),
            call_reference: CallReference::new(wire_call_reference),
        }
    } else {
        ServerMessage::StopMulticastMediaTransmission {
            conference_id: key.conference_id,
            passthrough_party_id: request.token().get().into(),
            call_reference: CallReference::new(wire_call_reference),
        }
    }
}

fn take_multicast_stop(
    state: &mut SessionState,
    key: MulticastKey,
    receive: bool,
) -> Option<ServerMessage> {
    let session = state.multicast.get_mut(&key)?;
    let request = if receive {
        session.receive.take().map(|leg| leg.request)
    } else {
        session.transmit.take().map(|leg| leg.request)
    }?;
    let message = multicast_stop_message(key, session.wire_call_reference, request, receive);
    if session.receive.is_none() && session.transmit.is_none() {
        state.multicast.remove(&key);
    }
    Some(message)
}

fn expire_multicast_reception_acknowledgements(
    state: &mut SessionState,
    now: Instant,
) -> Vec<(MulticastKey, ServerMessage)> {
    let mut expired = state
        .multicast
        .iter()
        .filter_map(|(key, session)| {
            session.receive.as_ref().and_then(|receive| {
                matches!(
                    receive.state,
                    MulticastReceiveState::AwaitingAcknowledgement { deadline }
                        if deadline <= now
                )
                .then_some(*key)
            })
        })
        .collect::<Vec<_>>();
    expired.sort_unstable_by_key(|key| (key.conference_id.get(), key.call_id.get()));
    expired
        .into_iter()
        .filter_map(|key| take_multicast_stop(state, key, true).map(|stop| (key, stop)))
        .collect()
}

fn take_multicast_stops_for_call(state: &mut SessionState, call_id: CallId) -> Vec<ServerMessage> {
    let mut keys = state
        .multicast
        .keys()
        .copied()
        .filter(|key| key.call_id == call_id)
        .collect::<Vec<_>>();
    keys.sort_unstable_by_key(|key| key.conference_id.get());
    keys.into_iter()
        .flat_map(|key| {
            [
                take_multicast_stop(state, key, true),
                take_multicast_stop(state, key, false),
            ]
            .into_iter()
            .flatten()
        })
        .collect()
}

fn take_all_multicast_stops(state: &mut SessionState) -> Vec<ServerMessage> {
    let mut sessions = std::mem::take(&mut state.multicast)
        .into_iter()
        .collect::<Vec<_>>();
    sessions.sort_unstable_by_key(|(key, _)| (key.conference_id.get(), key.call_id.get()));
    sessions
        .into_iter()
        .flat_map(|(key, session)| {
            [
                session.receive.map(|leg| {
                    multicast_stop_message(key, session.wire_call_reference, leg.request, true)
                }),
                session.transmit.map(|leg| {
                    multicast_stop_message(key, session.wire_call_reference, leg.request, false)
                }),
            ]
            .into_iter()
            .flatten()
        })
        .collect()
}

async fn drain_session_media(stream: &mut dyn StationIo, state: &mut SessionState) {
    let protocol = state.registration.protocol;
    let messages = take_all_multimedia_receive_closes(state)
        .into_iter()
        .chain(take_all_multimedia_transmit_stops(state))
        .chain(take_all_multicast_stops(state));
    for message in messages {
        if send_message(stream, &message, protocol).await.is_err() {
            break;
        }
    }
}

async fn stop_call_multicast(
    stream: &mut dyn StationIo,
    state: &mut SessionState,
    call_id: CallId,
    protocol: ProtocolVersion,
) -> Result<(), ServerError> {
    for message in take_multicast_stops_for_call(state, call_id) {
        send_message(stream, &message, protocol).await?;
    }
    Ok(())
}

fn allocate_media_request_identity(
    state: &mut SessionState,
    call_id: CallId,
) -> Result<MediaRequestIdentity, ServerError> {
    let generation = require_call(state, call_id)?
        .media
        .generation
        .checked_add(1)
        .ok_or(ServerError::MediaRequestIdentityExhausted)?;
    let token = state
        .next_media_token
        .ok_or(ServerError::MediaRequestIdentityExhausted)?;
    let identity = MediaRequestIdentity::new(generation, token)
        .ok_or(ServerError::MediaRequestIdentityExhausted)?;
    state.next_media_token = token.checked_next();
    require_call_mut(state, call_id)?.media.generation = generation;
    Ok(identity)
}

fn media_request_party_id(
    request: Option<MediaRequestIdentity>,
    stable_call_reference: u32,
) -> u32 {
    request.map_or(stable_call_reference, |identity| identity.token().get())
}

fn remove_call(state: &mut SessionState, call_id: CallId) {
    if let Some(call) = state.calls_by_id.remove(&call_id) {
        state.calls_by_wire.remove(&call.wire_reference);
        if state.active_call_id == Some(call_id) {
            state.active_call_id = None;
        }
    }
}

fn canonical_ip_address(address: IpAddr) -> IpAddr {
    match address {
        IpAddr::V6(address) => address
            .to_ipv4_mapped()
            .map_or(IpAddr::V6(address), IpAddr::V4),
        address => address,
    }
}

fn server_response_address(
    local: IpAddr,
    configured_ipv4_fallback: Ipv4Addr,
    configured_ipv6_fallback: Option<Ipv6Addr>,
) -> IpAddr {
    match canonical_ip_address(local) {
        IpAddr::V4(address) if address.is_unspecified() => IpAddr::V4(configured_ipv4_fallback),
        IpAddr::V6(address) if address.is_unspecified() => {
            configured_ipv6_fallback.map_or(IpAddr::V4(configured_ipv4_fallback), IpAddr::V6)
        }
        local => local,
    }
}

fn server_response_endpoints(
    context: &SessionContext,
    protocol: ProtocolVersion,
) -> Result<Vec<SignalingServerEndpoint>, ServerError> {
    let local_endpoint = || {
        let address = server_response_address(
            context.local.ip(),
            context.config.advertised_address,
            context.config.advertised_ipv6_address,
        );
        let address = if protocol < ProtocolVersion::V17 && address.is_ipv6() {
            IpAddr::V4(context.config.advertised_address)
        } else {
            address
        };
        if address.is_unspecified() {
            return Err(ServerError::InvalidConfig(
                "server-list fallback address is unspecified".into(),
            ));
        }
        Ok(SignalingServerEndpoint {
            name: context.config.server_name.clone(),
            address,
            port: NonZeroU16::new(context.local.port()).ok_or_else(|| {
                ServerError::InvalidConfig("accepted local endpoint has port zero".into())
            })?,
        })
    };
    if context.config.signaling_servers.is_empty() {
        return local_endpoint().map(|endpoint| vec![endpoint]);
    }

    let mut routes = context.config.signaling_servers.iter().collect::<Vec<_>>();
    routes.sort_unstable_by_key(|route| route.priority);
    let endpoints = routes
        .into_iter()
        .filter(|route| protocol >= ProtocolVersion::V17 || route.address.is_ipv4())
        .filter_map(|route| route.endpoint(context.transport))
        .collect::<Vec<_>>();
    if endpoints.is_empty() {
        local_endpoint().map(|endpoint| vec![endpoint])
    } else {
        Ok(endpoints)
    }
}

async fn close_call_media_messages(
    stream: &mut dyn StationIo,
    call: &SessionCall,
    protocol: ProtocolVersion,
) -> Result<(), ServerError> {
    if let Some(leg) = &call.video_receive.leg {
        send_message(
            stream,
            &multimedia_receive_close_message(call, leg),
            protocol,
        )
        .await?;
    }
    if let Some(leg) = &call.video_transmit.leg {
        send_message(
            stream,
            &multimedia_transmit_stop_message(call, leg),
            protocol,
        )
        .await?;
    }
    if call.media.receive.state != MediaChannelState::Closed {
        send_message(
            stream,
            &ServerMessage::CloseReceiveChannel(AudioStreamControl {
                conference_id: ConferenceId::new(call.wire_reference),
                call_reference: CallReference::new(call.wire_reference),
                passthrough_party_id: media_request_party_id(
                    call.media.receive.request,
                    call.wire_reference,
                )
                .into(),
                port_handling_flag: 0,
            }),
            protocol,
        )
        .await?;
    }
    if call.media.transmit.state != MediaChannelState::Closed {
        send_message(
            stream,
            &ServerMessage::StopMediaTransmission(AudioStreamControl {
                conference_id: ConferenceId::new(call.wire_reference),
                call_reference: CallReference::new(call.wire_reference),
                passthrough_party_id: media_request_party_id(
                    call.media.transmit.request,
                    call.wire_reference,
                )
                .into(),
                port_handling_flag: 0,
            }),
            protocol,
        )
        .await?;
    }
    Ok(())
}

async fn close_call_messages(
    stream: &mut dyn StationIo,
    call: &SessionCall,
    soft_keys: &SoftKeyProfile,
    protocol: ProtocolVersion,
    timezone_offset_minutes: i16,
) -> Result<(), ServerError> {
    send_message(
        stream,
        &ServerMessage::StopTone {
            line_instance: call.line_instance,
            call_reference: call.wire_reference,
        },
        protocol,
    )
    .await?;
    send_message(
        stream,
        &ServerMessage::SetLamp {
            stimulus: ButtonType::Line,
            instance: call.line_instance,
            mode: LampMode::Off,
        },
        protocol,
    )
    .await?;
    send_message(
        stream,
        &ServerMessage::ClearPrompt {
            line_instance: call.line_instance,
            call_reference: call.wire_reference,
        },
        protocol,
    )
    .await?;
    send_message(
        stream,
        &ServerMessage::CallState {
            state: CallState::OnHook,
            line_instance: call.line_instance,
            call_reference: call.wire_reference,
        },
        protocol,
    )
    .await?;
    send_message(
        stream,
        &ServerMessage::SelectSoftKeys {
            line_instance: 0,
            call_reference: 0,
            set: KeyMode::OnHook,
            valid_mask: soft_keys.valid_mask(KeyMode::OnHook),
        },
        protocol,
    )
    .await?;
    send_message(
        stream,
        &time_date_message(timezone_offset_minutes),
        protocol,
    )
    .await?;
    send_message(
        stream,
        &ServerMessage::SetSpeakerMode(SpeakerMode::Off),
        protocol,
    )
    .await?;
    // Publish the matching OnHook state before stopping alerting.
    send_message(
        stream,
        &ServerMessage::SetRinger {
            mode: RingerMode::Off,
            duration: RingDuration::Normal,
            line_instance: call.line_instance,
            call_reference: call.wire_reference,
        },
        protocol,
    )
    .await?;
    Ok(())
}

fn time_date_message(timezone_offset_minutes: i16) -> ServerMessage {
    time_date_message_at(SystemTime::now(), timezone_offset_minutes)
}

fn time_date_message_at(now: SystemTime, timezone_offset_minutes: i16) -> ServerMessage {
    let unix = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let local = (unix as i128 + i128::from(timezone_offset_minutes) * 60)
        .clamp(0, i128::from(u32::MAX)) as u64;
    let days = (local / 86_400) as i64;
    let seconds = local % 86_400;
    let (year, month, day) = civil_from_days(days);
    ServerMessage::TimeDate {
        year: year as u32,
        month,
        weekday: ((days + 4).rem_euclid(7) + 1) as u32,
        day,
        hour: (seconds / 3600) as u32,
        minute: ((seconds % 3600) / 60) as u32,
        second: (seconds % 60) as u32,
        milliseconds: 0,
        unix_seconds: local as u32,
    }
}

// Howard Hinnant's civil-from-days algorithm, with days based at Unix epoch.
fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month as u32, day as u32)
}

#[cfg(test)]
mod tests {
    use std::net::Ipv6Addr;

    use super::*;
    use crate::PhoneSoftKeyPosition;
    use crate::message::values::{
        AnnouncementPlayMode, EndOfAnnouncementAck, IpAddressType, RFC2833_TELEPHONE_EVENT_PAYLOAD,
        ReceiveTransmit,
    };
    use crate::message::wire::Frame;
    use crate::message::{
        ClientMessage, MediaTransmissionAck, RegistrationMessage, ServerMessage, id,
    };
    use crate::types::{
        BlfSpeedDialDefinition, FeatureDefinition, LineAppearance, LineDefinition,
        ServiceDefinition, SpeedDialDefinition,
    };

    fn definition() -> DeviceDefinition {
        definition_for("SEP001122334455")
    }

    fn definition_for(device_id: &str) -> DeviceDefinition {
        DeviceDefinition {
            id: DeviceId::new(device_id).unwrap(),
            description: "Test phone".into(),
            transport: StationTransportRequirement::Either,
            signaling_qos: None,
            buttons: vec![ButtonDefinition::Line(LineAppearance::new(
                1,
                LineDefinition {
                    number: "1001".into(),
                    display_name: "Desk 1001".into(),
                },
            ))],
            soft_keys: SoftKeyProfile::default(),
            ui: Default::default(),
        }
    }

    #[test]
    fn session_generations_are_nonzero_monotonic_and_fail_closed_at_exhaustion() {
        let next = AtomicU64::new(1);
        let first = allocate_session_generation(&next).unwrap();
        let second = allocate_session_generation(&next).unwrap();
        assert_eq!(u64::from(first), 1);
        assert_eq!(u64::from(second), 2);

        let exhausted = AtomicU64::new(u64::MAX);
        assert!(matches!(
            allocate_session_generation(&exhausted),
            Err(ServerError::SessionGenerationExhausted)
        ));
        assert_eq!(exhausted.load(Ordering::Relaxed), u64::MAX);

        let boundary = AtomicU64::new(u64::MAX - 1);
        assert_eq!(
            u64::from(allocate_session_generation(&boundary).unwrap()),
            u64::MAX - 1
        );
        assert!(matches!(
            allocate_session_generation(&boundary),
            Err(ServerError::SessionGenerationExhausted)
        ));
        assert_eq!(boundary.load(Ordering::Relaxed), u64::MAX);

        let invalid = AtomicU64::new(0);
        assert!(matches!(
            allocate_session_generation(&invalid),
            Err(ServerError::SessionGenerationExhausted)
        ));
        assert_eq!(invalid.load(Ordering::Relaxed), 0);
    }

    fn multicast_test_state(protocol: ProtocolVersion) -> SessionState {
        let device = definition();
        SessionState {
            registration: DeviceRegistration {
                id: device.id.clone(),
                peer: "127.0.0.1:2000".parse().unwrap(),
                transport: StationTransport::Clear,
                reported_address: Some(Ipv4Addr::LOCALHOST),
                reported_ipv6_address: None,
                device_type: DeviceType::Undefined,
                protocol,
                firmware: "test".into(),
            },
            device,
            features: PhoneFeatures::empty(),
            generation: SessionGeneration::new(1).unwrap(),
            calls_by_id: HashMap::new(),
            calls_by_wire: HashMap::new(),
            media_capabilities: vec![MediaCapability {
                codec: Codec::Pcmu,
                max_frames_per_packet: 2,
                codec_parameters: [0; 8],
            }]
            .into(),
            next_media_token: MediaRequestToken::new(1),
            next_multicast_generation: 0,
            multicast: HashMap::new(),
            pending_connection_statistics: HashMap::new(),
            statistics_references: HashSet::new(),
            cancelled_calls: HashSet::new(),
            last_number_by_line: HashMap::new(),
            forwarding_by_line: HashMap::new(),
            feature_states: HashMap::new(),
            mwi_by_line: HashMap::new(),
            mobility_appearances: HashMap::new(),
            active_key_mode: KeyMode::OnHook,
            active_call_id: None,
            pending_parking_menu: None,
            persistent_status_message: false,
            headset_enabled: false,
            media_path_states: HashMap::new(),
        }
    }

    fn multicast_route(address: IpAddr, codec: Codec) -> MulticastMediaRoute {
        MulticastMediaRoute {
            address,
            port: 5004,
            codec,
            packet_millis: 20,
        }
    }

    const fn test_rtp_payload_number(value: u8) -> crate::RtpPayloadNumber {
        match crate::RtpPayloadNumber::new(value as u32) {
            Ok(payload_number) => payload_number,
            Err(_) => panic!("test RTP payload number is out of range"),
        }
    }

    fn video_receive_descriptor(
        conference_id: u32,
        source: MediaEndpointAddress,
    ) -> MultimediaReceiveDescriptor {
        MultimediaReceiveDescriptor {
            conference_id: ConferenceId::new(conference_id),
            payload: MultimediaPayload::from_wire(
                0,
                test_rtp_payload_number(97),
                [0xa5; crate::MULTIMEDIA_CAPABILITY_BYTES],
                Codec::H264,
                MultimediaPayloadDirection::Receive,
                ProtocolVersion::V22,
            ),
            conference_creator: false,
            encryption: None,
            stream_passthrough_id: conference_id + 100,
            associated_stream_id: 0,
            source,
            requested_address_type: IpAddressType::Ipv4AndIpv6,
        }
    }

    fn video_transmit_descriptor(
        conference_id: u32,
        endpoint: MediaEndpointAddress,
    ) -> MultimediaTransmitDescriptor {
        MultimediaTransmitDescriptor {
            conference_id: ConferenceId::new(conference_id),
            endpoint,
            payload: MultimediaPayload::from_wire(
                0,
                test_rtp_payload_number(98),
                [0x5a; crate::MULTIMEDIA_CAPABILITY_BYTES],
                Codec::H264,
                MultimediaPayloadDirection::Transmit,
                ProtocolVersion::V22,
            ),
            traffic_class: MediaTrafficClass::from_wire(136),
            encryption: None,
            stream_passthrough_id: conference_id + 200,
            associated_stream_id: 0,
        }
    }

    #[test]
    fn video_receive_validation_uses_typed_station_policy_without_touching_audio() {
        let mut state = multicast_test_state(ProtocolVersion::V22);
        state.media_capabilities = StationMediaCapabilities::new(
            state.media_capabilities.audio().to_vec(),
            vec![crate::message::capabilities::VideoCapability {
                codec: Codec::H264,
                direction: ReceiveTransmit::RECEIVE,
                level_preferences: Vec::new(),
                codec_parameters: Vec::new(),
                encryption_capability: Some(EncryptionCapability::Capable),
                address_type: Some(IpAddressType::Ipv4AndIpv6),
            }],
        );
        let audio_before = state.media_capabilities.audio().to_vec();
        let descriptor = video_receive_descriptor(
            70,
            MediaEndpointAddress {
                address: "192.0.2.10".parse().unwrap(),
                port: 5004,
            },
        );
        assert!(validate_multimedia_receive(&state, &descriptor).is_ok());
        assert_eq!(state.media_capabilities.audio(), audio_before);

        let receive_capability = state.media_capabilities.video()[0].clone();
        state.media_capabilities = StationMediaCapabilities::new(
            audio_before.clone(),
            vec![crate::message::capabilities::VideoCapability {
                direction: ReceiveTransmit::TRANSMIT,
                ..receive_capability.clone()
            }],
        );
        assert!(matches!(
            validate_multimedia_receive(&state, &descriptor),
            Err(ServerError::UnsupportedMultimediaReceive)
        ));
        state.media_capabilities =
            StationMediaCapabilities::new(audio_before.clone(), vec![receive_capability]);

        let mut wrong_direction = descriptor.clone();
        wrong_direction.payload = video_transmit_descriptor(
            70,
            MediaEndpointAddress {
                address: "192.0.2.10".parse().unwrap(),
                port: 5004,
            },
        )
        .payload;
        assert!(matches!(
            wrong_direction.validate(),
            Err(ServerError::InvalidMultimediaReceive(_))
        ));

        let mut mismatched_address = descriptor.clone();
        mismatched_address.requested_address_type = IpAddressType::Ipv6;
        assert!(matches!(
            mismatched_address.validate(),
            Err(ServerError::InvalidMultimediaReceive(_))
        ));

        for protocol in [
            ProtocolVersion::V3,
            ProtocolVersion::V10,
            ProtocolVersion::V11,
        ] {
            state.registration.protocol = protocol;
            let mut legacy_descriptor = descriptor.clone();
            legacy_descriptor.source = MediaEndpointAddress {
                address: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                port: 0,
            };
            legacy_descriptor.requested_address_type = IpAddressType::Ipv4;
            legacy_descriptor.payload = MultimediaPayload::from_wire(
                0,
                test_rtp_payload_number(97),
                [0xa5; crate::MULTIMEDIA_CAPABILITY_BYTES],
                Codec::H264,
                MultimediaPayloadDirection::Receive,
                protocol,
            );
            assert!(validate_multimedia_receive(&state, &legacy_descriptor).is_ok());
            assert_eq!(state.media_capabilities.audio(), audio_before);
        }
    }

    #[test]
    fn video_transmit_validation_requires_exact_direction_endpoint_and_protocol() {
        let mut state = multicast_test_state(ProtocolVersion::V22);
        let audio_before = state.media_capabilities.audio().to_vec();
        let capability = crate::message::capabilities::VideoCapability {
            codec: Codec::H264,
            direction: ReceiveTransmit::TRANSMIT,
            level_preferences: Vec::new(),
            codec_parameters: Vec::new(),
            encryption_capability: Some(EncryptionCapability::Capable),
            address_type: Some(IpAddressType::Ipv4AndIpv6),
        };
        state.media_capabilities =
            StationMediaCapabilities::new(audio_before.clone(), vec![capability.clone()]);
        let descriptor = video_transmit_descriptor(
            80,
            MediaEndpointAddress {
                address: "192.0.2.80".parse().unwrap(),
                port: 5080,
            },
        );
        assert!(validate_multimedia_transmit(&state, &descriptor).is_ok());
        assert_eq!(state.media_capabilities.audio(), audio_before);

        state.media_capabilities = StationMediaCapabilities::new(
            audio_before.clone(),
            vec![crate::message::capabilities::VideoCapability {
                direction: ReceiveTransmit::RECEIVE,
                ..capability.clone()
            }],
        );
        assert!(matches!(
            validate_multimedia_transmit(&state, &descriptor),
            Err(ServerError::UnsupportedMultimediaTransmit)
        ));
        state.media_capabilities =
            StationMediaCapabilities::new(audio_before.clone(), vec![capability]);

        for invalid in [
            MultimediaTransmitDescriptor {
                payload: video_receive_descriptor(
                    80,
                    MediaEndpointAddress {
                        address: "192.0.2.80".parse().unwrap(),
                        port: 5080,
                    },
                )
                .payload,
                ..descriptor.clone()
            },
            MultimediaTransmitDescriptor {
                endpoint: MediaEndpointAddress {
                    address: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                    port: 5080,
                },
                ..descriptor.clone()
            },
        ] {
            assert!(matches!(
                invalid.validate(),
                Err(ServerError::InvalidMultimediaTransmit(_))
            ));
        }

        for protocol in [ProtocolVersion::V3, ProtocolVersion::V10] {
            state.registration.protocol = protocol;
            let mut legacy = descriptor.clone();
            legacy.payload = MultimediaPayload::from_wire(
                0,
                test_rtp_payload_number(98),
                [0x5a; crate::MULTIMEDIA_CAPABILITY_BYTES],
                Codec::H264,
                MultimediaPayloadDirection::Transmit,
                protocol,
            );
            assert!(validate_multimedia_transmit(&state, &legacy).is_ok());
        }

        state.registration.protocol = ProtocolVersion::V16;
        let ipv6 = MultimediaTransmitDescriptor {
            endpoint: MediaEndpointAddress {
                address: "2001:db8::80".parse().unwrap(),
                port: 5080,
            },
            ..descriptor
        };
        assert!(matches!(
            validate_multimedia_transmit(&state, &ipv6),
            Err(ServerError::InvalidMultimediaTransmit(_))
        ));
        assert_eq!(state.media_capabilities.audio(), audio_before);
    }

    #[test]
    fn multimedia_transmit_controls_encode_only_their_typed_parameter_words() {
        fn assert_control(
            control: MultimediaTransmitControl,
            expected_command: MiscCommandType,
            expected_words: &[u32],
        ) {
            let (command, data) = encode_multimedia_transmit_control(control).unwrap();
            let expected = expected_words
                .iter()
                .flat_map(|word| word.to_le_bytes())
                .collect::<Vec<_>>();
            assert_eq!(command, expected_command);
            assert_eq!(data.as_bytes(), expected);
        }

        assert_control(
            MultimediaTransmitControl::FreezePicture,
            MiscCommandType::VideoFreezePicture,
            &[],
        );
        assert_control(
            MultimediaTransmitControl::FastPictureUpdate {
                first_gob: 1,
                gob_count: 2,
            },
            MiscCommandType::VideoFastUpdatePicture,
            &[1, 2],
        );
        assert_control(
            MultimediaTransmitControl::FastGobUpdate {
                first_gob: 3,
                gob_count: 4,
            },
            MiscCommandType::VideoFastUpdateGob,
            &[3, 4],
        );
        assert_control(
            MultimediaTransmitControl::FastMacroblockUpdate {
                first_gob: 5,
                first_macroblock: 6,
                macroblock_count: 7,
            },
            MiscCommandType::VideoFastUpdateMacroblock,
            &[5, 6, 7],
        );
        assert_control(
            MultimediaTransmitControl::LostPicture {
                picture_number: 8,
                long_term_picture_index: 9,
            },
            MiscCommandType::LostPicture,
            &[8, 9],
        );
        assert_control(
            MultimediaTransmitControl::LostPartialPicture {
                picture_number: 10,
                long_term_picture_index: 11,
                first_macroblock: 12,
                macroblock_count: 13,
            },
            MiscCommandType::LostPartialPicture,
            &[10, 11, 12, 13],
        );
        let pictures = VideoPictureReferences::new([
            VideoPictureReference {
                picture_number: 14,
                long_term_picture_index: 15,
            },
            VideoPictureReference {
                picture_number: 16,
                long_term_picture_index: 17,
            },
        ])
        .unwrap();
        assert_control(
            MultimediaTransmitControl::RecoveryReferencePicture { pictures },
            MiscCommandType::RecoveryReferencePicture,
            &[2, 14, 15, 16, 17],
        );
        assert_control(
            MultimediaTransmitControl::TemporalSpatialTradeoff { value: 18 },
            MiscCommandType::TemporalSpatialTradeoff,
            &[18],
        );
        assert!(matches!(
            VideoPictureReferences::new(std::iter::repeat(VideoPictureReference {
                picture_number: 1,
                long_term_picture_index: 2,
            })),
            Err(ServerError::InvalidMultimediaTransmitControl(_))
        ));
    }

    #[tokio::test]
    async fn video_receive_session_correlates_fragmented_acknowledgements_and_preserves_audio() {
        let device = definition();
        let device_id = device.id.clone();
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let (server, handle, mut events) = Server::bind(config, [device]).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let mut phone = TcpStream::connect(address).await.unwrap();
        let mut decoder = FrameDecoder::new();
        let protocol = ProtocolVersion::V22;
        let call_id = CallId::new(71);

        phone.write_all(&register_bytes(protocol)).await.unwrap();
        read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                event: DeviceEventKind::Registered(_),
                ..
            }))
        ));
        phone
            .write_all(&capability_update_bytes(
                protocol,
                Codec::Pcmu,
                Codec::H264,
                71,
            ))
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                event: DeviceEventKind::Capabilities { .. },
                ..
            }))
        ));

        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::BeginCall {
                    line_instance: LineInstance::new(1),
                    call_id,
                    codec: Codec::Pcmu,
                },
            ))
            .await
            .unwrap();
        let begin = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
            matches!(message, ServerMessage::SelectSoftKeys { .. })
        })
        .await;
        let wire_reference = begin
            .iter()
            .find_map(|message| match message {
                ServerMessage::CallState { call_reference, .. } => {
                    Some(CallReference::new(*call_reference))
                }
                _ => None,
            })
            .expect("begin call omitted its wire identity");
        assert!(matches!(
            handle
                .send_confirmed(Command::new(
                    device_id.clone(),
                    CommandAction::OpenMultimediaReceiveChannel {
                        call_id,
                        descriptor: video_receive_descriptor(
                            699,
                            MediaEndpointAddress {
                                address: "192.0.2.69".parse().unwrap(),
                                port: 5068,
                            },
                        ),
                    },
                ))
                .await,
            Err(ServerError::CommandWrite(_))
        ));
        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::SetCallState {
                    call_id,
                    state: CallState::Connected,
                },
            ))
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, id::SELECT_SOFT_KEYS).await;

        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::OpenReceiveChannel {
                    call_id,
                    source: None,
                    codec: Codec::Pcmu,
                    packet_ms: 20,
                    max_frames_per_packet: 2,
                    dtmf_mode: DtmfMode::Rfc2833,
                    audio_processing: AudioProcessingPolicy::default(),
                },
            ))
            .await
            .unwrap();
        let audio_open = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
            matches!(message, ServerMessage::OpenReceiveChannel { .. })
        })
        .await;
        let audio_token = audio_open
            .iter()
            .find_map(|message| match message {
                ServerMessage::OpenReceiveChannel {
                    passthrough_party_id,
                    ..
                } => Some(*passthrough_party_id),
                _ => None,
            })
            .unwrap();

        let first_descriptor = video_receive_descriptor(
            700,
            MediaEndpointAddress {
                address: "192.0.2.70".parse().unwrap(),
                port: 5070,
            },
        );
        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::OpenMultimediaReceiveChannel {
                    call_id,
                    descriptor: first_descriptor.clone(),
                },
            ))
            .await
            .unwrap();
        let first_open = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
            matches!(message, ServerMessage::OpenMultimediaChannel(_))
        })
        .await
        .into_iter()
        .find_map(|message| match message {
            ServerMessage::OpenMultimediaChannel(open) => Some(open),
            _ => None,
        })
        .unwrap();
        assert_eq!(first_open.payload.codec(), Codec::H264);
        assert_eq!(first_open.line_instance, 1);
        assert_eq!(first_open.call_reference, wire_reference);
        assert_eq!(first_open.payload, first_descriptor.payload);
        let first_token = first_open.passthrough_party_id;
        let endpoint = MediaEndpointAddress {
            address: "198.51.100.70".parse().unwrap(),
            port: 6070,
        };
        let wrong_call = ClientMessage::OpenMultimediaReceiveChannelAck(
            crate::OpenMultimediaReceiveChannelAck {
                status: MediaStatus::Ok,
                endpoint,
                passthrough_party_id: first_token,
                call_reference: CallReference::new(wire_reference.get() + 1),
            },
        )
        .encode(protocol)
        .unwrap();
        let exact = ClientMessage::OpenMultimediaReceiveChannelAck(
            crate::OpenMultimediaReceiveChannelAck {
                status: MediaStatus::Ok,
                endpoint,
                passthrough_party_id: first_token,
                call_reference: wire_reference,
            },
        )
        .encode(protocol)
        .unwrap();
        let mut coalesced_prefix = wrong_call;
        coalesced_prefix.extend(
            ClientMessage::OpenMultimediaReceiveChannelAck(
                crate::OpenMultimediaReceiveChannelAck {
                    status: MediaStatus::Ok,
                    endpoint: MediaEndpointAddress {
                        address: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                        port: endpoint.port,
                    },
                    passthrough_party_id: first_token,
                    call_reference: wire_reference,
                },
            )
            .encode(protocol)
            .unwrap(),
        );
        coalesced_prefix.push(exact[0]);
        phone.write_all(&coalesced_prefix).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(25), events.recv())
                .await
                .is_err()
        );
        for fragment in exact[1..].chunks(3) {
            phone.write_all(fragment).await.unwrap();
        }
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                event: DeviceEventKind::MultimediaReceiveChannelOpened {
                    call_id: actual_call,
                    codec: Codec::H264,
                    endpoint: actual_endpoint,
                    passthrough_party_id,
                },
                ..
            })) if actual_call == call_id
                && actual_endpoint == endpoint
                && passthrough_party_id == first_token
        ));
        phone.write_all(&exact).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(25), events.recv())
                .await
                .is_err()
        );

        phone
            .write_all(
                &ClientMessage::OpenReceiveChannelAck {
                    status: MediaStatus::Ok,
                    address: "198.51.100.71".parse().unwrap(),
                    port: 6072,
                    call_reference: wire_reference.get(),
                    passthrough_party_id: audio_token,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                event: DeviceEventKind::ReceiveChannelOpened {
                    call_id: actual_call,
                    status: MediaStatus::Ok,
                    ..
                },
                ..
            })) if actual_call == call_id
        ));

        let replacement_descriptor = video_receive_descriptor(
            701,
            MediaEndpointAddress {
                address: "192.0.2.71".parse().unwrap(),
                port: 5072,
            },
        );
        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::OpenMultimediaReceiveChannel {
                    call_id,
                    descriptor: replacement_descriptor,
                },
            ))
            .await
            .unwrap();
        let replacement =
            read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
                matches!(
                    message,
                    ServerMessage::OpenMultimediaChannel(open)
                        if open.conference_id == ConferenceId::new(701)
                )
            })
            .await;
        let close_index = replacement
            .iter()
            .position(|message| {
                matches!(
                    message,
                    ServerMessage::CloseMultimediaReceiveChannel(control)
                        if control.passthrough_party_id == first_token
                )
            })
            .expect("replacement omitted the old video close");
        let (open_index, replacement_open) = replacement
            .iter()
            .enumerate()
            .find_map(|(index, message)| match message {
                ServerMessage::OpenMultimediaChannel(open)
                    if open.conference_id == ConferenceId::new(701) =>
                {
                    Some((index, open))
                }
                _ => None,
            })
            .unwrap();
        assert!(close_index < open_index);
        assert_ne!(replacement_open.passthrough_party_id, first_token);

        let negative = ClientMessage::OpenMultimediaReceiveChannelAck(
            crate::OpenMultimediaReceiveChannelAck {
                status: MediaStatus::OutOfChannels,
                endpoint,
                passthrough_party_id: replacement_open.passthrough_party_id,
                call_reference: wire_reference,
            },
        )
        .encode(protocol)
        .unwrap();
        phone.write_all(&exact).await.unwrap();
        phone.write_all(&negative).await.unwrap();
        read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
            matches!(
                message,
                ServerMessage::CloseMultimediaReceiveChannel(control)
                    if control.passthrough_party_id
                        == replacement_open.passthrough_party_id
            )
        })
        .await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                event: DeviceEventKind::MultimediaReceiveChannelFailed {
                    call_id: actual_call,
                    codec: Codec::H264,
                    status: MediaStatus::OutOfChannels,
                    endpoint: actual_endpoint,
                    passthrough_party_id,
                },
                ..
            })) if actual_call == call_id
                && actual_endpoint == endpoint
                && passthrough_party_id == replacement_open.passthrough_party_id
        ));
        phone.write_all(&negative).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(25), events.recv())
                .await
                .is_err()
        );

        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::OpenMultimediaReceiveChannel {
                    call_id,
                    descriptor: video_receive_descriptor(
                        702,
                        MediaEndpointAddress {
                            address: "192.0.2.72".parse().unwrap(),
                            port: 5074,
                        },
                    ),
                },
            ))
            .await
            .unwrap();
        let final_open = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
            matches!(
                message,
                ServerMessage::OpenMultimediaChannel(open)
                    if open.conference_id == ConferenceId::new(702)
            )
        })
        .await
        .into_iter()
        .find_map(|message| match message {
            ServerMessage::OpenMultimediaChannel(open)
                if open.conference_id == ConferenceId::new(702) =>
            {
                Some(open)
            }
            _ => None,
        })
        .unwrap();
        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::CloseCall { call_id },
            ))
            .await
            .unwrap();
        let close_messages =
            read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
                matches!(
                    message,
                    ServerMessage::CallState {
                        state: CallState::OnHook,
                        ..
                    }
                )
            })
            .await;
        let video_close = close_messages
            .iter()
            .position(|message| {
                matches!(
                    message,
                    ServerMessage::CloseMultimediaReceiveChannel(control)
                        if control.passthrough_party_id
                            == final_open.passthrough_party_id
                )
            })
            .expect("call close omitted the video receive leg");
        let audio_close = close_messages
            .iter()
            .position(|message| {
                matches!(
                    message,
                    ServerMessage::CloseReceiveChannel(control)
                        if control.passthrough_party_id.get() == audio_token
                )
            })
            .expect("call close omitted the independently opened audio receive leg");
        let on_hook = close_messages
            .iter()
            .position(|message| {
                matches!(
                    message,
                    ServerMessage::CallState {
                        state: CallState::OnHook,
                        ..
                    }
                )
            })
            .unwrap();
        assert!(video_close < audio_close && audio_close < on_hook);

        let reconfigured_call_id = CallId::new(73);
        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::BeginCall {
                    line_instance: LineInstance::new(1),
                    call_id: reconfigured_call_id,
                    codec: Codec::Pcmu,
                },
            ))
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, id::SELECT_SOFT_KEYS).await;
        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::SetCallState {
                    call_id: reconfigured_call_id,
                    state: CallState::Connected,
                },
            ))
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, id::SELECT_SOFT_KEYS).await;
        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::OpenMultimediaReceiveChannel {
                    call_id: reconfigured_call_id,
                    descriptor: video_receive_descriptor(
                        705,
                        MediaEndpointAddress {
                            address: "192.0.2.75".parse().unwrap(),
                            port: 5080,
                        },
                    ),
                },
            ))
            .await
            .unwrap();
        let reconfigure_open =
            read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
                matches!(
                    message,
                    ServerMessage::OpenMultimediaChannel(open)
                        if open.conference_id == ConferenceId::new(705)
                )
            })
            .await
            .into_iter()
            .find_map(|message| match message {
                ServerMessage::OpenMultimediaChannel(open)
                    if open.conference_id == ConferenceId::new(705) =>
                {
                    Some(open)
                }
                _ => None,
            })
            .unwrap();
        handle
            .send_confirmed(Command::new(
                device_id,
                CommandAction::StartMultimediaTransmission {
                    call_id: reconfigured_call_id,
                    descriptor: video_transmit_descriptor(
                        706,
                        MediaEndpointAddress {
                            address: "192.0.2.76".parse().unwrap(),
                            port: 5082,
                        },
                    ),
                },
            ))
            .await
            .unwrap();
        let reconfigure_start =
            read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
                matches!(
                    message,
                    ServerMessage::StartMultimediaTransmission(start)
                        if start.conference_id == ConferenceId::new(706)
                )
            })
            .await
            .into_iter()
            .find_map(|message| match message {
                ServerMessage::StartMultimediaTransmission(start)
                    if start.conference_id == ConferenceId::new(706) =>
                {
                    Some(start)
                }
                _ => None,
            })
            .unwrap();
        let mut replacement = definition();
        replacement.description = "replacement".into();
        handle.reconfigure([replacement]).await.unwrap();
        let reconfigure_messages =
            read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
                matches!(
                    message,
                    ServerMessage::StopMultimediaTransmission(control)
                        if control.passthrough_party_id
                            == reconfigure_start.passthrough_party_id
                )
            })
            .await;
        let receive_close = reconfigure_messages
            .iter()
            .position(|message| {
                matches!(
                    message,
                    ServerMessage::CloseMultimediaReceiveChannel(control)
                        if control.passthrough_party_id
                            == reconfigure_open.passthrough_party_id
                )
            })
            .expect("reconfigure omitted the video receive close");
        let transmit_stop = reconfigure_messages
            .iter()
            .position(|message| {
                matches!(
                    message,
                    ServerMessage::StopMultimediaTransmission(control)
                        if control.passthrough_party_id
                            == reconfigure_start.passthrough_party_id
                )
            })
            .expect("reconfigure omitted the video transmit stop");
        assert!(receive_close < transmit_stop);
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                event: DeviceEventKind::Disconnected {},
                ..
            }))
        ));

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn video_transmit_session_correlates_frames_and_preserves_receive_and_audio() {
        let device = definition();
        let device_id = device.id.clone();
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let (server, handle, mut events) = Server::bind(config, [device]).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let mut phone = TcpStream::connect(address).await.unwrap();
        let mut decoder = FrameDecoder::new();
        let protocol = ProtocolVersion::V22;
        let call_id = CallId::new(81);

        phone.write_all(&register_bytes(protocol)).await.unwrap();
        read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                event: DeviceEventKind::Registered(_),
                ..
            }))
        ));
        phone
            .write_all(&capability_update_bytes(
                protocol,
                Codec::Pcmu,
                Codec::H264,
                81,
            ))
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                event: DeviceEventKind::Capabilities { .. },
                ..
            }))
        ));
        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::BeginCall {
                    line_instance: LineInstance::new(1),
                    call_id,
                    codec: Codec::Pcmu,
                },
            ))
            .await
            .unwrap();
        let begin = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
            matches!(message, ServerMessage::SelectSoftKeys { .. })
        })
        .await;
        let call_reference = begin
            .iter()
            .find_map(|message| match message {
                ServerMessage::CallState { call_reference, .. } => {
                    Some(CallReference::new(*call_reference))
                }
                _ => None,
            })
            .unwrap();
        assert!(matches!(
            handle
                .send_confirmed(Command::new(
                    device_id.clone(),
                    CommandAction::StartMultimediaTransmission {
                        call_id,
                        descriptor: video_transmit_descriptor(
                            809,
                            MediaEndpointAddress {
                                address: "192.0.2.89".parse().unwrap(),
                                port: 5088,
                            },
                        ),
                    },
                ))
                .await,
            Err(ServerError::CommandWrite(_))
        ));
        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::SetCallState {
                    call_id,
                    state: CallState::Connected,
                },
            ))
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, id::SELECT_SOFT_KEYS).await;

        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::OpenReceiveChannel {
                    call_id,
                    source: None,
                    codec: Codec::Pcmu,
                    packet_ms: 20,
                    max_frames_per_packet: 2,
                    dtmf_mode: DtmfMode::Rfc2833,
                    audio_processing: AudioProcessingPolicy::default(),
                },
            ))
            .await
            .unwrap();
        let audio_open = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
            matches!(message, ServerMessage::OpenReceiveChannel { .. })
        })
        .await;
        let audio_token = audio_open
            .iter()
            .find_map(|message| match message {
                ServerMessage::OpenReceiveChannel {
                    passthrough_party_id,
                    ..
                } => Some(*passthrough_party_id),
                _ => None,
            })
            .unwrap();

        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::OpenMultimediaReceiveChannel {
                    call_id,
                    descriptor: video_receive_descriptor(
                        810,
                        MediaEndpointAddress {
                            address: "192.0.2.81".parse().unwrap(),
                            port: 5082,
                        },
                    ),
                },
            ))
            .await
            .unwrap();
        let receive_open =
            read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
                matches!(message, ServerMessage::OpenMultimediaChannel(_))
            })
            .await
            .into_iter()
            .find_map(|message| match message {
                ServerMessage::OpenMultimediaChannel(open) => Some(open),
                _ => None,
            })
            .unwrap();

        let first_descriptor = video_transmit_descriptor(
            811,
            MediaEndpointAddress {
                address: "192.0.2.82".parse().unwrap(),
                port: 5084,
            },
        );
        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::StartMultimediaTransmission {
                    call_id,
                    descriptor: first_descriptor.clone(),
                },
            ))
            .await
            .unwrap();
        let first_start =
            read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
                matches!(message, ServerMessage::StartMultimediaTransmission(_))
            })
            .await
            .into_iter()
            .find_map(|message| match message {
                ServerMessage::StartMultimediaTransmission(start) => Some(start),
                _ => None,
            })
            .unwrap();
        assert_eq!(first_start.call_reference, call_reference);
        assert_eq!(first_start.endpoint, first_descriptor.endpoint);
        assert_eq!(first_start.payload, first_descriptor.payload);
        let first_token = first_start.passthrough_party_id;
        assert!(matches!(
            handle
                .send_confirmed(Command::new(
                    device_id.clone(),
                    CommandAction::ControlMultimediaTransmission {
                        call_id,
                        passthrough_party_id: first_token,
                        control: MultimediaTransmitControl::FreezePicture,
                    },
                ))
                .await,
            Err(ServerError::CommandWrite(_))
        ));
        let station_endpoint = MediaEndpointAddress {
            address: "198.51.100.81".parse().unwrap(),
            port: 6082,
        };
        let wrong_conference =
            ClientMessage::StartMultimediaTransmissionAck(crate::StartMultimediaTransmissionAck {
                conference_id: ConferenceId::new(812),
                passthrough_party_id: first_token,
                call_reference,
                endpoint: station_endpoint,
                status: MediaStatus::Ok,
            })
            .encode(protocol)
            .unwrap();
        let wrong_call =
            ClientMessage::StartMultimediaTransmissionAck(crate::StartMultimediaTransmissionAck {
                conference_id: first_start.conference_id,
                passthrough_party_id: first_token,
                call_reference: CallReference::new(call_reference.get() + 1),
                endpoint: station_endpoint,
                status: MediaStatus::Ok,
            })
            .encode(protocol)
            .unwrap();
        let exact =
            ClientMessage::StartMultimediaTransmissionAck(crate::StartMultimediaTransmissionAck {
                conference_id: first_start.conference_id,
                passthrough_party_id: first_token,
                call_reference,
                endpoint: station_endpoint,
                status: MediaStatus::Ok,
            })
            .encode(protocol)
            .unwrap();
        let mut coalesced = wrong_conference;
        coalesced.extend(wrong_call);
        coalesced.extend(
            ClientMessage::StartMultimediaTransmissionAck(crate::StartMultimediaTransmissionAck {
                conference_id: first_start.conference_id,
                passthrough_party_id: PassthroughPartyId::new(first_token.get() + 1),
                call_reference,
                endpoint: station_endpoint,
                status: MediaStatus::Ok,
            })
            .encode(protocol)
            .unwrap(),
        );
        coalesced.extend(
            ClientMessage::StartMultimediaTransmissionAck(crate::StartMultimediaTransmissionAck {
                conference_id: first_start.conference_id,
                passthrough_party_id: first_token,
                call_reference,
                endpoint: MediaEndpointAddress {
                    address: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                    port: station_endpoint.port,
                },
                status: MediaStatus::Ok,
            })
            .encode(protocol)
            .unwrap(),
        );
        coalesced.push(exact[0]);
        phone.write_all(&coalesced).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(25), events.recv())
                .await
                .is_err()
        );
        for fragment in exact[1..].chunks(3) {
            phone.write_all(fragment).await.unwrap();
        }
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                event: DeviceEventKind::MultimediaTransmitStarted {
                    call_id: actual_call,
                    codec: Codec::H264,
                    endpoint,
                    passthrough_party_id,
                },
                ..
            })) if actual_call == call_id
                && endpoint == station_endpoint
                && passthrough_party_id == first_token
        ));
        phone.write_all(&exact).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(25), events.recv())
                .await
                .is_err()
        );

        assert!(matches!(
            handle
                .send_confirmed(Command::new(
                    device_id.clone(),
                    CommandAction::SetMultimediaTransmitBitRate {
                        call_id,
                        passthrough_party_id: PassthroughPartyId::new(first_token.get() + 1),
                        maximum_bit_rate: 512_000,
                    },
                ))
                .await,
            Err(ServerError::CommandWrite(_))
        ));
        for action in [
            CommandAction::SetMultimediaTransmitBitRate {
                call_id,
                passthrough_party_id: first_token,
                maximum_bit_rate: 512_000,
            },
            CommandAction::NotifyMultimediaTransmitBitRate {
                call_id,
                passthrough_party_id: first_token,
                maximum_bit_rate: 384_000,
            },
            CommandAction::ControlMultimediaTransmission {
                call_id,
                passthrough_party_id: first_token,
                control: MultimediaTransmitControl::FastPictureUpdate {
                    first_gob: 4,
                    gob_count: 2,
                },
            },
        ] {
            handle
                .send_confirmed(Command::new(device_id.clone(), action))
                .await
                .unwrap();
        }
        let control_messages =
            read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
                matches!(message, ServerMessage::MiscellaneousCommand(_))
            })
            .await;
        assert!(matches!(
            control_messages.as_slice(),
            [
                ServerMessage::FlowControlCommand(VideoFlowControl {
                    conference_id,
                    passthrough_party_id,
                    call_reference: actual_call,
                    maximum_bit_rate: 512_000,
                }),
                ServerMessage::FlowControlNotify(VideoFlowControl {
                    conference_id: notify_conference,
                    passthrough_party_id: notify_token,
                    call_reference: notify_call,
                    maximum_bit_rate: 384_000,
                }),
                ServerMessage::MiscellaneousCommand(MiscellaneousCommand {
                    conference_id: command_conference,
                    passthrough_party_id: command_token,
                    call_reference: command_call,
                    command: MiscCommandType::VideoFastUpdatePicture,
                    data,
                }),
            ] if *conference_id == first_start.conference_id
                && *passthrough_party_id == first_token
                && *actual_call == call_reference
                && *notify_conference == first_start.conference_id
                && *notify_token == first_token
                && *notify_call == call_reference
                && *command_conference == first_start.conference_id
                && *command_token == first_token
                && *command_call == call_reference
                && data.as_bytes()[..8]
                    == [4_u32.to_le_bytes(), 2_u32.to_le_bytes()].concat()
                && data.as_bytes()[8..].iter().all(|byte| *byte == 0)
        ));

        phone
            .write_all(
                &ClientMessage::OpenMultimediaReceiveChannelAck(
                    crate::OpenMultimediaReceiveChannelAck {
                        status: MediaStatus::Ok,
                        endpoint: MediaEndpointAddress {
                            address: "198.51.100.82".parse().unwrap(),
                            port: 6084,
                        },
                        passthrough_party_id: receive_open.passthrough_party_id,
                        call_reference,
                    },
                )
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                event: DeviceEventKind::MultimediaReceiveChannelOpened {
                    call_id: actual_call,
                    ..
                },
                ..
            })) if actual_call == call_id
        ));
        phone
            .write_all(
                &ClientMessage::OpenReceiveChannelAck {
                    status: MediaStatus::Ok,
                    address: "198.51.100.83".parse().unwrap(),
                    port: 6086,
                    call_reference: call_reference.get(),
                    passthrough_party_id: audio_token,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                event: DeviceEventKind::ReceiveChannelOpened {
                    call_id: actual_call,
                    status: MediaStatus::Ok,
                    ..
                },
                ..
            })) if actual_call == call_id
        ));

        let replacement_descriptor = video_transmit_descriptor(
            813,
            MediaEndpointAddress {
                address: "192.0.2.83".parse().unwrap(),
                port: 5086,
            },
        );
        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::StartMultimediaTransmission {
                    call_id,
                    descriptor: replacement_descriptor,
                },
            ))
            .await
            .unwrap();
        let replacement =
            read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
                matches!(
                    message,
                    ServerMessage::StartMultimediaTransmission(start)
                        if start.conference_id == ConferenceId::new(813)
                )
            })
            .await;
        let stop_index = replacement
            .iter()
            .position(|message| {
                matches!(
                    message,
                    ServerMessage::StopMultimediaTransmission(control)
                        if control.passthrough_party_id == first_token
                )
            })
            .expect("replacement omitted the old video transmit stop");
        let (start_index, replacement_start) = replacement
            .iter()
            .enumerate()
            .find_map(|(index, message)| match message {
                ServerMessage::StartMultimediaTransmission(start)
                    if start.conference_id == ConferenceId::new(813) =>
                {
                    Some((index, start))
                }
                _ => None,
            })
            .unwrap();
        assert!(stop_index < start_index);
        assert_ne!(replacement_start.passthrough_party_id, first_token);
        for passthrough_party_id in [first_token, replacement_start.passthrough_party_id] {
            assert!(matches!(
                handle
                    .send_confirmed(Command::new(
                        device_id.clone(),
                        CommandAction::NotifyMultimediaTransmitBitRate {
                            call_id,
                            passthrough_party_id,
                            maximum_bit_rate: 256_000,
                        },
                    ))
                    .await,
                Err(ServerError::CommandWrite(_))
            ));
        }

        let negative =
            ClientMessage::StartMultimediaTransmissionAck(crate::StartMultimediaTransmissionAck {
                conference_id: replacement_start.conference_id,
                passthrough_party_id: replacement_start.passthrough_party_id,
                call_reference,
                endpoint: station_endpoint,
                status: MediaStatus::OutOfChannels,
            })
            .encode(protocol)
            .unwrap();
        phone.write_all(&exact).await.unwrap();
        phone.write_all(&negative).await.unwrap();
        read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
            matches!(
                message,
                ServerMessage::StopMultimediaTransmission(control)
                    if control.passthrough_party_id
                        == replacement_start.passthrough_party_id
            )
        })
        .await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                event: DeviceEventKind::MultimediaTransmitFailed {
                    call_id: actual_call,
                    codec: Codec::H264,
                    status: MediaStatus::OutOfChannels,
                    endpoint,
                    passthrough_party_id,
                },
                ..
            })) if actual_call == call_id
                && endpoint == station_endpoint
                && passthrough_party_id == replacement_start.passthrough_party_id
        ));
        phone.write_all(&negative).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(25), events.recv())
                .await
                .is_err()
        );

        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::StartMultimediaTransmission {
                    call_id,
                    descriptor: video_transmit_descriptor(
                        814,
                        MediaEndpointAddress {
                            address: "192.0.2.84".parse().unwrap(),
                            port: 5088,
                        },
                    ),
                },
            ))
            .await
            .unwrap();
        let stopped_start =
            read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
                matches!(
                    message,
                    ServerMessage::StartMultimediaTransmission(start)
                        if start.conference_id == ConferenceId::new(814)
                )
            })
            .await
            .into_iter()
            .find_map(|message| match message {
                ServerMessage::StartMultimediaTransmission(start)
                    if start.conference_id == ConferenceId::new(814) =>
                {
                    Some(start)
                }
                _ => None,
            })
            .unwrap();
        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::StopMultimediaTransmission { call_id },
            ))
            .await
            .unwrap();
        read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
            matches!(
                message,
                ServerMessage::StopMultimediaTransmission(control)
                    if control.passthrough_party_id
                        == stopped_start.passthrough_party_id
            )
        })
        .await;
        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::StopMultimediaTransmission { call_id },
            ))
            .await
            .unwrap();
        phone
            .write_all(&ClientMessage::KeepAlive.encode(protocol).unwrap())
            .await
            .unwrap();
        let after_duplicate_stop =
            read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
                matches!(message, ServerMessage::KeepAliveAck)
            })
            .await;
        assert!(!after_duplicate_stop.iter().any(|message| {
            matches!(
                message,
                ServerMessage::StopMultimediaTransmission(control)
                    if control.passthrough_party_id
                        == stopped_start.passthrough_party_id
            )
        }));

        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::StartMultimediaTransmission {
                    call_id,
                    descriptor: video_transmit_descriptor(
                        815,
                        MediaEndpointAddress {
                            address: "192.0.2.85".parse().unwrap(),
                            port: 5090,
                        },
                    ),
                },
            ))
            .await
            .unwrap();
        let final_start =
            read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
                matches!(
                    message,
                    ServerMessage::StartMultimediaTransmission(start)
                        if start.conference_id == ConferenceId::new(815)
                )
            })
            .await
            .into_iter()
            .find_map(|message| match message {
                ServerMessage::StartMultimediaTransmission(start)
                    if start.conference_id == ConferenceId::new(815) =>
                {
                    Some(start)
                }
                _ => None,
            })
            .unwrap();
        handle
            .send_confirmed(Command::new(
                device_id,
                CommandAction::CloseCall { call_id },
            ))
            .await
            .unwrap();
        let close_messages =
            read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
                matches!(
                    message,
                    ServerMessage::CallState {
                        state: CallState::OnHook,
                        ..
                    }
                )
            })
            .await;
        let video_receive_close = close_messages
            .iter()
            .position(|message| {
                matches!(
                    message,
                    ServerMessage::CloseMultimediaReceiveChannel(control)
                        if control.passthrough_party_id
                            == receive_open.passthrough_party_id
                )
            })
            .unwrap();
        let video_transmit_stop = close_messages
            .iter()
            .position(|message| {
                matches!(
                    message,
                    ServerMessage::StopMultimediaTransmission(control)
                        if control.passthrough_party_id
                            == final_start.passthrough_party_id
                )
            })
            .unwrap();
        let audio_close = close_messages
            .iter()
            .position(|message| {
                matches!(
                    message,
                    ServerMessage::CloseReceiveChannel(control)
                        if control.passthrough_party_id.get() == audio_token
                )
            })
            .unwrap();
        let on_hook = close_messages
            .iter()
            .position(|message| {
                matches!(
                    message,
                    ServerMessage::CallState {
                        state: CallState::OnHook,
                        ..
                    }
                )
            })
            .unwrap();
        assert!(
            video_receive_close < video_transmit_stop
                && video_transmit_stop < audio_close
                && audio_close < on_hook
        );

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn video_receive_deadline_closes_and_retires_the_exact_generation() {
        let device = definition();
        let device_id = device.id.clone();
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let (server, handle, mut events, ingress) = Server::with_ingress(config, [device]).unwrap();
        let task = tokio::spawn(server.run());
        let (server_stream, mut phone) = tokio::io::duplex(8_192);
        ingress
            .accept(
                server_stream,
                SocketAddr::from(([127, 0, 0, 1], 40_071)),
                SocketAddr::from(([127, 0, 0, 1], 2_000)),
                StationTransport::Clear,
            )
            .await
            .unwrap();
        let mut decoder = FrameDecoder::new();
        let protocol = ProtocolVersion::V22;
        let call_id = CallId::new(72);

        phone.write_all(&register_bytes(protocol)).await.unwrap();
        read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                event: DeviceEventKind::Registered(_),
                ..
            }))
        ));
        phone
            .write_all(&capability_update_bytes(
                protocol,
                Codec::Pcmu,
                Codec::H264,
                72,
            ))
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                event: DeviceEventKind::Capabilities { .. },
                ..
            }))
        ));
        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::BeginCall {
                    line_instance: LineInstance::new(1),
                    call_id,
                    codec: Codec::Pcmu,
                },
            ))
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, id::SELECT_SOFT_KEYS).await;
        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::SetCallState {
                    call_id,
                    state: CallState::Connected,
                },
            ))
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, id::SELECT_SOFT_KEYS).await;
        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::OpenMultimediaReceiveChannel {
                    call_id,
                    descriptor: video_receive_descriptor(
                        703,
                        MediaEndpointAddress {
                            address: "192.0.2.73".parse().unwrap(),
                            port: 5076,
                        },
                    ),
                },
            ))
            .await
            .unwrap();
        let open = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
            matches!(message, ServerMessage::OpenMultimediaChannel(_))
        })
        .await
        .into_iter()
        .find_map(|message| match message {
            ServerMessage::OpenMultimediaChannel(open) => Some(open),
            _ => None,
        })
        .unwrap();

        tokio::time::advance(HANDSET_ACKNOWLEDGEMENT_TIMEOUT + Duration::from_millis(100)).await;
        read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
            matches!(
                message,
                ServerMessage::CloseMultimediaReceiveChannel(control)
                    if control.passthrough_party_id == open.passthrough_party_id
            )
        })
        .await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                event: DeviceEventKind::MultimediaReceiveChannelTimedOut {
                    call_id: actual_call,
                    codec: Codec::H264,
                    passthrough_party_id,
                },
                ..
            })) if actual_call == call_id
                && passthrough_party_id == open.passthrough_party_id
        ));

        phone
            .write_all(
                &ClientMessage::OpenMultimediaReceiveChannelAck(
                    crate::OpenMultimediaReceiveChannelAck {
                        status: MediaStatus::Ok,
                        endpoint: MediaEndpointAddress {
                            address: "198.51.100.73".parse().unwrap(),
                            port: 6076,
                        },
                        passthrough_party_id: open.passthrough_party_id,
                        call_reference: open.call_reference,
                    },
                )
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(25), events.recv())
                .await
                .is_err()
        );
        phone
            .write_all(&ClientMessage::KeepAlive.encode(protocol).unwrap())
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, id::KEEP_ALIVE_ACK).await;

        handle
            .send_confirmed(Command::new(
                device_id,
                CommandAction::OpenMultimediaReceiveChannel {
                    call_id,
                    descriptor: video_receive_descriptor(
                        704,
                        MediaEndpointAddress {
                            address: "192.0.2.74".parse().unwrap(),
                            port: 5078,
                        },
                    ),
                },
            ))
            .await
            .unwrap();
        let shutdown_open =
            read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
                matches!(
                    message,
                    ServerMessage::OpenMultimediaChannel(open)
                        if open.conference_id == ConferenceId::new(704)
                )
            })
            .await
            .into_iter()
            .find_map(|message| match message {
                ServerMessage::OpenMultimediaChannel(open)
                    if open.conference_id == ConferenceId::new(704) =>
                {
                    Some(open)
                }
                _ => None,
            })
            .unwrap();
        handle.shutdown().await.unwrap();
        read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
            matches!(
                message,
                ServerMessage::CloseMultimediaReceiveChannel(control)
                    if control.passthrough_party_id == shutdown_open.passthrough_party_id
            )
        })
        .await;
        task.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn video_transmit_deadline_stops_and_retires_the_exact_generation() {
        let device = definition();
        let device_id = device.id.clone();
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let (server, handle, mut events, ingress) = Server::with_ingress(config, [device]).unwrap();
        let task = tokio::spawn(server.run());
        let (server_stream, mut phone) = tokio::io::duplex(8_192);
        ingress
            .accept(
                server_stream,
                SocketAddr::from(([127, 0, 0, 1], 40_081)),
                SocketAddr::from(([127, 0, 0, 1], 2_000)),
                StationTransport::Clear,
            )
            .await
            .unwrap();
        let mut decoder = FrameDecoder::new();
        let protocol = ProtocolVersion::V22;
        let call_id = CallId::new(82);

        phone.write_all(&register_bytes(protocol)).await.unwrap();
        read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                event: DeviceEventKind::Registered(_),
                ..
            }))
        ));
        phone
            .write_all(&capability_update_bytes(
                protocol,
                Codec::Pcmu,
                Codec::H264,
                82,
            ))
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                event: DeviceEventKind::Capabilities { .. },
                ..
            }))
        ));
        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::BeginCall {
                    line_instance: LineInstance::new(1),
                    call_id,
                    codec: Codec::Pcmu,
                },
            ))
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, id::SELECT_SOFT_KEYS).await;
        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::SetCallState {
                    call_id,
                    state: CallState::Connected,
                },
            ))
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, id::SELECT_SOFT_KEYS).await;
        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::StartMultimediaTransmission {
                    call_id,
                    descriptor: video_transmit_descriptor(
                        820,
                        MediaEndpointAddress {
                            address: "192.0.2.82".parse().unwrap(),
                            port: 5090,
                        },
                    ),
                },
            ))
            .await
            .unwrap();
        let start = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
            matches!(message, ServerMessage::StartMultimediaTransmission(_))
        })
        .await
        .into_iter()
        .find_map(|message| match message {
            ServerMessage::StartMultimediaTransmission(start) => Some(start),
            _ => None,
        })
        .unwrap();

        tokio::time::advance(HANDSET_ACKNOWLEDGEMENT_TIMEOUT + Duration::from_millis(100)).await;
        read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
            matches!(
                message,
                ServerMessage::StopMultimediaTransmission(control)
                    if control.passthrough_party_id == start.passthrough_party_id
            )
        })
        .await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                event: DeviceEventKind::MultimediaTransmitTimedOut {
                    call_id: actual_call,
                    codec: Codec::H264,
                    passthrough_party_id,
                },
                ..
            })) if actual_call == call_id
                && passthrough_party_id == start.passthrough_party_id
        ));
        phone
            .write_all(
                &ClientMessage::StartMultimediaTransmissionAck(
                    crate::StartMultimediaTransmissionAck {
                        conference_id: start.conference_id,
                        passthrough_party_id: start.passthrough_party_id,
                        call_reference: start.call_reference,
                        endpoint: MediaEndpointAddress {
                            address: "198.51.100.82".parse().unwrap(),
                            port: 6090,
                        },
                        status: MediaStatus::Ok,
                    },
                )
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(25), events.recv())
                .await
                .is_err()
        );

        handle
            .send_confirmed(Command::new(
                device_id,
                CommandAction::StartMultimediaTransmission {
                    call_id,
                    descriptor: video_transmit_descriptor(
                        821,
                        MediaEndpointAddress {
                            address: "192.0.2.83".parse().unwrap(),
                            port: 5092,
                        },
                    ),
                },
            ))
            .await
            .unwrap();
        let shutdown_start =
            read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
                matches!(
                    message,
                    ServerMessage::StartMultimediaTransmission(start)
                        if start.conference_id == ConferenceId::new(821)
                )
            })
            .await
            .into_iter()
            .find_map(|message| match message {
                ServerMessage::StartMultimediaTransmission(start)
                    if start.conference_id == ConferenceId::new(821) =>
                {
                    Some(start)
                }
                _ => None,
            })
            .unwrap();
        handle.shutdown().await.unwrap();
        read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
            matches!(
                message,
                ServerMessage::StopMultimediaTransmission(control)
                    if control.passthrough_party_id
                        == shutdown_start.passthrough_party_id
            )
        })
        .await;
        task.await.unwrap().unwrap();
    }

    #[test]
    fn multicast_admission_requires_a_routable_supported_audio_shape() {
        let state = multicast_test_state(ProtocolVersion::V22);
        let valid = multicast_route("239.1.2.3".parse().unwrap(), Codec::Pcmu);
        assert!(validate_multicast_route(&state, valid, Some(2)).is_ok());

        for route in [
            MulticastMediaRoute {
                address: "192.0.2.1".parse().unwrap(),
                ..valid
            },
            MulticastMediaRoute { port: 0, ..valid },
            MulticastMediaRoute {
                packet_millis: 0,
                ..valid
            },
        ] {
            assert!(matches!(
                validate_multicast_route(&state, route, None),
                Err(ServerError::InvalidMulticastMedia(_))
            ));
        }
        assert!(matches!(
            validate_multicast_route(
                &state,
                multicast_route("239.1.2.3".parse().unwrap(), Codec::H264),
                None,
            ),
            Err(ServerError::UnsupportedMulticastCodec)
        ));
        assert!(matches!(
            validate_multicast_route(
                &state,
                multicast_route("239.1.2.3".parse().unwrap(), Codec::Pcma),
                None,
            ),
            Err(ServerError::UnsupportedMulticastCodec)
        ));
        for requested_frames in [0, 3] {
            assert!(matches!(
                validate_multicast_route(&state, valid, Some(requested_frames)),
                Err(ServerError::InvalidMulticastMedia(_))
            ));
        }

        let legacy = multicast_test_state(ProtocolVersion::V16);
        assert!(matches!(
            validate_multicast_route(
                &legacy,
                multicast_route("ff15::1".parse().unwrap(), Codec::Pcmu),
                None,
            ),
            Err(ServerError::InvalidMulticastMedia(_))
        ));
        assert!(
            validate_multicast_route(
                &state,
                multicast_route("ff15::1".parse().unwrap(), Codec::Pcmu),
                None,
            )
            .is_ok()
        );
    }

    #[test]
    fn multicast_transactions_correlate_exactly_and_retire_once_in_wire_order() {
        let now = Instant::now();
        let mut state = multicast_test_state(ProtocolVersion::V22);
        let call = insert_call(&mut state, CallId(10), 1, Codec::Pcmu, CallState::Connected);
        let key = MulticastKey {
            conference_id: ConferenceId::new(90),
            call_id: call.call_id,
        };
        let receive_request =
            MediaRequestIdentity::new(1, MediaRequestToken::new(101).expect("nonzero media token"))
                .expect("nonzero generation");
        let transmit_request =
            MediaRequestIdentity::new(2, MediaRequestToken::new(102).expect("nonzero media token"))
                .expect("nonzero generation");
        let route = multicast_route("239.1.2.3".parse().unwrap(), Codec::Pcmu);
        state.multicast.insert(
            key,
            MulticastSession {
                wire_call_reference: call.wire_reference,
                receive: Some(MulticastReceive {
                    request: receive_request,
                    route,
                    state: MulticastReceiveState::AwaitingAcknowledgement { deadline: now },
                }),
                transmit: Some(MulticastTransmit {
                    request: transmit_request,
                    route,
                }),
            },
        );

        assert_eq!(
            find_multicast_receive_key(&state, call.wire_reference, 100),
            None
        );
        assert_eq!(
            find_multicast_receive_key(&state, call.wire_reference + 1, 101),
            None
        );
        assert_eq!(
            find_multicast_receive_key(&state, call.wire_reference, 101),
            Some(key)
        );
        assert_eq!(
            find_multicast_transmit_key(
                &state,
                90,
                call.wire_reference,
                102,
                route.address,
                route.port + 1,
            ),
            None
        );
        assert_eq!(
            find_multicast_transmit_key(
                &state,
                90,
                call.wire_reference,
                102,
                route.address,
                route.port,
            ),
            Some(key)
        );

        let expired = expire_multicast_reception_acknowledgements(&mut state, now);
        assert!(matches!(
            expired.as_slice(),
            [(
                actual_key,
                ServerMessage::StopMulticastMediaReception { passthrough_party_id, .. }
            )] if *actual_key == key && passthrough_party_id.get() == 101
        ));
        assert!(expire_multicast_reception_acknowledgements(&mut state, now).is_empty());

        let other_call = insert_call(&mut state, CallId(20), 1, Codec::Pcmu, CallState::Connected);
        let other_key = MulticastKey {
            conference_id: ConferenceId::new(91),
            call_id: other_call.call_id,
        };
        state.multicast.insert(
            other_key,
            MulticastSession {
                wire_call_reference: other_call.wire_reference,
                receive: Some(MulticastReceive {
                    request: MediaRequestIdentity::new(
                        3,
                        MediaRequestToken::new(103).expect("nonzero media token"),
                    )
                    .expect("nonzero generation"),
                    route,
                    state: MulticastReceiveState::Open,
                }),
                transmit: None,
            },
        );

        let remaining = take_multicast_stops_for_call(&mut state, call.call_id);
        assert!(matches!(
            remaining.as_slice(),
            [ServerMessage::StopMulticastMediaTransmission { passthrough_party_id, .. }]
                if passthrough_party_id.get() == 102
        ));
        assert!(take_multicast_stops_for_call(&mut state, call.call_id).is_empty());
        assert!(state.multicast.contains_key(&other_key));
        let shutdown_stops = take_all_multicast_stops(&mut state);
        assert!(matches!(
            shutdown_stops.as_slice(),
            [ServerMessage::StopMulticastMediaReception { passthrough_party_id, .. }]
                if passthrough_party_id.get() == 103
        ));
        assert!(take_all_multicast_stops(&mut state).is_empty());
        assert!(state.multicast.is_empty());
    }

    #[tokio::test]
    async fn multicast_session_enforces_transaction_identity_order_and_teardown() {
        let device = definition();
        let device_id = device.id.clone();
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let (server, handle, mut events) = Server::bind(config, [device]).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let mut phone = TcpStream::connect(address).await.unwrap();
        let mut decoder = FrameDecoder::new();
        let protocol = ProtocolVersion::V22;
        let call_id = CallId(41);
        let conference_id = ConferenceId::new(900);
        let first_route = multicast_route("239.1.2.3".parse().unwrap(), Codec::Pcmu);
        let second_route = MulticastMediaRoute {
            address: "239.1.2.4".parse().unwrap(),
            port: 5006,
            ..first_route
        };

        phone.write_all(&register_bytes(protocol)).await.unwrap();
        read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                event: DeviceEventKind::Registered(_),
                ..
            }))
        ));
        phone
            .write_all(
                &ClientMessage::CapabilitiesResponse(vec![MediaCapability {
                    codec: Codec::Pcmu,
                    max_frames_per_packet: 2,
                    codec_parameters: [0; 8],
                }])
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                event: DeviceEventKind::Capabilities { .. },
                ..
            }))
        ));
        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::BeginCall {
                    line_instance: LineInstance::new(1),
                    call_id,
                    codec: Codec::Pcmu,
                },
            ))
            .await
            .unwrap();
        let messages = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
            matches!(message, ServerMessage::SelectSoftKeys { .. })
        })
        .await;
        let wire_call_reference = messages
            .iter()
            .find_map(|message| match message {
                ServerMessage::CallState {
                    state: CallState::OffHook,
                    call_reference,
                    ..
                } => Some(*call_reference),
                _ => None,
            })
            .expect("begin call omitted its wire reference");

        for invalid_route in [
            MulticastMediaRoute {
                address: "192.0.2.1".parse().unwrap(),
                ..first_route
            },
            MulticastMediaRoute {
                codec: Codec::Pcma,
                ..first_route
            },
        ] {
            assert!(matches!(
                handle
                    .send_confirmed(Command::new(
                        device_id.clone(),
                        CommandAction::StartMulticastReception {
                            conference_id,
                            call_id,
                            route: invalid_route,
                            echo_cancellation: EchoCancellation::On,
                            g723_bitrate: G723BitRate::Rate5_3,
                        },
                    ))
                    .await,
                Err(ServerError::CommandWrite(_))
            ));
        }
        phone
            .write_all(&ClientMessage::KeepAlive.encode(protocol).unwrap())
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, id::KEEP_ALIVE_ACK).await;

        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::StartMulticastReception {
                    conference_id,
                    call_id,
                    route: first_route,
                    echo_cancellation: EchoCancellation::On,
                    g723_bitrate: G723BitRate::Rate5_3,
                },
            ))
            .await
            .unwrap();
        let messages = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
            matches!(message, ServerMessage::StartMulticastMediaReception(_))
        })
        .await;
        let first_receive_token = messages
            .iter()
            .find_map(|message| match message {
                ServerMessage::StartMulticastMediaReception(request) => {
                    assert_eq!(request.conference_id, conference_id);
                    assert_eq!(request.call_reference.get(), wire_call_reference);
                    assert_eq!(request.address, first_route.address);
                    assert_eq!(request.port, first_route.port);
                    assert_eq!(request.codec, first_route.codec);
                    Some(request.passthrough_party_id)
                }
                _ => None,
            })
            .expect("multicast reception omitted its request");

        let mismatched = ClientMessage::MulticastMediaReceptionAck {
            status: MediaStatus::Ok,
            passthrough_party_id: first_receive_token,
            call_reference: CallReference::new(wire_call_reference + 1),
        }
        .encode(protocol)
        .unwrap();
        let exact = ClientMessage::MulticastMediaReceptionAck {
            status: MediaStatus::Ok,
            passthrough_party_id: first_receive_token,
            call_reference: CallReference::new(wire_call_reference),
        }
        .encode(protocol)
        .unwrap();
        let mut coalesced_prefix = mismatched;
        coalesced_prefix.push(exact[0]);
        phone.write_all(&coalesced_prefix).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(25), events.recv())
                .await
                .is_err(),
            "a mismatched acknowledgement completed the transaction"
        );
        for fragment in exact[1..].chunks(2) {
            phone.write_all(fragment).await.unwrap();
        }
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent { session_generation: _,
                event: DeviceEventKind::MulticastReceptionStarted {
                    conference_id: actual_conference,
                    call_id: actual_call,
                    route,
                },
                ..
            })) if actual_conference == conference_id && actual_call == call_id && route == first_route
        ));
        phone.write_all(&exact).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(25), events.recv())
                .await
                .is_err(),
            "a duplicate acknowledgement emitted another event"
        );

        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::StartMulticastReception {
                    conference_id,
                    call_id,
                    route: second_route,
                    echo_cancellation: EchoCancellation::Off,
                    g723_bitrate: G723BitRate::Rate6_3,
                },
            ))
            .await
            .unwrap();
        let messages = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
            matches!(
                message,
                ServerMessage::StartMulticastMediaReception(request)
                    if request.address == second_route.address
            )
        })
        .await;
        let stop_index = messages
            .iter()
            .position(|message| {
                matches!(
                    message,
                    ServerMessage::StopMulticastMediaReception { passthrough_party_id, .. }
                        if *passthrough_party_id == first_receive_token
                )
            })
            .expect("replacement did not stop the previous generation");
        let (start_index, second_receive_token) = messages
            .iter()
            .enumerate()
            .find_map(|(index, message)| match message {
                ServerMessage::StartMulticastMediaReception(request)
                    if request.address == second_route.address =>
                {
                    Some((index, request.passthrough_party_id))
                }
                _ => None,
            })
            .expect("replacement did not start a fresh generation");
        assert!(stop_index < start_index);
        assert_ne!(first_receive_token, second_receive_token);

        let negative = ClientMessage::MulticastMediaReceptionAck {
            status: MediaStatus::OutOfChannels,
            passthrough_party_id: second_receive_token,
            call_reference: CallReference::new(wire_call_reference),
        }
        .encode(protocol)
        .unwrap();
        phone.write_all(&negative).await.unwrap();
        read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
            matches!(
                message,
                ServerMessage::StopMulticastMediaReception { passthrough_party_id, .. }
                    if *passthrough_party_id == second_receive_token
            )
        })
        .await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent { session_generation: _,
                event: DeviceEventKind::MulticastReceptionFailed {
                    conference_id: actual_conference,
                    call_id: actual_call,
                    status: MediaStatus::OutOfChannels,
                },
                ..
            })) if actual_conference == conference_id && actual_call == call_id
        ));
        phone.write_all(&negative).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(25), events.recv())
                .await
                .is_err(),
            "a duplicate failure acknowledgement emitted another event"
        );

        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::StartMulticastTransmission {
                    conference_id,
                    call_id,
                    route: first_route,
                    precedence: 0,
                    silence_suppression: SilenceSuppression::Off,
                    max_frames_per_packet: 2,
                    g723_bitrate: G723BitRate::Rate5_3,
                },
            ))
            .await
            .unwrap();
        let messages = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
            matches!(message, ServerMessage::StartMulticastMediaTransmission(_))
        })
        .await;
        let transmit_token = messages
            .iter()
            .find_map(|message| match message {
                ServerMessage::StartMulticastMediaTransmission(request) => {
                    Some(request.passthrough_party_id.get())
                }
                _ => None,
            })
            .expect("multicast transmission omitted its request");
        let started_event = events.recv().await;
        assert!(matches!(
            started_event,
            Some(Event::Device(DeviceEvent { session_generation: _,
                event: DeviceEventKind::MulticastTransmissionStarted {
                    conference_id: actual_conference,
                    call_id: actual_call,
                    route,
                },
                ..
            })) if actual_conference == conference_id && actual_call == call_id && route == first_route
        ));
        let mismatch_failure = ClientMessage::MediaTransmissionFailure {
            conference_id: conference_id.get(),
            passthrough_party_id: transmit_token,
            address: first_route.address,
            port: first_route.port + 1,
            call_reference: wire_call_reference,
            status: MediaStatus::UnspecifiedError,
        }
        .encode(protocol)
        .unwrap();
        phone.write_all(&mismatch_failure).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(25), events.recv())
                .await
                .is_err(),
            "a mismatched transmission failure retired the transaction"
        );
        let exact_failure = ClientMessage::MediaTransmissionFailure {
            conference_id: conference_id.get(),
            passthrough_party_id: transmit_token,
            address: first_route.address,
            port: first_route.port,
            call_reference: wire_call_reference,
            status: MediaStatus::UnspecifiedError,
        }
        .encode(protocol)
        .unwrap();
        phone.write_all(&exact_failure).await.unwrap();
        read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
            matches!(
                message,
                ServerMessage::StopMulticastMediaTransmission { passthrough_party_id, .. }
                    if passthrough_party_id.get() == transmit_token
            )
        })
        .await;
        let failure_event = events.recv().await;
        assert!(
            matches!(
                failure_event,
                Some(Event::Device(DeviceEvent { session_generation: _,
                    event: DeviceEventKind::MulticastTransmissionFailed {
                        conference_id: actual_conference,
                        call_id: actual_call,
                        status: MediaStatus::UnspecifiedError,
                        ..
                    },
                    ..
                })) if actual_conference == conference_id && actual_call == call_id
            ),
            "unexpected multicast transmission failure event: {failure_event:?}"
        );
        phone.write_all(&exact_failure).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(25), events.recv())
                .await
                .is_err(),
            "a duplicate transmission failure emitted another event"
        );

        phone
            .write_all(&ClientMessage::KeepAlive.encode(protocol).unwrap())
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, id::KEEP_ALIVE_ACK).await;

        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::StartMulticastReception {
                    conference_id,
                    call_id,
                    route: first_route,
                    echo_cancellation: EchoCancellation::On,
                    g723_bitrate: G723BitRate::Rate5_3,
                },
            ))
            .await
            .unwrap();
        read_until_message(
            &mut phone,
            &mut decoder,
            id::START_MULTICAST_MEDIA_RECEPTION,
        )
        .await;
        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::StartMulticastTransmission {
                    conference_id,
                    call_id,
                    route: first_route,
                    precedence: 0,
                    silence_suppression: SilenceSuppression::Off,
                    max_frames_per_packet: 2,
                    g723_bitrate: G723BitRate::Rate5_3,
                },
            ))
            .await
            .unwrap();
        read_until_message(
            &mut phone,
            &mut decoder,
            id::START_MULTICAST_MEDIA_TRANSMISSION,
        )
        .await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                event: DeviceEventKind::MulticastTransmissionStarted { .. },
                ..
            }))
        ));
        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::CloseCall { call_id },
            ))
            .await
            .unwrap();
        let messages = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
            matches!(
                message,
                ServerMessage::CallState {
                    state: CallState::OnHook,
                    ..
                }
            )
        })
        .await;
        let receive_stop = messages
            .iter()
            .position(|message| {
                matches!(message, ServerMessage::StopMulticastMediaReception { .. })
            })
            .expect("call close omitted multicast reception stop");
        let transmit_stop = messages
            .iter()
            .position(|message| {
                matches!(
                    message,
                    ServerMessage::StopMulticastMediaTransmission { .. }
                )
            })
            .expect("call close omitted multicast transmission stop");
        let on_hook = messages
            .iter()
            .position(|message| {
                matches!(
                    message,
                    ServerMessage::CallState {
                        state: CallState::OnHook,
                        ..
                    }
                )
            })
            .unwrap();
        assert!(receive_stop < transmit_stop && transmit_stop < on_hook);

        let disconnect_call = CallId(42);
        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::BeginCall {
                    line_instance: LineInstance::new(1),
                    call_id: disconnect_call,
                    codec: Codec::Pcmu,
                },
            ))
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, id::SELECT_SOFT_KEYS).await;
        for action in [
            CommandAction::StartMulticastReception {
                conference_id,
                call_id: disconnect_call,
                route: first_route,
                echo_cancellation: EchoCancellation::On,
                g723_bitrate: G723BitRate::Rate5_3,
            },
            CommandAction::StartMulticastTransmission {
                conference_id,
                call_id: disconnect_call,
                route: first_route,
                precedence: 0,
                silence_suppression: SilenceSuppression::Off,
                max_frames_per_packet: 2,
                g723_bitrate: G723BitRate::Rate5_3,
            },
        ] {
            handle
                .send_confirmed(Command::new(device_id.clone(), action))
                .await
                .unwrap();
        }
        read_until_message(
            &mut phone,
            &mut decoder,
            id::START_MULTICAST_MEDIA_TRANSMISSION,
        )
        .await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent { session_generation: _,
                event: DeviceEventKind::MulticastTransmissionStarted {
                    call_id: actual_call,
                    ..
                },
                ..
            })) if actual_call == disconnect_call
        ));

        let mut replacement = definition();
        replacement.description = "replacement".into();
        handle.reconfigure([replacement]).await.unwrap();
        let messages = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
            matches!(
                message,
                ServerMessage::StopMulticastMediaTransmission { .. }
            )
        })
        .await;
        let receive_stops = messages
            .iter()
            .enumerate()
            .filter_map(|(index, message)| {
                matches!(message, ServerMessage::StopMulticastMediaReception { .. })
                    .then_some(index)
            })
            .collect::<Vec<_>>();
        let transmit_stops = messages
            .iter()
            .enumerate()
            .filter_map(|(index, message)| {
                matches!(
                    message,
                    ServerMessage::StopMulticastMediaTransmission { .. }
                )
                .then_some(index)
            })
            .collect::<Vec<_>>();
        assert_eq!(receive_stops.len(), 1);
        assert_eq!(transmit_stops.len(), 1);
        assert!(receive_stops[0] < transmit_stops[0]);
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                event: DeviceEventKind::Disconnected {},
                ..
            }))
        ));

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn multicast_receive_deadline_stops_and_retires_the_pending_generation() {
        let device = definition();
        let device_id = device.id.clone();
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let (server, handle, mut events, ingress) = Server::with_ingress(config, [device]).unwrap();
        let task = tokio::spawn(server.run());
        let (server_stream, mut phone) = tokio::io::duplex(8_192);
        ingress
            .accept(
                server_stream,
                SocketAddr::from(([127, 0, 0, 1], 40_000)),
                SocketAddr::from(([127, 0, 0, 1], 2_000)),
                StationTransport::Clear,
            )
            .await
            .unwrap();
        let mut decoder = FrameDecoder::new();
        let protocol = ProtocolVersion::V22;
        let call_id = CallId(43);
        let conference_id = ConferenceId::new(901);
        let route = multicast_route("239.1.2.5".parse().unwrap(), Codec::Pcmu);

        phone.write_all(&register_bytes(protocol)).await.unwrap();
        read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                event: DeviceEventKind::Registered(_),
                ..
            }))
        ));
        phone
            .write_all(
                &ClientMessage::CapabilitiesResponse(vec![MediaCapability {
                    codec: Codec::Pcmu,
                    max_frames_per_packet: 1,
                    codec_parameters: [0; 8],
                }])
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                event: DeviceEventKind::Capabilities { .. },
                ..
            }))
        ));
        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::BeginCall {
                    line_instance: LineInstance::new(1),
                    call_id,
                    codec: Codec::Pcmu,
                },
            ))
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, id::SELECT_SOFT_KEYS).await;
        handle
            .send_confirmed(Command::new(
                device_id,
                CommandAction::StartMulticastReception {
                    conference_id,
                    call_id,
                    route,
                    echo_cancellation: EchoCancellation::On,
                    g723_bitrate: G723BitRate::Rate5_3,
                },
            ))
            .await
            .unwrap();
        let messages = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
            matches!(message, ServerMessage::StartMulticastMediaReception(_))
        })
        .await;
        let request = messages
            .iter()
            .find_map(|message| match message {
                ServerMessage::StartMulticastMediaReception(request) => Some(request.clone()),
                _ => None,
            })
            .unwrap();

        tokio::time::advance(HANDSET_ACKNOWLEDGEMENT_TIMEOUT + Duration::from_millis(100)).await;
        read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
            matches!(
                message,
                ServerMessage::StopMulticastMediaReception { passthrough_party_id, .. }
                    if *passthrough_party_id == request.passthrough_party_id
            )
        })
        .await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent { session_generation: _,
                event: DeviceEventKind::MulticastReceptionTimedOut {
                    conference_id: actual_conference,
                    call_id: actual_call,
                },
                ..
            })) if actual_conference == conference_id && actual_call == call_id
        ));
        phone
            .write_all(
                &ClientMessage::MulticastMediaReceptionAck {
                    status: MediaStatus::Ok,
                    passthrough_party_id: request.passthrough_party_id,
                    call_reference: request.call_reference,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(25), events.recv())
                .await
                .is_err(),
            "a late acknowledgement resurrected the expired generation"
        );
        phone
            .write_all(&ClientMessage::KeepAlive.encode(protocol).unwrap())
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, id::KEEP_ALIVE_ACK).await;

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    fn mixed_definition() -> DeviceDefinition {
        let mut device = definition();
        device.buttons.extend([
            ButtonDefinition::SpeedDial(SpeedDialDefinition {
                instance: 1,
                number: "2001".into(),
                display_name: "Reception".into(),
            }),
            ButtonDefinition::Feature(FeatureDefinition {
                instance: 1,
                label: "DND".into(),
                feature: ButtonType::DoNotDisturb,
            }),
            ButtonDefinition::Service(ServiceDefinition {
                instance: 1,
                label: "Directory".into(),
                url: "http://services.invalid/directory".into(),
            }),
            ButtonDefinition::Unused,
            ButtonDefinition::BlfSpeedDial(BlfSpeedDialDefinition {
                instance: 2,
                number: "2002".into(),
                display_name: "Warehouse".into(),
                hint: "2002@internal".into(),
            }),
        ]);
        device
    }

    fn profile_with(mode: KeyMode, actions: Vec<SoftKey>) -> SoftKeyProfile {
        let default = SoftKeyProfile::default();
        SoftKeyProfile::new(KeyMode::ALL_KNOWN.iter().copied().map(|candidate| {
            if candidate == mode {
                (candidate, actions.clone())
            } else {
                (candidate, default.actions(candidate).to_vec())
            }
        }))
        .unwrap()
    }

    fn session_call(call_id: u64) -> SessionCall {
        SessionCall {
            call_id: CallId(call_id),
            wire_reference: call_id as u32,
            line_instance: 1,
            media: CallMedia::new(Codec::Pcmu),
            video_receive: VideoReceive::default(),
            video_transmit: VideoTransmit::default(),
            state: CallState::Connected,
            history_disposition: CallHistoryDisposition::Placed,
            dialed_number: String::new(),
            statistics_directory_number: String::new(),
            transfer_role: None,
        }
    }

    #[test]
    fn every_call_state_and_soft_key_has_an_explicit_availability_result() {
        let expected_modes = [
            (CallState::OffHook, KeyMode::OffHook),
            (CallState::OnHook, KeyMode::OnHook),
            (CallState::RingOut, KeyMode::RingOut),
            (CallState::RingIn, KeyMode::RingIn),
            (CallState::Connected, KeyMode::Connected),
            (CallState::Busy, KeyMode::OffHook),
            (CallState::Congestion, KeyMode::OffHook),
            (CallState::Hold, KeyMode::OnHold),
            (CallState::CallWaiting, KeyMode::RingIn),
            (CallState::Transfer, KeyMode::ConnectedTransfer),
            (CallState::Park, KeyMode::OnHook),
            (CallState::Proceed, KeyMode::RingOut),
            (CallState::RemoteMultiline, KeyMode::OnHookStealable),
            (CallState::InvalidNumber, KeyMode::OffHook),
            (CallState::HoldYellow, KeyMode::OnHold),
            (CallState::IntercomOneWay, KeyMode::OffHook),
            (CallState::HoldRed, KeyMode::OnHold),
        ];
        assert_eq!(CallState::ALL_KNOWN.len(), expected_modes.len());
        assert_eq!(
            CallState::ALL_KNOWN,
            expected_modes
                .iter()
                .map(|(state, _)| *state)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            key_mode_for_call_state(CallState::Unknown(99)),
            KeyMode::OnHook
        );

        let profile = SoftKeyProfile::built_in();
        for (state, expected_mode) in expected_modes {
            let mode = key_mode_for_call_state(state);
            assert_eq!(mode, expected_mode, "unexpected key mode for {state:?}");
            let expected_actions: &[SoftKey] = match mode {
                KeyMode::OnHook => &[SoftKey::NewCall],
                KeyMode::Connected | KeyMode::ConnectedTransfer => {
                    &[SoftKey::Hold, SoftKey::EndCall, SoftKey::Transfer]
                }
                KeyMode::OnHold | KeyMode::OffHookFeature | KeyMode::HoldConference => {
                    &[SoftKey::Resume, SoftKey::NewCall, SoftKey::EndCall]
                }
                KeyMode::RingIn => &[SoftKey::Answer, SoftKey::EndCall],
                KeyMode::OffHook | KeyMode::RingOut => &[SoftKey::EndCall],
                KeyMode::DigitsFollowing => &[SoftKey::Backspace, SoftKey::EndCall, SoftKey::Dial],
                KeyMode::ConnectedConference => &[SoftKey::Hold, SoftKey::EndCall],
                KeyMode::OnHookStealable => &[SoftKey::Intercept, SoftKey::NewCall],
                KeyMode::InUseHint | KeyMode::Empty | KeyMode::Unknown(_) => &[],
            };
            for &soft_key in SoftKey::ALL_KNOWN {
                assert_eq!(
                    profile.allows(mode, soft_key),
                    expected_actions.contains(&soft_key),
                    "state={state:?} mode={mode:?} soft_key={soft_key:?}"
                );
            }
        }
    }

    #[test]
    fn blf_updates_map_every_state_to_icon_and_lamp() {
        let cases = [
            (BlfState::Idle, BusyLampFieldState::Idle, LampMode::Off),
            (
                BlfState::Ringing,
                BusyLampFieldState::Alerting,
                LampMode::Blink,
            ),
            (BlfState::Busy, BusyLampFieldState::InUse, LampMode::On),
            (BlfState::Held, BusyLampFieldState::InUse, LampMode::Hold),
            (
                BlfState::Unavailable,
                BusyLampFieldState::UnknownState,
                LampMode::Flash,
            ),
            (
                BlfState::Unknown,
                BusyLampFieldState::UnknownState,
                LampMode::Wink,
            ),
        ];

        for (state, expected_icon, expected_lamp) in cases {
            let [speed_dial, feature, lamp] =
                blf_status_messages(7, "4100", "Support", state, None);
            assert!(matches!(
                speed_dial,
                ServerMessage::SpeedDialStatus {
                    instance: 7,
                    ref number,
                    ref display_name,
                } if number == "4100" && display_name == "Support"
            ));
            assert!(matches!(
                feature,
                ServerMessage::FeatureStatus {
                    instance: 7,
                    button_type: ButtonType::BlfSpeedDial,
                    ref label,
                    state,
                } if label == "Support" && state == expected_icon.wire_value()
            ));
            assert!(matches!(
                lamp,
                ServerMessage::SetLamp {
                    stimulus: ButtonType::BlfSpeedDial,
                    instance: 7,
                    mode,
                } if mode == expected_lamp
            ));
        }
    }

    #[test]
    fn hinted_ringing_policy_adds_only_a_ringing_notification() {
        let caller = BlfCallerInfo {
            name: "Taylor".into(),
            number: "5550100".into(),
        };
        let mut disabled = definition();
        assert_eq!(
            hinted_ringing_notification(&disabled, "Dispatch", Some(&caller), BlfState::Ringing,),
            None
        );

        disabled.ui.hinted_ringing_notification = true;
        assert_eq!(
            hinted_ringing_notification(&disabled, "Dispatch", Some(&caller), BlfState::Ringing,),
            Some(HandsetStatusMessage::Display {
                text: "Dispatch is ringing: Taylor (5550100)".into(),
                timeout_seconds: 5,
                priority: None,
            })
        );
        for state in [
            BlfState::Idle,
            BlfState::Busy,
            BlfState::Held,
            BlfState::Unavailable,
            BlfState::Unknown,
        ] {
            assert_eq!(
                hinted_ringing_notification(&disabled, "Dispatch", Some(&caller), state),
                None,
                "non-ringing BLF state {state:?} must not be replaced by a notification"
            );
            assert_eq!(
                blf_status_messages(7, "4100", "Dispatch", state, Some(&caller)).len(),
                3,
                "ordinary BLF projection must remain intact"
            );
        }
    }

    #[test]
    fn last_number_uses_the_configured_terminator_recording_policy() {
        let without_terminator = ServerConfig {
            dial_terminator: Digit::Star,
            record_dial_terminator: false,
            ..ServerConfig::default()
        };
        let with_terminator = ServerConfig {
            record_dial_terminator: true,
            ..without_terminator.clone()
        };

        assert_eq!(
            normalized_last_number(" 5551212* ", &without_terminator),
            Some("5551212".into())
        );
        assert_eq!(
            normalized_last_number(" 5551212* ", &with_terminator),
            Some("5551212*".into())
        );
        assert_eq!(
            normalized_last_number("5551212#", &without_terminator),
            Some("5551212#".into()),
            "non-terminator DTMF must remain part of the remembered number"
        );
        assert_eq!(normalized_last_number("***", &without_terminator), None);
    }

    #[tokio::test]
    async fn invalid_server_dial_terminator_is_rejected_before_binding() {
        let result = Server::bind(
            ServerConfig {
                dial_terminator: Digit::Unknown(99),
                ..ServerConfig::default()
            },
            [definition()],
        )
        .await;
        assert!(matches!(result, Err(ServerError::InvalidConfig(_))));
    }

    #[test]
    fn invalid_server_signaling_qos_is_rejected_for_external_ingress() {
        let result = Server::with_ingress(
            ServerConfig {
                signaling_qos: SignalingQos::new(64, 0),
                ..ServerConfig::default()
            },
            [definition()],
        );

        assert!(
            matches!(result, Err(ServerError::InvalidConfig(message)) if message.contains("DSCP 64"))
        );
    }

    #[test]
    fn invalid_failover_policy_is_rejected_before_ingress_starts() {
        let route = |priority| SignalingServerRoute {
            priority,
            name: format!("node-{priority}"),
            address: IpAddr::V4(Ipv4Addr::new(192, 0, 2, priority)),
            clear_port: NonZeroU16::new(2000),
            secure_port: None,
        };
        let invalid = [
            ServerConfig {
                advertised_address: Ipv4Addr::UNSPECIFIED,
                ..ServerConfig::default()
            },
            ServerConfig {
                secondary_keepalive_seconds: 4,
                ..ServerConfig::default()
            },
            ServerConfig {
                registration_tokens: RegistrationTokenPolicy {
                    backoff: Duration::from_secs(29),
                    ..RegistrationTokenPolicy::default()
                },
                ..ServerConfig::default()
            },
            ServerConfig {
                signaling_servers: vec![route(1), route(1)],
                ..ServerConfig::default()
            },
            ServerConfig {
                signaling_servers: vec![route(2)],
                ..ServerConfig::default()
            },
            ServerConfig {
                signaling_servers: (1..=6).map(route).collect(),
                ..ServerConfig::default()
            },
        ];

        for config in invalid {
            assert!(matches!(
                Server::with_ingress(config, [definition()]),
                Err(ServerError::InvalidConfig(_))
            ));
        }
    }

    #[test]
    fn blf_update_only_displays_explicitly_permitted_caller_information() {
        let [_, without_caller, _] =
            blf_status_messages(2, "4200", "Dispatch", BlfState::Ringing, None);
        assert!(matches!(
            without_caller,
            ServerMessage::FeatureStatus { label, .. } if label == "Dispatch"
        ));

        let caller = BlfCallerInfo {
            name: "Taylor".into(),
            number: "5550100".into(),
        };
        let [_, with_caller, _] =
            blf_status_messages(2, "4200", "Dispatch", BlfState::Ringing, Some(&caller));
        assert!(matches!(
            with_caller,
            ServerMessage::FeatureStatus { label, .. }
                if label == "Dispatch: Taylor (5550100)"
        ));
    }

    #[test]
    fn button_template_uses_ordered_semantic_device_buttons() {
        let device = mixed_definition();

        assert_eq!(
            button_template(&device),
            vec![
                ButtonTemplateEntry {
                    instance: 1,
                    button_type: ButtonType::Line,
                },
                ButtonTemplateEntry {
                    instance: 1,
                    button_type: ButtonType::SpeedDial,
                },
                ButtonTemplateEntry {
                    instance: 1,
                    button_type: ButtonType::DoNotDisturb,
                },
                ButtonTemplateEntry {
                    instance: 1,
                    button_type: ButtonType::ServiceUrl,
                },
                ButtonTemplateEntry {
                    instance: 0,
                    button_type: ButtonType::Unused,
                },
                ButtonTemplateEntry {
                    instance: 2,
                    button_type: ButtonType::BlfSpeedDial,
                },
            ]
        );
    }

    #[test]
    fn expansion_module_reserves_exact_model_capacity_and_places_configured_keys() {
        let mut device = definition();
        device.buttons.extend([
            ButtonDefinition::AddonModule(crate::types::AddonModuleDefinition {
                slot: 1,
                device_type: DeviceType::CiscoAddon7914,
            }),
            ButtonDefinition::SpeedDial(SpeedDialDefinition {
                instance: 1,
                number: "2001".into(),
                display_name: "Reception".into(),
            }),
            ButtonDefinition::Feature(FeatureDefinition {
                instance: 1,
                label: "DND".into(),
                feature: ButtonType::DoNotDisturb,
            }),
        ]);
        device.validate().unwrap();

        let layout = button_template(&device);
        assert_eq!(layout.len(), 15, "one base key plus fourteen sidecar keys");
        assert_eq!(
            &layout[..3],
            [
                ButtonTemplateEntry {
                    instance: 1,
                    button_type: ButtonType::Line,
                },
                ButtonTemplateEntry {
                    instance: 1,
                    button_type: ButtonType::SpeedDial,
                },
                ButtonTemplateEntry {
                    instance: 1,
                    button_type: ButtonType::DoNotDisturb,
                },
            ]
        );
        assert!(
            layout[3..]
                .iter()
                .all(|button| { button.instance == 0 && button.button_type == ButtonType::Unused })
        );

        let mut over_capacity = definition();
        over_capacity.buttons.push(ButtonDefinition::AddonModule(
            crate::types::AddonModuleDefinition {
                slot: 1,
                device_type: DeviceType::AddonSpa500s,
            },
        ));
        over_capacity
            .buttons
            .extend(std::iter::repeat_n(ButtonDefinition::Unused, 33));
        assert!(matches!(
            over_capacity.validate(),
            Err(CodecError::InvalidDefinition(message))
                if message.contains("more buttons than its addon module provides")
        ));
    }

    #[test]
    fn static_button_statuses_use_typed_instances_and_safe_unknowns() {
        let device = mixed_definition();

        assert_eq!(
            speed_dial_status(&device, 1),
            ServerMessage::SpeedDialStatus {
                instance: 1,
                number: "2001".into(),
                display_name: "Reception".into(),
            }
        );
        assert_eq!(
            speed_dial_status(&device, 99),
            ServerMessage::SpeedDialStatus {
                instance: 99,
                number: String::new(),
                display_name: String::new(),
            }
        );
        assert_eq!(
            feature_status(&device, 1, 0),
            Some(ServerMessage::FeatureStatus {
                instance: 1,
                button_type: ButtonType::DoNotDisturb,
                label: "DND".into(),
                state: 0,
            })
        );
        assert_eq!(feature_status(&device, 99, 0), None);
        assert_eq!(
            speed_dial_status(&device, 2),
            ServerMessage::SpeedDialStatus {
                instance: 2,
                number: "2002".into(),
                display_name: "Warehouse".into(),
            }
        );
        assert_eq!(
            feature_status(&device, 2, 1),
            Some(ServerMessage::FeatureStatus {
                instance: 2,
                button_type: ButtonType::BlfSpeedDial,
                label: "Warehouse".into(),
                state: BusyLampFieldState::UnknownState.wire_value(),
            })
        );
        assert_eq!(
            service_url_status(&device, 1),
            Some(ServerMessage::ServiceUrlStatus {
                index: 1,
                url: "http://services.invalid/directory".into(),
                label: "Directory".into(),
                extension_text: String::new(),
            })
        );
        assert_eq!(service_url_status(&device, 99), None);
    }

    #[test]
    fn do_not_disturb_status_preserves_exact_mode_and_button_behavior() {
        let device = mixed_definition();

        for (mode, state, lamp) in [
            (DoNotDisturbMode::Off, 0x010000, LampMode::Off),
            (DoNotDisturbMode::Reject, 0x020202, LampMode::On),
            (DoNotDisturbMode::Silent, 0x030302, LampMode::Blink),
        ] {
            assert_eq!(
                do_not_disturb_state_messages(
                    &device,
                    1,
                    mode,
                    DoNotDisturbButtonMode::Cycle,
                    ProtocolVersion::V22,
                ),
                Some([
                    ServerMessage::FeatureStatus {
                        instance: 1,
                        button_type: ButtonType::MultiblinkFeature,
                        label: "DND".into(),
                        state,
                    },
                    ServerMessage::SetLamp {
                        stimulus: ButtonType::DoNotDisturb,
                        instance: 1,
                        mode: lamp,
                    },
                ])
            );
        }

        for (button_mode, mode, enabled, lamp) in [
            (
                DoNotDisturbButtonMode::Silent,
                DoNotDisturbMode::Silent,
                1,
                LampMode::Blink,
            ),
            (
                DoNotDisturbButtonMode::Silent,
                DoNotDisturbMode::Reject,
                0,
                LampMode::Off,
            ),
            (
                DoNotDisturbButtonMode::Reject,
                DoNotDisturbMode::Reject,
                1,
                LampMode::On,
            ),
            (
                DoNotDisturbButtonMode::Reject,
                DoNotDisturbMode::Silent,
                0,
                LampMode::Off,
            ),
        ] {
            let [feature, lamp_message] =
                do_not_disturb_state_messages(&device, 1, mode, button_mode, ProtocolVersion::V22)
                    .unwrap();
            assert!(matches!(
                feature,
                ServerMessage::FeatureStatus {
                    button_type: ButtonType::DoNotDisturb,
                    state,
                    ..
                } if state == enabled
            ));
            assert!(matches!(
                lamp_message,
                ServerMessage::SetLamp { mode, .. } if mode == lamp
            ));
        }

        let [legacy_feature, legacy_lamp] = do_not_disturb_state_messages(
            &device,
            1,
            DoNotDisturbMode::Silent,
            DoNotDisturbButtonMode::Cycle,
            ProtocolVersion::V15,
        )
        .unwrap();
        assert!(matches!(
            legacy_feature,
            ServerMessage::FeatureStatus {
                button_type: ButtonType::DoNotDisturb,
                state: 1,
                ..
            }
        ));
        assert!(matches!(
            legacy_lamp,
            ServerMessage::SetLamp {
                mode: LampMode::Blink,
                ..
            }
        ));

        assert!(
            do_not_disturb_state_messages(
                &device,
                99,
                DoNotDisturbMode::Reject,
                DoNotDisturbButtonMode::Cycle,
                ProtocolVersion::V22,
            )
            .is_none()
        );
    }

    #[test]
    fn shared_line_appearances_keep_distinct_instances_and_labels() {
        let logical_line = LineDefinition {
            number: "4100".into(),
            display_name: "Operations".into(),
        };
        let mut primary = LineAppearance::new(1, logical_line.clone());
        primary.label = Some("Operations primary".into());
        let mut shared = LineAppearance::new(2, logical_line);
        shared.label = Some("Operations shared".into());
        let device = DeviceDefinition {
            id: DeviceId::new("SEP00AABBCCDDEE").unwrap(),
            description: "Shared line phone".into(),
            transport: StationTransportRequirement::Either,
            signaling_qos: None,
            buttons: vec![
                ButtonDefinition::Line(primary),
                ButtonDefinition::Line(shared),
            ],
            soft_keys: SoftKeyProfile::default(),
            ui: Default::default(),
        };

        device.validate().unwrap();
        assert_eq!(
            line_status(&device, 1),
            Some(ServerMessage::LineStatus {
                instance: 1,
                number: "4100".into(),
                display_name: "Operations primary".into(),
            })
        );
        assert_eq!(
            line_status(&device, 2),
            Some(ServerMessage::LineStatus {
                instance: 2,
                number: "4100".into(),
                display_name: "Operations shared".into(),
            })
        );
        assert_eq!(line_status(&device, 3), None);
    }

    #[tokio::test]
    async fn registered_phone_receives_its_mixed_button_template() {
        let device = mixed_definition();
        let expected = button_template(&device);
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let (server, handle, _events) = Server::bind(config, [device]).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let mut phone = TcpStream::connect(address).await.unwrap();
        let mut decoder = FrameDecoder::new();

        phone
            .write_all(&register_bytes(ProtocolVersion::V22))
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, id::REGISTER_ACK).await;
        phone
            .write_all(
                &ClientMessage::ButtonTemplateRequest
                    .encode(ProtocolVersion::V22)
                    .unwrap(),
            )
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::BUTTON_TEMPLATE).await;
        let frame = frames
            .into_iter()
            .find(|frame| frame.message_id == id::BUTTON_TEMPLATE)
            .unwrap();
        assert_eq!(
            ServerMessage::decode(frame, ProtocolVersion::V22).unwrap(),
            ServerMessage::ButtonTemplate { buttons: expected }
        );

        phone
            .write_all(
                &ClientMessage::SpeedDialStatusRequest {
                    speed_dial_instance: 1,
                }
                .encode(ProtocolVersion::V22)
                .unwrap(),
            )
            .await
            .unwrap();
        let frames =
            read_until_message(&mut phone, &mut decoder, id::SPEED_DIAL_STAT_DYNAMIC).await;
        let frame = frames
            .into_iter()
            .find(|frame| frame.message_id == id::SPEED_DIAL_STAT_DYNAMIC)
            .unwrap();
        assert_eq!(
            ServerMessage::decode(frame, ProtocolVersion::V22).unwrap(),
            ServerMessage::SpeedDialStatus {
                instance: 1,
                number: "2001".into(),
                display_name: "Reception".into(),
            }
        );

        phone
            .write_all(
                &ClientMessage::SpeedDialStatusRequest {
                    speed_dial_instance: 2,
                }
                .encode(ProtocolVersion::V22)
                .unwrap(),
            )
            .await
            .unwrap();
        let frames =
            read_until_message(&mut phone, &mut decoder, id::SPEED_DIAL_STAT_DYNAMIC).await;
        let frame = frames
            .into_iter()
            .find(|frame| frame.message_id == id::SPEED_DIAL_STAT_DYNAMIC)
            .unwrap();
        assert_eq!(
            ServerMessage::decode(frame, ProtocolVersion::V22).unwrap(),
            ServerMessage::SpeedDialStatus {
                instance: 2,
                number: "2002".into(),
                display_name: "Warehouse".into(),
            }
        );

        phone
            .write_all(
                &ClientMessage::SpeedDialStatusRequest {
                    speed_dial_instance: 99,
                }
                .encode(ProtocolVersion::V22)
                .unwrap(),
            )
            .await
            .unwrap();
        let frames =
            read_until_message(&mut phone, &mut decoder, id::SPEED_DIAL_STAT_DYNAMIC).await;
        let frame = frames
            .into_iter()
            .find(|frame| frame.message_id == id::SPEED_DIAL_STAT_DYNAMIC)
            .unwrap();
        assert_eq!(
            ServerMessage::decode(frame, ProtocolVersion::V22).unwrap(),
            ServerMessage::SpeedDialStatus {
                instance: 99,
                number: String::new(),
                display_name: String::new(),
            }
        );

        phone
            .write_all(
                &ClientMessage::FeatureStatusRequest {
                    index: 1,
                    capabilities: 0,
                }
                .encode(ProtocolVersion::V22)
                .unwrap(),
            )
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::FEATURE_STAT).await;
        let frame = frames
            .into_iter()
            .find(|frame| frame.message_id == id::FEATURE_STAT)
            .unwrap();
        assert_eq!(
            ServerMessage::decode(frame, ProtocolVersion::V22).unwrap(),
            ServerMessage::FeatureStatus {
                instance: 1,
                button_type: ButtonType::DoNotDisturb,
                label: "DND".into(),
                state: 0,
            }
        );

        phone
            .write_all(
                &ClientMessage::FeatureStatusRequest {
                    index: 2,
                    capabilities: 1,
                }
                .encode(ProtocolVersion::V22)
                .unwrap(),
            )
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::FEATURE_STAT).await;
        let frame = frames
            .into_iter()
            .find(|frame| frame.message_id == id::FEATURE_STAT)
            .unwrap();
        assert_eq!(
            ServerMessage::decode(frame, ProtocolVersion::V22).unwrap(),
            ServerMessage::FeatureStatus {
                instance: 2,
                button_type: ButtonType::BlfSpeedDial,
                label: "Warehouse".into(),
                state: BusyLampFieldState::UnknownState.wire_value(),
            }
        );

        phone
            .write_all(
                &ClientMessage::ServiceUrlStatusRequest { index: 1 }
                    .encode(ProtocolVersion::V22)
                    .unwrap(),
            )
            .await
            .unwrap();
        let frames =
            read_until_message(&mut phone, &mut decoder, id::SERVICE_URL_STAT_DYNAMIC).await;
        let frame = frames
            .into_iter()
            .find(|frame| frame.message_id == id::SERVICE_URL_STAT_DYNAMIC)
            .unwrap();
        assert_eq!(
            ServerMessage::decode(frame, ProtocolVersion::V22).unwrap(),
            ServerMessage::ServiceUrlStatus {
                index: 1,
                url: "http://services.invalid/directory".into(),
                label: "Directory".into(),
                extension_text: String::new(),
            }
        );

        let unknown_requests = [
            ClientMessage::FeatureStatusRequest {
                index: 99,
                capabilities: 0,
            }
            .encode(ProtocolVersion::V22)
            .unwrap(),
            ClientMessage::ServiceUrlStatusRequest { index: 99 }
                .encode(ProtocolVersion::V22)
                .unwrap(),
            ClientMessage::KeepAlive
                .encode(ProtocolVersion::V22)
                .unwrap(),
        ]
        .concat();
        phone.write_all(&unknown_requests).await.unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::KEEP_ALIVE_ACK).await;
        assert!(
            frames.iter().all(|frame| !matches!(
                frame.message_id,
                id::FEATURE_STAT
                    | id::FEATURE_STAT_DYNAMIC
                    | id::SERVICE_URL_STAT
                    | id::SERVICE_URL_STAT_DYNAMIC
            )),
            "unknown feature and service requests must not produce placeholder statuses"
        );

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn anonymous_hotline_registration_gets_one_restricted_public_line() {
        let label = "Guest assistance";
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            anonymous_hotline: Some(AnonymousHotlineDefinition::new(label).unwrap()),
            ..ServerConfig::default()
        };
        let (server, handle, mut events) = Server::bind(config, [definition()]).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let mut phone = TcpStream::connect(address).await.unwrap();
        let mut decoder = FrameDecoder::new();
        let protocol = ProtocolVersion::V22;
        let device = "SEPFFEEDDCCBBAA";

        phone
            .write_all(&register_bytes_for_device(protocol, 115, device))
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent { session_generation: _, device_id: _, event: DeviceEventKind::Registered(registration) })) if registration.id.as_str() == device
        ));

        phone
            .write_all(
                &[
                    ClientMessage::LineStatRequest { line_instance: 1 }
                        .encode(protocol)
                        .unwrap(),
                    ClientMessage::ButtonTemplateRequest
                        .encode(protocol)
                        .unwrap(),
                    ClientMessage::SoftKeySetRequest.encode(protocol).unwrap(),
                ]
                .concat(),
            )
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::SOFT_KEY_SET_RES).await;
        assert!(frames.iter().any(|frame| matches!(
            ServerMessage::decode(frame.clone(), protocol),
            Ok(ServerMessage::LineStatus { instance: 1, number, display_name })
                if number == "hotline" && display_name == label
        )));
        assert!(frames.iter().any(|frame| matches!(
            ServerMessage::decode(frame.clone(), protocol),
            Ok(ServerMessage::ButtonTemplate { buttons })
                if buttons == vec![ButtonTemplateEntry {
                    instance: 1,
                    button_type: ButtonType::Line,
                }]
        )));
        assert!(frames.iter().any(|frame| matches!(
            ServerMessage::decode(frame.clone(), protocol),
            Ok(ServerMessage::SoftKeySet { profile })
                if profile.actions(KeyMode::OnHook) == [SoftKey::NewCall]
                    && profile.actions(KeyMode::OffHook) == [SoftKey::EndCall]
                    && profile.actions(KeyMode::RingOut) == [SoftKey::EndCall]
                    && profile.actions(KeyMode::Connected).is_empty()
        )));

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn anonymous_hotline_reload_isolated_from_configured_session_and_is_idempotent() {
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            anonymous_hotline: Some(AnonymousHotlineDefinition::new("Guest A").unwrap()),
            ..ServerConfig::default()
        };
        let (server, handle, mut events) = Server::bind(config, [definition()]).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let protocol = ProtocolVersion::V22;
        let mut configured = TcpStream::connect(address).await.unwrap();
        let mut configured_decoder = FrameDecoder::new();
        configured
            .write_all(&register_bytes(protocol))
            .await
            .unwrap();
        read_until_message(
            &mut configured,
            &mut configured_decoder,
            id::CAPABILITIES_REQ,
        )
        .await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Registered(_)
            }))
        ));

        let guest_id = "SEPFFEEDDCCBBAA";
        let mut guest = TcpStream::connect(address).await.unwrap();
        let mut guest_decoder = FrameDecoder::new();
        guest
            .write_all(&register_bytes_for_device(protocol, 115, guest_id))
            .await
            .unwrap();
        read_until_message(&mut guest, &mut guest_decoder, id::CAPABILITIES_REQ).await;
        loop {
            match events.recv().await {
                Some(Event::Device(DeviceEvent {
                    session_generation: _,
                    device_id: _,
                    event: DeviceEventKind::Registered(registration),
                })) if registration.id.as_str() == guest_id => {
                    break;
                }
                Some(_) => {}
                None => panic!("server stopped before replacement guest registration"),
            }
        }

        assert_eq!(
            handle
                .reconfigure_anonymous_hotline(Some(
                    AnonymousHotlineDefinition::new("Guest A").unwrap(),
                ))
                .await
                .unwrap(),
            0
        );
        let station_policy = handle
            .reconfigure_station_policy(
                [definition()],
                [],
                Some(AnonymousHotlineDefinition::new("Guest B").unwrap()),
            )
            .await
            .unwrap();
        assert!(station_policy.is_unchanged());
        let mut closed = [0_u8; 1];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), guest.read(&mut closed))
                .await
                .unwrap()
                .unwrap(),
            0
        );

        configured
            .write_all(
                &ClientMessage::LineStatRequest { line_instance: 1 }
                    .encode(protocol)
                    .unwrap(),
            )
            .await
            .unwrap();
        let frames = read_until_message(
            &mut configured,
            &mut configured_decoder,
            id::LINE_STAT_DYNAMIC,
        )
        .await;
        assert!(frames.into_iter().any(|frame| matches!(
            ServerMessage::decode(frame, protocol),
            Ok(ServerMessage::LineStatus { number, .. }) if number == "1001"
        )));

        let mut replacement = TcpStream::connect(address).await.unwrap();
        let mut replacement_decoder = FrameDecoder::new();
        replacement
            .write_all(&register_bytes_for_device(protocol, 115, guest_id))
            .await
            .unwrap();
        read_until_message(
            &mut replacement,
            &mut replacement_decoder,
            id::CAPABILITIES_REQ,
        )
        .await;
        loop {
            match events.recv().await {
                Some(Event::Device(DeviceEvent {
                    session_generation: _,
                    device_id: _,
                    event: DeviceEventKind::Registered(registration),
                })) if registration.id.as_str() == guest_id => {
                    break;
                }
                Some(_) => {}
                None => panic!("server stopped before replacement guest registration"),
            }
        }
        replacement
            .write_all(
                &ClientMessage::LineStatRequest { line_instance: 1 }
                    .encode(protocol)
                    .unwrap(),
            )
            .await
            .unwrap();
        let frames = read_until_message(
            &mut replacement,
            &mut replacement_decoder,
            id::LINE_STAT_DYNAMIC,
        )
        .await;
        assert!(frames.into_iter().any(|frame| matches!(
            ServerMessage::decode(frame, protocol),
            Ok(ServerMessage::LineStatus { number, display_name, .. })
                if number == "hotline" && display_name == "Guest B"
        )));

        assert_eq!(handle.reconfigure_anonymous_hotline(None).await.unwrap(), 1);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), replacement.read(&mut closed))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        assert_eq!(handle.reconfigure_anonymous_hotline(None).await.unwrap(), 0);

        assert_eq!(
            handle
                .reconfigure_anonymous_hotline(Some(
                    AnonymousHotlineDefinition::new("Guest C").unwrap(),
                ))
                .await
                .unwrap(),
            0
        );
        let mut promoted = TcpStream::connect(address).await.unwrap();
        let mut promoted_decoder = FrameDecoder::new();
        promoted
            .write_all(&register_bytes_for_device(protocol, 115, guest_id))
            .await
            .unwrap();
        read_until_message(&mut promoted, &mut promoted_decoder, id::CAPABILITIES_REQ).await;
        loop {
            match events.recv().await {
                Some(Event::Device(DeviceEvent {
                    session_generation: _,
                    device_id: _,
                    event: DeviceEventKind::Registered(registration),
                })) if registration.id.as_str() == guest_id => {
                    break;
                }
                Some(_) => {}
                None => panic!("server stopped before promoted guest registration"),
            }
        }
        let result = handle
            .reconfigure_affected(
                [definition(), definition_for(guest_id)],
                [DeviceId::new(guest_id).unwrap()],
            )
            .await
            .unwrap();
        assert_eq!(result.added, [DeviceId::new(guest_id).unwrap()]);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), promoted.read(&mut closed))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        let mut configured_guest = TcpStream::connect(address).await.unwrap();
        let mut configured_guest_decoder = FrameDecoder::new();
        configured_guest
            .write_all(&register_bytes_for_device(protocol, 115, guest_id))
            .await
            .unwrap();
        read_until_message(
            &mut configured_guest,
            &mut configured_guest_decoder,
            id::CAPABILITIES_REQ,
        )
        .await;
        configured_guest
            .write_all(
                &ClientMessage::LineStatRequest { line_instance: 1 }
                    .encode(protocol)
                    .unwrap(),
            )
            .await
            .unwrap();
        let frames = read_until_message(
            &mut configured_guest,
            &mut configured_guest_decoder,
            id::LINE_STAT_DYNAMIC,
        )
        .await;
        assert!(frames.into_iter().any(|frame| matches!(
            ServerMessage::decode(frame, protocol),
            Ok(ServerMessage::LineStatus { number, .. }) if number == "1001"
        )));

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn duplicate_anonymous_registration_replaces_only_the_previous_guest_session() {
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            anonymous_hotline: Some(AnonymousHotlineDefinition::new("Guest").unwrap()),
            ..ServerConfig::default()
        };
        let (server, handle, mut events) = Server::bind(config, []).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let protocol = ProtocolVersion::V22;
        let guest_id = "SEPFFEEDDCCBBAA";
        let mut first = TcpStream::connect(address).await.unwrap();
        let mut first_decoder = FrameDecoder::new();
        first
            .write_all(&register_bytes_for_device(protocol, 115, guest_id))
            .await
            .unwrap();
        read_until_message(&mut first, &mut first_decoder, id::CAPABILITIES_REQ).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Registered(_)
            }))
        ));

        let mut second = TcpStream::connect(address).await.unwrap();
        let mut second_decoder = FrameDecoder::new();
        second
            .write_all(&register_bytes_for_device(protocol, 115, guest_id))
            .await
            .unwrap();
        read_until_message(&mut second, &mut second_decoder, id::CAPABILITIES_REQ).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Registered(_)
            }))
        ));
        let mut closed = [0_u8; 1];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), first.read(&mut closed))
                .await
                .unwrap()
                .unwrap(),
            0
        );

        second
            .write_all(
                &ClientMessage::LineStatRequest { line_instance: 1 }
                    .encode(protocol)
                    .unwrap(),
            )
            .await
            .unwrap();
        read_until_message(&mut second, &mut second_decoder, id::LINE_STAT_DYNAMIC).await;

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn anonymous_hotline_disabled_rejects_unknown_without_affecting_configured_devices() {
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let (server, handle, _events) = Server::bind(config, [definition()]).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let mut phone = TcpStream::connect(address).await.unwrap();
        let mut decoder = FrameDecoder::new();

        phone
            .write_all(&register_bytes_for_device(
                ProtocolVersion::V22,
                115,
                "SEPFFEEDDCCBBAA",
            ))
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::REGISTER_REJECT).await;
        assert!(frames.into_iter().any(|frame| matches!(
            ServerMessage::decode(frame, ProtocolVersion::V17),
            Ok(ServerMessage::RegisterReject { reason }) if reason == "Device not configured"
        )));

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[test]
    fn anonymous_hotline_definition_bounds_and_debug_are_destination_free() {
        assert!(AnonymousHotlineDefinition::new("").is_err());
        assert!(AnonymousHotlineDefinition::new("x".repeat(80)).is_err());
        assert!(AnonymousHotlineDefinition::new("guest\nline").is_err());
        let definition = AnonymousHotlineDefinition::new("Guest").unwrap();
        assert!(!format!("{definition:?}").contains("111"));
    }

    #[tokio::test]
    async fn mutable_forwarding_and_feature_state_is_published_and_answered() {
        let protocol = ProtocolVersion::V22;
        let device_id = DeviceId::new("SEP001122334455").unwrap();
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let (server, handle, mut events) =
            Server::bind(config, [mixed_definition()]).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let mut phone = TcpStream::connect(address).await.unwrap();
        let mut decoder = FrameDecoder::new();
        phone.write_all(&register_bytes(protocol)).await.unwrap();
        read_until_message(&mut phone, &mut decoder, id::REGISTER_ACK).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Registered(_)
            }))
        ));

        handle
            .send(Command::new(
                device_id.clone(),
                CommandAction::SetForwardStatus {
                    line_instance: LineInstance(1),
                    forward_all: Some("9000".into()),
                    forward_busy: None,
                    forward_no_answer: Some("9001".into()),
                },
            ))
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::FORWARD_STAT).await;
        assert!(frames.into_iter().any(|frame| matches!(
            ServerMessage::decode(frame, protocol),
            Ok(ServerMessage::ForwardStatus {
                line_instance: 1,
                ref forward_all,
                forward_busy: None,
                ref forward_no_answer,
            }) if forward_all.as_deref() == Some("9000")
                && forward_no_answer.as_deref() == Some("9001")
        )));

        handle
            .send(Command::new(
                device_id.clone(),
                CommandAction::SetDoNotDisturbStatus {
                    instance: LineInstance::new(1),
                    mode: DoNotDisturbMode::Reject,
                    button_mode: DoNotDisturbButtonMode::Cycle,
                },
            ))
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::SET_LAMP).await;
        assert!(frames.iter().any(|frame| matches!(
            ServerMessage::decode(frame.clone(), protocol),
            Ok(ServerMessage::FeatureStatus {
                instance: 1,
                button_type: ButtonType::MultiblinkFeature,
                state: 0x020202,
                ..
            })
        )));
        assert!(frames.into_iter().any(|frame| matches!(
            ServerMessage::decode(frame, protocol),
            Ok(ServerMessage::SetLamp {
                stimulus: ButtonType::DoNotDisturb,
                instance: 1,
                mode: LampMode::On,
            })
        )));

        phone
            .write_all(
                &[
                    ClientMessage::ForwardStatusRequest { line_instance: 1 }
                        .encode(protocol)
                        .unwrap(),
                    ClientMessage::FeatureStatusRequest {
                        index: 1,
                        capabilities: 0,
                    }
                    .encode(protocol)
                    .unwrap(),
                ]
                .concat(),
            )
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::FEATURE_STAT).await;
        assert!(frames.iter().any(|frame| matches!(
            ServerMessage::decode(frame.clone(), protocol),
            Ok(ServerMessage::ForwardStatus { ref forward_all, .. })
                if forward_all.as_deref() == Some("9000")
        )));
        assert!(frames.into_iter().any(|frame| matches!(
            ServerMessage::decode(frame, protocol),
            Ok(ServerMessage::FeatureStatus {
                instance: 1,
                button_type: ButtonType::MultiblinkFeature,
                state: 0x020202,
                ..
            })
        )));

        phone
            .write_all(
                &ClientMessage::Stimulus {
                    stimulus: Stimulus::DoNotDisturb,
                    instance: 1,
                    call_reference: 0,
                    status: 0,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent { session_generation: _, device_id: actual_device, event: DeviceEventKind::DoNotDisturbButton {
                instance: LineInstance(1),
            } })) if actual_device == device_id
        ));
        phone
            .write_all(
                &ClientMessage::Stimulus {
                    stimulus: Stimulus::DoNotDisturb,
                    instance: 99,
                    call_reference: 0,
                    status: 0,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(25), events.recv())
                .await
                .is_err()
        );

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn registered_phone_receives_configured_soft_key_sets_and_masks() {
        let mut device = definition();
        device.soft_keys = profile_with(KeyMode::OnHook, vec![SoftKey::NewCall, SoftKey::Redial]);
        let default = device.soft_keys.clone();
        device.soft_keys = SoftKeyProfile::new(KeyMode::ALL_KNOWN.iter().copied().map(|mode| {
            if mode == KeyMode::OffHook {
                (
                    mode,
                    vec![SoftKey::EndCall, SoftKey::Pickup, SoftKey::GroupPickup],
                )
            } else {
                (mode, default.actions(mode).to_vec())
            }
        }))
        .unwrap();
        let expected_profile = device.soft_keys.clone();
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let (server, handle, mut events) = Server::bind(config, [device]).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let mut phone = TcpStream::connect(address).await.unwrap();
        let mut decoder = FrameDecoder::new();
        let protocol = ProtocolVersion::V22;

        phone.write_all(&register_bytes(protocol)).await.unwrap();
        read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Registered(_)
            }))
        ));

        phone
            .write_all(
                &[
                    ClientMessage::SoftKeyTemplateRequest
                        .encode(protocol)
                        .unwrap(),
                    ClientMessage::SoftKeySetRequest.encode(protocol).unwrap(),
                ]
                .concat(),
            )
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::SOFT_KEY_SET_RES).await;
        assert!(frames.iter().any(|frame| matches!(
            ServerMessage::decode(frame.clone(), protocol),
            Ok(ServerMessage::SoftKeyTemplate { actions })
                if actions == expected_profile.template_actions()
        )));
        assert!(frames.iter().any(|frame| matches!(
            ServerMessage::decode(frame.clone(), protocol),
            Ok(ServerMessage::SoftKeySet { profile }) if profile == expected_profile
        )));

        phone
            .write_all(
                &ClientMessage::SoftKeyEvent {
                    event: SoftKey::NewCall.wire_value(),
                    line_instance: 1,
                    call_reference: 0,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::SELECT_SOFT_KEYS).await;
        assert!(frames.iter().any(|frame| matches!(
            ServerMessage::decode(frame.clone(), protocol),
            Ok(ServerMessage::SelectSoftKeys {
                set: KeyMode::OffHook,
                valid_mask: 0b111,
                ..
            })
        )));
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::OffHook { .. }
            }))
        ));
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::SoftKey {
                    soft_key: SoftKey::NewCall,
                    ..
                }
            }))
        ));

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn generic_feature_button_emits_only_for_the_configured_instance() {
        let mut device = definition();
        device
            .buttons
            .push(ButtonDefinition::Feature(FeatureDefinition {
                instance: 1,
                label: "Night service".into(),
                feature: ButtonType::Feature,
            }));
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let (server, handle, mut events) = Server::bind(config, [device]).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let mut phone = TcpStream::connect(address).await.unwrap();
        let mut decoder = FrameDecoder::new();
        let protocol = ProtocolVersion::V22;

        phone.write_all(&register_bytes(protocol)).await.unwrap();
        read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Registered(_)
            }))
        ));

        phone
            .write_all(
                &ClientMessage::Stimulus {
                    stimulus: Stimulus::Privacy,
                    instance: 99,
                    call_reference: 0,
                    status: 0,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), events.recv())
                .await
                .is_err()
        );

        phone
            .write_all(
                &ClientMessage::Stimulus {
                    stimulus: Stimulus::Privacy,
                    instance: 1,
                    call_reference: 0,
                    status: 0,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent { session_generation: _, device_id, event: DeviceEventKind::FeatureButton {
                instance: LineInstance(1),
            } })) if device_id == DeviceId::new("SEP001122334455").unwrap()
        ));

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[test]
    fn mobility_candidate_rebuilds_every_slot_in_physical_button_order() {
        let mut configured = definition();
        configured
            .buttons
            .push(ButtonDefinition::Feature(FeatureDefinition {
                instance: 4,
                label: "Mobility A".into(),
                feature: ButtonType::Mobility,
            }));
        configured
            .buttons
            .push(ButtonDefinition::Feature(FeatureDefinition {
                instance: 5,
                label: "Mobility B".into(),
                feature: ButtonType::Mobility,
            }));
        let first = LineAppearance::new(
            2,
            LineDefinition {
                number: "9001".into(),
                display_name: "Roaming 9001".into(),
            },
        );
        let second = LineAppearance::new(
            3,
            LineDefinition {
                number: "9002".into(),
                display_name: "Roaming 9002".into(),
            },
        );

        let first_map = HashMap::from([(4, first.clone())]);
        let with_first = mobility_device_candidate(&configured, &HashMap::new(), &first_map)
            .expect("first roaming appearance is valid");
        let both_map = HashMap::from([(5, second.clone()), (4, first)]);
        let with_both = mobility_device_candidate(&with_first, &first_map, &both_map)
            .expect("both roaming appearances are valid");
        let projected = with_both
            .buttons
            .iter()
            .filter_map(|button| match button {
                ButtonDefinition::Feature(feature) if feature.feature == ButtonType::Mobility => {
                    Some(("mobility", feature.instance))
                }
                ButtonDefinition::Line(line) if line.instance > 1 => Some(("line", line.instance)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            projected,
            vec![("mobility", 4), ("line", 2), ("mobility", 5), ("line", 3),]
        );
        assert!(matches!(
            line_status(&with_both, 2),
            Some(ServerMessage::LineStatus { number, .. }) if number == "9001"
        ));
        assert!(matches!(
            line_status(&with_both, 3),
            Some(ServerMessage::LineStatus { number, .. }) if number == "9002"
        ));

        let second_map = HashMap::from([(5, second)]);
        let without_first = mobility_device_candidate(&with_both, &both_map, &second_map)
            .expect("removing one roaming appearance preserves the other");
        assert!(line_status(&without_first, 2).is_none());
        assert!(matches!(
            line_status(&without_first, 3),
            Some(ServerMessage::LineStatus { number, .. }) if number == "9002"
        ));
    }

    #[tokio::test]
    async fn mobility_button_and_live_appearance_refresh_preserve_the_session_call() {
        let mut device = definition();
        device
            .buttons
            .push(ButtonDefinition::Feature(FeatureDefinition {
                instance: 4,
                label: "Mobility".into(),
                feature: ButtonType::Mobility,
            }));
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let (server, handle, mut events) = Server::bind(config, [device]).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let mut phone = TcpStream::connect(address).await.unwrap();
        let mut decoder = FrameDecoder::new();
        let protocol = ProtocolVersion::V22;
        let device_id = DeviceId::new("SEP001122334455").unwrap();

        phone.write_all(&register_bytes(protocol)).await.unwrap();
        read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Registered(_)
            }))
        ));
        phone
            .write_all(
                &ClientMessage::Stimulus {
                    stimulus: Stimulus::Mobility,
                    instance: 4,
                    call_reference: 0,
                    status: 0,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::MobilityButton {
                    instance: LineInstance(4),
                    ..
                }
            }))
        ));

        let call_id = CallId(77);
        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::BeginCall {
                    line_instance: LineInstance(1),
                    call_id,
                    codec: Codec::Pcmu,
                },
            ))
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, id::SELECT_SOFT_KEYS).await;

        let roaming = LineAppearance::new(
            2,
            LineDefinition {
                number: "9001".into(),
                display_name: "Roaming 9001".into(),
            },
        );
        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::SetMobilityAppearance {
                    mobility_instance: LineInstance::new(4),
                    appearance: Some(roaming.clone()),
                },
            ))
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::LINE_STAT_DYNAMIC).await;
        assert!(frames.iter().any(|frame| matches!(
            ServerMessage::decode(frame.clone(), protocol),
            Ok(ServerMessage::ButtonTemplate { ref buttons })
                if buttons.iter().any(|button| button.instance == 2 && button.button_type == ButtonType::Line)
        )));
        assert!(frames.iter().any(|frame| matches!(
            ServerMessage::decode(frame.clone(), protocol),
            Ok(ServerMessage::LineStatus { instance: 2, ref number, .. }) if number == "9001"
        )));

        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::SetMobilityAppearance {
                    mobility_instance: LineInstance::new(4),
                    appearance: None,
                },
            ))
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::LINE_STAT_DYNAMIC).await;
        assert!(frames.iter().any(|frame| matches!(
            ServerMessage::decode(frame.clone(), protocol),
            Ok(ServerMessage::LineStatus { instance: 2, ref number, .. }) if number.is_empty()
        )));
        handle
            .send_confirmed(Command::new(
                device_id,
                CommandAction::SetCallState {
                    call_id,
                    state: CallState::Connected,
                },
            ))
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::CALL_STATE).await;
        assert!(frames.iter().any(|frame| matches!(
            ServerMessage::decode(frame.clone(), protocol),
            Ok(ServerMessage::CallState {
                state: CallState::Connected,
                ..
            })
        )));

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn parking_button_menu_and_selection_are_typed_end_to_end() {
        let mut device = definition();
        device
            .buttons
            .push(ButtonDefinition::Feature(FeatureDefinition {
                instance: 4,
                label: "Parking".into(),
                feature: ButtonType::ParkingLot,
            }));
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let (server, handle, mut events) = Server::bind(config, [device]).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let mut phone = TcpStream::connect(address).await.unwrap();
        let mut decoder = FrameDecoder::new();
        let protocol = ProtocolVersion::V22;
        let device_id = DeviceId::new("SEP001122334455").unwrap();

        phone.write_all(&register_bytes(protocol)).await.unwrap();
        read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Registered(_)
            }))
        ));

        phone
            .write_all(
                &ClientMessage::Stimulus {
                    stimulus: Stimulus::ParkingLot,
                    instance: 4,
                    call_reference: 0,
                    status: 0,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent { session_generation: _, device_id: actual, event: DeviceEventKind::ParkingLotButton {
                instance: LineInstance(4),
                call_id: None,
                line_instance: LineInstance(1),
            } })) if actual == device_id
        ));

        handle
            .send(Command::new(
                device_id.clone(),
                CommandAction::ShowParkingMenu {
                    instance: LineInstance::new(4),
                    transaction_id: TransactionId(17),
                    lot: "east & west".into(),
                    calls: vec![ParkingMenuEntry {
                        slot: 701,
                        caller_name: "Taylor <T>".into(),
                        caller_number: "2100".into(),
                        connected_name: "Desk".into(),
                        connected_number: "1001".into(),
                    }],
                },
            ))
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::USER_TO_DEVICE_DATA_V1).await;
        let message = frames
            .into_iter()
            .find(|frame| frame.message_id == id::USER_TO_DEVICE_DATA_V1)
            .map(|frame| ServerMessage::decode(frame, protocol).unwrap())
            .unwrap();
        let ServerMessage::UserToDeviceDataV1(menu) = message else {
            panic!("expected parking menu application data");
        };
        assert_eq!(menu.application_id, PARKING_APPLICATION_ID);
        assert_eq!(menu.line_instance, 4);
        assert_eq!(menu.call_reference, 0);
        assert_eq!(menu.transaction_id, 17);
        let xml = String::from_utf8(menu.data).unwrap();
        assert!(xml.contains("Taylor &lt;T&gt;"));
        assert!(xml.contains("UserCallData:9090:4:0:17:"));
        assert!(xml.contains("retrieve/east%20%26%20west/701"));

        phone
            .write_all(
                &ClientMessage::DeviceToUserDataV1(UserDataV1Message {
                    application_id: PARKING_APPLICATION_ID,
                    line_instance: 4,
                    call_reference: 0,
                    transaction_id: 17,
                    sequence_flag: 0,
                    display_priority: 0,
                    conference_id: 0,
                    application_instance_id: 4,
                    routing: 0,
                    data: b"retrieve/east%20%26%20west/701".to_vec(),
                })
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent { session_generation: _, device_id: actual, event: DeviceEventKind::ParkingMenuSelection {
                lot,
                slot: 701,
            } })) if actual == device_id && lot == "east & west"
        ));
        let Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: actual,
            event: DeviceEventKind::PhoneServiceResponse { response },
        })) = events.recv().await
        else {
            panic!("expected typed phone-service response");
        };
        assert_eq!(actual, device_id);
        assert_eq!(response.kind, PhoneServiceMessageKind::Data);
        assert_eq!(response.routing.application_id, ApplicationId::new(9090));
        assert_eq!(response.routing.line_instance, LineInstance::new(4));
        assert_eq!(response.routing.call_reference, CallReference::new(0));
        assert_eq!(response.routing.transaction_id, TransactionId::new(17));
        let PhoneServicePayload::Submission(submission) = response.payload else {
            panic!("expected typed menu submission");
        };
        assert_eq!(submission.route, ["retrieve", "east & west", "701"]);
        assert!(submission.values.is_empty());

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn conference_list_uses_protocol_family_and_routes_typed_actions() {
        for (protocol, family) in [
            (ProtocolVersion::V3, ConferenceMenuFamily::Menu),
            (ProtocolVersion::V22, ConferenceMenuFamily::IconMenu),
        ] {
            let config = ServerConfig {
                bind: "127.0.0.1:0".parse().unwrap(),
                advertised_address: Ipv4Addr::LOCALHOST,
                ..ServerConfig::default()
            };
            let (server, handle, mut events) = Server::bind(config, [definition()]).await.unwrap();
            let address = server.local_addr().unwrap();
            let task = tokio::spawn(server.run());
            let mut phone = TcpStream::connect(address).await.unwrap();
            let mut decoder = FrameDecoder::new();
            let device_id = DeviceId::new("SEP001122334455").unwrap();

            phone.write_all(&register_bytes(protocol)).await.unwrap();
            read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
            assert!(matches!(
                events.recv().await,
                Some(Event::Device(DeviceEvent {
                    session_generation: _,
                    device_id: _,
                    event: DeviceEventKind::Registered(_)
                }))
            ));
            handle
                .send(Command::new(
                    device_id.clone(),
                    CommandAction::BeginCall {
                        line_instance: LineInstance(1),
                        call_id: CallId(7001),
                        codec: Codec::Pcma,
                    },
                ))
                .await
                .unwrap();
            read_until_message(&mut phone, &mut decoder, id::CALL_STATE).await;

            handle
                .send(Command::new(
                    device_id.clone(),
                    CommandAction::ShowConferenceList {
                        call_id: CallId(7001),
                        conference_id: ConferenceId::new(44),
                        participants: vec![ConferenceListEntry {
                            participant_id: crate::ParticipantId::new(7),
                            name: "Taylor <T>".into(),
                            number: "2100".into(),
                            moderator: false,
                            muted: false,
                        }],
                    },
                ))
                .await
                .unwrap();
            let frames =
                read_until_message(&mut phone, &mut decoder, id::USER_TO_DEVICE_DATA_V1).await;
            let message = frames
                .into_iter()
                .find(|frame| frame.message_id == id::USER_TO_DEVICE_DATA_V1)
                .map(|frame| ServerMessage::decode(frame, protocol).unwrap())
                .unwrap();
            let ServerMessage::UserToDeviceDataV1(menu) = message else {
                panic!("expected conference-list application data");
            };
            assert_eq!(menu.application_id, ConferenceListAction::APPLICATION_ID);
            assert_eq!(menu.conference_id, 44);
            let document = ConferenceListDocument::from_xml(&menu.data, family).unwrap();
            assert_eq!(
                document.actions().collect::<Vec<_>>(),
                [
                    ConferenceListAction::Participant {
                        conference_id: ConferenceId::new(44),
                        participant_id: crate::ParticipantId::new(7),
                    },
                    ConferenceListAction::End {
                        conference_id: ConferenceId::new(44),
                    },
                ]
            );
            assert!(
                String::from_utf8(menu.data)
                    .unwrap()
                    .contains("Taylor &lt;T&gt;")
            );

            phone
                .write_all(
                    &ClientMessage::DeviceToUserDataV1(UserDataV1Message {
                        application_id: ConferenceListAction::APPLICATION_ID,
                        line_instance: 1,
                        call_reference: 7001,
                        transaction_id: 44,
                        sequence_flag: 0,
                        display_priority: 0,
                        conference_id: 44,
                        application_instance_id: 1,
                        routing: 0,
                        data: b"conference/44/participant/7".to_vec(),
                    })
                    .encode(protocol)
                    .unwrap(),
                )
                .await
                .unwrap();
            assert!(matches!(
                events.recv().await,
                Some(Event::Device(DeviceEvent { session_generation: _, device_id: actual, event: DeviceEventKind::ConferenceListAction {
                    action: ConferenceListAction::Participant {
                        conference_id,
                        participant_id,
                    },
                } })) if actual == device_id
                    && conference_id == ConferenceId::new(44)
                    && participant_id == crate::ParticipantId::new(7)
            ));
            assert!(matches!(
                events.recv().await,
                Some(Event::Device(DeviceEvent {
                    session_generation: _,
                    device_id: _,
                    event: DeviceEventKind::PhoneServiceResponse { .. }
                }))
            ));

            handle
                .send(Command::new(
                    device_id.clone(),
                    CommandAction::ShowConferenceParticipantActions {
                        call_id: CallId(7001),
                        conference_id: ConferenceId::new(44),
                        participant: ConferenceListEntry {
                            participant_id: crate::ParticipantId::new(7),
                            name: "Taylor <T>".into(),
                            number: "2100".into(),
                            moderator: false,
                            muted: false,
                        },
                        removable: true,
                        demotable: false,
                    },
                ))
                .await
                .unwrap();
            let frames =
                read_until_message(&mut phone, &mut decoder, id::USER_TO_DEVICE_DATA_V1).await;
            let message = frames
                .into_iter()
                .find(|frame| frame.message_id == id::USER_TO_DEVICE_DATA_V1)
                .map(|frame| ServerMessage::decode(frame, protocol).unwrap())
                .unwrap();
            let ServerMessage::UserToDeviceDataV1(menu) = message else {
                panic!("expected conference-participant action menu");
            };
            let document =
                ConferenceParticipantActionsDocument::from_xml(&menu.data, family).unwrap();
            assert_eq!(
                document.actions().collect::<Vec<_>>(),
                [
                    ConferenceListAction::Mute {
                        conference_id: ConferenceId::new(44),
                        participant_id: crate::ParticipantId::new(7),
                    },
                    ConferenceListAction::Remove {
                        conference_id: ConferenceId::new(44),
                        participant_id: crate::ParticipantId::new(7),
                    },
                    ConferenceListAction::Promote {
                        conference_id: ConferenceId::new(44),
                        participant_id: crate::ParticipantId::new(7),
                    },
                ]
            );

            phone
                .write_all(
                    &ClientMessage::DeviceToUserDataV1(UserDataV1Message {
                        application_id: ConferenceListAction::APPLICATION_ID,
                        line_instance: 1,
                        call_reference: 7001,
                        transaction_id: 44,
                        sequence_flag: 0,
                        display_priority: 0,
                        conference_id: 44,
                        application_instance_id: 1,
                        routing: 0,
                        data: b"conference/44/participant/7/remove".to_vec(),
                    })
                    .encode(protocol)
                    .unwrap(),
                )
                .await
                .unwrap();
            assert!(matches!(
                events.recv().await,
                Some(Event::Device(DeviceEvent { session_generation: _, device_id: actual, event: DeviceEventKind::ConferenceListAction {
                    action: ConferenceListAction::Remove {
                        conference_id,
                        participant_id,
                    },
                } })) if actual == device_id
                    && conference_id == ConferenceId::new(44)
                    && participant_id == crate::ParticipantId::new(7)
            ));
            assert!(matches!(
                events.recv().await,
                Some(Event::Device(DeviceEvent {
                    session_generation: _,
                    device_id: _,
                    event: DeviceEventKind::PhoneServiceResponse { .. }
                }))
            ));
            for (route, expected) in [
                (
                    b"conference/44/participant/7/mute".as_slice(),
                    ConferenceListAction::Mute {
                        conference_id: ConferenceId::new(44),
                        participant_id: crate::ParticipantId::new(7),
                    },
                ),
                (
                    b"conference/44/participant/7/unmute".as_slice(),
                    ConferenceListAction::Unmute {
                        conference_id: ConferenceId::new(44),
                        participant_id: crate::ParticipantId::new(7),
                    },
                ),
                (
                    b"conference/44/participant/7/promote".as_slice(),
                    ConferenceListAction::Promote {
                        conference_id: ConferenceId::new(44),
                        participant_id: crate::ParticipantId::new(7),
                    },
                ),
                (
                    b"conference/44/participant/7/demote".as_slice(),
                    ConferenceListAction::Demote {
                        conference_id: ConferenceId::new(44),
                        participant_id: crate::ParticipantId::new(7),
                    },
                ),
                (
                    b"conference/44/end".as_slice(),
                    ConferenceListAction::End {
                        conference_id: ConferenceId::new(44),
                    },
                ),
            ] {
                phone
                    .write_all(
                        &ClientMessage::DeviceToUserDataV1(UserDataV1Message {
                            application_id: ConferenceListAction::APPLICATION_ID,
                            line_instance: 1,
                            call_reference: 7001,
                            transaction_id: 44,
                            sequence_flag: 0,
                            display_priority: 0,
                            conference_id: 44,
                            application_instance_id: 1,
                            routing: 0,
                            data: route.to_vec(),
                        })
                        .encode(protocol)
                        .unwrap(),
                    )
                    .await
                    .unwrap();
                assert!(matches!(
                    events.recv().await,
                    Some(Event::Device(DeviceEvent { session_generation: _, device_id: actual, event: DeviceEventKind::ConferenceListAction {
                        action,
                    } })) if actual == device_id && action == expected
                ));
                assert!(matches!(
                    events.recv().await,
                    Some(Event::Device(DeviceEvent {
                        session_generation: _,
                        device_id: _,
                        event: DeviceEventKind::PhoneServiceResponse { .. }
                    }))
                ));
            }

            handle
                .send(Command::new(
                    device_id.clone(),
                    CommandAction::ShowConferenceParticipantActions {
                        call_id: CallId(7001),
                        conference_id: ConferenceId::new(44),
                        participant: ConferenceListEntry {
                            participant_id: crate::ParticipantId::new(7),
                            name: "Taylor <T>".into(),
                            number: "2100".into(),
                            moderator: true,
                            muted: false,
                        },
                        removable: false,
                        demotable: true,
                    },
                ))
                .await
                .unwrap();
            let frames =
                read_until_message(&mut phone, &mut decoder, id::USER_TO_DEVICE_DATA_V1).await;
            let message = frames
                .into_iter()
                .find(|frame| frame.message_id == id::USER_TO_DEVICE_DATA_V1)
                .map(|frame| ServerMessage::decode(frame, protocol).unwrap())
                .unwrap();
            let ServerMessage::UserToDeviceDataV1(menu) = message else {
                panic!("expected moderator conference-participant action menu");
            };
            let document =
                ConferenceParticipantActionsDocument::from_xml(&menu.data, family).unwrap();
            assert_eq!(
                document.actions().collect::<Vec<_>>(),
                [ConferenceListAction::Demote {
                    conference_id: ConferenceId::new(44),
                    participant_id: crate::ParticipantId::new(7),
                }]
            );

            handle.shutdown().await.unwrap();
            task.await.unwrap().unwrap();
        }
    }

    #[tokio::test]
    async fn phone_service_responses_preserve_legacy_and_extended_routing() {
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let (server, handle, mut events) = Server::bind(config, [definition()]).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let mut phone = TcpStream::connect(address).await.unwrap();
        let mut decoder = FrameDecoder::new();
        let protocol = ProtocolVersion::V22;
        let device_id = DeviceId::new("SEP001122334455").unwrap();

        phone.write_all(&register_bytes(protocol)).await.unwrap();
        read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Registered(_)
            }))
        ));

        phone
            .write_all(
                &ClientMessage::DeviceToUserDataResponseV1(UserDataV1Message {
                    application_id: 9084,
                    line_instance: 2,
                    call_reference: 42,
                    transaction_id: 73,
                    sequence_flag: 1,
                    display_priority: 2,
                    conference_id: 51,
                    application_instance_id: 6,
                    routing: 4,
                    data: br#"<CiscoIPPhoneResponse><ResponseItem Status="0" Data="ok &amp; ready" URL="Init:Services"/></CiscoIPPhoneResponse>"#.to_vec(),
                })
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        let Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: actual,
            event: DeviceEventKind::PhoneServiceResponse { response },
        })) = events.recv().await
        else {
            panic!("expected typed execute response");
        };
        assert_eq!(actual, device_id);
        assert_eq!(response.kind, PhoneServiceMessageKind::Response);
        assert_eq!(response.routing.application_id, ApplicationId::new(9084));
        assert_eq!(response.routing.line_instance, LineInstance::new(2));
        assert_eq!(response.routing.call_reference, CallReference::new(42));
        assert_eq!(response.routing.transaction_id, TransactionId::new(73));
        assert_eq!(
            response.extended,
            Some(PhoneServiceExtendedRouting {
                sequence_flag: 1,
                display_priority: 2,
                conference_id: 51,
                application_instance_id: 6,
                routing: 4,
            })
        );
        let PhoneServicePayload::ExecuteResponse(execute) = response.payload else {
            panic!("expected typed execute response payload");
        };
        assert_eq!(execute.items.len(), 1);
        assert_eq!(execute.items[0].status.get(), 0);
        assert_eq!(execute.items[0].data, "ok & ready");
        assert_eq!(execute.items[0].url, "Init:Services");

        phone
            .write_all(
                &ClientMessage::DeviceToUserData(crate::message::UserDataMessage {
                    application_id: 9083,
                    line_instance: 1,
                    call_reference: 43,
                    transaction_id: 74,
                    data: b"invite?NUMBER=555%2A12&NUMBER=555%2A13&NAME=Fran%C3%A7ois".to_vec(),
                })
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        let Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::PhoneServiceResponse { response, .. },
        })) = events.recv().await
        else {
            panic!("expected typed input submission");
        };
        assert_eq!(response.extended, None);
        assert_eq!(response.routing.application_id, ApplicationId::new(9083));
        assert_eq!(response.routing.line_instance, LineInstance::new(1));
        assert_eq!(response.routing.call_reference, CallReference::new(43));
        assert_eq!(response.routing.transaction_id, TransactionId::new(74));
        let PhoneServicePayload::Submission(submission) = response.payload else {
            panic!("expected typed input submission payload");
        };
        assert_eq!(submission.route, ["invite"]);
        assert_eq!(
            submission.values_named("NUMBER").collect::<Vec<_>>(),
            ["555*12", "555*13"]
        );
        assert_eq!(
            submission.values_named("NAME").collect::<Vec<_>>(),
            ["François"]
        );

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn parking_selection_requires_the_pending_envelope_and_survives_malformed_data() {
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let (server, handle, mut events) = Server::bind(config, [definition()]).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let mut phone = TcpStream::connect(address).await.unwrap();
        let mut decoder = FrameDecoder::new();
        let protocol = ProtocolVersion::V22;
        let device_id = DeviceId::new("SEP001122334455").unwrap();

        phone.write_all(&register_bytes(protocol)).await.unwrap();
        read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Registered(_)
            }))
        ));
        handle
            .send(Command::new(
                device_id.clone(),
                CommandAction::ShowParkingMenu {
                    instance: LineInstance::new(4),
                    transaction_id: TransactionId(17),
                    lot: "main".into(),
                    calls: vec![],
                },
            ))
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, id::USER_TO_DEVICE_DATA_V1).await;

        let response = |application_id,
                        line_instance,
                        call_reference,
                        transaction_id,
                        instance,
                        data: &[u8]| {
            ClientMessage::DeviceToUserDataV1(UserDataV1Message {
                application_id,
                line_instance,
                call_reference,
                transaction_id,
                sequence_flag: 0,
                display_priority: 0,
                conference_id: 0,
                application_instance_id: instance,
                routing: 0,
                data: data.to_vec(),
            })
            .encode(protocol)
            .unwrap()
        };

        for (application_id, line_instance, call_reference, transaction_id, instance) in [
            (9083, 4, 0, 17, 4),
            (PARKING_APPLICATION_ID, 5, 0, 17, 4),
            (PARKING_APPLICATION_ID, 4, 9, 17, 4),
            (PARKING_APPLICATION_ID, 4, 0, 18, 4),
            (PARKING_APPLICATION_ID, 4, 0, 17, 5),
        ] {
            phone
                .write_all(&response(
                    application_id,
                    line_instance,
                    call_reference,
                    transaction_id,
                    instance,
                    b"retrieve/main/701",
                ))
                .await
                .unwrap();
            let Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event:
                    DeviceEventKind::PhoneServiceResponse {
                        response: routed, ..
                    },
            })) = events.recv().await
            else {
                panic!("expected mismatched response to remain generically routed");
            };
            assert_eq!(routed.routing.application_id.get(), application_id);
            assert_eq!(routed.routing.line_instance.get(), line_instance);
            assert_eq!(routed.routing.call_reference.get(), call_reference);
            assert_eq!(routed.routing.transaction_id.get(), transaction_id);
            assert!(
                tokio::time::timeout(Duration::from_millis(25), events.recv())
                    .await
                    .is_err(),
                "mismatched envelope emitted a parking action"
            );
        }

        phone
            .write_all(&response(
                PARKING_APPLICATION_ID,
                4,
                0,
                17,
                4,
                b"retrieve/secret%GG/701",
            ))
            .await
            .unwrap();
        let Some(Event::ProtocolWarning {
            message_id, error, ..
        }) = events.recv().await
        else {
            panic!("expected malformed service-data warning");
        };
        assert_eq!(message_id, id::DEVICE_TO_USER_DATA_V1);
        assert!(!error.contains("secret"));

        phone
            .write_all(&response(
                PARKING_APPLICATION_ID,
                4,
                0,
                17,
                4,
                b"retrieve/main/701",
            ))
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent { session_generation: _, device_id: actual, event: DeviceEventKind::ParkingMenuSelection {
                lot,
                slot: 701,
            } })) if actual == device_id && lot == "main"
        ));
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::PhoneServiceResponse { .. }
            }))
        ));

        phone
            .write_all(&response(
                PARKING_APPLICATION_ID,
                4,
                0,
                17,
                4,
                b"retrieve/main/701",
            ))
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::PhoneServiceResponse { .. }
            }))
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(25), events.recv())
                .await
                .is_err(),
            "replayed selection emitted a second parking action"
        );

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[test]
    fn parking_menu_xml_is_typed_round_trippable_and_size_bounded() {
        let calls = [ParkingMenuEntry {
            slot: 701,
            caller_name: "Taylor <T> & Co".into(),
            caller_number: "2100".into(),
            connected_name: "Desk".into(),
            connected_number: "1001".into(),
        }];
        let xml = parking_menu_xml(4, 17, "east & west", &calls).unwrap();
        assert!(xml.contains("Taylor &lt;T&gt; &amp; Co"));
        let decoded = CiscoIpPhoneMenu::from_xml_with_limit(xml.as_bytes(), 2_000).unwrap();
        assert_eq!(decoded.title.as_deref(), Some("Parked calls - east & west"));
        assert_eq!(decoded.items.len(), 1);
        assert_eq!(
            decoded.items[0].url.as_deref(),
            Some("UserCallData:9090:4:0:17:retrieve/east%20%26%20west/701")
        );

        let oversized = [ParkingMenuEntry {
            caller_name: "x".repeat(2100),
            ..calls[0].clone()
        }];
        assert!(matches!(
            parking_menu_xml(4, 17, "main", &oversized),
            Err(ServerError::PhoneXml(PhoneXmlError::InvalidField {
                field: "menu item name",
                ..
            }))
        ));
        let byte_oversized = vec![
            ParkingMenuEntry {
                caller_name: "x".repeat(45),
                ..calls[0].clone()
            };
            PARKING_MENU_MAX_ITEMS
        ];
        assert!(matches!(
            parking_menu_xml(4, 17, "main", &byte_oversized),
            Err(ServerError::PhoneXml(PhoneXmlError::LimitExceeded {
                kind: "phone XML document",
                maximum: 2_000,
                ..
            }))
        ));
        assert!(matches!(
            parking_menu_xml(
                4,
                17,
                "main",
                &vec![calls[0].clone(); PARKING_MENU_MAX_ITEMS + 1]
            ),
            Err(ServerError::PhoneXml(error)) if error.to_string().contains("maximum is 32")
        ));
        assert!(
            CiscoIpPhoneMenu::from_xml_with_limit(b"<CiscoIPPhoneMenu><Title>broken", 2_000,)
                .is_err()
        );

        #[derive(Debug)]
        struct FailingWriter;

        impl std::fmt::Write for FailingWriter {
            fn write_str(&mut self, _value: &str) -> std::fmt::Result {
                Err(std::fmt::Error)
            }
        }

        assert!(matches!(
            phone_xml::to_writer(FailingWriter, &decoded, 2_000),
            Err(PhoneXmlError::Write(_))
        ));
    }

    #[tokio::test]
    async fn begin_call_creates_the_reserved_retrieval_identity() {
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let (server, handle, mut events) = Server::bind(config, [definition()]).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let mut phone = TcpStream::connect(address).await.unwrap();
        let mut decoder = FrameDecoder::new();
        let protocol = ProtocolVersion::V22;
        let device_id = DeviceId::new("SEP001122334455").unwrap();

        phone.write_all(&register_bytes(protocol)).await.unwrap();
        read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Registered(_)
            }))
        ));
        handle
            .send(Command::new(
                device_id.clone(),
                CommandAction::BeginCall {
                    line_instance: LineInstance(1),
                    call_id: CallId(7001),
                    codec: Codec::Pcma,
                },
            ))
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::CALL_STATE).await;
        assert!(frames.into_iter().any(|frame| matches!(
            ServerMessage::decode(frame, protocol),
            Ok(ServerMessage::CallState {
                state: CallState::OffHook,
                line_instance: 1,
                call_reference: 7001,
            })
        )));

        let info = CallInfo {
            direction: crate::types::CallDirection::Inbound,
            calling_name: "Caller".into(),
            calling_number: "2100".into(),
            called_name: "Park 701".into(),
            called_number: "701".into(),
            original_called_name: "Reception".into(),
            original_called_number: "2000".into(),
            last_redirecting_name: "Front Desk".into(),
            last_redirecting_number: "2050".into(),
            original_redirect_reason: 2,
            last_redirect_reason: 4,
            party_restrictions: 0xf,
        };
        handle
            .send(Command::new(
                device_id,
                CommandAction::SetCallInfo {
                    call_id: CallId(7001),
                    info: info.clone(),
                },
            ))
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::CALL_INFO_DYNAMIC).await;
        assert!(frames.into_iter().any(|frame| matches!(
            ServerMessage::decode(frame, protocol),
            Ok(ServerMessage::CallInfo {
                info: actual,
                line_instance: 1,
                call_reference: 7001,
            }) if actual == info
        )));

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn unavailable_soft_key_events_and_stimuli_preserve_on_hook_state() {
        let mut device = definition();
        device.soft_keys = profile_with(KeyMode::OnHook, vec![SoftKey::Redial]);
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let (server, handle, mut events) = Server::bind(config, [device]).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let mut phone = TcpStream::connect(address).await.unwrap();
        let mut decoder = FrameDecoder::new();
        let protocol = ProtocolVersion::V22;

        phone.write_all(&register_bytes(protocol)).await.unwrap();
        read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Registered(_)
            }))
        ));

        phone
            .write_all(
                &[
                    ClientMessage::SoftKeyEvent {
                        event: SoftKey::NewCall.wire_value(),
                        line_instance: 1,
                        call_reference: 0,
                    }
                    .encode(protocol)
                    .unwrap(),
                    ClientMessage::Stimulus {
                        stimulus: Stimulus::NewCall,
                        instance: 1,
                        call_reference: 0,
                        status: 0,
                    }
                    .encode(protocol)
                    .unwrap(),
                ]
                .concat(),
            )
            .await
            .unwrap();
        let mut buffer = [0_u8; 256];
        assert!(
            tokio::time::timeout(Duration::from_millis(50), phone.read(&mut buffer))
                .await
                .is_err(),
            "unavailable actions unexpectedly changed the handset UI"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), events.recv())
                .await
                .is_err(),
            "unavailable actions unexpectedly emitted an application event"
        );

        phone
            .write_all(
                &ClientMessage::Stimulus {
                    stimulus: Stimulus::Line,
                    instance: 1,
                    call_reference: 0,
                    status: 0,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, id::SELECT_SOFT_KEYS).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::OffHook {
                    call_id: CallId(1),
                    ..
                }
            }))
        ));

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn one_way_intercom_uses_restricted_keys_active_identity_and_microphone_frame() {
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let (server, handle, mut events) = Server::bind(config, [definition()]).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let mut phone = TcpStream::connect(address).await.unwrap();
        let mut decoder = FrameDecoder::new();
        let protocol = ProtocolVersion::V22;

        phone.write_all(&register_bytes(protocol)).await.unwrap();
        read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Registered(_)
            }))
        ));

        let device_id = DeviceId::new("SEP001122334455").unwrap();
        let call_id = CallId(7010);
        handle
            .send(Command::new(
                device_id.clone(),
                CommandAction::BeginCall {
                    line_instance: LineInstance(1),
                    call_id,
                    codec: Codec::Pcma,
                },
            ))
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, id::SELECT_SOFT_KEYS).await;
        handle
            .send(Command::new(
                device_id.clone(),
                CommandAction::SetCallState {
                    call_id,
                    state: CallState::IntercomOneWay,
                },
            ))
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::SELECT_SOFT_KEYS).await;
        assert!(frames.iter().any(|frame| matches!(
            ServerMessage::decode(frame.clone(), protocol),
            Ok(ServerMessage::CallState {
                state: CallState::IntercomOneWay,
                line_instance: 1,
                call_reference: 7010,
            })
        )));
        assert!(frames.iter().any(|frame| matches!(
            ServerMessage::decode(frame.clone(), protocol),
            Ok(ServerMessage::SelectSoftKeys {
                line_instance: 1,
                call_reference: 7010,
                set: KeyMode::OffHook,
                valid_mask: 1,
            })
        )));

        phone
            .write_all(
                &ClientMessage::SoftKeyEvent {
                    event: SoftKey::NewCall.wire_value(),
                    line_instance: 1,
                    call_reference: 0,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), events.recv())
                .await
                .is_err()
        );
        phone
            .write_all(
                &ClientMessage::SoftKeyEvent {
                    event: SoftKey::EndCall.wire_value(),
                    line_instance: 1,
                    call_reference: 0,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent { session_generation: _, device_id: actual_device, event: DeviceEventKind::SoftKey {
                call_id: Some(CallId(7010)),
                line_instance: LineInstance(1),
                soft_key: SoftKey::EndCall,
            } })) if actual_device == device_id
        ));

        handle
            .send_confirmed(Command::new(
                device_id,
                CommandAction::SetMicrophoneMode { enabled: false },
            ))
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::SET_MICROPHONE_MODE).await;
        assert!(frames.into_iter().any(|frame| matches!(
            ServerMessage::decode(frame, protocol),
            Ok(ServerMessage::SetMicrophoneMode(MicrophoneMode::Off))
        )));

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn outbound_media_writes_receive_then_transmit_without_an_ack_boundary() {
        let device = definition();
        let device_id = device.id.clone();
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let (server, handle, mut events) = Server::bind(config, [device]).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let mut phone = TcpStream::connect(address).await.unwrap();
        let mut decoder = FrameDecoder::new();
        let protocol = ProtocolVersion::V22;

        phone.write_all(&register_bytes(protocol)).await.unwrap();
        read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Registered(_)
            }))
        ));
        phone
            .write_all(
                &ClientMessage::Stimulus {
                    stimulus: Stimulus::Line,
                    instance: 1,
                    call_reference: 0,
                    status: 0,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, id::SELECT_SOFT_KEYS).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::OffHook { .. }
            }))
        ));

        phone
            .write_all(
                &ClientMessage::KeypadButton {
                    button: Digit::Number(2),
                    line_instance: 1,
                    call_reference: 1,
                    wire_layout: None,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, id::SELECT_SOFT_KEYS).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Digit { .. }
            }))
        ));
        phone
            .write_all(
                &ClientMessage::KeypadButton {
                    button: Digit::Pound,
                    line_instance: 1,
                    call_reference: 1,
                    wire_layout: None,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Digit {
                    digit: Digit::Pound,
                    ..
                }
            }))
        ));
        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::CommitOutboundCall {
                    call_id: CallId(1),
                    info: CallInfo {
                        direction: crate::CallDirection::Outbound,
                        called_number: "2".into(),
                        ..CallInfo::default()
                    },
                },
            ))
            .await
            .unwrap();
        let prefix = read_until_message(&mut phone, &mut decoder, id::CALL_STATE).await;
        let stop_tone = prefix
            .iter()
            .position(|frame| frame.message_id == id::STOP_TONE)
            .expect("outbound route prefix omitted StopTone");
        let call_info = prefix
            .iter()
            .position(|frame| frame.message_id == id::CALL_INFO_DYNAMIC)
            .expect("outbound route prefix omitted CallInfo");
        let dialed_number = prefix
            .iter()
            .position(|frame| {
                matches!(
                    ServerMessage::decode(frame.clone(), protocol),
                    Ok(ServerMessage::DialedNumber { ref number, .. }) if number == "2"
                )
            })
            .expect("outbound route prefix omitted DialedNumber");
        let proceed = prefix
            .iter()
            .position(|frame| {
                matches!(
                    ServerMessage::decode(frame.clone(), protocol),
                    Ok(ServerMessage::CallState {
                        state: CallState::Proceed,
                        ..
                    })
                )
            })
            .expect("outbound media prefix omitted Proceed");
        assert!(stop_tone < call_info && call_info < dialed_number && dialed_number < proceed);
        assert!(prefix[..proceed].iter().all(|frame| {
            !matches!(
                ServerMessage::decode(frame.clone(), protocol),
                Ok(ServerMessage::CallState {
                    state: CallState::OffHook,
                    ..
                })
            ) && frame.message_id != id::ACTIVATE_CALL_PLANE
        }));

        let outbound_info = CallInfo {
            direction: crate::CallDirection::Outbound,
            called_name: "Remote Party".into(),
            called_number: "2".into(),
            ..CallInfo::default()
        };
        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::PresentOutboundProceeding {
                    call_id: CallId(1),
                    info: outbound_info.clone(),
                },
            ))
            .await
            .unwrap();
        let proceeding =
            read_until_message(&mut phone, &mut decoder, id::DISPLAY_DYNAMIC_PROMPT_STATUS).await;
        let proceeding_ids = proceeding
            .iter()
            .map(|frame| frame.message_id)
            .collect::<Vec<_>>();
        let stop = proceeding_ids
            .iter()
            .position(|message_id| *message_id == id::STOP_TONE)
            .unwrap();
        let state = proceeding_ids
            .iter()
            .position(|message_id| *message_id == id::CALL_STATE)
            .unwrap();
        let info = proceeding_ids
            .iter()
            .position(|message_id| *message_id == id::CALL_INFO_DYNAMIC)
            .unwrap();
        let prompt = proceeding_ids
            .iter()
            .position(|message_id| *message_id == id::DISPLAY_DYNAMIC_PROMPT_STATUS)
            .unwrap();
        assert!(stop < state && state < info && info < prompt);

        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::PresentOutboundRinging {
                    call_id: CallId(1),
                    info: outbound_info,
                },
            ))
            .await
            .unwrap();
        let ringing = read_until_message(&mut phone, &mut decoder, id::CALL_INFO_DYNAMIC).await;
        let ringing_ids = ringing
            .iter()
            .map(|frame| frame.message_id)
            .collect::<Vec<_>>();
        let state = ringing_ids
            .iter()
            .position(|message_id| *message_id == id::CALL_STATE)
            .unwrap();
        let prompt = ringing_ids
            .iter()
            .position(|message_id| *message_id == id::DISPLAY_DYNAMIC_PROMPT_STATUS)
            .unwrap();
        let tone = ringing_ids
            .iter()
            .position(|message_id| *message_id == id::START_TONE)
            .unwrap();
        let keys = ringing_ids
            .iter()
            .position(|message_id| *message_id == id::SELECT_SOFT_KEYS)
            .unwrap();
        let info = ringing_ids
            .iter()
            .position(|message_id| *message_id == id::CALL_INFO_DYNAMIC)
            .unwrap();
        assert!(state < prompt && prompt < tone && tone < keys && keys < info);
        assert_eq!(
            ringing
                .iter()
                .filter(|frame| frame.message_id == id::DISPLAY_DYNAMIC_PROMPT_STATUS)
                .count(),
            1,
            "outbound ringing flashed an intermediate prompt"
        );

        let endpoint = MediaEndpoint {
            address: "198.51.100.20".parse().unwrap(),
            rtp_port: 6000,
            rtcp_port: 6001,
            codec: Codec::Pcma,
            packet_ms: 20,
            max_frames_per_packet: 1,
            telephone_event_payload: 0,
        };
        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::OpenOutboundMedia {
                    call_id: CallId(1),
                    source: None,
                    endpoint,
                    codec: Codec::Pcma,
                    packet_ms: 20,
                    max_frames_per_packet: 1,
                    dtmf_mode: DtmfMode::Auto,
                    audio_processing: AudioProcessingPolicy::default(),
                    traffic_class: MediaTrafficClass::default(),
                },
            ))
            .await
            .unwrap();
        let frames =
            read_until_message(&mut phone, &mut decoder, id::START_MEDIA_TRANSMISSION).await;
        let receive = frames
            .iter()
            .position(|frame| frame.message_id == id::OPEN_RECEIVE_CHANNEL)
            .expect("coupled transaction omitted OpenReceiveChannel");
        let transmit = frames
            .iter()
            .position(|frame| frame.message_id == id::START_MEDIA_TRANSMISSION)
            .expect("coupled transaction omitted StartMediaTransmission");
        let first_request_party = coupled_media_request_party(&frames, protocol);
        assert_eq!(transmit, receive + 1);
        assert!(matches!(
            ServerMessage::decode(frames[receive].clone(), protocol).unwrap(),
            ServerMessage::OpenReceiveChannel {
                source_address: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                source_port: 0,
                codec: Codec::Pcma,
                ..
            }
        ));
        assert!(matches!(
            ServerMessage::decode(frames[transmit].clone(), protocol).unwrap(),
            ServerMessage::StartMediaTransmission {
                endpoint: actual,
                ..
            } if actual.address == endpoint.address
                && actual.rtp_port == endpoint.rtp_port
                && actual.codec == endpoint.codec
        ));

        let receive_peer = MediaEndpoint {
            address: "192.0.2.44".parse().unwrap(),
            rtp_port: 4000,
            rtcp_port: 4001,
            codec: Codec::Pcma,
            packet_ms: 20,
            max_frames_per_packet: 1,
            telephone_event_payload: 0,
        };
        phone
            .write_all(
                &ClientMessage::OpenReceiveChannelAck {
                    status: MediaStatus::Ok,
                    address: receive_peer.address,
                    port: receive_peer.rtp_port,
                    call_reference: 1,
                    passthrough_party_id: first_request_party,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent { session_generation: _, device_id: _, event: DeviceEventKind::ReceiveChannelOpened {
                call_id: CallId(1),
                status: MediaStatus::Ok,
                endpoint: actual,
                ..
            } })) if actual == receive_peer
        ));
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent { session_generation: _, device_id: _, event: DeviceEventKind::TransmitChannelImplied {
                call_id: CallId(1),
                endpoint: actual,
                ..
            } })) if actual == endpoint
        ));

        phone
            .write_all(
                &ClientMessage::StartMediaTransmissionAck(MediaTransmissionAck {
                    conference_id: 1,
                    passthrough_party_id: first_request_party,
                    call_reference: 1,
                    status: MediaStatus::Ok,
                    address: endpoint.address,
                    port: endpoint.rtp_port,
                    wire: None,
                })
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), events.recv())
                .await
                .is_err(),
            "late explicit transmit acknowledgement re-settled coupled media"
        );

        handle
            .send(Command::new(
                device_id.clone(),
                CommandAction::OpenOutboundMedia {
                    call_id: CallId(1),
                    source: None,
                    endpoint,
                    codec: Codec::Pcma,
                    packet_ms: 20,
                    max_frames_per_packet: 1,
                    dtmf_mode: DtmfMode::Auto,
                    audio_processing: AudioProcessingPolicy::default(),
                    traffic_class: MediaTrafficClass::default(),
                },
            ))
            .await
            .unwrap();
        let frames =
            read_until_message(&mut phone, &mut decoder, id::START_MEDIA_TRANSMISSION).await;
        let second_request_party = coupled_media_request_party(&frames, protocol);
        assert_ne!(second_request_party, first_request_party);
        phone
            .write_all(
                &ClientMessage::StartMediaTransmissionAck(MediaTransmissionAck {
                    conference_id: 1,
                    passthrough_party_id: first_request_party,
                    call_reference: 1,
                    status: MediaStatus::Ok,
                    address: endpoint.address,
                    port: endpoint.rtp_port,
                    wire: None,
                })
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), events.recv())
                .await
                .is_err(),
            "a prior media generation settled the reopened transmit request"
        );
        phone
            .write_all(
                &ClientMessage::StartMediaTransmissionAck(MediaTransmissionAck {
                    conference_id: 1,
                    passthrough_party_id: second_request_party,
                    call_reference: 1,
                    status: MediaStatus::Ok,
                    address: endpoint.address,
                    port: endpoint.rtp_port,
                    wire: None,
                })
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::TransmitChannelStarted {
                    call_id: CallId(1),
                    status: MediaStatus::Ok,
                    ..
                }
            }))
        ));
        phone
            .write_all(
                &ClientMessage::OpenReceiveChannelAck {
                    status: MediaStatus::Ok,
                    address: receive_peer.address,
                    port: receive_peer.rtp_port,
                    call_reference: 1,
                    passthrough_party_id: second_request_party,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::ReceiveChannelOpened {
                    call_id: CallId(1),
                    status: MediaStatus::Ok,
                    ..
                }
            }))
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), events.recv())
                .await
                .is_err(),
            "receive acknowledgement duplicated an explicitly settled transmit event"
        );

        handle
            .send(Command::new(
                device_id.clone(),
                CommandAction::OpenOutboundMedia {
                    call_id: CallId(1),
                    source: None,
                    endpoint,
                    codec: Codec::Pcma,
                    packet_ms: 20,
                    max_frames_per_packet: 1,
                    dtmf_mode: DtmfMode::Auto,
                    audio_processing: AudioProcessingPolicy::default(),
                    traffic_class: MediaTrafficClass::default(),
                },
            ))
            .await
            .unwrap();
        let frames =
            read_until_message(&mut phone, &mut decoder, id::START_MEDIA_TRANSMISSION).await;
        let third_request_party = coupled_media_request_party(&frames, protocol);
        assert_ne!(third_request_party, second_request_party);
        phone
            .write_all(
                &ClientMessage::OpenReceiveChannelAck {
                    status: MediaStatus::UnspecifiedError,
                    address: receive_peer.address,
                    port: receive_peer.rtp_port,
                    call_reference: 1,
                    passthrough_party_id: third_request_party,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::ReceiveChannelOpened {
                    call_id: CallId(1),
                    status: MediaStatus::UnspecifiedError,
                    ..
                }
            }))
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), events.recv())
                .await
                .is_err(),
            "failed coupled receive emitted a transmit-success event"
        );
        phone
            .write_all(
                &ClientMessage::StartMediaTransmissionAck(MediaTransmissionAck {
                    conference_id: 1,
                    passthrough_party_id: third_request_party,
                    call_reference: 1,
                    status: MediaStatus::Ok,
                    address: endpoint.address,
                    port: endpoint.rtp_port,
                    wire: None,
                })
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), events.recv())
                .await
                .is_err(),
            "late transmit acknowledgement resurrected a failed coupled transaction"
        );

        handle
            .send(Command::new(
                device_id,
                CommandAction::OpenOutboundMedia {
                    call_id: CallId(1),
                    source: None,
                    endpoint,
                    codec: Codec::Pcma,
                    packet_ms: 20,
                    max_frames_per_packet: 1,
                    dtmf_mode: DtmfMode::Auto,
                    audio_processing: AudioProcessingPolicy::default(),
                    traffic_class: MediaTrafficClass::default(),
                },
            ))
            .await
            .unwrap();
        let frames =
            read_until_message(&mut phone, &mut decoder, id::START_MEDIA_TRANSMISSION).await;
        let fourth_request_party = coupled_media_request_party(&frames, protocol);
        assert_ne!(fourth_request_party, third_request_party);
        phone
            .write_all(
                &ClientMessage::StartMediaTransmissionAck(MediaTransmissionAck {
                    conference_id: 1,
                    passthrough_party_id: fourth_request_party,
                    call_reference: 1,
                    status: MediaStatus::UnspecifiedError,
                    address: endpoint.address,
                    port: endpoint.rtp_port,
                    wire: None,
                })
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::TransmitChannelStarted {
                    call_id: CallId(1),
                    status: MediaStatus::UnspecifiedError,
                    ..
                }
            }))
        ));
        phone
            .write_all(
                &ClientMessage::OpenReceiveChannelAck {
                    status: MediaStatus::Ok,
                    address: receive_peer.address,
                    port: receive_peer.rtp_port,
                    call_reference: 1,
                    passthrough_party_id: fourth_request_party,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), events.recv())
                .await
                .is_err(),
            "late receive acknowledgement resurrected a failed coupled transaction"
        );

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn invalid_coupled_media_is_rejected_without_disconnect() {
        let device = definition();
        let device_id = device.id.clone();
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let (server, handle, mut events) = Server::bind(config, [device]).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let mut phone = TcpStream::connect(address).await.unwrap();
        let mut decoder = FrameDecoder::new();
        let protocol = ProtocolVersion::V22;

        phone.write_all(&register_bytes(protocol)).await.unwrap();
        read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Registered(_)
            }))
        ));
        phone
            .write_all(
                &ClientMessage::Stimulus {
                    stimulus: Stimulus::Line,
                    instance: 1,
                    call_reference: 0,
                    status: 0,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, id::SELECT_SOFT_KEYS).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::OffHook { .. }
            }))
        ));

        let endpoint = MediaEndpoint {
            address: "198.51.100.20".parse().unwrap(),
            rtp_port: 6000,
            rtcp_port: 6001,
            codec: Codec::Pcma,
            packet_ms: 20,
            max_frames_per_packet: 1,
            telephone_event_payload: 0,
        };
        assert!(matches!(
            handle
                .send_confirmed(Command::new(device_id.clone(), CommandAction::OpenOutboundMedia {
                    call_id: CallId(1),
                    source: None,
                    endpoint,
                    codec: Codec::Pcma,
                    packet_ms: 20,
                    max_frames_per_packet: 1,
                    dtmf_mode: DtmfMode::Auto,
                    audio_processing: AudioProcessingPolicy::default(),
                    traffic_class: MediaTrafficClass::default(),
                }))
                .await,
            Err(ServerError::CommandWrite(message))
                if message.contains("cannot open coupled outbound media while in state OffHook")
        ));
        assert!(!task.is_finished());

        handle
            .send_confirmed(Command::new(
                device_id,
                CommandAction::SetCallState {
                    call_id: CallId(1),
                    state: CallState::Proceed,
                },
            ))
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::SELECT_SOFT_KEYS).await;
        assert!(frames.iter().any(|frame| matches!(
            ServerMessage::decode(frame.clone(), protocol),
            Ok(ServerMessage::CallState {
                state: CallState::Proceed,
                ..
            })
        )));

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn stale_public_command_does_not_stop_the_listener() {
        let device = definition();
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let (server, handle, mut events) = Server::bind(config, [device]).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());

        handle
            .send(Command::new(
                DeviceId::new("SEPFFFFFFFFFFFF").unwrap(),
                CommandAction::SetMwi {
                    line_instance: LineInstance(1),
                    enabled: true,
                },
            ))
            .await
            .unwrap();
        tokio::task::yield_now().await;
        assert!(!task.is_finished());

        let protocol = ProtocolVersion::V22;
        let mut phone = TcpStream::connect(address).await.unwrap();
        let mut decoder = FrameDecoder::new();
        phone.write_all(&register_bytes(protocol)).await.unwrap();
        read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Registered(_)
            }))
        ));

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn configured_dtmf_mode_selects_rtp_or_signaling_without_duplicate_digits() {
        let device = definition();
        let device_id = device.id.clone();
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let (server, handle, mut events) = Server::bind(config, [device]).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let mut phone = TcpStream::connect(address).await.unwrap();
        let mut decoder = FrameDecoder::new();
        let protocol = ProtocolVersion::V22;

        phone
            .write_all(&register_bytes_with_features(
                protocol,
                PhoneFeatures::RFC2833,
            ))
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Registered(_)
            }))
        ));

        phone
            .write_all(
                &ClientMessage::Stimulus {
                    stimulus: Stimulus::Line,
                    instance: 1,
                    call_reference: 0,
                    status: 0,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, id::SELECT_SOFT_KEYS).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::OffHook {
                    call_id: CallId(1),
                    ..
                }
            }))
        ));

        handle
            .send(Command::new(
                device_id.clone(),
                CommandAction::SetCallState {
                    call_id: CallId(1),
                    state: CallState::Connected,
                },
            ))
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, id::SELECT_SOFT_KEYS).await;
        handle
            .send(Command::new(
                device_id.clone(),
                CommandAction::OpenReceiveChannel {
                    call_id: CallId(1),
                    source: Some(MediaEndpoint {
                        address: "192.0.2.1".parse().unwrap(),
                        rtp_port: 4000,
                        rtcp_port: 4001,
                        codec: Codec::Pcmu,
                        packet_ms: 20,
                        max_frames_per_packet: 1,
                        telephone_event_payload: RFC2833_TELEPHONE_EVENT_PAYLOAD,
                    }),
                    codec: Codec::Pcmu,
                    packet_ms: 20,
                    max_frames_per_packet: 1,
                    dtmf_mode: DtmfMode::Auto,
                    audio_processing: AudioProcessingPolicy {
                        echo_cancellation: crate::EchoCancellation::Off,
                        silence_suppression: crate::SilenceSuppression::On,
                    },
                },
            ))
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::OPEN_RECEIVE_CHANNEL).await;
        assert_eq!(
            frames
                .iter()
                .filter(|frame| frame.message_id == id::SUBSCRIBE_DTMF_PAYLOAD_REQ)
                .count(),
            0,
            "RFC2833 is negotiated in the media messages, not with an unsolicited subscription"
        );
        handle
            .send(Command::new(
                device_id.clone(),
                CommandAction::StopMedia { call_id: CallId(1) },
            ))
            .await
            .unwrap();
        let frame = frames
            .into_iter()
            .find(|frame| frame.message_id == id::OPEN_RECEIVE_CHANNEL)
            .unwrap();
        assert!(matches!(
            ServerMessage::decode(frame, protocol).unwrap(),
            ServerMessage::OpenReceiveChannel {
                echo_cancellation: crate::EchoCancellation::Off,
                telephone_event_payload: RFC2833_TELEPHONE_EVENT_PAYLOAD,
                source_address,
                source_port: 4000,
                ..
            } if source_address == "192.0.2.1".parse::<std::net::IpAddr>().unwrap()
        ));
        handle
            .send(Command::new(
                device_id.clone(),
                CommandAction::StartMedia {
                    call_id: CallId(1),
                    endpoint: MediaEndpoint {
                        address: IpAddr::V4(Ipv4Addr::LOCALHOST),
                        rtp_port: 4000,
                        rtcp_port: 4001,
                        codec: Codec::Pcmu,
                        packet_ms: 20,
                        max_frames_per_packet: 1,
                        telephone_event_payload: 0,
                    },
                    dtmf_mode: DtmfMode::Auto,
                    audio_processing: AudioProcessingPolicy {
                        echo_cancellation: crate::EchoCancellation::Off,
                        silence_suppression: crate::SilenceSuppression::On,
                    },
                    traffic_class: MediaTrafficClass::default(),
                },
            ))
            .await
            .unwrap();
        let frames =
            read_until_message(&mut phone, &mut decoder, id::START_MEDIA_TRANSMISSION).await;
        assert!(
            frames
                .iter()
                .all(|frame| frame.message_id != id::SUBSCRIBE_DTMF_PAYLOAD_REQ),
            "starting the second media direction resubscribed RFC2833"
        );
        let start_media_party = start_media_request_party(&frames, protocol);
        let frame = frames
            .into_iter()
            .find(|frame| frame.message_id == id::START_MEDIA_TRANSMISSION)
            .unwrap();
        assert!(matches!(
            ServerMessage::decode(frame, protocol).unwrap(),
            ServerMessage::StartMediaTransmission {
                silence_suppression: crate::SilenceSuppression::On,
                endpoint: MediaEndpoint {
                    telephone_event_payload: RFC2833_TELEPHONE_EVENT_PAYLOAD,
                    ..
                },
                ..
            }
        ));

        phone
            .write_all(
                &ClientMessage::KeypadButton {
                    button: Digit::Number(4),
                    line_instance: 1,
                    call_reference: 1,
                    wire_layout: None,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Digit {
                    call_id: CallId(1),
                    digit: Digit::Number(4),
                    ..
                }
            }))
        ));

        phone
            .write_all(
                &ClientMessage::StartMediaTransmissionAck(MediaTransmissionAck {
                    conference_id: 99,
                    passthrough_party_id: start_media_party,
                    call_reference: 1,
                    status: MediaStatus::Ok,
                    address: IpAddr::V4(Ipv4Addr::LOCALHOST),
                    port: 4998,
                    wire: None,
                })
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), events.recv())
                .await
                .is_err(),
            "mismatched conference identifier was correlated to a call"
        );

        phone
            .write_all(
                &ClientMessage::StartMediaTransmissionAck(MediaTransmissionAck {
                    conference_id: 1,
                    passthrough_party_id: start_media_party.saturating_add(1),
                    call_reference: 1,
                    status: MediaStatus::Ok,
                    address: IpAddr::V4(Ipv4Addr::LOCALHOST),
                    port: 4999,
                    wire: None,
                })
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), events.recv())
                .await
                .is_err(),
            "mismatched media identifiers were correlated to a call"
        );

        phone
            .write_all(
                &ClientMessage::StartMediaTransmissionAck(MediaTransmissionAck {
                    conference_id: 0,
                    passthrough_party_id: start_media_party,
                    call_reference: 1,
                    status: MediaStatus::Ok,
                    address: "192.168.10.20".parse().unwrap(),
                    port: 4000,
                    wire: None,
                })
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent { session_generation: _, device_id: _, event: DeviceEventKind::TransmitChannelStarted {
                call_id: CallId(1),
                status: MediaStatus::Ok,
                endpoint: MediaEndpoint {
                    address,
                    rtp_port: 4000,
                    telephone_event_payload: RFC2833_TELEPHONE_EVENT_PAYLOAD,
                    ..
                },
                ..
            } })) if address == "192.168.10.20".parse::<IpAddr>().unwrap()
        ));

        phone
            .write_all(
                &ClientMessage::StartMediaTransmissionAck(MediaTransmissionAck {
                    conference_id: 1,
                    passthrough_party_id: start_media_party,
                    call_reference: 1,
                    status: MediaStatus::Ok,
                    address: IpAddr::V4(Ipv4Addr::LOCALHOST),
                    port: 4000,
                    wire: None,
                })
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), events.recv())
                .await
                .is_err(),
            "duplicate transmit acknowledgement emitted a second event"
        );

        let failed_address = "192.168.10.20".parse().unwrap();
        phone
            .write_all(
                &ClientMessage::MediaTransmissionFailure {
                    conference_id: 99,
                    passthrough_party_id: start_media_party,
                    address: failed_address,
                    port: 4000,
                    call_reference: 1,
                    status: MediaStatus::UnspecifiedError,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), events.recv())
                .await
                .is_err(),
            "mismatched conference identifier emitted a media failure"
        );
        let failure = ClientMessage::MediaTransmissionFailure {
            conference_id: 1,
            passthrough_party_id: start_media_party,
            address: failed_address,
            port: 4000,
            call_reference: 1,
            status: MediaStatus::UnspecifiedError,
        };
        phone
            .write_all(&failure.encode(protocol).unwrap())
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent { session_generation: _, device_id: _, event: DeviceEventKind::MediaTransmissionFailed {
                call_id: CallId(1),
                status: MediaStatus::UnspecifiedError,
                endpoint: MediaEndpoint {
                    address,
                    rtp_port: 4000,
                    ..
                },
                ..
            } })) if address == failed_address
        ));
        phone
            .write_all(&failure.encode(protocol).unwrap())
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), events.recv())
                .await
                .is_err(),
            "duplicate media failure emitted a second event"
        );

        let recovery_address = IpAddr::V4(Ipv4Addr::LOCALHOST);
        handle
            .send(Command::new(
                device_id.clone(),
                CommandAction::StartMedia {
                    call_id: CallId(1),
                    endpoint: MediaEndpoint {
                        address: recovery_address,
                        rtp_port: 5000,
                        rtcp_port: 5001,
                        codec: Codec::Pcmu,
                        packet_ms: 20,
                        max_frames_per_packet: 1,
                        telephone_event_payload: 0,
                    },
                    dtmf_mode: DtmfMode::Auto,
                    audio_processing: AudioProcessingPolicy::default(),
                    traffic_class: MediaTrafficClass::default(),
                },
            ))
            .await
            .unwrap();
        let frames =
            read_until_message(&mut phone, &mut decoder, id::START_MEDIA_TRANSMISSION).await;
        let recovery_media_party = start_media_request_party(&frames, protocol);
        assert_ne!(recovery_media_party, start_media_party);
        phone
            .write_all(
                &ClientMessage::StartMediaTransmissionAck(MediaTransmissionAck {
                    conference_id: 1,
                    passthrough_party_id: recovery_media_party,
                    call_reference: 1,
                    status: MediaStatus::Ok,
                    address: recovery_address,
                    port: 5000,
                    wire: None,
                })
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent { session_generation: _, device_id: _, event: DeviceEventKind::TransmitChannelStarted {
                call_id: CallId(1),
                status: MediaStatus::Ok,
                endpoint: MediaEndpoint {
                    address,
                    rtp_port: 5000,
                    ..
                },
                ..
            } })) if address == recovery_address
        ));

        phone
            .write_all(
                &ClientMessage::KeypadButton {
                    button: Digit::Number(5),
                    line_instance: 1,
                    call_reference: 1,
                    wire_layout: None,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), events.recv())
                .await
                .is_err(),
            "acknowledged RTP DTMF also emitted a signaling digit"
        );

        handle
            .send(Command::new(
                device_id.clone(),
                CommandAction::OpenReceiveChannel {
                    call_id: CallId(1),
                    source: Some(MediaEndpoint {
                        address: "192.0.2.1".parse().unwrap(),
                        rtp_port: 4000,
                        rtcp_port: 4001,
                        codec: Codec::Pcmu,
                        packet_ms: 20,
                        max_frames_per_packet: 1,
                        telephone_event_payload: 0,
                    }),
                    codec: Codec::Pcmu,
                    packet_ms: 20,
                    max_frames_per_packet: 1,
                    dtmf_mode: DtmfMode::Skinny,
                    audio_processing: AudioProcessingPolicy::default(),
                },
            ))
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::OPEN_RECEIVE_CHANNEL).await;
        assert_eq!(
            frames
                .iter()
                .filter(|frame| frame.message_id == id::UNSUBSCRIBE_DTMF_PAYLOAD_REQ)
                .count(),
            0,
            "changing one media direction unsubscribed the remaining RFC2833 stream"
        );
        let frame = frames
            .into_iter()
            .find(|frame| frame.message_id == id::OPEN_RECEIVE_CHANNEL)
            .unwrap();
        assert!(matches!(
            ServerMessage::decode(frame, protocol).unwrap(),
            ServerMessage::OpenReceiveChannel {
                telephone_event_payload: 0,
                ..
            }
        ));
        phone
            .write_all(
                &ClientMessage::KeypadButton {
                    button: Digit::Number(6),
                    line_instance: 1,
                    call_reference: 1,
                    wire_layout: None,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), events.recv())
                .await
                .is_err(),
            "the remaining RTP direction also emitted a signaling digit"
        );

        handle
            .send(Command::new(
                device_id.clone(),
                CommandAction::StopMedia { call_id: CallId(1) },
            ))
            .await
            .unwrap();
        let frames =
            read_until_message(&mut phone, &mut decoder, id::STOP_MEDIA_TRANSMISSION).await;
        assert!(
            frames
                .iter()
                .all(|frame| frame.message_id != id::UNSUBSCRIBE_DTMF_PAYLOAD_REQ)
        );
        handle
            .send(Command::new(
                device_id.clone(),
                CommandAction::StopMedia { call_id: CallId(1) },
            ))
            .await
            .unwrap();
        phone
            .write_all(
                &ClientMessage::KeypadButton {
                    button: Digit::Number(7),
                    line_instance: 1,
                    call_reference: 1,
                    wire_layout: None,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Digit {
                    call_id: CallId(1),
                    digit: Digit::Number(7),
                    ..
                }
            }))
        ));

        handle
            .send(Command::new(
                device_id.clone(),
                CommandAction::CloseReceiveChannel { call_id: CallId(1) },
            ))
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::CLOSE_RECEIVE_CHANNEL).await;
        assert_eq!(
            frames
                .iter()
                .filter(|frame| frame.message_id == id::CLOSE_RECEIVE_CHANNEL)
                .count(),
            1
        );
        handle
            .send(Command::new(
                device_id.clone(),
                CommandAction::CloseReceiveChannel { call_id: CallId(1) },
            ))
            .await
            .unwrap();
        handle
            .send(Command::new(
                device_id.clone(),
                CommandAction::CloseCall { call_id: CallId(1) },
            ))
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::SET_RINGER).await;
        assert!(frames.iter().all(|frame| !matches!(
            frame.message_id,
            id::STOP_MEDIA_TRANSMISSION | id::CLOSE_RECEIVE_CHANNEL
        )));

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[test]
    fn handset_acknowledgement_deadlines_are_bounded_ordered_and_exactly_once() {
        let now = Instant::now();
        let mut first = session_call(20);
        first.media.receive.state = MediaChannelState::Opening;
        first.media.receive.deadline = Some(now);
        first.media.transmit.state = MediaChannelState::Opening;
        first.media.transmit.deadline = Some(now + Duration::from_millis(1));
        first.media.coupled_transmit_endpoint = Some(MediaEndpoint {
            address: "198.51.100.20".parse().unwrap(),
            rtp_port: 6000,
            rtcp_port: 6001,
            codec: Codec::Pcmu,
            packet_ms: 20,
            max_frames_per_packet: 1,
            telephone_event_payload: 0,
        });
        let mut second = session_call(10);
        second.media.transmit.state = MediaChannelState::Opening;
        second.media.transmit.deadline = Some(now);
        let mut calls = HashMap::from([(first.call_id, first), (second.call_id, second)]);

        assert_eq!(
            expire_handset_acknowledgements(&mut calls, now),
            [
                (CallId(10), HandsetAcknowledgement::StartMediaTransmission,),
                (CallId(20), HandsetAcknowledgement::OpenReceiveChannel),
            ]
        );
        assert_eq!(
            calls[&CallId(10)].media.transmit.state,
            MediaChannelState::Closed
        );
        assert_eq!(
            calls[&CallId(20)].media.receive.state,
            MediaChannelState::Closed
        );
        assert_eq!(
            calls[&CallId(20)].media.transmit.state,
            MediaChannelState::Closed
        );
        assert!(calls[&CallId(20)].media.coupled_transmit_endpoint.is_none());
        assert!(expire_handset_acknowledgements(&mut calls, now).is_empty());
        assert!(
            expire_handset_acknowledgements(&mut calls, now + Duration::from_millis(1)).is_empty()
        );
        assert!(
            expire_handset_acknowledgements(&mut calls, now + Duration::from_secs(1)).is_empty()
        );
    }

    #[tokio::test]
    async fn unknown_device_type_receives_the_configured_generic_layout() {
        let device = mixed_definition();
        let expected = button_template(&device);
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let (server, handle, _events) = Server::bind(config, [device]).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let mut phone = TcpStream::connect(address).await.unwrap();
        let mut decoder = FrameDecoder::new();

        phone
            .write_all(&register_bytes_for_device_type(
                ProtocolVersion::V22,
                0xffff_fffe,
            ))
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
        phone
            .write_all(
                &ClientMessage::ButtonTemplateRequest
                    .encode(ProtocolVersion::V22)
                    .unwrap(),
            )
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::BUTTON_TEMPLATE).await;
        let frame = frames
            .into_iter()
            .find(|frame| frame.message_id == id::BUTTON_TEMPLATE)
            .unwrap();
        assert_eq!(
            ServerMessage::decode(frame, ProtocolVersion::V22).unwrap(),
            ServerMessage::ButtonTemplate { buttons: expected }
        );

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn on_hook_enbloc_creates_one_addressable_call_before_atomic_routing() {
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let (server, handle, mut events) = Server::bind(config, [definition()]).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let mut phone = TcpStream::connect(address).await.unwrap();
        let mut decoder = FrameDecoder::new();
        let protocol = ProtocolVersion::V22;

        phone.write_all(&register_bytes(protocol)).await.unwrap();
        read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Registered(_)
            }))
        ));

        phone
            .write_all(
                &ClientMessage::EnblocCall {
                    called_party: "8675309".into(),
                    line_instance: 1,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        let initial = read_until_message(&mut phone, &mut decoder, id::SELECT_SOFT_KEYS).await;
        assert!(initial.iter().any(|frame| {
            matches!(
                ServerMessage::decode(frame.clone(), protocol),
                Ok(ServerMessage::CallState {
                    state: CallState::OffHook,
                    ..
                })
            )
        }));
        assert!(
            initial
                .iter()
                .all(|frame| frame.message_id != id::DIALED_NUMBER)
        );

        let call_id = match events.recv().await {
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event:
                    DeviceEventKind::OffHook {
                        call_id,
                        line_instance: LineInstance(1),
                        ..
                    },
            })) => call_id,
            event => {
                panic!("expected addressable off-hook call before en-bloc routing, got {event:?}")
            }
        };
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent { session_generation: _, device_id: _, event: DeviceEventKind::EnblocCall {
                call_id: routed_call_id,
                line_instance: LineInstance(1),
                ref number,
                ..
            } })) if routed_call_id == call_id && number == "8675309"
        ));
        handle
            .send_confirmed(Command::new(
                DeviceId::new("SEP001122334455").unwrap(),
                CommandAction::CommitOutboundCall {
                    call_id,
                    info: CallInfo {
                        direction: crate::CallDirection::Outbound,
                        called_number: "8675309".into(),
                        ..CallInfo::default()
                    },
                },
            ))
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::CALL_STATE).await;
        assert_eq!(
            frames
                .iter()
                .filter(|frame| frame.message_id == id::DIALED_NUMBER)
                .count(),
            1
        );
        assert!(frames.iter().any(|frame| matches!(
            ServerMessage::decode(frame.clone(), protocol),
            Ok(ServerMessage::DialedNumber { ref number, .. }) if number == "8675309"
        )));

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn redial_reuses_the_last_completed_number_on_the_selected_line() {
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let mut device = definition();
        device.soft_keys = profile_with(KeyMode::OnHook, vec![SoftKey::Redial, SoftKey::NewCall]);
        let ButtonDefinition::Line(line) = &mut device.buttons[0] else {
            panic!("test station lost its line button");
        };
        line.initial_tone = Tone::RecallDial;
        let (server, handle, mut events) = Server::bind(config, [device]).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let mut phone = TcpStream::connect(address).await.unwrap();
        let mut decoder = FrameDecoder::new();
        let protocol = ProtocolVersion::V22;

        phone.write_all(&register_bytes(protocol)).await.unwrap();
        read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Registered(_)
            }))
        ));

        phone
            .write_all(
                &ClientMessage::OffHook {
                    line_instance: 1,
                    call_reference: 0,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::SELECT_SOFT_KEYS).await;
        let speaker = frames
            .iter()
            .position(|frame| {
                matches!(
                    ServerMessage::decode(frame.clone(), protocol),
                    Ok(ServerMessage::SetSpeakerMode(SpeakerMode::On))
                )
            })
            .expect("physical OffHook did not enable the speaker");
        let line_lamp = frames
            .iter()
            .position(|frame| {
                matches!(
                    ServerMessage::decode(frame.clone(), protocol),
                    Ok(ServerMessage::SetLamp {
                        stimulus: ButtonType::Line,
                        mode: LampMode::On,
                        ..
                    })
                )
            })
            .expect("physical OffHook did not enable the line lamp");
        let off_hook = frames
            .iter()
            .position(|frame| {
                matches!(
                    ServerMessage::decode(frame.clone(), protocol),
                    Ok(ServerMessage::CallState {
                        state: CallState::OffHook,
                        ..
                    })
                )
            })
            .expect("physical OffHook did not publish OffHook");
        let activate = frames
            .iter()
            .position(|frame| frame.message_id == id::ACTIVATE_CALL_PLANE)
            .expect("physical OffHook did not activate the call plane");
        let prompt = frames
            .iter()
            .position(|frame| {
                matches!(
                    ServerMessage::decode(frame.clone(), protocol),
                    Ok(ServerMessage::DisplayPrompt { ref text, .. }) if text == "Enter number"
                )
            })
            .expect("physical OffHook did not prompt for digits");
        let dial_tone = frames
            .iter()
            .position(|frame| {
                matches!(
                    ServerMessage::decode(frame.clone(), protocol),
                    Ok(ServerMessage::StartTone {
                        tone: Tone::RecallDial,
                        ..
                    })
                )
            })
            .expect("physical OffHook did not start dial tone");
        let soft_keys = frames
            .iter()
            .position(|frame| frame.message_id == id::SELECT_SOFT_KEYS)
            .expect("physical OffHook did not select off-hook keys");
        assert!(
            speaker < line_lamp
                && line_lamp < off_hook
                && off_hook < activate
                && activate < prompt
                && prompt < dial_tone
                && dial_tone < soft_keys
        );
        let call_reference = frames
            .iter()
            .find_map(
                |frame| match ServerMessage::decode(frame.clone(), protocol) {
                    Ok(ServerMessage::CallState { call_reference, .. }) => Some(call_reference),
                    _ => None,
                },
            )
            .unwrap();
        let first_call_id = match events.recv().await {
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::OffHook { call_id, .. },
            })) => call_id,
            event => panic!("unexpected first redial OffHook event: {event:?}"),
        };

        phone
            .write_all(
                &ClientMessage::EnblocCall {
                    called_party: "5551212".into(),
                    line_instance: 1,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent { session_generation: _, device_id: _, event: DeviceEventKind::EnblocCall { ref number, .. } })) if number == "5551212"
        ));
        handle
            .send_confirmed(Command::new(
                DeviceId::new("SEP001122334455").unwrap(),
                CommandAction::CommitOutboundCall {
                    call_id: first_call_id,
                    info: CallInfo {
                        direction: crate::CallDirection::Outbound,
                        called_number: "5551212".into(),
                        ..CallInfo::default()
                    },
                },
            ))
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, id::CALL_STATE).await;

        phone
            .write_all(
                &ClientMessage::OnHook {
                    line_instance: 1,
                    call_reference,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, id::SET_RINGER).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::OnHook { .. }
            }))
        ));

        phone
            .write_all(
                &ClientMessage::SoftKeyEvent {
                    event: SoftKey::Redial.wire_value(),
                    line_instance: 1,
                    call_reference: 0,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        let initial = read_until_message(&mut phone, &mut decoder, id::SELECT_SOFT_KEYS).await;
        let redial_call_reference =
            initial.iter().find_map(
                |frame| match ServerMessage::decode(frame.clone(), protocol) {
                    Ok(ServerMessage::CallState {
                        state: CallState::OffHook,
                        call_reference,
                        ..
                    }) => Some(call_reference),
                    _ => None,
                },
            );
        let redial_call_id = match events.recv().await {
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::OffHook { call_id, .. },
            })) => call_id,
            event => panic!("unexpected redial OffHook event: {event:?}"),
        };
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent { session_generation: _, device_id: _, event: DeviceEventKind::EnblocCall { ref number, .. } })) if number == "5551212"
        ));
        handle
            .send_confirmed(Command::new(
                DeviceId::new("SEP001122334455").unwrap(),
                CommandAction::CommitOutboundCall {
                    call_id: redial_call_id,
                    info: CallInfo {
                        direction: crate::CallDirection::Outbound,
                        called_number: "5551212".into(),
                        ..CallInfo::default()
                    },
                },
            ))
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::CALL_STATE).await;
        assert!(frames.into_iter().any(|frame| matches!(
            ServerMessage::decode(frame, protocol),
            Ok(ServerMessage::DialedNumber { ref number, .. }) if number == "5551212"
        )));

        phone
            .write_all(
                &ClientMessage::OnHook {
                    line_instance: 1,
                    call_reference: redial_call_reference.unwrap(),
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, id::SET_RINGER).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::OnHook { .. }
            }))
        ));

        phone
            .write_all(
                &ClientMessage::Stimulus {
                    stimulus: Stimulus::LastNumberRedial,
                    instance: 1,
                    call_reference: 0,
                    status: 0,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, id::SELECT_SOFT_KEYS).await;
        let stimulus_call_id = match events.recv().await {
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::OffHook { call_id, .. },
            })) => call_id,
            event => panic!("unexpected stimulus-redial OffHook event: {event:?}"),
        };
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent { session_generation: _, device_id: _, event: DeviceEventKind::EnblocCall { ref number, .. } })) if number == "5551212"
        ));
        handle
            .send_confirmed(Command::new(
                DeviceId::new("SEP001122334455").unwrap(),
                CommandAction::CommitOutboundCall {
                    call_id: stimulus_call_id,
                    info: CallInfo {
                        direction: crate::CallDirection::Outbound,
                        called_number: "5551212".into(),
                        ..CallInfo::default()
                    },
                },
            ))
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::CALL_STATE).await;
        assert!(frames.into_iter().any(|frame| matches!(
            ServerMessage::decode(frame, protocol),
            Ok(ServerMessage::DialedNumber { ref number, .. }) if number == "5551212"
        )));

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn configured_redial_menu_uses_typed_native_action_with_legacy_fallback_policy() {
        assert!(!placed_calls_menu_supported(ProtocolVersion::V3));
        assert!(placed_calls_menu_supported(ProtocolVersion::V8));
        assert!(placed_calls_menu_supported(ProtocolVersion::V22));

        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let mut device = definition();
        device.soft_keys = profile_with(KeyMode::OnHook, vec![SoftKey::Redial]);
        device.ui.placed_calls_redial_menu = true;
        let (server, handle, mut events) = Server::bind(config, [device]).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let mut phone = TcpStream::connect(address).await.unwrap();
        let mut decoder = FrameDecoder::new();
        let protocol = ProtocolVersion::V22;

        phone.write_all(&register_bytes(protocol)).await.unwrap();
        read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Registered(_)
            }))
        ));

        phone
            .write_all(
                &ClientMessage::SoftKeyEvent {
                    event: SoftKey::Redial.wire_value(),
                    line_instance: 1,
                    call_reference: 0,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::USER_TO_DEVICE_DATA_V1).await;
        let message = frames
            .into_iter()
            .find_map(|frame| match ServerMessage::decode(frame, protocol) {
                Ok(ServerMessage::UserToDeviceDataV1(message)) => Some(message),
                _ => None,
            })
            .expect("placed-calls execute envelope");
        let document = CiscoIpPhoneExecute::from_xml(&message.data).unwrap();
        assert_eq!(
            document,
            CiscoIpPhoneExecute::new(vec![
                CiscoIpPhoneExecuteItem::new("Application:PlacedCalls").unwrap()
            ])
            .unwrap()
        );
        assert_eq!(message.line_instance, 1);
        assert_eq!(message.call_reference, 0);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), events.recv())
                .await
                .is_err(),
            "opening the native placed-calls menu must not create or route a call"
        );

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn new_call_key_and_stimulus_support_dial_and_backspace() {
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let (server, handle, mut events) = Server::bind(config, [definition()]).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let mut phone = TcpStream::connect(address).await.unwrap();
        let mut decoder = FrameDecoder::new();
        let protocol = ProtocolVersion::V22;

        phone.write_all(&register_bytes(protocol)).await.unwrap();
        read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Registered(_)
            }))
        ));

        phone
            .write_all(
                &ClientMessage::SoftKeyEvent {
                    event: SoftKey::NewCall.wire_value(),
                    line_instance: 1,
                    call_reference: 0,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::SELECT_SOFT_KEYS).await;
        let call_reference = frames
            .into_iter()
            .find_map(|frame| match ServerMessage::decode(frame, protocol) {
                Ok(ServerMessage::CallState { call_reference, .. }) => Some(call_reference),
                _ => None,
            })
            .unwrap();
        let new_call_id = match events.recv().await {
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::OffHook { call_id, .. },
            })) => call_id,
            event => panic!("unexpected new-call event: {event:?}"),
        };
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::SoftKey {
                    call_id: Some(_),
                    soft_key: SoftKey::NewCall,
                    ..
                }
            }))
        ));

        for (index, digit) in [Digit::Number(1), Digit::Number(2)].into_iter().enumerate() {
            phone
                .write_all(
                    &ClientMessage::KeypadButton {
                        button: digit,
                        line_instance: 1,
                        call_reference,
                        wire_layout: None,
                    }
                    .encode(protocol)
                    .unwrap(),
                )
                .await
                .unwrap();
            if index == 0 {
                let frames =
                    read_until_message(&mut phone, &mut decoder, id::SELECT_SOFT_KEYS).await;
                assert!(frames.iter().any(|frame| frame.message_id == id::STOP_TONE));
                assert!(
                    frames
                        .iter()
                        .all(|frame| frame.message_id != id::DIALED_NUMBER)
                );
            }
            assert!(matches!(
                events.recv().await,
                Some(Event::Device(DeviceEvent {
                    session_generation: _,
                    device_id: _,
                    event: DeviceEventKind::Digit { .. }
                }))
            ));
            if index == 1 {
                assert!(
                    tokio::time::timeout(Duration::from_millis(50), phone.read_u8())
                        .await
                        .is_err(),
                    "a repeated digit emitted redundant station UI"
                );
            }
        }

        phone
            .write_all(
                &ClientMessage::SoftKeyEvent {
                    event: SoftKey::Backspace.wire_value(),
                    line_instance: 1,
                    call_reference,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::BACKSPACE_RESPONSE).await;
        assert!(
            frames
                .iter()
                .all(|frame| frame.message_id != id::DIALED_NUMBER)
        );
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::SoftKey {
                    soft_key: SoftKey::Backspace,
                    ..
                }
            }))
        ));

        phone
            .write_all(
                &ClientMessage::SoftKeyEvent {
                    event: SoftKey::Dial.wire_value(),
                    line_instance: 1,
                    call_reference,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::SoftKey {
                    soft_key: SoftKey::Dial,
                    ..
                }
            }))
        ));

        handle
            .send(Command::new(
                DeviceId::new("SEP001122334455").unwrap(),
                CommandAction::CommitOutboundCall {
                    call_id: new_call_id,
                    info: CallInfo {
                        direction: crate::CallDirection::Outbound,
                        called_number: "1".into(),
                        ..CallInfo::default()
                    },
                },
            ))
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::CALL_STATE).await;
        let stop_tone = frames
            .iter()
            .position(|frame| frame.message_id == id::STOP_TONE)
            .expect("dial commit did not stop tone");
        let dialed_number = frames
            .iter()
            .position(|frame| {
                matches!(
                    ServerMessage::decode(frame.clone(), protocol),
                    Ok(ServerMessage::DialedNumber { ref number, .. }) if number == "1"
                )
            })
            .expect("dial commit did not publish the complete number");
        let proceed = frames
            .iter()
            .position(|frame| {
                matches!(
                    ServerMessage::decode(frame.clone(), protocol),
                    Ok(ServerMessage::CallState {
                        state: CallState::Proceed,
                        ..
                    })
                )
            })
            .expect("dial commit did not publish Proceed");
        assert!(stop_tone < dialed_number && dialed_number < proceed);
        assert_eq!(
            frames
                .iter()
                .filter(|frame| frame.message_id == id::DIALED_NUMBER)
                .count(),
            1
        );

        phone
            .write_all(
                &ClientMessage::OnHook {
                    line_instance: 1,
                    call_reference,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, id::SET_LAMP).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::OnHook { .. }
            }))
        ));

        phone
            .write_all(
                &ClientMessage::Stimulus {
                    stimulus: Stimulus::NewCall,
                    instance: 1,
                    call_reference: 0,
                    status: 0,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, id::SELECT_SOFT_KEYS).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::OffHook { .. }
            }))
        ));
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::SoftKey {
                    call_id: Some(_),
                    soft_key: SoftKey::NewCall,
                    ..
                }
            }))
        ));

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn pickup_key_and_stimulus_create_an_addressable_call_before_dispatch() {
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let mut device = definition();
        device.soft_keys =
            profile_with(KeyMode::OnHook, vec![SoftKey::Pickup, SoftKey::GroupPickup]);
        let (server, handle, mut events) = Server::bind(config, [device]).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let mut phone = TcpStream::connect(address).await.unwrap();
        let mut decoder = FrameDecoder::new();
        let protocol = ProtocolVersion::V22;

        phone.write_all(&register_bytes(protocol)).await.unwrap();
        read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Registered(_)
            }))
        ));

        phone
            .write_all(
                &ClientMessage::SoftKeyEvent {
                    event: SoftKey::Pickup.wire_value(),
                    line_instance: 1,
                    call_reference: 0,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::SELECT_SOFT_KEYS).await;
        let call_reference = frames
            .into_iter()
            .find_map(|frame| match ServerMessage::decode(frame, protocol) {
                Ok(ServerMessage::CallState { call_reference, .. }) => Some(call_reference),
                _ => None,
            })
            .unwrap();
        let call_id = match events.recv().await {
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::OffHook { call_id, .. },
            })) => call_id,
            event => panic!("expected pickup OffHook event, got {event:?}"),
        };
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent { session_generation: _, device_id: _, event: DeviceEventKind::SoftKey {
                call_id: Some(event_call_id),
                soft_key: SoftKey::Pickup,
                ..
            } })) if event_call_id == call_id
        ));

        phone
            .write_all(
                &ClientMessage::OnHook {
                    line_instance: 1,
                    call_reference,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, id::SET_LAMP).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::OnHook { .. }
            }))
        ));

        phone
            .write_all(
                &ClientMessage::Stimulus {
                    stimulus: Stimulus::GroupCallPickup,
                    instance: 1,
                    call_reference: 0,
                    status: 0,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, id::SELECT_SOFT_KEYS).await;
        let call_id = match events.recv().await {
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::OffHook { call_id, .. },
            })) => call_id,
            event => panic!("expected group-pickup OffHook event, got {event:?}"),
        };
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent { session_generation: _, device_id: _, event: DeviceEventKind::SoftKey {
                call_id: Some(event_call_id),
                soft_key: SoftKey::GroupPickup,
                ..
            } })) if event_call_id == call_id
        ));

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn configured_voicemail_button_creates_an_exact_line_call_before_routing() {
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let mut device = definition();
        device
            .buttons
            .push(ButtonDefinition::Feature(FeatureDefinition {
                instance: 1,
                label: "Messages".into(),
                feature: ButtonType::Voicemail,
            }));
        let (server, handle, mut events) = Server::bind(config, [device]).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let mut phone = TcpStream::connect(address).await.unwrap();
        let mut decoder = FrameDecoder::new();
        let protocol = ProtocolVersion::V22;

        phone.write_all(&register_bytes(protocol)).await.unwrap();
        read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Registered(_)
            }))
        ));
        phone
            .write_all(
                &ClientMessage::Stimulus {
                    stimulus: Stimulus::Voicemail,
                    instance: 1,
                    call_reference: 0,
                    status: 0,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, id::SELECT_SOFT_KEYS).await;
        let call_id = match events.recv().await {
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event:
                    DeviceEventKind::OffHook {
                        call_id,
                        line_instance: LineInstance(1),
                        ..
                    },
            })) => call_id,
            event => panic!("expected voicemail OffHook event, got {event:?}"),
        };
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent { session_generation: _, device_id: _, event: DeviceEventKind::VoicemailButton {
                call_id: routed_call,
                line_instance: LineInstance(1),
                ..
            } })) if routed_call == call_id
        ));

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn meetme_key_and_stimulus_reserve_a_distinct_addressable_call() {
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let mut device = definition();
        device.soft_keys = SoftKeyProfile::new(
            KeyMode::ALL_KNOWN
                .iter()
                .copied()
                .map(|mode| (mode, vec![SoftKey::MeetMe])),
        )
        .unwrap();
        let (server, handle, mut events) = Server::bind(config, [device]).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let mut phone = TcpStream::connect(address).await.unwrap();
        let mut decoder = FrameDecoder::new();
        let protocol = ProtocolVersion::V22;

        phone.write_all(&register_bytes(protocol)).await.unwrap();
        read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Registered(_)
            }))
        ));

        phone
            .write_all(
                &ClientMessage::SoftKeyEvent {
                    event: SoftKey::MeetMe.wire_value(),
                    line_instance: 1,
                    call_reference: 0,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::SELECT_SOFT_KEYS).await;
        let first_reference = frames
            .into_iter()
            .find_map(|frame| match ServerMessage::decode(frame, protocol) {
                Ok(ServerMessage::CallState { call_reference, .. }) => Some(call_reference),
                _ => None,
            })
            .unwrap();
        let first_call = match events.recv().await {
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::OffHook { call_id, .. },
            })) => call_id,
            event => panic!("expected conference-destination OffHook event, got {event:?}"),
        };
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent { session_generation: _, device_id: _, event: DeviceEventKind::SoftKey {
                call_id: Some(call_id),
                soft_key: SoftKey::MeetMe,
                ..
            } })) if call_id == first_call
        ));

        handle
            .send(Command::new(
                DeviceId::new("SEP001122334455").unwrap(),
                CommandAction::SetCallState {
                    call_id: first_call,
                    state: CallState::Connected,
                },
            ))
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, id::SELECT_SOFT_KEYS).await;
        phone
            .write_all(
                &ClientMessage::Stimulus {
                    stimulus: Stimulus::MeetMeConference,
                    instance: 1,
                    call_reference: first_reference,
                    status: 0,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, id::SELECT_SOFT_KEYS).await;
        let second_call = match events.recv().await {
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::OffHook { call_id, .. },
            })) => call_id,
            event => panic!("expected a new conference-destination OffHook event, got {event:?}"),
        };
        assert_ne!(second_call, first_call);
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent { session_generation: _, device_id: _, event: DeviceEventKind::SoftKey {
                call_id: Some(call_id),
                soft_key: SoftKey::MeetMe,
                ..
            } })) if call_id == second_call
        ));

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn registered_handset_routes_every_configured_conference_control_with_exact_call() {
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let conference_keys = vec![
            SoftKey::Conference,
            SoftKey::Join,
            SoftKey::ConferenceList,
            SoftKey::Select,
            SoftKey::Hold,
            SoftKey::Resume,
            SoftKey::EndCall,
        ];
        let mut device = definition();
        device.soft_keys = SoftKeyProfile::new(
            KeyMode::ALL_KNOWN
                .iter()
                .copied()
                .map(|mode| (mode, conference_keys.clone())),
        )
        .unwrap();
        let (server, handle, mut events) = Server::bind(config, [device]).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let mut phone = TcpStream::connect(address).await.unwrap();
        let mut decoder = FrameDecoder::new();
        let protocol = ProtocolVersion::V22;
        let device_id = DeviceId::new("SEP001122334455").unwrap();
        let call_id = CallId(7001);

        phone.write_all(&register_bytes(protocol)).await.unwrap();
        read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Registered(_)
            }))
        ));
        handle
            .send(Command::new(
                device_id.clone(),
                CommandAction::BeginCall {
                    line_instance: LineInstance(1),
                    call_id,
                    codec: Codec::Pcma,
                },
            ))
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, id::SELECT_SOFT_KEYS).await;
        handle
            .send(Command::new(
                device_id.clone(),
                CommandAction::SetCallState {
                    call_id,
                    state: CallState::Connected,
                },
            ))
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, id::SELECT_SOFT_KEYS).await;

        for soft_key in conference_keys {
            phone
                .write_all(
                    &ClientMessage::SoftKeyEvent {
                        event: soft_key.wire_value(),
                        line_instance: 1,
                        call_reference: 7001,
                    }
                    .encode(protocol)
                    .unwrap(),
                )
                .await
                .unwrap();
            assert!(matches!(
                events.recv().await,
                Some(Event::Device(DeviceEvent { session_generation: _, device_id: actual_device, event: DeviceEventKind::SoftKey {
                    line_instance: LineInstance(1),
                    call_id: Some(actual_call),
                    soft_key: actual_key,
                } })) if actual_device == device_id
                    && actual_call == call_id
                    && actual_key == soft_key
            ));
        }

        for (stimulus, soft_key) in [
            (Stimulus::Conference, SoftKey::Conference),
            (Stimulus::ConferenceList, SoftKey::ConferenceList),
        ] {
            phone
                .write_all(
                    &ClientMessage::Stimulus {
                        stimulus,
                        instance: 1,
                        call_reference: 7001,
                        status: 0,
                    }
                    .encode(protocol)
                    .unwrap(),
                )
                .await
                .unwrap();
            assert!(matches!(
                events.recv().await,
                Some(Event::Device(DeviceEvent { session_generation: _, device_id: actual_device, event: DeviceEventKind::SoftKey {
                    line_instance: LineInstance(1),
                    call_id: Some(actual_call),
                    soft_key: actual_key,
                } })) if actual_device == device_id
                    && actual_call == call_id
                    && actual_key == soft_key
            ));
        }

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn legacy_phone_receives_static_button_status_layouts() {
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let (server, handle, _events) = Server::bind(config, [mixed_definition()]).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let mut phone = TcpStream::connect(address).await.unwrap();
        let mut decoder = FrameDecoder::new();

        phone
            .write_all(&register_bytes(ProtocolVersion::V3))
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
        let requests = [
            ClientMessage::SpeedDialStatusRequest {
                speed_dial_instance: 1,
            }
            .encode(ProtocolVersion::V3)
            .unwrap(),
            ClientMessage::FeatureStatusRequest {
                index: 1,
                capabilities: 0,
            }
            .encode(ProtocolVersion::V3)
            .unwrap(),
            ClientMessage::ServiceUrlStatusRequest { index: 1 }
                .encode(ProtocolVersion::V3)
                .unwrap(),
        ]
        .concat();
        phone.write_all(&requests).await.unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::SERVICE_URL_STAT).await;

        for (message_id, expected) in [
            (
                id::SPEED_DIAL_STAT,
                ServerMessage::SpeedDialStatus {
                    instance: 1,
                    number: "2001".into(),
                    display_name: "Reception".into(),
                },
            ),
            (
                id::FEATURE_STAT,
                ServerMessage::FeatureStatus {
                    instance: 1,
                    button_type: ButtonType::DoNotDisturb,
                    label: "DND".into(),
                    state: 0,
                },
            ),
            (
                id::SERVICE_URL_STAT,
                ServerMessage::ServiceUrlStatus {
                    index: 1,
                    url: "http://services.invalid/directory".into(),
                    label: "Directory".into(),
                    extension_text: String::new(),
                },
            ),
        ] {
            let frame = frames
                .iter()
                .find(|frame| frame.message_id == message_id)
                .cloned()
                .unwrap();
            assert_eq!(
                ServerMessage::decode(frame, ProtocolVersion::V3).unwrap(),
                expected
            );
        }

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    fn register_bytes(protocol: ProtocolVersion) -> Vec<u8> {
        register_bytes_for_device_type(protocol, 115)
    }

    fn register_bytes_with_features(protocol: ProtocolVersion, features: PhoneFeatures) -> Vec<u8> {
        register_bytes_for_device_with_features(protocol, 115, "SEP001122334455", features)
    }

    fn register_bytes_for_device_type(protocol: ProtocolVersion, device_type: u32) -> Vec<u8> {
        register_bytes_for_device(protocol, device_type, "SEP001122334455")
    }

    fn register_bytes_for_device(
        protocol: ProtocolVersion,
        device_type: u32,
        device_id: &str,
    ) -> Vec<u8> {
        register_bytes_for_device_with_features(
            protocol,
            device_type,
            device_id,
            PhoneFeatures::empty(),
        )
    }

    fn register_bytes_for_device_with_features(
        protocol: ProtocolVersion,
        device_type: u32,
        device_id: &str,
        features: PhoneFeatures,
    ) -> Vec<u8> {
        let mut payload = vec![0_u8; 124];
        let device_id = device_id.as_bytes();
        assert!(device_id.len() <= 16);
        payload[..device_id.len()].copy_from_slice(device_id);
        payload[24..28].copy_from_slice(&[127, 0, 0, 1]);
        payload[28..32].copy_from_slice(&device_type.to_le_bytes());
        payload[40..44].copy_from_slice(&(protocol.wire() | features.bits()).to_le_bytes());
        payload[92..101].copy_from_slice(b"SCCP42.9-");
        Frame::new(0, id::REGISTER, payload).encode().unwrap()
    }

    fn capability_update_bytes(
        protocol: ProtocolVersion,
        audio_codec: Codec,
        video_codec: Codec,
        marker: u32,
    ) -> Vec<u8> {
        const AUDIO_OFFSET: usize = 312;
        const VIDEO_OFFSET: usize = 600;

        fn put(payload: &mut [u8], offset: usize, value: u32) {
            payload[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }

        let mut payload = vec![0; 2_380];
        put(&mut payload, 0, 1);
        put(&mut payload, 4, 1);
        put(&mut payload, AUDIO_OFFSET, audio_codec.wire_value());
        put(&mut payload, AUDIO_OFFSET + 4, marker);
        payload[AUDIO_OFFSET + 8..AUDIO_OFFSET + 16]
            .copy_from_slice(&marker.to_le_bytes().repeat(2));

        put(&mut payload, VIDEO_OFFSET, video_codec.wire_value());
        put(
            &mut payload,
            VIDEO_OFFSET + 4,
            (ReceiveTransmit::RECEIVE | ReceiveTransmit::TRANSMIT).bits(),
        );
        put(&mut payload, VIDEO_OFFSET + 8, 1);
        for (index, value) in [marker, 5, 4_000, 128, 2, 7].into_iter().enumerate() {
            put(&mut payload, VIDEO_OFFSET + 12 + index * 4, value);
        }
        put(
            &mut payload,
            VIDEO_OFFSET + 108,
            u32::from(EncryptionCapability::Capable),
        );
        for (index, value) in [66, 31, 120, 240, 360, marker].into_iter().enumerate() {
            put(&mut payload, VIDEO_OFFSET + 112 + index * 4, value);
        }
        put(
            &mut payload,
            VIDEO_OFFSET + 136,
            u32::from(IpAddressType::Ipv4AndIpv6),
        );
        Frame::new(protocol.wire(), id::UPDATE_CAPABILITIES_V3, payload)
            .encode()
            .unwrap()
    }

    #[tokio::test]
    async fn capability_snapshots_replace_atomically_and_remain_session_scoped() {
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let (server, handle, mut events) = Server::bind(config, [definition()]).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let protocol = ProtocolVersion::V22;

        let mut first_phone = TcpStream::connect(address).await.unwrap();
        let mut first_decoder = FrameDecoder::new();
        first_phone
            .write_all(&register_bytes(protocol))
            .await
            .unwrap();
        read_until_message(&mut first_phone, &mut first_decoder, id::CAPABILITIES_REQ).await;
        let first_generation = match events.recv().await {
            Some(Event::Device(DeviceEvent {
                session_generation,
                event: DeviceEventKind::Registered(_),
                ..
            })) => session_generation,
            event => panic!("expected first registration, got {event:?}"),
        };

        first_phone
            .write_all(&capability_update_bytes(
                protocol,
                Codec::Pcmu,
                Codec::H264,
                11,
            ))
            .await
            .unwrap();
        let first_capabilities = match events.recv().await {
            Some(Event::Device(DeviceEvent {
                session_generation,
                event: DeviceEventKind::Capabilities { capabilities },
                ..
            })) => {
                assert_eq!(session_generation, first_generation);
                capabilities
            }
            event => panic!("expected first capability update, got {event:?}"),
        };
        assert_eq!(first_capabilities.audio()[0].codec, Codec::Pcmu);
        assert_eq!(first_capabilities.video()[0].codec, Codec::H264);
        assert_eq!(
            first_capabilities.video()[0].direction,
            ReceiveTransmit::RECEIVE | ReceiveTransmit::TRANSMIT
        );
        assert_eq!(
            first_capabilities.video()[0].encryption_capability,
            Some(EncryptionCapability::Capable)
        );
        assert_eq!(
            first_capabilities.video()[0].address_type,
            Some(IpAddressType::Ipv4AndIpv6)
        );
        assert_eq!(first_capabilities.video()[0].codec_parameters[5], 11);

        first_phone
            .write_all(&capability_update_bytes(
                protocol,
                Codec::G72264k,
                Codec::H263,
                22,
            ))
            .await
            .unwrap();
        let replacement_capabilities = match events.recv().await {
            Some(Event::Device(DeviceEvent {
                session_generation,
                event: DeviceEventKind::Capabilities { capabilities },
                ..
            })) => {
                assert_eq!(session_generation, first_generation);
                capabilities
            }
            event => panic!("expected replacement capability update, got {event:?}"),
        };
        assert_eq!(replacement_capabilities.audio().len(), 1);
        assert_eq!(replacement_capabilities.audio()[0].codec, Codec::G72264k);
        assert_eq!(replacement_capabilities.video().len(), 1);
        assert_eq!(replacement_capabilities.video()[0].codec, Codec::H263);
        assert_eq!(replacement_capabilities.video()[0].codec_parameters[5], 22);
        assert_eq!(first_capabilities.video()[0].codec, Codec::H264);

        let mut second_phone = TcpStream::connect(address).await.unwrap();
        let mut second_decoder = FrameDecoder::new();
        second_phone
            .write_all(&register_bytes(protocol))
            .await
            .unwrap();
        read_until_message(&mut second_phone, &mut second_decoder, id::CAPABILITIES_REQ).await;
        let second_generation = match events.recv().await {
            Some(Event::Device(DeviceEvent {
                session_generation,
                event: DeviceEventKind::Registered(_),
                ..
            })) => session_generation,
            event => panic!("expected replacement registration, got {event:?}"),
        };
        assert!(second_generation > first_generation);
        assert!(
            tokio::time::timeout(Duration::from_millis(25), events.recv())
                .await
                .is_err(),
            "replaced session emitted a late disconnect"
        );

        second_phone
            .write_all(
                &ClientMessage::CapabilitiesResponse(vec![MediaCapability {
                    codec: Codec::Pcma,
                    max_frames_per_packet: 2,
                    codec_parameters: [0; 8],
                }])
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        match events.recv().await {
            Some(Event::Device(DeviceEvent {
                session_generation,
                event: DeviceEventKind::Capabilities { capabilities },
                ..
            })) => {
                assert_eq!(session_generation, second_generation);
                assert_eq!(capabilities.audio()[0].codec, Codec::Pcma);
                assert!(capabilities.video().is_empty());
            }
            event => panic!("expected reconnect capability response, got {event:?}"),
        }
        assert_ne!(first_generation, second_generation);

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    async fn read_until_message(
        phone: &mut dyn StationIo,
        decoder: &mut FrameDecoder,
        message_id: u32,
    ) -> Vec<Frame> {
        let mut frames = Vec::new();
        let mut buffer = [0_u8; 2048];
        while !frames
            .iter()
            .any(|frame: &Frame| frame.message_id == message_id)
        {
            let count = tokio::time::timeout(Duration::from_secs(1), phone.read(&mut buffer))
                .await
                .expect("timed out waiting for SCCP response")
                .expect("could not read SCCP response");
            assert_ne!(count, 0, "SCCP session closed while waiting for response");
            frames.extend(decoder.push(&buffer[..count]).unwrap());
        }
        frames
    }

    async fn read_until_server_message(
        phone: &mut dyn StationIo,
        decoder: &mut FrameDecoder,
        protocol: ProtocolVersion,
        predicate: impl Fn(&ServerMessage) -> bool,
    ) -> Vec<ServerMessage> {
        let mut messages = Vec::new();
        let mut buffer = [0_u8; 2048];
        while !messages.iter().any(&predicate) {
            let count = tokio::time::timeout(Duration::from_secs(1), phone.read(&mut buffer))
                .await
                .expect("timed out waiting for SCCP response")
                .expect("could not read SCCP response");
            assert_ne!(count, 0, "SCCP session closed while waiting for response");
            messages.extend(
                decoder
                    .push(&buffer[..count])
                    .unwrap()
                    .into_iter()
                    .map(|frame| ServerMessage::decode(frame, protocol).unwrap()),
            );
        }
        messages
    }

    fn open_receive_request_party(frames: &[Frame], protocol: ProtocolVersion) -> u32 {
        frames
            .iter()
            .find_map(
                |frame| match ServerMessage::decode(frame.clone(), protocol).ok()? {
                    ServerMessage::OpenReceiveChannel {
                        passthrough_party_id,
                        ..
                    } => Some(passthrough_party_id),
                    _ => None,
                },
            )
            .expect("transaction omitted OpenReceiveChannel")
    }

    fn start_media_request_party(frames: &[Frame], protocol: ProtocolVersion) -> u32 {
        frames
            .iter()
            .find_map(
                |frame| match ServerMessage::decode(frame.clone(), protocol).ok()? {
                    ServerMessage::StartMediaTransmission {
                        passthrough_party_id,
                        ..
                    } => Some(passthrough_party_id),
                    _ => None,
                },
            )
            .expect("transaction omitted StartMediaTransmission")
    }

    fn coupled_media_request_party(frames: &[Frame], protocol: ProtocolVersion) -> u32 {
        let receive = open_receive_request_party(frames, protocol);
        let transmit = start_media_request_party(frames, protocol);
        assert_ne!(receive, 0);
        assert_eq!(receive, transmit, "coupled request identities diverged");
        receive
    }

    fn test_connection_statistics(
        directory_number: &str,
        call_reference: u32,
    ) -> ConnectionStatistics {
        ConnectionStatistics {
            directory_number: directory_number.into(),
            call_reference,
            processing: StatisticsProcessing::Clear,
            packets_sent: 120,
            octets_sent: 9_600,
            packets_received: 118,
            octets_received: 9_440,
            packets_lost: 2,
            jitter_millis: 6,
            latency_millis: 17,
            quality: crate::ConnectionQualityStatistics::new(b"MLQK=4.4".to_vec()).unwrap(),
        }
    }

    #[tokio::test]
    async fn hangup_statistics_are_exactly_correlated_retained_and_not_replayed() {
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let (server, handle, mut events) = Server::bind(config, [definition()]).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let mut phone = TcpStream::connect(address).await.unwrap();
        let mut decoder = FrameDecoder::new();
        let protocol = ProtocolVersion::V22;
        let device_id = DeviceId::new("SEP001122334455").unwrap();
        let call_id = CallId(7001);

        phone.write_all(&register_bytes(protocol)).await.unwrap();
        read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Registered(_)
            }))
        ));
        handle
            .send(Command::new(
                device_id.clone(),
                CommandAction::BeginCall {
                    line_instance: LineInstance(1),
                    call_id,
                    codec: Codec::Pcma,
                },
            ))
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, id::CALL_STATE).await;
        handle
            .send(Command::new(
                device_id.clone(),
                CommandAction::SetCallInfo {
                    call_id,
                    info: CallInfo {
                        direction: crate::types::CallDirection::Outbound,
                        called_number: "2002".into(),
                        ..CallInfo::default()
                    },
                },
            ))
            .await
            .unwrap();
        handle
            .send(Command::new(
                device_id.clone(),
                CommandAction::OpenReceiveChannel {
                    call_id,
                    source: Some(MediaEndpoint {
                        address: "192.0.2.1".parse().unwrap(),
                        rtp_port: 5000,
                        rtcp_port: 5001,
                        codec: Codec::Pcma,
                        packet_ms: 30,
                        max_frames_per_packet: 2,
                        telephone_event_payload: 0,
                    }),
                    codec: Codec::Pcma,
                    packet_ms: 30,
                    max_frames_per_packet: 2,
                    dtmf_mode: DtmfMode::Skinny,
                    audio_processing: AudioProcessingPolicy::default(),
                },
            ))
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::OPEN_RECEIVE_CHANNEL).await;
        let receive_media_party = open_receive_request_party(&frames, protocol);
        let receive_peer = MediaEndpoint {
            address: "192.0.2.10".parse().unwrap(),
            rtp_port: 4000,
            rtcp_port: 4001,
            codec: Codec::Pcma,
            packet_ms: 30,
            max_frames_per_packet: 2,
            telephone_event_payload: 0,
        };
        phone
            .write_all(
                &ClientMessage::OpenReceiveChannelAck {
                    status: MediaStatus::Ok,
                    address: receive_peer.address,
                    port: receive_peer.rtp_port,
                    call_reference: 7001,
                    passthrough_party_id: receive_media_party,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent { session_generation: _, device_id: _, event: DeviceEventKind::ReceiveChannelOpened { endpoint, .. } })) if endpoint == receive_peer
        ));
        handle
            .send(Command::new(
                device_id.clone(),
                CommandAction::StartMedia {
                    call_id,
                    endpoint: MediaEndpoint {
                        address: "198.51.100.20".parse().unwrap(),
                        rtp_port: 6000,
                        rtcp_port: 6001,
                        codec: Codec::Pcma,
                        packet_ms: 30,
                        max_frames_per_packet: 2,
                        telephone_event_payload: 0,
                    },
                    dtmf_mode: DtmfMode::Skinny,
                    audio_processing: AudioProcessingPolicy::default(),
                    traffic_class: MediaTrafficClass::default(),
                },
            ))
            .await
            .unwrap();
        let frames =
            read_until_message(&mut phone, &mut decoder, id::START_MEDIA_TRANSMISSION).await;
        let transmit_media_party = start_media_request_party(&frames, protocol);
        let transmit_peer = MediaEndpoint {
            address: "2001:db8::20".parse().unwrap(),
            rtp_port: 5000,
            rtcp_port: 5001,
            codec: Codec::Pcma,
            packet_ms: 30,
            max_frames_per_packet: 2,
            telephone_event_payload: 0,
        };
        phone
            .write_all(
                &ClientMessage::StartMediaTransmissionAck(MediaTransmissionAck {
                    conference_id: 7001,
                    passthrough_party_id: transmit_media_party,
                    call_reference: 7001,
                    status: MediaStatus::Ok,
                    address: transmit_peer.address,
                    port: transmit_peer.rtp_port,
                    wire: None,
                })
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent { session_generation: _, device_id: _, event: DeviceEventKind::TransmitChannelStarted { endpoint, .. } })) if endpoint == transmit_peer
        ));
        handle
            .send(Command::new(
                device_id.clone(),
                CommandAction::CloseReceiveChannel { call_id },
            ))
            .await
            .unwrap();
        handle
            .send(Command::new(
                device_id.clone(),
                CommandAction::StopMedia { call_id },
            ))
            .await
            .unwrap();
        handle
            .send(Command::new(
                device_id.clone(),
                CommandAction::CloseCall { call_id },
            ))
            .await
            .unwrap();
        let frames =
            read_until_message(&mut phone, &mut decoder, id::CONNECTION_STATISTICS_REQ).await;
        let trailing = if frames
            .iter()
            .any(|frame| frame.message_id == id::SET_RINGER)
        {
            Vec::new()
        } else {
            read_until_message(&mut phone, &mut decoder, id::SET_RINGER).await
        };
        assert_eq!(
            frames
                .iter()
                .chain(&trailing)
                .filter(|frame| frame.message_id == id::STOP_MEDIA_TRANSMISSION)
                .count(),
            1
        );
        assert_eq!(
            frames
                .iter()
                .chain(&trailing)
                .filter(|frame| frame.message_id == id::CLOSE_RECEIVE_CHANNEL)
                .count(),
            1
        );
        let close_receive = frames
            .iter()
            .position(|frame| frame.message_id == id::CLOSE_RECEIVE_CHANNEL)
            .expect("hangup did not close receive media");
        let stop_media = frames
            .iter()
            .position(|frame| frame.message_id == id::STOP_MEDIA_TRANSMISSION)
            .expect("hangup did not stop transmit media");
        let on_hook = frames
            .iter()
            .position(|frame| {
                matches!(
                    ServerMessage::decode(frame.clone(), protocol),
                    Ok(ServerMessage::CallState {
                        state: CallState::OnHook,
                        ..
                    })
                )
            })
            .expect("hangup did not publish OnHook");
        let statistics = frames
            .iter()
            .position(|frame| frame.message_id == id::CONNECTION_STATISTICS_REQ)
            .expect("hangup did not request connection statistics");
        assert!(close_receive < stop_media && stop_media < on_hook && on_hook < statistics);
        assert!(
            frames
                .iter()
                .all(|frame| frame.message_id != id::CALL_HISTORY_DISPOSITION)
        );
        assert!(frames.iter().any(|frame| matches!(
            ServerMessage::decode(frame.clone(), protocol),
            Ok(ServerMessage::ConnectionStatisticsRequest {
                directory_number,
                call_reference: 7001,
                processing: StatisticsProcessing::Clear,
            }) if directory_number == "2002"
        )));

        phone
            .write_all(
                &ClientMessage::ConnectionStatisticsResponse(test_connection_statistics(
                    "wrong", 7001,
                ))
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(25), events.recv())
                .await
                .is_err()
        );

        let mut unknown_processing = test_connection_statistics("2002", 7001);
        unknown_processing.processing = StatisticsProcessing::Unknown(9);
        phone
            .write_all(
                &ClientMessage::ConnectionStatisticsResponse(unknown_processing)
                    .encode(protocol)
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(25), events.recv())
                .await
                .is_err()
        );

        let expected = test_connection_statistics("2002", 7001);
        phone
            .write_all(
                &ClientMessage::ConnectionStatisticsResponse(expected.clone())
                    .encode(protocol)
                    .unwrap(),
            )
            .await
            .unwrap();
        let Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: actual_device,
            event: DeviceEventKind::ConnectionStatisticsCollected { snapshot },
        })) = events.recv().await
        else {
            panic!("expected a correlated statistics event");
        };
        assert_eq!(actual_device, device_id);
        assert_eq!(snapshot.call_id, call_id);
        assert_eq!(snapshot.line_instance, LineInstance::new(1));
        assert_eq!(snapshot.codec, Codec::Pcma);
        assert_eq!(snapshot.packet_ms, 30);
        assert_eq!(snapshot.max_frames_per_packet, 2);
        assert_eq!(snapshot.receive_peer, Some(receive_peer));
        assert_eq!(snapshot.transmit_peer, Some(transmit_peer));
        assert_eq!(snapshot.packets_sent, expected.packets_sent);
        assert_eq!(snapshot.octets_sent, expected.octets_sent);
        assert_eq!(snapshot.packets_received, expected.packets_received);
        assert_eq!(snapshot.octets_received, expected.octets_received);
        assert_eq!(snapshot.packets_lost, expected.packets_lost);
        assert_eq!(snapshot.jitter_millis, expected.jitter_millis);
        assert_eq!(snapshot.latency_millis, expected.latency_millis);
        assert_eq!(
            snapshot.quality_byte_count,
            expected.quality.as_bytes().len()
        );
        let debug = format!("{snapshot:?}");
        assert!(!debug.contains("2002"));
        assert!(!debug.contains("MLQK"));
        assert_eq!(
            handle.latest_media_statistics(&device_id),
            Some(snapshot.clone())
        );
        assert_eq!(
            handle.media_statistics(),
            vec![(device_id.clone(), snapshot.clone())]
        );

        phone
            .write_all(
                &ClientMessage::ConnectionStatisticsResponse(expected)
                    .encode(protocol)
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(25), events.recv())
                .await
                .is_err(),
            "a duplicate response emitted a second event"
        );

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[test]
    fn expired_statistics_requests_are_pruned_at_the_deadline() {
        let now = Instant::now();
        let mut pending = HashMap::from([(
            42,
            PendingConnectionStatistics {
                session_generation: SessionGeneration::new(1).unwrap(),
                request_generation: 2,
                call_id: CallId(3),
                line_instance: 1,
                codec: Codec::Pcmu,
                packet_ms: 20,
                max_frames_per_packet: 1,
                receive_peer: None,
                transmit_peer: None,
                directory_number: "2002".into(),
                processing: StatisticsProcessing::Clear,
                expires_at: now,
            },
        )]);
        prune_connection_statistics(&mut pending, now);
        assert!(pending.is_empty());
    }

    #[test]
    fn statistics_directory_follows_the_call_direction() {
        let inbound = CallInfo {
            direction: crate::types::CallDirection::Inbound,
            calling_number: "inbound-peer".into(),
            called_number: "local-line".into(),
            ..CallInfo::default()
        };
        let outbound = CallInfo {
            direction: crate::types::CallDirection::Outbound,
            calling_number: "local-line".into(),
            called_number: "outbound-peer".into(),
            ..CallInfo::default()
        };
        assert_eq!(statistics_directory_for_call_info(&inbound), "inbound-peer");
        assert_eq!(
            statistics_directory_for_call_info(&outbound),
            "outbound-peer"
        );
    }

    #[test]
    fn replacement_calls_and_media_requests_never_reuse_identifiers() {
        let device = definition();
        let mut state = SessionState {
            registration: DeviceRegistration {
                id: device.id.clone(),
                peer: "127.0.0.1:2000".parse().unwrap(),
                transport: StationTransport::Clear,
                reported_address: Some(Ipv4Addr::LOCALHOST),
                reported_ipv6_address: None,
                device_type: DeviceType::Cisco7962,
                protocol: ProtocolVersion::V22,
                firmware: "test".into(),
            },
            device,
            features: PhoneFeatures::empty(),
            generation: SessionGeneration::new(1).unwrap(),
            calls_by_id: HashMap::new(),
            calls_by_wire: HashMap::new(),
            media_capabilities: StationMediaCapabilities::default(),
            next_media_token: MediaRequestToken::new(1),
            next_multicast_generation: 0,
            multicast: HashMap::new(),
            pending_connection_statistics: HashMap::new(),
            statistics_references: HashSet::from([42]),
            cancelled_calls: HashSet::new(),
            last_number_by_line: HashMap::new(),
            forwarding_by_line: HashMap::new(),
            feature_states: HashMap::new(),
            mwi_by_line: HashMap::new(),
            mobility_appearances: HashMap::new(),
            active_key_mode: KeyMode::OnHook,
            active_call_id: None,
            pending_parking_menu: None,
            persistent_status_message: false,
            headset_enabled: false,
            media_path_states: HashMap::new(),
        };
        let replacement = insert_call(&mut state, CallId(42), 1, Codec::Pcmu, CallState::OffHook);
        assert_eq!(replacement.wire_reference, 43);
        assert_eq!(state.calls_by_wire.get(&42), None);
        assert_eq!(state.calls_by_wire.get(&43), Some(&CallId(42)));

        state.next_media_token = MediaRequestToken::new(u32::MAX);
        let final_identity = allocate_media_request_identity(&mut state, CallId(42)).unwrap();
        assert_eq!(final_identity.token().get(), u32::MAX);
        assert!(state.next_media_token.is_none());
        let generation_after_final_token = state.calls_by_id[&CallId(42)].media.generation;
        assert!(matches!(
            allocate_media_request_identity(&mut state, CallId(42)),
            Err(ServerError::MediaRequestIdentityExhausted)
        ));
        assert_eq!(
            state.calls_by_id[&CallId(42)].media.generation,
            generation_after_final_token,
            "failed allocation mutated the call generation"
        );

        state.next_media_token = MediaRequestToken::new(7);
        state
            .calls_by_id
            .get_mut(&CallId(42))
            .unwrap()
            .media
            .generation = u64::MAX;
        assert!(matches!(
            allocate_media_request_identity(&mut state, CallId(42)),
            Err(ServerError::MediaRequestIdentityExhausted)
        ));
        assert_eq!(state.next_media_token.unwrap().get(), 7);
    }

    #[test]
    fn omitted_call_reference_uses_active_then_configured_answer_order() {
        let device = definition();
        let mut state = SessionState {
            registration: DeviceRegistration {
                id: device.id.clone(),
                peer: "127.0.0.1:2000".parse().unwrap(),
                transport: StationTransport::Clear,
                reported_address: Some(Ipv4Addr::LOCALHOST),
                reported_ipv6_address: None,
                device_type: DeviceType::Cisco7962,
                protocol: ProtocolVersion::V22,
                firmware: "test".into(),
            },
            device,
            features: PhoneFeatures::empty(),
            generation: SessionGeneration::new(1).unwrap(),
            calls_by_id: HashMap::new(),
            calls_by_wire: HashMap::new(),
            media_capabilities: StationMediaCapabilities::default(),
            next_media_token: MediaRequestToken::new(1),
            next_multicast_generation: 0,
            multicast: HashMap::new(),
            pending_connection_statistics: HashMap::new(),
            statistics_references: HashSet::new(),
            cancelled_calls: HashSet::new(),
            last_number_by_line: HashMap::new(),
            forwarding_by_line: HashMap::new(),
            feature_states: HashMap::new(),
            mwi_by_line: HashMap::new(),
            mobility_appearances: HashMap::new(),
            active_key_mode: KeyMode::RingIn,
            active_call_id: None,
            pending_parking_menu: None,
            persistent_status_message: false,
            headset_enabled: false,
            media_path_states: HashMap::new(),
        };
        let first = insert_call(
            &mut state,
            CallId(10),
            1,
            Codec::Pcmu,
            CallState::CallWaiting,
        );
        let last = insert_call(&mut state, CallId(20), 2, Codec::Pcma, CallState::RingIn);

        assert_eq!(
            find_answer_call(&state, 0, 0, CallSelectionOrder::OldestFirst)
                .map(|call| call.call_id),
            Some(CallId(10))
        );
        assert_eq!(
            find_answer_call(&state, 0, 0, CallSelectionOrder::LastFirst).map(|call| call.call_id),
            Some(CallId(20))
        );
        assert_eq!(
            find_answer_call(
                &state,
                first.wire_reference,
                1,
                CallSelectionOrder::LastFirst,
            )
            .map(|call| call.call_id),
            Some(CallId(10))
        );
        assert!(
            find_answer_call(
                &state,
                last.wire_reference,
                1,
                CallSelectionOrder::LastFirst,
            )
            .is_none()
        );
        assert_eq!(
            find_answer_call(&state, 0, 1, CallSelectionOrder::LastFirst).map(|call| call.call_id),
            Some(CallId(10))
        );

        state.active_call_id = Some(last.call_id);
        assert_eq!(
            find_call(&state, 0).map(|call| call.call_id),
            Some(CallId(20))
        );
        assert_eq!(
            find_answer_call(&state, 0, 0, CallSelectionOrder::OldestFirst)
                .map(|call| call.call_id),
            Some(CallId(20))
        );
        remove_call(&mut state, CallId(20));
        assert_eq!(state.active_call_id, None);
        assert_eq!(
            find_call(&state, 0).map(|call| call.call_id),
            Some(CallId(10))
        );
    }

    #[test]
    fn distinct_and_urgent_ring_modes_preserve_exact_waiting_semantics() {
        assert_eq!(
            incoming_ringer(Some(IncomingRing::default()), CallState::RingIn),
            Some(IncomingRing {
                mode: RingerMode::Inside,
                duration: RingDuration::Normal,
            })
        );
        assert_eq!(
            incoming_ringer(
                Some(IncomingRing {
                    mode: RingerMode::Bellcore4,
                    duration: RingDuration::Normal,
                }),
                CallState::RingIn,
            ),
            Some(IncomingRing {
                mode: RingerMode::Bellcore4,
                duration: RingDuration::Normal,
            })
        );
        assert_eq!(
            incoming_ringer(
                Some(IncomingRing {
                    mode: RingerMode::Bellcore4,
                    duration: RingDuration::Normal,
                }),
                CallState::CallWaiting,
            ),
            Some(IncomingRing {
                mode: RingerMode::Silent,
                duration: RingDuration::Single,
            })
        );
        assert_eq!(
            incoming_ringer(
                Some(IncomingRing {
                    mode: RingerMode::Urgent,
                    duration: RingDuration::Normal,
                }),
                CallState::CallWaiting,
            ),
            Some(IncomingRing {
                mode: RingerMode::Urgent,
                duration: RingDuration::Single,
            })
        );
        assert_eq!(incoming_ringer(None, CallState::CallWaiting), None);
    }

    #[test]
    fn answer_order_reload_updates_the_shared_policy_without_replacing_sessions() {
        let (command_tx, _command_rx) = mpsc::channel(1);
        let order = Arc::new(RwLock::new(CallSelectionOrder::OldestFirst));
        let handle = ServerHandle {
            command_tx,
            next_call_id: Arc::new(AtomicU64::new(1)),
            latest_media_statistics: Arc::new(RwLock::new(HashMap::new())),
            call_answer_order: Arc::clone(&order),
        };
        handle.set_call_answer_order(CallSelectionOrder::LastFirst);
        assert_eq!(
            *order.read().expect("test answer-order lock poisoned"),
            CallSelectionOrder::LastFirst
        );
    }

    #[test]
    fn calendar_conversion_is_stable() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_358), (2023, 1, 1));
        assert_eq!(
            time_date_message_at(UNIX_EPOCH + Duration::from_secs(23 * 3_600 + 45 * 60), 30),
            ServerMessage::TimeDate {
                year: 1970,
                month: 1,
                weekday: 6,
                day: 2,
                hour: 0,
                minute: 15,
                second: 0,
                milliseconds: 0,
                unix_seconds: 87_300,
            }
        );
    }

    #[test]
    fn mwi_policy_projects_configured_cadence_and_on_call_visibility() {
        let hidden_on_call = crate::types::StationUiPolicy {
            mwi_lamp_mode: LampMode::Flash,
            mwi_on_call: false,
            ..Default::default()
        };
        assert_eq!(
            projected_mwi_lamp(hidden_on_call, false, true),
            LampMode::Flash
        );
        assert_eq!(
            projected_mwi_lamp(hidden_on_call, true, true),
            LampMode::Off
        );
        assert_eq!(
            projected_mwi_lamp(hidden_on_call, false, false),
            LampMode::Off
        );

        let visible_on_call = crate::types::StationUiPolicy {
            mwi_lamp_mode: LampMode::Blink,
            mwi_on_call: true,
            ..Default::default()
        };
        assert_eq!(
            projected_mwi_lamp(visible_on_call, true, true),
            LampMode::Blink
        );
    }

    #[test]
    fn call_history_distinguishes_answered_missed_and_elsewhere_answered() {
        assert_eq!(
            updated_history_disposition(CallHistoryDisposition::Missed, CallState::Connected),
            CallHistoryDisposition::Received
        );
        assert_eq!(
            updated_history_disposition(CallHistoryDisposition::Missed, CallState::OnHook),
            CallHistoryDisposition::Missed
        );
        assert_eq!(
            updated_history_disposition(CallHistoryDisposition::Missed, CallState::RemoteMultiline,),
            CallHistoryDisposition::Ignore
        );
        assert_eq!(
            updated_history_disposition(CallHistoryDisposition::Placed, CallState::Connected),
            CallHistoryDisposition::Placed
        );
    }

    #[test]
    fn reconfiguration_classifies_added_changed_removed_and_unchanged_devices() {
        let unchanged = definition_for("SEP001122334455");
        let mut changed = definition_for("SEP112233445566");
        let removed = definition_for("SEP223344556677");
        let added = definition_for("SEP334455667788");
        let current = HashMap::from([
            (unchanged.id.clone(), unchanged.clone()),
            (changed.id.clone(), changed.clone()),
            (removed.id.clone(), removed),
        ]);
        changed.description = "Changed station".into();
        let next = HashMap::from([
            (unchanged.id.clone(), unchanged),
            (changed.id.clone(), changed),
            (added.id.clone(), added),
        ]);

        assert_eq!(
            reconfigure_result(&current, &next, &HashSet::new()),
            ReconfigureResult {
                added: vec![DeviceId::new("SEP334455667788").unwrap()],
                changed: vec![DeviceId::new("SEP112233445566").unwrap()],
                removed: vec![DeviceId::new("SEP223344556677").unwrap()],
            }
        );
        assert!(reconfigure_result(&current, &current, &HashSet::new()).is_unchanged());
        assert_eq!(
            reconfigure_result(
                &current,
                &current,
                &HashSet::from([DeviceId::new("SEP001122334455").unwrap()]),
            )
            .changed,
            vec![DeviceId::new("SEP001122334455").unwrap()]
        );
    }

    #[tokio::test]
    async fn reconfiguration_preserves_unchanged_session_calls_and_rolls_back_invalid_candidates() {
        let original = definition();
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let (server, handle, mut events) = Server::bind(config, [original.clone()]).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let mut phone = TcpStream::connect(address).await.unwrap();
        let mut decoder = FrameDecoder::new();
        let protocol = ProtocolVersion::V22;

        phone.write_all(&register_bytes(protocol)).await.unwrap();
        read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Registered(_)
            }))
        ));
        phone
            .write_all(
                &ClientMessage::OffHook {
                    line_instance: 1,
                    call_reference: 0,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        let call_id = match events.recv().await {
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::OffHook { call_id, .. },
            })) => call_id,
            event => panic!("unexpected event: {event:?}"),
        };

        let added = definition_for("SEP112233445566");
        assert_eq!(
            handle
                .reconfigure([original.clone(), added.clone()])
                .await
                .unwrap(),
            ReconfigureResult {
                added: vec![added.id],
                ..ReconfigureResult::default()
            }
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), events.recv())
                .await
                .is_err()
        );

        let mut invalid = original.clone();
        let ButtonDefinition::Line(line) = &mut invalid.buttons[0] else {
            panic!("expected line button");
        };
        line.instance = 0;
        assert!(handle.reconfigure([invalid]).await.is_err());

        phone
            .write_all(
                &ClientMessage::KeypadButton {
                    button: Digit::Number(7),
                    line_instance: 1,
                    call_reference: 0,
                    wire_layout: None,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent { session_generation: _, device_id: _, event: DeviceEventKind::Digit {
                call_id: same_call,
                digit: Digit::Number(7),
                ..
            } })) if same_call == call_id
        ));

        let mut changed = original;
        changed.description = "Changed station".into();
        let report = handle.reconfigure([changed]).await.unwrap();
        assert_eq!(
            report.changed,
            vec![DeviceId::new("SEP001122334455").unwrap()]
        );
        assert_eq!(
            report.removed,
            vec![DeviceId::new("SEP112233445566").unwrap()]
        );
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), events.recv()).await,
            Ok(Some(Event::Device(DeviceEvent { session_generation: _, device_id, event: DeviceEventKind::Disconnected {} })))
                if device_id == DeviceId::new("SEP001122334455").unwrap()
        ));

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn concurrent_registration_and_disconnect_storm_retires_every_session_once() {
        const PHONE_COUNT: usize = 48;
        let device_ids = (0..PHONE_COUNT)
            .map(|index| format!("SEP{index:012X}"))
            .collect::<Vec<_>>();
        let definitions = device_ids
            .iter()
            .map(|device_id| definition_for(device_id))
            .collect::<Vec<_>>();
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let (server, handle, mut events) = Server::bind(config, definitions).await.unwrap();
        let address = server.local_addr().unwrap();
        let server_task = tokio::spawn(server.run());
        let barrier = Arc::new(tokio::sync::Barrier::new(PHONE_COUNT));
        let mut registrations = tokio::task::JoinSet::new();
        for device_id in &device_ids {
            let barrier = Arc::clone(&barrier);
            let device_id = device_id.clone();
            registrations.spawn(async move {
                let mut phone = TcpStream::connect(address).await.unwrap();
                let mut decoder = FrameDecoder::new();
                barrier.wait().await;
                phone
                    .write_all(&register_bytes_for_device(
                        ProtocolVersion::V22,
                        115,
                        &device_id,
                    ))
                    .await
                    .unwrap();
                read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
                (device_id, phone)
            });
        }
        let mut phones = Vec::with_capacity(PHONE_COUNT);
        while let Some(result) =
            tokio::time::timeout(Duration::from_secs(5), registrations.join_next())
                .await
                .expect("registration storm exceeded its bound")
        {
            phones.push(result.unwrap());
        }
        assert_eq!(phones.len(), PHONE_COUNT);

        let mut registered = HashSet::new();
        while registered.len() < PHONE_COUNT {
            let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
                .await
                .expect("registration events exceeded their bound")
                .expect("server stopped during registration storm");
            if let Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Registered(registration),
            }) = event
            {
                assert!(registered.insert(registration.id));
            }
        }
        drop(phones);

        let mut disconnected = HashSet::new();
        while disconnected.len() < PHONE_COUNT {
            let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
                .await
                .expect("disconnect events exceeded their bound")
                .expect("server stopped during disconnect storm");
            if let Event::Device(DeviceEvent {
                session_generation: _,
                device_id,
                event: DeviceEventKind::Disconnected {},
            }) = event
            {
                assert!(disconnected.insert(device_id));
            }
        }
        assert_eq!(registered, disconnected);

        handle.shutdown().await.unwrap();
        server_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn repeated_server_load_and_unload_releases_all_shared_runtime_state() {
        const CYCLES: usize = 32;
        for _ in 0..CYCLES {
            let config = ServerConfig {
                bind: "127.0.0.1:0".parse().unwrap(),
                advertised_address: Ipv4Addr::LOCALHOST,
                ..ServerConfig::default()
            };
            let (server, handle, events) = Server::bind(config, [definition()]).await.unwrap();
            let sessions = Arc::downgrade(&server.sessions);
            let call_ids = Arc::downgrade(&handle.next_call_id);
            let statistics = Arc::downgrade(&handle.latest_media_statistics);
            let answer_order = Arc::downgrade(&handle.call_answer_order);
            let server_task = tokio::spawn(server.run());

            handle.shutdown().await.unwrap();
            server_task.await.unwrap().unwrap();
            drop(events);
            drop(handle);

            assert!(sessions.upgrade().is_none());
            assert!(call_ids.upgrade().is_none());
            assert!(statistics.upgrade().is_none());
            assert!(answer_order.upgrade().is_none());
        }
    }

    #[tokio::test]
    async fn reconfiguration_disconnects_a_removed_device_without_touching_its_peer() {
        let retained = definition_for("SEP001122334455");
        let removed = definition_for("SEP112233445566");
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let (server, handle, mut events) =
            Server::bind(config, [retained.clone(), removed.clone()])
                .await
                .unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let protocol = ProtocolVersion::V22;

        let mut retained_phone = TcpStream::connect(address).await.unwrap();
        let mut retained_decoder = FrameDecoder::new();
        retained_phone
            .write_all(&register_bytes_for_device(
                protocol,
                115,
                retained.id.as_str(),
            ))
            .await
            .unwrap();
        read_until_message(
            &mut retained_phone,
            &mut retained_decoder,
            id::CAPABILITIES_REQ,
        )
        .await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Registered(_)
            }))
        ));

        let mut removed_phone = TcpStream::connect(address).await.unwrap();
        let mut removed_decoder = FrameDecoder::new();
        removed_phone
            .write_all(&register_bytes_for_device(
                protocol,
                115,
                removed.id.as_str(),
            ))
            .await
            .unwrap();
        read_until_message(
            &mut removed_phone,
            &mut removed_decoder,
            id::CAPABILITIES_REQ,
        )
        .await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Registered(_)
            }))
        ));

        let report = handle.reconfigure([retained.clone()]).await.unwrap();
        assert_eq!(report.removed, vec![removed.id.clone()]);
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), events.recv()).await,
            Ok(Some(Event::Device(DeviceEvent { session_generation: _, device_id, event: DeviceEventKind::Disconnected {} }))) if device_id == removed.id
        ));

        retained_phone
            .write_all(
                &Frame::new(protocol.wire(), id::KEEP_ALIVE, Vec::new())
                    .encode()
                    .unwrap(),
            )
            .await
            .unwrap();
        read_until_message(
            &mut retained_phone,
            &mut retained_decoder,
            id::KEEP_ALIVE_ACK,
        )
        .await;
        assert!(
            tokio::time::timeout(Duration::from_millis(50), events.recv())
                .await
                .is_err()
        );

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[test]
    fn server_response_uses_the_accepted_local_interface_with_configured_fallback() {
        assert_eq!(
            server_response_address(
                "10.20.30.40".parse().unwrap(),
                "192.0.2.10".parse().unwrap(),
                Some("2001:db8::10".parse().unwrap()),
            ),
            "10.20.30.40".parse::<IpAddr>().unwrap()
        );
        assert_eq!(
            server_response_address(
                "2001:db8::20".parse().unwrap(),
                "192.0.2.10".parse().unwrap(),
                Some("2001:db8::10".parse().unwrap()),
            ),
            "2001:db8::20".parse::<IpAddr>().unwrap()
        );
        assert_eq!(
            server_response_address(
                "0.0.0.0".parse().unwrap(),
                "192.0.2.10".parse().unwrap(),
                Some("2001:db8::10".parse().unwrap()),
            ),
            "192.0.2.10".parse::<IpAddr>().unwrap()
        );
        assert_eq!(
            server_response_address(
                "::".parse().unwrap(),
                "192.0.2.10".parse().unwrap(),
                Some("2001:db8::10".parse().unwrap()),
            ),
            "2001:db8::10".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn synchronous_offer_and_hangup_commands_cannot_overtake_each_other() {
        let (command_tx, mut command_rx) = mpsc::channel(4);
        let handle = ServerHandle {
            command_tx,
            next_call_id: Arc::new(AtomicU64::new(1)),
            latest_media_statistics: Arc::new(RwLock::new(HashMap::new())),
            call_answer_order: Arc::new(RwLock::new(CallSelectionOrder::OldestFirst)),
        };
        let device_id = DeviceId::new("SEP001122334455").unwrap();
        let call_id = CallId(42);
        handle
            .try_offer_incoming_call_with_id(
                device_id.clone(),
                LineInstance::new(1),
                call_id,
                CallInfo {
                    direction: crate::types::CallDirection::Inbound,
                    calling_name: "Caller".into(),
                    calling_number: "1002".into(),
                    called_name: "Desk".into(),
                    called_number: "1001".into(),
                    ..CallInfo::default()
                },
            )
            .unwrap();
        handle
            .try_send(Command::new(
                device_id.clone(),
                CommandAction::CloseCall { call_id },
            ))
            .unwrap();

        assert!(matches!(
            command_rx.try_recv().unwrap(),
            ServerCommand::OfferIncoming {
                device_id: offered_device,
                call_id: offered_call,
                ..
            } if offered_device == device_id && offered_call == call_id
        ));
        assert!(matches!(
            command_rx.try_recv().unwrap(),
            ServerCommand::Public(command)
                if matches!(command.as_ref(), Command {
                    device_id: closed_device,
                    action: CommandAction::CloseCall {
                    call_id: closed_call,
                } } if closed_device == &device_id && *closed_call == call_id)
        ));
    }

    #[test]
    fn synchronous_command_queue_reports_saturation_and_recovers_after_drain() {
        let (command_tx, mut command_rx) = mpsc::channel(1);
        let handle = ServerHandle {
            command_tx,
            next_call_id: Arc::new(AtomicU64::new(1)),
            latest_media_statistics: Arc::new(RwLock::new(HashMap::new())),
            call_answer_order: Arc::new(RwLock::new(CallSelectionOrder::OldestFirst)),
        };
        let device_id = DeviceId::new("SEP001122334455").unwrap();

        handle
            .try_send(Command::new(
                device_id.clone(),
                CommandAction::DisconnectDevice {},
            ))
            .unwrap();
        assert!(matches!(
            handle.try_send(Command::new(
                device_id.clone(),
                CommandAction::DisconnectDevice {}
            )),
            Err(ServerError::CommandQueueFull)
        ));
        assert!(matches!(
            handle.try_offer_incoming_call_with_id(
                device_id.clone(),
                LineInstance::new(1),
                CallId(42),
                CallInfo {
                    direction: crate::types::CallDirection::Inbound,
                    calling_name: "Caller".into(),
                    calling_number: "1002".into(),
                    called_name: "Desk".into(),
                    called_number: "1001".into(),
                    ..CallInfo::default()
                },
            ),
            Err(ServerError::CommandQueueFull)
        ));

        assert!(matches!(
            command_rx.try_recv().unwrap(),
            ServerCommand::Public(command)
                if matches!(command.as_ref(), Command {
                    device_id: queued_device,
                    action: CommandAction::DisconnectDevice { .. },
                } if queued_device == &device_id)
        ));
        handle
            .try_send(Command::new(
                device_id.clone(),
                CommandAction::DisconnectDevice {},
            ))
            .unwrap();
        assert!(matches!(
            command_rx.try_recv().unwrap(),
            ServerCommand::Public(command)
                if matches!(command.as_ref(), Command {
                    device_id: queued_device,
                    action: CommandAction::DisconnectDevice { .. },
                } if queued_device == &device_id)
        ));
    }

    #[tokio::test]
    async fn confirmed_command_waits_for_device_write_and_propagates_failure() {
        let (command_tx, mut command_rx) = mpsc::channel(2);
        let handle = ServerHandle {
            command_tx,
            next_call_id: Arc::new(AtomicU64::new(1)),
            latest_media_statistics: Arc::new(RwLock::new(HashMap::new())),
            call_answer_order: Arc::new(RwLock::new(CallSelectionOrder::OldestFirst)),
        };
        let device_id = DeviceId::new("SEP001122334455").unwrap();

        let success = tokio::spawn({
            let handle = handle.clone();
            let device_id = device_id.clone();
            async move {
                handle
                    .send_confirmed(Command::new(
                        device_id,
                        CommandAction::StopAnnouncement {
                            conference_id: ConferenceId::new(44),
                        },
                    ))
                    .await
            }
        });
        let ServerCommand::Confirmed { written, .. } = command_rx.recv().await.unwrap() else {
            panic!("expected a confirmed command")
        };
        assert!(!success.is_finished());
        written.send(Ok(())).unwrap();
        assert!(success.await.unwrap().is_ok());

        let failure = tokio::spawn({
            let handle = handle.clone();
            async move {
                handle
                    .send_confirmed(Command::new(
                        device_id,
                        CommandAction::SetMicrophoneMode { enabled: false },
                    ))
                    .await
            }
        });
        let ServerCommand::Confirmed { written, .. } = command_rx.recv().await.unwrap() else {
            panic!("expected a confirmed command")
        };
        written.send(Err("socket closed".into())).unwrap();
        assert!(matches!(
            failure.await.unwrap(),
            Err(ServerError::CommandWrite(message)) if message == "socket closed"
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn ordering_acknowledgement_timeout_bounds_a_stalled_writer_and_retires_sender() {
        let (command_tx, mut command_rx) = mpsc::channel(1);
        let handle = ServerHandle {
            command_tx,
            next_call_id: Arc::new(AtomicU64::new(1)),
            latest_media_statistics: Arc::new(RwLock::new(HashMap::new())),
            call_answer_order: Arc::new(RwLock::new(CallSelectionOrder::OldestFirst)),
        };
        let pending = tokio::spawn(async move {
            handle
                .send_confirmed(Command::new(
                    DeviceId::new("SEP001122334455").unwrap(),
                    CommandAction::SetMicrophoneMode { enabled: false },
                ))
                .await
        });
        let ServerCommand::Confirmed { written, .. } = command_rx.recv().await.unwrap() else {
            panic!("expected a confirmed command")
        };

        tokio::time::advance(ORDERING_ACKNOWLEDGEMENT_TIMEOUT).await;
        assert!(matches!(
            pending.await.unwrap(),
            Err(ServerError::CommandAcknowledgementTimeout)
        ));
        assert!(written.send(Ok(())).is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn expired_confirmed_commands_are_retired_at_both_queue_boundaries() {
        let device = definition();
        let device_id = device.id.clone();
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let (mut server, handle, _events) = Server::bind(config, [device]).await.unwrap();
        let (session_tx, mut session_rx) = mpsc::channel(2);
        server.sessions.lock().await.insert(
            device_id.clone(),
            SessionSender {
                generation: SessionGeneration::new(1).unwrap(),
                anonymous_hotline: false,
                tx: session_tx,
            },
        );

        let server_queued = tokio::spawn({
            let handle = handle.clone();
            let device_id = device_id.clone();
            async move {
                handle
                    .send_confirmed(Command::new(
                        device_id,
                        CommandAction::SetMicrophoneMode { enabled: false },
                    ))
                    .await
            }
        });
        let ServerCommand::Confirmed {
            command,
            written,
            expires_at,
        } = server.command_rx.recv().await.unwrap()
        else {
            panic!("expected a server-queued confirmed command")
        };
        tokio::time::advance(ORDERING_ACKNOWLEDGEMENT_TIMEOUT).await;
        assert!(matches!(
            server_queued.await.unwrap(),
            Err(ServerError::CommandAcknowledgementTimeout)
        ));
        server
            .dispatch_confirmed(command, written, expires_at)
            .await;
        assert!(matches!(
            session_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));

        let session_queued = tokio::spawn({
            let handle = handle.clone();
            async move {
                handle
                    .send_confirmed(Command::new(
                        device_id,
                        CommandAction::SetMicrophoneMode { enabled: true },
                    ))
                    .await
            }
        });
        let ServerCommand::Confirmed {
            command,
            written,
            expires_at,
        } = server.command_rx.recv().await.unwrap()
        else {
            panic!("expected another server-queued confirmed command")
        };
        server
            .dispatch_confirmed(command, written, expires_at)
            .await;
        let queued = session_rx.recv().await.unwrap();
        tokio::time::advance(ORDERING_ACKNOWLEDGEMENT_TIMEOUT).await;
        assert!(matches!(
            session_queued.await.unwrap(),
            Err(ServerError::CommandAcknowledgementTimeout)
        ));
        assert!(prepare_session_command(queued).is_none());
    }

    #[tokio::test]
    async fn forwarding_collection_commands_propagate_confirmed_writer_failures() {
        let (command_tx, mut command_rx) = mpsc::channel(3);
        let handle = ServerHandle {
            command_tx,
            next_call_id: Arc::new(AtomicU64::new(1)),
            latest_media_statistics: Arc::new(RwLock::new(HashMap::new())),
            call_answer_order: Arc::new(RwLock::new(CallSelectionOrder::OldestFirst)),
        };
        let device_id = DeviceId::new("SEP001122334455").unwrap();
        let commands = [
            Command::new(
                device_id.clone(),
                CommandAction::BeginCall {
                    line_instance: LineInstance(1),
                    call_id: CallId(42),
                    codec: Codec::Pcmu,
                },
            ),
            Command::new(
                device_id.clone(),
                CommandAction::DisplayPrompt {
                    call_id: CallId(42),
                    timeout_seconds: 0,
                    text: "Enter forwarding destination".into(),
                },
            ),
            Command::new(
                device_id,
                CommandAction::CloseCall {
                    call_id: CallId(42),
                },
            ),
        ];

        for (index, command) in commands.into_iter().enumerate() {
            let pending = tokio::spawn({
                let handle = handle.clone();
                async move { handle.send_confirmed(command).await }
            });
            let ServerCommand::Confirmed { written, .. } = command_rx.recv().await.unwrap() else {
                panic!("expected a confirmed forwarding command")
            };
            assert!(!pending.is_finished());
            written
                .send(Err(format!("forwarding writer failed at stage {index}")))
                .unwrap();
            assert!(matches!(
                pending.await.unwrap(),
                Err(ServerError::CommandWrite(message))
                    if message == format!("forwarding writer failed at stage {index}")
            ));
        }
    }

    #[tokio::test]
    async fn two_phone_shared_offer_honors_ring_policy_and_remote_control_events() {
        let protocol = ProtocolVersion::V22;
        let first_id = DeviceId::new("SEP001122334455").unwrap();
        let second_id = DeviceId::new("SEP112233445566").unwrap();
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let mut first_definition = definition_for(first_id.as_str());
        let mut second_definition = definition_for(second_id.as_str());
        for definition in [&mut first_definition, &mut second_definition] {
            definition.soft_keys = profile_with(
                KeyMode::OnHookStealable,
                vec![
                    SoftKey::Intercept,
                    SoftKey::Barge,
                    SoftKey::Conference,
                    SoftKey::NewCall,
                ],
            );
        }
        let stealable_mask = second_definition
            .soft_keys
            .valid_mask(KeyMode::OnHookStealable);
        let (server, handle, mut events) =
            Server::bind(config, [first_definition, second_definition])
                .await
                .unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let mut first = TcpStream::connect(address).await.unwrap();
        let mut second = TcpStream::connect(address).await.unwrap();
        let mut first_decoder = FrameDecoder::new();
        let mut second_decoder = FrameDecoder::new();
        first
            .write_all(&register_bytes_for_device(protocol, 115, first_id.as_str()))
            .await
            .unwrap();
        second
            .write_all(&register_bytes_for_device(
                protocol,
                115,
                second_id.as_str(),
            ))
            .await
            .unwrap();
        read_until_message(&mut first, &mut first_decoder, id::REGISTER_ACK).await;
        read_until_message(&mut second, &mut second_decoder, id::REGISTER_ACK).await;
        let mut registered = HashSet::new();
        while registered.len() < 2 {
            if let Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Registered(registration),
            })) = events.recv().await
            {
                registered.insert(registration.id);
            }
        }
        assert_eq!(
            registered,
            HashSet::from([first_id.clone(), second_id.clone()])
        );

        let info = CallInfo {
            direction: crate::types::CallDirection::Inbound,
            calling_name: "Caller".into(),
            calling_number: "1002".into(),
            called_name: "Shared desk".into(),
            called_number: "1001".into(),
            ..CallInfo::default()
        };
        let first_call = CallId(101);
        let second_call = CallId(102);
        handle
            .try_offer_incoming_call_with_id_and_ring(
                first_id.clone(),
                LineInstance::new(1),
                first_call,
                info.clone(),
                true,
            )
            .unwrap();
        handle
            .try_offer_incoming_call_with_id_and_ring(
                second_id.clone(),
                LineInstance::new(1),
                second_call,
                info,
                false,
            )
            .unwrap();
        let first_frames = read_until_message(
            &mut first,
            &mut first_decoder,
            id::DISPLAY_DYNAMIC_PROMPT_STATUS,
        )
        .await;
        let second_frames = read_until_message(
            &mut second,
            &mut second_decoder,
            id::DISPLAY_DYNAMIC_PROMPT_STATUS,
        )
        .await;
        assert!(first_frames.iter().any(|frame| matches!(
            ServerMessage::decode(frame.clone(), protocol),
            Ok(ServerMessage::SetRinger {
                mode: RingerMode::Inside,
                ..
            })
        )));
        assert!(!second_frames.iter().any(|frame| matches!(
            ServerMessage::decode(frame.clone(), protocol),
            Ok(ServerMessage::SetRinger {
                mode: RingerMode::Inside,
                ..
            })
        )));

        for (device_id, call_id) in [
            (first_id.clone(), first_call),
            (second_id.clone(), second_call),
        ] {
            handle
                .send(Command::new(
                    device_id,
                    CommandAction::SetCallState {
                        call_id,
                        state: CallState::RemoteMultiline,
                    },
                ))
                .await
                .unwrap();
        }
        let first_frames =
            read_until_message(&mut first, &mut first_decoder, id::SELECT_SOFT_KEYS).await;
        let second_frames =
            read_until_message(&mut second, &mut second_decoder, id::SELECT_SOFT_KEYS).await;
        assert!(first_frames.iter().any(|frame| matches!(
            ServerMessage::decode(frame.clone(), protocol),
            Ok(ServerMessage::SetRinger {
                mode: RingerMode::Off,
                ..
            })
        )));
        assert!(first_frames.iter().any(|frame| matches!(
            ServerMessage::decode(frame.clone(), protocol),
            Ok(ServerMessage::SetLamp {
                stimulus: ButtonType::Line,
                mode: LampMode::On,
                ..
            })
        )));
        assert!(second_frames.iter().any(|frame| matches!(
            ServerMessage::decode(frame.clone(), protocol),
            Ok(ServerMessage::SelectSoftKeys {
                set: KeyMode::OnHookStealable,
                valid_mask,
                ..
            }) if valid_mask == stealable_mask
        )));

        second
            .write_all(
                &ClientMessage::SoftKeyEvent {
                    event: SoftKey::Intercept.wire_value(),
                    line_instance: 1,
                    call_reference: 0,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent { session_generation: _, device_id, event: DeviceEventKind::SoftKey {
                call_id: Some(call_id),
                soft_key: SoftKey::Intercept,
                ..
            } })) if device_id == second_id && call_id == second_call
        ));
        second
            .write_all(
                &ClientMessage::SoftKeyEvent {
                    event: SoftKey::Barge.wire_value(),
                    line_instance: 1,
                    call_reference: 0,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent { session_generation: _, device_id, event: DeviceEventKind::SoftKey {
                call_id: Some(call_id),
                soft_key: SoftKey::Barge,
                ..
            } })) if device_id == second_id && call_id == second_call
        ));
        second
            .write_all(
                &ClientMessage::Stimulus {
                    stimulus: Stimulus::Conference,
                    instance: 1,
                    call_reference: 0,
                    status: 0,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent { session_generation: _, device_id, event: DeviceEventKind::SoftKey {
                call_id: Some(call_id),
                soft_key: SoftKey::Conference,
                ..
            } })) if device_id == second_id && call_id == second_call
        ));
        second
            .write_all(
                &ClientMessage::Stimulus {
                    stimulus: Stimulus::Line,
                    instance: 1,
                    call_reference: 0,
                    status: 0,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent { session_generation: _, device_id, event: DeviceEventKind::LineButton {
                call_id: Some(call_id),
                ..
            } })) if device_id == second_id && call_id == second_call
        ));

        second
            .write_all(
                &ClientMessage::OnHook {
                    line_instance: 1,
                    call_reference: 0,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        read_until_message(&mut second, &mut second_decoder, id::DEFINE_TIME_DATE).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent { session_generation: _, device_id, event: DeviceEventKind::OnHook {
                call_id,
                ..
            } })) if device_id == second_id && call_id == second_call
        ));
        handle
            .send(Command::new(
                second_id.clone(),
                CommandAction::SetCallState {
                    call_id: second_call,
                    state: CallState::RemoteMultiline,
                },
            ))
            .await
            .unwrap();
        let restored =
            read_until_message(&mut second, &mut second_decoder, id::SELECT_SOFT_KEYS).await;
        assert!(restored.iter().any(|frame| matches!(
            ServerMessage::decode(frame.clone(), protocol),
            Ok(ServerMessage::CallState {
                state: CallState::RemoteMultiline,
                ..
            })
        )));

        for (device_id, call_id) in [
            (first_id.clone(), first_call),
            (second_id.clone(), second_call),
        ] {
            handle
                .send(Command::new(
                    device_id,
                    CommandAction::CloseCall { call_id },
                ))
                .await
                .unwrap();
        }
        let first_close = read_until_message(&mut first, &mut first_decoder, id::CALL_STATE).await;
        let second_close =
            read_until_message(&mut second, &mut second_decoder, id::CALL_STATE).await;
        for frames in [first_close, second_close] {
            assert!(frames.iter().any(|frame| matches!(
                ServerMessage::decode(frame.clone(), protocol),
                Ok(ServerMessage::CallState {
                    state: CallState::OnHook,
                    ..
                })
            )));
        }

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn standalone_server_registers_and_serves_line_status() {
        for protocol in [
            ProtocolVersion::V3,
            ProtocolVersion::V17,
            ProtocolVersion::V22,
        ] {
            let config = ServerConfig {
                bind: "127.0.0.1:0".parse().unwrap(),
                advertised_address: Ipv4Addr::LOCALHOST,
                ..ServerConfig::default()
            };
            let (server, handle, mut events) = Server::bind(config, [definition()]).await.unwrap();
            let address = server.local_addr().unwrap();
            let task = tokio::spawn(server.run());
            let mut phone = TcpStream::connect(address).await.unwrap();
            let mut alarm_payload = vec![0; 2_000];
            let alarm = b"<?xml version=\"1.0\"?><x-cisco-alarm></x-cisco-alarm>";
            alarm_payload[..alarm.len()].copy_from_slice(alarm);
            phone
                .write_all(
                    &Frame::new(0, id::XML_ALARM, alarm_payload)
                        .encode()
                        .unwrap(),
                )
                .await
                .unwrap();
            phone.write_all(&register_bytes(protocol)).await.unwrap();
            let mut decoder = FrameDecoder::new();
            let mut buffer = [0_u8; 1024];
            let frames = read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
            let ack = frames
                .iter()
                .find(|frame| frame.message_id == id::REGISTER_ACK)
                .cloned()
                .unwrap();
            assert_eq!(
                ServerMessage::decode(ack, protocol).unwrap(),
                ServerMessage::RegisterAck {
                    keepalive_seconds: 30,
                    secondary_keepalive_seconds: 30,
                    protocol,
                    features: PhoneFeatures::empty(),
                    date_template: Default::default(),
                }
            );
            assert!(
                frames
                    .iter()
                    .any(|frame| frame.message_id == id::CAPABILITIES_REQ)
            );
            assert!(matches!(
                events.recv().await,
                Some(Event::Device(DeviceEvent {
                    session_generation: _,
                    device_id: _,
                    event: DeviceEventKind::Registered(_)
                }))
            ));

            let malformed = Frame::new(protocol.wire(), id::IP_PORT, vec![0, 1])
                .encode()
                .unwrap();
            let keepalive = Frame::new(protocol.wire(), id::KEEP_ALIVE, vec![0; 4])
                .encode()
                .unwrap();
            phone
                .write_all(&[malformed, keepalive].concat())
                .await
                .unwrap();
            let count = phone.read(&mut buffer).await.unwrap();
            let frames = decoder.push(&buffer[..count]).unwrap();
            assert!(
                frames
                    .iter()
                    .any(|frame| frame.message_id == id::KEEP_ALIVE_ACK),
                "session did not survive a malformed application message"
            );
            assert!(matches!(
                events.recv().await,
                Some(Event::ProtocolWarning {
                    message_id: id::IP_PORT,
                    ..
                })
            ));

            phone
                .write_all(
                    &Frame::new(
                        protocol.wire(),
                        id::LINE_STAT_REQ,
                        1_u32.to_le_bytes().to_vec(),
                    )
                    .encode()
                    .unwrap(),
                )
                .await
                .unwrap();
            let line_message_id = if protocol >= ProtocolVersion::V17 {
                id::LINE_STAT_DYNAMIC
            } else {
                id::LINE_STAT
            };
            let frames = read_until_message(&mut phone, &mut decoder, line_message_id).await;
            let line = frames
                .iter()
                .find(|frame| frame.message_id == line_message_id)
                .unwrap();
            assert!(matches!(
                ServerMessage::decode(line.clone(), protocol).unwrap(),
                ServerMessage::LineStatus { number, .. } if number == "1001"
            ));

            phone
                .write_all(&ClientMessage::ServerRequest.encode(protocol).unwrap())
                .await
                .unwrap();
            let frames = read_until_message(&mut phone, &mut decoder, id::SERVER_RES).await;
            let response = frames
                .into_iter()
                .find(|frame| frame.message_id == id::SERVER_RES)
                .unwrap();
            assert_eq!(
                ServerMessage::decode(response, protocol).unwrap(),
                ServerMessage::ServerResponse {
                    servers: vec![SignalingServerEndpoint {
                        name: "sccp-protocol".into(),
                        address: address.ip(),
                        port: NonZeroU16::new(address.port()).unwrap(),
                    }],
                }
            );

            let cancelled_call = CallId(9_001);
            handle
                .try_send(Command::new(
                    DeviceId::new("SEP001122334455").unwrap(),
                    CommandAction::CloseCall {
                        call_id: cancelled_call,
                    },
                ))
                .unwrap();
            handle
                .try_offer_incoming_call_with_id(
                    DeviceId::new("SEP001122334455").unwrap(),
                    LineInstance::new(1),
                    cancelled_call,
                    CallInfo {
                        direction: crate::types::CallDirection::Inbound,
                        calling_name: "Cancelled caller".into(),
                        calling_number: "1009".into(),
                        called_name: "Desk".into(),
                        called_number: "1001".into(),
                        ..CallInfo::default()
                    },
                )
                .unwrap();
            handle
                .try_send(Command::new(
                    DeviceId::new("SEP001122334455").unwrap(),
                    CommandAction::SetCallState {
                        call_id: cancelled_call,
                        state: CallState::Connected,
                    },
                ))
                .unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(50), phone.read(&mut buffer))
                    .await
                    .is_err(),
                "a call cancelled before its offer still rang the phone"
            );

            let incoming = handle
                .offer_incoming_call(
                    DeviceId::new("SEP001122334455").unwrap(),
                    LineInstance::new(1),
                    CallInfo {
                        direction: crate::types::CallDirection::Inbound,
                        calling_name: "Caller".into(),
                        calling_number: "1002".into(),
                        called_name: "Desk".into(),
                        called_number: "1001".into(),
                        ..CallInfo::default()
                    },
                )
                .await
                .unwrap();
            let frames = read_until_message(
                &mut phone,
                &mut decoder,
                if protocol >= ProtocolVersion::V8 {
                    id::DISPLAY_DYNAMIC_PROMPT_STATUS
                } else {
                    id::DISPLAY_PROMPT_STATUS
                },
            )
            .await;
            assert!(
                frames
                    .iter()
                    .all(|frame| frame.message_id != id::ACTIVATE_CALL_PLANE),
                "RingIn activated the call plane before answer"
            );

            phone
                .write_all(
                    &ClientMessage::SoftKeyEvent {
                        event: SoftKey::Answer.wire_value(),
                        line_instance: 0,
                        call_reference: 0,
                    }
                    .encode(protocol)
                    .unwrap(),
                )
                .await
                .unwrap();
            let frames = read_until_message(&mut phone, &mut decoder, id::SET_LAMP).await;
            let off_hook = frames
                .iter()
                .position(|frame| {
                    matches!(
                        ServerMessage::decode(frame.clone(), protocol),
                        Ok(ServerMessage::CallState {
                            state: CallState::OffHook,
                            ..
                        })
                    )
                })
                .expect("answer did not transition through OffHook");
            let activate = frames
                .iter()
                .position(|frame| frame.message_id == id::ACTIVATE_CALL_PLANE)
                .expect("answer did not activate the call plane");
            assert!(
                off_hook < activate,
                "OffHook must precede call-plane activation"
            );
            let answer_event = events.recv().await;
            assert!(
                matches!(
                    answer_event,
                    Some(Event::Device(DeviceEvent { session_generation: _, device_id: _, event: DeviceEventKind::SoftKey {
                        call_id: Some(answered),
                        soft_key: SoftKey::Answer,
                        ..
                    } })) if answered == incoming
                ),
                "unexpected answer event: {answer_event:?}"
            );

            handle
                .send(Command::new(
                    DeviceId::new("SEP001122334455").unwrap(),
                    CommandAction::SetCallState {
                        call_id: incoming,
                        state: CallState::Connected,
                    },
                ))
                .await
                .unwrap();
            let frames = read_until_message(&mut phone, &mut decoder, id::SELECT_SOFT_KEYS).await;
            assert!(
                frames
                    .iter()
                    .any(|frame| frame.message_id == id::SET_SPEAKER_MODE),
                "Connected did not enable the active audio accessory"
            );
            assert!(frames.iter().any(|frame| {
                matches!(
                    ServerMessage::decode(frame.clone(), protocol),
                    Ok(ServerMessage::DisplayPrompt { text, .. }) if text == "Connected"
                )
            }));

            phone
                .write_all(
                    &ClientMessage::SoftKeyEvent {
                        event: SoftKey::Hold.wire_value(),
                        line_instance: 1,
                        call_reference: 0,
                    }
                    .encode(protocol)
                    .unwrap(),
                )
                .await
                .unwrap();
            assert!(matches!(
                events.recv().await,
                Some(Event::Device(DeviceEvent { session_generation: _, device_id: _, event: DeviceEventKind::SoftKey {
                    call_id: Some(held),
                    soft_key: SoftKey::Hold,
                    ..
                } })) if held == incoming
            ));
            handle
                .send(Command::new(
                    DeviceId::new("SEP001122334455").unwrap(),
                    CommandAction::SetCallState {
                        call_id: incoming,
                        state: CallState::Hold,
                    },
                ))
                .await
                .unwrap();
            let frames = read_until_message(&mut phone, &mut decoder, id::SELECT_SOFT_KEYS).await;
            assert!(frames.iter().any(|frame| matches!(
                ServerMessage::decode(frame.clone(), protocol),
                Ok(ServerMessage::CallState {
                    state: CallState::Hold,
                    ..
                })
            )));
            assert!(frames.iter().any(|frame| matches!(
                ServerMessage::decode(frame.clone(), protocol),
                Ok(ServerMessage::SetLamp {
                    mode: LampMode::Wink,
                    ..
                })
            )));
            assert!(frames.iter().any(|frame| matches!(
                ServerMessage::decode(frame.clone(), protocol),
                Ok(ServerMessage::SetSpeakerMode(SpeakerMode::Off))
            )));
            assert!(frames.iter().any(|frame| matches!(
                ServerMessage::decode(frame.clone(), protocol),
                Ok(ServerMessage::SelectSoftKeys {
                    set: KeyMode::OnHold,
                    ..
                })
            )));

            phone
                .write_all(
                    &ClientMessage::SoftKeyEvent {
                        event: SoftKey::Resume.wire_value(),
                        line_instance: 1,
                        call_reference: 0,
                    }
                    .encode(protocol)
                    .unwrap(),
                )
                .await
                .unwrap();
            assert!(matches!(
                events.recv().await,
                Some(Event::Device(DeviceEvent { session_generation: _, device_id: _, event: DeviceEventKind::SoftKey {
                    call_id: Some(resumed),
                    soft_key: SoftKey::Resume,
                    ..
                } })) if resumed == incoming
            ));
            handle
                .send(Command::new(
                    DeviceId::new("SEP001122334455").unwrap(),
                    CommandAction::SetCallState {
                        call_id: incoming,
                        state: CallState::Connected,
                    },
                ))
                .await
                .unwrap();
            let frames = read_until_message(&mut phone, &mut decoder, id::SELECT_SOFT_KEYS).await;
            assert!(frames.iter().any(|frame| matches!(
                ServerMessage::decode(frame.clone(), protocol),
                Ok(ServerMessage::CallState {
                    state: CallState::Connected,
                    ..
                })
            )));
            assert!(frames.iter().any(|frame| matches!(
                ServerMessage::decode(frame.clone(), protocol),
                Ok(ServerMessage::SetSpeakerMode(SpeakerMode::On))
            )));
            assert!(frames.iter().any(|frame| matches!(
                ServerMessage::decode(frame.clone(), protocol),
                Ok(ServerMessage::SelectSoftKeys {
                    set: KeyMode::Connected,
                    ..
                })
            )));

            phone
                .write_all(
                    &ClientMessage::KeypadButton {
                        button: Digit::Number(5),
                        line_instance: 1,
                        call_reference: 0,
                        wire_layout: None,
                    }
                    .encode(protocol)
                    .unwrap(),
                )
                .await
                .unwrap();
            assert!(matches!(
                events.recv().await,
                Some(Event::Device(DeviceEvent { session_generation: _, device_id: _, event: DeviceEventKind::Digit {
                    call_id,
                    digit: Digit::Number(5),
                    ..
                } })) if call_id == incoming
            ));
            assert!(
                tokio::time::timeout(Duration::from_millis(50), phone.read_u8())
                    .await
                    .is_err(),
                "connected DTMF emitted dial-collection UI"
            );

            phone
                .write_all(
                    &ClientMessage::SoftKeyEvent {
                        event: SoftKey::EndCall.wire_value(),
                        line_instance: 1,
                        call_reference: 0,
                    }
                    .encode(protocol)
                    .unwrap(),
                )
                .await
                .unwrap();
            assert!(matches!(
                events.recv().await,
                Some(Event::Device(DeviceEvent { session_generation: _, device_id: _, event: DeviceEventKind::SoftKey {
                    call_id: Some(ended),
                    soft_key: SoftKey::EndCall,
                    ..
                } })) if ended == incoming
            ));

            handle
                .send(Command::new(
                    DeviceId::new("SEP001122334455").unwrap(),
                    CommandAction::CloseCall { call_id: incoming },
                ))
                .await
                .unwrap();
            let frames = read_until_message(&mut phone, &mut decoder, id::SET_RINGER).await;
            let on_hook = frames
                .iter()
                .position(|frame| {
                    matches!(
                        ServerMessage::decode(frame.clone(), protocol),
                        Ok(ServerMessage::CallState {
                            state: CallState::OnHook,
                            ..
                        })
                    )
                })
                .expect("close did not send OnHook");
            let ringer_off = frames
                .iter()
                .position(|frame| {
                    matches!(
                        ServerMessage::decode(frame.clone(), protocol),
                        Ok(ServerMessage::SetRinger {
                            mode: RingerMode::Off,
                            ..
                        })
                    )
                })
                .expect("close did not stop the ringer");
            assert!(
                on_hook < ringer_off,
                "79x1 phones require OnHook before the final ringer-off indication"
            );

            let mut updated = definition();
            let crate::types::ButtonDefinition::Line(line) = &mut updated.buttons[0] else {
                panic!("expected line button");
            };
            line.label = Some("Updated desk".into());
            handle.reconfigure([updated]).await.unwrap();
            assert!(matches!(
                tokio::time::timeout(Duration::from_secs(1), events.recv()).await,
                Ok(Some(Event::Device(DeviceEvent { session_generation: _, device_id, event: DeviceEventKind::Disconnected {} })))
                    if device_id.as_str() == "SEP001122334455"
            ));

            handle.shutdown().await.unwrap();
            task.await.unwrap().unwrap();
        }
    }

    #[tokio::test]
    async fn injected_streams_enforce_station_transport_requirements() {
        for (requirement, transport, accepted) in [
            (
                StationTransportRequirement::Clear,
                StationTransport::Clear,
                true,
            ),
            (
                StationTransportRequirement::Clear,
                StationTransport::Secure,
                false,
            ),
            (
                StationTransportRequirement::Secure,
                StationTransport::Secure,
                true,
            ),
            (
                StationTransportRequirement::Secure,
                StationTransport::Clear,
                false,
            ),
            (
                StationTransportRequirement::Either,
                StationTransport::Clear,
                true,
            ),
            (
                StationTransportRequirement::Either,
                StationTransport::Secure,
                true,
            ),
        ] {
            let mut station = definition();
            station.transport = requirement;
            let config = ServerConfig {
                bind: "127.0.0.1:0".parse().unwrap(),
                advertised_address: Ipv4Addr::LOCALHOST,
                ..ServerConfig::default()
            };
            let (server, handle, mut events, ingress) =
                Server::with_ingress(config, [station]).unwrap();
            let task = tokio::spawn(server.run());
            let (server_stream, mut phone) = tokio::io::duplex(8_192);
            let peer = SocketAddr::from(([127, 0, 0, 1], 40_000));
            let local = SocketAddr::from(([127, 0, 0, 1], 2_000));
            ingress
                .accept(server_stream, peer, local, transport)
                .await
                .unwrap();
            phone
                .write_all(&register_bytes(ProtocolVersion::V22))
                .await
                .unwrap();

            let mut decoder = FrameDecoder::new();
            let expected = if accepted {
                id::REGISTER_ACK
            } else {
                id::REGISTER_REJECT
            };
            let frames = read_until_message(&mut phone, &mut decoder, expected).await;
            assert!(frames.iter().any(|frame| frame.message_id == expected));
            if accepted {
                assert!(matches!(
                    events.recv().await,
                    Some(Event::Device(DeviceEvent { session_generation: _,
                        event: DeviceEventKind::Registered(registration),
                        ..
                    })) if registration.transport == transport
                ));
            }

            handle.shutdown().await.unwrap();
            task.await.unwrap().unwrap();
        }
    }

    #[tokio::test]
    async fn registration_tokens_apply_transport_priority_parity_and_configured_backoff() {
        let cases = [
            (
                RegistrationFallback::Reject,
                1,
                "SEP001122334455",
                StationTransport::Clear,
                false,
            ),
            (
                RegistrationFallback::ReturnToPrimary,
                1,
                "SEP001122334455",
                StationTransport::Clear,
                true,
            ),
            (
                RegistrationFallback::ReturnToPrimary,
                2,
                "SEP001122334455",
                StationTransport::Clear,
                false,
            ),
            (
                RegistrationFallback::DeviceIdOdd,
                2,
                "SEP001122334455",
                StationTransport::Clear,
                true,
            ),
            (
                RegistrationFallback::DeviceIdEven,
                2,
                "SEP001122334455",
                StationTransport::Clear,
                false,
            ),
        ];
        for (fallback, server_priority, device_id, transport, accepted) in cases {
            let station = definition();
            let config = ServerConfig {
                registration_tokens: RegistrationTokenPolicy {
                    fallback,
                    backoff: Duration::from_secs(75),
                    server_priority,
                },
                ..ServerConfig::default()
            };
            let (server, handle, _events, ingress) =
                Server::with_ingress(config, [station]).unwrap();
            let task = tokio::spawn(server.run());
            let (server_stream, mut phone) = tokio::io::duplex(2_048);
            ingress
                .accept(
                    server_stream,
                    SocketAddr::from(([127, 0, 0, 1], 40_000)),
                    SocketAddr::from(([127, 0, 0, 1], 2_000)),
                    transport,
                )
                .await
                .unwrap();
            phone
                .write_all(
                    &ClientMessage::RegisterToken(crate::message::RegisterTokenMessage {
                        device_id: DeviceId::new(device_id).unwrap(),
                        device_instance: 1,
                        address: IpAddr::V4(Ipv4Addr::LOCALHOST),
                        device_type: DeviceType::from(115),
                        flags: 0,
                    })
                    .encode(ProtocolVersion::V17)
                    .unwrap(),
                )
                .await
                .unwrap();
            let expected = if accepted {
                id::REGISTER_TOKEN_ACK
            } else {
                id::REGISTER_TOKEN_REJECT
            };
            let mut decoder = FrameDecoder::new();
            let frames = read_until_message(&mut phone, &mut decoder, expected).await;
            let response = frames
                .into_iter()
                .find(|frame| frame.message_id == expected)
                .unwrap();
            if accepted {
                assert_eq!(
                    ServerMessage::decode(response, ProtocolVersion::V17).unwrap(),
                    ServerMessage::RegisterTokenAck
                );
            } else {
                assert_eq!(
                    ServerMessage::decode(response, ProtocolVersion::V17).unwrap(),
                    ServerMessage::RegisterTokenReject {
                        backoff_seconds: 75,
                    }
                );
            }
            handle.shutdown().await.unwrap();
            task.await.unwrap().unwrap();
        }

        let mut secure_station = definition();
        secure_station.transport = StationTransportRequirement::Secure;
        let config = ServerConfig {
            registration_tokens: RegistrationTokenPolicy {
                fallback: RegistrationFallback::ReturnToPrimary,
                backoff: Duration::from_secs(90),
                server_priority: 1,
            },
            ..ServerConfig::default()
        };
        let (server, handle, _events, ingress) =
            Server::with_ingress(config, [secure_station]).unwrap();
        let task = tokio::spawn(server.run());
        let (server_stream, mut phone) = tokio::io::duplex(2_048);
        ingress
            .accept(
                server_stream,
                SocketAddr::from(([127, 0, 0, 1], 40_001)),
                SocketAddr::from(([127, 0, 0, 1], 2_000)),
                StationTransport::Clear,
            )
            .await
            .unwrap();
        phone
            .write_all(
                &ClientMessage::RegisterToken(crate::message::RegisterTokenMessage {
                    device_id: DeviceId::new("SEP001122334455").unwrap(),
                    device_instance: 1,
                    address: IpAddr::V4(Ipv4Addr::LOCALHOST),
                    device_type: DeviceType::from(115),
                    flags: 0,
                })
                .encode(ProtocolVersion::V17)
                .unwrap(),
            )
            .await
            .unwrap();
        let mut decoder = FrameDecoder::new();
        let response = read_until_message(&mut phone, &mut decoder, id::REGISTER_TOKEN_REJECT)
            .await
            .into_iter()
            .find(|frame| frame.message_id == id::REGISTER_TOKEN_REJECT)
            .unwrap();
        assert_eq!(
            ServerMessage::decode(response, ProtocolVersion::V17).unwrap(),
            ServerMessage::RegisterTokenReject {
                backoff_seconds: 90,
            }
        );
        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[test]
    fn registration_token_parity_requires_a_canonical_sep_mac_identity() {
        let policy = |fallback| RegistrationTokenPolicy {
            fallback,
            server_priority: 2,
            ..RegistrationTokenPolicy::default()
        };
        assert!(
            policy(RegistrationFallback::DeviceIdOdd)
                .accepts(&DeviceId::new("SEP001122334455").unwrap())
        );
        assert!(
            policy(RegistrationFallback::DeviceIdEven)
                .accepts(&DeviceId::new("SEP001122334454").unwrap())
        );
        for device_id in ["ALICE1", "SEP1", "SEP00112233445Z"] {
            let device_id = DeviceId::new(device_id).unwrap();
            assert!(!policy(RegistrationFallback::DeviceIdOdd).accepts(&device_id));
            assert!(!policy(RegistrationFallback::DeviceIdEven).accepts(&device_id));
        }
        let return_to_primary = RegistrationTokenPolicy {
            fallback: RegistrationFallback::ReturnToPrimary,
            server_priority: 1,
            ..RegistrationTokenPolicy::default()
        };
        assert!(return_to_primary.accepts(&DeviceId::new("ALICE1").unwrap()));
    }

    #[tokio::test]
    async fn duplicate_registration_token_leaves_the_live_session_addressable() {
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            registration_tokens: RegistrationTokenPolicy {
                fallback: RegistrationFallback::ReturnToPrimary,
                backoff: Duration::from_secs(75),
                server_priority: 1,
            },
            ..ServerConfig::default()
        };
        let (server, handle, mut events) = Server::bind(config, [definition()]).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let protocol = ProtocolVersion::V22;
        let device_id = DeviceId::new("SEP001122334455").unwrap();
        let call_id = CallId(7001);

        let mut phone = TcpStream::connect(address).await.unwrap();
        let mut decoder = FrameDecoder::new();
        phone.write_all(&register_bytes(protocol)).await.unwrap();
        read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                event: DeviceEventKind::Registered(_),
                ..
            }))
        ));

        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::BeginCall {
                    line_instance: LineInstance::new(1),
                    call_id,
                    codec: Codec::Pcmu,
                },
            ))
            .await
            .unwrap();
        let wire_call_reference = read_until_message(&mut phone, &mut decoder, id::CALL_STATE)
            .await
            .into_iter()
            .find_map(|frame| match ServerMessage::decode(frame, protocol) {
                Ok(ServerMessage::CallState { call_reference, .. }) => Some(call_reference),
                _ => None,
            })
            .expect("begin call omitted its wire reference");
        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::OpenReceiveChannel {
                    call_id,
                    source: None,
                    codec: Codec::Pcmu,
                    packet_ms: 20,
                    max_frames_per_packet: 1,
                    dtmf_mode: DtmfMode::Skinny,
                    audio_processing: AudioProcessingPolicy::default(),
                },
            ))
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::OPEN_RECEIVE_CHANNEL).await;
        let media_party = open_receive_request_party(&frames, protocol);
        phone
            .write_all(
                &ClientMessage::OpenReceiveChannelAck {
                    status: MediaStatus::Ok,
                    address: IpAddr::V4(Ipv4Addr::LOCALHOST),
                    port: 4000,
                    call_reference: wire_call_reference,
                    passthrough_party_id: media_party,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent { session_generation: _,
                event: DeviceEventKind::ReceiveChannelOpened {
                    call_id: actual_call_id,
                    ..
                },
                ..
            })) if actual_call_id == call_id
        ));

        let mut contender = TcpStream::connect(address).await.unwrap();
        let mut contender_decoder = FrameDecoder::new();
        contender
            .write_all(
                &ClientMessage::RegisterToken(crate::message::RegisterTokenMessage {
                    device_id: device_id.clone(),
                    device_instance: 1,
                    address: IpAddr::V4(Ipv4Addr::LOCALHOST),
                    device_type: DeviceType::from(115),
                    flags: 0,
                })
                .encode(ProtocolVersion::V17)
                .unwrap(),
            )
            .await
            .unwrap();
        let response = read_until_message(
            &mut contender,
            &mut contender_decoder,
            id::REGISTER_TOKEN_REJECT,
        )
        .await
        .into_iter()
        .find(|frame| frame.message_id == id::REGISTER_TOKEN_REJECT)
        .unwrap();
        assert_eq!(
            ServerMessage::decode(response, ProtocolVersion::V17).unwrap(),
            ServerMessage::RegisterTokenReject {
                backoff_seconds: 75,
            }
        );

        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::SetCallState {
                    call_id,
                    state: CallState::Connected,
                },
            ))
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::CALL_STATE).await;
        assert!(frames.into_iter().any(|frame| matches!(
            ServerMessage::decode(frame, protocol),
            Ok(ServerMessage::CallState {
                state: CallState::Connected,
                ..
            })
        )));
        handle
            .send_confirmed(Command::new(
                device_id,
                CommandAction::CloseReceiveChannel { call_id },
            ))
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, id::CLOSE_RECEIVE_CHANNEL).await;
        phone
            .write_all(&ClientMessage::KeepAlive.encode(protocol).unwrap())
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, id::KEEP_ALIVE_ACK).await;

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn server_list_selects_ordered_endpoints_for_the_active_transport() {
        let config = ServerConfig {
            signaling_servers: vec![
                SignalingServerRoute {
                    priority: 2,
                    name: "backup".into(),
                    address: "192.0.2.20".parse().unwrap(),
                    clear_port: NonZeroU16::new(2001),
                    secure_port: None,
                },
                SignalingServerRoute {
                    priority: 1,
                    name: "primary".into(),
                    address: "192.0.2.10".parse().unwrap(),
                    clear_port: NonZeroU16::new(2000),
                    secure_port: NonZeroU16::new(2443),
                },
            ],
            ..ServerConfig::default()
        };
        let (server, handle, mut events, ingress) =
            Server::with_ingress(config, [definition()]).unwrap();
        let task = tokio::spawn(server.run());

        for (index, transport, expected) in [
            (
                0,
                StationTransport::Clear,
                vec![
                    SignalingServerEndpoint {
                        name: "primary".into(),
                        address: "192.0.2.10".parse().unwrap(),
                        port: NonZeroU16::new(2000).unwrap(),
                    },
                    SignalingServerEndpoint {
                        name: "backup".into(),
                        address: "192.0.2.20".parse().unwrap(),
                        port: NonZeroU16::new(2001).unwrap(),
                    },
                ],
            ),
            (
                1,
                StationTransport::Secure,
                vec![SignalingServerEndpoint {
                    name: "primary".into(),
                    address: "192.0.2.10".parse().unwrap(),
                    port: NonZeroU16::new(2443).unwrap(),
                }],
            ),
        ] {
            let (server_stream, mut phone) = tokio::io::duplex(8_192);
            ingress
                .accept(
                    server_stream,
                    SocketAddr::from(([127, 0, 0, 1], 40_000 + index)),
                    SocketAddr::from(([127, 0, 0, 1], if index == 0 { 2_000 } else { 2_443 })),
                    transport,
                )
                .await
                .unwrap();
            let protocol = ProtocolVersion::V22;
            phone.write_all(&register_bytes(protocol)).await.unwrap();
            let mut decoder = FrameDecoder::new();
            read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
            loop {
                if matches!(
                    events.recv().await,
                    Some(Event::Device(DeviceEvent {
                        session_generation: _,
                        event: DeviceEventKind::Registered(_),
                        ..
                    }))
                ) {
                    break;
                }
            }
            phone
                .write_all(&ClientMessage::ServerRequest.encode(protocol).unwrap())
                .await
                .unwrap();
            let response = read_until_message(&mut phone, &mut decoder, id::SERVER_RES)
                .await
                .into_iter()
                .find(|frame| frame.message_id == id::SERVER_RES)
                .unwrap();
            assert_eq!(
                ServerMessage::decode(response, protocol).unwrap(),
                ServerMessage::ServerResponse { servers: expected }
            );
        }

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn server_list_never_empties_when_routes_do_not_fit_the_session() {
        let config = ServerConfig {
            advertised_address: "192.0.2.99".parse().unwrap(),
            signaling_servers: vec![SignalingServerRoute {
                priority: 1,
                name: "secure-v6".into(),
                address: "2001:db8::20".parse().unwrap(),
                clear_port: None,
                secure_port: NonZeroU16::new(2443),
            }],
            ..ServerConfig::default()
        };
        let (server, handle, mut events, ingress) =
            Server::with_ingress(config, [definition()]).unwrap();
        let task = tokio::spawn(server.run());

        for (offset, transport, local_address, expected_address) in [
            (
                0,
                StationTransport::Clear,
                "192.0.2.30".parse().unwrap(),
                "192.0.2.30".parse().unwrap(),
            ),
            (
                1,
                StationTransport::Secure,
                "192.0.2.31".parse().unwrap(),
                "192.0.2.31".parse().unwrap(),
            ),
            (
                2,
                StationTransport::Secure,
                "2001:db8::30".parse().unwrap(),
                "192.0.2.99".parse().unwrap(),
            ),
        ] {
            let local = SocketAddr::new(
                local_address,
                if transport == StationTransport::Clear {
                    2000
                } else {
                    2443
                },
            );
            let (server_stream, mut phone) = tokio::io::duplex(8_192);
            ingress
                .accept(
                    server_stream,
                    SocketAddr::from(([127, 0, 0, 1], 41_000 + offset)),
                    local,
                    transport,
                )
                .await
                .unwrap();
            let protocol = ProtocolVersion::V3;
            phone.write_all(&register_bytes(protocol)).await.unwrap();
            let mut decoder = FrameDecoder::new();
            read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
            while !matches!(
                events.recv().await,
                Some(Event::Device(DeviceEvent {
                    session_generation: _,
                    event: DeviceEventKind::Registered(_),
                    ..
                }))
            ) {}
            phone
                .write_all(&ClientMessage::ServerRequest.encode(protocol).unwrap())
                .await
                .unwrap();
            let response = read_until_message(&mut phone, &mut decoder, id::SERVER_RES)
                .await
                .into_iter()
                .find(|frame| frame.message_id == id::SERVER_RES)
                .unwrap();
            assert_eq!(
                ServerMessage::decode(response, protocol).unwrap(),
                ServerMessage::ServerResponse {
                    servers: vec![SignalingServerEndpoint {
                        name: "sccp-protocol".into(),
                        address: expected_address,
                        port: NonZeroU16::new(local.port()).unwrap(),
                    }]
                }
            );
        }

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn secondary_sessions_use_the_secondary_keepalive_deadline() {
        let config = ServerConfig {
            keepalive_seconds: 5,
            secondary_keepalive_seconds: 20,
            registration_tokens: RegistrationTokenPolicy {
                server_priority: 2,
                ..RegistrationTokenPolicy::default()
            },
            ..ServerConfig::default()
        };
        let (server, handle, mut events, ingress) =
            Server::with_ingress(config, [definition()]).unwrap();
        let task = tokio::spawn(server.run());
        let (server_stream, mut phone) = tokio::io::duplex(8_192);
        ingress
            .accept(
                server_stream,
                SocketAddr::from(([127, 0, 0, 1], 40_000)),
                SocketAddr::from(([127, 0, 0, 1], 2_000)),
                StationTransport::Clear,
            )
            .await
            .unwrap();
        let protocol = ProtocolVersion::V22;
        phone.write_all(&register_bytes(protocol)).await.unwrap();
        let mut decoder = FrameDecoder::new();
        let frames = read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
        let acknowledgement = frames
            .into_iter()
            .find(|frame| frame.message_id == id::REGISTER_ACK)
            .unwrap();
        assert_eq!(
            ServerMessage::decode(acknowledgement, protocol).unwrap(),
            ServerMessage::RegisterAck {
                keepalive_seconds: 5,
                secondary_keepalive_seconds: 20,
                protocol,
                features: PhoneFeatures::empty(),
                date_template: Default::default(),
            }
        );
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                event: DeviceEventKind::Registered(_),
                ..
            }))
        ));

        tokio::time::advance(Duration::from_secs(59)).await;
        tokio::task::yield_now().await;
        assert!(events.try_recv().is_err());
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                event: DeviceEventKind::Disconnected {},
                ..
            }))
        ));

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[derive(Debug)]
    struct RecordingSocketQos {
        applied: Arc<std::sync::Mutex<Vec<SignalingQos>>>,
        fail: bool,
    }

    impl StationSocketQos for RecordingSocketQos {
        fn apply(&self, qos: SignalingQos) -> SocketQosReport {
            self.applied.lock().unwrap().push(qos);
            if self.fail {
                SocketQosReport::failed(
                    SocketQosMark::SocketPriority,
                    std::io::Error::new(std::io::ErrorKind::Unsupported, "test platform"),
                )
            } else {
                SocketQosReport::default()
            }
        }
    }

    #[tokio::test]
    async fn registration_applies_device_socket_qos_without_making_failure_fatal() {
        let baseline = SignalingQos::new(8, 1);
        let device_policy = SignalingQos::new(26, 5);
        let mut station = definition();
        station.signaling_qos = Some(device_policy);
        let config = ServerConfig {
            signaling_qos: baseline,
            ..ServerConfig::default()
        };
        let (server, handle, mut events, ingress) =
            Server::with_ingress(config, [station]).unwrap();
        let task = tokio::spawn(server.run());
        let (server_stream, mut phone) = tokio::io::duplex(8_192);
        let applied = Arc::new(std::sync::Mutex::new(Vec::new()));
        ingress
            .accept_with_socket_qos(
                server_stream,
                SocketAddr::from(([127, 0, 0, 1], 40_000)),
                SocketAddr::from(([127, 0, 0, 1], 2_000)),
                StationTransport::Clear,
                RecordingSocketQos {
                    applied: Arc::clone(&applied),
                    fail: true,
                },
            )
            .await
            .unwrap();
        phone
            .write_all(&register_bytes(ProtocolVersion::V22))
            .await
            .unwrap();

        let mut decoder = FrameDecoder::new();
        let frames = read_until_message(&mut phone, &mut decoder, id::REGISTER_ACK).await;
        assert!(
            frames
                .iter()
                .any(|frame| frame.message_id == id::REGISTER_ACK)
        );
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                event: DeviceEventKind::Registered(_),
                ..
            }))
        ));
        assert_eq!(*applied.lock().unwrap(), vec![baseline, device_policy]);

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn headset_and_accessory_changes_are_typed_and_duplicate_stable() {
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let (server, handle, mut events) = Server::bind(config, [definition()]).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let mut phone = TcpStream::connect(address).await.unwrap();
        let mut decoder = FrameDecoder::new();
        let protocol = ProtocolVersion::V22;

        phone.write_all(&register_bytes(protocol)).await.unwrap();
        read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Registered(_)
            }))
        ));
        phone
            .write_all(
                &ClientMessage::HeadsetStatus { enabled: true }
                    .encode(protocol)
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::HeadsetStatusChanged { enabled: true, .. }
            }))
        ));
        phone
            .write_all(
                &ClientMessage::MediaPathEvent {
                    path: crate::MediaPathId::Speaker,
                    event: crate::MediaPathEvent::On,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::MediaPathChanged {
                    path: crate::MediaPathId::Speaker,
                    event: crate::MediaPathEvent::On,
                    ..
                }
            }))
        ));
        phone
            .write_all(
                &ClientMessage::MediaPathEvent {
                    path: crate::MediaPathId::Speaker,
                    event: crate::MediaPathEvent::On,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), events.recv())
                .await
                .is_err()
        );

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn ipv6_signaling_requires_extended_layouts_and_preserves_station_addresses() {
        let config = ServerConfig {
            bind: "[::1]:0".parse().unwrap(),
            ..ServerConfig::default()
        };
        let (server, handle, mut events) = Server::bind(config, [definition()]).await.unwrap();
        let address = server.local_addr().unwrap();
        assert!(address.is_ipv6());
        let task = tokio::spawn(server.run());

        let mut legacy = TcpStream::connect(address).await.unwrap();
        legacy
            .write_all(&register_bytes(ProtocolVersion::V3))
            .await
            .unwrap();
        let mut legacy_decoder = FrameDecoder::new();
        let rejection = read_until_message(&mut legacy, &mut legacy_decoder, id::REGISTER_REJECT)
            .await
            .into_iter()
            .find(|frame| frame.message_id == id::REGISTER_REJECT)
            .unwrap();
        assert!(matches!(
            ServerMessage::decode(rejection, ProtocolVersion::V3).unwrap(),
            ServerMessage::RegisterReject { reason } if reason == "IPv6 requires protocol v17"
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(25), events.recv())
                .await
                .is_err()
        );

        let protocol = ProtocolVersion::V22;
        let reported_ipv6: Ipv6Addr = "2001:db8::42".parse().unwrap();
        let registration = ClientMessage::Register(RegistrationMessage {
            device_id: DeviceId::new("SEP001122334455").unwrap(),
            reported_address: None,
            reported_ipv6_address: Some(reported_ipv6),
            device_type: DeviceType::Cisco7962,
            advertised_protocol: protocol.wire(),
            features: PhoneFeatures::empty(),
            firmware: "test-load".into(),
            configuration_version_stamp: crate::message::BoundedBytes::default(),
            wire: None,
        });
        let mut phone = TcpStream::connect(address).await.unwrap();
        phone
            .write_all(&registration.encode(protocol).unwrap())
            .await
            .unwrap();
        let mut decoder = FrameDecoder::new();
        read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent { session_generation: _, device_id: _, event: DeviceEventKind::Registered(DeviceRegistration {
                peer,
                transport: StationTransport::Clear,
                reported_address: None,
                reported_ipv6_address: Some(reported),
                ..
            }),
                ..
            })) if peer.is_ipv6() && reported == reported_ipv6
        ));

        phone
            .write_all(&ClientMessage::ServerRequest.encode(protocol).unwrap())
            .await
            .unwrap();
        let response = read_until_message(&mut phone, &mut decoder, id::SERVER_RES)
            .await
            .into_iter()
            .find(|frame| frame.message_id == id::SERVER_RES)
            .unwrap();
        assert_eq!(
            ServerMessage::decode(response, protocol).unwrap(),
            ServerMessage::ServerResponse {
                servers: vec![SignalingServerEndpoint {
                    name: "sccp-protocol".into(),
                    address: address.ip(),
                    port: NonZeroU16::new(address.port()).unwrap(),
                }],
            }
        );

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[test]
    fn status_messages_preserve_persistence_timeout_priority_and_phone_family() {
        let mut persistent = false;
        assert!(matches!(
            status_message_frames(
                HandsetStatusMessage::Display {
                    text: "Persistent".into(),
                    timeout_seconds: 0,
                    priority: None,
                },
                DeviceType::Cisco7960,
                &mut persistent,
            )
            .as_slice(),
            [ServerMessage::DisplayPrompt {
                timeout_seconds: 0,
                line_instance: 0,
                call_reference: 0,
                text,
            }] if text == "Persistent"
        ));
        assert!(persistent);
        assert_eq!(
            status_message_frames(
                HandsetStatusMessage::Clear { priority: None },
                DeviceType::Cisco7960,
                &mut persistent,
            ),
            [
                ServerMessage::ClearPrompt {
                    line_instance: 0,
                    call_reference: 0,
                },
                ServerMessage::ClearPriorityNotify {
                    priority: NotificationPriority::Timed,
                },
            ]
        );
        assert!(!persistent);

        assert!(matches!(
            status_message_frames(
                HandsetStatusMessage::Display {
                    text: "Timed".into(),
                    timeout_seconds: 9,
                    priority: None,
                },
                DeviceType::Cisco7960,
                &mut persistent,
            )
            .as_slice(),
            [ServerMessage::DisplayPriorityNotify {
                timeout_seconds: 9,
                priority: NotificationPriority::Timed,
                text,
            }] if text == "Timed"
        ));
        assert!(matches!(
            status_message_frames(
                HandsetStatusMessage::Display {
                    text: "Timed".into(),
                    timeout_seconds: 9,
                    priority: None,
                },
                DeviceType::Cisco6945,
                &mut persistent,
            )
            .as_slice(),
            [ServerMessage::DisplayPrompt {
                timeout_seconds: 9,
                line_instance: 0,
                call_reference: 0,
                ..
            }]
        ));
    }

    #[test]
    fn every_status_priority_round_trips_through_typed_frames() {
        for priority in NotificationPriority::ALL_KNOWN {
            let mut persistent = false;
            assert_eq!(
                status_message_frames(
                    HandsetStatusMessage::Display {
                        text: "Priority".into(),
                        timeout_seconds: 5,
                        priority: Some(*priority),
                    },
                    DeviceType::Cisco7960,
                    &mut persistent,
                ),
                [ServerMessage::DisplayPriorityNotify {
                    timeout_seconds: 5,
                    priority: *priority,
                    text: "Priority".into(),
                }]
            );
            assert_eq!(
                status_message_frames(
                    HandsetStatusMessage::Clear {
                        priority: Some(*priority),
                    },
                    DeviceType::Cisco7960,
                    &mut persistent,
                ),
                [ServerMessage::ClearPriorityNotify {
                    priority: *priority,
                }]
            );
        }
    }

    #[test]
    fn text_service_delivery_types_priority_and_segments_only_modern_documents() {
        let short = CiscoIpPhoneText::new("Sender", "Read", "Hello & goodbye").unwrap();
        let legacy = text_service_messages(
            LineInstance::new(3),
            CallReference::new(71),
            TransactionId::new(99),
            PhoneServicePriority::NORMAL,
            &short,
            ProtocolVersion::V17,
        )
        .unwrap();
        assert!(matches!(
            legacy.as_slice(),
            [ServerMessage::UserToDeviceDataV1(message)]
                if message.application_id == PHONE_TEXT_APPLICATION_ID
                    && message.line_instance == 3
                    && message.call_reference == 71
                    && message.transaction_id == 99
                    && message.sequence_flag == 2
                    && message.display_priority == 1
                    && message.conference_id == 71
                    && message.application_instance_id == PHONE_TEXT_APPLICATION_ID
                    && message.routing == 1
                    && CiscoIpPhoneText::from_xml(&message.data).unwrap() == short
        ));

        let legacy_oversized = CiscoIpPhoneText::new(
            "Sender",
            "Read",
            "x".repeat(PHONE_TEXT_LEGACY_MAX_CHARS + 1),
        )
        .unwrap();
        assert!(matches!(
            text_service_messages(
                LineInstance::new(0),
                CallReference::new(1),
                TransactionId::new(1),
                PhoneServicePriority::LOW,
                &legacy_oversized,
                ProtocolVersion::V17,
            ),
            Err(ServerError::PhoneXml(PhoneXmlError::InvalidField {
                field: "legacy phone text body",
                ..
            }))
        ));

        let modern = CiscoIpPhoneText::new("Sender", "Read", "&".repeat(3_000)).unwrap();
        let messages = text_service_messages(
            LineInstance::new(0),
            CallReference::new(1),
            TransactionId::new(100),
            PhoneServicePriority::HIGH,
            &modern,
            ProtocolVersion::V18,
        )
        .unwrap();
        assert!(messages.len() > 2);
        let mut reassembled = Vec::new();
        for (index, message) in messages.iter().enumerate() {
            let ServerMessage::UserToDeviceDataV1(message) = message else {
                panic!("expected text application-data segment");
            };
            assert!(message.data.len() <= 2_000);
            assert_eq!(message.display_priority, 2);
            assert_eq!(
                message.sequence_flag,
                if index == 0 {
                    0
                } else if index + 1 == messages.len() {
                    2
                } else {
                    1
                }
            );
            reassembled.extend_from_slice(&message.data);
        }
        assert_eq!(CiscoIpPhoneText::from_xml(&reassembled).unwrap(), modern);
    }

    #[tokio::test]
    async fn registered_phone_receives_typed_text_service_controls_and_priority() {
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let (server, handle, mut events) = Server::bind(config, [definition()]).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let mut phone = TcpStream::connect(address).await.unwrap();
        let mut decoder = FrameDecoder::new();
        let protocol = ProtocolVersion::V22;
        let device_id = DeviceId::new("SEP001122334455").unwrap();

        phone.write_all(&register_bytes(protocol)).await.unwrap();
        read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Registered(_)
            }))
        ));

        let mut expected =
            CiscoIpPhoneText::new("Dispatch", "Read", "Café <ready> & waiting").unwrap();
        expected.soft_keys.push(CiscoIpPhoneSoftKeyItem {
            name: Some("Refresh".into()),
            position: PhoneSoftKeyPosition::new(1).unwrap(),
            url: Some("https://pbx.example/text?id=7&view=full".into()),
            url_down: None,
        });
        handle
            .send(Command::new(
                device_id.clone(),
                CommandAction::ShowTextService {
                    line_instance: LineInstance::new(2),
                    call_reference: CallReference::new(42),
                    transaction_id: TransactionId::new(73),
                    priority: PhoneServicePriority::HIGH,
                    document: expected.clone(),
                },
            ))
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::USER_TO_DEVICE_DATA_V1).await;
        let message = frames
            .into_iter()
            .find(|frame| frame.message_id == id::USER_TO_DEVICE_DATA_V1)
            .map(|frame| ServerMessage::decode(frame, protocol).unwrap())
            .unwrap();
        let ServerMessage::UserToDeviceDataV1(message) = message else {
            panic!("expected text application data");
        };
        assert_eq!(message.application_id, PHONE_TEXT_APPLICATION_ID);
        assert_eq!(message.line_instance, 2);
        assert_eq!(message.call_reference, 42);
        assert_eq!(message.transaction_id, 73);
        assert_eq!(message.sequence_flag, 2);
        assert_eq!(message.display_priority, 2);
        assert_eq!(CiscoIpPhoneText::from_xml(&message.data).unwrap(), expected);

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[test]
    fn input_service_delivery_preserves_typed_fields_and_modern_segmentation() {
        let short = CiscoIpPhoneInput::new(
            "Invite",
            "Enter number",
            "conference/44/invite",
            vec![CiscoIpPhoneInputItem {
                display_name: Some("Number".into()),
                parameter: PhoneInputParameterName::new("NUMBER").unwrap(),
                flags: PhoneInputFlags::Telephone,
                default_value: Some("5550100".into()),
            }],
        )
        .unwrap();
        let legacy = input_service_messages(
            LineInstance::new(3),
            CallReference::new(71),
            ApplicationId::new(9_092),
            TransactionId::new(99),
            PhoneServicePriority::NORMAL,
            &short,
            ProtocolVersion::V17,
        )
        .unwrap();
        assert!(matches!(
            legacy.as_slice(),
            [ServerMessage::UserToDeviceDataV1(message)]
                if message.application_id == 9_092
                    && message.line_instance == 3
                    && message.call_reference == 71
                    && message.transaction_id == 99
                    && message.sequence_flag == 2
                    && message.display_priority == 1
                    && message.conference_id == 71
                    && message.application_instance_id == 9_092
                    && message.routing == 1
                    && CiscoIpPhoneInput::from_xml(&message.data).unwrap() == short
        ));

        let mut large = short;
        large.key_items = (0..32)
            .map(|index| CiscoIpPhoneKeyItem {
                key: PhoneXmlKey::NavBack,
                url: Some(format!("{}-{index:02}", "x".repeat(252))),
                url_down: Some(format!("{}-{index:02}", "y".repeat(252))),
            })
            .collect();
        assert!(matches!(
            input_service_messages(
                LineInstance::new(3),
                CallReference::new(71),
                ApplicationId::new(9_092),
                TransactionId::new(100),
                PhoneServicePriority::HIGH,
                &large,
                ProtocolVersion::V17,
            ),
            Err(ServerError::PhoneXml(PhoneXmlError::LimitExceeded {
                maximum: 2_000,
                ..
            }))
        ));
        let messages = input_service_messages(
            LineInstance::new(3),
            CallReference::new(71),
            ApplicationId::new(9_092),
            TransactionId::new(100),
            PhoneServicePriority::HIGH,
            &large,
            ProtocolVersion::V18,
        )
        .unwrap();
        assert!(messages.len() > 2);
        let mut reassembled = Vec::new();
        for (index, message) in messages.iter().enumerate() {
            let ServerMessage::UserToDeviceDataV1(message) = message else {
                panic!("expected input application-data segment");
            };
            assert!(message.data.len() <= 2_000);
            assert_eq!(message.application_id, 9_092);
            assert_eq!(message.display_priority, 2);
            assert_eq!(
                message.sequence_flag,
                if index == 0 {
                    0
                } else if index + 1 == messages.len() {
                    2
                } else {
                    1
                }
            );
            reassembled.extend_from_slice(&message.data);
        }
        assert_eq!(CiscoIpPhoneInput::from_xml(&reassembled).unwrap(), large);
    }

    #[tokio::test]
    async fn registered_phone_receives_typed_input_and_returns_ordered_submission() {
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let (server, handle, mut events) = Server::bind(config, [definition()]).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let mut phone = TcpStream::connect(address).await.unwrap();
        let mut decoder = FrameDecoder::new();
        let protocol = ProtocolVersion::V22;
        let device_id = DeviceId::new("SEP001122334455").unwrap();

        phone.write_all(&register_bytes(protocol)).await.unwrap();
        read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Registered(_)
            }))
        ));

        let mut expected = CiscoIpPhoneInput::new(
            "Invite <guest>",
            "Enter details",
            "conference/44/invite",
            vec![
                CiscoIpPhoneInputItem {
                    display_name: Some("Number".into()),
                    parameter: PhoneInputParameterName::new("NUMBER").unwrap(),
                    flags: PhoneInputFlags::Telephone,
                    default_value: None,
                },
                CiscoIpPhoneInputItem {
                    display_name: Some("Name".into()),
                    parameter: PhoneInputParameterName::new("NAME").unwrap(),
                    flags: PhoneInputFlags::Alphabetic,
                    default_value: Some("François".into()),
                },
            ],
        )
        .unwrap();
        expected.soft_keys.push(CiscoIpPhoneSoftKeyItem {
            name: Some("Submit".into()),
            position: PhoneSoftKeyPosition::new(1).unwrap(),
            url: Some("SoftKey:Submit".into()),
            url_down: None,
        });
        handle
            .send(Command::new(
                device_id.clone(),
                CommandAction::ShowInputService {
                    line_instance: LineInstance::new(2),
                    call_reference: CallReference::new(42),
                    application_id: ApplicationId::new(9_092),
                    transaction_id: TransactionId::new(73),
                    priority: PhoneServicePriority::HIGH,
                    document: expected.clone(),
                },
            ))
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::USER_TO_DEVICE_DATA_V1).await;
        let message = frames
            .into_iter()
            .find(|frame| frame.message_id == id::USER_TO_DEVICE_DATA_V1)
            .map(|frame| ServerMessage::decode(frame, protocol).unwrap())
            .unwrap();
        let ServerMessage::UserToDeviceDataV1(message) = message else {
            panic!("expected input application data");
        };
        assert_eq!(message.application_id, 9_092);
        assert_eq!(message.line_instance, 2);
        assert_eq!(message.call_reference, 42);
        assert_eq!(message.transaction_id, 73);
        assert_eq!(message.sequence_flag, 2);
        assert_eq!(message.display_priority, 2);
        assert_eq!(
            CiscoIpPhoneInput::from_xml(&message.data).unwrap(),
            expected
        );

        phone
            .write_all(
                &ClientMessage::DeviceToUserDataV1(UserDataV1Message {
                    application_id: 9_092,
                    line_instance: 2,
                    call_reference: 42,
                    transaction_id: 73,
                    sequence_flag: 2,
                    display_priority: 2,
                    conference_id: 42,
                    application_instance_id: 9_092,
                    routing: 1,
                    data: b"conference/44/invite?NUMBER=555%2A12&NAME=Fran%C3%A7ois".to_vec(),
                })
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        let Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::PhoneServiceResponse { response, .. },
        })) = events.recv().await
        else {
            panic!("expected typed input submission");
        };
        assert_eq!(response.routing.application_id, ApplicationId::new(9_092));
        assert_eq!(response.routing.line_instance, LineInstance::new(2));
        assert_eq!(response.routing.call_reference, CallReference::new(42));
        assert_eq!(response.routing.transaction_id, TransactionId::new(73));
        let PhoneServicePayload::Submission(submission) = response.payload else {
            panic!("expected typed input submission payload");
        };
        assert_eq!(submission.route, ["conference", "44", "invite"]);
        assert_eq!(
            submission.values_named("NUMBER").collect::<Vec<_>>(),
            ["555*12"]
        );
        assert_eq!(
            submission.values_named("NAME").collect::<Vec<_>>(),
            ["François"]
        );

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[test]
    fn execute_action_delivery_preserves_envelope_order_and_protocol_bounds() {
        let short = CiscoIpPhoneExecute::new(vec![
            CiscoIpPhoneExecuteItem::with_priority(
                "Key:Directories?view=all&side=west",
                PhoneExecutePriority::LOW,
            )
            .unwrap(),
            CiscoIpPhoneExecuteItem::new("Application:PlacedCalls").unwrap(),
        ])
        .unwrap();
        let legacy = execute_phone_action_messages(
            LineInstance::new(3),
            CallReference::new(71),
            ApplicationId::new(9_093),
            TransactionId::new(99),
            PhoneServicePriority::NORMAL,
            &short,
            ProtocolVersion::V17,
        )
        .unwrap();
        assert!(matches!(
            legacy.as_slice(),
            [ServerMessage::UserToDeviceDataV1(message)]
                if message.application_id == 9_093
                    && message.line_instance == 3
                    && message.call_reference == 71
                    && message.transaction_id == 99
                    && message.sequence_flag == 2
                    && message.display_priority == 1
                    && message.routing == 1
                    && CiscoIpPhoneExecute::from_xml(&message.data).unwrap() == short
        ));

        let large = CiscoIpPhoneExecute::new(
            (0..PHONE_EXECUTE_MAX_ITEMS)
                .map(|_| {
                    CiscoIpPhoneExecuteItem::with_priority(
                        "\"".repeat(256),
                        PhoneExecutePriority::HIGH,
                    )
                    .unwrap()
                })
                .collect(),
        )
        .unwrap();
        assert!(matches!(
            execute_phone_action_messages(
                LineInstance::new(3),
                CallReference::new(71),
                ApplicationId::new(9_093),
                TransactionId::new(100),
                PhoneServicePriority::HIGH,
                &large,
                ProtocolVersion::V17,
            ),
            Err(ServerError::PhoneXml(PhoneXmlError::LimitExceeded {
                maximum: 2_000,
                ..
            }))
        ));
        let messages = execute_phone_action_messages(
            LineInstance::new(3),
            CallReference::new(71),
            ApplicationId::new(9_093),
            TransactionId::new(100),
            PhoneServicePriority::HIGH,
            &large,
            ProtocolVersion::V18,
        )
        .unwrap();
        assert!(messages.len() > 2);
        let mut reassembled = Vec::new();
        for (index, message) in messages.iter().enumerate() {
            let ServerMessage::UserToDeviceDataV1(message) = message else {
                panic!("expected execute application-data segment");
            };
            assert!(message.data.len() <= 2_000);
            assert_eq!(message.display_priority, 2);
            assert_eq!(
                message.sequence_flag,
                if index == 0 {
                    0
                } else if index + 1 == messages.len() {
                    2
                } else {
                    1
                }
            );
            reassembled.extend_from_slice(&message.data);
        }
        assert_eq!(CiscoIpPhoneExecute::from_xml(&reassembled).unwrap(), large);
    }

    #[test]
    fn image_service_delivery_preserves_family_envelope_and_protocol_bounds() {
        let short = PhoneImageDocument::ImageFile(CiscoIpPhoneImageFile {
            keypad_target: None,
            application_id: Some("maps".into()),
            on_focus_lost: None,
            on_focus_gained: None,
            on_minimized: None,
            on_closed: Some("Notify:maps/closed".into()),
            title: Some("Floor map".into()),
            prompt: Some("Inspect".into()),
            soft_keys: Vec::new(),
            key_items: Vec::new(),
            location_x: Some(-1),
            location_y: Some(167),
            url: PhoneImageUrl::new("https://pbx.example/map.png?floor=2&site=east").unwrap(),
        });
        let legacy = image_service_messages(
            LineInstance::new(3),
            CallReference::new(71),
            ApplicationId::new(9_095),
            TransactionId::new(101),
            PhoneServicePriority::NORMAL,
            &short,
            ProtocolVersion::V17,
        )
        .unwrap();
        assert!(matches!(
            legacy.as_slice(),
            [ServerMessage::UserToDeviceDataV1(message)]
                if message.application_id == 9_095
                    && message.line_instance == 3
                    && message.call_reference == 71
                    && message.transaction_id == 101
                    && message.sequence_flag == 2
                    && message.display_priority == 1
                    && message.routing == 1
                    && PhoneImageDocument::from_xml(&message.data).unwrap() == short
        ));

        let large = PhoneImageDocument::GraphicFileMenu(CiscoIpPhoneGraphicFileMenu {
            keypad_target: None,
            application_id: Some("map-regions".into()),
            on_focus_lost: None,
            on_focus_gained: None,
            on_minimized: None,
            on_closed: None,
            title: Some("Map regions".into()),
            prompt: Some("Choose".into()),
            soft_keys: Vec::new(),
            key_items: Vec::new(),
            location_x: Some(0),
            location_y: Some(0),
            url: PhoneImageUrl::new("https://pbx.example/map.png").unwrap(),
            items: (0..crate::phone::xml::PHONE_GRAPHIC_FILE_MENU_MAX_ITEMS)
                .map(|index| CiscoIpPhoneTouchAreaMenuItem {
                    name: Some(format!("Region {index}")),
                    url: Some("x".repeat(256)),
                    touch_area: Some(PhoneTouchArea {
                        x1: index as u16,
                        y1: index as u16,
                        x2: index as u16 + 1,
                        y2: index as u16 + 1,
                    }),
                })
                .collect(),
        });
        assert!(matches!(
            image_service_messages(
                LineInstance::new(3),
                CallReference::new(71),
                ApplicationId::new(9_095),
                TransactionId::new(102),
                PhoneServicePriority::HIGH,
                &large,
                ProtocolVersion::V17,
            ),
            Err(ServerError::PhoneXml(PhoneXmlError::LimitExceeded {
                maximum: 2_000,
                ..
            }))
        ));
        let messages = image_service_messages(
            LineInstance::new(3),
            CallReference::new(71),
            ApplicationId::new(9_095),
            TransactionId::new(102),
            PhoneServicePriority::HIGH,
            &large,
            ProtocolVersion::V18,
        )
        .unwrap();
        assert!(messages.len() > 2);
        let mut reassembled = Vec::new();
        for (index, message) in messages.iter().enumerate() {
            let ServerMessage::UserToDeviceDataV1(message) = message else {
                panic!("expected image application-data segment");
            };
            assert!(message.data.len() <= 2_000);
            assert_eq!(message.display_priority, 2);
            assert_eq!(
                message.sequence_flag,
                if index == 0 {
                    0
                } else if index + 1 == messages.len() {
                    2
                } else {
                    1
                }
            );
            reassembled.extend_from_slice(&message.data);
        }
        assert_eq!(PhoneImageDocument::from_xml(&reassembled).unwrap(), large);
    }

    #[test]
    fn background_control_delivery_uses_reserved_application_envelope_and_typed_xml() {
        let set = CiscoIpPhoneSetBackground::new(
            PhoneBackgroundHttpUrl::new("http://pbx.example/background.png?site=east").unwrap(),
            PhoneBackgroundHttpUrl::new("http://pbx.example/background-thumb.png").unwrap(),
        );
        let message = background_control_message(
            TransactionId::new(107),
            &PhoneBackgroundControlDocument::Set(set.clone()),
        )
        .unwrap();
        assert!(matches!(
            message,
            ServerMessage::UserToDeviceDataV1(message)
                if message.application_id == PHONE_BACKGROUND_APPLICATION_ID
                    && message.line_instance == 0
                    && message.call_reference == 0
                    && message.transaction_id == 107
                    && message.sequence_flag == 2
                    && message.display_priority == 0
                    && message.conference_id == 0
                    && message.application_instance_id == PHONE_BACKGROUND_APPLICATION_ID
                    && message.routing == 1
                    && CiscoIpPhoneSetBackground::from_xml(&message.data).unwrap() == set
        ));

        let preview = CiscoIpPhoneSetBackgroundPreview::new(
            PhoneBackgroundHttpUrl::new("http://pbx.example/background.png").unwrap(),
        );
        let message = background_control_message(
            TransactionId::new(108),
            &PhoneBackgroundControlDocument::Preview(preview.clone()),
        )
        .unwrap();
        assert!(matches!(
            message,
            ServerMessage::UserToDeviceDataV1(message)
                if message.application_id == PHONE_BACKGROUND_APPLICATION_ID
                    && message.transaction_id == 108
                    && CiscoIpPhoneSetBackgroundPreview::from_xml(&message.data).unwrap() == preview
        ));
    }

    #[test]
    fn ringtone_control_delivery_uses_reserved_application_envelope_and_typed_xml() {
        let document = CiscoIpPhoneSetRingTone::new(
            PhoneRingtoneUrl::new("http://pbx.example/ringtones/Classic.raw?locale=sv").unwrap(),
        );
        let message = ringtone_control_message(TransactionId::new(111), &document).unwrap();
        assert!(matches!(
            message,
            ServerMessage::UserToDeviceDataV1(message)
                if message.application_id == PHONE_RINGTONE_APPLICATION_ID
                    && message.line_instance == 0
                    && message.call_reference == 0
                    && message.transaction_id == 111
                    && message.sequence_flag == 2
                    && message.display_priority == 0
                    && message.conference_id == 0
                    && message.application_instance_id == PHONE_RINGTONE_APPLICATION_ID
                    && message.routing == 1
                    && CiscoIpPhoneSetRingTone::from_xml(&message.data).unwrap() == document
        ));
    }

    #[tokio::test]
    async fn registered_phone_receives_typed_background_selection_and_preview_commands() {
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let (server, handle, mut events) = Server::bind(config, [definition()]).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let mut phone = TcpStream::connect(address).await.unwrap();
        let mut decoder = FrameDecoder::new();
        let protocol = ProtocolVersion::V22;
        let device_id = DeviceId::new("SEP001122334455").unwrap();

        phone.write_all(&register_bytes(protocol)).await.unwrap();
        read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Registered(_)
            }))
        ));

        let set = CiscoIpPhoneSetBackground::new(
            PhoneBackgroundHttpUrl::new("http://pbx.example/background.png").unwrap(),
            PhoneBackgroundHttpUrl::new("http://pbx.example/background-thumb.png").unwrap(),
        );
        handle
            .send(Command::new(
                device_id.clone(),
                CommandAction::SetBackgroundImage {
                    transaction_id: TransactionId::new(109),
                    document: set.clone(),
                },
            ))
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::USER_TO_DEVICE_DATA_V1).await;
        assert!(frames.into_iter().any(|frame| matches!(
            ServerMessage::decode(frame, protocol),
            Ok(ServerMessage::UserToDeviceDataV1(message))
                if message.application_id == PHONE_BACKGROUND_APPLICATION_ID
                    && message.transaction_id == 109
                    && CiscoIpPhoneSetBackground::from_xml(&message.data).unwrap() == set
        )));

        let preview = CiscoIpPhoneSetBackgroundPreview::new(
            PhoneBackgroundHttpUrl::new("http://pbx.example/background.png?preview=1").unwrap(),
        );
        handle
            .send(Command::new(
                device_id,
                CommandAction::PreviewBackgroundImage {
                    transaction_id: TransactionId::new(110),
                    document: preview.clone(),
                },
            ))
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::USER_TO_DEVICE_DATA_V1).await;
        assert!(frames.into_iter().any(|frame| matches!(
            ServerMessage::decode(frame, protocol),
            Ok(ServerMessage::UserToDeviceDataV1(message))
                if message.application_id == PHONE_BACKGROUND_APPLICATION_ID
                    && message.transaction_id == 110
                    && CiscoIpPhoneSetBackgroundPreview::from_xml(&message.data).unwrap() == preview
        )));

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn registered_phone_receives_typed_ringtone_command() {
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let (server, handle, mut events) = Server::bind(config, [definition()]).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let mut phone = TcpStream::connect(address).await.unwrap();
        let mut decoder = FrameDecoder::new();
        let protocol = ProtocolVersion::V22;
        let device_id = DeviceId::new("SEP001122334455").unwrap();

        phone.write_all(&register_bytes(protocol)).await.unwrap();
        read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Registered(_)
            }))
        ));

        let document = CiscoIpPhoneSetRingTone::new(
            PhoneRingtoneUrl::new("http://pbx.example/ringtones/Classic.raw?locale=sv").unwrap(),
        );
        handle
            .send(Command::new(
                device_id,
                CommandAction::SetRingtone {
                    transaction_id: TransactionId::new(112),
                    document: document.clone(),
                },
            ))
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::USER_TO_DEVICE_DATA_V1).await;
        assert!(frames.into_iter().any(|frame| matches!(
            ServerMessage::decode(frame, protocol),
            Ok(ServerMessage::UserToDeviceDataV1(message))
                if message.application_id == PHONE_RINGTONE_APPLICATION_ID
                    && message.line_instance == 0
                    && message.call_reference == 0
                    && message.transaction_id == 112
                    && message.sequence_flag == 2
                    && message.display_priority == 0
                    && message.conference_id == 0
                    && message.application_instance_id == PHONE_RINGTONE_APPLICATION_ID
                    && message.routing == 1
                    && CiscoIpPhoneSetRingTone::from_xml(&message.data).unwrap() == document
        )));

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[test]
    fn status_service_delivery_preserves_items_icons_timers_and_envelope() {
        let bitmap = PhoneStatusDocument::Bitmap(CiscoIpPhoneStatus {
            text: Some("Calls waiting".into()),
            timer_seconds: Some(30),
            location_x: Some(-1),
            location_y: Some(20),
            width: 106,
            height: 21,
            depth: 2,
            data: Some(PhoneBitmapData::new(vec![0x5a; PHONE_STATUS_BITMAP_MAX_BYTES]).unwrap()),
        });
        let legacy = status_service_messages(
            LineInstance::new(3),
            CallReference::new(71),
            ApplicationId::new(9_096),
            TransactionId::new(103),
            PhoneServicePriority::HIGH,
            &bitmap,
            ProtocolVersion::V17,
        )
        .unwrap();
        assert!(matches!(
            legacy.as_slice(),
            [ServerMessage::UserToDeviceDataV1(message)]
                if message.application_id == 9_096
                    && message.line_instance == 3
                    && message.call_reference == 71
                    && message.transaction_id == 103
                    && message.sequence_flag == 2
                    && message.display_priority == 2
                    && message.routing == 1
                    && PhoneStatusDocument::from_xml(&message.data).unwrap() == bitmap
        ));

        let file = PhoneStatusDocument::File(CiscoIpPhoneStatusFile {
            text: Some("Map status".into()),
            timer_seconds: Some(0),
            location_x: Some(261),
            location_y: Some(49),
            url: PhoneImageUrl::new("https://pbx.example/status.png?site=east").unwrap(),
        });
        let modern = status_service_messages(
            LineInstance::new(3),
            CallReference::new(71),
            ApplicationId::new(9_096),
            TransactionId::new(104),
            PhoneServicePriority::LOW,
            &file,
            ProtocolVersion::V22,
        )
        .unwrap();
        assert!(matches!(
            modern.as_slice(),
            [ServerMessage::UserToDeviceDataV1(message)]
                if message.sequence_flag == 2
                    && message.display_priority == 0
                    && PhoneStatusDocument::from_xml(&message.data).unwrap() == file
        ));

        let invalid = PhoneStatusDocument::Bitmap(CiscoIpPhoneStatus {
            text: None,
            timer_seconds: None,
            location_x: None,
            location_y: None,
            width: 1,
            height: 1,
            depth: 1,
            data: Some(PhoneBitmapData::new(vec![0; PHONE_STATUS_BITMAP_MAX_BYTES + 1]).unwrap()),
        });
        assert!(matches!(
            status_service_messages(
                LineInstance::new(3),
                CallReference::new(71),
                ApplicationId::new(9_096),
                TransactionId::new(105),
                PhoneServicePriority::NORMAL,
                &invalid,
                ProtocolVersion::V22,
            ),
            Err(ServerError::PhoneXml(PhoneXmlError::LimitExceeded {
                kind: "phone status bitmap bytes",
                maximum: PHONE_STATUS_BITMAP_MAX_BYTES,
                ..
            }))
        ));
    }

    #[tokio::test]
    async fn registered_phone_receives_typed_execute_image_status_tone_and_announcement_commands() {
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let (server, handle, mut events) = Server::bind(config, [definition()]).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let mut phone = TcpStream::connect(address).await.unwrap();
        let mut decoder = FrameDecoder::new();
        let protocol = ProtocolVersion::V22;
        let device_id = DeviceId::new("SEP001122334455").unwrap();

        phone.write_all(&register_bytes(protocol)).await.unwrap();
        read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Registered(_)
            }))
        ));

        let execute = CiscoIpPhoneExecute::new(vec![
            CiscoIpPhoneExecuteItem::with_priority("App:Close:9093", PhoneExecutePriority::NORMAL)
                .unwrap(),
        ])
        .unwrap();
        handle
            .send(Command::new(
                device_id.clone(),
                CommandAction::ExecutePhoneActions {
                    line_instance: LineInstance::new(2),
                    call_reference: CallReference::new(42),
                    application_id: ApplicationId::new(9_093),
                    transaction_id: TransactionId::new(73),
                    priority: PhoneServicePriority::HIGH,
                    document: execute.clone(),
                },
            ))
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::USER_TO_DEVICE_DATA_V1).await;
        assert!(frames.into_iter().any(|frame| matches!(
            ServerMessage::decode(frame, protocol),
            Ok(ServerMessage::UserToDeviceDataV1(message))
                if message.application_id == 9_093
                    && message.line_instance == 2
                    && message.call_reference == 42
                    && message.transaction_id == 73
                    && message.display_priority == 2
                    && CiscoIpPhoneExecute::from_xml(&message.data).unwrap() == execute
        )));

        let image = PhoneImageDocument::ImageFile(CiscoIpPhoneImageFile {
            keypad_target: None,
            application_id: Some("map".into()),
            on_focus_lost: None,
            on_focus_gained: None,
            on_minimized: None,
            on_closed: None,
            title: Some("Site map".into()),
            prompt: Some("Inspect".into()),
            soft_keys: Vec::new(),
            key_items: Vec::new(),
            location_x: Some(12),
            location_y: Some(8),
            url: PhoneImageUrl::new("https://pbx.example/site.png?view=all").unwrap(),
        });
        handle
            .send(Command::new(
                device_id.clone(),
                CommandAction::ShowImageService {
                    line_instance: LineInstance::new(2),
                    call_reference: CallReference::new(42),
                    application_id: ApplicationId::new(9_095),
                    transaction_id: TransactionId::new(74),
                    priority: PhoneServicePriority::LOW,
                    document: image.clone(),
                },
            ))
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::USER_TO_DEVICE_DATA_V1).await;
        assert!(frames.into_iter().any(|frame| matches!(
            ServerMessage::decode(frame, protocol),
            Ok(ServerMessage::UserToDeviceDataV1(message))
                if message.application_id == 9_095
                    && message.line_instance == 2
                    && message.call_reference == 42
                    && message.transaction_id == 74
                    && message.display_priority == 0
                    && PhoneImageDocument::from_xml(&message.data).unwrap() == image
        )));

        let status = PhoneStatusDocument::File(CiscoIpPhoneStatusFile {
            text: Some("Queue ready".into()),
            timer_seconds: Some(10),
            location_x: Some(4),
            location_y: Some(8),
            url: PhoneImageUrl::new("https://pbx.example/status.png?queue=support").unwrap(),
        });
        handle
            .send(Command::new(
                device_id.clone(),
                CommandAction::ShowStatusService {
                    line_instance: LineInstance::new(2),
                    call_reference: CallReference::new(42),
                    application_id: ApplicationId::new(9_096),
                    transaction_id: TransactionId::new(75),
                    priority: PhoneServicePriority::NORMAL,
                    document: status.clone(),
                },
            ))
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::USER_TO_DEVICE_DATA_V1).await;
        assert!(frames.into_iter().any(|frame| matches!(
            ServerMessage::decode(frame, protocol),
            Ok(ServerMessage::UserToDeviceDataV1(message))
                if message.application_id == 9_096
                    && message.line_instance == 2
                    && message.call_reference == 42
                    && message.transaction_id == 75
                    && message.display_priority == 1
                    && PhoneStatusDocument::from_xml(&message.data).unwrap() == status
        )));

        let call_id = CallId(7001);
        handle
            .send(Command::new(
                device_id.clone(),
                CommandAction::BeginCall {
                    line_instance: LineInstance(1),
                    call_id,
                    codec: Codec::Pcma,
                },
            ))
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, id::SELECT_SOFT_KEYS).await;
        handle
            .send(Command::new(
                device_id.clone(),
                CommandAction::StartTone {
                    call_id,
                    tone: Tone::RecorderWarning,
                },
            ))
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::START_TONE).await;
        assert!(frames.into_iter().any(|frame| matches!(
            ServerMessage::decode(frame, protocol),
            Ok(ServerMessage::StartTone {
                tone: Tone::RecorderWarning,
                direction: ToneDirection::User,
                line_instance: 1,
                call_reference: 7001,
            })
        )));

        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::SetMicrophoneMode { enabled: false },
            ))
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::SET_MICROPHONE_MODE).await;
        assert!(frames.into_iter().any(|frame| matches!(
            ServerMessage::decode(frame, protocol),
            Ok(ServerMessage::SetMicrophoneMode(MicrophoneMode::Off))
        )));

        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::SetRecordingStatus {
                    call_id,
                    active: true,
                },
            ))
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::RECORDING_STATUS).await;
        assert!(frames.into_iter().any(|frame| matches!(
            ServerMessage::decode(frame, protocol),
            Ok(ServerMessage::RecordingStatus {
                call_reference: 7001,
                active: true,
            })
        )));

        let conference_id = ConferenceId::new(44);
        let rejected = handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::StartAnnouncement {
                    conference_id,
                    announcements: vec![AnnouncementEntry {
                        locale: 1,
                        country: 46,
                        tone: Tone::Zip,
                    }],
                    end_of_ack: true,
                    participant_ids: vec![ParticipantId::new(7), ParticipantId::new(9)],
                    hearing_participant_mask: 0b11,
                    play_mode: 2,
                },
            ))
            .await
            .unwrap_err();
        assert!(
            matches!(rejected, ServerError::CommandWrite(message) if message.contains("not a station command"))
        );

        // Rejecting a service-node message must not retire the handset
        // session or poison subsequent station UI delivery.
        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::SetMicrophoneMode { enabled: true },
            ))
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::SET_MICROPHONE_MODE).await;
        assert!(frames.into_iter().any(|frame| matches!(
            ServerMessage::decode(frame, protocol),
            Ok(ServerMessage::SetMicrophoneMode(MicrophoneMode::On))
        )));

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[test]
    fn announcement_command_mapping_preserves_typed_ids_and_wire_bounds() {
        let message = start_announcement_message(
            ConferenceId::new(44),
            vec![AnnouncementEntry {
                locale: 1,
                country: 46,
                tone: Tone::Zip,
            }],
            true,
            vec![ParticipantId::new(7), ParticipantId::new(9)],
            0b11,
            2,
        );
        assert!(matches!(
            message,
            ServerMessage::StartAnnouncement {
                conference_id: 44,
                end_of_ack: 1,
                ref matrix_conference_party_ids,
                ..
            } if matrix_conference_party_ids == &[7, 9]
        ));
        assert!(matches!(
            message.encode(ProtocolVersion::V22),
            Err(CodecError::UnexpectedRoute {
                actual: crate::MessageRoute::IntraControl,
                ..
            })
        ));
        let control = ControlMessage::StartAnnouncement {
            announcements: vec![AnnouncementEntry {
                locale: 1,
                country: 46,
                tone: Tone::Zip,
            }],
            end_of_ack: EndOfAnnouncementAck::Required,
            conference_id: 44,
            matrix_conference_party_ids: vec![7, 9],
            hearing_conference_party_mask: 0b11,
            play_mode: AnnouncementPlayMode::Continuous,
        };
        assert!(control.encode(ProtocolVersion::V22).is_ok());

        let too_many_announcements = start_announcement_message(
            ConferenceId::new(44),
            vec![
                AnnouncementEntry {
                    locale: 1,
                    country: 46,
                    tone: Tone::Zip,
                };
                33
            ],
            false,
            Vec::new(),
            0,
            0,
        );
        assert!(matches!(
            too_many_announcements.encode(ProtocolVersion::V22),
            Err(CodecError::CountTooLarge {
                field: "announcements",
                maximum: 32,
                ..
            })
        ));

        let too_many_participants = start_announcement_message(
            ConferenceId::new(44),
            Vec::new(),
            false,
            (1..=17).map(ParticipantId::new).collect(),
            0,
            0,
        );
        assert!(matches!(
            too_many_participants.encode(ProtocolVersion::V22),
            Err(CodecError::CountTooLarge {
                field: "matrix conference party identifiers",
                maximum: 16,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn registered_xml_alarms_route_typed_or_opaque_without_leaking_payloads() {
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let (server, handle, mut events) = Server::bind(config, [definition()]).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let mut phone = TcpStream::connect(address).await.unwrap();
        let mut decoder = FrameDecoder::new();
        let protocol = ProtocolVersion::V22;

        phone.write_all(&register_bytes(protocol)).await.unwrap();
        read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Registered(_)
            }))
        ));

        let known = "<x-cisco-alarm><Alarm Name=\"LastOutOfServiceInformation\"><ParameterList><String name=\"DeviceName\">private-device-name</String><Enum name=\"ReasonForOutOfService\">25</Enum></ParameterList></Alarm></x-cisco-alarm>";
        phone
            .write_all(
                &ClientMessage::XmlAlarm(XmlAlarmMessage::from_xml(known).unwrap())
                    .encode(protocol)
                    .unwrap(),
            )
            .await
            .unwrap();
        let Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id,
            event: DeviceEventKind::XmlAlarm { telemetry },
        })) = events.recv().await
        else {
            panic!("typed XML alarm event was not emitted");
        };
        assert_eq!(device_id, DeviceId::new("SEP001122334455").unwrap());
        assert_eq!(
            telemetry.summary(),
            Some(crate::phone::xml::PhoneAlarmSummary {
                kind: crate::phone::xml::PhoneAlarmKind::LastOutOfService,
                reason_for_out_of_service: Some(25),
            })
        );
        assert!(!format!("{telemetry:?}").contains("private-device-name"));

        let unknown = "<vendor-alarm><Credential>private-token</Credential></vendor-alarm>";
        phone
            .write_all(
                &ClientMessage::XmlAlarm(XmlAlarmMessage::from_xml(unknown).unwrap())
                    .encode(protocol)
                    .unwrap(),
            )
            .await
            .unwrap();
        let Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::XmlAlarm { telemetry, .. },
        })) = events.recv().await
        else {
            panic!("opaque XML alarm event was not emitted");
        };
        assert!(telemetry.is_opaque());
        assert_eq!(telemetry.summary(), None);
        assert!(!format!("{telemetry:?}").contains("private-token"));

        phone
            .write_all(
                &ClientMessage::XmlAlarm(
                    XmlAlarmMessage::from_xml(
                        "<x-cisco-alarm><Alarm Name=\"Unknown\">&undeclared;</Alarm></x-cisco-alarm>",
                    )
                    .unwrap(),
                )
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), events.recv())
                .await
                .is_err()
        );

        phone
            .write_all(&ClientMessage::KeepAlive.encode(protocol).unwrap())
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::KEEP_ALIVE_ACK).await;
        assert!(
            frames
                .iter()
                .any(|frame| frame.message_id == id::KEEP_ALIVE_ACK)
        );

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn registered_location_information_routes_typed_or_opaque_without_leaking_fields() {
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let (server, handle, mut events) = Server::bind(config, [definition()]).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let mut phone = TcpStream::connect(address).await.unwrap();
        let mut decoder = FrameDecoder::new();
        let protocol = ProtocolVersion::V22;

        phone.write_all(&register_bytes(protocol)).await.unwrap();
        read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Registered(_)
            }))
        ));

        let known = "<Interface1><wifi><BSSID>E8:ED:F3:10:29:FD</BSSID><SSID>private-network</SSID><APName>private-access-point</APName></wifi><OffPrem></OffPrem></Interface1>";
        phone
            .write_all(
                &ClientMessage::LocationInfo { xml: known.into() }
                    .encode(protocol)
                    .unwrap(),
            )
            .await
            .unwrap();
        let Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id,
            event: DeviceEventKind::LocationInformation { telemetry },
        })) = events.recv().await
        else {
            panic!("typed location-information event was not emitted");
        };
        assert_eq!(device_id, DeviceId::new("SEP001122334455").unwrap());
        assert_eq!(
            telemetry.summary(),
            Some(crate::phone::xml::PhoneLocationSummary {
                kind: crate::phone::xml::PhoneLocationKind::WirelessInterface,
                off_premises: true,
            })
        );
        let crate::phone::xml::PhoneLocationTelemetry::WirelessInterface(location) = &telemetry
        else {
            panic!("known wireless location was not typed");
        };
        assert_eq!(
            location.wifi.bssid.octets(),
            [0xe8, 0xed, 0xf3, 0x10, 0x29, 0xfd]
        );
        assert_eq!(location.wifi.ssid, "private-network");
        assert_eq!(location.wifi.access_point_name, "private-access-point");
        let debug = format!("{telemetry:?}");
        assert!(!debug.contains("private-network"));
        assert!(!debug.contains("private-access-point"));
        assert!(!debug.contains("E8:ED:F3:10:29:FD"));

        let unknown =
            "<DeviceLocation><CivicAddress>private-building</CivicAddress></DeviceLocation>";
        phone
            .write_all(
                &ClientMessage::LocationInfo {
                    xml: unknown.into(),
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        let Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::LocationInformation { telemetry, .. },
        })) = events.recv().await
        else {
            panic!("opaque location-information event was not emitted");
        };
        assert!(telemetry.is_opaque());
        assert_eq!(telemetry.summary(), None);
        assert!(!format!("{telemetry:?}").contains("private-building"));

        phone
            .write_all(
                &ClientMessage::LocationInfo {
                    xml: "<Interface1>&undeclared;</Interface1>".into(),
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), events.recv())
                .await
                .is_err()
        );

        phone
            .write_all(&ClientMessage::KeepAlive.encode(protocol).unwrap())
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::KEEP_ALIVE_ACK).await;
        assert!(
            frames
                .iter()
                .any(|frame| frame.message_id == id::KEEP_ALIVE_ACK)
        );

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn transfer_presentation_marks_source_and_keeps_consultation_active() {
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let (server, handle, mut events) = Server::bind(config, [definition()]).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let mut phone = TcpStream::connect(address).await.unwrap();
        let mut decoder = FrameDecoder::new();
        let protocol = ProtocolVersion::V22;
        let device_id = DeviceId::new("SEP001122334455").unwrap();

        phone.write_all(&register_bytes(protocol)).await.unwrap();
        read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Registered(_)
            }))
        ));
        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::BeginCall {
                    line_instance: LineInstance(1),
                    call_id: CallId(10),
                    codec: Codec::Pcmu,
                },
            ))
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, id::SELECT_SOFT_KEYS).await;
        for state in [CallState::Connected, CallState::Hold] {
            handle
                .send_confirmed(Command::new(
                    device_id.clone(),
                    CommandAction::SetCallState {
                        call_id: CallId(10),
                        state,
                    },
                ))
                .await
                .unwrap();
            read_until_message(&mut phone, &mut decoder, id::SELECT_SOFT_KEYS).await;
        }

        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::BeginTransfer {
                    source_call_id: CallId(10),
                    consultation_line_instance: LineInstance(1),
                    consultation_call_id: CallId(20),
                    codec: Codec::Pcmu,
                },
            ))
            .await
            .unwrap();
        let messages = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
            matches!(
                message,
                ServerMessage::SetLamp {
                    stimulus: ButtonType::Transfer,
                    mode: LampMode::Flash,
                    ..
                }
            )
        })
        .await;
        let states = messages
            .iter()
            .filter_map(|message| match message {
                ServerMessage::CallState {
                    state,
                    call_reference,
                    ..
                } => Some((*state, *call_reference)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            states,
            vec![(CallState::Transfer, 10), (CallState::OffHook, 20)]
        );
        assert!(messages.iter().any(|message| matches!(
            message,
            ServerMessage::SelectSoftKeys {
                call_reference: 20,
                set: KeyMode::OffHookFeature,
                ..
            }
        )));
        assert!(!messages.iter().any(|message| matches!(
            message,
            ServerMessage::CallState {
                state: CallState::Transfer,
                call_reference: 20,
                ..
            }
        )));

        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::SetCallState {
                    call_id: CallId(20),
                    state: CallState::Connected,
                },
            ))
            .await
            .unwrap();
        let messages = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
            matches!(
                message,
                ServerMessage::SelectSoftKeys {
                    call_reference: 20,
                    set: KeyMode::ConnectedTransfer,
                    ..
                }
            )
        })
        .await;
        assert!(messages.iter().any(|message| matches!(
            message,
            ServerMessage::SelectSoftKeys {
                call_reference: 20,
                set: KeyMode::ConnectedTransfer,
                ..
            }
        )));

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn active_call_selection_and_hook_flash_use_exact_session_identity() {
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let (server, handle, mut events) = Server::bind(config, [definition()]).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let mut phone = TcpStream::connect(address).await.unwrap();
        let mut decoder = FrameDecoder::new();
        let protocol = ProtocolVersion::V22;
        let device_id = DeviceId::new("SEP001122334455").unwrap();

        phone.write_all(&register_bytes(protocol)).await.unwrap();
        read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Registered(_)
            }))
        ));
        for call_id in [CallId(10), CallId(20)] {
            handle
                .send(Command::new(
                    device_id.clone(),
                    CommandAction::BeginCall {
                        line_instance: LineInstance(1),
                        call_id,
                        codec: Codec::Pcmu,
                    },
                ))
                .await
                .unwrap();
            read_until_message(&mut phone, &mut decoder, id::SELECT_SOFT_KEYS).await;
        }

        handle
            .send(Command::new(
                device_id.clone(),
                CommandAction::SetCallSelected {
                    call_id: CallId(10),
                    selected: true,
                },
            ))
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, id::CALL_SELECT_STAT).await;
        assert!(frames.into_iter().any(|frame| matches!(
            ServerMessage::decode(frame, protocol),
            Ok(ServerMessage::CallSelectStatus {
                status: 1,
                call_reference: 10,
                line_instance: 1,
            })
        )));

        phone
            .write_all(
                &ClientMessage::HookFlash {
                    line_instance: 1,
                    call_reference: 0,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::HookFlash {
                    call_id: Some(CallId(20)),
                    line_instance: LineInstance(1),
                    ..
                }
            }))
        ));

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn omitted_answer_uses_live_policy_and_skips_an_offer_closed_before_input() {
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            call_answer_order: CallSelectionOrder::LastFirst,
            ..ServerConfig::default()
        };
        let (server, handle, mut events) = Server::bind(config, [definition()]).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let mut phone = TcpStream::connect(address).await.unwrap();
        let mut decoder = FrameDecoder::new();
        let protocol = ProtocolVersion::V22;
        let device_id = DeviceId::new("SEP001122334455").unwrap();

        phone.write_all(&register_bytes(protocol)).await.unwrap();
        read_until_message(&mut phone, &mut decoder, id::CAPABILITIES_REQ).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Registered(_)
            }))
        ));
        handle
            .send(Command::new(
                device_id.clone(),
                CommandAction::BeginCall {
                    line_instance: LineInstance(1),
                    call_id: CallId(1),
                    codec: Codec::Pcmu,
                },
            ))
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, id::SELECT_SOFT_KEYS).await;
        handle
            .send(Command::new(
                device_id.clone(),
                CommandAction::SetCallState {
                    call_id: CallId(1),
                    state: CallState::Connected,
                },
            ))
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, id::CALL_STATE).await;

        let info = CallInfo {
            direction: crate::types::CallDirection::Inbound,
            calling_name: "Caller".into(),
            calling_number: "1002".into(),
            called_name: "Desk".into(),
            called_number: "1001".into(),
            ..CallInfo::default()
        };
        for call_id in [CallId(10), CallId(20)] {
            handle
                .offer_incoming_call_with_id(
                    device_id.clone(),
                    LineInstance::new(1),
                    call_id,
                    info.clone(),
                )
                .await
                .unwrap();
            read_until_message(&mut phone, &mut decoder, id::DISPLAY_DYNAMIC_PROMPT_STATUS).await;
        }
        phone
            .write_all(
                &ClientMessage::SoftKeyEvent {
                    event: SoftKey::Answer.wire_value(),
                    line_instance: 1,
                    call_reference: 0,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::SoftKey {
                    call_id: Some(CallId(20)),
                    soft_key: SoftKey::Answer,
                    ..
                }
            }))
        ));

        handle
            .try_offer_incoming_call_with_id(
                device_id.clone(),
                LineInstance::new(1),
                CallId(30),
                info,
            )
            .unwrap();
        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::CloseCall {
                    call_id: CallId(30),
                },
            ))
            .await
            .unwrap();
        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::CloseCall {
                    call_id: CallId(20),
                },
            ))
            .await
            .unwrap();
        phone
            .write_all(
                &ClientMessage::OffHook {
                    line_instance: 1,
                    call_reference: 0,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::OffHook {
                    call_id: CallId(10),
                    line_instance: LineInstance(1),
                    ..
                }
            }))
        ));

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[test]
    fn server_messages_are_decodeable_frames() {
        let bytes = ServerMessage::CapabilitiesRequest
            .encode(ProtocolVersion::V22)
            .unwrap();
        assert_eq!(
            FrameDecoder::new().push(&bytes).unwrap()[0].message_id,
            id::CAPABILITIES_REQ
        );
        assert!(matches!(
            ClientMessage::decode(Frame::new(0, id::KEEP_ALIVE, Vec::new())).unwrap(),
            ClientMessage::KeepAlive
        ));
    }
}
