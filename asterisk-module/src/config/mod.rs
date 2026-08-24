//! Parsing, normalization, and validation for `sccp.conf`.
//!
//! [`ModuleConfig::parse`] accepts `[general]`, device, line, and soft-key
//! profile sections plus Asterisk-style templates. It resolves inheritance,
//! ordered repeated fields, button positions, codec operations, line
//! appearances, feature policy, network/listener policy, channel metadata,
//! registration targets, and referenced profiles into one immutable snapshot.
//! Unknown keys, wrong-scope keys, conflicting aliases, duplicate scalar
//! settings, invalid references, ambiguous shared targets, and out-of-bound text
//! reject the complete candidate.
//!
//! The installed module reads `sccp.conf` under Asterisk's compiled
//! configuration directory unless the `SCCP_CONFIG` environment variable names
//! an exact path. [`provider`] supplies file, realtime, and hybrid candidates;
//! [`realtime`] preserves backend ordering and `NULL`/empty distinctions; and
//! the feature-gated `reload` module plans transactional application without
//! weakening parser rules.
//!
//! The repository's `sccp.conf.example` uses one canonical spelling for every
//! supported semantic option. Compatibility aliases are parser inputs, not
//! additional settings, so mutually exclusive aliases are described in
//! owning type/field documentation instead of being combined in the sample.
//! Parser tests load that distributed sample and assert representative values
//! at general, device, line, button, media, feature, registration, date/time,
//! and MWI scopes.
//!
//! # Network policy boundary
//!
//! The runtime binds the configured clear listener and optional secure
//! listener. Per-device transport requirements are enforced at registration.
//! NAT mode, `localnet`, external/advertised IPv4 and IPv6 addresses, and
//! hostname refresh are active inputs to signaling-peer and RTP address
//! selection. Address-family mismatch, unusable endpoints, and unresolved
//! required external addresses fail closed.
//!
//! Sensitive configuration values use typed wrappers with redacted [`Debug`]
//! output. In particular, mobility PIN comparison scans the full fixed seven
//! digit bound without a data-dependent early exit; PINs, forwarding targets,
//! channel-variable values, TLS paths where marked sensitive, and opaque
//! provider values do not appear in validation diagnostics.

pub mod provider;
pub mod realtime;
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-23"))]
pub mod reload;
mod section_values;
mod serde_section;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use sccp_protocol::{
    AddonModuleDefinition, AppearanceRingMode, AudioProcessingPolicy, BlfSpeedDialDefinition,
    ButtonDefinition, ButtonType, Codec, CodecKind, DateTemplate, DeviceDefinition, DeviceId,
    DeviceType, DtmfMode, EchoCancellation, FeatureDefinition, KeyMode, LampMode, LegacyCodePage,
    LineAppearance, LineDefinition, RingerMode, ServiceDefinition, SignalingQos,
    SignalingServerRoute, SilenceSuppression, SoftKey, SoftKeyProfile as StationSoftKeyProfile,
    SpeedDialDefinition, StationTransportRequirement, StationUiPolicy, Tone,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::call::forwarding::ForwardingDestination;
use crate::call::hotline::{HotlineDestination, MAX_HOTLINE_DESTINATION_BYTES};
use crate::call::metadata::{
    ChannelVariable, MAX_ACCOUNT_CODE_BYTES, MAX_LANGUAGE_BYTES, MAX_VARIABLE_AGGREGATE_BYTES,
    MAX_VARIABLES,
};
use crate::call::voicemail::VoicemailDestination;
use crate::media::encryption::{
    MediaEncryptionPolicy, MediaEncryptionProfile, MediaEncryptionRequirement,
};
use crate::media::formats::{pbx_audio_format, unsupported_audio_reason};
use section_values::SectionValues;
use serde_section::{deserialize_entries, deserialize_section, serialized_key};

pub const DEFAULT_SOFT_KEY_PROFILE: &str = "default";
pub const MAX_SOFT_KEYS_PER_MODE: usize = 16;
pub const MAX_CODEC_PREFERENCES: usize = 32;
pub const MAX_HOTLINE_FIELD_BYTES: usize = MAX_HOTLINE_DESTINATION_BYTES;
pub const MAX_EXTERNAL_REFRESH_SECONDS: u32 = 86_400;
pub const MAX_REALTIME_FAMILY_BYTES: usize = 45;
pub const MAX_MOBILITY_PIN_DIGITS: usize = 7;
pub const MAX_REGISTRATION_IDENTIFIER_BYTES: usize = 79;
pub const MAX_REGISTRATION_EXTENSION_LIST_BYTES: usize = 255;

const KEY_MODES: [KeyMode; 14] = [
    KeyMode::OnHook,
    KeyMode::Connected,
    KeyMode::OnHold,
    KeyMode::RingIn,
    KeyMode::OffHook,
    KeyMode::ConnectedTransfer,
    KeyMode::DigitsFollowing,
    KeyMode::ConnectedConference,
    KeyMode::RingOut,
    KeyMode::OffHookFeature,
    KeyMode::InUseHint,
    KeyMode::OnHookStealable,
    KeyMode::HoldConference,
    KeyMode::Empty,
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SoftKeyProfile {
    pub name: String,
    /// Ordered actions for every handset key mode. Missing configuration
    /// entries normalize to an empty set.
    pub sets: HashMap<KeyMode, Vec<SoftKey>>,
}

impl SoftKeyProfile {
    pub fn actions(&self, mode: KeyMode) -> &[SoftKey] {
        self.sets.get(&mode).map_or(&[], Vec::as_slice)
    }

    fn empty(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            sets: KEY_MODES
                .into_iter()
                .map(|mode| (mode, Vec::new()))
                .collect(),
        }
    }

    fn built_in() -> Self {
        let mut profile = Self::empty(DEFAULT_SOFT_KEY_PROFILE);
        profile.sets.extend([
            (KeyMode::OnHook, vec![SoftKey::NewCall]),
            (
                KeyMode::Connected,
                vec![SoftKey::Hold, SoftKey::EndCall, SoftKey::Transfer],
            ),
            (
                KeyMode::OnHold,
                vec![SoftKey::Resume, SoftKey::NewCall, SoftKey::EndCall],
            ),
            (KeyMode::RingIn, vec![SoftKey::Answer, SoftKey::EndCall]),
            (KeyMode::OffHook, vec![SoftKey::EndCall]),
            (
                KeyMode::ConnectedTransfer,
                vec![SoftKey::Hold, SoftKey::EndCall, SoftKey::Transfer],
            ),
            (
                KeyMode::DigitsFollowing,
                vec![SoftKey::Backspace, SoftKey::EndCall, SoftKey::Dial],
            ),
            (
                KeyMode::ConnectedConference,
                vec![SoftKey::Hold, SoftKey::EndCall],
            ),
            (KeyMode::RingOut, vec![SoftKey::EndCall]),
            (
                KeyMode::OffHookFeature,
                vec![SoftKey::Resume, SoftKey::NewCall, SoftKey::EndCall],
            ),
            (
                KeyMode::OnHookStealable,
                vec![SoftKey::Intercept, SoftKey::NewCall],
            ),
            (
                KeyMode::HoldConference,
                vec![SoftKey::Resume, SoftKey::NewCall, SoftKey::EndCall],
            ),
        ]);
        profile
    }

    fn station_profile(&self) -> StationSoftKeyProfile {
        StationSoftKeyProfile::new(
            KEY_MODES
                .into_iter()
                .map(|mode| (mode, self.actions(mode).to_vec())),
        )
        .expect("configuration parser produced an invalid station soft-key profile")
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct GeneralConfig {
    pub bind: SocketAddr,
    pub advertised_address: Ipv4Addr,
    pub server_name: String,
    /// Default PBX prompt language inherited by logical lines.
    pub language: String,
    /// Optional default CDR account code inherited by logical lines.
    pub account_code: Option<String>,
    pub keepalive_seconds: u32,
    pub secondary_keepalive_seconds: u32,
    pub signaling_servers: Vec<SignalingServerRoute>,
    pub first_digit_timeout_ms: u64,
    pub interdigit_timeout_ms: u64,
    pub dial_terminator: DialTerminatorConfig,
    pub simulate_enbloc: bool,
    /// Service policy for speed-dial digit collection.
    pub speed_dial_await_further_digits: bool,
    pub allow_overlap: bool,
    /// Complete an eligible in-flight consultation when its handset leg goes
    /// on-hook. The value is captured when the transfer begins.
    pub transfer_on_hangup: bool,
    /// Ordering used when selecting among multiple answerable calls.
    pub call_answer_order: CallAnswerOrder,
    /// Fixed SCCP station wall-clock offset from UTC.
    pub timezone_offset_minutes: i16,
    /// SCCP station date-field order and separator.
    pub date_template: DateTemplate,
    /// Default physical ringer for ordinary inbound presentations.
    pub ring_type: RingerMode,
    /// Tone played on the existing active call when another call arrives.
    pub call_waiting_tone: Option<Tone>,
    /// Repeat interval in seconds; zero disables repeats.
    pub call_waiting_interval_seconds: u32,
    pub codecs: Vec<Codec>,
    pub audio_encryption: MediaEncryptionPolicy,
    /// Defaults for destination-based conference dialing. Device sections may
    /// override these values, and line sections may override them again.
    pub conference_dialing: ConferenceDialingConfig,
    pub auto_answer: AutoAnswerConfig,
    /// Tone played while an active handset presentation briefly reports a
    /// passive remote termination. `None` disables the delayed notification.
    pub remote_hangup_tone: Option<Tone>,
    pub guest_hotline: GuestHotlineConfig,
    pub direct_media: bool,
    pub early_media: bool,
    /// Station-side echo cancellation and silence suppression defaults. Line
    /// sections may override either setting independently.
    pub audio_processing: AudioProcessingPolicy,
    pub jitter_buffer: JitterBufferConfig,
    pub registration: RegistrationConfig,
    /// Policy used when a station registered to another configured server asks
    /// whether it should move back to this server.
    pub fallback_registration: FallbackRegistrationConfig,
    pub network: NetworkPolicy,
    pub qos: QosPolicy,
    pub listeners: ListenerPolicy,
    /// Realtime table families selected for file-plus-realtime configuration.
    /// Both families are required so refreshes always build a complete
    /// device/line candidate before replacing the live snapshot.
    pub realtime_tables: Option<RealtimeTableConfig>,
}

/// Runtime-ready timing values derived from the integer syntax accepted by
/// `sccp.conf`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GeneralTimingPolicy {
    pub keepalive: Duration,
    pub secondary_keepalive: Duration,
    pub first_digit_timeout: Duration,
    pub interdigit_timeout: Duration,
    pub call_waiting_repeat: Duration,
}

/// Runtime-ready station presentation defaults derived from general policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GeneralStationPolicy {
    pub timezone_offset_minutes: i16,
    pub date_template: DateTemplate,
    pub ring_type: RingerMode,
    pub call_waiting_tone: Option<Tone>,
}

impl GeneralConfig {
    pub fn timing_policy(&self) -> GeneralTimingPolicy {
        GeneralTimingPolicy {
            keepalive: Duration::from_secs(self.keepalive_seconds.into()),
            secondary_keepalive: Duration::from_secs(self.secondary_keepalive_seconds.into()),
            first_digit_timeout: Duration::from_millis(self.first_digit_timeout_ms),
            interdigit_timeout: Duration::from_millis(self.interdigit_timeout_ms),
            call_waiting_repeat: Duration::from_secs(self.call_waiting_interval_seconds.into()),
        }
    }

    pub fn station_policy(&self) -> GeneralStationPolicy {
        GeneralStationPolicy {
            timezone_offset_minutes: self.timezone_offset_minutes,
            date_template: self.date_template.clone(),
            ring_type: self.ring_type,
            call_waiting_tone: self.call_waiting_tone,
        }
    }
}

impl fmt::Debug for GeneralConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GeneralConfig")
            .field("bind", &self.bind)
            .field("advertised_address", &self.advertised_address)
            .field("server_name", &self.server_name)
            .field("language", &self.language)
            .field(
                "account_code",
                &self.account_code.as_ref().map(|_| "<redacted>"),
            )
            .field("keepalive_seconds", &self.keepalive_seconds)
            .field(
                "secondary_keepalive_seconds",
                &self.secondary_keepalive_seconds,
            )
            .field("signaling_servers", &self.signaling_servers)
            .field("codecs", &self.codecs)
            .field("audio_encryption", &self.audio_encryption)
            .finish_non_exhaustive()
    }
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([0, 0, 0, 0], 2000)),
            advertised_address: Ipv4Addr::LOCALHOST,
            server_name: "Asterisk SCCP".into(),
            language: "en".into(),
            account_code: None,
            keepalive_seconds: 30,
            secondary_keepalive_seconds: 30,
            signaling_servers: Vec::new(),
            first_digit_timeout_ms: 10_000,
            interdigit_timeout_ms: 5_000,
            dial_terminator: DialTerminatorConfig::default(),
            simulate_enbloc: true,
            speed_dial_await_further_digits: false,
            allow_overlap: false,
            transfer_on_hangup: false,
            call_answer_order: CallAnswerOrder::default(),
            timezone_offset_minutes: 0,
            date_template: DateTemplate::default(),
            ring_type: RingerMode::Outside,
            call_waiting_tone: Some(Tone::CallWaiting),
            call_waiting_interval_seconds: 0,
            // This is an allow-set, not a hidden preference imposed on a
            // registered station. Runtime negotiation preserves the phone's
            // advertised order and lets Asterisk choose the PBX format.
            codecs: mapped_audio_codecs(),
            audio_encryption: MediaEncryptionPolicy::default(),
            conference_dialing: ConferenceDialingConfig::default(),
            auto_answer: AutoAnswerConfig::default(),
            remote_hangup_tone: None,
            guest_hotline: GuestHotlineConfig::default(),
            direct_media: false,
            early_media: true,
            audio_processing: AudioProcessingPolicy::default(),
            jitter_buffer: JitterBufferConfig::default(),
            registration: RegistrationConfig::default(),
            fallback_registration: FallbackRegistrationConfig::default(),
            network: NetworkPolicy::default(),
            qos: QosPolicy::default(),
            listeners: ListenerPolicy::default(),
            realtime_tables: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CallAnswerOrder {
    #[default]
    OldestFirst,
    LastFirst,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DialTerminatorConfig {
    pub character: char,
    pub record: bool,
}

impl Default for DialTerminatorConfig {
    fn default() -> Self {
        Self {
            character: '#',
            record: false,
        }
    }
}

/// Global dialplan contexts populated while configured lines are registered.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RegistrationConfig {
    /// Ordered, delimiter-free context names from `regcontext`.
    pub contexts: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FallbackRegistrationConfig {
    pub decision: FallbackDecision,
    pub backoff_seconds: u32,
    pub server_priority: u8,
}

impl Default for FallbackRegistrationConfig {
    fn default() -> Self {
        Self {
            decision: FallbackDecision::Reject,
            backoff_seconds: 60,
            server_priority: 1,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FallbackDecision {
    Reject,
    Accept,
    DeviceIdOdd,
    DeviceIdEven,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RealtimeTableConfig {
    pub device_family: String,
    pub line_family: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AclAction {
    Deny,
    Permit,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IpNetwork {
    pub address: IpAddr,
    pub prefix: u8,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AclRule {
    pub action: AclAction,
    pub network: IpNetwork,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AccessControlList {
    /// First-to-last ordered rules. An empty list imposes no address filter.
    pub rules: Vec<AclRule>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum NatMode {
    #[default]
    Auto,
    Off,
    AutoOff,
    On,
    AutoOn,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExternalAddress {
    Address(IpAddr),
    Hostname { name: String, refresh_seconds: u32 },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdvertisedAddresses {
    pub ipv4: Option<Ipv4Addr>,
    pub ipv6: Option<Ipv6Addr>,
}

impl Default for AdvertisedAddresses {
    fn default() -> Self {
        Self {
            ipv4: Some(Ipv4Addr::LOCALHOST),
            ipv6: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NetworkPolicy {
    pub acl: AccessControlList,
    pub local_networks: Vec<IpNetwork>,
    pub external: Option<ExternalAddress>,
    pub advertised: AdvertisedAddresses,
    pub nat: NatMode,
}

impl Default for NetworkPolicy {
    fn default() -> Self {
        Self {
            acl: AccessControlList::default(),
            local_networks: internal_networks(),
            external: None,
            advertised: AdvertisedAddresses::default(),
            nat: NatMode::Auto,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Dscp(pub u8);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cos(pub u8);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QosClass {
    pub dscp: Dscp,
    pub cos: Cos,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QosPolicy {
    pub signaling: QosClass,
    pub audio: QosClass,
    pub video: QosClass,
}

impl Default for QosPolicy {
    fn default() -> Self {
        Self {
            signaling: QosClass {
                dscp: Dscp(26),
                cos: Cos(4),
            },
            audio: QosClass {
                dscp: Dscp(46),
                cos: Cos(6),
            },
            video: QosClass {
                dscp: Dscp(34),
                cos: Cos(5),
            },
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub enum TlsCredentials {
    CombinedPem(PathBuf),
    SplitPem {
        certificate: PathBuf,
        private_key: PathBuf,
        trust_store: Option<PathBuf>,
    },
}

impl fmt::Debug for TlsCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CombinedPem(_) => formatter.write_str("CombinedPem(<redacted>)"),
            Self::SplitPem { trust_store, .. } => formatter
                .debug_struct("SplitPem")
                .field("certificate", &"<redacted>")
                .field("private_key", &"<redacted>")
                .field("trust_store", &trust_store.as_ref().map(|_| "<redacted>"))
                .finish(),
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct TlsListener {
    pub bind: SocketAddr,
    pub credentials: TlsCredentials,
}

impl fmt::Debug for TlsListener {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TlsListener")
            .field("bind", &self.bind)
            .field("credentials", &self.credentials)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ListenerPolicy {
    pub clear: SocketAddr,
    pub tls: Option<TlsListener>,
}

impl Default for ListenerPolicy {
    fn default() -> Self {
        Self {
            clear: SocketAddr::from(([0, 0, 0, 0], 2000)),
            tls: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TransportRequirement {
    Clear,
    Tls,
    #[default]
    Either,
}

impl From<TransportRequirement> for StationTransportRequirement {
    fn from(requirement: TransportRequirement) -> Self {
        match requirement {
            TransportRequirement::Clear => Self::Clear,
            TransportRequirement::Tls => Self::Secure,
            TransportRequirement::Either => Self::Either,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceNetworkPolicy {
    pub acl: AccessControlList,
    pub permitted_hosts: Vec<String>,
    pub nat: NatMode,
    pub qos: QosPolicy,
    pub transport: TransportRequirement,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DndMode {
    #[default]
    Off,
    Silent,
    Reject,
}

/// State transition selected by one configured DND feature button.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DndButtonMode {
    #[default]
    Cycle,
    Silent,
    Reject,
}

impl DndButtonMode {
    const fn canonical(self) -> Option<&'static str> {
        match self {
            Self::Cycle => None,
            Self::Silent => Some("silent"),
            Self::Reject => Some("reject"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForwardingDefaults {
    pub all_enabled: bool,
    pub busy_enabled: bool,
    pub no_answer_enabled: bool,
    pub no_answer_timeout_seconds: u32,
    pub all: Option<ForwardingDestination>,
    pub busy: Option<ForwardingDestination>,
    pub no_answer: Option<ForwardingDestination>,
}

impl Default for ForwardingDefaults {
    fn default() -> Self {
        Self {
            all_enabled: true,
            busy_enabled: true,
            no_answer_enabled: true,
            no_answer_timeout_seconds: 30,
            all: None,
            busy: None,
            no_answer: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceFeatureDefaults {
    pub forwarding: ForwardingDefaults,
    pub dnd_enabled: bool,
    pub dnd: DndMode,
    pub privacy_enabled: bool,
    pub privacy: bool,
    /// Every configured feature-button instance, including defaults that are
    /// explicitly or implicitly false.
    pub buttons: HashMap<u32, bool>,
}

impl Default for DeviceFeatureDefaults {
    fn default() -> Self {
        Self {
            forwarding: ForwardingDefaults::default(),
            dnd_enabled: true,
            dnd: DndMode::Off,
            privacy_enabled: true,
            privacy: false,
            buttons: HashMap::new(),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct VoicemailDefaults {
    pub number: Option<VoicemailDestination>,
    pub transfer_destination: Option<VoicemailDestination>,
}

impl VoicemailDefaults {
    pub fn divert_destination(&self) -> Option<&VoicemailDestination> {
        self.transfer_destination.as_ref()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PickupConfig {
    pub call_groups: BTreeSet<u8>,
    pub pickup_groups: BTreeSet<u8>,
    pub named_call_groups: BTreeSet<String>,
    pub named_pickup_groups: BTreeSet<String>,
    pub directed: bool,
    /// `None` means use the line's normal dialplan context.
    pub directed_context: Option<String>,
    pub answer_directed: bool,
}

impl Default for PickupConfig {
    fn default() -> Self {
        Self {
            call_groups: BTreeSet::new(),
            pickup_groups: BTreeSet::new(),
            named_call_groups: BTreeSet::new(),
            named_pickup_groups: BTreeSet::new(),
            directed: true,
            directed_context: None,
            answer_directed: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ParkingRetrievalBehavior {
    /// Immediately retrieve the call when the lot contains exactly one call;
    /// otherwise show the parked-call menu.
    #[default]
    RetrieveSingle,
    /// Show the parked-call menu even when the lot contains one call.
    AlwaysShowMenu,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParkingLotButtonConfig {
    pub lot: String,
    pub retrieval: ParkingRetrievalBehavior,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceParkingConfig {
    pub enabled: bool,
    /// Typed settings keyed by feature-button instance.
    pub feature_buttons: HashMap<u32, ParkingLotButtonConfig>,
}

impl Default for DeviceParkingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            feature_buttons: HashMap::new(),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LineParkingConfig {
    /// Named Asterisk parking lot selected when this line parks a call.
    pub lot: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConferenceDialingConfig {
    pub enabled: bool,
    /// Opaque application option string. Its interpretation belongs to the
    /// selected Asterisk conference application.
    pub application_options: String,
}

impl Default for ConferenceDialingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            application_options: "qxd".into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceConferenceConfig {
    pub allowed: bool,
    /// `None` explicitly disables conference music on hold.
    pub music_on_hold_class: Option<String>,
    pub play_general_announcements: bool,
    pub play_participant_announcements: bool,
    pub mute_on_entry: bool,
    pub show_conference_list: bool,
    pub dialing: ConferenceDialingConfig,
}

impl Default for DeviceConferenceConfig {
    fn default() -> Self {
        Self {
            allowed: true,
            music_on_hold_class: Some("default".into()),
            play_general_announcements: true,
            play_participant_announcements: true,
            mute_on_entry: false,
            show_conference_list: true,
            dialing: ConferenceDialingConfig::default(),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LineConferenceConfig {
    /// `None` inherits the device conference-dialing default.
    pub enabled: Option<bool>,
    pub destination: Option<String>,
    /// `None` inherits device options. `Some("")` explicitly supplies no
    /// application options.
    pub application_options: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedConferenceDialing {
    pub enabled: bool,
    pub destination: Option<String>,
    pub application_options: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AutoAnswerConfig {
    pub ring_time_seconds: u32,
    pub tone: Tone,
}

impl Default for AutoAnswerConfig {
    fn default() -> Self {
        Self {
            ring_time_seconds: 1,
            tone: Tone::Zip,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GuestHotlineConfig {
    /// Whether an otherwise unknown device may register on the shared guest
    /// hotline line.
    pub enabled: bool,
    pub extension: Option<HotlineDestination>,
    pub context: String,
    pub label: String,
}

impl Default for GuestHotlineConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            extension: Some(
                HotlineDestination::new("111").expect("built-in guest-hotline extension is valid"),
            ),
            context: "default".into(),
            label: "hotline".into(),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LineHotlineConfig {
    /// Destination dialed when this configured line goes off-hook without an
    /// explicitly selected line.
    pub destination: Option<HotlineDestination>,
}

/// Tones used while an outbound call is collecting digits on a logical line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LineDialToneConfig {
    pub initial: Tone,
    /// An empty configured value disables the secondary dial tone.
    pub secondary_prefix: Option<String>,
    pub secondary: Tone,
}

impl Default for LineDialToneConfig {
    fn default() -> Self {
        Self {
            initial: Tone::InsideDial,
            secondary_prefix: None,
            secondary: Tone::OutsideDial,
        }
    }
}

/// A line's optional PIN for handset Extension Mobility login.
///
/// The value deliberately has a redacted `Debug` representation so a complete
/// normalized configuration can be logged without exposing credentials.
#[derive(Clone, Eq, PartialEq)]
pub struct MobilityPin(String);

impl MobilityPin {
    /// Verify a candidate without returning early for a mismatched byte or
    /// length. Both inputs are bounded by [`MAX_MOBILITY_PIN_DIGITS`], so every
    /// verification performs exactly that many byte comparisons.
    pub fn verify(&self, candidate: &str) -> bool {
        let expected = self.0.as_bytes();
        let actual = candidate.as_bytes();
        let mut difference = expected.len() ^ actual.len();
        for index in 0..MAX_MOBILITY_PIN_DIGITS {
            let expected = expected.get(index).copied().unwrap_or_default();
            let actual = actual.get(index).copied().unwrap_or_default();
            difference |= usize::from(expected ^ actual);
        }
        difference == 0
    }

    pub fn digits(&self) -> usize {
        self.0.len()
    }
}

impl fmt::Debug for MobilityPin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("MobilityPin(<redacted>)")
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LineMobilityConfig {
    pub pin: Option<MobilityPin>,
}

/// One configured registration extension before global-context expansion.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RegistrationExtension {
    pub extension: String,
    /// An explicit context overrides the global context list for this entry.
    pub context: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LineRegistrationConfig {
    /// Ordered, delimiter-free entries from `regexten`. An omitted or empty
    /// value normalizes to the logical line number.
    pub extensions: Vec<RegistrationExtension>,
}

/// A fully resolved extension/context pair ready for a dialplan adapter.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RegistrationTarget {
    pub extension: String,
    pub context: String,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum VideoMode {
    Off,
    User,
    #[default]
    Auto,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum JitterBufferImplementation {
    #[default]
    Fixed,
    Adaptive,
}

/// Global Asterisk receive-side jitter-buffer policy. These settings are not
/// valid in device or line sections.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JitterBufferConfig {
    pub enabled: bool,
    pub forced: bool,
    pub log_frames: bool,
    pub max_size_ms: u32,
    pub resync_threshold_ms: u32,
    pub implementation: JitterBufferImplementation,
}

impl Default for JitterBufferConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            forced: false,
            log_frames: false,
            max_size_ms: 200,
            resync_threshold_ms: 1_000,
            implementation: JitterBufferImplementation::Fixed,
        }
    }
}

impl JitterBufferConfig {
    pub const fn should_configure_channel(self, direct_media: bool) -> bool {
        self.enabled && (self.forced || !direct_media)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceMediaConfig {
    pub codecs: Vec<Codec>,
    pub audio_encryption: MediaEncryptionPolicy,
    pub dtmf_mode: DtmfMode,
    pub direct_media: bool,
    pub early_media: bool,
}

impl Default for DeviceMediaConfig {
    fn default() -> Self {
        Self {
            codecs: GeneralConfig::default().codecs,
            audio_encryption: MediaEncryptionPolicy::default(),
            dtmf_mode: DtmfMode::Auto,
            direct_media: false,
            early_media: true,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LineMediaConfig {
    pub codecs: Vec<Codec>,
    pub audio_encryption: MediaEncryptionPolicy,
    pub video_mode: VideoMode,
    pub audio_processing: AudioProcessingPolicy,
}

impl Default for LineMediaConfig {
    fn default() -> Self {
        Self {
            codecs: GeneralConfig::default().codecs,
            audio_encryption: MediaEncryptionPolicy::default(),
            video_mode: VideoMode::Auto,
            audio_processing: AudioProcessingPolicy::default(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedMediaConfig {
    pub codecs: Vec<Codec>,
    pub audio_encryption: MediaEncryptionPolicy,
    pub dtmf_mode: DtmfMode,
    pub direct_media: bool,
    pub early_media: bool,
    pub video_mode: VideoMode,
    pub audio_processing: AudioProcessingPolicy,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RedialMode {
    #[default]
    LastNumber,
    PlacedCallsMenu,
}

/// Per-device call-history and hinted-line presentation policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceCallUiConfig {
    pub redial_mode: RedialMode,
    pub hinted_ringing_notification: bool,
    pub mwi_lamp_mode: LampMode,
    pub mwi_on_call: bool,
    pub legacy_code_page: LegacyCodePage,
}

impl Default for DeviceCallUiConfig {
    fn default() -> Self {
        Self {
            redial_mode: RedialMode::LastNumber,
            hinted_ringing_notification: false,
            mwi_lamp_mode: LampMode::On,
            mwi_on_call: false,
            legacy_code_page: LegacyCodePage::Iso8859_1,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LineFeatureConfig {
    pub incoming_limit: u32,
    pub voicemail: VoicemailDefaults,
    pub pickup: PickupConfig,
    pub parking: LineParkingConfig,
    pub conference: LineConferenceConfig,
    pub hotline: LineHotlineConfig,
    pub dial_tones: LineDialToneConfig,
    pub mobility: LineMobilityConfig,
    pub registration: LineRegistrationConfig,
    pub media: LineMediaConfig,
}

impl Default for LineFeatureConfig {
    fn default() -> Self {
        Self {
            incoming_limit: 6,
            voicemail: VoicemailDefaults::default(),
            pickup: PickupConfig::default(),
            parking: LineParkingConfig::default(),
            conference: LineConferenceConfig::default(),
            hotline: LineHotlineConfig::default(),
            dial_tones: LineDialToneConfig::default(),
            mobility: LineMobilityConfig::default(),
            registration: LineRegistrationConfig::default(),
            media: LineMediaConfig::default(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceConfig {
    pub id: DeviceId,
    pub description: String,
    /// Line names in line-instance order, retained for channel lookups.
    pub lines: Vec<String>,
    /// Physical station buttons in configuration order.
    pub buttons: Vec<ButtonDefinition>,
    /// Optional feature arguments keyed by feature-button instance.
    pub feature_arguments: HashMap<u32, String>,
    /// Ordered, validated PBX variables applied before logical-line values.
    pub channel_variables: Vec<ChannelVariable>,
    /// Canonical name of the resolved reusable soft-key profile.
    pub soft_key_profile: String,
    /// Initial mutable feature state and feature availability.
    pub feature_defaults: DeviceFeatureDefaults,
    pub parking: DeviceParkingConfig,
    pub conference: DeviceConferenceConfig,
    pub call_ui: DeviceCallUiConfig,
    pub allow_overlap: bool,
    pub media: DeviceMediaConfig,
    pub network: DeviceNetworkPolicy,
}

#[derive(Clone, Eq, PartialEq)]
pub struct LineConfig {
    pub number: String,
    pub label: String,
    pub context: String,
    pub caller_name: String,
    pub caller_number: String,
    pub mailbox: Option<String>,
    pub language: String,
    pub account_code: Option<String>,
    /// Ordered, validated PBX variables applied after device values.
    pub channel_variables: Vec<ChannelVariable>,
}

impl fmt::Debug for LineConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LineConfig")
            .field("number", &self.number)
            .field("label", &self.label)
            .field("context", &self.context)
            .field("caller_name", &self.caller_name)
            .field("caller_number", &self.caller_number)
            .field("mailbox", &self.mailbox)
            .field("language", &self.language)
            .field(
                "account_code",
                &self.account_code.as_ref().map(|_| "<redacted>"),
            )
            .field("channel_variables", &self.channel_variables)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LineBinding {
    pub device_id: DeviceId,
    pub line_instance: u32,
    pub appearance: LineAppearance,
    pub line: LineConfig,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModuleConfig {
    pub general: GeneralConfig,
    pub devices: HashMap<DeviceId, DeviceConfig>,
    pub lines: HashMap<String, LineConfig>,
    pub line_features: HashMap<String, LineFeatureConfig>,
    pub soft_key_profiles: HashMap<String, SoftKeyProfile>,
    bindings: Vec<LineBinding>,
    bindings_by_line: HashMap<String, Vec<usize>>,
    bindings_by_device: HashMap<DeviceId, Vec<usize>>,
    binding_by_button: HashMap<(DeviceId, u32), usize>,
    device_codec_overrides: HashSet<DeviceId>,
    line_codec_overrides: HashSet<String>,
    device_audio_encryption_overrides: HashSet<DeviceId>,
    line_audio_encryption_overrides: HashSet<String>,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum ConfigError {
    #[error("line {line}: {message}")]
    Syntax { line: usize, message: String },
    #[error("section [{section}] has an unknown type {kind}")]
    UnknownSectionType { section: String, kind: String },
    #[error("section [{0}] is missing type=device, type=line, or type=softkey_profile")]
    MissingSectionType(String),
    #[error("duplicate section [{0}]")]
    DuplicateSection(String),
    #[error("{key}: invalid value {value}")]
    InvalidValue { key: String, value: String },
    #[error("device {device} references unknown line {line}")]
    UnknownLine { device: DeviceId, line: String },
    #[error("device {device} references unknown soft-key profile {profile}")]
    UnknownSoftKeyProfile { device: DeviceId, profile: String },
    #[error("section [{section}] references missing template [{parent}]")]
    MissingTemplate { section: String, parent: String },
    #[error("section [{section}] references non-template section [{parent}]")]
    ParentIsNotTemplate { section: String, parent: String },
    #[error(
        "section [{section}] is type={child_kind} but template [{parent}] is type={parent_kind}"
    )]
    WrongTemplateKind {
        section: String,
        child_kind: String,
        parent: String,
        parent_kind: String,
    },
    #[error("template [{section}] must resolve to type=device or type=line, got {kind}")]
    InvalidTemplateKind { section: String, kind: String },
    #[error("inheritance cycle: {0}")]
    InheritanceCycle(String),
    #[error("line {0} is not assigned to a device")]
    UnassignedLine(String),
    #[error("device {0} has no lines")]
    DeviceWithoutLines(DeviceId),
    #[error("configuration must contain at least one device and one line")]
    Empty,
    #[error("invalid SCCP device ID: {0}")]
    InvalidDevice(String),
}

#[derive(Clone, Default)]
struct RawSection {
    name: String,
    line: usize,
    is_template: bool,
    parents: Vec<String>,
    values: Vec<RawValue>,
}

#[derive(Clone)]
struct RawValue {
    key: String,
    value: String,
    line: usize,
    section: String,
}

/// Serde is the authoritative spelling table for general options. Aliases are
/// accepted production inputs; serialization always yields the canonical
/// Asterisk-style spelling used by examples and diagnostics.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum GeneralOption {
    #[serde(rename = "dateformat")]
    DateFormat,
    #[serde(rename = "tzoffset")]
    TimezoneOffset,
    #[serde(alias = "clearbind")]
    Bind,
    #[serde(alias = "bindaddr", alias = "clearbindaddr")]
    BindAddress,
    #[serde(alias = "clearport")]
    Port,
    AdvertisedAddress,
    #[serde(alias = "advertisedaddressipv4")]
    AdvertisedIpv4,
    #[serde(alias = "advertisedaddressipv6")]
    AdvertisedIpv6,
    #[serde(alias = "securebind")]
    TlsBind,
    #[serde(alias = "secbindaddr", alias = "tlsbindaddr")]
    TlsBindAddress,
    #[serde(alias = "secport", alias = "tlsport")]
    TlsPort,
    #[serde(alias = "certfile", alias = "tlscombinedpem")]
    TlsCombinedPem,
    #[serde(alias = "tlscertificatefile")]
    TlsCertificate,
    #[serde(alias = "tlsprivatekeyfile")]
    TlsPrivateKey,
    #[serde(alias = "tlscafile")]
    TlsTrustStore,
    Deny,
    Permit,
    #[serde(rename = "localnet")]
    LocalNetwork,
    #[serde(rename = "externip", alias = "externaladdress")]
    ExternalAddress,
    #[serde(rename = "externhost", alias = "externalhost")]
    ExternalHost,
    #[serde(rename = "externrefresh", alias = "externalrefresh")]
    ExternalRefresh,
    Nat,
    #[serde(rename = "sccp_tos", alias = "signalingtos")]
    SignalingTos,
    #[serde(
        rename = "sccp_dscp",
        alias = "sccpdscp",
        alias = "signalingdscp",
        alias = "signaling_dscp"
    )]
    SignalingDscp,
    #[serde(rename = "sccp_cos", alias = "signalingcos", alias = "signaling_cos")]
    SignalingCos,
    #[serde(alias = "audiotos")]
    AudioTos,
    #[serde(alias = "audiodscp")]
    AudioDscp,
    #[serde(alias = "audiocos")]
    AudioCos,
    #[serde(alias = "videotos")]
    VideoTos,
    #[serde(alias = "videodscp")]
    VideoDscp,
    #[serde(alias = "videocos")]
    VideoCos,
    #[serde(alias = "trustphoneip")]
    TrustPhoneIp,
    #[serde(alias = "servername")]
    ServerName,
    Language,
    #[serde(rename = "accountcode")]
    AccountCode,
    Keepalive,
    SecondaryKeepalive,
    SignalingServer,
    #[serde(alias = "firstdigittimeout")]
    FirstDigitTimeout,
    InterdigitTimeoutMs,
    #[serde(alias = "digittimeout")]
    DigitTimeout,
    #[serde(alias = "digittimeoutchar")]
    DigitTimeoutChar,
    #[serde(alias = "recorddigittimeoutchar")]
    RecordDigitTimeoutChar,
    SimulateEnbloc,
    #[serde(alias = "speeddialawaitfurtherdigits")]
    SpeedDialAwaitFurtherDigits,
    #[serde(alias = "allowoverlap")]
    AllowOverlap,
    TransferOnHangup,
    #[serde(alias = "callanswerorder")]
    CallAnswerOrder,
    #[serde(alias = "ringtype")]
    RingType,
    #[serde(alias = "callwaitingtone")]
    CallWaitingTone,
    #[serde(alias = "callwaitinginterval")]
    CallWaitingInterval,
    Fallback,
    BackoffTime,
    ServerPriority,
    Allow,
    Disallow,
    #[serde(rename = "meetme")]
    ConferenceEnabled,
    #[serde(rename = "meetmeopts")]
    ConferenceOptions,
    #[serde(alias = "autoanswerringtime")]
    AutoanswerRingTime,
    #[serde(alias = "autoanswertone")]
    AutoanswerTone,
    #[serde(alias = "remotehangup_tone")]
    RemoteHangupTone,
    #[serde(alias = "hotlineenabled")]
    HotlineEnabled,
    #[serde(alias = "hotlineextension")]
    HotlineExtension,
    #[serde(alias = "hotlinecontext")]
    HotlineContext,
    #[serde(alias = "hotlinelabel")]
    HotlineLabel,
    #[serde(rename = "direct_media", alias = "directrtp")]
    DirectMedia,
    #[serde(rename = "early_media", alias = "earlyrtp")]
    EarlyMedia,
    #[serde(alias = "audioencryption")]
    AudioEncryption,
    #[serde(rename = "echocancel")]
    EchoCancel,
    #[serde(rename = "silencesuppression")]
    SilenceSuppression,
    #[serde(alias = "jbenable")]
    JbEnable,
    #[serde(alias = "jbforce")]
    JbForce,
    #[serde(alias = "jblog")]
    JbLog,
    #[serde(alias = "jbmaxsize")]
    JbMaxSize,
    #[serde(alias = "jbresyncthreshold")]
    JbResyncThreshold,
    #[serde(alias = "jbimpl")]
    JbImplementation,
    #[serde(rename = "regcontext")]
    RegistrationContext,
    #[serde(alias = "devicetable")]
    DeviceTable,
    #[serde(alias = "linetable")]
    LineTable,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum LineOption {
    Type,
    Label,
    Context,
    #[serde(rename = "callerid")]
    CallerId,
    #[serde(alias = "incominglimit")]
    IncomingLimit,
    Language,
    #[serde(rename = "accountcode")]
    AccountCode,
    #[serde(rename = "setvar")]
    SetVariable,
    Mailbox,
    #[serde(alias = "vmnum", alias = "voicemailnumber")]
    VoicemailNumber,
    #[serde(
        alias = "trnsfvm",
        alias = "voicemailtransfer",
        alias = "transfertovoicemail"
    )]
    VoicemailTransfer,
    #[serde(alias = "callgroup")]
    CallGroup,
    #[serde(alias = "pickupgroup")]
    PickupGroup,
    #[serde(alias = "namedcallgroup")]
    NamedCallGroup,
    #[serde(alias = "namedpickupgroup")]
    NamedPickupGroup,
    #[serde(alias = "directedpickup")]
    DirectedPickup,
    #[serde(alias = "directedpickupcontext")]
    DirectedPickupContext,
    #[serde(alias = "pickupmodeanswer", alias = "directedpickupmodeanswer")]
    PickupModeAnswer,
    #[serde(rename = "parkinglot")]
    ParkingLot,
    #[serde(rename = "meetme")]
    ConferenceEnabled,
    #[serde(rename = "meetmenum")]
    ConferenceNumber,
    #[serde(rename = "meetmeopts")]
    ConferenceOptions,
    #[serde(alias = "adhocnumber")]
    AdhocNumber,
    InitialDialtoneTone,
    SecondaryDialtoneDigits,
    SecondaryDialtoneTone,
    Pin,
    #[serde(rename = "regexten")]
    RegistrationExtension,
    Allow,
    Disallow,
    #[serde(alias = "videomode")]
    VideoMode,
    #[serde(alias = "audioencryption")]
    AudioEncryption,
    #[serde(rename = "echocancel")]
    EchoCancel,
    #[serde(rename = "silencesuppression")]
    SilenceSuppression,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum DeviceOption {
    Type,
    Description,
    #[serde(alias = "softkeyprofile")]
    SoftkeyProfile,
    #[serde(
        rename = "cfwdall",
        alias = "forwardallenabled",
        alias = "forward_all_enabled"
    )]
    ForwardAllEnabled,
    #[serde(
        rename = "cfwdbusy",
        alias = "forwardbusyenabled",
        alias = "forward_busy_enabled"
    )]
    ForwardBusyEnabled,
    #[serde(
        rename = "cfwdnoanswer",
        alias = "forwardnoanswerenabled",
        alias = "forward_no_answer_enabled"
    )]
    ForwardNoAnswerEnabled,
    #[serde(
        rename = "forward_no_answer_timeout",
        alias = "cfwdnoanswertimeout",
        alias = "forwardnoanswertimeout"
    )]
    ForwardNoAnswerTimeout,
    ForwardAll,
    ForwardBusy,
    ForwardNoAnswer,
    #[serde(alias = "dndfeature")]
    DndFeature,
    Dnd,
    #[serde(
        rename = "privacy_feature",
        alias = "private",
        alias = "privacyfeature"
    )]
    PrivacyFeature,
    Privacy,
    #[serde(alias = "featuredefault")]
    FeatureDefault,
    #[serde(rename = "setvar")]
    SetVariable,
    Park,
    #[serde(rename = "conf_allow", alias = "confallow", alias = "conference_allow")]
    ConferenceAllow,
    #[serde(
        rename = "conf_music_on_hold_class",
        alias = "confmusiconholdclass",
        alias = "conference_music_on_hold_class"
    )]
    ConferenceMusicOnHoldClass,
    #[serde(
        rename = "conf_play_general_announce",
        alias = "confplaygeneralannounce",
        alias = "conference_play_general_announce"
    )]
    ConferencePlayGeneralAnnounce,
    #[serde(
        rename = "conf_play_part_announce",
        alias = "confplaypartannounce",
        alias = "conference_play_participant_announce"
    )]
    ConferencePlayParticipantAnnounce,
    #[serde(
        rename = "conf_mute_on_entry",
        alias = "confmuteonentry",
        alias = "conference_mute_on_entry"
    )]
    ConferenceMuteOnEntry,
    #[serde(
        rename = "conf_show_conflist",
        alias = "confshowconflist",
        alias = "conference_show_list"
    )]
    ConferenceShowList,
    #[serde(rename = "meetme")]
    ConferenceDialingEnabled,
    #[serde(rename = "meetmeopts")]
    ConferenceOptions,
    #[serde(alias = "useredialmenu")]
    UseRedialMenu,
    #[serde(alias = "allowringinnotification")]
    AllowRinginNotification,
    #[serde(alias = "mwilamp")]
    MwiLamp,
    #[serde(alias = "mwioncall")]
    MwiOnCall,
    #[serde(alias = "phonecodepage")]
    PhoneCodePage,
    #[serde(alias = "allowoverlap")]
    AllowOverlap,
    #[serde(alias = "forcedtmfmode", alias = "force_dtmfmode")]
    ForceDtmfMode,
    #[serde(rename = "direct_media", alias = "directrtp")]
    DirectMedia,
    #[serde(rename = "early_media", alias = "earlyrtp")]
    EarlyMedia,
    #[serde(alias = "audioencryption")]
    AudioEncryption,
    Deny,
    Permit,
    #[serde(alias = "permithost")]
    PermitHost,
    Nat,
    #[serde(alias = "transportrequirement", alias = "transport_requirement")]
    Transport,
    #[serde(rename = "sccp_tos", alias = "signalingtos")]
    SignalingTos,
    #[serde(
        rename = "sccp_dscp",
        alias = "sccpdscp",
        alias = "signalingdscp",
        alias = "signaling_dscp"
    )]
    SignalingDscp,
    #[serde(rename = "sccp_cos", alias = "signalingcos", alias = "signaling_cos")]
    SignalingCos,
    #[serde(alias = "audiotos")]
    AudioTos,
    #[serde(alias = "audiodscp")]
    AudioDscp,
    #[serde(alias = "audiocos")]
    AudioCos,
    #[serde(alias = "videotos")]
    VideoTos,
    #[serde(alias = "videodscp")]
    VideoDscp,
    #[serde(alias = "videocos")]
    VideoCos,
    #[serde(alias = "trustphoneip")]
    TrustPhoneIp,
    #[serde(alias = "dtmfmode")]
    ObsoleteDtmfMode,
    Allow,
    Disallow,
    Line,
    Button,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConfigOverlayKind {
    Device,
    Line,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ConfigOverlayValue {
    pub key: String,
    /// `None` deletes the matching file value. `Some("")` is an explicit
    /// empty override and therefore remains present through inheritance.
    pub value: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ConfigOverlaySection {
    pub name: String,
    pub source: String,
    pub line: usize,
    pub kind: Option<ConfigOverlayKind>,
    pub delete: bool,
    pub values: Vec<ConfigOverlayValue>,
}

impl RawSection {
    fn diagnostic_key(&self, key: &str) -> String {
        if let Some(value) = self
            .values
            .iter()
            .rev()
            .find(|value| value.key.eq_ignore_ascii_case(key))
        {
            value.diagnostic_key()
        } else {
            format!("line {} [{}].{key}", self.line, self.name)
        }
    }

    fn section_location(&self) -> String {
        format!("line {} [{}]", self.line, self.name)
    }
}

impl RawValue {
    fn diagnostic_key(&self) -> String {
        format!("line {} [{}].{}", self.line, self.section, self.key)
    }
}

#[derive(Default)]
struct ButtonInstances {
    line: u32,
    speed_dial: u32,
    feature: u32,
    service: u32,
}

impl ButtonInstances {
    fn next(counter: &mut u32) -> u32 {
        *counter += 1;
        *counter
    }
}

struct ParsedButton {
    definition: ButtonDefinition,
    feature_argument: Option<(u32, String)>,
}

struct ParsedLine {
    line: LineConfig,
    features: LineFeatureConfig,
}

/// Values collected while one line section is decoded. Keeping the unresolved
/// values together makes the parse/resolve boundary explicit: Serde owns key
/// selection, this draft owns typed values and presence, and the final resolver
/// applies general inheritance and cross-field validation.
#[derive(Default)]
struct LineSectionDraft<'a> {
    incoming_limit: Option<u32>,
    mailbox: Option<Option<String>>,
    voicemail_number: Option<Option<VoicemailDestination>>,
    voicemail_transfer: Option<Option<VoicemailDestination>>,
    call_groups: Option<BTreeSet<u8>>,
    pickup_groups: Option<BTreeSet<u8>>,
    named_call_groups: Option<BTreeSet<String>>,
    named_pickup_groups: Option<BTreeSet<String>>,
    directed_pickup: Option<bool>,
    directed_pickup_context: Option<Option<String>>,
    pickup_mode_answer: Option<bool>,
    parking_lot: Option<Option<String>>,
    conference_enabled: Option<bool>,
    conference_destination: Option<Option<String>>,
    conference_options: Option<String>,
    hotline_destination: Option<Option<HotlineDestination>>,
    initial_dialtone_tone: Option<Tone>,
    secondary_dialtone_digits: Option<Option<String>>,
    secondary_dialtone_tone: Option<Tone>,
    mobility_pin: Option<Option<MobilityPin>>,
    registration_extensions: Option<Option<Vec<RegistrationExtension>>>,
    video_mode: Option<VideoMode>,
    audio_encryption: Option<MediaEncryptionPolicy>,
    echo_cancellation: Option<bool>,
    silence_suppression: Option<bool>,
    language: Option<String>,
    account_code: Option<Option<String>>,
    channel_variables: Vec<ChannelVariable>,
    codec_settings: Vec<(bool, &'a str)>,
}

#[derive(Default)]
struct QosPolicyPatch {
    signaling_dscp: Option<Dscp>,
    signaling_cos: Option<Cos>,
    audio_dscp: Option<Dscp>,
    audio_cos: Option<Cos>,
    video_dscp: Option<Dscp>,
    video_cos: Option<Cos>,
}

impl QosPolicyPatch {
    fn resolve(self, mut base: QosPolicy) -> QosPolicy {
        base.signaling.dscp = self.signaling_dscp.unwrap_or(base.signaling.dscp);
        base.signaling.cos = self.signaling_cos.unwrap_or(base.signaling.cos);
        base.audio.dscp = self.audio_dscp.unwrap_or(base.audio.dscp);
        base.audio.cos = self.audio_cos.unwrap_or(base.audio.cos);
        base.video.dscp = self.video_dscp.unwrap_or(base.video.dscp);
        base.video.cos = self.video_cos.unwrap_or(base.video.cos);
        base
    }
}

/// Unresolved values for one device section. Optional collections preserve the
/// difference between inheritance (`None`) and an explicitly cleared list
/// (`Some(Vec::new())`).
#[derive(Default)]
struct DeviceSectionDraft<'a> {
    buttons: Vec<ButtonDefinition>,
    feature_arguments: HashMap<u32, String>,
    instances: ButtonInstances,
    soft_key_profile: Option<String>,
    forward_all_enabled: Option<bool>,
    forward_busy_enabled: Option<bool>,
    forward_no_answer_enabled: Option<bool>,
    forward_no_answer_timeout: Option<u32>,
    forward_all: Option<Option<ForwardingDestination>>,
    forward_busy: Option<Option<ForwardingDestination>>,
    forward_no_answer: Option<Option<ForwardingDestination>>,
    dnd_enabled: Option<bool>,
    dnd: Option<DndMode>,
    privacy_enabled: Option<bool>,
    privacy: Option<bool>,
    parking_enabled: Option<bool>,
    conference_allowed: Option<bool>,
    conference_music_on_hold_class: Option<Option<String>>,
    conference_play_general_announcements: Option<bool>,
    conference_play_participant_announcements: Option<bool>,
    conference_mute_on_entry: Option<bool>,
    conference_show_list: Option<bool>,
    conference_dialing_enabled: Option<bool>,
    conference_application_options: Option<String>,
    use_redial_menu: Option<bool>,
    allow_ringing_notification: Option<bool>,
    mwi_lamp_mode: Option<LampMode>,
    mwi_on_call: Option<bool>,
    legacy_code_page: Option<LegacyCodePage>,
    allow_overlap: Option<bool>,
    dtmf_mode: Option<DtmfMode>,
    direct_media: Option<bool>,
    early_media: Option<bool>,
    audio_encryption: Option<MediaEncryptionPolicy>,
    codec_settings: Vec<(bool, &'a str)>,
    acl_rules: Option<Vec<AclRule>>,
    permitted_hosts: Option<Vec<String>>,
    nat: Option<NatMode>,
    qos: QosPolicyPatch,
    transport: Option<TransportRequirement>,
    configured_feature_defaults: Vec<(u32, bool)>,
    channel_variables: Vec<ChannelVariable>,
}

/// Unresolved general-section values. Optional fields retain whether the user
/// actually supplied them; inherited structures are represented as patches.
#[derive(Default)]
struct GeneralSectionDraft<'a> {
    call_answer_order: Option<CallAnswerOrder>,
    timezone_offset_minutes: Option<i16>,
    date_template: Option<DateTemplate>,
    ring_type: Option<RingerMode>,
    call_waiting_tone: Option<Option<Tone>>,
    call_waiting_interval: Option<u32>,
    first_digit_timeout: Option<u64>,
    interdigit_timeout: Option<u64>,
    dial_terminator: Option<char>,
    record_dial_terminator: Option<bool>,
    simulate_enbloc: Option<bool>,
    speed_dial_await_further_digits: Option<bool>,
    allow_overlap: Option<bool>,
    transfer_on_hangup: Option<bool>,
    fallback_decision: Option<FallbackDecision>,
    fallback_backoff: Option<u32>,
    fallback_server_priority: Option<u8>,
    conference_enabled: Option<bool>,
    conference_options: Option<String>,
    auto_answer_ring_time: Option<u32>,
    auto_answer_tone: Option<Tone>,
    remote_hangup_tone: Option<Option<Tone>>,
    hotline_enabled: Option<bool>,
    hotline_extension: Option<Option<HotlineDestination>>,
    hotline_context: Option<String>,
    hotline_label: Option<String>,
    direct_media: Option<bool>,
    early_media: Option<bool>,
    audio_encryption: Option<MediaEncryptionPolicy>,
    echo_cancellation: Option<bool>,
    silence_suppression: Option<bool>,
    jitter_enabled: Option<bool>,
    jitter_forced: Option<bool>,
    jitter_log_frames: Option<bool>,
    jitter_max_size_ms: Option<u32>,
    jitter_resync_threshold_ms: Option<u32>,
    jitter_implementation: Option<JitterBufferImplementation>,
    registration_contexts: Option<Vec<String>>,
    codec_settings: Vec<(bool, &'a str)>,
    clear_bind: Option<SocketAddr>,
    clear_address: Option<IpAddr>,
    clear_port: Option<u16>,
    tls_bind: Option<SocketAddr>,
    tls_address: Option<IpAddr>,
    tls_port: Option<u16>,
    combined_pem: Option<PathBuf>,
    tls_certificate: Option<PathBuf>,
    tls_private_key: Option<PathBuf>,
    tls_trust_store: Option<PathBuf>,
    acl_rules: Option<Vec<AclRule>>,
    local_networks: Option<Vec<IpNetwork>>,
    external_address: Option<Option<IpAddr>>,
    external_hostname: Option<Option<String>>,
    external_refresh: Option<u32>,
    nat: Option<NatMode>,
    advertised_ipv4: Option<Option<Ipv4Addr>>,
    advertised_ipv6: Option<Option<Ipv6Addr>>,
    advertised_alias_seen: bool,
    qos: QosPolicyPatch,
    device_table: Option<String>,
    line_table: Option<String>,
    language: Option<String>,
    account_code: Option<Option<String>>,
}

impl ModuleConfig {
    pub fn parse(input: &str) -> Result<Self, ConfigError> {
        Self::from_raw_sections(parse_sections(input)?)
    }

    /// Validate that every option in a source file uses the Serde schema's
    /// canonical spelling. Runtime parsing remains case-insensitive and may
    /// accept explicitly declared compatibility aliases.
    pub fn check_canonical(input: &str) -> Result<(), ConfigError> {
        Self::parse(input)?;
        let sections = parse_sections(input)?;
        for section in &sections {
            let kind = source_section_kind(section, &sections)?;
            check_canonical_section(section, &kind)?;
        }
        Ok(())
    }

    /// Render a validated, deterministic configuration using canonical option
    /// names. Templates are resolved and the source is never modified.
    pub fn to_canonical_string(input: &str) -> Result<String, ConfigError> {
        Self::parse(input)?;
        let mut sections = resolve_inheritance(parse_sections(input)?)?;
        sections.sort_by(|left, right| {
            canonical_section_rank(left)
                .cmp(&canonical_section_rank(right))
                .then_with(|| {
                    left.name
                        .to_ascii_lowercase()
                        .cmp(&right.name.to_ascii_lowercase())
                })
        });

        let mut output = String::new();
        for (index, section) in sections.iter().enumerate() {
            if index != 0 {
                output.push('\n');
            }
            output.push('[');
            output.push_str(&section.name);
            output.push_str("]\n");
            for entry in canonical_section_entries(section)? {
                output.push_str(&entry.key);
                output.push_str(" = ");
                output.push_str(&canonical_value(&entry.value));
                output.push('\n');
            }
        }
        Ok(output)
    }

    pub(crate) fn parse_with_overlays(
        input: &str,
        overlays: &[ConfigOverlaySection],
    ) -> Result<Self, ConfigError> {
        let mut sections = parse_sections(input)?;
        apply_config_overlays(&mut sections, overlays)?;
        Self::from_raw_sections(sections)
    }

    pub(crate) fn realtime_tables_from_source(
        input: &str,
    ) -> Result<Option<RealtimeTableConfig>, ConfigError> {
        let sections = parse_sections(input)?;
        let mut general = GeneralConfig::default();
        if let Some(section) = sections
            .iter()
            .find(|section| section.name.eq_ignore_ascii_case("general"))
        {
            parse_general(&mut general, section)
                .map_err(|error| locate_section_error(error, section))?;
        }
        Ok(general.realtime_tables)
    }

    fn from_raw_sections(sections: Vec<RawSection>) -> Result<Self, ConfigError> {
        let sections = resolve_inheritance(sections)?;
        let mut general = GeneralConfig::default();
        let mut devices = HashMap::new();
        let mut lines = HashMap::new();
        let mut line_features = HashMap::new();
        let mut registration_target_owners = HashMap::<RegistrationTarget, String>::new();
        let mut device_codec_overrides = HashSet::new();
        let mut line_codec_overrides = HashSet::new();
        let mut device_audio_encryption_overrides = HashSet::new();
        let mut line_audio_encryption_overrides = HashSet::new();
        let mut soft_key_profiles = HashMap::from([(
            DEFAULT_SOFT_KEY_PROFILE.to_owned(),
            SoftKeyProfile::built_in(),
        )]);

        // Resolve general defaults before typing lines and devices so section
        // order cannot affect inherited media policy.
        for section in &sections {
            if section.name.eq_ignore_ascii_case("general") {
                parse_general(&mut general, section)
                    .map_err(|error| locate_section_error(error, section))?;
            }
        }

        // Lines are collected before devices so button declarations may refer
        // to line sections that appear later in the file.
        for section in &sections {
            if section.name.eq_ignore_ascii_case("general") {
                continue;
            }

            let kind = value(section, "type")
                .ok_or_else(|| ConfigError::MissingSectionType(section.name.clone()))?;
            match kind.to_ascii_lowercase().as_str() {
                "device" => {}
                "line" => {
                    let config = parse_line(section, &general)
                        .map_err(|error| locate_section_error(error, section))?;
                    let number = config.line.number.clone();
                    if lines.contains_key(&number) {
                        return Err(ConfigError::DuplicateSection(section.name.clone()));
                    }
                    for target in resolve_registration_targets(
                        &general.registration.contexts,
                        &config.features.registration.extensions,
                    ) {
                        if let Some(previous) =
                            registration_target_owners.insert(target.clone(), number.clone())
                        {
                            return Err(invalid_option(
                                section.diagnostic_key("regexten"),
                                &format!("{}@{}", target.extension, target.context),
                                &format!(
                                    "a registration target unique across lines; already used by [{previous}]"
                                ),
                                false,
                            ));
                        }
                    }
                    if section_has_codec_settings(section) {
                        line_codec_overrides.insert(number.clone());
                    }
                    if section_has_audio_encryption_setting(section) {
                        line_audio_encryption_overrides.insert(number.clone());
                    }
                    lines.insert(number.clone(), config.line);
                    line_features.insert(number, config.features);
                }
                "softkey_profile" => {
                    let config = parse_soft_key_profile(section)
                        .map_err(|error| locate_section_error(error, section))?;
                    soft_key_profiles.insert(canonical_profile_name(&config.name), config);
                }
                other => {
                    return Err(ConfigError::UnknownSectionType {
                        section: section.name.clone(),
                        kind: other.to_owned(),
                    });
                }
            }
        }

        for section in &sections {
            if section.name.eq_ignore_ascii_case("general")
                || !value(section, "type").is_some_and(|kind| kind.eq_ignore_ascii_case("device"))
            {
                continue;
            }
            let config = parse_device(section, &lines, &soft_key_profiles, &general)
                .map_err(|error| locate_section_error(error, section))?;
            if section_has_codec_settings(section) {
                device_codec_overrides.insert(config.id.clone());
            }
            if section_has_audio_encryption_setting(section) {
                device_audio_encryption_overrides.insert(config.id.clone());
            }
            if devices.insert(config.id.clone(), config).is_some() {
                return Err(ConfigError::DuplicateSection(section.name.clone()));
            }
        }

        if devices.is_empty() || lines.is_empty() {
            return Err(ConfigError::Empty);
        }
        if general.bind.port() == 0
            || (general.network.advertised.ipv4.is_none()
                && general.network.advertised.ipv6.is_none())
        {
            return Err(ConfigError::InvalidValue {
                key: "[general] listener/advertised address policy".into(),
                value: format!(
                    "clear={} advertised_ipv4={:?} advertised_ipv6={:?}; expected a nonzero listener port and at least one advertised address",
                    general.bind, general.network.advertised.ipv4, general.network.advertised.ipv6
                ),
            });
        }
        let timing = general.timing_policy();
        if timing.keepalive < Duration::from_secs(5)
            || timing.secondary_keepalive < Duration::from_secs(5)
            || timing.interdigit_timeout < Duration::from_millis(250)
        {
            return Err(ConfigError::InvalidValue {
                key: "keepalive/secondary_keepalive/interdigit_timeout".into(),
                value: format!(
                    "{}/{}/{}",
                    general.keepalive_seconds,
                    general.secondary_keepalive_seconds,
                    general.interdigit_timeout_ms
                ),
            });
        }
        if general.signaling_servers.len() > sccp_protocol::MAX_SIGNALING_SERVERS {
            return Err(ConfigError::InvalidValue {
                key: "signaling_server".into(),
                value: "too many configured endpoints".into(),
            });
        }
        let priorities = general
            .signaling_servers
            .iter()
            .map(|server| server.priority)
            .collect::<HashSet<_>>();
        if priorities.len() != general.signaling_servers.len()
            || !general.signaling_servers.is_empty()
                && !priorities.contains(&general.fallback_registration.server_priority)
        {
            return Err(ConfigError::InvalidValue {
                key: "signaling_server/server_priority".into(),
                value: "priorities must be unique and include this server".into(),
            });
        }

        let mut bindings = Vec::new();
        let mut bindings_by_line = HashMap::<String, Vec<usize>>::new();
        let mut bindings_by_device = HashMap::<DeviceId, Vec<usize>>::new();
        let mut binding_by_button = HashMap::new();
        let mut device_ids: Vec<_> = devices.keys().cloned().collect();
        device_ids.sort();
        for device_id in device_ids {
            let device = devices.get(&device_id).expect("device ID came from map");
            let mut seen = HashSet::new();
            for line_definition in device.buttons.iter().filter_map(|button| match button {
                ButtonDefinition::Line(line) => Some(line),
                _ => None,
            }) {
                let line_name = &line_definition.number;
                if !seen.insert(line_name) {
                    return Err(ConfigError::InvalidValue {
                        key: format!("{}.line", device.id),
                        value: line_name.clone(),
                    });
                }
                let line =
                    lines
                        .get(line_name)
                        .cloned()
                        .ok_or_else(|| ConfigError::UnknownLine {
                            device: device.id.clone(),
                            line: line_name.clone(),
                        })?;
                let binding = LineBinding {
                    device_id: device.id.clone(),
                    line_instance: line_definition.instance,
                    appearance: line_definition.clone(),
                    line,
                };
                let index = bindings.len();
                bindings.push(binding);
                bindings_by_line
                    .entry(line_name.clone())
                    .or_default()
                    .push(index);
                bindings_by_device
                    .entry(device.id.clone())
                    .or_default()
                    .push(index);
                if binding_by_button
                    .insert((device.id.clone(), line_definition.instance), index)
                    .is_some()
                {
                    return Err(ConfigError::InvalidValue {
                        key: format!("{}.line_instance", device.id),
                        value: line_definition.instance.to_string(),
                    });
                }
            }
        }
        if let Some(unassigned) = lines
            .keys()
            .find(|line| !bindings_by_line.contains_key(*line))
        {
            return Err(ConfigError::UnassignedLine(unassigned.clone()));
        }

        Ok(Self {
            general,
            devices,
            lines,
            line_features,
            soft_key_profiles,
            bindings,
            bindings_by_line,
            bindings_by_device,
            binding_by_button,
            device_codec_overrides,
            line_codec_overrides,
            device_audio_encryption_overrides,
            line_audio_encryption_overrides,
        })
    }

    pub fn line(&self, number: &str) -> Option<&LineBinding> {
        self.appearances_for_line(number).next()
    }

    pub fn soft_key_profile(&self, name: &str) -> Option<&SoftKeyProfile> {
        self.soft_key_profiles.get(&canonical_profile_name(name))
    }

    pub fn soft_key_profile_for_device(&self, device: &DeviceId) -> Option<&SoftKeyProfile> {
        let profile = &self.devices.get(device)?.soft_key_profile;
        self.soft_key_profiles.get(profile)
    }

    pub fn feature_defaults_for_device(&self, device: &DeviceId) -> Option<&DeviceFeatureDefaults> {
        Some(&self.devices.get(device)?.feature_defaults)
    }

    pub fn dnd_button_mode(
        &self,
        device: &DeviceId,
        feature_instance: u32,
    ) -> Option<DndButtonMode> {
        self.dnd_buttons_for_device(device)
            .find_map(|(instance, mode)| (instance == feature_instance).then_some(mode))
    }

    pub fn dnd_buttons_for_device<'a>(
        &'a self,
        device: &DeviceId,
    ) -> impl Iterator<Item = (u32, DndButtonMode)> + 'a {
        self.devices.get(device).into_iter().flat_map(|device| {
            device.buttons.iter().filter_map(|button| match button {
                ButtonDefinition::Feature(feature)
                    if feature.feature == ButtonType::DoNotDisturb =>
                {
                    Some((
                        feature.instance,
                        match device.feature_arguments.get(&feature.instance) {
                            Some(argument) if argument == "silent" => DndButtonMode::Silent,
                            Some(argument) if argument == "reject" => DndButtonMode::Reject,
                            Some(_) => {
                                unreachable!("DND feature arguments are normalized during parsing")
                            }
                            None => DndButtonMode::Cycle,
                        },
                    ))
                }
                _ => None,
            })
        })
    }

    pub fn features_for_line(&self, number: &str) -> Option<&LineFeatureConfig> {
        self.line_features.get(number)
    }

    pub fn parking_for_device(&self, device: &DeviceId) -> Option<&DeviceParkingConfig> {
        Some(&self.devices.get(device)?.parking)
    }

    pub fn parking_for_line(&self, number: &str) -> Option<&LineParkingConfig> {
        Some(&self.line_features.get(number)?.parking)
    }

    pub fn parking_lot_for_button(
        &self,
        device: &DeviceId,
        feature_instance: u32,
    ) -> Option<&ParkingLotButtonConfig> {
        self.devices
            .get(device)?
            .parking
            .feature_buttons
            .get(&feature_instance)
    }

    pub fn conference_for_device(&self, device: &DeviceId) -> Option<&DeviceConferenceConfig> {
        Some(&self.devices.get(device)?.conference)
    }

    pub fn conference_for_line(&self, number: &str) -> Option<&LineConferenceConfig> {
        Some(&self.line_features.get(number)?.conference)
    }

    pub fn call_answer_order(&self) -> CallAnswerOrder {
        self.general.call_answer_order
    }

    pub fn call_ui_for_device(&self, device: &DeviceId) -> Option<&DeviceCallUiConfig> {
        Some(&self.devices.get(device)?.call_ui)
    }

    pub fn auto_answer(&self) -> &AutoAnswerConfig {
        &self.general.auto_answer
    }

    pub fn guest_hotline(&self) -> &GuestHotlineConfig {
        &self.general.guest_hotline
    }

    /// Build the policy-neutral logical line used by an otherwise unknown
    /// station admitted through the anonymous guest-hotline policy. The PBX
    /// destination is deliberately not copied into the binding.
    pub fn guest_hotline_binding(
        &self,
        device_id: &DeviceId,
        line_instance: u32,
    ) -> Option<LineBinding> {
        let guest = self.guest_hotline();
        if self.devices.contains_key(device_id)
            || !guest.enabled
            || guest.extension.is_none()
            || line_instance != 1
        {
            return None;
        }
        let line = LineConfig {
            number: "hotline".into(),
            label: guest.label.clone(),
            context: guest.context.clone(),
            caller_name: guest.label.clone(),
            caller_number: "hotline".into(),
            mailbox: None,
            language: self.general.language.clone(),
            account_code: self.general.account_code.clone(),
            channel_variables: Vec::new(),
        };
        let mut appearance = LineAppearance::new(
            line_instance,
            LineDefinition {
                number: line.number.clone(),
                display_name: guest.label.clone(),
            },
        );
        appearance.label = Some(guest.label.clone());
        Some(LineBinding {
            device_id: device_id.clone(),
            line_instance,
            appearance,
            line,
        })
    }

    pub fn hotline_for_line(&self, number: &str) -> Option<&LineHotlineConfig> {
        Some(&self.line_features.get(number)?.hotline)
    }

    pub fn hotline_destination_for_binding(
        &self,
        binding: &LineBinding,
    ) -> Option<&HotlineDestination> {
        if self.devices.contains_key(&binding.device_id) {
            return self
                .hotline_for_line(&binding.line.number)?
                .destination
                .as_ref();
        }
        let guest = self.guest_hotline();
        (guest.enabled && binding.line_instance == 1 && binding.line.number == "hotline")
            .then_some(guest.extension.as_ref())
            .flatten()
    }

    pub fn registration_contexts(&self) -> &[String] {
        &self.general.registration.contexts
    }

    pub fn fallback_registration(&self) -> &FallbackRegistrationConfig {
        &self.general.fallback_registration
    }

    pub fn mobility_for_line(&self, number: &str) -> Option<&LineMobilityConfig> {
        Some(&self.line_features.get(number)?.mobility)
    }

    pub fn registration_for_line(&self, number: &str) -> Option<&LineRegistrationConfig> {
        Some(&self.line_features.get(number)?.registration)
    }

    pub fn registration_targets_for_line(&self, number: &str) -> Option<Vec<RegistrationTarget>> {
        let registration = &self.line_features.get(number)?.registration;
        Some(resolve_registration_targets(
            &self.general.registration.contexts,
            &registration.extensions,
        ))
    }

    pub fn media_for_device(&self, device: &DeviceId) -> Option<&DeviceMediaConfig> {
        Some(&self.devices.get(device)?.media)
    }

    pub fn network_policy(&self) -> &NetworkPolicy {
        &self.general.network
    }

    pub fn listener_policy(&self) -> &ListenerPolicy {
        &self.general.listeners
    }

    pub fn qos_policy(&self) -> &QosPolicy {
        &self.general.qos
    }

    pub fn realtime_tables(&self) -> Option<&RealtimeTableConfig> {
        self.general.realtime_tables.as_ref()
    }

    pub fn network_for_device(&self, device: &DeviceId) -> Option<&DeviceNetworkPolicy> {
        Some(&self.devices.get(device)?.network)
    }

    pub fn media_for_line(&self, number: &str) -> Option<&LineMediaConfig> {
        Some(&self.line_features.get(number)?.media)
    }

    pub fn media_for_appearance(
        &self,
        device: &DeviceId,
        line_instance: u32,
    ) -> Option<ResolvedMediaConfig> {
        let binding = self.line_for_device(device, line_instance)?;
        self.media_for_binding(binding)
    }

    /// Resolve media policy for either a configured or runtime-created line
    /// appearance. Runtime mobility bindings still name a configured device
    /// and logical line, so they use the same normalization precedence.
    pub fn media_for_binding(&self, binding: &LineBinding) -> Option<ResolvedMediaConfig> {
        let device = &binding.device_id;
        let device_config = self.devices.get(device)?;
        let line = self.line_features.get(&binding.line.number)?;
        let codecs = if self.line_codec_overrides.contains(&binding.line.number) {
            line.media.codecs.clone()
        } else if self.device_codec_overrides.contains(device) {
            device_config.media.codecs.clone()
        } else {
            self.general.codecs.clone()
        };
        let audio_encryption = if self
            .line_audio_encryption_overrides
            .contains(&binding.line.number)
        {
            line.media.audio_encryption.clone()
        } else if self.device_audio_encryption_overrides.contains(device) {
            device_config.media.audio_encryption.clone()
        } else {
            self.general.audio_encryption.clone()
        };
        Some(ResolvedMediaConfig {
            codecs,
            audio_encryption,
            dtmf_mode: device_config.media.dtmf_mode,
            direct_media: device_config.media.direct_media,
            early_media: device_config.media.early_media,
            video_mode: line.media.video_mode,
            audio_processing: line.media.audio_processing,
        })
    }

    /// Resolve the general, device, and line conference-dialing layers for a
    /// concrete line appearance.
    pub fn conference_dialing_for_appearance(
        &self,
        device: &DeviceId,
        line_instance: u32,
    ) -> Option<ResolvedConferenceDialing> {
        let binding = self.line_for_device(device, line_instance)?;
        self.conference_dialing_for_binding(binding)
    }

    pub fn conference_dialing_for_binding(
        &self,
        binding: &LineBinding,
    ) -> Option<ResolvedConferenceDialing> {
        let device = self.devices.get(&binding.device_id)?;
        let line = self.line_features.get(&binding.line.number)?;
        Some(ResolvedConferenceDialing {
            enabled: line
                .conference
                .enabled
                .unwrap_or(device.conference.dialing.enabled),
            destination: line.conference.destination.clone(),
            application_options: line
                .conference
                .application_options
                .clone()
                .unwrap_or_else(|| device.conference.dialing.application_options.clone()),
        })
    }

    pub fn line_appearance_count(&self, number: &str) -> usize {
        self.bindings_by_line.get(number).map_or(0, Vec::len)
    }

    pub fn appearances_for_line(&self, number: &str) -> impl Iterator<Item = &LineBinding> {
        self.bindings_by_line
            .get(number)
            .into_iter()
            .flatten()
            .filter_map(|index| self.bindings.get(*index))
    }

    pub fn appearances_for_device(&self, device: &DeviceId) -> impl Iterator<Item = &LineBinding> {
        self.bindings_by_device
            .get(device)
            .into_iter()
            .flatten()
            .filter_map(|index| self.bindings.get(*index))
    }

    pub fn line_for_device(&self, device: &DeviceId, instance: u32) -> Option<&LineBinding> {
        self.binding_by_button
            .get(&(device.clone(), instance))
            .and_then(|index| self.bindings.get(*index))
    }

    /// Resolve either `line` or the legacy-compatible `device/line` dial form.
    pub fn dial_target(&self, address: &str) -> Option<&LineBinding> {
        let mut parts = address.split('/').map(str::trim);
        let first = parts.next()?;
        let second = parts.next();
        if parts.next().is_some() {
            return None;
        }
        let Some(line) = second else {
            return self.line(first);
        };
        let device = DeviceId::new(first).ok()?;
        self.appearances_for_line(line)
            .find(|binding| binding.device_id == device)
    }

    pub fn device_definitions(&self) -> Vec<DeviceDefinition> {
        let mut definitions: Vec<_> = self
            .devices
            .values()
            .map(|device| DeviceDefinition {
                id: device.id.clone(),
                description: device.description.clone(),
                transport: device.network.transport.into(),
                signaling_qos: Some(SignalingQos::new(
                    device.network.qos.signaling.dscp.0,
                    device.network.qos.signaling.cos.0,
                )),
                buttons: device
                    .buttons
                    .iter()
                    .cloned()
                    .map(|button| match button {
                        ButtonDefinition::Line(mut appearance) => {
                            if let Some(features) = self.line_features.get(&appearance.number) {
                                appearance.initial_tone = features.dial_tones.initial;
                            }
                            ButtonDefinition::Line(appearance)
                        }
                        button => button,
                    })
                    .collect(),
                soft_keys: self
                    .soft_key_profiles
                    .get(&device.soft_key_profile)
                    .expect("device soft-key profile was validated during parsing")
                    .station_profile(),
                ui: StationUiPolicy {
                    placed_calls_redial_menu: matches!(
                        device.call_ui.redial_mode,
                        RedialMode::PlacedCallsMenu
                    ),
                    hinted_ringing_notification: device.call_ui.hinted_ringing_notification,
                    speed_dial_await_further_digits: self.general.speed_dial_await_further_digits,
                    mwi_lamp_mode: device.call_ui.mwi_lamp_mode,
                    mwi_on_call: device.call_ui.mwi_on_call,
                    legacy_code_page: device.call_ui.legacy_code_page,
                },
            })
            .collect();
        definitions.sort_by(|left, right| left.id.cmp(&right.id));
        definitions
    }
}

#[derive(Debug)]
struct CanonicalEntry<'a> {
    key: String,
    value: &'a str,
}

fn canonical_section_kind(section: &RawSection) -> Result<&str, ConfigError> {
    if section.name.eq_ignore_ascii_case("general") {
        return Ok("general");
    }
    value(section, "type").ok_or_else(|| ConfigError::MissingSectionType(section.name.clone()))
}

fn canonical_section_rank(section: &RawSection) -> u8 {
    match canonical_section_kind(section)
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "general" => 0,
        "softkey_profile" => 1,
        "device" => 2,
        "line" => 3,
        _ => 4,
    }
}

fn canonical_section_entries(section: &RawSection) -> Result<Vec<CanonicalEntry<'_>>, ConfigError> {
    fn typed<'a, K>(section: &'a RawSection) -> Result<Vec<CanonicalEntry<'a>>, ConfigError>
    where
        K: serde::de::DeserializeOwned + Serialize,
    {
        deserialize_entries::<K>(section)?
            .into_iter()
            .map(|entry| {
                Ok(CanonicalEntry {
                    key: serialized_key(&entry.key)?,
                    value: entry.value(),
                })
            })
            .collect()
    }

    match canonical_section_kind(section)?
        .to_ascii_lowercase()
        .as_str()
    {
        "general" => typed::<GeneralOption>(section),
        "device" => typed::<DeviceOption>(section),
        "line" => typed::<LineOption>(section),
        "softkey_profile" => {
            let _: SoftKeyProfileSection = deserialize_section(section)?;
            Ok(section
                .values
                .iter()
                .map(|entry| CanonicalEntry {
                    key: entry.key.to_ascii_lowercase(),
                    value: entry.value.as_str(),
                })
                .collect())
        }
        kind => Err(ConfigError::UnknownSectionType {
            section: section.name.clone(),
            kind: kind.to_owned(),
        }),
    }
}

fn source_section_kind(
    section: &RawSection,
    sections: &[RawSection],
) -> Result<String, ConfigError> {
    if section.name.eq_ignore_ascii_case("general") {
        return Ok("general".into());
    }
    if let Some(kind) = value(section, "type") {
        return Ok(kind.to_ascii_lowercase());
    }
    for parent in &section.parents {
        let parent = sections
            .iter()
            .find(|candidate| candidate.name.eq_ignore_ascii_case(parent))
            .ok_or_else(|| ConfigError::MissingTemplate {
                section: section.name.clone(),
                parent: parent.clone(),
            })?;
        if let Ok(kind) = source_section_kind(parent, sections) {
            return Ok(kind);
        }
    }
    Err(ConfigError::MissingSectionType(section.name.clone()))
}

fn check_canonical_section(section: &RawSection, kind: &str) -> Result<(), ConfigError> {
    let canonical_entries = match kind {
        "general" => canonical_typed_entries::<GeneralOption>(section)?,
        "device" => canonical_typed_entries::<DeviceOption>(section)?,
        "line" => canonical_typed_entries::<LineOption>(section)?,
        "softkey_profile" => {
            let _: SoftKeyProfileSection = deserialize_section(section)?;
            section
                .values
                .iter()
                .map(|entry| CanonicalEntry {
                    key: entry.key.to_ascii_lowercase(),
                    value: entry.value.as_str(),
                })
                .collect()
        }
        other => {
            return Err(ConfigError::UnknownSectionType {
                section: section.name.clone(),
                kind: other.to_owned(),
            });
        }
    };
    for (entry, canonical) in section.values.iter().zip(canonical_entries) {
        if entry.key != canonical.key {
            return Err(invalid_option(
                &entry.diagnostic_key(),
                &entry.value,
                &format!("canonical option name {}", canonical.key),
                section_values::sensitive_option_name(&entry.key),
            ));
        }
    }
    Ok(())
}

fn canonical_typed_entries<K>(section: &RawSection) -> Result<Vec<CanonicalEntry<'_>>, ConfigError>
where
    K: serde::de::DeserializeOwned + Serialize,
{
    deserialize_entries::<K>(section)?
        .into_iter()
        .map(|entry| {
            Ok(CanonicalEntry {
                key: serialized_key(&entry.key)?,
                value: entry.value(),
            })
        })
        .collect()
}

fn canonical_value(value: &str) -> String {
    if value.trim() != value || value.contains(';') || value.starts_with('#') {
        format!("\"{}\"", value.replace('"', "\\\""))
    } else {
        value.to_owned()
    }
}

fn section_has_codec_settings(section: &RawSection) -> bool {
    section
        .values
        .iter()
        .any(|value| matches!(normalize_name(&value.key).as_str(), "allow" | "disallow"))
}

fn section_has_audio_encryption_setting(section: &RawSection) -> bool {
    section
        .values
        .iter()
        .any(|value| normalize_name(&value.key) == "audioencryption")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TemplateKind {
    Device,
    Line,
}

impl TemplateKind {
    fn from_name(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "device" => Some(Self::Device),
            "line" => Some(Self::Line),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Device => "device",
            Self::Line => "line",
        }
    }
}

fn resolve_inheritance(sections: Vec<RawSection>) -> Result<Vec<RawSection>, ConfigError> {
    let indexes: HashMap<_, _> = sections
        .iter()
        .enumerate()
        .map(|(index, section)| (section.name.to_ascii_lowercase(), index))
        .collect();
    let mut states = vec![0_u8; sections.len()];
    let mut resolved = vec![None; sections.len()];
    let mut stack = Vec::new();

    for index in 0..sections.len() {
        resolve_section(
            index,
            &sections,
            &indexes,
            &mut states,
            &mut resolved,
            &mut stack,
        )?;
    }

    Ok(sections
        .iter()
        .enumerate()
        .filter(|(_, section)| !section.is_template)
        .filter_map(|(index, _)| resolved[index].clone())
        .collect())
}

fn resolve_section(
    index: usize,
    sections: &[RawSection],
    indexes: &HashMap<String, usize>,
    states: &mut [u8],
    resolved: &mut [Option<RawSection>],
    stack: &mut Vec<usize>,
) -> Result<RawSection, ConfigError> {
    if states[index] == 2 {
        return Ok(resolved[index]
            .as_ref()
            .expect("resolved inheritance state has a value")
            .clone());
    }
    if states[index] == 1 {
        let start = stack
            .iter()
            .position(|candidate| *candidate == index)
            .unwrap_or(0);
        let mut cycle: Vec<_> = stack[start..]
            .iter()
            .map(|candidate| sections[*candidate].name.as_str())
            .collect();
        cycle.push(&sections[index].name);
        return Err(ConfigError::InheritanceCycle(cycle.join(" -> ")));
    }

    states[index] = 1;
    stack.push(index);
    let section = &sections[index];
    let own_kind_name = value(section, "type").map(str::trim);
    let own_kind = own_kind_name.and_then(TemplateKind::from_name);
    let mut inherited_kind = own_kind;
    let mut values = Vec::new();

    for parent_name in &section.parents {
        let canonical = parent_name.to_ascii_lowercase();
        let parent_index =
            indexes
                .get(&canonical)
                .copied()
                .ok_or_else(|| ConfigError::MissingTemplate {
                    section: section.name.clone(),
                    parent: parent_name.clone(),
                })?;
        let parent_source = &sections[parent_index];
        if !parent_source.is_template {
            return Err(ConfigError::ParentIsNotTemplate {
                section: section.name.clone(),
                parent: parent_source.name.clone(),
            });
        }
        let parent = resolve_section(parent_index, sections, indexes, states, resolved, stack)?;
        let parent_kind = resolved_template_kind(&parent)?;
        if let Some(child_kind) = inherited_kind {
            if child_kind != parent_kind {
                return Err(ConfigError::WrongTemplateKind {
                    section: section.name.clone(),
                    child_kind: child_kind.as_str().into(),
                    parent: parent.name.clone(),
                    parent_kind: parent_kind.as_str().into(),
                });
            }
        } else if let Some(kind) = own_kind_name {
            return Err(ConfigError::WrongTemplateKind {
                section: section.name.clone(),
                child_kind: kind.to_owned(),
                parent: parent.name.clone(),
                parent_kind: parent_kind.as_str().into(),
            });
        } else {
            inherited_kind = Some(parent_kind);
        }
        merge_template_values(&mut values, &parent.values, parent_kind);
    }

    if section.parents.is_empty() {
        values.clone_from(&section.values);
    } else if let Some(kind) = inherited_kind {
        merge_template_values(&mut values, &section.values, kind);
    } else {
        values.clone_from(&section.values);
    }
    let result = RawSection {
        name: section.name.clone(),
        line: section.line,
        is_template: section.is_template,
        parents: section.parents.clone(),
        values,
    };
    if result.is_template {
        resolved_template_kind(&result)?;
    }

    stack.pop();
    states[index] = 2;
    resolved[index] = Some(result.clone());
    Ok(result)
}

fn resolved_template_kind(section: &RawSection) -> Result<TemplateKind, ConfigError> {
    let raw = value(section, "type").unwrap_or("missing");
    TemplateKind::from_name(raw).ok_or_else(|| ConfigError::InvalidTemplateKind {
        section: section.name.clone(),
        kind: raw.to_owned(),
    })
}

fn merge_template_values(merged: &mut Vec<RawValue>, incoming: &[RawValue], kind: TemplateKind) {
    let mut overridden = HashSet::new();
    for value in incoming {
        let identity = template_option_identity(kind, &value.key);
        let repeated = matches!(
            identity.as_str(),
            "allow" | "disallow" | "deny" | "permit" | "permithost" | "setvar"
        ) || kind == TemplateKind::Device
            && matches!(identity.as_str(), "button" | "line" | "featuredefault");
        if !repeated && overridden.insert(identity.clone()) {
            merged.retain(|candidate| template_option_identity(kind, &candidate.key) != identity);
        }
        merged.push(value.clone());
    }
}

fn template_option_identity(kind: TemplateKind, key: &str) -> String {
    let normalized = normalize_name(key);
    match (kind, normalized.as_str()) {
        (TemplateKind::Device, "forwardallenabled") => "cfwdall".into(),
        (TemplateKind::Device, "forwardbusyenabled") => "cfwdbusy".into(),
        (TemplateKind::Device, "forwardnoanswerenabled") => "cfwdnoanswer".into(),
        (TemplateKind::Device, "forwardnoanswertimeout") => "cfwdnoanswertimeout".into(),
        (TemplateKind::Device, "privacyfeature") => "private".into(),
        (TemplateKind::Device, "transportrequirement") => "transport".into(),
        (TemplateKind::Device, "signalingtos" | "sccpdscp" | "signalingdscp") => "sccptos".into(),
        (TemplateKind::Device, "signalingcos") => "sccpcos".into(),
        (TemplateKind::Device, "audiodscp") => "audiotos".into(),
        (TemplateKind::Device, "videodscp") => "videotos".into(),
        (TemplateKind::Line, "voicemailnumber") => "vmnum".into(),
        (TemplateKind::Line, "voicemailtransfer" | "transfertovoicemail") => "trnsfvm".into(),
        (TemplateKind::Line, "directedpickupmodeanswer") => "pickupmodeanswer".into(),
        _ => normalized,
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields, rename_all = "snake_case")]
struct SoftKeyProfileSection {
    #[serde(rename = "type")]
    section_type: Option<String>,
    on_hook: Option<String>,
    connected: Option<String>,
    on_hold: Option<String>,
    ring_in: Option<String>,
    off_hook: Option<String>,
    connected_transfer: Option<String>,
    digits_following: Option<String>,
    connected_conference: Option<String>,
    ring_out: Option<String>,
    off_hook_feature: Option<String>,
    in_use_hint: Option<String>,
    on_hook_stealable: Option<String>,
    hold_conference: Option<String>,
    empty: Option<String>,
}

fn parse_soft_key_profile(section: &RawSection) -> Result<SoftKeyProfile, ConfigError> {
    let name = canonical_profile_name(&section.name);
    if name.is_empty() {
        return Err(ConfigError::InvalidValue {
            key: "softkey_profile.name".into(),
            value: section.name.clone(),
        });
    }
    let decoded: SoftKeyProfileSection = deserialize_section(section)?;
    if decoded
        .section_type
        .as_deref()
        .is_none_or(|kind| !kind.eq_ignore_ascii_case("softkey_profile"))
    {
        return Err(ConfigError::InvalidValue {
            key: section.diagnostic_key("type"),
            value: format!(
                "{:?}; expected one type = softkey_profile",
                decoded.section_type.as_deref().unwrap_or("")
            ),
        });
    }

    let mut profile = SoftKeyProfile::empty(name);
    for (mode, raw) in [
        (KeyMode::OnHook, decoded.on_hook),
        (KeyMode::Connected, decoded.connected),
        (KeyMode::OnHold, decoded.on_hold),
        (KeyMode::RingIn, decoded.ring_in),
        (KeyMode::OffHook, decoded.off_hook),
        (KeyMode::ConnectedTransfer, decoded.connected_transfer),
        (KeyMode::DigitsFollowing, decoded.digits_following),
        (KeyMode::ConnectedConference, decoded.connected_conference),
        (KeyMode::RingOut, decoded.ring_out),
        (KeyMode::OffHookFeature, decoded.off_hook_feature),
        (KeyMode::InUseHint, decoded.in_use_hint),
        (KeyMode::OnHookStealable, decoded.on_hook_stealable),
        (KeyMode::HoldConference, decoded.hold_conference),
        (KeyMode::Empty, decoded.empty),
    ] {
        let Some(raw) = raw else {
            continue;
        };
        let diagnostic = section.diagnostic_key(key_mode_option(mode));
        let mut actions = Vec::new();
        let mut seen_actions = HashSet::new();
        if !raw.trim().is_empty() {
            for name in raw.split(',') {
                let name = name.trim();
                let action = parse_soft_key(name).ok_or_else(|| ConfigError::InvalidValue {
                    key: diagnostic.clone(),
                    value: format!("{name:?}; expected a recognized soft-key action"),
                })?;
                if !seen_actions.insert(action) {
                    return Err(ConfigError::InvalidValue {
                        key: diagnostic,
                        value: format!("{name:?}; expected unique soft-key actions"),
                    });
                }
                actions.push(action);
                if actions.len() > MAX_SOFT_KEYS_PER_MODE {
                    return Err(ConfigError::InvalidValue {
                        key: diagnostic,
                        value: format!(
                            "{raw:?}; expected at most {MAX_SOFT_KEYS_PER_MODE} actions"
                        ),
                    });
                }
            }
        }
        profile.sets.insert(mode, actions);
    }

    Ok(profile)
}

fn key_mode_option(mode: KeyMode) -> &'static str {
    match mode {
        KeyMode::OnHook => "on_hook",
        KeyMode::Connected => "connected",
        KeyMode::OnHold => "on_hold",
        KeyMode::RingIn => "ring_in",
        KeyMode::OffHook => "off_hook",
        KeyMode::ConnectedTransfer => "connected_transfer",
        KeyMode::DigitsFollowing => "digits_following",
        KeyMode::ConnectedConference => "connected_conference",
        KeyMode::RingOut => "ring_out",
        KeyMode::OffHookFeature => "off_hook_feature",
        KeyMode::InUseHint => "in_use_hint",
        KeyMode::OnHookStealable => "on_hook_stealable",
        KeyMode::HoldConference => "hold_conference",
        KeyMode::Empty => "empty",
        KeyMode::Unknown(_) => "unknown",
    }
}

fn parse_soft_key(raw: &str) -> Option<SoftKey> {
    Some(match normalize_name(raw).as_str() {
        "redial" => SoftKey::Redial,
        "newcall" => SoftKey::NewCall,
        "hold" => SoftKey::Hold,
        "transfer" => SoftKey::Transfer,
        "forwardall" | "cfwdall" => SoftKey::ForwardAll,
        "forwardbusy" | "cfwdbusy" => SoftKey::ForwardBusy,
        "forwardnoanswer" | "cfwdnoanswer" => SoftKey::ForwardNoAnswer,
        "backspace" => SoftKey::Backspace,
        "endcall" => SoftKey::EndCall,
        "resume" => SoftKey::Resume,
        "answer" => SoftKey::Answer,
        "info" => SoftKey::Info,
        "conference" => SoftKey::Conference,
        "park" => SoftKey::Park,
        "join" => SoftKey::Join,
        "meetme" => SoftKey::MeetMe,
        "pickup" => SoftKey::Pickup,
        "grouppickup" => SoftKey::GroupPickup,
        "monitor" => SoftKey::Monitor,
        "callback" => SoftKey::Callback,
        "barge" => SoftKey::Barge,
        "donotdisturb" | "dnd" => SoftKey::DoNotDisturb,
        "conferencelist" => SoftKey::ConferenceList,
        "select" => SoftKey::Select,
        "private" => SoftKey::Private,
        "transfertovoicemail" => SoftKey::TransferToVoicemail,
        "directtransfer" => SoftKey::DirectTransfer,
        "immediatedivert" => SoftKey::ImmediateDivert,
        "videomode" => SoftKey::VideoMode,
        "intercept" => SoftKey::Intercept,
        "empty" => SoftKey::Empty,
        "dial" => SoftKey::Dial,
        _ => return None,
    })
}

fn canonical_profile_name(raw: &str) -> String {
    raw.trim().to_ascii_lowercase()
}

fn parse_line(section: &RawSection, general: &GeneralConfig) -> Result<ParsedLine, ConfigError> {
    let mut draft = LineSectionDraft::default();

    for entry in deserialize_entries::<LineOption>(section)? {
        let key = &entry.source.key;
        let raw = &entry.source.value;
        let diagnostic = entry.source.diagnostic_key();
        match entry.key {
            LineOption::Type | LineOption::Label | LineOption::Context | LineOption::CallerId => {}
            LineOption::IncomingLimit => {
                set_once(
                    &mut draft.incoming_limit,
                    section,
                    key,
                    raw,
                    raw.trim()
                        .parse::<u32>()
                        .ok()
                        .filter(|limit| *limit <= 255)
                        .ok_or_else(|| {
                            invalid_option(&diagnostic, raw, "incoming call limit 0..255", false)
                        })?,
                )?;
            }
            LineOption::Language => set_once(
                &mut draft.language,
                section,
                key,
                raw,
                parse_metadata_required(&diagnostic, raw, MAX_LANGUAGE_BYTES, false)?,
            )?,
            LineOption::AccountCode => set_once(
                &mut draft.account_code,
                section,
                key,
                "<redacted>",
                parse_metadata_optional(&diagnostic, raw, MAX_ACCOUNT_CODE_BYTES, true)?,
            )?,
            LineOption::SetVariable => {
                push_channel_variable(&mut draft.channel_variables, &diagnostic, raw)?
            }
            LineOption::Mailbox => set_once(
                &mut draft.mailbox,
                section,
                key,
                raw,
                parse_mailbox(&diagnostic, raw)?,
            )?,
            LineOption::VoicemailNumber => set_once(
                &mut draft.voicemail_number,
                section,
                key,
                "<redacted>",
                parse_optional_voicemail_destination(&diagnostic, raw)?,
            )?,
            LineOption::VoicemailTransfer => set_once(
                &mut draft.voicemail_transfer,
                section,
                key,
                "<redacted>",
                parse_optional_voicemail_destination(&diagnostic, raw)?,
            )?,
            LineOption::CallGroup => set_once(
                &mut draft.call_groups,
                section,
                key,
                raw,
                parse_numeric_groups(&diagnostic, raw)?,
            )?,
            LineOption::PickupGroup => set_once(
                &mut draft.pickup_groups,
                section,
                key,
                raw,
                parse_numeric_groups(&diagnostic, raw)?,
            )?,
            LineOption::NamedCallGroup => set_once(
                &mut draft.named_call_groups,
                section,
                key,
                raw,
                parse_named_groups(&diagnostic, raw)?,
            )?,
            LineOption::NamedPickupGroup => set_once(
                &mut draft.named_pickup_groups,
                section,
                key,
                raw,
                parse_named_groups(&diagnostic, raw)?,
            )?,
            LineOption::DirectedPickup => set_once(
                &mut draft.directed_pickup,
                section,
                key,
                raw,
                parse_bool(&diagnostic, raw)?,
            )?,
            LineOption::DirectedPickupContext => set_once(
                &mut draft.directed_pickup_context,
                section,
                key,
                raw,
                parse_optional_setting(&diagnostic, raw)?,
            )?,
            LineOption::PickupModeAnswer => set_once(
                &mut draft.pickup_mode_answer,
                section,
                key,
                raw,
                parse_bool(&diagnostic, raw)?,
            )?,
            LineOption::ParkingLot => set_once(
                &mut draft.parking_lot,
                section,
                key,
                raw,
                parse_empty_optional_setting(&diagnostic, raw)?,
            )?,
            LineOption::ConferenceEnabled => set_once(
                &mut draft.conference_enabled,
                section,
                key,
                raw,
                parse_bool(&diagnostic, raw)?,
            )?,
            LineOption::ConferenceNumber => set_once(
                &mut draft.conference_destination,
                section,
                key,
                raw,
                parse_empty_optional_setting(&diagnostic, raw)?,
            )?,
            LineOption::ConferenceOptions => set_once(
                &mut draft.conference_options,
                section,
                key,
                raw,
                parse_application_options(&diagnostic, raw)?,
            )?,
            LineOption::AdhocNumber => set_once(
                &mut draft.hotline_destination,
                section,
                key,
                "<redacted>",
                parse_optional_hotline_destination(&diagnostic, raw)?,
            )?,
            LineOption::InitialDialtoneTone => {
                set_once(
                    &mut draft.initial_dialtone_tone,
                    section,
                    key,
                    raw,
                    parse_tone(&diagnostic, raw)?,
                )?;
            }
            LineOption::SecondaryDialtoneDigits => {
                set_once(
                    &mut draft.secondary_dialtone_digits,
                    section,
                    key,
                    raw,
                    parse_secondary_dialtone_digits(&diagnostic, raw)?,
                )?;
            }
            LineOption::SecondaryDialtoneTone => {
                set_once(
                    &mut draft.secondary_dialtone_tone,
                    section,
                    key,
                    raw,
                    parse_tone(&diagnostic, raw)?,
                )?;
            }
            LineOption::Pin => {
                let pin = parse_mobility_pin(&diagnostic, raw)?;
                set_once(&mut draft.mobility_pin, section, key, "<redacted>", pin)?;
            }
            LineOption::RegistrationExtension => {
                set_once(
                    &mut draft.registration_extensions,
                    section,
                    key,
                    raw,
                    parse_registration_extensions(&diagnostic, raw)?,
                )?;
            }
            LineOption::Allow => draft.codec_settings.push((true, raw.as_str())),
            LineOption::Disallow => draft.codec_settings.push((false, raw.as_str())),
            LineOption::VideoMode => set_once(
                &mut draft.video_mode,
                section,
                key,
                raw,
                parse_video_mode(&diagnostic, raw)?,
            )?,
            LineOption::AudioEncryption => set_once(
                &mut draft.audio_encryption,
                section,
                key,
                raw,
                parse_media_encryption_policy(&diagnostic, raw)?,
            )?,
            LineOption::EchoCancel => set_once(
                &mut draft.echo_cancellation,
                section,
                key,
                raw,
                parse_bool(&diagnostic, raw)?,
            )?,
            LineOption::SilenceSuppression => set_once(
                &mut draft.silence_suppression,
                section,
                key,
                raw,
                parse_bool(&diagnostic, raw)?,
            )?,
        }
    }

    let number = section.name.clone();
    let registration_extensions = draft.registration_extensions.take().flatten();
    if registration_extensions.is_some() && general.registration.contexts.is_empty() {
        return Err(invalid_option(
            section.diagnostic_key("regexten"),
            value(section, "regexten").unwrap_or_default(),
            "at least one general regcontext when regexten is configured",
            false,
        ));
    }
    if registration_extensions.is_none() && !general.registration.contexts.is_empty() {
        validate_registration_identifier(
            &section.section_location(),
            &number,
            "a logical line name usable as a registration extension",
        )?;
    }
    let registration_extensions = registration_extensions.unwrap_or_else(|| {
        vec![RegistrationExtension {
            extension: number.clone(),
            context: None,
        }]
    });
    let conference_destination = draft.conference_destination.take().unwrap_or(None);
    if draft.conference_enabled == Some(false)
        && (conference_destination.is_some() || draft.conference_options.is_some())
    {
        return Err(ConfigError::InvalidValue {
            key: format!("{}.meetme", section.name),
            value: "disabled with conference destination or options".into(),
        });
    }
    if draft.conference_enabled == Some(true) && conference_destination.is_none() {
        return Err(ConfigError::InvalidValue {
            key: format!("{}.meetmenum", section.name),
            value: "conference dialing is enabled without a destination".into(),
        });
    }
    let codecs = if draft.codec_settings.is_empty() {
        general.codecs.clone()
    } else {
        apply_codec_settings(
            Vec::new(),
            &draft.codec_settings,
            &format!("{}.codecs", section.name),
        )?
    };
    let (caller_name, caller_number) = value(section, "callerid")
        .map(parse_caller_id)
        .unwrap_or_else(|| (number.clone(), number.clone()));
    Ok(ParsedLine {
        line: LineConfig {
            number: number.clone(),
            label: value(section, "label").unwrap_or(&number).to_owned(),
            context: parse_required_setting(
                &format!("{}.context", section.name),
                value(section, "context").unwrap_or("from-sccp"),
            )?,
            caller_name,
            caller_number,
            mailbox: draft.mailbox.unwrap_or(None),
            language: draft.language.unwrap_or_else(|| general.language.clone()),
            account_code: draft
                .account_code
                .unwrap_or_else(|| general.account_code.clone()),
            channel_variables: draft.channel_variables,
        },
        features: LineFeatureConfig {
            incoming_limit: draft.incoming_limit.unwrap_or(6),
            voicemail: VoicemailDefaults {
                number: draft.voicemail_number.unwrap_or(None),
                transfer_destination: draft.voicemail_transfer.unwrap_or(None),
            },
            pickup: PickupConfig {
                call_groups: draft.call_groups.unwrap_or_default(),
                pickup_groups: draft.pickup_groups.unwrap_or_default(),
                named_call_groups: draft.named_call_groups.unwrap_or_default(),
                named_pickup_groups: draft.named_pickup_groups.unwrap_or_default(),
                directed: draft.directed_pickup.unwrap_or(true),
                directed_context: draft.directed_pickup_context.unwrap_or(None),
                answer_directed: draft.pickup_mode_answer.unwrap_or(true),
            },
            parking: LineParkingConfig {
                lot: draft.parking_lot.unwrap_or(None),
            },
            conference: LineConferenceConfig {
                enabled: draft.conference_enabled,
                destination: conference_destination,
                application_options: draft.conference_options,
            },
            hotline: LineHotlineConfig {
                destination: draft.hotline_destination.unwrap_or(None),
            },
            dial_tones: LineDialToneConfig {
                initial: draft.initial_dialtone_tone.unwrap_or(Tone::InsideDial),
                secondary_prefix: draft.secondary_dialtone_digits.unwrap_or(None),
                secondary: draft.secondary_dialtone_tone.unwrap_or(Tone::OutsideDial),
            },
            mobility: LineMobilityConfig {
                pin: draft.mobility_pin.flatten(),
            },
            registration: LineRegistrationConfig {
                extensions: registration_extensions,
            },
            media: LineMediaConfig {
                codecs,
                audio_encryption: draft
                    .audio_encryption
                    .unwrap_or_else(|| general.audio_encryption.clone()),
                video_mode: draft.video_mode.unwrap_or(VideoMode::Auto),
                audio_processing: AudioProcessingPolicy {
                    echo_cancellation: draft
                        .echo_cancellation
                        .map(|enabled| {
                            if enabled {
                                EchoCancellation::On
                            } else {
                                EchoCancellation::Off
                            }
                        })
                        .unwrap_or(general.audio_processing.echo_cancellation),
                    silence_suppression: draft
                        .silence_suppression
                        .map(|enabled| {
                            if enabled {
                                SilenceSuppression::On
                            } else {
                                SilenceSuppression::Off
                            }
                        })
                        .unwrap_or(general.audio_processing.silence_suppression),
                },
            },
        },
    })
}

fn parse_device(
    section: &RawSection,
    lines: &HashMap<String, LineConfig>,
    soft_key_profiles: &HashMap<String, SoftKeyProfile>,
    general: &GeneralConfig,
) -> Result<DeviceConfig, ConfigError> {
    let id = DeviceId::new(&section.name)
        .map_err(|_| ConfigError::InvalidDevice(section.name.clone()))?;
    let mut draft = DeviceSectionDraft::default();
    let mut section_values = SectionValues::new(section);

    for entry in deserialize_entries::<DeviceOption>(section)? {
        let key = &entry.source.key;
        let raw = &entry.source.value;
        let diagnostic = entry.source.diagnostic_key();
        let parsed = match entry.key {
            DeviceOption::Type | DeviceOption::Description => continue,
            DeviceOption::SoftkeyProfile => {
                if draft.soft_key_profile.is_some() {
                    return Err(invalid_option(
                        &diagnostic,
                        raw,
                        "one soft-key profile reference",
                        false,
                    ));
                }
                let name = canonical_profile_name(raw);
                if name.is_empty() {
                    return Err(invalid_option(
                        &diagnostic,
                        raw,
                        "the name of a declared soft-key profile",
                        false,
                    ));
                }
                if !soft_key_profiles.contains_key(&name) {
                    return Err(ConfigError::UnknownSoftKeyProfile {
                        device: id.clone(),
                        profile: raw.clone(),
                    });
                }
                draft.soft_key_profile = Some(name);
                continue;
            }
            DeviceOption::ForwardAllEnabled => {
                set_once(
                    &mut draft.forward_all_enabled,
                    section,
                    key,
                    raw,
                    parse_bool(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::ForwardBusyEnabled => {
                set_once(
                    &mut draft.forward_busy_enabled,
                    section,
                    key,
                    raw,
                    parse_bool(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::ForwardNoAnswerEnabled => {
                set_once(
                    &mut draft.forward_no_answer_enabled,
                    section,
                    key,
                    raw,
                    parse_bool(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::ForwardNoAnswerTimeout => {
                let timeout = parse::<u32>(&diagnostic, raw)?;
                if timeout == 0 || timeout > 86_400 {
                    return Err(ConfigError::InvalidValue {
                        key: diagnostic,
                        value: format!("{raw:?}; expected timeout seconds 1..86400"),
                    });
                }
                set_once(
                    &mut draft.forward_no_answer_timeout,
                    section,
                    key,
                    raw,
                    timeout,
                )?;
                continue;
            }
            DeviceOption::ForwardAll => {
                set_once(
                    &mut draft.forward_all,
                    section,
                    key,
                    raw,
                    parse_optional_forwarding_destination(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::ForwardBusy => {
                set_once(
                    &mut draft.forward_busy,
                    section,
                    key,
                    raw,
                    parse_optional_forwarding_destination(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::ForwardNoAnswer => {
                set_once(
                    &mut draft.forward_no_answer,
                    section,
                    key,
                    raw,
                    parse_optional_forwarding_destination(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::DndFeature => {
                set_once(
                    &mut draft.dnd_enabled,
                    section,
                    key,
                    raw,
                    parse_bool(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::Dnd => {
                set_once(
                    &mut draft.dnd,
                    section,
                    key,
                    raw,
                    parse_dnd_mode(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::PrivacyFeature => {
                set_once(
                    &mut draft.privacy_enabled,
                    section,
                    key,
                    raw,
                    parse_bool(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::Privacy => {
                set_once(
                    &mut draft.privacy,
                    section,
                    key,
                    raw,
                    parse_bool(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::FeatureDefault => {
                draft
                    .configured_feature_defaults
                    .push(parse_feature_default(&diagnostic, raw)?);
                continue;
            }
            DeviceOption::SetVariable => {
                push_channel_variable(&mut draft.channel_variables, &diagnostic, raw)?;
                continue;
            }
            DeviceOption::Park => {
                set_once(
                    &mut draft.parking_enabled,
                    section,
                    key,
                    raw,
                    parse_bool(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::ConferenceAllow => {
                set_once(
                    &mut draft.conference_allowed,
                    section,
                    key,
                    raw,
                    parse_bool(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::ConferenceMusicOnHoldClass => {
                set_once(
                    &mut draft.conference_music_on_hold_class,
                    section,
                    key,
                    raw,
                    parse_empty_optional_setting(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::ConferencePlayGeneralAnnounce => {
                set_once(
                    &mut draft.conference_play_general_announcements,
                    section,
                    key,
                    raw,
                    parse_bool(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::ConferencePlayParticipantAnnounce => {
                set_once(
                    &mut draft.conference_play_participant_announcements,
                    section,
                    key,
                    raw,
                    parse_bool(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::ConferenceMuteOnEntry => {
                set_once(
                    &mut draft.conference_mute_on_entry,
                    section,
                    key,
                    raw,
                    parse_bool(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::ConferenceShowList => {
                set_once(
                    &mut draft.conference_show_list,
                    section,
                    key,
                    raw,
                    parse_bool(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::ConferenceDialingEnabled => {
                set_once(
                    &mut draft.conference_dialing_enabled,
                    section,
                    key,
                    raw,
                    parse_bool(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::ConferenceOptions => {
                set_once(
                    &mut draft.conference_application_options,
                    section,
                    key,
                    raw,
                    parse_application_options(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::UseRedialMenu => {
                set_once(
                    &mut draft.use_redial_menu,
                    section,
                    key,
                    raw,
                    parse_bool(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::AllowRinginNotification => {
                set_once(
                    &mut draft.allow_ringing_notification,
                    section,
                    key,
                    raw,
                    parse_bool(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::MwiLamp => {
                let mode = match raw.trim().to_ascii_lowercase().as_str() {
                    "off" => LampMode::Off,
                    "on" => LampMode::On,
                    "wink" => LampMode::Wink,
                    "flash" => LampMode::Flash,
                    "blink" => LampMode::Blink,
                    _ => {
                        return Err(invalid_option(
                            &diagnostic,
                            raw,
                            "off, on, wink, flash, or blink",
                            false,
                        ));
                    }
                };
                set_once(&mut draft.mwi_lamp_mode, section, key, raw, mode)?;
                continue;
            }
            DeviceOption::MwiOnCall => {
                set_once(
                    &mut draft.mwi_on_call,
                    section,
                    key,
                    raw,
                    parse_bool(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::PhoneCodePage => {
                let code_page = match normalize_name(raw).as_str() {
                    "iso88591" | "latin1" => LegacyCodePage::Iso8859_1,
                    "ascii" | "usascii" => LegacyCodePage::Ascii,
                    _ => {
                        return Err(invalid_option(
                            &diagnostic,
                            raw,
                            "ISO8859-1 or ASCII",
                            false,
                        ));
                    }
                };
                set_once(&mut draft.legacy_code_page, section, key, raw, code_page)?;
                continue;
            }
            DeviceOption::AllowOverlap => {
                set_once(
                    &mut draft.allow_overlap,
                    section,
                    key,
                    raw,
                    parse_bool(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::ForceDtmfMode => {
                set_once(
                    &mut draft.dtmf_mode,
                    section,
                    key,
                    raw,
                    parse_dtmf_mode(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::DirectMedia => {
                set_once(
                    &mut draft.direct_media,
                    section,
                    key,
                    raw,
                    parse_bool(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::EarlyMedia => {
                set_once(
                    &mut draft.early_media,
                    section,
                    key,
                    raw,
                    parse_early_media(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::AudioEncryption => {
                set_once(
                    &mut draft.audio_encryption,
                    section,
                    key,
                    raw,
                    parse_media_encryption_policy(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::Deny | DeviceOption::Permit => {
                apply_acl_entry(
                    draft.acl_rules.get_or_insert_default(),
                    if matches!(entry.key, DeviceOption::Permit) {
                        AclAction::Permit
                    } else {
                        AclAction::Deny
                    },
                    &diagnostic,
                    raw,
                )?;
                continue;
            }
            DeviceOption::PermitHost => {
                let permitted_hosts = draft.permitted_hosts.get_or_insert_default();
                if raw.trim().is_empty() {
                    permitted_hosts.clear();
                } else {
                    let hostname = parse_hostname(&diagnostic, raw)?;
                    if permitted_hosts.contains(&hostname) {
                        return Err(invalid_option(
                            &diagnostic,
                            raw,
                            "a unique permitted hostname",
                            false,
                        ));
                    }
                    permitted_hosts.push(hostname);
                }
                continue;
            }
            DeviceOption::Nat => {
                set_once(
                    &mut draft.nat,
                    section,
                    key,
                    raw,
                    parse_nat_mode(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::Transport => {
                set_once(
                    &mut draft.transport,
                    section,
                    key,
                    raw,
                    parse_transport_requirement(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::SignalingTos => {
                section_values.claim_alias("signaling_dscp", entry.source)?;
                draft.qos.signaling_dscp = Some(parse_tos_as_dscp(&diagnostic, raw)?);
                continue;
            }
            DeviceOption::SignalingDscp => {
                section_values.claim_alias("signaling_dscp", entry.source)?;
                draft.qos.signaling_dscp = Some(parse_dscp(&diagnostic, raw)?);
                continue;
            }
            DeviceOption::SignalingCos => {
                section_values.claim_alias("signaling_cos", entry.source)?;
                draft.qos.signaling_cos = Some(parse_cos(&diagnostic, raw)?);
                continue;
            }
            DeviceOption::AudioTos => {
                section_values.claim_alias("audio_dscp", entry.source)?;
                draft.qos.audio_dscp = Some(parse_tos_as_dscp(&diagnostic, raw)?);
                continue;
            }
            DeviceOption::AudioDscp => {
                section_values.claim_alias("audio_dscp", entry.source)?;
                draft.qos.audio_dscp = Some(parse_dscp(&diagnostic, raw)?);
                continue;
            }
            DeviceOption::AudioCos => {
                section_values.claim_alias("audio_cos", entry.source)?;
                draft.qos.audio_cos = Some(parse_cos(&diagnostic, raw)?);
                continue;
            }
            DeviceOption::VideoTos => {
                section_values.claim_alias("video_dscp", entry.source)?;
                draft.qos.video_dscp = Some(parse_tos_as_dscp(&diagnostic, raw)?);
                continue;
            }
            DeviceOption::VideoDscp => {
                section_values.claim_alias("video_dscp", entry.source)?;
                draft.qos.video_dscp = Some(parse_dscp(&diagnostic, raw)?);
                continue;
            }
            DeviceOption::VideoCos => {
                section_values.claim_alias("video_cos", entry.source)?;
                draft.qos.video_cos = Some(parse_cos(&diagnostic, raw)?);
                continue;
            }
            DeviceOption::TrustPhoneIp | DeviceOption::ObsoleteDtmfMode => {
                return Err(invalid_option(
                    &diagnostic,
                    raw,
                    if matches!(entry.key, DeviceOption::TrustPhoneIp) {
                        "remove obsolete trustphoneip; peer addresses are always authoritative"
                    } else {
                        "remove obsolete dtmfmode and use force_dtmfmode"
                    },
                    false,
                ));
            }
            DeviceOption::Allow => {
                draft.codec_settings.push((true, raw.as_str()));
                continue;
            }
            DeviceOption::Disallow => {
                draft.codec_settings.push((false, raw.as_str()));
                continue;
            }
            DeviceOption::Line => parse_line_button(raw, &id, lines, &mut draft.instances)?,
            DeviceOption::Button => parse_button(raw, &id, lines, &mut draft.instances)?,
        };
        if let Some((instance, argument)) = parsed.feature_argument {
            draft.feature_arguments.insert(instance, argument);
        }
        draft.buttons.push(parsed.definition);
    }

    for feature in draft.buttons.iter().filter_map(|button| match button {
        ButtonDefinition::Feature(feature) if feature.feature == ButtonType::DoNotDisturb => {
            Some(feature)
        }
        _ => None,
    }) {
        let Some(argument) = draft.feature_arguments.get_mut(&feature.instance) else {
            continue;
        };
        let mode = parse_dnd_button_mode(
            &format!("{}.button.feature.{}", section.name, feature.instance),
            argument,
        )?;
        *argument = mode
            .canonical()
            .expect("a DND feature argument is never cycle")
            .to_owned();
    }

    let line_names: Vec<_> = draft
        .buttons
        .iter()
        .filter_map(|button| match button {
            ButtonDefinition::Line(line) => Some(line.number.clone()),
            _ => None,
        })
        .collect();
    if line_names.is_empty() {
        return Err(ConfigError::DeviceWithoutLines(id));
    }

    let description = value(section, "description")
        .unwrap_or(id.as_str())
        .to_owned();
    let resolved_soft_key_profile = draft
        .soft_key_profile
        .unwrap_or_else(|| DEFAULT_SOFT_KEY_PROFILE.to_owned());
    DeviceDefinition {
        id: id.clone(),
        description: description.clone(),
        transport: StationTransportRequirement::Either,
        signaling_qos: None,
        buttons: draft.buttons.clone(),
        soft_keys: soft_key_profiles
            .get(&resolved_soft_key_profile)
            .expect("device soft-key profile was validated during parsing")
            .station_profile(),
        ui: StationUiPolicy::default(),
    }
    .validate()
    .map_err(|error| ConfigError::InvalidValue {
        key: format!("{}.button", section.name),
        value: error.to_string(),
    })?;

    let mut feature_defaults = DeviceFeatureDefaults::default();
    feature_defaults.forwarding.all_enabled = draft.forward_all_enabled.unwrap_or(true);
    feature_defaults.forwarding.busy_enabled = draft.forward_busy_enabled.unwrap_or(true);
    feature_defaults.forwarding.no_answer_enabled = draft.forward_no_answer_enabled.unwrap_or(true);
    feature_defaults.forwarding.no_answer_timeout_seconds =
        draft.forward_no_answer_timeout.unwrap_or(30);
    feature_defaults.forwarding.all = draft.forward_all.unwrap_or(None);
    feature_defaults.forwarding.busy = draft.forward_busy.unwrap_or(None);
    feature_defaults.forwarding.no_answer = draft.forward_no_answer.unwrap_or(None);
    feature_defaults.dnd_enabled = draft.dnd_enabled.unwrap_or(true);
    feature_defaults.dnd = draft.dnd.unwrap_or(DndMode::Off);
    feature_defaults.privacy_enabled = draft.privacy_enabled.unwrap_or(true);
    feature_defaults.privacy = draft.privacy.unwrap_or(false);
    for feature in draft.buttons.iter().filter_map(|button| match button {
        ButtonDefinition::Feature(feature) => Some(feature),
        _ => None,
    }) {
        feature_defaults.buttons.insert(feature.instance, false);
    }
    for (instance, enabled) in draft.configured_feature_defaults {
        let Some(value) = feature_defaults.buttons.get_mut(&instance) else {
            return Err(ConfigError::InvalidValue {
                key: format!("{}.feature_default", section.name),
                value: instance.to_string(),
            });
        };
        *value = enabled;
    }

    let mut parking = DeviceParkingConfig {
        enabled: draft.parking_enabled.unwrap_or(true),
        feature_buttons: HashMap::new(),
    };
    for feature in draft.buttons.iter().filter_map(|button| match button {
        ButtonDefinition::Feature(feature) if feature.feature == ButtonType::ParkingLot => {
            Some(feature)
        }
        _ => None,
    }) {
        let button = parse_parking_lot_button(
            &format!("{}.button.feature.{}", section.name, feature.instance),
            draft
                .feature_arguments
                .get(&feature.instance)
                .map(String::as_str),
        )?;
        parking.feature_buttons.insert(feature.instance, button);
    }

    let conference = DeviceConferenceConfig {
        allowed: draft.conference_allowed.unwrap_or(true),
        music_on_hold_class: draft
            .conference_music_on_hold_class
            .unwrap_or_else(|| Some("default".into())),
        play_general_announcements: draft.conference_play_general_announcements.unwrap_or(true),
        play_participant_announcements: draft
            .conference_play_participant_announcements
            .unwrap_or(true),
        mute_on_entry: draft.conference_mute_on_entry.unwrap_or(false),
        show_conference_list: draft.conference_show_list.unwrap_or(true),
        dialing: ConferenceDialingConfig {
            enabled: draft
                .conference_dialing_enabled
                .unwrap_or(general.conference_dialing.enabled),
            application_options: draft
                .conference_application_options
                .unwrap_or_else(|| general.conference_dialing.application_options.clone()),
        },
    };
    let call_ui = DeviceCallUiConfig {
        redial_mode: if draft.use_redial_menu.unwrap_or(false) {
            RedialMode::PlacedCallsMenu
        } else {
            RedialMode::LastNumber
        },
        hinted_ringing_notification: draft.allow_ringing_notification.unwrap_or(false),
        mwi_lamp_mode: draft.mwi_lamp_mode.unwrap_or(LampMode::On),
        mwi_on_call: draft.mwi_on_call.unwrap_or(false),
        legacy_code_page: draft.legacy_code_page.unwrap_or(LegacyCodePage::Iso8859_1),
    };
    let codecs = if draft.codec_settings.is_empty() {
        general.codecs.clone()
    } else {
        apply_codec_settings(
            Vec::new(),
            &draft.codec_settings,
            &format!("{}.codecs", section.name),
        )?
    };
    let media = DeviceMediaConfig {
        codecs,
        audio_encryption: draft
            .audio_encryption
            .unwrap_or_else(|| general.audio_encryption.clone()),
        dtmf_mode: draft.dtmf_mode.unwrap_or(DtmfMode::Auto),
        direct_media: draft.direct_media.unwrap_or(general.direct_media),
        early_media: draft.early_media.unwrap_or(general.early_media),
    };
    let transport = draft.transport.unwrap_or_default();
    if transport == TransportRequirement::Tls && general.listeners.tls.is_none() {
        return Err(invalid_option(
            section.diagnostic_key("transport"),
            "tls",
            "a configured general TLS listener and credentials",
            false,
        ));
    }
    let network = DeviceNetworkPolicy {
        acl: draft.acl_rules.map_or_else(
            || general.network.acl.clone(),
            |rules| AccessControlList { rules },
        ),
        permitted_hosts: draft.permitted_hosts.unwrap_or_default(),
        nat: draft.nat.unwrap_or(general.network.nat),
        qos: draft.qos.resolve(general.qos),
        transport,
    };

    Ok(DeviceConfig {
        id,
        description,
        lines: line_names,
        buttons: draft.buttons,
        feature_arguments: draft.feature_arguments,
        channel_variables: draft.channel_variables,
        soft_key_profile: resolved_soft_key_profile,
        feature_defaults,
        parking,
        conference,
        call_ui,
        allow_overlap: draft.allow_overlap.unwrap_or(general.allow_overlap),
        media,
        network,
    })
}

fn parse_line_button(
    raw: &str,
    device: &DeviceId,
    lines: &HashMap<String, LineConfig>,
    instances: &mut ButtonInstances,
) -> Result<ParsedButton, ConfigError> {
    let fields: Vec<_> = raw.split(',').map(str::trim).collect();
    let number = required_button_field(fields[0], "line")?;
    let line = lines.get(number).ok_or_else(|| ConfigError::UnknownLine {
        device: device.clone(),
        line: number.to_owned(),
    })?;
    let instance = ButtonInstances::next(&mut instances.line);
    let mut appearance = LineAppearance::new(
        instance,
        LineDefinition {
            number: line.number.clone(),
            display_name: line.label.clone(),
        },
    );
    let mut options = HashSet::new();
    for option in &fields[1..] {
        let Some((key, value)) = option.split_once('=') else {
            return Err(invalid_button(raw));
        };
        let key = normalize_name(required_button_field(key, raw)?);
        let value = required_button_field(value, raw)?;
        if !options.insert(key.clone()) {
            return Err(ConfigError::InvalidValue {
                key: format!("button.line.{key}"),
                value: raw.into(),
            });
        }
        match key.as_str() {
            "label" => appearance.label = Some(value.into()),
            "callername" => appearance.caller_id.name = Some(value.into()),
            "callernumber" => appearance.caller_id.number = Some(value.into()),
            "ring" | "ringmode" => {
                appearance.ring_mode = match normalize_name(value).as_str() {
                    "normal" => AppearanceRingMode::Normal,
                    "silent" => AppearanceRingMode::Silent,
                    "disabled" | "off" => AppearanceRingMode::Disabled,
                    _ => {
                        return Err(ConfigError::InvalidValue {
                            key: "button.line.ring".into(),
                            value: value.into(),
                        });
                    }
                }
            }
            "subscription" | "subscriptionidentity" => {
                appearance.subscription_identity = Some(value.into())
            }
            "privacy" => appearance.privacy = parse_bool("button.line.privacy", value)?,
            _ => {
                return Err(ConfigError::InvalidValue {
                    key: format!("button.line.{key}"),
                    value: value.into(),
                });
            }
        }
    }
    Ok(ParsedButton {
        definition: ButtonDefinition::Line(appearance),
        feature_argument: None,
    })
}

fn parse_button(
    raw: &str,
    device: &DeviceId,
    lines: &HashMap<String, LineConfig>,
    instances: &mut ButtonInstances,
) -> Result<ParsedButton, ConfigError> {
    let fields: Vec<_> = raw.split(',').map(str::trim).collect();
    let Some(kind) = fields.first().copied().filter(|kind| !kind.is_empty()) else {
        return Err(invalid_button(raw));
    };

    match normalize_name(kind).as_str() {
        "line" if fields.len() >= 2 => {
            parse_line_button(&fields[1..].join(","), device, lines, instances)
        }
        "speeddial" if matches!(fields.len(), 3 | 4) => {
            let label = required_button_field(fields[1], raw)?;
            let number = required_button_field(fields[2], raw)?;
            if fields.len() == 4 {
                let hint = parse_blf_hint(fields[3], raw)?;
                let instance = ButtonInstances::next(&mut instances.speed_dial);
                Ok(ParsedButton {
                    definition: ButtonDefinition::BlfSpeedDial(BlfSpeedDialDefinition {
                        instance,
                        number: number.to_owned(),
                        display_name: label.to_owned(),
                        hint: hint.to_owned(),
                    }),
                    feature_argument: None,
                })
            } else {
                let instance = ButtonInstances::next(&mut instances.speed_dial);
                Ok(ParsedButton {
                    definition: ButtonDefinition::SpeedDial(SpeedDialDefinition {
                        instance,
                        number: number.to_owned(),
                        display_name: label.to_owned(),
                    }),
                    feature_argument: None,
                })
            }
        }
        "blf" | "blfspeeddial" if fields.len() == 4 => {
            let label = required_button_field(fields[1], raw)?;
            let number = required_button_field(fields[2], raw)?;
            let hint = parse_blf_hint(fields[3], raw)?;
            let instance = ButtonInstances::next(&mut instances.speed_dial);
            Ok(ParsedButton {
                definition: ButtonDefinition::BlfSpeedDial(BlfSpeedDialDefinition {
                    instance,
                    number: number.to_owned(),
                    display_name: label.to_owned(),
                    hint: hint.to_owned(),
                }),
                feature_argument: None,
            })
        }
        "feature" if fields.len() >= 3 => {
            let label = required_button_field(fields[1], raw)?;
            let feature_name = required_button_field(fields[2], raw)?;
            let feature = parse_feature(feature_name)?;
            let instance = ButtonInstances::next(&mut instances.feature);
            let feature_argument = if fields.len() > 3 {
                let argument = fields[3..].join(",");
                Some((instance, required_button_field(&argument, raw)?.to_owned()))
            } else {
                None
            };
            Ok(ParsedButton {
                definition: ButtonDefinition::Feature(FeatureDefinition {
                    instance,
                    label: label.to_owned(),
                    feature,
                }),
                feature_argument,
            })
        }
        "service" if fields.len() >= 3 => {
            let label = required_button_field(fields[1], raw)?;
            let url = fields[2..].join(",");
            let url = required_button_field(&url, raw)?;
            let instance = ButtonInstances::next(&mut instances.service);
            Ok(ParsedButton {
                definition: ButtonDefinition::Service(ServiceDefinition {
                    instance,
                    label: label.to_owned(),
                    url: url.to_owned(),
                }),
                feature_argument: None,
            })
        }
        "empty" | "unused" if fields.len() == 1 => Ok(ParsedButton {
            definition: ButtonDefinition::Unused,
            feature_argument: None,
        }),
        "addon" | "addonmodule" if fields.len() == 3 => {
            let slot = parse::<u32>("button.addon.slot", required_button_field(fields[1], raw)?)?;
            if !(1..=56).contains(&slot) {
                return Err(ConfigError::InvalidValue {
                    key: "button.addon.slot".into(),
                    value: slot.to_string(),
                });
            }
            let device_type = parse_addon_type(required_button_field(fields[2], raw)?)?;
            Ok(ParsedButton {
                definition: ButtonDefinition::AddonModule(AddonModuleDefinition {
                    slot,
                    device_type,
                }),
                feature_argument: None,
            })
        }
        _ => Err(invalid_button(raw)),
    }
}

fn parse_feature(raw: &str) -> Result<ButtonType, ConfigError> {
    let feature = match normalize_name(raw).as_str() {
        "redial" | "lastnumberredial" => ButtonType::LastNumberRedial,
        "hold" => ButtonType::Hold,
        "transfer" => ButtonType::Transfer,
        "forwardall" | "cfwdall" => ButtonType::ForwardAll,
        "forwardbusy" | "cfwdbusy" => ButtonType::ForwardBusy,
        "forwardnoanswer" | "cfwdnoanswer" => ButtonType::ForwardNoAnswer,
        "video" => ButtonType::Video,
        "voicemail" => ButtonType::Voicemail,
        "answerrelease" => ButtonType::AnswerRelease,
        "autoanswer" => ButtonType::AutoAnswer,
        "select" => ButtonType::Select,
        "feature" => ButtonType::Feature,
        "maliciouscall" => ButtonType::MaliciousCall,
        "meetme" | "meetmeconference" => ButtonType::MeetMeConference,
        "conference" => ButtonType::Conference,
        "park" | "callpark" => ButtonType::CallPark,
        "pickup" | "callpickup" => ButtonType::CallPickup,
        "grouppickup" | "groupcallpickup" => ButtonType::GroupCallPickup,
        "mobility" => ButtonType::Mobility,
        "dnd" | "donotdisturb" => ButtonType::DoNotDisturb,
        "conferencelist" => ButtonType::ConferenceList,
        "removelastparticipant" => ButtonType::RemoveLastParticipant,
        "qualityreport" | "qualityreporttool" => ButtonType::QualityReportTool,
        "callback" => ButtonType::Callback,
        "otherpickup" => ButtonType::OtherPickup,
        "videomode" => ButtonType::VideoMode,
        "newcall" => ButtonType::NewCall,
        "endcall" => ButtonType::EndCall,
        "huntgrouplogin" => ButtonType::HuntGroupLogin,
        "queue" | "queuing" => ButtonType::Queuing,
        "parkinglot" => ButtonType::ParkingLot,
        "messages" => ButtonType::Messages,
        "directory" => ButtonType::Directory,
        "application" => ButtonType::Application,
        "headset" => ButtonType::Headset,
        "echocancellation" | "acousticechocancellation" => ButtonType::AcousticEchoCancellation,
        _ => {
            return Err(ConfigError::InvalidValue {
                key: "button.feature".into(),
                value: raw.into(),
            });
        }
    };
    Ok(feature)
}

fn parse_addon_type(raw: &str) -> Result<DeviceType, ConfigError> {
    let device_type = match normalize_name(raw).as_str() {
        "7914" | "cisco7914" | "ciscoaddon7914" => DeviceType::CiscoAddon7914,
        "791512" | "cisco791512" | "ciscoaddon791512" => DeviceType::CiscoAddon7915_12,
        "791524" | "cisco791524" | "ciscoaddon791524" => DeviceType::CiscoAddon7915_24,
        "791612" | "cisco791612" | "ciscoaddon791612" => DeviceType::CiscoAddon7916_12,
        "791624" | "cisco791624" | "ciscoaddon791624" => DeviceType::CiscoAddon7916_24,
        "spa500s" | "addonspa500s" => DeviceType::AddonSpa500s,
        "spa500ds" | "addonspa500ds" => DeviceType::AddonSpa500ds,
        "spa932ds" | "addonspa932ds" => DeviceType::AddonSpa932ds,
        _ => {
            return Err(ConfigError::InvalidValue {
                key: "button.addon.type".into(),
                value: raw.into(),
            });
        }
    };
    Ok(device_type)
}

fn required_button_field<'a>(field: &'a str, raw: &str) -> Result<&'a str, ConfigError> {
    let field = field.trim();
    if field.is_empty() {
        Err(invalid_button(raw))
    } else {
        Ok(field)
    }
}

fn parse_blf_hint<'a>(field: &'a str, raw: &str) -> Result<&'a str, ConfigError> {
    let hint = required_button_field(field, raw)?;
    let Some((extension, context)) = hint.split_once('@') else {
        return Err(ConfigError::InvalidValue {
            key: "button.blf.hint".into(),
            value: hint.into(),
        });
    };
    if extension.trim().is_empty() || context.trim().is_empty() || context.contains('@') {
        return Err(ConfigError::InvalidValue {
            key: "button.blf.hint".into(),
            value: hint.into(),
        });
    }
    Ok(hint)
}

fn invalid_button(raw: &str) -> ConfigError {
    ConfigError::InvalidValue {
        key: "button".into(),
        value: raw.into(),
    }
}

fn normalize_name(raw: &str) -> String {
    raw.chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn parse_sections(input: &str) -> Result<Vec<RawSection>, ConfigError> {
    let mut sections = Vec::<RawSection>::new();
    let mut current: Option<RawSection> = None;
    let mut names = HashSet::new();
    for (index, raw_line) in input.lines().enumerate() {
        let line_number = index + 1;
        let line = strip_inline_comment(raw_line).trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            let Some(close) = line.find(']') else {
                return Err(ConfigError::Syntax {
                    line: line_number,
                    message: "malformed section header".into(),
                });
            };
            let name = line[1..close].trim().to_owned();
            if name.is_empty() {
                return Err(ConfigError::Syntax {
                    line: line_number,
                    message: "section name cannot be empty".into(),
                });
            }
            let suffix = line[close + 1..].trim();
            let mut is_template = false;
            let mut parents = Vec::new();
            if !suffix.is_empty() {
                let Some(specification) = suffix
                    .strip_prefix('(')
                    .and_then(|suffix| suffix.strip_suffix(')'))
                else {
                    return Err(ConfigError::Syntax {
                        line: line_number,
                        message: "malformed inheritance list".into(),
                    });
                };
                let mut inherited_names = HashSet::new();
                for entry in specification.split(',') {
                    let entry = entry.trim();
                    if entry.is_empty() {
                        return Err(ConfigError::Syntax {
                            line: line_number,
                            message: "empty inheritance entry".into(),
                        });
                    }
                    if entry == "!" {
                        if is_template {
                            return Err(ConfigError::Syntax {
                                line: line_number,
                                message: "duplicate template marker".into(),
                            });
                        }
                        is_template = true;
                    } else {
                        let canonical = entry.to_ascii_lowercase();
                        if !inherited_names.insert(canonical) {
                            return Err(ConfigError::Syntax {
                                line: line_number,
                                message: format!("duplicate parent template [{entry}]"),
                            });
                        }
                        parents.push(entry.to_owned());
                    }
                }
            }
            if let Some(section) = current.take() {
                sections.push(section);
            }
            let canonical = name.to_ascii_lowercase();
            if !names.insert(canonical) {
                return Err(ConfigError::DuplicateSection(name));
            }
            current = Some(RawSection {
                name,
                line: line_number,
                is_template,
                parents,
                values: Vec::new(),
            });
            continue;
        }
        let Some(section) = current.as_mut() else {
            return Err(ConfigError::Syntax {
                line: line_number,
                message: "setting appears before a section".into(),
            });
        };
        let Some((key, value)) = line.split_once('=') else {
            return Err(ConfigError::Syntax {
                line: line_number,
                message: "expected key = value".into(),
            });
        };
        section.values.push(RawValue {
            key: key.trim().to_owned(),
            value: unquote(value.trim()),
            line: line_number,
            section: section.name.clone(),
        });
    }
    if let Some(section) = current {
        sections.push(section);
    }
    Ok(sections)
}

fn apply_config_overlays(
    sections: &mut Vec<RawSection>,
    overlays: &[ConfigOverlaySection],
) -> Result<(), ConfigError> {
    for overlay in overlays {
        if overlay.name.trim().is_empty() {
            return Err(ConfigError::Syntax {
                line: overlay.line,
                message: format!("{} has an empty section name", overlay.source),
            });
        }
        if overlay.delete {
            sections.retain(|section| !section.name.eq_ignore_ascii_case(&overlay.name));
            continue;
        }

        let index = sections
            .iter()
            .position(|section| section.name.eq_ignore_ascii_case(&overlay.name));
        let index = if let Some(index) = index {
            index
        } else {
            sections.push(RawSection {
                name: overlay.name.clone(),
                line: overlay.line,
                is_template: false,
                parents: Vec::new(),
                values: Vec::new(),
            });
            sections.len() - 1
        };
        let section = &mut sections[index];
        let kind = overlay.kind.map(|kind| match kind {
            ConfigOverlayKind::Device => TemplateKind::Device,
            ConfigOverlayKind::Line => TemplateKind::Line,
        });
        let mut values = overlay.values.clone();
        if let Some(kind) = kind {
            values.insert(
                0,
                ConfigOverlayValue {
                    key: "type".into(),
                    value: Some(kind.as_str().into()),
                },
            );
        }

        let mut replaced = HashSet::new();
        for value in values {
            let identity = overlay_option_identity(kind, &value.key);
            if replaced.insert(identity.clone()) {
                section
                    .values
                    .retain(|candidate| overlay_option_identity(kind, &candidate.key) != identity);
            }
            if let Some(raw) = value.value {
                section.values.push(RawValue {
                    key: value.key.trim().to_ascii_lowercase(),
                    value: raw,
                    line: overlay.line,
                    section: overlay.source.clone(),
                });
            }
        }
    }
    Ok(())
}

fn overlay_option_identity(kind: Option<TemplateKind>, key: &str) -> String {
    let normalized = normalize_name(key);
    if let Some(kind) = kind {
        return template_option_identity(kind, &normalized);
    }
    match normalized.as_str() {
        "clearbind" => "bind".into(),
        "clearbindaddr" => "bindaddr".into(),
        "clearport" => "port".into(),
        "advertisedaddressipv4" => "advertisedipv4".into(),
        "advertisedaddressipv6" => "advertisedipv6".into(),
        "securebind" => "tlsbind".into(),
        "tlsbindaddr" => "secbindaddr".into(),
        "tlsport" => "secport".into(),
        "tlscombinedpem" => "certfile".into(),
        "tlscertificatefile" => "tlscertificate".into(),
        "tlsprivatekeyfile" => "tlsprivatekey".into(),
        "tlscafile" => "tlstruststore".into(),
        "externaladdress" => "externip".into(),
        "externalhost" => "externhost".into(),
        "externalrefresh" => "externrefresh".into(),
        "signalingtos" | "sccpdscp" | "signalingdscp" => "sccptos".into(),
        "signalingcos" => "sccpcos".into(),
        "audiodscp" => "audiotos".into(),
        "videodscp" => "videotos".into(),
        _ => normalized,
    }
}

fn internal_networks() -> Vec<IpNetwork> {
    vec![
        IpNetwork {
            address: "10.0.0.0".parse().expect("constant IPv4 address"),
            prefix: 8,
        },
        IpNetwork {
            address: "172.16.0.0".parse().expect("constant IPv4 address"),
            prefix: 12,
        },
        IpNetwork {
            address: "192.168.0.0".parse().expect("constant IPv4 address"),
            prefix: 16,
        },
    ]
}

fn invalid_option(
    key: impl Into<String>,
    raw: &str,
    expected: &str,
    sensitive: bool,
) -> ConfigError {
    let found = if sensitive { "<redacted>" } else { raw };
    ConfigError::InvalidValue {
        key: key.into(),
        value: format!("{found:?}; expected {expected}"),
    }
}

fn locate_section_error(error: ConfigError, section: &RawSection) -> ConfigError {
    let ConfigError::InvalidValue { key, mut value } = error else {
        return error;
    };
    if key.starts_with("line ") {
        return ConfigError::InvalidValue { key, value };
    }

    let key_parts: Vec<_> = key.split('.').map(normalize_name).collect();
    let source = section.values.iter().rev().find(|entry| {
        key_parts.contains(&normalize_name(&entry.key))
            || key_parts.iter().any(|part| {
                part == "codecs"
                    && matches!(normalize_name(&entry.key).as_str(), "allow" | "disallow")
            })
    });
    let located_key = source.map_or_else(
        || format!("{}.{}", section.section_location(), key),
        RawValue::diagnostic_key,
    );
    if !value.contains("expected") {
        value.push_str("; expected a valid value for this setting");
    }
    ConfigError::InvalidValue {
        key: located_key,
        value,
    }
}

fn parse_ip_networks(key: &str, raw: &str) -> Result<Vec<IpNetwork>, ConfigError> {
    let raw = raw.trim();
    if raw.eq_ignore_ascii_case("internal") {
        return Ok(internal_networks());
    }
    let (address, mask) = raw.split_once('/').ok_or_else(|| {
        invalid_option(
            key,
            raw,
            "internal or an IPv4/IPv6 network in address/prefix form",
            false,
        )
    })?;
    let address: IpAddr = address.trim().parse().map_err(|_| {
        invalid_option(
            key,
            raw,
            "internal or an IPv4/IPv6 network in address/prefix form",
            false,
        )
    })?;
    let prefix = match address {
        IpAddr::V4(_) => mask
            .trim()
            .parse::<u8>()
            .ok()
            .filter(|prefix| *prefix <= 32)
            .or_else(|| {
                let mask = mask.trim().parse::<Ipv4Addr>().ok()?;
                let bits = u32::from(mask);
                let prefix = bits.leading_ones() as u8;
                (bits == u32::MAX.checked_shl(u32::from(32 - prefix)).unwrap_or(0))
                    .then_some(prefix)
            }),
        IpAddr::V6(_) => mask
            .trim()
            .parse::<u8>()
            .ok()
            .filter(|prefix| *prefix <= 128),
    }
    .ok_or_else(|| {
        invalid_option(
            key,
            raw,
            "a contiguous IPv4 netmask/prefix 0..32 or IPv6 prefix 0..128",
            false,
        )
    })?;
    let address = match address {
        IpAddr::V4(address) => {
            let mask = if prefix == 0 {
                0
            } else {
                u32::MAX << (32 - prefix)
            };
            IpAddr::V4(Ipv4Addr::from(u32::from(address) & mask))
        }
        IpAddr::V6(address) => {
            let mask = if prefix == 0 {
                0
            } else {
                u128::MAX << (128 - prefix)
            };
            IpAddr::V6(Ipv6Addr::from(u128::from(address) & mask))
        }
    };
    Ok(vec![IpNetwork { address, prefix }])
}

fn apply_acl_entry(
    rules: &mut Vec<AclRule>,
    action: AclAction,
    key: &str,
    raw: &str,
) -> Result<(), ConfigError> {
    if raw.trim().is_empty() {
        rules.clear();
        return Ok(());
    }
    rules.extend(
        parse_ip_networks(key, raw)?
            .into_iter()
            .map(|network| AclRule { action, network }),
    );
    Ok(())
}

fn parse_nat_mode(key: &str, raw: &str) -> Result<NatMode, ConfigError> {
    match normalize_name(raw).as_str() {
        "auto" => Ok(NatMode::Auto),
        "off" => Ok(NatMode::Off),
        "autooff" => Ok(NatMode::AutoOff),
        "on" => Ok(NatMode::On),
        "autoon" => Ok(NatMode::AutoOn),
        _ => Err(invalid_option(
            key,
            raw,
            "auto, off, (auto)off, on, or (auto)on",
            false,
        )),
    }
}

fn parse_dscp(key: &str, raw: &str) -> Result<Dscp, ConfigError> {
    let normalized = normalize_name(raw);
    let named = match normalized.as_str() {
        "none" => Some(0),
        "ef" => Some(46),
        "lowdelay" => Some(4),
        "throughput" => Some(2),
        "reliability" => Some(1),
        "mincost" => Some(0),
        value if value.len() == 3 && value.starts_with("cs") => value[2..]
            .parse::<u8>()
            .ok()
            .filter(|class| *class <= 7)
            .map(|class| class * 8),
        value if value.len() == 4 && value.starts_with("af") => {
            let class = value[2..3].parse::<u8>().ok();
            let drop = value[3..].parse::<u8>().ok();
            match (class, drop) {
                (Some(class @ 1..=4), Some(drop @ 1..=3)) => Some(class * 8 + drop * 2),
                _ => None,
            }
        }
        _ => None,
    };
    let value = named.or_else(|| raw.trim().parse::<u8>().ok());
    value.filter(|value| *value <= 63).map(Dscp).ok_or_else(|| {
        invalid_option(
            key,
            raw,
            "DSCP 0..63, CS0..CS7, AF11..AF43, EF, or none",
            false,
        )
    })
}

fn parse_tos_as_dscp(key: &str, raw: &str) -> Result<Dscp, ConfigError> {
    if let Ok(dscp) = parse_dscp(key, raw) {
        return Ok(dscp);
    }
    let trimmed = raw.trim();
    let value = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .map(|hex| u8::from_str_radix(hex, 16))
        .unwrap_or_else(|| trimmed.parse::<u8>())
        .map_err(|_| {
            invalid_option(key, raw, "TOS byte 0..255/0x00..0xff or a DSCP name", false)
        })?;
    Ok(Dscp(value >> 2))
}

fn parse_cos(key: &str, raw: &str) -> Result<Cos, ConfigError> {
    raw.trim()
        .parse::<u8>()
        .ok()
        .filter(|value| *value <= 7)
        .map(Cos)
        .ok_or_else(|| invalid_option(key, raw, "COS priority 0..7", false))
}

fn parse_transport_requirement(key: &str, raw: &str) -> Result<TransportRequirement, ConfigError> {
    match normalize_name(raw).as_str() {
        "clear" | "tcp" => Ok(TransportRequirement::Clear),
        "tls" | "secure" => Ok(TransportRequirement::Tls),
        "either" | "any" => Ok(TransportRequirement::Either),
        _ => Err(invalid_option(key, raw, "clear, tls, or either", false)),
    }
}

fn parse_path(key: &str, raw: &str, sensitive: bool) -> Result<PathBuf, ConfigError> {
    let value = raw.trim();
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err(invalid_option(
            key,
            raw,
            "a non-empty filesystem path without control characters",
            sensitive,
        ));
    }
    Ok(PathBuf::from(value))
}

fn parse_hostname(key: &str, raw: &str) -> Result<String, ConfigError> {
    let value = raw.trim();
    if value.is_empty()
        || value.len() > 253
        || value.starts_with('.')
        || value.ends_with('.')
        || value.split('.').any(|label| {
            label.is_empty()
                || label.len() > 63
                || label.starts_with('-')
                || label.ends_with('-')
                || !label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
    {
        return Err(invalid_option(
            key,
            raw,
            "a valid DNS hostname up to 253 bytes",
            false,
        ));
    }
    Ok(value.to_ascii_lowercase())
}

fn parse_general(config: &mut GeneralConfig, section: &RawSection) -> Result<(), ConfigError> {
    let mut draft = GeneralSectionDraft::default();
    let mut section_values = SectionValues::new(section);
    for entry in deserialize_entries::<GeneralOption>(section)? {
        let key = &entry.source.key;
        let raw = &entry.source.value;
        let diagnostic = entry.source.diagnostic_key();
        match entry.key {
            GeneralOption::DateFormat => set_once(
                &mut draft.date_template,
                section,
                key,
                raw,
                DateTemplate::new(raw.trim())
                    .map_err(|error| invalid_option(&diagnostic, raw, &error.to_string(), false))?,
            )?,
            GeneralOption::TimezoneOffset => {
                let hours = raw
                    .trim()
                    .parse::<i16>()
                    .ok()
                    .filter(|hours| (-14..=14).contains(hours))
                    .ok_or_else(|| {
                        invalid_option(&diagnostic, raw, "UTC offset -14..14 hours", false)
                    })?;
                set_once(
                    &mut draft.timezone_offset_minutes,
                    section,
                    key,
                    raw,
                    hours * 60,
                )?;
            }
            GeneralOption::Bind => {
                let address = parse::<SocketAddr>(&diagnostic, raw).map_err(|_| {
                    invalid_option(
                        &diagnostic,
                        raw,
                        "an IPv4/IPv6 socket address with port",
                        false,
                    )
                })?;
                if address.port() == 0 {
                    return Err(invalid_option(
                        &diagnostic,
                        raw,
                        "clear listener port 1..65535",
                        false,
                    ));
                }
                set_once(&mut draft.clear_bind, section, key, raw, address)?;
            }
            GeneralOption::BindAddress => {
                let address = parse::<IpAddr>(&diagnostic, raw).map_err(|_| {
                    invalid_option(&diagnostic, raw, "an IPv4 or IPv6 address", false)
                })?;
                set_once(&mut draft.clear_address, section, key, raw, address)?;
            }
            GeneralOption::Port => {
                let port = parse::<u16>(&diagnostic, raw)
                    .map_err(|_| invalid_option(&diagnostic, raw, "TCP port 1..65535", false))?;
                if port == 0 {
                    return Err(invalid_option(&diagnostic, raw, "TCP port 1..65535", false));
                }
                set_once(&mut draft.clear_port, section, key, raw, port)?;
            }
            GeneralOption::AdvertisedAddress => {
                if draft.advertised_alias_seen
                    || draft.advertised_ipv4.is_some()
                    || draft.advertised_ipv6.is_some()
                {
                    return Err(invalid_option(
                        &diagnostic,
                        raw,
                        "one advertised_address or explicit advertised_ipv4/advertised_ipv6 values",
                        false,
                    ));
                }
                draft.advertised_alias_seen = true;
                let address: IpAddr = parse(&diagnostic, raw).map_err(|_| {
                    invalid_option(
                        &diagnostic,
                        raw,
                        "a non-unspecified IPv4 or IPv6 address",
                        false,
                    )
                })?;
                if address.is_unspecified() {
                    return Err(invalid_option(
                        &diagnostic,
                        raw,
                        "a non-unspecified IPv4 or IPv6 address",
                        false,
                    ));
                }
                match address {
                    IpAddr::V4(address) => {
                        draft.advertised_ipv4 = Some(Some(address));
                        draft.advertised_ipv6 = Some(None);
                    }
                    IpAddr::V6(address) => {
                        draft.advertised_ipv4 = Some(None);
                        draft.advertised_ipv6 = Some(Some(address));
                    }
                }
            }
            GeneralOption::AdvertisedIpv4 => {
                if draft.advertised_alias_seen || draft.advertised_ipv4.is_some() {
                    return Err(invalid_option(
                        &diagnostic,
                        raw,
                        "one value for the advertised IPv4 address",
                        false,
                    ));
                }
                let value = raw.trim();
                draft.advertised_ipv4 =
                    Some(if value.is_empty() || value.eq_ignore_ascii_case("none") {
                        None
                    } else {
                        let address: Ipv4Addr = parse(&diagnostic, value).map_err(|_| {
                            invalid_option(&diagnostic, raw, "an IPv4 address or none", false)
                        })?;
                        if address.is_unspecified() {
                            return Err(invalid_option(
                                &diagnostic,
                                raw,
                                "a non-unspecified IPv4 address or none",
                                false,
                            ));
                        }
                        Some(address)
                    });
            }
            GeneralOption::AdvertisedIpv6 => {
                if draft.advertised_alias_seen || draft.advertised_ipv6.is_some() {
                    return Err(invalid_option(
                        &diagnostic,
                        raw,
                        "one value for the advertised IPv6 address",
                        false,
                    ));
                }
                let value = raw.trim();
                draft.advertised_ipv6 =
                    Some(if value.is_empty() || value.eq_ignore_ascii_case("none") {
                        None
                    } else {
                        let address: Ipv6Addr = parse(&diagnostic, value).map_err(|_| {
                            invalid_option(&diagnostic, raw, "an IPv6 address or none", false)
                        })?;
                        if address.is_unspecified() {
                            return Err(invalid_option(
                                &diagnostic,
                                raw,
                                "a non-unspecified IPv6 address or none",
                                false,
                            ));
                        }
                        Some(address)
                    });
            }
            GeneralOption::TlsBind => {
                let address = parse::<SocketAddr>(&diagnostic, raw).map_err(|_| {
                    invalid_option(
                        &diagnostic,
                        raw,
                        "an IPv4/IPv6 TLS socket address with port",
                        false,
                    )
                })?;
                if address.port() == 0 {
                    return Err(invalid_option(
                        &diagnostic,
                        raw,
                        "TLS listener port 1..65535",
                        false,
                    ));
                }
                set_once(&mut draft.tls_bind, section, key, raw, address)?;
            }
            GeneralOption::TlsBindAddress => {
                let address = parse::<IpAddr>(&diagnostic, raw).map_err(|_| {
                    invalid_option(&diagnostic, raw, "an IPv4 or IPv6 TLS bind address", false)
                })?;
                set_once(&mut draft.tls_address, section, key, raw, address)?;
            }
            GeneralOption::TlsPort => {
                let port = parse::<u16>(&diagnostic, raw)
                    .map_err(|_| invalid_option(&diagnostic, raw, "TLS port 1..65535", false))?;
                if port == 0 {
                    return Err(invalid_option(&diagnostic, raw, "TLS port 1..65535", false));
                }
                set_once(&mut draft.tls_port, section, key, raw, port)?;
            }
            GeneralOption::TlsCombinedPem => {
                let path = parse_path(&diagnostic, raw, true)?;
                set_once(&mut draft.combined_pem, section, key, "<redacted>", path)?;
            }
            GeneralOption::TlsCertificate => {
                let path = parse_path(&diagnostic, raw, false)?;
                set_once(&mut draft.tls_certificate, section, key, raw, path)?;
            }
            GeneralOption::TlsPrivateKey => {
                let path = parse_path(&diagnostic, raw, true)?;
                set_once(&mut draft.tls_private_key, section, key, "<redacted>", path)?;
            }
            GeneralOption::TlsTrustStore => {
                let path = parse_path(&diagnostic, raw, true)?;
                set_once(&mut draft.tls_trust_store, section, key, "<redacted>", path)?;
            }
            GeneralOption::Deny | GeneralOption::Permit => {
                apply_acl_entry(
                    draft.acl_rules.get_or_insert_default(),
                    if matches!(entry.key, GeneralOption::Permit) {
                        AclAction::Permit
                    } else {
                        AclAction::Deny
                    },
                    &diagnostic,
                    raw,
                )?;
            }
            GeneralOption::LocalNetwork => {
                let local_networks = draft.local_networks.get_or_insert_default();
                if raw.trim().is_empty() {
                    local_networks.clear();
                } else {
                    local_networks.extend(parse_ip_networks(&diagnostic, raw)?);
                }
            }
            GeneralOption::ExternalAddress => {
                let value = raw.trim();
                let address = if value.is_empty() || value.eq_ignore_ascii_case("none") {
                    None
                } else {
                    let address: IpAddr = parse(&diagnostic, value).map_err(|_| {
                        invalid_option(&diagnostic, raw, "an IPv4/IPv6 address or none", false)
                    })?;
                    if address.is_unspecified() {
                        return Err(invalid_option(
                            &diagnostic,
                            raw,
                            "a non-unspecified IPv4/IPv6 address or none",
                            false,
                        ));
                    }
                    Some(address)
                };
                set_once(&mut draft.external_address, section, key, raw, address)?;
            }
            GeneralOption::ExternalHost => {
                let value = raw.trim();
                let hostname = if value.is_empty() || value.eq_ignore_ascii_case("none") {
                    None
                } else {
                    Some(parse_hostname(&diagnostic, value)?)
                };
                set_once(&mut draft.external_hostname, section, key, raw, hostname)?;
            }
            GeneralOption::ExternalRefresh => {
                let refresh = parse::<u32>(&diagnostic, raw).map_err(|_| {
                    invalid_option(
                        &diagnostic,
                        raw,
                        "external DNS refresh interval 1..86400 seconds",
                        false,
                    )
                })?;
                if !(1..=MAX_EXTERNAL_REFRESH_SECONDS).contains(&refresh) {
                    return Err(invalid_option(
                        &diagnostic,
                        raw,
                        "external DNS refresh interval 1..86400 seconds",
                        false,
                    ));
                }
                set_once(&mut draft.external_refresh, section, key, raw, refresh)?;
            }
            GeneralOption::Nat => {
                let mode = parse_nat_mode(&diagnostic, raw)?;
                set_once(&mut draft.nat, section, key, raw, mode)?;
            }
            GeneralOption::SignalingTos => {
                section_values.claim_alias("signaling_dscp", entry.source)?;
                draft.qos.signaling_dscp = Some(parse_tos_as_dscp(&diagnostic, raw)?);
            }
            GeneralOption::SignalingDscp => {
                section_values.claim_alias("signaling_dscp", entry.source)?;
                draft.qos.signaling_dscp = Some(parse_dscp(&diagnostic, raw)?);
            }
            GeneralOption::SignalingCos => {
                section_values.claim_alias("signaling_cos", entry.source)?;
                draft.qos.signaling_cos = Some(parse_cos(&diagnostic, raw)?);
            }
            GeneralOption::AudioTos => {
                section_values.claim_alias("audio_dscp", entry.source)?;
                draft.qos.audio_dscp = Some(parse_tos_as_dscp(&diagnostic, raw)?);
            }
            GeneralOption::AudioDscp => {
                section_values.claim_alias("audio_dscp", entry.source)?;
                draft.qos.audio_dscp = Some(parse_dscp(&diagnostic, raw)?);
            }
            GeneralOption::AudioCos => {
                section_values.claim_alias("audio_cos", entry.source)?;
                draft.qos.audio_cos = Some(parse_cos(&diagnostic, raw)?);
            }
            GeneralOption::VideoTos => {
                section_values.claim_alias("video_dscp", entry.source)?;
                draft.qos.video_dscp = Some(parse_tos_as_dscp(&diagnostic, raw)?);
            }
            GeneralOption::VideoDscp => {
                section_values.claim_alias("video_dscp", entry.source)?;
                draft.qos.video_dscp = Some(parse_dscp(&diagnostic, raw)?);
            }
            GeneralOption::VideoCos => {
                section_values.claim_alias("video_cos", entry.source)?;
                draft.qos.video_cos = Some(parse_cos(&diagnostic, raw)?);
            }
            GeneralOption::TrustPhoneIp => {
                return Err(invalid_option(
                    &diagnostic,
                    raw,
                    "remove obsolete trustphoneip; peer addresses are always authoritative",
                    false,
                ));
            }
            GeneralOption::ServerName => config.server_name.clone_from(raw),
            GeneralOption::Language => set_once(
                &mut draft.language,
                section,
                key,
                raw,
                parse_metadata_required(&diagnostic, raw, MAX_LANGUAGE_BYTES, false)?,
            )?,
            GeneralOption::AccountCode => set_once(
                &mut draft.account_code,
                section,
                key,
                "<redacted>",
                parse_metadata_optional(&diagnostic, raw, MAX_ACCOUNT_CODE_BYTES, true)?,
            )?,
            GeneralOption::Keepalive => config.keepalive_seconds = parse(&diagnostic, raw)?,
            GeneralOption::SecondaryKeepalive => {
                config.secondary_keepalive_seconds = parse(&diagnostic, raw)?;
            }
            GeneralOption::SignalingServer => {
                config
                    .signaling_servers
                    .push(parse_signaling_server(&diagnostic, raw)?);
            }
            GeneralOption::FirstDigitTimeout => {
                let seconds = parse::<u64>(&diagnostic, raw).map_err(|_| {
                    invalid_option(
                        &diagnostic,
                        raw,
                        "first-digit timeout 1..86400 seconds",
                        false,
                    )
                })?;
                if !(1..=86_400).contains(&seconds) {
                    return Err(invalid_option(
                        &diagnostic,
                        raw,
                        "first-digit timeout 1..86400 seconds",
                        false,
                    ));
                }
                set_once(
                    &mut draft.first_digit_timeout,
                    section,
                    key,
                    raw,
                    seconds * 1_000,
                )?;
            }
            GeneralOption::InterdigitTimeoutMs => {
                let milliseconds = parse::<u64>(&diagnostic, raw).map_err(|_| {
                    invalid_option(
                        &diagnostic,
                        raw,
                        "subsequent-digit timeout 250..86400000 milliseconds",
                        false,
                    )
                })?;
                if !(250..=86_400_000).contains(&milliseconds) {
                    return Err(invalid_option(
                        &diagnostic,
                        raw,
                        "subsequent-digit timeout 250..86400000 milliseconds",
                        false,
                    ));
                }
                set_once(
                    &mut draft.interdigit_timeout,
                    section,
                    key,
                    raw,
                    milliseconds,
                )?;
            }
            GeneralOption::DigitTimeout => {
                let seconds = parse::<u64>(&diagnostic, raw).map_err(|_| {
                    invalid_option(
                        &diagnostic,
                        raw,
                        "subsequent-digit timeout 1..86400 seconds",
                        false,
                    )
                })?;
                if !(1..=86_400).contains(&seconds) {
                    return Err(invalid_option(
                        &diagnostic,
                        raw,
                        "subsequent-digit timeout 1..86400 seconds",
                        false,
                    ));
                }
                set_once(
                    &mut draft.interdigit_timeout,
                    section,
                    key,
                    raw,
                    seconds * 1_000,
                )?;
            }
            GeneralOption::DigitTimeoutChar => {
                set_once(
                    &mut draft.dial_terminator,
                    section,
                    key,
                    raw,
                    parse_dial_terminator(&diagnostic, raw)?,
                )?;
            }
            GeneralOption::RecordDigitTimeoutChar => {
                set_once(
                    &mut draft.record_dial_terminator,
                    section,
                    key,
                    raw,
                    parse_bool(&diagnostic, raw)?,
                )?;
            }
            GeneralOption::SimulateEnbloc => {
                set_once(
                    &mut draft.simulate_enbloc,
                    section,
                    key,
                    raw,
                    parse_bool(&diagnostic, raw)?,
                )?;
            }
            GeneralOption::SpeedDialAwaitFurtherDigits => {
                set_once(
                    &mut draft.speed_dial_await_further_digits,
                    section,
                    key,
                    raw,
                    parse_bool(&diagnostic, raw)?,
                )?;
            }
            GeneralOption::AllowOverlap => {
                set_once(
                    &mut draft.allow_overlap,
                    section,
                    key,
                    raw,
                    parse_bool(&diagnostic, raw)?,
                )?;
            }
            GeneralOption::TransferOnHangup => {
                set_once(
                    &mut draft.transfer_on_hangup,
                    section,
                    key,
                    raw,
                    parse_bool(&diagnostic, raw)?,
                )?;
            }
            GeneralOption::CallAnswerOrder => {
                set_once(
                    &mut draft.call_answer_order,
                    section,
                    key,
                    raw,
                    parse_call_answer_order(&diagnostic, raw)?,
                )?;
            }
            GeneralOption::RingType => {
                set_once(
                    &mut draft.ring_type,
                    section,
                    key,
                    raw,
                    parse_ringer_mode(&diagnostic, raw)?,
                )?;
            }
            GeneralOption::CallWaitingTone => {
                let tone = if raw.trim() == "0" {
                    None
                } else {
                    Some(parse_tone(&diagnostic, raw)?)
                };
                set_once(&mut draft.call_waiting_tone, section, key, raw, tone)?;
            }
            GeneralOption::CallWaitingInterval => {
                set_once(
                    &mut draft.call_waiting_interval,
                    section,
                    key,
                    raw,
                    raw.trim()
                        .parse::<u32>()
                        .ok()
                        .filter(|seconds| *seconds <= 86_400)
                        .ok_or_else(|| {
                            invalid_option(
                                &diagnostic,
                                raw,
                                "call-waiting interval 0..86400 seconds",
                                false,
                            )
                        })?,
                )?;
            }
            GeneralOption::Fallback => {
                set_once(
                    &mut draft.fallback_decision,
                    section,
                    key,
                    raw,
                    parse_fallback_decision(&diagnostic, raw)?,
                )?;
            }
            GeneralOption::BackoffTime => {
                let seconds = parse::<u32>(&diagnostic, raw).map_err(|_| {
                    invalid_option(
                        &diagnostic,
                        raw,
                        "registration-token backoff of at least 30 seconds",
                        false,
                    )
                })?;
                if seconds < 30 {
                    return Err(invalid_option(
                        &diagnostic,
                        raw,
                        "registration-token backoff of at least 30 seconds",
                        false,
                    ));
                }
                set_once(&mut draft.fallback_backoff, section, key, raw, seconds)?;
            }
            GeneralOption::ServerPriority => {
                let priority = parse::<u8>(&diagnostic, raw).map_err(|_| {
                    invalid_option(&diagnostic, raw, "positive fallback-server priority", false)
                })?;
                if priority == 0 {
                    return Err(invalid_option(
                        &diagnostic,
                        raw,
                        "positive fallback-server priority",
                        false,
                    ));
                }
                set_once(
                    &mut draft.fallback_server_priority,
                    section,
                    key,
                    raw,
                    priority,
                )?;
            }
            GeneralOption::Allow => draft.codec_settings.push((true, raw.as_str())),
            GeneralOption::Disallow => draft.codec_settings.push((false, raw.as_str())),
            GeneralOption::ConferenceEnabled => set_once(
                &mut draft.conference_enabled,
                section,
                key,
                raw,
                parse_bool(&diagnostic, raw)?,
            )?,
            GeneralOption::ConferenceOptions => set_once(
                &mut draft.conference_options,
                section,
                key,
                raw,
                parse_application_options(&diagnostic, raw)?,
            )?,
            GeneralOption::AutoanswerRingTime => set_once(
                &mut draft.auto_answer_ring_time,
                section,
                key,
                raw,
                parse::<u32>(&diagnostic, raw)?,
            )?,
            GeneralOption::AutoanswerTone => set_once(
                &mut draft.auto_answer_tone,
                section,
                key,
                raw,
                parse_tone(&diagnostic, raw)?,
            )?,
            GeneralOption::RemoteHangupTone => {
                let tone = if raw.trim() == "0" {
                    None
                } else {
                    Some(parse_tone(&diagnostic, raw)?)
                };
                set_once(&mut draft.remote_hangup_tone, section, key, raw, tone)?;
            }
            GeneralOption::HotlineEnabled => set_once(
                &mut draft.hotline_enabled,
                section,
                key,
                raw,
                parse_bool(&diagnostic, raw)?,
            )?,
            GeneralOption::HotlineExtension => set_once(
                &mut draft.hotline_extension,
                section,
                key,
                "<redacted>",
                parse_optional_hotline_destination(&diagnostic, raw)?,
            )?,
            GeneralOption::HotlineContext => set_once(
                &mut draft.hotline_context,
                section,
                key,
                raw,
                parse_bounded_setting_allow_empty(&diagnostic, raw, MAX_HOTLINE_FIELD_BYTES)?,
            )?,
            GeneralOption::HotlineLabel => set_once(
                &mut draft.hotline_label,
                section,
                key,
                raw,
                parse_bounded_setting_allow_empty(&diagnostic, raw, MAX_HOTLINE_FIELD_BYTES)?,
            )?,
            GeneralOption::DirectMedia => set_once(
                &mut draft.direct_media,
                section,
                key,
                raw,
                parse_bool(&diagnostic, raw)?,
            )?,
            GeneralOption::EarlyMedia => set_once(
                &mut draft.early_media,
                section,
                key,
                raw,
                parse_early_media(&diagnostic, raw)?,
            )?,
            GeneralOption::AudioEncryption => set_once(
                &mut draft.audio_encryption,
                section,
                key,
                raw,
                parse_media_encryption_policy(&diagnostic, raw)?,
            )?,
            GeneralOption::EchoCancel => set_once(
                &mut draft.echo_cancellation,
                section,
                key,
                raw,
                parse_bool(&diagnostic, raw)?,
            )?,
            GeneralOption::SilenceSuppression => set_once(
                &mut draft.silence_suppression,
                section,
                key,
                raw,
                parse_bool(&diagnostic, raw)?,
            )?,
            GeneralOption::JbEnable => set_once(
                &mut draft.jitter_enabled,
                section,
                key,
                raw,
                parse_bool(&diagnostic, raw)?,
            )?,
            GeneralOption::JbForce => set_once(
                &mut draft.jitter_forced,
                section,
                key,
                raw,
                parse_bool(&diagnostic, raw)?,
            )?,
            GeneralOption::JbLog => set_once(
                &mut draft.jitter_log_frames,
                section,
                key,
                raw,
                parse_bool(&diagnostic, raw)?,
            )?,
            GeneralOption::JbMaxSize => set_once(
                &mut draft.jitter_max_size_ms,
                section,
                key,
                raw,
                parse_positive_jitter_millis(&diagnostic, raw)?,
            )?,
            GeneralOption::JbResyncThreshold => set_once(
                &mut draft.jitter_resync_threshold_ms,
                section,
                key,
                raw,
                parse_positive_jitter_millis(&diagnostic, raw)?,
            )?,
            GeneralOption::JbImplementation => set_once(
                &mut draft.jitter_implementation,
                section,
                key,
                raw,
                parse_jitter_buffer_implementation(&diagnostic, raw)?,
            )?,
            GeneralOption::RegistrationContext => {
                set_once(
                    &mut draft.registration_contexts,
                    section,
                    key,
                    raw,
                    parse_registration_contexts(&diagnostic, raw)?,
                )?;
            }
            GeneralOption::DeviceTable => set_once(
                &mut draft.device_table,
                section,
                key,
                raw,
                parse_realtime_family(&diagnostic, raw)?,
            )?,
            GeneralOption::LineTable => set_once(
                &mut draft.line_table,
                section,
                key,
                raw,
                parse_realtime_family(&diagnostic, raw)?,
            )?,
        }
    }
    if let Some(enabled) = draft.conference_enabled {
        config.conference_dialing.enabled = enabled;
    }
    if let Some(order) = draft.call_answer_order {
        config.call_answer_order = order;
    }
    if let Some(offset) = draft.timezone_offset_minutes {
        config.timezone_offset_minutes = offset;
    }
    if let Some(template) = draft.date_template {
        config.date_template = template;
    }
    if let Some(mode) = draft.ring_type {
        config.ring_type = mode;
    }
    if let Some(tone) = draft.call_waiting_tone {
        config.call_waiting_tone = tone;
    }
    if let Some(seconds) = draft.call_waiting_interval {
        config.call_waiting_interval_seconds = seconds;
    }
    if let Some(timeout_ms) = draft.first_digit_timeout {
        config.first_digit_timeout_ms = timeout_ms;
    }
    if let Some(timeout_ms) = draft.interdigit_timeout {
        config.interdigit_timeout_ms = timeout_ms;
    }
    if let Some(character) = draft.dial_terminator {
        config.dial_terminator.character = character;
    }
    if let Some(record) = draft.record_dial_terminator {
        config.dial_terminator.record = record;
    }
    if let Some(enabled) = draft.simulate_enbloc {
        config.simulate_enbloc = enabled;
    }
    if let Some(enabled) = draft.speed_dial_await_further_digits {
        config.speed_dial_await_further_digits = enabled;
    }
    if let Some(enabled) = draft.allow_overlap {
        config.allow_overlap = enabled;
    }
    if let Some(enabled) = draft.transfer_on_hangup {
        config.transfer_on_hangup = enabled;
    }
    if let Some(decision) = draft.fallback_decision {
        config.fallback_registration.decision = decision;
    }
    if let Some(seconds) = draft.fallback_backoff {
        config.fallback_registration.backoff_seconds = seconds;
    }
    if let Some(priority) = draft.fallback_server_priority {
        config.fallback_registration.server_priority = priority;
    }
    if let Some(options) = draft.conference_options {
        config.conference_dialing.application_options = options;
    }
    if let Some(contexts) = draft.registration_contexts {
        config.registration.contexts = contexts;
    }
    if !draft.codec_settings.is_empty() {
        config.codecs = apply_codec_settings(Vec::new(), &draft.codec_settings, "general.codecs")?;
    }
    if let Some(ring_time_seconds) = draft.auto_answer_ring_time {
        config.auto_answer.ring_time_seconds = ring_time_seconds;
    }
    if let Some(tone) = draft.auto_answer_tone {
        config.auto_answer.tone = tone;
    }
    if let Some(tone) = draft.remote_hangup_tone {
        config.remote_hangup_tone = tone;
    }
    if let Some(enabled) = draft.hotline_enabled {
        config.guest_hotline.enabled = enabled;
    }
    if let Some(extension) = draft.hotline_extension {
        config.guest_hotline.extension = extension;
    }
    if let Some(context) = draft.hotline_context {
        config.guest_hotline.context = context;
    }
    if let Some(label) = draft.hotline_label {
        config.guest_hotline.label = label;
    }
    if config.guest_hotline.enabled
        && (config.guest_hotline.extension.is_none()
            || config.guest_hotline.context.is_empty()
            || config.guest_hotline.label.is_empty())
    {
        return Err(ConfigError::InvalidValue {
            key: "general.hotline_enabled".into(),
            value: "enabled guest hotline requires extension, context, and label".into(),
        });
    }
    if draft.clear_bind.is_some() && (draft.clear_address.is_some() || draft.clear_port.is_some()) {
        return Err(invalid_option(
            section.section_location(),
            "clear listener aliases",
            "either bind/clear_bind or bindaddr+port, not both",
            false,
        ));
    }
    let clear = draft.clear_bind.unwrap_or_else(|| {
        SocketAddr::new(
            draft.clear_address.unwrap_or(config.listeners.clear.ip()),
            draft.clear_port.unwrap_or(config.listeners.clear.port()),
        )
    });
    if clear.port() == 0 {
        return Err(invalid_option(
            section.section_location(),
            &clear.to_string(),
            "clear listener port 1..65535",
            false,
        ));
    }
    config.bind = clear;
    config.listeners.clear = clear;

    if let Some(ipv4) = draft.advertised_ipv4 {
        config.network.advertised.ipv4 = ipv4;
        if let Some(ipv4) = ipv4 {
            config.advertised_address = ipv4;
        }
    }
    if let Some(ipv6) = draft.advertised_ipv6 {
        config.network.advertised.ipv6 = ipv6;
    }
    if config.network.advertised.ipv4.is_none() && config.network.advertised.ipv6.is_none() {
        return Err(invalid_option(
            section.section_location(),
            "none",
            "at least one advertised IPv4 or IPv6 address",
            false,
        ));
    }

    if let Some(rules) = draft.acl_rules {
        config.network.acl.rules = rules;
    }
    if let Some(local_networks) = draft.local_networks {
        config.network.local_networks = local_networks;
    }
    config.network.nat = draft.nat.unwrap_or(NatMode::Auto);

    let external_address = draft.external_address.take().flatten();
    let external_hostname = draft.external_hostname.take().flatten();
    if external_address.is_some() && external_hostname.is_some() {
        return Err(invalid_option(
            section.section_location(),
            "externip + externhost",
            "exactly one external address source: externip or externhost",
            false,
        ));
    }
    if draft.external_refresh.is_some() && external_hostname.is_none() {
        return Err(invalid_option(
            section.section_location(),
            "externrefresh without externhost",
            "externrefresh only together with externhost",
            false,
        ));
    }
    config.network.external = if let Some(address) = external_address {
        Some(ExternalAddress::Address(address))
    } else {
        external_hostname.map(|name| ExternalAddress::Hostname {
            name,
            refresh_seconds: draft.external_refresh.unwrap_or(60),
        })
    };

    if draft.tls_bind.is_some() && (draft.tls_address.is_some() || draft.tls_port.is_some()) {
        return Err(invalid_option(
            section.section_location(),
            "TLS listener aliases",
            "either tls_bind or secbindaddr+secport, not both",
            false,
        ));
    }
    let split_credentials_requested = draft.tls_certificate.is_some()
        || draft.tls_private_key.is_some()
        || draft.tls_trust_store.is_some();
    if draft.combined_pem.is_some() && split_credentials_requested {
        return Err(invalid_option(
            section.section_location(),
            "<redacted TLS credentials>",
            "either certfile/combined PEM or split certificate+private key+optional trust store",
            true,
        ));
    }
    let tls_requested = draft.tls_bind.is_some()
        || draft.tls_address.is_some()
        || draft.tls_port.is_some()
        || draft.combined_pem.is_some()
        || split_credentials_requested;
    config.listeners.tls = if tls_requested {
        let credentials = if let Some(path) = draft.combined_pem {
            TlsCredentials::CombinedPem(path)
        } else {
            let certificate = draft.tls_certificate.ok_or_else(|| {
                invalid_option(
                    section.section_location(),
                    "<redacted TLS credentials>",
                    "tls_certificate together with tls_private_key",
                    true,
                )
            })?;
            let private_key = draft.tls_private_key.ok_or_else(|| {
                invalid_option(
                    section.section_location(),
                    "<redacted TLS credentials>",
                    "tls_private_key together with tls_certificate",
                    true,
                )
            })?;
            TlsCredentials::SplitPem {
                certificate,
                private_key,
                trust_store: draft.tls_trust_store,
            }
        };
        let bind = draft.tls_bind.unwrap_or_else(|| {
            SocketAddr::new(
                draft
                    .tls_address
                    .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
                draft.tls_port.unwrap_or(2443),
            )
        });
        if bind.port() == 0 {
            return Err(invalid_option(
                section.section_location(),
                &bind.to_string(),
                "TLS listener port 1..65535",
                false,
            ));
        }
        if bind == clear {
            return Err(invalid_option(
                section.section_location(),
                &bind.to_string(),
                "distinct clear and TLS listener socket addresses",
                false,
            ));
        }
        Some(TlsListener { bind, credentials })
    } else {
        None
    };
    config.qos = draft.qos.resolve(config.qos);
    if let Some(language) = draft.language {
        config.language = language;
    }
    if let Some(account_code) = draft.account_code {
        config.account_code = account_code;
    }
    config.direct_media = draft.direct_media.unwrap_or(false);
    config.early_media = draft.early_media.unwrap_or(true);
    config.audio_encryption = draft.audio_encryption.unwrap_or_default();
    config.audio_processing = AudioProcessingPolicy {
        echo_cancellation: if draft.echo_cancellation.unwrap_or(true) {
            EchoCancellation::On
        } else {
            EchoCancellation::Off
        },
        silence_suppression: if draft.silence_suppression.unwrap_or(false) {
            SilenceSuppression::On
        } else {
            SilenceSuppression::Off
        },
    };
    config.jitter_buffer = JitterBufferConfig {
        enabled: draft.jitter_enabled.unwrap_or(false),
        forced: draft.jitter_forced.unwrap_or(false),
        log_frames: draft.jitter_log_frames.unwrap_or(false),
        max_size_ms: draft.jitter_max_size_ms.unwrap_or(200),
        resync_threshold_ms: draft.jitter_resync_threshold_ms.unwrap_or(1_000),
        implementation: draft.jitter_implementation.unwrap_or_default(),
    };
    config.realtime_tables = match (draft.device_table, draft.line_table) {
        (None, None) => None,
        (Some(device_family), Some(line_family)) if device_family != line_family => {
            Some(RealtimeTableConfig {
                device_family,
                line_family,
            })
        }
        (Some(device_family), Some(_line_family)) => {
            return Err(invalid_option(
                section.section_location(),
                &device_family,
                "different devicetable and linetable family names",
                false,
            ));
        }
        (Some(_), None) => {
            return Err(invalid_option(
                section.diagnostic_key("devicetable"),
                "devicetable without linetable",
                "devicetable and linetable together",
                false,
            ));
        }
        (None, Some(_)) => {
            return Err(invalid_option(
                section.diagnostic_key("linetable"),
                "linetable without devicetable",
                "devicetable and linetable together",
                false,
            ));
        }
    };
    Ok(())
}

fn parse_realtime_family(key: &str, raw: &str) -> Result<String, ConfigError> {
    let family = raw.trim();
    if family.is_empty()
        || family.len() > MAX_REALTIME_FAMILY_BYTES
        || !family
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(invalid_option(
            key,
            raw,
            "a non-empty realtime family name up to 45 bytes using letters, digits, or underscore",
            false,
        ));
    }
    Ok(family.into())
}

fn parse<T: FromStr>(key: &str, raw: &str) -> Result<T, ConfigError> {
    raw.parse()
        .map_err(|_| invalid_option(key, raw, std::any::type_name::<T>(), false))
}

fn set_once<T>(
    setting: &mut Option<T>,
    section: &RawSection,
    key: &str,
    raw: &str,
    value: T,
) -> Result<(), ConfigError> {
    SectionValues::new(section).set_once(setting, key, raw, value)
}

fn parse_required_setting(key: &str, raw: &str) -> Result<String, ConfigError> {
    let value = raw.trim();
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err(invalid_option(
            key,
            raw,
            "a nonempty printable value",
            false,
        ));
    }
    Ok(value.into())
}

fn parse_metadata_required(
    key: &str,
    raw: &str,
    max_bytes: usize,
    sensitive: bool,
) -> Result<String, ConfigError> {
    let value = raw.trim();
    if value.is_empty()
        || value.len() > max_bytes
        || value
            .chars()
            .any(|character| character == '\0' || character.is_control())
    {
        return Err(invalid_option(
            key,
            raw,
            &format!("a nonempty printable value of at most {max_bytes} bytes"),
            sensitive,
        ));
    }
    Ok(value.into())
}

fn parse_metadata_optional(
    key: &str,
    raw: &str,
    max_bytes: usize,
    sensitive: bool,
) -> Result<Option<String>, ConfigError> {
    if raw.trim().is_empty() {
        return Ok(None);
    }
    parse_metadata_required(key, raw, max_bytes, sensitive).map(Some)
}

fn push_channel_variable(
    variables: &mut Vec<ChannelVariable>,
    key: &str,
    raw: &str,
) -> Result<(), ConfigError> {
    let invalid = || {
        invalid_option(
            key,
            raw,
            "a unique, nonsensitive NAME=value assignment within channel-variable bounds",
            true,
        )
    };
    let (name, value) = raw.split_once('=').ok_or_else(invalid)?;
    let name = name.trim();
    let value = value.trim();
    if value.is_empty() {
        return Err(invalid());
    }
    let variable = ChannelVariable::new(name, value).map_err(|_| invalid())?;
    if variables.len() >= MAX_VARIABLES
        || variables
            .iter()
            .any(|configured| configured.name() == variable.name())
    {
        return Err(invalid());
    }
    let aggregate = variables
        .iter()
        .map(|configured| configured.name().len() + configured.value().len())
        .sum::<usize>()
        .checked_add(variable.name().len() + variable.value().len())
        .ok_or_else(invalid)?;
    if aggregate > MAX_VARIABLE_AGGREGATE_BYTES {
        return Err(invalid());
    }
    variables.push(variable);
    Ok(())
}

fn parse_optional_setting(key: &str, raw: &str) -> Result<Option<String>, ConfigError> {
    let value = raw.trim();
    if value.is_empty()
        || matches!(
            value.to_ascii_lowercase().as_str(),
            "none" | "off" | "disabled"
        )
    {
        return Ok(None);
    }
    parse_required_setting(key, value).map(Some)
}

fn parse_optional_voicemail_destination(
    key: &str,
    raw: &str,
) -> Result<Option<VoicemailDestination>, ConfigError> {
    let value = raw.trim();
    if value.is_empty()
        || matches!(
            value.to_ascii_lowercase().as_str(),
            "none" | "off" | "disabled"
        )
    {
        return Ok(None);
    }
    VoicemailDestination::new(value)
        .map(Some)
        .map_err(|_| invalid_option(key, "<redacted>", "a bounded printable destination", true))
}

fn parse_optional_forwarding_destination(
    key: &str,
    raw: &str,
) -> Result<Option<ForwardingDestination>, ConfigError> {
    let value = raw.trim();
    if value.is_empty()
        || matches!(
            value.to_ascii_lowercase().as_str(),
            "none" | "off" | "disabled"
        )
    {
        return Ok(None);
    }
    ForwardingDestination::new(value)
        .map(Some)
        .map_err(|_| invalid_option(key, "<redacted>", "a bounded printable destination", true))
}

fn parse_optional_hotline_destination(
    key: &str,
    raw: &str,
) -> Result<Option<HotlineDestination>, ConfigError> {
    let value = raw.trim();
    if value.is_empty() {
        return Ok(None);
    }
    HotlineDestination::new(value)
        .map(Some)
        .map_err(|_| invalid_option(key, "<redacted>", "a bounded printable destination", true))
}

fn parse_empty_optional_setting(key: &str, raw: &str) -> Result<Option<String>, ConfigError> {
    let value = raw.trim();
    if value.is_empty() {
        return Ok(None);
    }
    parse_required_setting(key, value).map(Some)
}

fn parse_setting_allow_empty(key: &str, raw: &str) -> Result<String, ConfigError> {
    let value = raw.trim();
    if value.chars().any(char::is_control) {
        return Err(invalid_option(key, raw, "a printable value", false));
    }
    Ok(value.into())
}

fn parse_bounded_setting_allow_empty(
    key: &str,
    raw: &str,
    max_bytes: usize,
) -> Result<String, ConfigError> {
    let value = parse_setting_allow_empty(key, raw)?;
    if value.len() > max_bytes {
        return Err(invalid_option(
            key,
            raw,
            &format!("at most {max_bytes} bytes"),
            false,
        ));
    }
    Ok(value)
}

fn parse_application_options(key: &str, raw: &str) -> Result<String, ConfigError> {
    let value = raw.trim();
    if value.chars().any(char::is_control) {
        return Err(invalid_option(
            key,
            raw,
            "printable application options",
            false,
        ));
    }
    Ok(value.into())
}

fn parse_parking_lot_button(
    key: &str,
    raw: Option<&str>,
) -> Result<ParkingLotButtonConfig, ConfigError> {
    let fields: Vec<_> = raw
        .unwrap_or("default,RetrieveSingle")
        .split(',')
        .map(str::trim)
        .collect();
    if !(1..=2).contains(&fields.len()) || fields[0].is_empty() {
        return Err(ConfigError::InvalidValue {
            key: key.into(),
            value: raw.unwrap_or_default().into(),
        });
    }
    let retrieval = if fields.len() == 1 {
        ParkingRetrievalBehavior::RetrieveSingle
    } else {
        match normalize_name(fields[1]).as_str() {
            "retrievesingle" => ParkingRetrievalBehavior::RetrieveSingle,
            "alwaysshowmenu" => ParkingRetrievalBehavior::AlwaysShowMenu,
            _ => {
                return Err(ConfigError::InvalidValue {
                    key: key.into(),
                    value: raw.unwrap_or_default().into(),
                });
            }
        }
    };
    Ok(ParkingLotButtonConfig {
        lot: parse_required_setting(key, fields[0])?,
        retrieval,
    })
}

fn parse_mailbox(key: &str, raw: &str) -> Result<Option<String>, ConfigError> {
    let Some(mailbox) = parse_optional_setting(key, raw)? else {
        return Ok(None);
    };
    let mut parts = mailbox.split('@');
    let name = parts.next().unwrap_or_default();
    let context = parts.next();
    if name.is_empty()
        || name.chars().any(char::is_whitespace)
        || context
            .is_some_and(|context| context.is_empty() || context.chars().any(char::is_whitespace))
        || parts.next().is_some()
    {
        return Err(invalid_option(
            key,
            raw,
            "mailbox or mailbox@context without whitespace",
            false,
        ));
    }
    Ok(Some(mailbox))
}

fn parse_mobility_pin(key: &str, raw: &str) -> Result<Option<MobilityPin>, ConfigError> {
    let value = raw.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if value.len() > MAX_MOBILITY_PIN_DIGITS || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid_option(
            key,
            raw,
            "one to seven ASCII digits, or empty to disable mobility login",
            true,
        ));
    }
    Ok(Some(MobilityPin(value.into())))
}

fn validate_registration_identifier(
    key: &str,
    value: &str,
    expected: &str,
) -> Result<(), ConfigError> {
    if value.is_empty()
        || value.len() > MAX_REGISTRATION_IDENTIFIER_BYTES
        || value
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
        || value.contains(['&', '@'])
    {
        return Err(invalid_option(key, value, expected, false));
    }
    Ok(())
}

fn parse_registration_contexts(key: &str, raw: &str) -> Result<Vec<String>, ConfigError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    if raw.len() > MAX_REGISTRATION_IDENTIFIER_BYTES {
        return Err(invalid_option(
            key,
            raw,
            "an ampersand-separated context list totaling at most 79 bytes",
            false,
        ));
    }
    let mut contexts = Vec::new();
    let mut seen = HashSet::new();
    for field in raw.split('&') {
        let context = field.trim();
        validate_registration_identifier(
            key,
            context,
            "unique, nonempty context names without whitespace, ampersands, or @",
        )?;
        if !seen.insert(context.to_owned()) {
            return Err(invalid_option(
                key,
                raw,
                "unique ampersand-separated context names",
                false,
            ));
        }
        contexts.push(context.to_owned());
    }
    Ok(contexts)
}

fn parse_registration_extensions(
    key: &str,
    raw: &str,
) -> Result<Option<Vec<RegistrationExtension>>, ConfigError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    if raw.len() > MAX_REGISTRATION_EXTENSION_LIST_BYTES {
        return Err(invalid_option(
            key,
            raw,
            "an ampersand-separated extension list totaling at most 255 bytes",
            false,
        ));
    }
    let mut extensions = Vec::new();
    let mut seen = HashSet::new();
    for field in raw.split('&') {
        let field = field.trim();
        let (extension, context) = if let Some((extension, context)) = field.split_once('@') {
            if context.contains('@') {
                return Err(invalid_option(
                    key,
                    raw,
                    "extension or extension@context entries separated by ampersands",
                    false,
                ));
            }
            (extension.trim(), Some(context.trim()))
        } else {
            (field, None)
        };
        validate_registration_identifier(
            key,
            extension,
            "a nonempty registration extension up to 79 bytes without whitespace, ampersands, or @",
        )?;
        if let Some(context) = context {
            validate_registration_identifier(
                key,
                context,
                "a nonempty registration context up to 79 bytes without whitespace, ampersands, or @",
            )?;
        }
        let entry = RegistrationExtension {
            extension: extension.into(),
            context: context.map(str::to_owned),
        };
        if !seen.insert(entry.clone()) {
            return Err(invalid_option(
                key,
                raw,
                "unique extension or extension@context entries",
                false,
            ));
        }
        extensions.push(entry);
    }
    Ok(Some(extensions))
}

fn resolve_registration_targets(
    contexts: &[String],
    extensions: &[RegistrationExtension],
) -> Vec<RegistrationTarget> {
    extensions
        .iter()
        .flat_map(|entry| {
            if let Some(context) = &entry.context {
                vec![RegistrationTarget {
                    extension: entry.extension.clone(),
                    context: context.clone(),
                }]
            } else {
                contexts
                    .iter()
                    .map(|context| RegistrationTarget {
                        extension: entry.extension.clone(),
                        context: context.clone(),
                    })
                    .collect()
            }
        })
        .collect()
}

fn parse_dnd_mode(key: &str, raw: &str) -> Result<DndMode, ConfigError> {
    match normalize_name(raw).as_str() {
        "off" | "none" | "disabled" => Ok(DndMode::Off),
        "silent" => Ok(DndMode::Silent),
        "reject" | "busy" => Ok(DndMode::Reject),
        _ => Err(invalid_option(key, raw, "off, silent, or reject", false)),
    }
}

fn parse_dnd_button_mode(key: &str, raw: &str) -> Result<DndButtonMode, ConfigError> {
    match normalize_name(raw).as_str() {
        "silent" => Ok(DndButtonMode::Silent),
        "reject" | "busy" => Ok(DndButtonMode::Reject),
        _ => Err(invalid_option(key, raw, "silent or reject", false)),
    }
}

fn parse_feature_default(key: &str, raw: &str) -> Result<(u32, bool), ConfigError> {
    let fields: Vec<_> = raw.split(',').map(str::trim).collect();
    if fields.len() != 2 {
        return Err(invalid_option(
            key,
            raw,
            "feature instance and boolean: instance,yes|no",
            false,
        ));
    }
    let instance = parse::<u32>(key, fields[0])?;
    if instance == 0 {
        return Err(invalid_option(
            key,
            raw,
            "feature instance >= 1 and boolean: instance,yes|no",
            false,
        ));
    }
    Ok((instance, parse_bool(key, fields[1])?))
}

fn parse_numeric_groups(key: &str, raw: &str) -> Result<BTreeSet<u8>, ConfigError> {
    let mut groups = BTreeSet::new();
    if raw.trim().is_empty() {
        return Ok(groups);
    }
    for field in raw.split(',') {
        let field = field.trim();
        if field.is_empty() {
            return Err(invalid_option(
                key,
                raw,
                "comma-separated groups or ranges in 0..63",
                false,
            ));
        }
        let (start, end) = if let Some((start, end)) = field.split_once('-') {
            if end.contains('-') {
                return Err(invalid_option(
                    key,
                    raw,
                    "comma-separated groups or ranges in 0..63",
                    false,
                ));
            }
            (
                parse::<u8>(key, start.trim())?,
                parse::<u8>(key, end.trim())?,
            )
        } else {
            let value = parse::<u8>(key, field)?;
            (value, value)
        };
        if start > end || end > 63 {
            return Err(invalid_option(
                key,
                raw,
                "ascending group values or ranges in 0..63",
                false,
            ));
        }
        for group in start..=end {
            if !groups.insert(group) {
                return Err(invalid_option(
                    key,
                    raw,
                    "unique group values in 0..63",
                    false,
                ));
            }
        }
    }
    Ok(groups)
}

fn parse_named_groups(key: &str, raw: &str) -> Result<BTreeSet<String>, ConfigError> {
    let mut groups = BTreeSet::new();
    if raw.trim().is_empty() {
        return Ok(groups);
    }
    for field in raw.split(',') {
        let group = field.trim();
        if group.is_empty()
            || group.chars().any(char::is_control)
            || !groups.insert(group.to_owned())
        {
            return Err(invalid_option(
                key,
                raw,
                "unique, nonempty named groups",
                false,
            ));
        }
    }
    Ok(groups)
}

fn parse_bool(key: &str, raw: &str) -> Result<bool, ConfigError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Ok(true),
        "false" | "no" | "off" | "0" => Ok(false),
        _ => Err(invalid_option(key, raw, "yes or no", false)),
    }
}

fn parse_dial_terminator(key: &str, raw: &str) -> Result<char, ConfigError> {
    let value = raw.trim();
    let mut characters = value.chars();
    let Some(character) = characters.next() else {
        return Err(invalid_option(key, raw, "one DTMF character", false));
    };
    if characters.next().is_some() {
        return Err(invalid_option(key, raw, "one DTMF character", false));
    }
    let character = character.to_ascii_uppercase();
    if !matches!(character, '0'..='9' | '*' | '#' | 'A'..='D') {
        return Err(invalid_option(
            key,
            raw,
            "one DTMF character: 0..9, *, #, or A..D",
            false,
        ));
    }
    Ok(character)
}

fn parse_secondary_dialtone_digits(key: &str, raw: &str) -> Result<Option<String>, ConfigError> {
    let value = raw.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if value.len() > 9
        || !value
            .chars()
            .all(|character| matches!(character, '0'..='9' | '*' | '#' | 'A'..='D' | 'a'..='d'))
    {
        return Err(invalid_option(
            key,
            raw,
            "up to 9 DTMF characters: 0..9, *, #, or A..D",
            false,
        ));
    }
    Ok(Some(value.to_ascii_uppercase()))
}

fn parse_call_answer_order(key: &str, raw: &str) -> Result<CallAnswerOrder, ConfigError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "oldestfirst" => Ok(CallAnswerOrder::OldestFirst),
        "lastfirst" => Ok(CallAnswerOrder::LastFirst),
        _ => Err(invalid_option(key, raw, "OldestFirst or LastFirst", false)),
    }
}

fn parse_fallback_decision(key: &str, raw: &str) -> Result<FallbackDecision, ConfigError> {
    let value = raw.trim();
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Ok(FallbackDecision::Accept),
        "false" | "no" | "off" | "0" => Ok(FallbackDecision::Reject),
        "odd" => Ok(FallbackDecision::DeviceIdOdd),
        "even" => Ok(FallbackDecision::DeviceIdEven),
        _ => Err(invalid_option(
            key,
            raw,
            "yes, no, odd, or even",
            value.contains('/') || value.contains('\\'),
        )),
    }
}

fn parse_signaling_server(key: &str, raw: &str) -> Result<SignalingServerRoute, ConfigError> {
    let fields = raw.split(',').map(str::trim).collect::<Vec<_>>();
    let invalid = || {
        invalid_option(
            key,
            raw,
            "priority,name,address,clear-port-or-none,secure-port-or-none",
            false,
        )
    };
    let [priority, name, address, clear_port, secure_port] = fields.as_slice() else {
        return Err(invalid());
    };
    let priority = priority
        .parse::<u8>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(invalid)?;
    if name.is_empty() || name.len() >= 48 || name.chars().any(char::is_control) {
        return Err(invalid());
    }
    let address = address
        .parse::<IpAddr>()
        .ok()
        .filter(|address| !address.is_unspecified() && !address.is_multicast())
        .ok_or_else(invalid)?;
    let port = |value: &str| {
        if matches!(normalize_name(value).as_str(), "none" | "off" | "disabled") {
            Some(None)
        } else {
            value
                .parse::<u16>()
                .ok()
                .and_then(std::num::NonZeroU16::new)
                .map(Some)
        }
    };
    let clear_port = port(clear_port).ok_or_else(invalid)?;
    let secure_port = port(secure_port).ok_or_else(invalid)?;
    if clear_port.is_none() && secure_port.is_none() {
        return Err(invalid());
    }
    Ok(SignalingServerRoute {
        priority,
        name: (*name).into(),
        address,
        clear_port,
        secure_port,
    })
}

fn parse_early_media(key: &str, raw: &str) -> Result<bool, ConfigError> {
    match normalize_name(raw).as_str() {
        "yes" | "true" | "on" | "1" => Ok(true),
        "no" | "false" | "off" | "0" | "none" => Ok(false),
        // Accepted compatibility values all map to enabled early media.
        "offhook" | "immediate" | "dial" | "ringout" | "progress" => Ok(true),
        _ => Err(invalid_option(
            key,
            raw,
            "yes, no, none, offhook, immediate, dial, ringout, or progress",
            false,
        )),
    }
}

fn parse_dtmf_mode(key: &str, raw: &str) -> Result<DtmfMode, ConfigError> {
    match normalize_name(raw).as_str() {
        "auto" => Ok(DtmfMode::Auto),
        "rfc2833" => Ok(DtmfMode::Rfc2833),
        "skinny" => Ok(DtmfMode::Skinny),
        _ => Err(invalid_option(key, raw, "auto, rfc2833, or skinny", false)),
    }
}

fn parse_video_mode(key: &str, raw: &str) -> Result<VideoMode, ConfigError> {
    match normalize_name(raw).as_str() {
        "off" => Ok(VideoMode::Off),
        "user" => Ok(VideoMode::User),
        "auto" => Ok(VideoMode::Auto),
        _ => Err(invalid_option(key, raw, "off, user, or auto", false)),
    }
}

fn parse_media_encryption_policy(
    key: &str,
    raw: &str,
) -> Result<MediaEncryptionPolicy, ConfigError> {
    let mut fields = raw.split(',').map(str::trim);
    let requirement = match fields.next().map(normalize_name).as_deref() {
        Some("off") => MediaEncryptionRequirement::Off,
        Some("optional") => MediaEncryptionRequirement::Optional,
        Some("required") => MediaEncryptionRequirement::Required,
        _ => {
            return Err(invalid_option(
                key,
                raw,
                "off, optional,<profile...>, or required,<profile...>",
                false,
            ));
        }
    };
    let profiles = fields
        .map(|profile| {
            profile.parse::<MediaEncryptionProfile>().map_err(|_| {
                invalid_option(key, raw, "a canonical media-encryption profile list", false)
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    MediaEncryptionPolicy::new(requirement, profiles).map_err(|_| {
        invalid_option(
            key,
            raw,
            "off without profiles, or optional/required with at least one profile",
            false,
        )
    })
}

fn parse_jitter_buffer_implementation(
    key: &str,
    raw: &str,
) -> Result<JitterBufferImplementation, ConfigError> {
    match normalize_name(raw).as_str() {
        "fixed" => Ok(JitterBufferImplementation::Fixed),
        "adaptive" => Ok(JitterBufferImplementation::Adaptive),
        _ => Err(invalid_option(key, raw, "fixed or adaptive", false)),
    }
}

fn parse_positive_jitter_millis(key: &str, raw: &str) -> Result<u32, ConfigError> {
    let value = raw.trim().parse::<u32>().map_err(|_| {
        invalid_option(
            key,
            raw,
            "a positive millisecond value no greater than 2147483647",
            false,
        )
    })?;
    if value == 0 || value > i32::MAX as u32 {
        return Err(invalid_option(
            key,
            raw,
            "a positive millisecond value no greater than 2147483647",
            false,
        ));
    }
    Ok(value)
}

fn parse_ringer_mode(key: &str, raw: &str) -> Result<RingerMode, ConfigError> {
    match normalize_name(raw).as_str() {
        "off" => Ok(RingerMode::Off),
        "inside" => Ok(RingerMode::Inside),
        "outside" => Ok(RingerMode::Outside),
        "feature" => Ok(RingerMode::Feature),
        "silent" => Ok(RingerMode::Silent),
        "urgent" => Ok(RingerMode::Urgent),
        "bellcore1" => Ok(RingerMode::Bellcore1),
        "bellcore2" => Ok(RingerMode::Bellcore2),
        "bellcore3" => Ok(RingerMode::Bellcore3),
        "bellcore4" => Ok(RingerMode::Bellcore4),
        "bellcore5" => Ok(RingerMode::Bellcore5),
        _ => Err(invalid_option(
            key,
            raw,
            "Off, Inside, Outside, Feature, Silent, Urgent, or Bellcore1..Bellcore5",
            false,
        )),
    }
}

fn parse_tone(key: &str, raw: &str) -> Result<Tone, ConfigError> {
    let trimmed = raw.trim();
    let numeric = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .map(|hex| u32::from_str_radix(hex, 16))
        .unwrap_or_else(|| trimmed.parse::<u32>());
    if let Ok(value) = numeric {
        if value <= u8::MAX.into() {
            return Ok(Tone::from(value));
        }
        return Err(ConfigError::InvalidValue {
            key: key.into(),
            value: raw.into(),
        });
    }

    let tone = match normalize_name(trimmed).as_str() {
        "silence" => Tone::Silence,
        "dtmf1" => Tone::Dtmf1,
        "dtmf2" => Tone::Dtmf2,
        "dtmf3" => Tone::Dtmf3,
        "dtmf4" => Tone::Dtmf4,
        "dtmf5" => Tone::Dtmf5,
        "dtmf6" => Tone::Dtmf6,
        "dtmf7" => Tone::Dtmf7,
        "dtmf8" => Tone::Dtmf8,
        "dtmf9" => Tone::Dtmf9,
        "dtmf0" => Tone::Dtmf0,
        "dtmfstar" => Tone::DtmfStar,
        "dtmfpound" => Tone::DtmfPound,
        "dtmfa" => Tone::DtmfA,
        "dtmfb" => Tone::DtmfB,
        "dtmfc" => Tone::DtmfC,
        "dtmfd" => Tone::DtmfD,
        "insidedial" | "insidedialtone" => Tone::InsideDial,
        "outsidedial" | "outsidedialtone" => Tone::OutsideDial,
        "linebusy" | "linebusytone" => Tone::LineBusy,
        "alerting" | "alertingtone" => Tone::Alerting,
        "reorder" | "reordertone" => Tone::Reorder,
        "recorderwarning" | "recorderwarningtone" => Tone::RecorderWarning,
        "recorderdetected" | "recorderdetectedtone" => Tone::RecorderDetected,
        "reverting" | "revertingtone" => Tone::Reverting,
        "receiveroffhook" | "receiveroffhooktone" => Tone::ReceiverOffHook,
        "partialdial" | "partialdialtone" => Tone::PartialDial,
        "nosuchnumber" | "nosuchnumbertone" => Tone::NoSuchNumber,
        "busyverification" | "busyverificationtone" => Tone::BusyVerification,
        "callwaiting" | "callwaitingtone" => Tone::CallWaiting,
        "confirmation" | "confirmationtone" => Tone::Confirmation,
        "campon" | "camponindicationtone" => Tone::CampOn,
        "recalldial" | "recalldialtone" => Tone::RecallDial,
        "zipzip" => Tone::ZipZip,
        "zip" => Tone::Zip,
        "beepbonk" => Tone::BeepBonk,
        "music" | "musictone" => Tone::Music,
        "hold" | "holdtone" => Tone::Hold,
        "test" | "testtone" => Tone::Test,
        "monitorwarning" | "dtmonitorwarningtone" => Tone::MonitorWarning,
        "addcallwaiting" => Tone::AddCallWaiting,
        "prioritycallwaiting" | "prioritycallwait" => Tone::PriorityCallWaiting,
        "bargein" | "bargin" => Tone::BargeIn,
        "distinctalert" => Tone::DistinctAlert,
        "priorityalert" => Tone::PriorityAlert,
        "reminderring" => Tone::ReminderRing,
        "precedenceringback" => Tone::PrecedenceRingback,
        "preemption" | "preemptiontone" => Tone::Preemption,
        "notone" => Tone::NoTone,
        "meetmegreeting" | "meetmegreetingtone" => Tone::MeetMeGreeting,
        "meetmenumberinvalid" | "meetmenumberinvalidtone" => Tone::MeetMeNumberInvalid,
        "meetmenumberfailed" | "meetmenumberfailedtone" => Tone::MeetMeNumberFailed,
        "meetmeenterpin" | "meetmeenterpintone" => Tone::MeetMeEnterPin,
        "meetmeinvalidpin" | "meetmeinvalidpintone" => Tone::MeetMeInvalidPin,
        "meetmefailedpin" | "meetmefailedpintone" => Tone::MeetMeFailedPin,
        "meetmecfbfailed" | "meetmecfbfailedtone" => Tone::MeetMeCfbFailed,
        "meetmeenteraccesscode" | "meetmeenteraccesscodetone" => Tone::MeetMeEnterAccessCode,
        "meetmeaccesscodeinvalid" | "meetmeaccesscodeinvalidtone" => Tone::MeetMeAccessCodeInvalid,
        "meetmeaccesscodefailed" | "meetmeaccesscodefailedtone" => Tone::MeetMeAccessCodeFailed,
        _ => {
            return Err(ConfigError::InvalidValue {
                key: key.into(),
                value: raw.into(),
            });
        }
    };
    Ok(tone)
}

fn apply_codec_settings(
    mut codecs: Vec<Codec>,
    settings: &[(bool, &str)],
    key: &str,
) -> Result<Vec<Codec>, ConfigError> {
    for (allow_setting, raw) in settings {
        let tokens = raw.split(',').map(str::trim).collect::<Vec<_>>();
        if tokens.len() > 1 && tokens.iter().any(|token| token.eq_ignore_ascii_case("all")) {
            return Err(ConfigError::InvalidValue {
                key: key.into(),
                value: (*raw).into(),
            });
        }
        for token in tokens {
            let mut token = token.trim();
            if token.is_empty() {
                return Err(ConfigError::InvalidValue {
                    key: key.into(),
                    value: (*raw).into(),
                });
            }
            let mut allow = *allow_setting;
            if let Some(negated) = token.strip_prefix('!') {
                token = negated.trim();
                allow = !allow;
                if token.is_empty() {
                    return Err(ConfigError::InvalidValue {
                        key: key.into(),
                        value: (*raw).into(),
                    });
                }
            }
            if token.eq_ignore_ascii_case("all") && !allow {
                codecs.clear();
                continue;
            }
            let candidates = codec_group(token).ok_or_else(|| ConfigError::InvalidValue {
                key: key.into(),
                value: token.into(),
            })?;
            for codec in candidates {
                codecs.retain(|candidate| candidate != &codec);
                if allow {
                    codecs.push(codec);
                    if codecs.len() > MAX_CODEC_PREFERENCES {
                        return Err(ConfigError::InvalidValue {
                            key: key.into(),
                            value: format!("more than {MAX_CODEC_PREFERENCES} codec preferences"),
                        });
                    }
                }
            }
        }
    }
    if !codecs.iter().any(|codec| codec.kind() == CodecKind::Audio) {
        return Err(ConfigError::InvalidValue {
            key: key.into(),
            value: "at least one audio codec is required".into(),
        });
    }
    if let Some(codec) = codecs
        .iter()
        .copied()
        .find(|codec| matches!(codec.kind(), CodecKind::Audio) && pbx_audio_format(*codec).is_err())
    {
        return Err(ConfigError::InvalidValue {
            key: key.into(),
            value: unsupported_audio_reason(codec)
                .unwrap_or("codec has no Asterisk audio format mapping")
                .into(),
        });
    }
    Ok(codecs)
}

fn mapped_audio_codecs() -> Vec<Codec> {
    vec![
        Codec::Pcmu,
        Codec::G711Ulaw56k,
        Codec::Pcma,
        Codec::G711Alaw56k,
        Codec::G72264k,
        Codec::G72256k,
        Codec::G72248k,
        Codec::G7231,
        Codec::G729,
        Codec::G729A,
        Codec::G729B,
        Codec::G729Ab,
        Codec::G729AnnexB,
        Codec::G726_32k,
        Codec::Gsm,
        Codec::Wideband256k,
        Codec::Ilbc,
        Codec::G7221_32k,
        Codec::Opus,
    ]
}

fn codec_group(raw: &str) -> Option<Vec<Codec>> {
    let codecs = match normalize_name(raw).as_str() {
        "all" => mapped_audio_codecs(),
        "is11172" => vec![Codec::Is11172],
        "is13872" => vec![Codec::Is13818],
        "gsm" => vec![Codec::Gsm],
        "slin16" => vec![Codec::Wideband256k],
        "activevoice" => vec![Codec::ActiveVoice],
        "alaw" => vec![Codec::Pcma, Codec::G711Alaw56k],
        "ulaw" => vec![Codec::Pcmu, Codec::G711Ulaw56k],
        "g722" => vec![Codec::G72264k, Codec::G72256k, Codec::G72248k],
        "g7221" => vec![Codec::G7221_32k],
        "g723" => vec![Codec::G7231],
        "g726" => vec![Codec::G726_32k],
        "g728" => vec![Codec::G728],
        "g729" => vec![
            Codec::G729,
            Codec::G729A,
            Codec::G729B,
            Codec::G729Ab,
            Codec::G729AnnexB,
        ],
        "ilbc" => vec![Codec::Ilbc],
        "isac" => vec![Codec::Isac],
        "opus" => vec![Codec::Opus],
        "h224" => vec![Codec::H224],
        "aac" => vec![Codec::Aac],
        "mp4alatm128" => vec![Codec::Mp4aLatm128],
        "mp4alatm64" => vec![Codec::Mp4aLatm64],
        "mp4alatm56" => vec![Codec::Mp4aLatm56],
        "mp4alatm48" => vec![Codec::Mp4aLatm48],
        "mp4alatm32" => vec![Codec::Mp4aLatm32],
        "mp4alatm24" => vec![Codec::Mp4aLatm24],
        "mp4alatmna" => vec![Codec::Mp4aLatm],
        "amr" => vec![Codec::Amr],
        "amrwb" => vec![Codec::AmrWb],
        "h261" => vec![Codec::H261],
        "h263" => vec![Codec::H263, Codec::H263Plus],
        "h264" => vec![Codec::H264, Codec::H264Svc, Codec::H264Fec, Codec::H264Uc],
        "h265" => vec![Codec::H265],
        "t120" => vec![Codec::T120],
        "data" => vec![Codec::Data64k, Codec::Data56k],
        "t38fax" => vec![Codec::T38Fax],
        "tote" => vec![Codec::Tote],
        "xv711u" => vec![Codec::Xv150ModemRelay711u],
        "v711u" => vec![Codec::NseVbd711u],
        "xv729a" => vec![Codec::Xv150ModemRelay729a],
        "v729a" => vec![Codec::NseVbd729a],
        "clearchan" => vec![Codec::ClearChannel],
        "univxcoder" => vec![Codec::UniversalTranscoder],
        "rfc2833" => vec![Codec::DtmfOutOfBandRfc2833],
        "passthrough" => vec![Codec::DtmfPassthrough],
        "dynamic" => vec![Codec::DtmfDynamic],
        "oob" => vec![Codec::DtmfOutOfBand],
        "rfc2833ib" => vec![Codec::DtmfInBandRfc2833],
        "cfb" => vec![Codec::CfbTones],
        "noaudio" => vec![Codec::DtmfNoAudio],
        "v150modem" => vec![Codec::V150ModemRelay],
        "v150sprt" => vec![Codec::V150Sprt],
        "v150sse" => vec![Codec::V150Sse],
        _ => return None,
    };
    Some(codecs)
}

fn parse_caller_id(raw: &str) -> (String, String) {
    let raw = raw.trim();
    if let Some((name, number)) = raw.rsplit_once('<')
        && let Some(number) = number.strip_suffix('>')
    {
        return (
            name.trim().trim_matches('"').to_owned(),
            number.trim().to_owned(),
        );
    }
    (raw.to_owned(), raw.to_owned())
}

fn value<'a>(section: &'a RawSection, key: &str) -> Option<&'a str> {
    section
        .values
        .iter()
        .rev()
        .find(|value| value.key.eq_ignore_ascii_case(key))
        .map(|value| value.value.as_str())
}

fn strip_inline_comment(line: &str) -> &str {
    let mut quoted = false;
    let mut escaped = false;
    for (index, character) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            ';' if !quoted => return &line[..index],
            _ => {}
        }
    }
    line
}

fn unquote(value: &str) -> String {
    let Some(inner) = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
    else {
        return value.to_owned();
    };
    let mut output = String::with_capacity(inner.len());
    let mut characters = inner.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            output.push(character);
            continue;
        }
        match characters.next() {
            Some(escaped @ ('\\' | '"')) => output.push(escaped),
            Some(other) => {
                output.push('\\');
                output.push(other);
            }
            None => output.push('\\'),
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"
        [general]
        bind = 0.0.0.0:2000
        advertised_address = 192.0.2.10
        disallow = all
        allow = ulaw
        allow = alaw

        [SEP001122334455]
        type = device
        description = Reception
        line = 1001

        [1001]
        type = line
        label = Reception
        context = from-sccp
        callerid = "Reception" <1001>
        mailbox = 1001@default
    "#;

    #[test]
    fn general_policy_views_use_runtime_duration_types_without_changing_defaults() {
        let general = GeneralConfig::default();
        assert_eq!(
            general.timing_policy(),
            GeneralTimingPolicy {
                keepalive: Duration::from_secs(30),
                secondary_keepalive: Duration::from_secs(30),
                first_digit_timeout: Duration::from_secs(10),
                interdigit_timeout: Duration::from_secs(5),
                call_waiting_repeat: Duration::ZERO,
            }
        );
        assert_eq!(
            general.station_policy(),
            GeneralStationPolicy {
                timezone_offset_minutes: 0,
                date_template: DateTemplate::default(),
                ring_type: RingerMode::Outside,
                call_waiting_tone: Some(Tone::CallWaiting),
            }
        );
    }

    #[test]
    fn sample_configuration_stays_parseable() {
        let config = ModuleConfig::parse(include_str!("../../sccp.conf.example")).unwrap();
        assert_eq!(config.devices.len(), 1);
        assert_eq!(config.lines.len(), 2);
        assert!(config.soft_key_profile("reception-softkeys").is_some());
        assert_eq!(config.general.listeners.clear.port(), 2000);
        assert_eq!(
            config
                .general
                .listeners
                .tls
                .as_ref()
                .expect("sample documents complete TLS policy")
                .bind
                .port(),
            2443
        );
        assert!(!config.general.network.acl.rules.is_empty());
        assert!(matches!(
            config.general.network.external,
            Some(ExternalAddress::Hostname {
                refresh_seconds: 60,
                ..
            })
        ));
        assert_eq!(config.general.date_template.as_str(), "D/M/Y");
        assert_eq!(config.general.timezone_offset_minutes, 0);
        assert_eq!(config.general.qos, QosPolicy::default());
        assert_eq!(config.general.registration.contexts.len(), 2);

        let device_id = DeviceId::new("SEP001122334455").unwrap();
        let device = &config.devices[&device_id];
        assert_eq!(device.network.transport, TransportRequirement::Either);
        assert_eq!(device.network.qos, QosPolicy::default());
        assert_eq!(device.call_ui.mwi_lamp_mode, LampMode::On);
        assert!(!device.call_ui.mwi_on_call);
        assert!(device.conference.allowed);
        assert_eq!(device.conference.dialing.application_options, "Mac");
        assert_eq!(
            device.feature_defaults.forwarding,
            ForwardingDefaults::default()
        );
        assert!(
            device
                .buttons
                .iter()
                .any(|button| matches!(button, ButtonDefinition::AddonModule(_)))
        );

        let line = config.features_for_line("1001").unwrap();
        assert_eq!(line.media.video_mode, VideoMode::Auto);
        assert_eq!(line.conference.destination.as_deref(), Some("700"));
        assert_eq!(
            line.voicemail
                .number
                .as_ref()
                .map(VoicemailDestination::as_str),
            Some("600")
        );
        assert_eq!(line.registration.extensions.len(), 2);
    }

    #[test]
    fn omitted_codec_policy_allows_every_mapped_audio_format() {
        let defaults = GeneralConfig::default();
        assert_eq!(defaults.codecs, mapped_audio_codecs());
        assert!(
            defaults
                .codecs
                .iter()
                .copied()
                .all(|codec| pbx_audio_format(codec).is_ok())
        );
    }

    #[test]
    fn explicitly_unrepresentable_audio_codec_is_rejected() {
        for codec in ["isac", "aac", "amr", "g728", "activevoice"] {
            let input = CONFIG.replace(
                "disallow = all\n        allow = ulaw\n        allow = alaw",
                &format!("disallow = all\n        allow = {codec}"),
            );
            assert!(matches!(
                ModuleConfig::parse(&input),
                Err(ConfigError::InvalidValue { .. })
            ));
        }
    }

    #[test]
    fn parses_native_configuration_and_builds_definitions() {
        let config = ModuleConfig::parse(CONFIG).unwrap();
        assert_eq!(
            config.general.advertised_address,
            "192.0.2.10".parse::<Ipv4Addr>().unwrap()
        );
        assert_eq!(
            config.general.codecs,
            [
                Codec::Pcmu,
                Codec::G711Ulaw56k,
                Codec::Pcma,
                Codec::G711Alaw56k,
            ]
        );
        assert_eq!(config.general.remote_hangup_tone, None);
        let device_id = DeviceId::new("SEP001122334455").unwrap();
        let profile = config.soft_key_profile_for_device(&device_id).unwrap();
        assert_eq!(profile.name, DEFAULT_SOFT_KEY_PROFILE);
        assert_eq!(profile.sets.len(), KeyMode::ALL_KNOWN.len());
        assert_eq!(profile.actions(KeyMode::OnHook), [SoftKey::NewCall]);
        assert_eq!(
            profile.actions(KeyMode::Connected),
            [SoftKey::Hold, SoftKey::EndCall, SoftKey::Transfer]
        );
        let features = config.feature_defaults_for_device(&device_id).unwrap();
        assert_eq!(features, &DeviceFeatureDefaults::default());
        let line_features = config.features_for_line("1001").unwrap();
        let mut expected_line_features = LineFeatureConfig::default();
        expected_line_features.registration.extensions = vec![RegistrationExtension {
            extension: "1001".into(),
            context: None,
        }];
        expected_line_features.media.codecs = config.general.codecs.clone();
        assert_eq!(line_features, &expected_line_features);
        assert!(config.registration_contexts().is_empty());
        assert!(
            config
                .registration_targets_for_line("1001")
                .unwrap()
                .is_empty()
        );
        let binding = config.line("1001").unwrap();
        assert_eq!(binding.device_id.as_str(), "SEP001122334455");
        assert_eq!(binding.line_instance, 1);
        assert_eq!(binding.line.caller_name, "Reception");
        assert_eq!(
            config.device_definitions()[0].first_line().unwrap().number,
            "1001"
        );
        assert_eq!(config.dial_target("1001"), Some(binding));
        assert_eq!(config.dial_target("SEP001122334455/1001"), Some(binding));
        assert!(config.dial_target("SEP000000000000/1001").is_none());
        assert_eq!(config.appearances_for_line("1001").count(), 1);
        assert_eq!(
            config
                .appearances_for_device(&binding.device_id)
                .map(|appearance| appearance.line_instance)
                .collect::<Vec<_>>(),
            [1]
        );
    }

    #[test]
    fn parses_bounded_channel_metadata_with_exact_inheritance_and_order() {
        let input = CONFIG
            .replace(
                "advertised_address = 192.0.2.10",
                "advertised_address = 192.0.2.10\n        language = sv\n        accountcode = general-private",
            )
            .replace(
                "description = Reception",
                "description = Reception\n        setvar = DEVICE_CLASS=desk\n        setvar = __TRACE_ID=alpha",
            )
            .replace(
                "mailbox = 1001@default",
                "mailbox = 1001@default\n        language = en_GB\n        accountcode = line-private\n        setvar = LINE_CLASS=reception",
            );
        let config = ModuleConfig::parse(&input).unwrap();
        assert_eq!(config.general.language, "sv");
        assert_eq!(
            config.general.account_code.as_deref(),
            Some("general-private")
        );
        let device = config
            .devices
            .get(&DeviceId::new("SEP001122334455").unwrap())
            .unwrap();
        assert_eq!(
            device
                .channel_variables
                .iter()
                .map(|variable| variable.name())
                .collect::<Vec<_>>(),
            ["DEVICE_CLASS", "__TRACE_ID"]
        );
        let line = config.lines.get("1001").unwrap();
        assert_eq!(line.language, "en_GB");
        assert_eq!(line.account_code.as_deref(), Some("line-private"));
        assert_eq!(line.channel_variables[0].name(), "LINE_CLASS");
        let debug = format!("{config:?}");
        for private in [
            "general-private",
            "line-private",
            "DEVICE_CLASS",
            "desk",
            "TRACE_ID",
            "LINE_CLASS",
        ] {
            assert!(!debug.contains(private), "debug leaked {private}");
        }

        let inherited = ModuleConfig::parse(
            &CONFIG.replace(
                "advertised_address = 192.0.2.10",
                "advertised_address = 192.0.2.10\n        language = sv\n        accountcode = general-private",
            ),
        )
        .unwrap();
        let line = inherited.lines.get("1001").unwrap();
        assert_eq!(line.language, "sv");
        assert_eq!(line.account_code.as_deref(), Some("general-private"));
    }

    #[test]
    fn channel_metadata_rejects_unsafe_or_duplicate_assignments_without_disclosure() {
        for assignment in [
            "FUNC(value)=private-one",
            "AUTHORIZATION_TOKEN=private-two",
            "DUPLICATE=one\n        setvar = DUPLICATE=two",
            "EMPTY=",
        ] {
            let input = CONFIG.replace(
                "description = Reception",
                &format!("description = Reception\n        setvar = {assignment}"),
            );
            let error = ModuleConfig::parse(&input).unwrap_err().to_string();
            assert!(error.contains("<redacted>"), "{error}");
            assert!(!error.contains("private-one"), "{error}");
            assert!(!error.contains("private-two"), "{error}");
        }

        let input = CONFIG.replace(
            "advertised_address = 192.0.2.10",
            &format!(
                "advertised_address = 192.0.2.10\n        accountcode = {}",
                "s".repeat(MAX_ACCOUNT_CODE_BYTES + 1)
            ),
        );
        let error = ModuleConfig::parse(&input).unwrap_err().to_string();
        assert!(error.contains("<redacted>"), "{error}");
        assert!(!error.contains(&"s".repeat(MAX_ACCOUNT_CODE_BYTES + 1)));
    }

    #[test]
    fn call_selection_and_station_history_policies_are_typed_with_safe_defaults() {
        let defaults = ModuleConfig::parse(CONFIG).unwrap();
        let device_id = DeviceId::new("SEP001122334455").unwrap();
        assert_eq!(defaults.call_answer_order(), CallAnswerOrder::OldestFirst);
        assert_eq!(
            defaults.call_ui_for_device(&device_id),
            Some(&DeviceCallUiConfig {
                redial_mode: RedialMode::LastNumber,
                hinted_ringing_notification: false,
                ..DeviceCallUiConfig::default()
            })
        );

        let input = CONFIG
            .replace(
                "advertised_address = 192.0.2.10",
                "advertised_address = 192.0.2.10\n        callanswerorder = LastFirst",
            )
            .replace(
                "description = Reception",
                "description = Reception\n        useRedialMenu = yes\n        allowRinginNotification = on\n        mwilamp = flash\n        mwioncall = yes\n        phonecodepage = ASCII",
            );
        let configured = ModuleConfig::parse(&input).unwrap();
        assert_eq!(configured.call_answer_order(), CallAnswerOrder::LastFirst);
        assert_eq!(
            configured.call_ui_for_device(&device_id),
            Some(&DeviceCallUiConfig {
                redial_mode: RedialMode::PlacedCallsMenu,
                hinted_ringing_notification: true,
                mwi_lamp_mode: LampMode::Flash,
                mwi_on_call: true,
                legacy_code_page: LegacyCodePage::Ascii,
            })
        );
    }

    #[test]
    fn station_calendar_policy_is_typed_and_bounded() {
        let input = CONFIG.replace(
            "advertised_address = 192.0.2.10",
            "advertised_address = 192.0.2.10\n        dateformat = Y.M.DA\n        tzoffset = -8",
        );
        let parsed = ModuleConfig::parse(&input).unwrap();
        assert_eq!(parsed.general.date_template.as_str(), "Y.M.DA");
        assert!(parsed.general.date_template.uses_twelve_hour_clock());
        assert_eq!(parsed.general.timezone_offset_minutes, -480);

        for setting in ["dateformat = MDY", "dateformat = D/M-M", "tzoffset = 15"] {
            let input = CONFIG.replace(
                "advertised_address = 192.0.2.10",
                &format!("advertised_address = 192.0.2.10\n        {setting}"),
            );
            assert!(ModuleConfig::parse(&input).is_err(), "accepted {setting}");
        }
    }

    #[test]
    fn ringing_waiting_and_incoming_limit_policies_are_typed_and_bounded() {
        let defaults = ModuleConfig::parse(CONFIG).unwrap();
        assert_eq!(defaults.general.ring_type, RingerMode::Outside);
        assert_eq!(defaults.general.call_waiting_tone, Some(Tone::CallWaiting));
        assert_eq!(defaults.general.call_waiting_interval_seconds, 0);
        assert_eq!(
            defaults.features_for_line("1001").unwrap().incoming_limit,
            6
        );

        let configured = ModuleConfig::parse(
            &CONFIG
                .replace(
                    "advertised_address = 192.0.2.10",
                    "advertised_address = 192.0.2.10\n        ringtype = Urgent\n        callwaitingtone = PriorityCallWaiting\n        callwaitinginterval = 12",
                )
                .replace("mailbox = 1001@default", "mailbox = 1001@default\n        incominglimit = 2"),
        )
        .unwrap();
        assert_eq!(configured.general.ring_type, RingerMode::Urgent);
        assert_eq!(
            configured.general.call_waiting_tone,
            Some(Tone::PriorityCallWaiting)
        );
        assert_eq!(configured.general.call_waiting_interval_seconds, 12);
        assert_eq!(
            configured.features_for_line("1001").unwrap().incoming_limit,
            2
        );

        let disabled = ModuleConfig::parse(&CONFIG.replace(
            "advertised_address = 192.0.2.10",
            "advertised_address = 192.0.2.10\n        callwaitingtone = 0",
        ))
        .unwrap();
        assert_eq!(disabled.general.call_waiting_tone, None);

        for setting in [
            "ringtype = emergency",
            "callwaitingtone = unknown",
            "callwaitinginterval = -1",
            "callwaitinginterval = 86401",
        ] {
            let input = CONFIG.replace(
                "advertised_address = 192.0.2.10",
                &format!("advertised_address = 192.0.2.10\n        {setting}"),
            );
            assert!(ModuleConfig::parse(&input).is_err(), "accepted {setting}");
        }
        for setting in ["incominglimit = -1", "incominglimit = 256"] {
            let input = CONFIG.replace(
                "mailbox = 1001@default",
                &format!("mailbox = 1001@default\n        {setting}"),
            );
            assert!(ModuleConfig::parse(&input).is_err(), "accepted {setting}");
        }
    }

    #[test]
    fn fallback_registration_policy_has_safe_typed_defaults() {
        let config = ModuleConfig::parse(CONFIG).unwrap();

        assert_eq!(
            config.fallback_registration(),
            &FallbackRegistrationConfig {
                decision: FallbackDecision::Reject,
                backoff_seconds: 60,
                server_priority: 1,
            }
        );
    }

    #[test]
    fn transfer_on_hangup_is_disabled_by_default_and_exactly_named() {
        let defaults = ModuleConfig::parse(CONFIG).unwrap();
        assert!(!defaults.general.transfer_on_hangup);

        let configured = ModuleConfig::parse(&CONFIG.replace(
            "advertised_address = 192.0.2.10",
            "advertised_address = 192.0.2.10\n        transfer_on_hangup = yes",
        ))
        .unwrap();
        assert!(configured.general.transfer_on_hangup);

        for settings in [
            "transfer-on-hangup = yes",
            "transferonhangup = yes",
            "transfer_on_hangup = maybe",
            "transfer_on_hangup = yes\n        transfer_on_hangup = no",
        ] {
            let input = CONFIG.replace(
                "advertised_address = 192.0.2.10",
                &format!("advertised_address = 192.0.2.10\n        {settings}"),
            );
            assert!(
                ModuleConfig::parse(&input).is_err(),
                "accepted invalid transfer policy: {settings}",
            );
        }
    }

    #[test]
    fn first_digit_timeout_is_distinct_bounded_and_exactly_named() {
        let defaults = ModuleConfig::parse(CONFIG).unwrap();
        assert_eq!(defaults.general.first_digit_timeout_ms, 10_000);

        let configured = ModuleConfig::parse(&CONFIG.replace(
            "advertised_address = 192.0.2.10",
            "advertised_address = 192.0.2.10\n        firstdigittimeout = 16",
        ))
        .unwrap();
        assert_eq!(configured.general.first_digit_timeout_ms, 16_000);

        for settings in [
            "firstdigittimeout = 0",
            "firstdigittimeout = 86401",
            "firstdigittimeout = -1",
            "firstdigittimeout = 10\n        firstdigittimeout = 11",
        ] {
            let input = CONFIG.replace(
                "advertised_address = 192.0.2.10",
                &format!("advertised_address = 192.0.2.10\n        {settings}"),
            );
            assert!(
                matches!(
                    ModuleConfig::parse(&input),
                    Err(ConfigError::InvalidValue { .. })
                ),
                "accepted invalid first-digit timeout: {settings}"
            );
        }
    }

    #[test]
    fn subsequent_digit_timeout_accepts_exact_seconds_or_milliseconds() {
        let defaults = ModuleConfig::parse(CONFIG).unwrap();
        assert_eq!(defaults.general.interdigit_timeout_ms, 5_000);

        for (setting, expected_ms) in [
            ("digittimeout = 8", 8_000),
            ("interdigit_timeout_ms = 1750", 1_750),
        ] {
            let input = CONFIG.replace(
                "advertised_address = 192.0.2.10",
                &format!("advertised_address = 192.0.2.10\n        {setting}"),
            );
            assert_eq!(
                ModuleConfig::parse(&input)
                    .unwrap()
                    .general
                    .interdigit_timeout_ms,
                expected_ms
            );
        }

        for settings in [
            "digittimeout = 0",
            "digittimeout = 86401",
            "interdigit_timeout_ms = 249",
            "interdigit_timeout_ms = 86400001",
            "interdigittimeout = 5",
            "digittimeout = 5\n        interdigit_timeout_ms = 1500",
            "digittimeout = 5\n        digittimeout = 6",
        ] {
            let input = CONFIG.replace(
                "advertised_address = 192.0.2.10",
                &format!("advertised_address = 192.0.2.10\n        {settings}"),
            );
            assert!(
                matches!(
                    ModuleConfig::parse(&input),
                    Err(ConfigError::InvalidValue { .. })
                ),
                "accepted invalid subsequent-digit timeout: {settings}"
            );
        }
    }

    #[test]
    fn dial_terminator_is_typed_bounded_and_exactly_named() {
        let defaults = ModuleConfig::parse(CONFIG).unwrap();
        assert_eq!(
            defaults.general.dial_terminator,
            DialTerminatorConfig::default()
        );

        for (raw, expected) in [
            ("0", '0'),
            ("9", '9'),
            ("*", '*'),
            ("#", '#'),
            ("a", 'A'),
            ("D", 'D'),
        ] {
            let input = CONFIG.replace(
                "advertised_address = 192.0.2.10",
                &format!(
                    "advertised_address = 192.0.2.10\n        digittimeoutchar = {raw}\n        recorddigittimeoutchar = yes"
                ),
            );
            assert_eq!(
                ModuleConfig::parse(&input).unwrap().general.dial_terminator,
                DialTerminatorConfig {
                    character: expected,
                    record: true,
                }
            );
        }

        for settings in [
            "digittimeoutchar =",
            "digittimeoutchar = 12",
            "digittimeoutchar = E",
            "digittimeoutchar = +",
            "recorddigittimeoutchar = perhaps",
            "digittimeoutchar = #\n        digittimeoutchar = *",
            "recorddigittimeoutchar = yes\n        recorddigittimeoutchar = no",
        ] {
            let input = CONFIG.replace(
                "advertised_address = 192.0.2.10",
                &format!("advertised_address = 192.0.2.10\n        {settings}"),
            );
            assert!(
                matches!(
                    ModuleConfig::parse(&input),
                    Err(ConfigError::InvalidValue { .. })
                ),
                "accepted invalid dial terminator policy: {settings}"
            );
        }
    }

    #[test]
    fn simulated_enbloc_has_a_safe_exact_boolean_policy() {
        let defaults = ModuleConfig::parse(CONFIG).unwrap();
        assert!(defaults.general.simulate_enbloc);

        let disabled = ModuleConfig::parse(&CONFIG.replace(
            "advertised_address = 192.0.2.10",
            "advertised_address = 192.0.2.10\n        simulate_enbloc = no",
        ))
        .unwrap();
        assert!(!disabled.general.simulate_enbloc);

        for settings in [
            "simulateenbloc = yes",
            "simulate_enbloc = perhaps",
            "simulate_enbloc = yes\n        simulate_enbloc = no",
        ] {
            let input = CONFIG.replace(
                "advertised_address = 192.0.2.10",
                &format!("advertised_address = 192.0.2.10\n        {settings}"),
            );
            assert!(
                matches!(
                    ModuleConfig::parse(&input),
                    Err(ConfigError::InvalidValue { .. })
                ),
                "accepted invalid simulated en-bloc policy: {settings}"
            );
        }
    }

    #[test]
    fn speed_dial_further_digit_policy_is_explicit_and_disabled_by_default() {
        let defaults = ModuleConfig::parse(CONFIG).unwrap();
        assert!(!defaults.general.speed_dial_await_further_digits);
        assert!(
            defaults
                .device_definitions()
                .iter()
                .all(|device| !device.ui.speed_dial_await_further_digits)
        );

        let enabled = ModuleConfig::parse(&CONFIG.replace(
            "advertised_address = 192.0.2.10",
            "advertised_address = 192.0.2.10\n        SpeedDialAwaitFurtherDigits = yes",
        ))
        .unwrap();
        assert!(enabled.general.speed_dial_await_further_digits);
        assert!(
            enabled
                .device_definitions()
                .iter()
                .all(|device| device.ui.speed_dial_await_further_digits)
        );

        for settings in [
            "SpeedDialAwaitFurtherDigits = perhaps",
            "SpeedDialAwaitFurtherDigits = yes\n        SpeedDialAwaitFurtherDigits = no",
        ] {
            let input = CONFIG.replace(
                "advertised_address = 192.0.2.10",
                &format!("advertised_address = 192.0.2.10\n        {settings}"),
            );
            assert!(
                matches!(
                    ModuleConfig::parse(&input),
                    Err(ConfigError::InvalidValue { .. })
                ),
                "accepted invalid speed-dial further-digit policy: {settings}"
            );
        }
    }

    #[test]
    fn overlap_dialing_is_explicit_disabled_by_default_and_device_overridable() {
        let device_id = DeviceId::new("SEP001122334455").unwrap();
        let defaults = ModuleConfig::parse(CONFIG).unwrap();
        assert!(!defaults.general.allow_overlap);
        assert!(!defaults.devices[&device_id].allow_overlap);

        let enabled = ModuleConfig::parse(&CONFIG.replace(
            "advertised_address = 192.0.2.10",
            "advertised_address = 192.0.2.10\n        allowoverlap = yes",
        ))
        .unwrap();
        assert!(enabled.general.allow_overlap);
        assert!(enabled.devices[&device_id].allow_overlap);

        let overridden = ModuleConfig::parse(
            &CONFIG
                .replace(
                    "advertised_address = 192.0.2.10",
                    "advertised_address = 192.0.2.10\n        allowoverlap = yes",
                )
                .replace(
                    "description = Reception",
                    "description = Reception\n        allowoverlap = no",
                ),
        )
        .unwrap();
        assert!(!overridden.devices[&device_id].allow_overlap);

        for settings in [
            "allowoverlap = perhaps",
            "allowoverlap = yes\n        allowoverlap = no",
        ] {
            let input = CONFIG.replace(
                "advertised_address = 192.0.2.10",
                &format!("advertised_address = 192.0.2.10\n        {settings}"),
            );
            assert!(
                matches!(
                    ModuleConfig::parse(&input),
                    Err(ConfigError::InvalidValue { .. })
                ),
                "accepted unsafe overlap setting: {settings}"
            );
        }
    }

    #[test]
    fn line_dial_tones_are_typed_inherited_bounded_and_exactly_named() {
        let defaults = ModuleConfig::parse(CONFIG).unwrap();
        assert_eq!(
            defaults.features_for_line("1001").unwrap().dial_tones,
            LineDialToneConfig::default()
        );

        let configured = ModuleConfig::parse(&CONFIG.replace(
            "mailbox = 1001@default",
            "mailbox = 1001@default\n        initial_dialtone_tone = Recall Dial Tone\n        secondary_dialtone_digits = 9a#\n        secondary_dialtone_tone = 0x2a",
        ))
        .unwrap();
        assert_eq!(
            configured.features_for_line("1001").unwrap().dial_tones,
            LineDialToneConfig {
                initial: Tone::RecallDial,
                secondary_prefix: Some("9A#".into()),
                secondary: Tone::PartialDial,
            }
        );
        let station_line = configured
            .device_definitions()
            .into_iter()
            .flat_map(|device| device.buttons)
            .find_map(|button| match button {
                ButtonDefinition::Line(line) if line.number == "1001" => Some(line),
                _ => None,
            })
            .unwrap();
        assert_eq!(station_line.initial_tone, Tone::RecallDial);

        let cleared = ModuleConfig::parse(&CONFIG.replace(
            "mailbox = 1001@default",
            "mailbox = 1001@default\n        secondary_dialtone_digits =",
        ))
        .unwrap();
        assert_eq!(
            cleared
                .features_for_line("1001")
                .unwrap()
                .dial_tones
                .secondary_prefix,
            None
        );

        for setting in [
            "initialdialtonetone = Inside Dial Tone",
            "secondarydialtonedigits = 9",
            "secondarydialtonetone = Outside Dial Tone",
            "secondary_dialtone_digits = 1234567890",
            "secondary_dialtone_digits = 9+",
            "secondary_dialtone_tone = unknown tone",
            "secondary_dialtone_digits = 9\n        secondary_dialtone_digits = 8",
        ] {
            let input = CONFIG.replace(
                "mailbox = 1001@default",
                &format!("mailbox = 1001@default\n        {setting}"),
            );
            assert!(
                matches!(
                    ModuleConfig::parse(&input),
                    Err(ConfigError::InvalidValue { .. })
                ),
                "accepted invalid line dial-tone setting: {setting}"
            );
        }
    }

    #[test]
    fn parses_fallback_decision_priority_and_backoff() {
        for (raw, expected) in [
            ("yes", FallbackDecision::Accept),
            ("no", FallbackDecision::Reject),
            ("odd", FallbackDecision::DeviceIdOdd),
            ("even", FallbackDecision::DeviceIdEven),
        ] {
            let input = CONFIG.replace(
                "advertised_address = 192.0.2.10",
                &format!(
                    "advertised_address = 192.0.2.10\n        fallback = {raw}\n        backoff_time = 90\n        server_priority = 2"
                ),
            );
            let config = ModuleConfig::parse(&input).unwrap();
            assert_eq!(
                config.fallback_registration(),
                &FallbackRegistrationConfig {
                    decision: expected,
                    backoff_seconds: 90,
                    server_priority: 2,
                }
            );
        }
    }

    #[test]
    fn rejects_invalid_duplicate_or_invented_fallback_settings() {
        for settings in [
            "fallback = sometimes",
            "fallback = /private/runner",
            "backoff_time = 29",
            "backoff_time = -1",
            "server_priority = 0",
            "server_priority = -1",
            "server_priority = 256",
            "fallback_mode = yes",
            "backoff-time = 60",
            "server-priority = 1",
            "fallback = yes\n        fallback = no",
            "backoff_time = 60\n        backoff_time = 90",
            "server_priority = 1\n        server_priority = 2",
        ] {
            let input = CONFIG.replace(
                "advertised_address = 192.0.2.10",
                &format!("advertised_address = 192.0.2.10\n        {settings}"),
            );
            assert!(
                matches!(
                    ModuleConfig::parse(&input),
                    Err(ConfigError::InvalidValue { .. })
                ),
                "accepted invalid fallback settings: {settings}"
            );
        }

        let rejected_path = "/private/runner";
        let error = ModuleConfig::parse(&CONFIG.replace(
            "advertised_address = 192.0.2.10",
            &format!("advertised_address = 192.0.2.10\n        fallback = {rejected_path}"),
        ))
        .unwrap_err();
        assert!(!error.to_string().contains(rejected_path));
    }

    #[test]
    fn parses_bounded_transport_specific_server_routes() {
        let input = CONFIG.replace(
            "advertised_address = 192.0.2.10",
            "advertised_address = 192.0.2.10\n        secondary_keepalive = 45\n        signaling_server = 1, primary, 192.0.2.10, 2000, 2443\n        signaling_server = 2, backup, 2001:db8::20, 2001, none",
        );
        let config = ModuleConfig::parse(&input).unwrap();

        assert_eq!(config.general.secondary_keepalive_seconds, 45);
        assert_eq!(
            config.general.signaling_servers,
            [
                SignalingServerRoute {
                    priority: 1,
                    name: "primary".into(),
                    address: "192.0.2.10".parse().unwrap(),
                    clear_port: std::num::NonZeroU16::new(2000),
                    secure_port: std::num::NonZeroU16::new(2443),
                },
                SignalingServerRoute {
                    priority: 2,
                    name: "backup".into(),
                    address: "2001:db8::20".parse().unwrap(),
                    clear_port: std::num::NonZeroU16::new(2001),
                    secure_port: None,
                },
            ]
        );
    }

    #[test]
    fn rejects_ambiguous_or_unusable_server_routes() {
        for settings in [
            "signaling_server = 1, primary, 192.0.2.10, none, none",
            "signaling_server = 1, primary, 192.0.2.10, 2000, none\n        signaling_server = 1, duplicate, 192.0.2.20, 2001, none",
            "server_priority = 2\n        signaling_server = 1, primary, 192.0.2.10, 2000, none",
            "signaling-server = 1, primary, 192.0.2.10, 2000, none",
            "secondarykeepalive = 45",
        ] {
            let input = CONFIG.replace(
                "advertised_address = 192.0.2.10",
                &format!("advertised_address = 192.0.2.10\n        {settings}"),
            );
            assert!(ModuleConfig::parse(&input).is_err(), "accepted {settings}");
        }
    }

    #[test]
    fn station_history_policies_follow_device_template_scalar_inheritance() {
        let input = CONFIG
            .replace(
                "[SEP001122334455]",
                "[station-ui](!)\n        type = device\n        useRedialMenu = yes\n        allowRinginNotification = yes\n\n        [SEP001122334455](station-ui)",
            )
            .replace(
                "description = Reception",
                "description = Reception\n        useRedialMenu = no",
            );
        let config = ModuleConfig::parse(&input).unwrap();
        let device_id = DeviceId::new("SEP001122334455").unwrap();
        assert_eq!(
            config.call_ui_for_device(&device_id),
            Some(&DeviceCallUiConfig {
                redial_mode: RedialMode::LastNumber,
                hinted_ringing_notification: true,
                ..DeviceCallUiConfig::default()
            })
        );
    }

    #[test]
    fn call_answer_order_rejects_documentation_typos_and_invented_values() {
        for value in [
            "lastestfirst",
            "latestfirst",
            "newestfirst",
            "oldest",
            "1",
            "",
        ] {
            let input = CONFIG.replace(
                "advertised_address = 192.0.2.10",
                &format!("advertised_address = 192.0.2.10\n        callanswerorder = {value}"),
            );
            let error = ModuleConfig::parse(&input).unwrap_err().to_string();
            assert!(error.contains("[general].callanswerorder"), "{error}");
            assert!(error.contains("OldestFirst or LastFirst"), "{error}");
        }
    }

    #[test]
    fn call_ui_options_reject_invented_names_values_and_wrong_scopes() {
        for setting in [
            "redialmenu = yes",
            "ringing_notification = yes",
            "useRedialMenu = placedcalls",
            "allowRinginNotification = ringing",
            "callanswerorder = LastFirst",
        ] {
            let input = CONFIG.replace(
                "description = Reception",
                &format!("description = Reception\n        {setting}"),
            );
            let error = ModuleConfig::parse(&input).unwrap_err().to_string();
            assert!(error.contains("line "), "{setting} produced {error}");
            assert!(error.contains("expected"), "{setting} produced {error}");
        }

        let wrong_general_scope = CONFIG.replace(
            "advertised_address = 192.0.2.10",
            "advertised_address = 192.0.2.10\n        useRedialMenu = yes",
        );
        let error = ModuleConfig::parse(&wrong_general_scope)
            .unwrap_err()
            .to_string();
        assert!(error.contains("[general].useRedialMenu"), "{error}");
        assert!(error.contains("expected"), "{error}");

        let invented_general_name = CONFIG.replace(
            "advertised_address = 192.0.2.10",
            "advertised_address = 192.0.2.10\n        answer_call_order = LastFirst",
        );
        let error = ModuleConfig::parse(&invented_general_name)
            .unwrap_err()
            .to_string();
        assert!(error.contains("[general].answer_call_order"), "{error}");
        assert!(error.contains("unknown variant"), "{error}");
    }

    #[test]
    fn duplicate_call_selection_and_station_history_settings_are_rejected() {
        let duplicate_general = CONFIG.replace(
            "advertised_address = 192.0.2.10",
            "advertised_address = 192.0.2.10\n        callanswerorder = OldestFirst\n        CALLANSWERORDER = LastFirst",
        );
        let error = ModuleConfig::parse(&duplicate_general)
            .unwrap_err()
            .to_string();
        assert!(error.contains("[general].CALLANSWERORDER"), "{error}");
        assert!(error.contains("duplicates"), "{error}");

        for setting in [
            "useRedialMenu = yes\n        USEREDIALMENU = no",
            "allowRinginNotification = yes\n        ALLOWRINGINNOTIFICATION = no",
        ] {
            let input = CONFIG.replace(
                "description = Reception",
                &format!("description = Reception\n        {setting}"),
            );
            let error = ModuleConfig::parse(&input).unwrap_err().to_string();
            assert!(error.contains("[SEP001122334455]"), "{error}");
            assert!(error.contains("duplicates"), "{error}");
        }
    }

    #[test]
    fn parses_typed_mobility_pin_contexts_extensions_and_resolved_targets() {
        let input = CONFIG
            .replace(
                "advertised_address = 192.0.2.10",
                "advertised_address = 192.0.2.10\n        regcontext = registrations & backup-registrations",
            )
            .replace(
                "mailbox = 1001@default",
                "mailbox = 1001@default\n        pin = 0012345\n        regexten = 1001 & 91001@external-registrations",
            );
        let config = ModuleConfig::parse(&input).unwrap();

        assert_eq!(
            config.registration_contexts(),
            ["registrations", "backup-registrations"]
        );
        let mobility = config.mobility_for_line("1001").unwrap();
        let pin = mobility.pin.as_ref().unwrap();
        assert_eq!(pin.digits(), 7);
        assert!(pin.verify("0012345"));
        assert!(!pin.verify("12345"));
        assert_eq!(format!("{pin:?}"), "MobilityPin(<redacted>)");
        assert!(!format!("{config:?}").contains("0012345"));

        assert_eq!(
            config.registration_for_line("1001").unwrap().extensions,
            [
                RegistrationExtension {
                    extension: "1001".into(),
                    context: None,
                },
                RegistrationExtension {
                    extension: "91001".into(),
                    context: Some("external-registrations".into()),
                },
            ]
        );
        assert_eq!(
            config.registration_targets_for_line("1001").unwrap(),
            [
                RegistrationTarget {
                    extension: "1001".into(),
                    context: "registrations".into(),
                },
                RegistrationTarget {
                    extension: "1001".into(),
                    context: "backup-registrations".into(),
                },
                RegistrationTarget {
                    extension: "91001".into(),
                    context: "external-registrations".into(),
                },
            ]
        );
    }

    #[test]
    fn omitted_or_cleared_registration_extension_uses_the_logical_line_number() {
        let input = CONFIG.replace(
            "advertised_address = 192.0.2.10",
            "advertised_address = 192.0.2.10\n        regcontext = primary&secondary",
        );
        let config = ModuleConfig::parse(&input).unwrap();

        assert_eq!(
            config.registration_for_line("1001").unwrap().extensions,
            [RegistrationExtension {
                extension: "1001".into(),
                context: None,
            }]
        );
        assert_eq!(
            config.registration_targets_for_line("1001").unwrap(),
            [
                RegistrationTarget {
                    extension: "1001".into(),
                    context: "primary".into(),
                },
                RegistrationTarget {
                    extension: "1001".into(),
                    context: "secondary".into(),
                },
            ]
        );

        let cleared = ModuleConfig::parse(&input.replace(
            "type = line\n        label = Reception",
            "type = line\n        pin =\n        regexten =\n        label = Reception",
        ))
        .unwrap();
        assert_eq!(
            cleared.registration_for_line("1001").unwrap().extensions,
            config.registration_for_line("1001").unwrap().extensions
        );
        assert!(cleared.mobility_for_line("1001").unwrap().pin.is_none());
    }

    #[test]
    fn mobility_and_registration_settings_follow_line_template_scalar_inheritance() {
        let input = CONFIG
            .replace(
                "advertised_address = 192.0.2.10",
                "advertised_address = 192.0.2.10\n        regcontext = registrations",
            )
            .replace(
                "[1001]",
                "[mobile-line](!)\n        type = line\n        pin = 7654321\n        regexten = 91001\n\n        [1001](mobile-line)",
            );
        let inherited = ModuleConfig::parse(&input).unwrap();
        assert!(
            inherited
                .mobility_for_line("1001")
                .unwrap()
                .pin
                .as_ref()
                .unwrap()
                .verify("7654321")
        );
        assert_eq!(
            inherited.registration_targets_for_line("1001").unwrap(),
            [RegistrationTarget {
                extension: "91001".into(),
                context: "registrations".into(),
            }]
        );

        let cleared = ModuleConfig::parse(&input.replace(
            "[1001](mobile-line)",
            "[1001](mobile-line)\n        pin =\n        regexten =",
        ))
        .unwrap();
        assert!(cleared.mobility_for_line("1001").unwrap().pin.is_none());
        assert_eq!(
            cleared.registration_targets_for_line("1001").unwrap(),
            [RegistrationTarget {
                extension: "1001".into(),
                context: "registrations".into(),
            }]
        );
    }

    #[test]
    fn mobility_pin_errors_are_located_and_never_disclose_the_value() {
        for pin in ["12A4", "12345678"] {
            let input = CONFIG.replace(
                "mailbox = 1001@default",
                &format!("mailbox = 1001@default\n        pin = {pin}"),
            );
            let error = ModuleConfig::parse(&input).unwrap_err();
            let text = error.to_string();
            assert!(text.contains("[1001].pin"), "{text}");
            assert!(text.contains("<redacted>"), "{text}");
            assert!(!text.contains(pin), "{text}");
        }
    }

    #[test]
    fn mobility_pin_verification_checks_full_bound_and_all_diagnostics_are_redacted() {
        let pin = MobilityPin("0123456".into());
        assert!(pin.verify("0123456"));
        for candidate in [
            "1123456", "0023456", "0133456", "0124456", "0123556", "0123466", "0123457", "", "0",
            "012345", "01234567",
        ] {
            assert!(
                !pin.verify(candidate),
                "accepted mismatched candidate length {}",
                candidate.len()
            );
        }

        let debug = format!("{pin:?}");
        assert_eq!(debug, "MobilityPin(<redacted>)");
        assert!(!debug.contains("0123456"));
        let config = ModuleConfig::parse(&CONFIG.replace(
            "mailbox = 1001@default",
            "mailbox = 1001@default\n        pin = 0123456",
        ))
        .unwrap();
        let diagnostic = format!("{config:?}");
        assert!(!diagnostic.contains("0123456"));
        assert!(diagnostic.contains("MobilityPin(<redacted>)"));
    }

    #[test]
    fn mobility_and_registration_options_reject_invented_aliases() {
        let general_alias = CONFIG.replace(
            "advertised_address = 192.0.2.10",
            "advertised_address = 192.0.2.10\n        reg_context = registrations",
        );
        let text = ModuleConfig::parse(&general_alias).unwrap_err().to_string();
        assert!(text.contains("[general].reg_context"), "{text}");
        assert!(text.contains("unknown variant"), "{text}");

        let extension_alias = CONFIG.replace(
            "mailbox = 1001@default",
            "mailbox = 1001@default\n        reg_exten = 91001",
        );
        let text = ModuleConfig::parse(&extension_alias)
            .unwrap_err()
            .to_string();
        assert!(text.contains("[1001].reg_exten"), "{text}");
        assert!(text.contains("unknown variant"), "{text}");

        let pin_alias = CONFIG.replace(
            "mailbox = 1001@default",
            "mailbox = 1001@default\n        p-in = 7654321",
        );
        let text = ModuleConfig::parse(&pin_alias).unwrap_err().to_string();
        assert!(text.contains("[1001].p-in"), "{text}");
        assert!(text.contains("unknown variant"), "{text}");
        assert!(text.contains("<redacted>"), "{text}");
        assert!(!text.contains("7654321"), "{text}");
        assert!(text.contains("<redacted>"), "{text}");
        assert!(!text.contains("7654321"), "{text}");
    }

    #[test]
    fn registration_lists_reject_empty_duplicate_oversized_and_unscoped_entries() {
        for contexts in [
            "primary&&secondary".to_owned(),
            "primary&primary".to_owned(),
            "primary context".to_owned(),
            "x".repeat(MAX_REGISTRATION_IDENTIFIER_BYTES + 1),
        ] {
            let input = CONFIG.replace(
                "advertised_address = 192.0.2.10",
                &format!("advertised_address = 192.0.2.10\n        regcontext = {contexts}"),
            );
            let text = ModuleConfig::parse(&input).unwrap_err().to_string();
            assert!(text.contains("[general].regcontext"), "{text}");
            assert!(text.contains("expected"), "{text}");
        }

        for extensions in [
            "1001&&2000".to_owned(),
            "@registrations".to_owned(),
            "1001@".to_owned(),
            "1001@one@two".to_owned(),
            "1001&1001".to_owned(),
            "1001&1001@registrations".to_owned(),
            "10 01".to_owned(),
            "x".repeat(MAX_REGISTRATION_IDENTIFIER_BYTES + 1),
            vec!["x".repeat(64); 4].join("&"),
        ] {
            let input = CONFIG
                .replace(
                    "advertised_address = 192.0.2.10",
                    "advertised_address = 192.0.2.10\n        regcontext = registrations",
                )
                .replace(
                    "mailbox = 1001@default",
                    &format!("mailbox = 1001@default\n        regexten = {extensions}"),
                );
            let text = ModuleConfig::parse(&input).unwrap_err().to_string();
            assert!(text.contains("[1001].regexten"), "{text}");
            assert!(text.contains("expected"), "{text}");
        }

        let unscoped = CONFIG.replace(
            "mailbox = 1001@default",
            "mailbox = 1001@default\n        regexten = 91001",
        );
        let text = ModuleConfig::parse(&unscoped).unwrap_err().to_string();
        assert!(text.contains("[1001].regexten"), "{text}");
        assert!(text.contains("general regcontext"), "{text}");
    }

    #[test]
    fn duplicate_resolved_registration_targets_across_lines_are_rejected() {
        let input = r#"
            [general]
            advertised_address = 192.0.2.10
            regcontext = registrations

            [SEP001122334455]
            type = device
            line = 1001
            line = 1002

            [1001]
            type = line
            regexten = shared

            [1002]
            type = line
            regexten = shared@registrations
        "#;
        let text = ModuleConfig::parse(input).unwrap_err().to_string();
        assert!(text.contains("[1002].regexten"), "{text}");
        assert!(text.contains("shared@registrations"), "{text}");
        assert!(text.contains("already used by [1001]"), "{text}");
    }

    #[test]
    fn network_listener_and_qos_defaults_are_normalized() {
        let config = ModuleConfig::parse(CONFIG).unwrap();
        let device_id = DeviceId::new("SEP001122334455").unwrap();

        assert_eq!(config.listener_policy(), &ListenerPolicy::default());
        assert_eq!(config.qos_policy(), &QosPolicy::default());
        let mut expected_network = NetworkPolicy::default();
        expected_network.advertised.ipv4 = Some("192.0.2.10".parse().unwrap());
        assert_eq!(config.network_policy(), &expected_network);
        assert_eq!(
            config.network_for_device(&device_id),
            Some(&DeviceNetworkPolicy {
                acl: AccessControlList::default(),
                permitted_hosts: Vec::new(),
                nat: NatMode::Auto,
                qos: QosPolicy::default(),
                transport: TransportRequirement::Either,
            })
        );
    }

    #[test]
    fn parses_ipv4_ipv6_acl_nat_qos_and_split_tls_policy() {
        let input = CONFIG
            .replace(
                "bind = 0.0.0.0:2000\n        advertised_address = 192.0.2.10",
                "bindaddr = ::\n        port = 2001\n        advertised_ipv4 = none\n        advertised_ipv6 = 2001:db8::10\n        deny = 0.0.0.0/0\n        permit = 192.0.2.99/255.255.255.0\n        permit = 2001:db8:1::99/64\n        localnet =\n        localnet = 10.10.99.1/16\n        localnet = 2001:db8:2::99/64\n        externhost = PBX.EXAMPLE.test\n        externrefresh = 120\n        nat = (auto)on\n        signaling_dscp = AF31\n        signaling_cos = 5\n        audio_tos = 0xb8\n        audio_cos = 6\n        video_dscp = CS4\n        video_cos = 4\n        tls_bind = [::]:2443\n        tls_certificate = /etc/asterisk/tls/server.crt\n        tls_private_key = /etc/asterisk/tls/server.key\n        tls_trust_store = /etc/asterisk/tls/ca.pem",
            )
            .replace(
                "description = Reception",
                "description = Reception\n        deny =\n        permit = 203.0.113.99/24\n        permit_host = PHONE.EXAMPLE.test\n        nat = off\n        audio_dscp = EF\n        audio_cos = 7\n        video_tos = 0x88\n        transport = tls",
            );
        let config = ModuleConfig::parse(&input).unwrap();
        let device_id = DeviceId::new("SEP001122334455").unwrap();

        assert_eq!(config.listener_policy().clear, "[::]:2001".parse().unwrap());
        assert_eq!(
            config.listener_policy().tls,
            Some(TlsListener {
                bind: "[::]:2443".parse().unwrap(),
                credentials: TlsCredentials::SplitPem {
                    certificate: PathBuf::from("/etc/asterisk/tls/server.crt"),
                    private_key: PathBuf::from("/etc/asterisk/tls/server.key"),
                    trust_store: Some(PathBuf::from("/etc/asterisk/tls/ca.pem")),
                },
            })
        );
        assert_eq!(
            config.network_policy().advertised,
            AdvertisedAddresses {
                ipv4: None,
                ipv6: Some("2001:db8::10".parse().unwrap()),
            }
        );
        assert_eq!(
            config.network_policy().external,
            Some(ExternalAddress::Hostname {
                name: "pbx.example.test".into(),
                refresh_seconds: 120,
            })
        );
        assert_eq!(config.network_policy().nat, NatMode::AutoOn);
        assert_eq!(
            config.network_policy().local_networks,
            [
                IpNetwork {
                    address: "10.10.0.0".parse().unwrap(),
                    prefix: 16,
                },
                IpNetwork {
                    address: "2001:db8:2::".parse().unwrap(),
                    prefix: 64,
                },
            ]
        );
        assert_eq!(
            config.qos_policy().signaling,
            QosClass {
                dscp: Dscp(26),
                cos: Cos(5)
            }
        );
        assert_eq!(
            config.qos_policy().audio,
            QosClass {
                dscp: Dscp(46),
                cos: Cos(6)
            }
        );
        assert_eq!(
            config.qos_policy().video,
            QosClass {
                dscp: Dscp(32),
                cos: Cos(4)
            }
        );

        let device = config.network_for_device(&device_id).unwrap();
        let station = config
            .device_definitions()
            .into_iter()
            .find(|definition| definition.id == device_id)
            .unwrap();
        assert_eq!(device.transport, TransportRequirement::Tls);
        assert_eq!(station.transport, StationTransportRequirement::Secure);
        assert_eq!(station.signaling_qos, Some(SignalingQos::new(26, 5)));
        assert_eq!(device.nat, NatMode::Off);
        assert_eq!(device.permitted_hosts, ["phone.example.test"]);
        assert_eq!(
            device.acl.rules,
            [AclRule {
                action: AclAction::Permit,
                network: IpNetwork {
                    address: "203.0.113.0".parse().unwrap(),
                    prefix: 24,
                },
            }]
        );
        assert_eq!(
            device.qos.audio,
            QosClass {
                dscp: Dscp(46),
                cos: Cos(7)
            }
        );
        assert_eq!(device.qos.video.dscp, Dscp(34));
    }

    #[test]
    fn device_network_policy_inherits_scalars_and_clears_ordered_rules() {
        let input = CONFIG.replace(
            "[SEP001122334455]",
            "[network-base](!)\n        type = device\n        deny = 0.0.0.0/0\n        permit = 10.0.0.0/8\n        permit_host = old.example.test\n        audio_cos = 2\n        transport = clear\n\n        [network-site](!, network-base)\n        deny =\n        permit = 2001:db8:5::1/64\n        permit_host =\n        permit_host = phone.example.test\n        audio_cos = 7\n\n        [SEP001122334455](network-site)",
        );
        let config = ModuleConfig::parse(&input).unwrap();
        let device_id = DeviceId::new("SEP001122334455").unwrap();
        let policy = config.network_for_device(&device_id).unwrap();

        assert_eq!(policy.transport, TransportRequirement::Clear);
        assert_eq!(policy.qos.audio.cos, Cos(7));
        assert_eq!(policy.permitted_hosts, ["phone.example.test"]);
        assert_eq!(policy.acl.rules.len(), 1);
        assert_eq!(policy.acl.rules[0].action, AclAction::Permit);
        assert_eq!(
            policy.acl.rules[0].network.address,
            "2001:db8:5::".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn combined_pem_and_accepted_transport_aliases_are_typed() {
        let input = CONFIG
            .replace(
                "advertised_address = 192.0.2.10",
                "advertised_address = 192.0.2.10\n        secbindaddr = 0.0.0.0\n        secport = 2443\n        certfile = /etc/asterisk/tls/server.pem",
            )
            .replace("description = Reception", "description = Reception\n        transport_requirement = secure");
        let config = ModuleConfig::parse(&input).unwrap();
        let device_id = DeviceId::new("SEP001122334455").unwrap();

        assert_eq!(
            config.listener_policy().tls.as_ref().unwrap().credentials,
            TlsCredentials::CombinedPem(PathBuf::from("/etc/asterisk/tls/server.pem"))
        );
        assert_eq!(
            config.network_for_device(&device_id).unwrap().transport,
            TransportRequirement::Tls
        );
    }

    #[test]
    fn single_advertised_address_alias_selects_exactly_one_ip_family() {
        let input = CONFIG.replace(
            "advertised_address = 192.0.2.10",
            "advertised_address = 2001:db8::20",
        );
        let config = ModuleConfig::parse(&input).unwrap();

        assert_eq!(
            config.network_policy().advertised,
            AdvertisedAddresses {
                ipv4: None,
                ipv6: Some("2001:db8::20".parse().unwrap()),
            }
        );
    }

    #[test]
    fn accepted_nat_and_transport_spellings_have_one_typed_result_each() {
        for (raw, expected) in [
            ("auto", NatMode::Auto),
            ("off", NatMode::Off),
            ("(auto)off", NatMode::AutoOff),
            ("on", NatMode::On),
            ("(auto)on", NatMode::AutoOn),
        ] {
            assert_eq!(parse_nat_mode("nat", raw).unwrap(), expected);
        }
        for (raw, expected) in [
            ("clear", TransportRequirement::Clear),
            ("tcp", TransportRequirement::Clear),
            ("tls", TransportRequirement::Tls),
            ("secure", TransportRequirement::Tls),
            ("either", TransportRequirement::Either),
            ("any", TransportRequirement::Either),
        ] {
            assert_eq!(
                parse_transport_requirement("transport", raw).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn rejects_invalid_acl_nat_qos_listener_and_external_ranges() {
        for (setting, expected) in [
            ("permit = 192.0.2.0/33", "prefix 0..32"),
            ("permit = 192.0.2.0/255.0.255.0", "contiguous IPv4 netmask"),
            ("permit = 2001:db8::/129", "IPv6 prefix 0..128"),
            ("nat = sometimes", "auto, off"),
            ("audio_dscp = 64", "DSCP 0..63"),
            ("video_cos = 8", "COS priority 0..7"),
            ("port = 0", "TCP port 1..65535"),
            (
                "tls_bind = 0.0.0.0:0\n        certfile = /tls.pem",
                "TLS listener port",
            ),
            ("externrefresh = 0", "1..86400"),
            ("externip = 0.0.0.0", "non-unspecified"),
        ] {
            let input = CONFIG.replace(
                "advertised_address = 192.0.2.10",
                &format!("advertised_address = 192.0.2.10\n        {setting}"),
            );
            let error = ModuleConfig::parse(&input).unwrap_err().to_string();
            assert!(error.contains("line "), "missing line in {error}");
            assert!(
                error.contains("[general]."),
                "missing section/key in {error}"
            );
            assert!(error.contains("expected"), "missing expectation in {error}");
            assert!(
                error.contains(expected),
                "{error} did not contain {expected}"
            );
        }
    }

    #[test]
    fn rejects_network_listener_and_tls_contradictions() {
        for settings in [
            "bind = 0.0.0.0:2001\n        bindaddr = ::",
            "advertised_ipv4 = none\n        advertised_ipv6 = none",
            "advertised_address = 192.0.2.20\n        advertised_ipv4 = 192.0.2.21",
            "externip = 192.0.2.20\n        externhost = pbx.example.test",
            "externip = 192.0.2.20\n        externrefresh = 60",
            "tls_bind = 0.0.0.0:2000\n        certfile = /tls.pem",
            "certfile = /tls.pem\n        tls_certificate = /tls.crt\n        tls_private_key = /tls.key",
            "tls_certificate = /tls.crt",
            "tls_private_key = /tls.key",
            "audio_tos = 0xb8\n        audio_dscp = EF",
        ] {
            let input = CONFIG.replace(
                "bind = 0.0.0.0:2000\n        advertised_address = 192.0.2.10",
                settings,
            );
            let error = ModuleConfig::parse(&input).unwrap_err().to_string();
            assert!(error.contains("[general]"), "{settings} produced {error}");
            assert!(error.contains("expected"), "{settings} produced {error}");
        }

        let input = CONFIG.replace(
            "description = Reception",
            "description = Reception\n        transport = tls",
        );
        let error = ModuleConfig::parse(&input).unwrap_err().to_string();
        assert!(error.contains("[SEP001122334455].transport"));
        assert!(error.contains("configured general TLS listener"));
    }

    #[test]
    fn tls_errors_report_locations_without_leaking_private_paths() {
        let secret = "/do/not/expose/private-server-key.pem";
        let input = CONFIG.replace(
            "advertised_address = 192.0.2.10",
            &format!("advertised_address = 192.0.2.10\n        tls_private_key = {secret}"),
        );
        let error = ModuleConfig::parse(&input).unwrap_err().to_string();

        assert!(error.contains("line 2 [general]"));
        assert!(error.contains("expected tls_certificate together with tls_private_key"));
        assert!(error.contains("<redacted>"));
        assert!(!error.contains(secret));
    }

    #[test]
    fn tls_policy_debug_output_redacts_credential_paths() {
        let listener = TlsListener {
            bind: "127.0.0.1:2443".parse().unwrap(),
            credentials: TlsCredentials::SplitPem {
                certificate: PathBuf::from("/private/server-certificate.pem"),
                private_key: PathBuf::from("/private/server-key.pem"),
                trust_store: Some(PathBuf::from("/private/client-roots.pem")),
            },
        };

        let debug = format!("{listener:?}");
        assert!(debug.contains("<redacted>"));
        for private in ["server-certificate", "server-key", "client-roots"] {
            assert!(!debug.contains(private), "debug leaked {private}");
        }
    }

    #[test]
    fn inherited_errors_retain_the_original_template_location() {
        let input = CONFIG.replace(
            "[SEP001122334455]",
            "[bad-network-template](!)\n        type = device\n        audio_cos = 9\n\n        [SEP001122334455](bad-network-template)",
        );
        let error = ModuleConfig::parse(&input).unwrap_err().to_string();

        assert!(error.contains("[bad-network-template].audio_cos"));
        assert!(!error.contains("[SEP001122334455].audio_cos"));
        assert!(error.contains("expected COS priority 0..7"));
    }

    #[test]
    fn rejects_obsolete_and_wrong_scope_network_options_with_guidance() {
        for (scope, setting, guidance) in [
            (
                "general",
                "trustphoneip = yes",
                "peer addresses are always authoritative",
            ),
            (
                "device",
                "trustphoneip = yes",
                "peer addresses are always authoritative",
            ),
            ("device", "dtmfmode = rfc2833", "use force_dtmfmode"),
            ("line", "permit = 192.0.2.0/24", "unknown variant"),
            ("line", "audio_dscp = EF", "unknown variant"),
        ] {
            let input = match scope {
                "general" => CONFIG.replace(
                    "advertised_address = 192.0.2.10",
                    &format!("advertised_address = 192.0.2.10\n        {setting}"),
                ),
                "device" => CONFIG.replace(
                    "description = Reception",
                    &format!("description = Reception\n        {setting}"),
                ),
                "line" => CONFIG.replace(
                    "label = Reception",
                    &format!("label = Reception\n        {setting}"),
                ),
                _ => unreachable!(),
            };
            let error = ModuleConfig::parse(&input).unwrap_err().to_string();
            assert!(error.contains("line "), "{error}");
            assert!(error.contains(guidance), "{error}");
        }
    }

    #[test]
    fn permits_one_logical_line_on_multiple_devices() {
        let config = format!("{CONFIG}\n[SEP112233445566]\ntype=device\nline=1001\n");
        let config = ModuleConfig::parse(&config).unwrap();
        let first = DeviceId::new("SEP001122334455").unwrap();
        let second = DeviceId::new("SEP112233445566").unwrap();

        assert_eq!(config.line_appearance_count("1001"), 2);
        assert_eq!(config.appearances_for_line("1001").count(), 2);
        assert_eq!(
            config
                .dial_target("SEP001122334455/1001")
                .unwrap()
                .device_id,
            first
        );
        assert_eq!(
            config
                .dial_target("SEP112233445566/1001")
                .unwrap()
                .device_id,
            second
        );
    }

    #[test]
    fn resolves_multilevel_device_and_line_templates_before_typing() {
        let input = r#"
            [general]
            advertised_address = 192.0.2.10

            [desk-keys]
            type = softkey_profile
            on_hook = redial, new_call

            [device-base](!)
            type = device
            description = Base phone
            softkey_profile = desk-keys
            button = speed_dial, Helpdesk, 2000

            [device-model](!, device-base)
            description = Model phone
            button = blf, Warehouse, 2001, 2001@internal

            [device-site](!)
            type = device
            description = Site phone
            button = empty

            [SEP001122334455](device-model, device-site)
            description = Reception phone
            button = line, 1001

            [SEP112233445566](device-model, device-site)
            button = line, 1001, label=Shared side desk

            [line-base](!)
            type = line
            label = Base line
            context = from-base
            callerid = "Base caller" <91001>
            mailbox = base@default

            [line-site](!, line-base)
            context = from-site
            callerid = "Site caller" <92001>

            [1001](line-site)
            label = Reception
            mailbox = 1001@default
        "#;

        let config = ModuleConfig::parse(input).unwrap();
        assert_eq!(config.devices.len(), 2);
        assert_eq!(config.lines.len(), 1);
        let first = config
            .devices
            .get(&DeviceId::new("SEP001122334455").unwrap())
            .unwrap();
        assert_eq!(first.description, "Reception phone");
        assert_eq!(first.soft_key_profile, "desk-keys");
        assert!(matches!(
            &first.buttons[0],
            ButtonDefinition::SpeedDial(speed) if speed.instance == 1 && speed.number == "2000"
        ));
        assert!(matches!(
            &first.buttons[1],
            ButtonDefinition::BlfSpeedDial(speed)
                if speed.instance == 2 && speed.number == "2001"
        ));
        assert!(matches!(&first.buttons[2], ButtonDefinition::Unused));
        assert!(matches!(
            &first.buttons[3],
            ButtonDefinition::Line(line) if line.instance == 1 && line.number == "1001"
        ));

        let second = config
            .devices
            .get(&DeviceId::new("SEP112233445566").unwrap())
            .unwrap();
        assert_eq!(second.description, "Site phone");
        assert!(matches!(
            &second.buttons[3],
            ButtonDefinition::Line(line)
                if line.instance == 1 && line.label.as_deref() == Some("Shared side desk")
        ));

        let line = config.lines.get("1001").unwrap();
        assert_eq!(line.label, "Reception");
        assert_eq!(line.context, "from-site");
        assert_eq!(line.caller_name, "Site caller");
        assert_eq!(line.caller_number, "92001");
        assert_eq!(line.mailbox.as_deref(), Some("1001@default"));
    }

    #[test]
    fn inheritance_rejects_cycles_missing_and_invalid_parents() {
        let cycle = r#"
            [device-a](!, device-b)
            type = device
            [device-b](!, device-c)
            [device-c](!, device-a)
        "#;
        assert!(matches!(
            ModuleConfig::parse(cycle),
            Err(ConfigError::InheritanceCycle(path))
                if path == "device-a -> device-b -> device-c -> device-a"
        ));

        let missing = r#"
            [SEP001122334455](missing-device)
            button = line, 1001
            [1001]
            type = line
        "#;
        assert!(matches!(
            ModuleConfig::parse(missing),
            Err(ConfigError::MissingTemplate { section, parent })
                if section == "SEP001122334455" && parent == "missing-device"
        ));

        let concrete_parent = r#"
            [SEP112233445566]
            type = device
            button = line, 1001
            [SEP001122334455](SEP112233445566)
            button = line, 1001
            [1001]
            type = line
        "#;
        assert!(matches!(
            ModuleConfig::parse(concrete_parent),
            Err(ConfigError::ParentIsNotTemplate { section, parent })
                if section == "SEP001122334455" && parent == "SEP112233445566"
        ));
    }

    #[test]
    fn inheritance_rejects_wrong_or_untyped_template_kinds() {
        let wrong_kind = r#"
            [line-defaults](!)
            type = line
            context = from-sccp
            [SEP001122334455](line-defaults)
            type = device
            button = line, 1001
            [1001]
            type = line
        "#;
        assert!(matches!(
            ModuleConfig::parse(wrong_kind),
            Err(ConfigError::WrongTemplateKind {
                section,
                child_kind,
                parent,
                parent_kind,
            }) if section == "SEP001122334455"
                && child_kind == "device"
                && parent == "line-defaults"
                && parent_kind == "line"
        ));

        let mixed_parents = r#"
            [device-defaults](!)
            type = device
            [line-defaults](!)
            type = line
            [mixed](!, device-defaults, line-defaults)
        "#;
        assert!(matches!(
            ModuleConfig::parse(mixed_parents),
            Err(ConfigError::WrongTemplateKind {
                section,
                child_kind,
                parent,
                parent_kind,
            }) if section == "mixed"
                && child_kind == "device"
                && parent == "line-defaults"
                && parent_kind == "line"
        ));

        let untyped = "[defaults](!)\ndescription = no kind\n";
        assert!(matches!(
            ModuleConfig::parse(untyped),
            Err(ConfigError::InvalidTemplateKind { section, kind })
                if section == "defaults" && kind == "missing"
        ));
    }

    #[test]
    fn inheritance_header_rejects_duplicate_and_empty_entries() {
        for (header, message) in [
            ("[child](!, base, BASE)", "duplicate parent template [BASE]"),
            ("[child](!, !)", "duplicate template marker"),
            ("[child]()", "empty inheritance entry"),
            ("[child](base, )", "empty inheritance entry"),
        ] {
            assert!(matches!(
                parse_sections(header),
                Err(ConfigError::Syntax { message: actual, .. }) if actual == message
            ));
        }
    }

    #[test]
    fn rejects_unknown_options() {
        let config = CONFIG.replace("keepalive = 30", "unknown = value");
        // Add a guaranteed unknown because the fixture relies on the default keepalive.
        let config = config.replace("bind = 0.0.0.0:2000", "bind = 0.0.0.0:2000\nwat = no");
        assert!(matches!(
            ModuleConfig::parse(&config),
            Err(ConfigError::InvalidValue { key, value })
                if key.contains("[general].wat") && value.contains("expected")
        ));
    }

    #[test]
    fn parses_typed_device_feature_defaults() {
        let input = CONFIG.replace(
            "description = Reception\n        line = 1001",
            r#"description = Reception
            cfwdall = no
            forward_busy_enabled = yes
            cfwdnoanswer = on
            forward_no_answer_timeout = 45
            forward_all = 2000
            forward_busy = none
            forward_no_answer = 2001
            dnd_feature = no
            dnd = reject
            privacy_feature = yes
            privacy = on
            button = feature, Do not disturb, dnd
            button = feature, Forward all, forward_all
            feature_default = 2, yes
            line = 1001"#,
        );
        let config = ModuleConfig::parse(&input).unwrap();
        let device_id = DeviceId::new("SEP001122334455").unwrap();
        let defaults = config.feature_defaults_for_device(&device_id).unwrap();

        assert!(!defaults.forwarding.all_enabled);
        assert!(defaults.forwarding.busy_enabled);
        assert!(defaults.forwarding.no_answer_enabled);
        assert_eq!(defaults.forwarding.no_answer_timeout_seconds, 45);
        assert_eq!(
            defaults
                .forwarding
                .all
                .as_ref()
                .map(ForwardingDestination::as_str),
            Some("2000")
        );
        assert_eq!(defaults.forwarding.busy, None);
        assert_eq!(
            defaults
                .forwarding
                .no_answer
                .as_ref()
                .map(ForwardingDestination::as_str),
            Some("2001")
        );
        assert!(!defaults.dnd_enabled);
        assert_eq!(defaults.dnd, DndMode::Reject);
        assert!(defaults.privacy_enabled);
        assert!(defaults.privacy);
        assert_eq!(defaults.buttons, HashMap::from([(1, false), (2, true)]));
    }

    #[test]
    fn dnd_feature_button_modes_are_typed_and_canonical() {
        let input = CONFIG.replace(
            "description = Reception",
            r#"description = Reception
            button = feature, Cycle DND, dnd
            button = feature, Silent DND, dnd, silent
            button = feature, Reject DND, dnd, busy"#,
        );
        let config = ModuleConfig::parse(&input).unwrap();
        let device = DeviceId::new("SEP001122334455").unwrap();
        assert_eq!(
            [1, 2, 3].map(|instance| config.dnd_button_mode(&device, instance)),
            [
                Some(DndButtonMode::Cycle),
                Some(DndButtonMode::Silent),
                Some(DndButtonMode::Reject),
            ]
        );
        assert_eq!(
            config.dnd_buttons_for_device(&device).collect::<Vec<_>>(),
            [
                (1, DndButtonMode::Cycle),
                (2, DndButtonMode::Silent),
                (3, DndButtonMode::Reject),
            ]
        );
        assert_eq!(
            config.devices[&device]
                .feature_arguments
                .get(&3)
                .map(String::as_str),
            Some("reject")
        );

        let invalid = CONFIG.replace(
            "description = Reception",
            "description = Reception\n        button = feature, DND, dnd, invented",
        );
        let error = ModuleConfig::parse(&invalid).unwrap_err();
        assert!(
            matches!(
                &error,
                ConfigError::InvalidValue { key, value }
                    if key == "line 12 [SEP001122334455].button"
                        && value == "\"invented\"; expected silent or reject"
            ),
            "unexpected error: {error:?}"
        );
    }

    #[test]
    fn parses_voicemail_and_pickup_groups_into_line_features() {
        let input = CONFIG.replace(
            "mailbox = 1001@default",
            r#"mailbox = 1001@default
            voicemail_number = 600
            voicemail_transfer = 61001
            call_group = 0, 2-4, 63
            pickup_group = 1, 5-6
            named_call_group = reception, front desk
            named_pickup_group = sales, support
            directed_pickup = no
            directed_pickup_context = pickup-internal
            pickup_mode_answer = off"#,
        );
        let config = ModuleConfig::parse(&input).unwrap();
        let line = config.lines.get("1001").unwrap();
        let features = config.features_for_line("1001").unwrap();

        assert_eq!(line.mailbox.as_deref(), Some("1001@default"));
        assert_eq!(
            features
                .voicemail
                .number
                .as_ref()
                .map(|value| value.as_str()),
            Some("600")
        );
        assert_eq!(
            features
                .voicemail
                .transfer_destination
                .as_ref()
                .map(|value| value.as_str()),
            Some("61001")
        );
        assert_eq!(
            features
                .voicemail
                .divert_destination()
                .map(VoicemailDestination::as_str),
            Some("61001")
        );
        assert_eq!(
            features.pickup.call_groups,
            BTreeSet::from([0, 2, 3, 4, 63])
        );
        assert_eq!(features.pickup.pickup_groups, BTreeSet::from([1, 5, 6]));
        assert_eq!(
            features.pickup.named_call_groups,
            BTreeSet::from(["front desk".into(), "reception".into()])
        );
        assert_eq!(
            features.pickup.named_pickup_groups,
            BTreeSet::from(["sales".into(), "support".into()])
        );
        assert!(!features.pickup.directed);
        assert_eq!(
            features.pickup.directed_context.as_deref(),
            Some("pickup-internal")
        );
        assert!(!features.pickup.answer_directed);
    }

    #[test]
    fn divert_actions_require_trnsfvm_and_never_fall_back_to_vmnum() {
        let voicemail = VoicemailDefaults {
            number: Some(VoicemailDestination::new("private-mailbox").unwrap()),
            transfer_destination: None,
        };
        assert!(voicemail.divert_destination().is_none());

        let voicemail = VoicemailDefaults {
            number: Some(VoicemailDestination::new("private-mailbox").unwrap()),
            transfer_destination: Some(VoicemailDestination::new("private-divert-target").unwrap()),
        };
        assert_eq!(
            voicemail
                .divert_destination()
                .map(VoicemailDestination::as_str),
            Some("private-divert-target")
        );
        assert!(!format!("{voicemail:?}").contains("private-divert-target"));
    }

    #[test]
    fn feature_and_pickup_defaults_follow_template_merge_semantics() {
        let input = r#"
            [general]
            advertised_address = 192.0.2.10

            [device-base](!)
            type = device
            forward_all = 2000
            dnd = silent
            button = feature, Do not disturb, dnd
            feature_default = 1, yes

            [device-child](!, device-base)
            forward_all = none
            dnd = reject
            button = feature, Forward all, forward_all
            feature_default = 1, no
            feature_default = 2, yes

            [SEP001122334455](device-child)
            button = line, 1001

            [line-base](!)
            type = line
            context = from-sccp
            vmnum = 600
            callgroup = 1-3
            namedpickupgroup = sales
            directed_pickup_context = inherited-pickup

            [line-child](!, line-base)
            voicemail_number = 700
            call_group =
            named_pickup_group = support
            directed_pickup_context = none

            [1001](line-child)
            label = Reception
        "#;
        let config = ModuleConfig::parse(input).unwrap();
        let device_id = DeviceId::new("SEP001122334455").unwrap();
        let defaults = config.feature_defaults_for_device(&device_id).unwrap();
        assert_eq!(defaults.forwarding.all, None);
        assert_eq!(defaults.dnd, DndMode::Reject);
        assert_eq!(defaults.buttons, HashMap::from([(1, false), (2, true)]));

        let line = config.features_for_line("1001").unwrap();
        assert_eq!(
            line.voicemail.number.as_ref().map(|value| value.as_str()),
            Some("700")
        );
        assert!(line.pickup.call_groups.is_empty());
        assert_eq!(
            line.pickup.named_pickup_groups,
            BTreeSet::from(["support".into()])
        );
        assert_eq!(line.pickup.directed_context, None);
    }

    #[test]
    fn parking_and_conference_defaults_are_fully_normalized() {
        let config = ModuleConfig::parse(CONFIG).unwrap();
        let device_id = DeviceId::new("SEP001122334455").unwrap();

        assert_eq!(
            config.general.conference_dialing,
            ConferenceDialingConfig::default()
        );
        assert_eq!(
            config.parking_for_device(&device_id),
            Some(&DeviceParkingConfig::default())
        );
        assert_eq!(
            config.conference_for_device(&device_id),
            Some(&DeviceConferenceConfig::default())
        );
        assert_eq!(
            config.parking_for_line("1001"),
            Some(&LineParkingConfig::default())
        );
        assert_eq!(
            config.conference_for_line("1001"),
            Some(&LineConferenceConfig::default())
        );
        assert_eq!(
            config
                .conference_dialing_for_appearance(&device_id, 1)
                .unwrap(),
            ResolvedConferenceDialing {
                enabled: true,
                destination: None,
                application_options: "qxd".into(),
            }
        );
    }

    #[test]
    fn parses_typed_parking_and_conference_policies_with_inheritance() {
        let input = r#"
            [general]
            advertised_address = 192.0.2.10
            meetme = no
            meetmeopts = qxd

            [device-base](!)
            type = device
            park = no
            conf_allow = no
            conf_music_on_hold_class =
            conf_play_general_announce = no
            conf_play_part_announce = no
            conf_mute_on_entry = yes
            conf_show_conflist = no
            meetme = no
            meetmeopts = qd
            button = feature, Main parking, parkinglot

            [device-site](!, device-base)
            park = yes
            conf_allow = yes
            conf_music_on_hold_class = office
            meetme = yes
            meetmeopts = Mac
            button = feature, Executive parking, parkinglot, executive, AlwaysShowMenu

            [SEP001122334455](device-site)
            line = 1001

            [line-base](!)
            type = line
            context = from-sccp
            parkinglot = default
            meetme = yes
            meetmenum = 700
            meetmeopts = qxd

            [1001](line-base)
            parkinglot = executive
            meetmeopts = M(acme_bridge)
        "#;
        let config = ModuleConfig::parse(input).unwrap();
        let device_id = DeviceId::new("SEP001122334455").unwrap();
        let parking = config.parking_for_device(&device_id).unwrap();

        assert!(parking.enabled);
        assert_eq!(
            parking.feature_buttons.get(&1),
            Some(&ParkingLotButtonConfig {
                lot: "default".into(),
                retrieval: ParkingRetrievalBehavior::RetrieveSingle,
            })
        );
        assert_eq!(
            config.parking_lot_for_button(&device_id, 2),
            Some(&ParkingLotButtonConfig {
                lot: "executive".into(),
                retrieval: ParkingRetrievalBehavior::AlwaysShowMenu,
            })
        );
        assert_eq!(
            config.parking_for_line("1001").unwrap().lot.as_deref(),
            Some("executive")
        );

        let conference = config.conference_for_device(&device_id).unwrap();
        assert!(conference.allowed);
        assert_eq!(conference.music_on_hold_class.as_deref(), Some("office"));
        assert!(!conference.play_general_announcements);
        assert!(!conference.play_participant_announcements);
        assert!(conference.mute_on_entry);
        assert!(!conference.show_conference_list);
        assert_eq!(
            conference.dialing,
            ConferenceDialingConfig {
                enabled: true,
                application_options: "Mac".into(),
            }
        );

        let line = config.conference_for_line("1001").unwrap();
        assert_eq!(line.enabled, Some(true));
        assert_eq!(line.destination.as_deref(), Some("700"));
        assert_eq!(line.application_options.as_deref(), Some("M(acme_bridge)"));
        assert_eq!(
            config
                .conference_dialing_for_appearance(&device_id, 1)
                .unwrap(),
            ResolvedConferenceDialing {
                enabled: true,
                destination: Some("700".into()),
                application_options: "M(acme_bridge)".into(),
            }
        );
    }

    #[test]
    fn empty_parking_and_conference_strings_have_exact_clear_semantics() {
        let input = CONFIG
            .replace(
                "description = Reception",
                "description = Reception\n        conf_music_on_hold_class =",
            )
            .replace(
                "mailbox = 1001@default",
                "mailbox = 1001@default\n        parkinglot =\n        meetmeopts =",
            );
        let config = ModuleConfig::parse(&input).unwrap();
        let device_id = DeviceId::new("SEP001122334455").unwrap();

        assert_eq!(
            config
                .conference_for_device(&device_id)
                .unwrap()
                .music_on_hold_class,
            None
        );
        assert_eq!(config.parking_for_line("1001").unwrap().lot, None);
        assert_eq!(
            config
                .conference_for_line("1001")
                .unwrap()
                .application_options
                .as_deref(),
            Some("")
        );
        assert_eq!(
            config
                .conference_dialing_for_appearance(&device_id, 1)
                .unwrap()
                .application_options,
            ""
        );
    }

    #[test]
    fn rejects_malformed_parking_retrieval_behavior() {
        for button in [
            "button = feature, Parking, parkinglot, default, SometimesShowMenu",
            "button = feature, Parking, parkinglot, default, RetrieveSingle, extra",
            "button = feature, Parking, parkinglot, , RetrieveSingle",
            "button = feature, Parking, parkinglot, default,",
        ] {
            let input = CONFIG.replace("line = 1001", &format!("{button}\n        line = 1001"));
            assert!(
                matches!(
                    ModuleConfig::parse(&input),
                    Err(ConfigError::InvalidValue { .. })
                ),
                "accepted {button}"
            );
        }
    }

    #[test]
    fn rejects_invalid_or_contradictory_conference_settings() {
        for setting in [
            "conf_allow = perhaps",
            "conf_play_general_announce = perhaps",
            "conf_play_part_announce = perhaps",
            "conf_mute_on_entry = perhaps",
            "conf_show_conflist = perhaps",
            "meetme = perhaps",
            "conf_allow = yes\n        conf-allow = no",
        ] {
            let input = CONFIG.replace(
                "description = Reception",
                &format!("description = Reception\n        {setting}"),
            );
            assert!(
                matches!(
                    ModuleConfig::parse(&input),
                    Err(ConfigError::InvalidValue { .. })
                ),
                "accepted {setting}"
            );
        }

        for setting in [
            "meetme = yes",
            "meetme = no\n        meetmenum = 700",
            "meetme = no\n        meetmeopts = Mac",
        ] {
            let input = CONFIG.replace(
                "mailbox = 1001@default",
                &format!("mailbox = 1001@default\n        {setting}"),
            );
            assert!(
                matches!(
                    ModuleConfig::parse(&input),
                    Err(ConfigError::InvalidValue { .. })
                ),
                "accepted {setting}"
            );
        }

        let inherited_clear = r#"
            [general]
            advertised_address = 192.0.2.10
            [SEP001122334455]
            type = device
            line = 1001
            [line-base](!)
            type = line
            meetme = yes
            meetmenum = 700
            [1001](line-base)
            meetmenum =
        "#;
        assert!(matches!(
            ModuleConfig::parse(inherited_clear),
            Err(ConfigError::InvalidValue { key, value })
                if key.contains("[1001].meetmenum") && value.contains("expected")
        ));
    }

    #[test]
    fn auto_answer_hotline_and_media_defaults_are_normalized() {
        let config = ModuleConfig::parse(CONFIG).unwrap();
        let device_id = DeviceId::new("SEP001122334455").unwrap();

        assert_eq!(config.auto_answer(), &AutoAnswerConfig::default());
        assert_eq!(config.guest_hotline(), &GuestHotlineConfig::default());
        assert_eq!(config.general.jitter_buffer, JitterBufferConfig::default());
        assert_eq!(
            config.hotline_for_line("1001"),
            Some(&LineHotlineConfig::default())
        );
        assert_eq!(
            config.media_for_device(&device_id).unwrap(),
            &DeviceMediaConfig {
                codecs: config.general.codecs.clone(),
                audio_encryption: MediaEncryptionPolicy::default(),
                dtmf_mode: DtmfMode::Auto,
                direct_media: false,
                early_media: true,
            }
        );
        assert_eq!(
            config.media_for_line("1001").unwrap(),
            &LineMediaConfig {
                codecs: config.general.codecs.clone(),
                audio_encryption: MediaEncryptionPolicy::default(),
                video_mode: VideoMode::Auto,
                audio_processing: AudioProcessingPolicy::default(),
            }
        );
        assert_eq!(
            config.media_for_appearance(&device_id, 1).unwrap(),
            ResolvedMediaConfig {
                codecs: config.general.codecs,
                audio_encryption: MediaEncryptionPolicy::default(),
                dtmf_mode: DtmfMode::Auto,
                direct_media: false,
                early_media: true,
                video_mode: VideoMode::Auto,
                audio_processing: AudioProcessingPolicy::default(),
            }
        );
    }

    #[test]
    fn echo_cancellation_and_silence_suppression_resolve_per_line() {
        let config = ModuleConfig::parse(
            r#"
            [general]
            advertised_address = 192.0.2.10
            echocancel = no
            silencesuppression = yes

            [line-base](!)
            type = line
            echocancel = yes

            [1001](line-base)
            silencesuppression = no

            [1002]
            type = line

            [SEP001122334455]
            type = device
            line = 1001
            line = 1002
            "#,
        )
        .unwrap();
        let device = DeviceId::new("SEP001122334455").unwrap();

        assert_eq!(
            config.general.audio_processing,
            AudioProcessingPolicy {
                echo_cancellation: EchoCancellation::Off,
                silence_suppression: SilenceSuppression::On,
            }
        );
        assert_eq!(
            config
                .media_for_appearance(&device, 1)
                .unwrap()
                .audio_processing,
            AudioProcessingPolicy {
                echo_cancellation: EchoCancellation::On,
                silence_suppression: SilenceSuppression::Off,
            }
        );
        assert_eq!(
            config
                .media_for_appearance(&device, 2)
                .unwrap()
                .audio_processing,
            config.general.audio_processing
        );

        for invalid in [
            "[general]\nadvertised_address = 192.0.2.10\nechocancel = maybe",
            "[general]\nadvertised_address = 192.0.2.10\nsilencesuppression = yes\nsilencesuppression = no",
            "[general]\nadvertised_address = 192.0.2.10\n[1001]\ntype = line\nechocancel = maybe",
            "[general]\nadvertised_address = 192.0.2.10\n[1001]\ntype = line\n[SEP001122334455]\ntype = device\nline = 1001\nechocancel = yes",
        ] {
            assert!(
                matches!(
                    ModuleConfig::parse(invalid),
                    Err(ConfigError::InvalidValue { .. })
                ),
                "accepted invalid audio-processing policy: {invalid}"
            );
        }
    }

    #[test]
    fn parses_exact_global_jitter_buffer_policy() {
        let config = ModuleConfig::parse(
            r#"
            [general]
            advertised_address = 192.0.2.10
            jbenable = yes
            jbforce = yes
            jblog = yes
            jbmaxsize = 320
            jbresyncthreshold = 1500
            jbimpl = adaptive

            [1001]
            type = line

            [SEP001122334455]
            type = device
            line = 1001
            "#,
        )
        .unwrap();

        assert_eq!(
            config.general.jitter_buffer,
            JitterBufferConfig {
                enabled: true,
                forced: true,
                log_frames: true,
                max_size_ms: 320,
                resync_threshold_ms: 1_500,
                implementation: JitterBufferImplementation::Adaptive,
            }
        );

        let forced_without_enabled = ModuleConfig::parse(
            "[general]\nadvertised_address = 192.0.2.10\njbforce = yes\n\
             [1001]\ntype = line\n\
             [SEP001122334455]\ntype = device\nline = 1001",
        )
        .unwrap();
        assert!(!forced_without_enabled.general.jitter_buffer.enabled);
        assert!(forced_without_enabled.general.jitter_buffer.forced);

        let mut policy = JitterBufferConfig::default();
        assert!(!policy.should_configure_channel(false));
        policy.enabled = true;
        assert!(policy.should_configure_channel(false));
        assert!(!policy.should_configure_channel(true));
        policy.forced = true;
        assert!(policy.should_configure_channel(true));
        policy.enabled = false;
        assert!(!policy.should_configure_channel(false));
    }

    #[test]
    fn rejects_invalid_scoped_or_invented_jitter_buffer_policy() {
        for invalid in [
            "[general]\nadvertised_address = 192.0.2.10\njbenable = maybe",
            "[general]\nadvertised_address = 192.0.2.10\njbmaxsize = 0",
            "[general]\nadvertised_address = 192.0.2.10\njbresyncthreshold = 2147483648",
            "[general]\nadvertised_address = 192.0.2.10\njbimpl = dynamic",
            "[general]\nadvertised_address = 192.0.2.10\njbenable = yes\njbenable = no",
            "[general]\nadvertised_address = 192.0.2.10\njbtargetextra = 40",
            "[general]\nadvertised_address = 192.0.2.10\n[1001]\ntype = line\njbenable = yes",
            "[general]\nadvertised_address = 192.0.2.10\n[1001]\ntype = line\n[SEP001122334455]\ntype = device\nline = 1001\njbforce = yes",
        ] {
            assert!(
                matches!(
                    ModuleConfig::parse(invalid),
                    Err(ConfigError::InvalidValue { .. })
                ),
                "accepted invalid jitter-buffer policy: {invalid}"
            );
        }
    }

    #[test]
    fn parses_auto_answer_guest_hotline_and_line_hotline() {
        let input = r#"
            [general]
            advertised_address = 192.0.2.10
            autoanswer_ring_time = 7
            autoanswer_tone = 0x31
            remotehangup_tone = 0
            hotline_enabled = yes
            hotline_extension = 9911
            hotline_context = emergency
            hotline_label = Emergency only

            [SEP001122334455]
            type = device
            line = 1001

            [line-base](!)
            type = line
            adhocNumber = 912

            [1001](line-base)
            adhoc_number = 911
        "#;
        let config = ModuleConfig::parse(input).unwrap();

        assert_eq!(
            config.auto_answer(),
            &AutoAnswerConfig {
                ring_time_seconds: 7,
                tone: Tone::ZipZip,
            }
        );
        assert_eq!(config.general.remote_hangup_tone, None);
        assert_eq!(
            config.guest_hotline(),
            &GuestHotlineConfig {
                enabled: true,
                extension: Some(HotlineDestination::new("9911").unwrap()),
                context: "emergency".into(),
                label: "Emergency only".into(),
            }
        );
        assert_eq!(
            config
                .hotline_for_line("1001")
                .unwrap()
                .destination
                .as_ref()
                .map(HotlineDestination::as_str),
            Some("911")
        );
        let debug = format!("{:?}", config.guest_hotline());
        assert!(!debug.contains("9911"));
        let debug = format!("{:?}", config.hotline_for_line("1001"));
        assert!(!debug.contains("911"));

        let configured_id = DeviceId::new("SEP001122334455").unwrap();
        let configured = config.line_for_device(&configured_id, 1).unwrap();
        assert_eq!(
            config
                .hotline_destination_for_binding(configured)
                .map(HotlineDestination::as_str),
            Some("911")
        );
        let guest_id = DeviceId::new("SEPFFEEDDCCBBAA").unwrap();
        let guest = config.guest_hotline_binding(&guest_id, 1).unwrap();
        assert_eq!(guest.line.number, "hotline");
        assert_eq!(guest.line.context, "emergency");
        assert_eq!(guest.appearance.display_label(), "Emergency only");
        assert_eq!(
            config
                .hotline_destination_for_binding(&guest)
                .map(HotlineDestination::as_str),
            Some("9911")
        );
        assert!(config.guest_hotline_binding(&guest_id, 2).is_none());
        assert!(config.guest_hotline_binding(&configured_id, 1).is_none());
    }

    #[test]
    fn disabled_guest_hotline_allows_cleared_identity_fields() {
        let input = CONFIG.replace(
            "advertised_address = 192.0.2.10",
            "advertised_address = 192.0.2.10\n        hotline_enabled = no\n        hotline_extension =\n        hotline_context =\n        hotline_label =",
        );
        let config = ModuleConfig::parse(&input).unwrap();

        assert_eq!(
            config.guest_hotline(),
            &GuestHotlineConfig {
                enabled: false,
                extension: None,
                context: "".into(),
                label: "".into(),
            }
        );
    }

    #[test]
    fn rejects_invalid_auto_answer_and_hotline_ranges() {
        for setting in [
            "autoanswer_ring_time = -1".to_owned(),
            "autoanswer_ring_time = 4294967296".to_owned(),
            "autoanswer_tone = teleport".to_owned(),
            "autoanswer_tone = 0x100".to_owned(),
            "remotehanguptone = Zip".to_owned(),
            "remotehangup_tone = teleport".to_owned(),
            "remotehangup_tone = Zip\nremotehangup_tone = ZipZip".to_owned(),
            "hotline_enabled = perhaps".to_owned(),
            "hotline_extension =".to_owned(),
            "hotline_context =".to_owned(),
            "hotline_label =".to_owned(),
            format!(
                "hotline_extension = {}",
                "1".repeat(MAX_HOTLINE_FIELD_BYTES + 1)
            ),
            format!(
                "hotline_context = {}",
                "c".repeat(MAX_HOTLINE_FIELD_BYTES + 1)
            ),
            format!(
                "hotline_label = {}",
                "l".repeat(MAX_HOTLINE_FIELD_BYTES + 1)
            ),
        ] {
            let input = CONFIG.replace(
                "advertised_address = 192.0.2.10",
                &format!("advertised_address = 192.0.2.10\n        {setting}"),
            );
            assert!(
                matches!(
                    ModuleConfig::parse(&input),
                    Err(ConfigError::InvalidValue { .. })
                ),
                "accepted {setting}"
            );
        }

        let oversized = "9".repeat(MAX_HOTLINE_FIELD_BYTES + 1);
        let input = CONFIG.replace(
            "mailbox = 1001@default",
            &format!("mailbox = 1001@default\n        adhocNumber = {oversized}"),
        );
        assert!(matches!(
            ModuleConfig::parse(&input),
            Err(ConfigError::InvalidValue { key, value })
                if key.contains("[1001].adhocNumber") && value.contains("expected")
        ));
    }

    #[test]
    fn parses_codec_dtmf_early_direct_and_video_policy_with_inheritance() {
        let input = r#"
            [general]
            advertised_address = 192.0.2.10
            disallow = all
            allow = ulaw, g729
            allow = h264
            directrtp = yes
            earlyrtp = none

            [device-base](!)
            type = device
            disallow = all
            allow = ulaw, g729
            force_dtmfmode = rfc2833
            directrtp = yes
            earlyrtp = none

            [device-site](!, device-base)
            disallow = g729
            allow = alaw
            force_dtmfmode = skinny
            directrtp = no
            earlyrtp = progress

            [SEP001122334455](device-site)
            line = 1001

            [line-base](!)
            type = line
            disallow = all
            allow = opus, h264
            videomode = auto

            [1001](line-base)
            videomode = user
        "#;
        let config = ModuleConfig::parse(input).unwrap();
        let device_id = DeviceId::new("SEP001122334455").unwrap();

        assert_eq!(
            config.general.codecs,
            [
                Codec::Pcmu,
                Codec::G711Ulaw56k,
                Codec::G729,
                Codec::G729A,
                Codec::G729B,
                Codec::G729Ab,
                Codec::G729AnnexB,
                Codec::H264,
                Codec::H264Svc,
                Codec::H264Fec,
                Codec::H264Uc,
            ]
        );
        assert!(config.general.direct_media);
        assert!(!config.general.early_media);

        let device = config.media_for_device(&device_id).unwrap();
        assert_eq!(
            device.codecs,
            [
                Codec::Pcmu,
                Codec::G711Ulaw56k,
                Codec::Pcma,
                Codec::G711Alaw56k,
            ]
        );
        assert_eq!(device.dtmf_mode, DtmfMode::Skinny);
        assert!(!device.direct_media);
        assert!(device.early_media);

        let line = config.media_for_line("1001").unwrap();
        assert_eq!(
            line.codecs,
            [
                Codec::Opus,
                Codec::H264,
                Codec::H264Svc,
                Codec::H264Fec,
                Codec::H264Uc,
            ]
        );
        assert_eq!(line.video_mode, VideoMode::User);
        assert_eq!(
            config.media_for_appearance(&device_id, 1).unwrap(),
            ResolvedMediaConfig {
                codecs: line.codecs.clone(),
                audio_encryption: MediaEncryptionPolicy::default(),
                dtmf_mode: DtmfMode::Skinny,
                direct_media: false,
                early_media: true,
                video_mode: VideoMode::User,
                audio_processing: AudioProcessingPolicy::default(),
            }
        );
    }

    #[test]
    fn general_media_defaults_apply_regardless_of_section_order() {
        let input = r#"
            [SEP001122334455]
            type = device
            line = 1001
            [1001]
            type = line
            [general]
            advertised_address = 192.0.2.10
            disallow = all
            allow = opus
            directrtp = yes
            earlyrtp = no
        "#;
        let config = ModuleConfig::parse(input).unwrap();
        let device_id = DeviceId::new("SEP001122334455").unwrap();

        assert_eq!(config.media_for_line("1001").unwrap().codecs, [Codec::Opus]);
        let device = config.media_for_device(&device_id).unwrap();
        assert_eq!(device.codecs, [Codec::Opus]);
        assert!(device.direct_media);
        assert!(!device.early_media);
    }

    #[test]
    fn appearance_codec_preferences_resolve_line_then_device_then_general() {
        let input = r#"
            [general]
            advertised_address = 192.0.2.10
            disallow = all
            allow = ulaw

            [SEP001122334455]
            type = device
            line = 1001
            line = 1002
            disallow = all
            allow = alaw

            [1001]
            type = line

            [1002]
            type = line
            disallow = all
            allow = g722
        "#;
        let config = ModuleConfig::parse(input).unwrap();
        let device_id = DeviceId::new("SEP001122334455").unwrap();

        assert_eq!(
            config.media_for_appearance(&device_id, 1).unwrap().codecs,
            [Codec::Pcma, Codec::G711Alaw56k]
        );
        assert_eq!(
            config.media_for_appearance(&device_id, 2).unwrap().codecs,
            [Codec::G72264k, Codec::G72256k, Codec::G72248k]
        );
    }

    #[test]
    fn audio_encryption_resolves_as_one_policy_line_then_device_then_general() {
        let input = r#"
            [general]
            advertised_address = 192.0.2.10
            audio_encryption = required,aes-128-hmac-sha1-80

            [SEP001122334455]
            type = device
            line = 1001
            line = 1002
            audio_encryption = optional,aead-aes-128-gcm

            [SEP001122334466]
            type = device
            line = 1003

            [1001]
            type = line

            [1002]
            type = line
            audio_encryption = required,aead-aes-256-gcm,aes-128-hmac-sha1-32

            [1003]
            type = line
        "#;
        let config = ModuleConfig::parse(input).unwrap();
        let first = DeviceId::new("SEP001122334455").unwrap();
        let second = DeviceId::new("SEP001122334466").unwrap();

        assert_eq!(
            config
                .media_for_appearance(&first, 1)
                .unwrap()
                .audio_encryption,
            MediaEncryptionPolicy::new(
                MediaEncryptionRequirement::Optional,
                [MediaEncryptionProfile::AEAD_AES_128_GCM]
            )
            .unwrap()
        );
        assert_eq!(
            config
                .media_for_appearance(&first, 2)
                .unwrap()
                .audio_encryption,
            MediaEncryptionPolicy::new(
                MediaEncryptionRequirement::Required,
                [
                    MediaEncryptionProfile::AEAD_AES_256_GCM,
                    MediaEncryptionProfile::AES_128_HMAC_SHA1_32,
                ]
            )
            .unwrap()
        );
        assert_eq!(
            config
                .media_for_appearance(&second, 1)
                .unwrap()
                .audio_encryption,
            MediaEncryptionPolicy::new(
                MediaEncryptionRequirement::Required,
                [MediaEncryptionProfile::AES_128_HMAC_SHA1_80]
            )
            .unwrap()
        );
    }

    #[test]
    fn audio_encryption_rejects_incomplete_or_unknown_policy() {
        for value in [
            "enabled",
            "off,aes-128-hmac-sha1-80",
            "optional",
            "required",
            "required,future-profile",
            "optional,aes-128-hmac-sha1-80,",
        ] {
            let input = CONFIG.replace(
                "advertised_address = 192.0.2.10",
                &format!("advertised_address = 192.0.2.10\n        audio_encryption = {value}"),
            );
            assert!(
                matches!(
                    ModuleConfig::parse(&input),
                    Err(ConfigError::InvalidValue { .. })
                ),
                "accepted {value}"
            );
        }
    }

    #[test]
    fn accepted_early_media_values_normalize_to_boolean_policy() {
        for (value, expected) in [
            ("yes", true),
            ("no", false),
            ("none", false),
            ("offhook", true),
            ("immediate", true),
            ("dial", true),
            ("ringout", true),
            ("progress", true),
        ] {
            let input = CONFIG.replace(
                "description = Reception",
                &format!("description = Reception\n        earlyrtp = {value}"),
            );
            let config = ModuleConfig::parse(&input).unwrap();
            let device_id = DeviceId::new("SEP001122334455").unwrap();
            assert_eq!(
                config.media_for_device(&device_id).unwrap().early_media,
                expected
            );
        }
    }

    #[test]
    fn rejects_invalid_or_unsafe_media_policy() {
        for setting in [
            "disallow = all".to_owned(),
            "disallow = all\n        allow = h264".to_owned(),
            "disallow = all\n        allow = unknown".to_owned(),
            "disallow = all\n        allow = ulaw,,alaw".to_owned(),
            "disallow = all\n        allow = all, g722".to_owned(),
            "directrtp = perhaps".to_owned(),
            "earlyrtp = perhaps".to_owned(),
        ] {
            let input = CONFIG.replace(
                "disallow = all\n        allow = ulaw\n        allow = alaw",
                &setting,
            );
            assert!(
                matches!(
                    ModuleConfig::parse(&input),
                    Err(ConfigError::InvalidValue { .. })
                ),
                "accepted {setting}"
            );
        }

        for setting in [
            "force_dtmfmode = inband",
            "dtmfmode = skinny",
            "earlyrtp = perhaps",
            "directrtp = perhaps",
        ] {
            let input = CONFIG.replace(
                "description = Reception",
                &format!("description = Reception\n        {setting}"),
            );
            assert!(
                matches!(
                    ModuleConfig::parse(&input),
                    Err(ConfigError::InvalidValue { .. })
                ),
                "accepted {setting}"
            );
        }

        let input = CONFIG.replace(
            "mailbox = 1001@default",
            "mailbox = 1001@default\n        videomode = immediate",
        );
        assert!(matches!(
            ModuleConfig::parse(&input),
            Err(ConfigError::InvalidValue { key, value })
                if key.contains("[1001].videomode") && value.contains("expected")
        ));
    }

    #[test]
    fn rejects_invalid_device_feature_defaults() {
        for setting in [
            "cfwdall = perhaps",
            "forward_no_answer_timeout = 0",
            "forward_no_answer_timeout = 86401",
            "dnd = user",
            "privacy = full",
            "feature_default = missing-fields",
            "feature_default = 0, yes",
            "feature_default = 2, yes",
        ] {
            let input = CONFIG.replace(
                "description = Reception",
                &format!(
                    "description = Reception\n        button = feature, DND, dnd\n        {setting}"
                ),
            );
            assert!(
                matches!(
                    ModuleConfig::parse(&input),
                    Err(ConfigError::InvalidValue { .. })
                ),
                "accepted {setting}"
            );
        }
    }

    #[test]
    fn rejects_invalid_voicemail_and_pickup_settings() {
        for setting in [
            "mailbox = @default",
            "mailbox = 1001@default@extra",
            "mailbox = desk one@default",
            "callgroup = 64",
            "callgroup = 4-2",
            "callgroup = 1,,2",
            "callgroup = 1,1",
            "namedcallgroup = sales,,support",
            "namedpickupgroup = sales,sales",
            "directed_pickup = perhaps",
            "pickup_mode_answer = perhaps",
        ] {
            let input = CONFIG.replace("mailbox = 1001@default", setting);
            assert!(
                matches!(
                    ModuleConfig::parse(&input),
                    Err(ConfigError::InvalidValue { .. })
                ),
                "accepted {setting}"
            );
        }
        for (setting, redacted) in [
            (format!("voicemail_number = {}", "6".repeat(80)), true),
            ("voicemail_transfer = 61\u{7}001".into(), false),
        ] {
            let input = CONFIG.replace("mailbox = 1001@default", &setting);
            let error = ModuleConfig::parse(&input).unwrap_err().to_string();
            if redacted {
                assert!(error.contains("<redacted>"));
                assert!(!error.contains(&"6".repeat(80)));
            }
        }
    }

    #[test]
    fn parses_ordered_mixed_button_layout() {
        let input = r#"
            [general]
            advertised_address = 192.0.2.10

            [SEP001122334455]
            type = device
            description = Reception
            button = line, 1001, label=Shared main, caller_name=Shared desk, caller_number=91001, ring=silent, subscription=1001@internal, privacy=yes
            button = empty
            button = speed_dial, Helpdesk, 2000
            button = blf, Warehouse, 2001, 2001@internal
            button = feature, Do not disturb, dnd, silent
            button = service, Directory, http://pbx.test/directory?view=all,compact
            button = addon, 1, 7914
            line = 1002

            [1001]
            type = line
            label = Main

            [1002]
            type = line
            label = Private
        "#;

        let config = ModuleConfig::parse(input).unwrap();
        let device = config
            .devices
            .get(&DeviceId::new("SEP001122334455").unwrap())
            .unwrap();
        assert_eq!(device.lines, ["1001", "1002"]);
        assert_eq!(
            device.feature_arguments.get(&1).map(String::as_str),
            Some("silent")
        );
        assert_eq!(
            config.dnd_button_mode(&device.id, 1),
            Some(DndButtonMode::Silent)
        );
        assert!(matches!(
            &device.buttons[0],
            ButtonDefinition::Line(line)
                if line.instance == 1
                    && line.number == "1001"
                    && line.display_name == "Main"
                    && line.label.as_deref() == Some("Shared main")
                    && line.caller_id.name.as_deref() == Some("Shared desk")
                    && line.caller_id.number.as_deref() == Some("91001")
                    && line.ring_mode == AppearanceRingMode::Silent
                    && line.subscription_identity.as_deref() == Some("1001@internal")
                    && line.privacy
        ));
        assert!(matches!(&device.buttons[1], ButtonDefinition::Unused));
        assert!(matches!(
            &device.buttons[2],
            ButtonDefinition::SpeedDial(speed_dial)
                if speed_dial.instance == 1
                    && speed_dial.display_name == "Helpdesk"
                    && speed_dial.number == "2000"
        ));
        assert!(matches!(
            &device.buttons[3],
            ButtonDefinition::BlfSpeedDial(blf)
                if blf.instance == 2 && blf.hint == "2001@internal"
        ));
        assert!(matches!(
            &device.buttons[4],
            ButtonDefinition::Feature(feature)
                if feature.instance == 1 && feature.feature == ButtonType::DoNotDisturb
        ));
        assert!(matches!(
            &device.buttons[5],
            ButtonDefinition::Service(service)
                if service.instance == 1
                    && service.url == "http://pbx.test/directory?view=all,compact"
        ));
        assert!(matches!(
            &device.buttons[6],
            ButtonDefinition::AddonModule(addon)
                if addon.slot == 1 && addon.device_type == DeviceType::CiscoAddon7914
        ));
        assert!(matches!(
            &device.buttons[7],
            ButtonDefinition::Line(line)
                if line.instance == 2 && line.number == "1002"
        ));
        assert_eq!(
            config.line_for_device(&device.id, 2).unwrap().line.number,
            "1002"
        );
        assert_eq!(
            config
                .appearances_for_device(&device.id)
                .map(|appearance| appearance.line.number.as_str())
                .collect::<HashSet<_>>(),
            HashSet::from(["1001", "1002"])
        );
        assert_eq!(config.device_definitions()[0].buttons, device.buttons);
    }

    #[test]
    fn speed_dial_hint_builds_a_blf_button() {
        let input = CONFIG.replace(
            "line = 1001",
            "button = line, 1001\nbutton = speeddial, Helpdesk, 2000, 2000@internal",
        );
        let buttons = &ModuleConfig::parse(&input).unwrap().device_definitions()[0].buttons;
        assert!(matches!(
            &buttons[1],
            ButtonDefinition::BlfSpeedDial(blf)
                if blf.instance == 1
                    && blf.number == "2000"
                    && blf.hint == "2000@internal"
        ));
    }

    #[test]
    fn parses_reusable_soft_key_profile_for_every_key_mode() {
        let input = CONFIG
            .replace(
                "[SEP001122334455]",
                r#"[Reception-Keys]
                type = softkey_profile
                on_hook = redial, new_call
                connected = hold, end_call, transfer
                on_hold = resume, new_call, end_call
                ring_in = answer, immediate_divert
                off_hook = end_call
                connected_transfer = direct_transfer, end_call
                digits_following = backspace, dial
                connected_conference = conference_list, join
                ring_out = callback, end_call
                off_hook_feature = pickup, group_pickup
                in_use_hint = barge
                on_hook_stealable = intercept, new_call
                hold_conference = select, conference
                empty =

                [SEP001122334455]"#,
            )
            .replace(
                "description = Reception",
                "description = Reception\n        softkey_profile = Reception-Keys",
            );

        let config = ModuleConfig::parse(&input).unwrap();
        let device_id = DeviceId::new("SEP001122334455").unwrap();
        let device = config.devices.get(&device_id).unwrap();
        assert_eq!(device.soft_key_profile, "reception-keys");
        let profile = config.soft_key_profile_for_device(&device_id).unwrap();
        assert_eq!(profile.name, "reception-keys");
        assert_eq!(profile.sets.len(), KeyMode::ALL_KNOWN.len());
        assert_eq!(
            profile.actions(KeyMode::OnHook),
            [SoftKey::Redial, SoftKey::NewCall]
        );
        assert_eq!(
            profile.actions(KeyMode::ConnectedTransfer),
            [SoftKey::DirectTransfer, SoftKey::EndCall]
        );
        assert_eq!(
            profile.actions(KeyMode::OffHookFeature),
            [SoftKey::Pickup, SoftKey::GroupPickup]
        );
        assert!(profile.actions(KeyMode::Empty).is_empty());
        assert_eq!(config.soft_key_profile("RECEPTION-KEYS"), Some(profile));
        let station = config.device_definitions().remove(0);
        assert_eq!(
            station.soft_keys.actions(KeyMode::OnHook),
            [SoftKey::Redial, SoftKey::NewCall]
        );
        assert_eq!(
            station.soft_keys.actions(KeyMode::ConnectedTransfer),
            [SoftKey::DirectTransfer, SoftKey::EndCall]
        );
    }

    #[test]
    fn parses_every_named_soft_key_in_declared_order() {
        let names = [
            "redial",
            "new_call",
            "hold",
            "transfer",
            "forward_all",
            "forward_busy",
            "forward_no_answer",
            "backspace",
            "end_call",
            "resume",
            "answer",
            "info",
            "conference",
            "park",
            "join",
            "meet_me",
            "pickup",
            "group_pickup",
            "monitor",
            "callback",
            "barge",
            "do_not_disturb",
            "conference_list",
            "select",
            "private",
            "transfer_to_voicemail",
            "direct_transfer",
            "immediate_divert",
            "video_mode",
            "intercept",
            "empty",
            "dial",
        ];
        let input = CONFIG.replace(
            "[SEP001122334455]",
            &format!(
                "[all-actions]\ntype = softkey_profile\non_hook = {}\nconnected = {}\n\n[SEP001122334455]",
                names[..16].join(", "),
                names[16..].join(", ")
            ),
        );
        let config = ModuleConfig::parse(&input).unwrap();
        let profile = config.soft_key_profile("all-actions").unwrap();
        let actions: Vec<_> = profile
            .actions(KeyMode::OnHook)
            .iter()
            .chain(profile.actions(KeyMode::Connected))
            .copied()
            .collect();
        assert_eq!(actions, SoftKey::ALL_KNOWN);
    }

    #[test]
    fn soft_key_profiles_reject_unknown_and_duplicate_entries() {
        for (setting, expected_key) in [
            ("type = softkey_profile", "[bad-keys].type"),
            ("waiting = answer", "[bad-keys].waiting"),
            ("on_hook = teleport", "[bad-keys].on_hook"),
            ("on_hook = new_call\non-hook = redial", "[bad-keys].on-hook"),
            ("on_hook = dnd, do_not_disturb", "[bad-keys].on_hook"),
            ("on_hook = hold, , transfer", "[bad-keys].on_hook"),
            (
                "on_hook = redial, new_call, hold, transfer, forward_all, forward_busy, forward_no_answer, backspace, end_call, resume, answer, info, conference, park, join, meet_me, pickup",
                "[bad-keys].on_hook",
            ),
        ] {
            let input = CONFIG.replace(
                "[SEP001122334455]",
                &format!("[bad-keys]\ntype = softkey_profile\n{setting}\n\n[SEP001122334455]"),
            );
            assert!(
                matches!(
                    ModuleConfig::parse(&input),
                    Err(ConfigError::InvalidValue { key, value })
                        if key.contains(expected_key) && value.contains("expected")
                ),
                "accepted {setting}"
            );
        }
    }

    #[test]
    fn soft_key_profile_references_are_required_to_resolve_once() {
        let unknown = CONFIG.replace(
            "description = Reception",
            "description = Reception\n        softkey_profile = missing",
        );
        assert!(matches!(
            ModuleConfig::parse(&unknown),
            Err(ConfigError::UnknownSoftKeyProfile { device, profile })
                if device.as_str() == "SEP001122334455" && profile == "missing"
        ));

        let duplicate = CONFIG.replace(
            "description = Reception",
            "description = Reception\n        softkey_profile = default\n        softkey_profile = DEFAULT",
        );
        assert!(matches!(
            ModuleConfig::parse(&duplicate),
            Err(ConfigError::InvalidValue { key, value })
                if key.contains("[SEP001122334455].softkey_profile")
                    && value.contains("expected")
        ));
    }

    #[test]
    fn configured_default_profile_replaces_the_builtin_default() {
        let input = CONFIG.replace(
            "[SEP001122334455]",
            "[default]\ntype = softkey_profile\non_hook = redial\n\n[SEP001122334455]",
        );
        let config = ModuleConfig::parse(&input).unwrap();
        let device_id = DeviceId::new("SEP001122334455").unwrap();

        assert_eq!(
            config
                .soft_key_profile_for_device(&device_id)
                .unwrap()
                .actions(KeyMode::OnHook),
            [SoftKey::Redial]
        );
    }

    #[test]
    fn rejects_malformed_and_unknown_buttons() {
        for button in [
            "button = speed_dial, Missing number",
            "button = line, 1001, ring=occasionally",
            "button = line, 1001, privacy=perhaps",
            "button = line, 1001, label=One, label=Two",
            "button = blf, Desk, 2000",
            "button = blf, Desk, 2000, missing-context",
            "button = feature, DND, unknown-feature",
            "button = service, Directory",
            "button = empty, extra",
            "button = addon, 0, 7914",
            "button = addon, 57, 7914",
        ] {
            let input = CONFIG.replace("line = 1001", &format!("line = 1001\n{button}"));
            assert!(
                matches!(
                    ModuleConfig::parse(&input),
                    Err(ConfigError::InvalidValue { .. })
                ),
                "accepted {button}"
            );
        }
    }

    #[test]
    fn rejects_duplicate_lines_and_oversized_button_layouts() {
        let duplicate = CONFIG.replace("line = 1001", "line = 1001\nbutton = line, 1001");
        assert!(matches!(
            ModuleConfig::parse(&duplicate),
            Err(ConfigError::InvalidValue { key, value })
                if key == "SEP001122334455.line" && value == "1001"
        ));

        let duplicate_addon = CONFIG.replace(
            "line = 1001",
            "line = 1001\nbutton = addon, 1, 7914\nbutton = addon, 1, 7914",
        );
        assert!(matches!(
            ModuleConfig::parse(&duplicate_addon),
            Err(ConfigError::InvalidValue { key, value })
                if key.contains("[SEP001122334455].button")
                    && value.contains("repeats addon module button instance 1")
                    && value.contains("expected")
        ));

        let empty_buttons = "button = empty\n".repeat(56);
        let oversized = CONFIG.replace(
            "line = 1001",
            &format!("button = line, 1001\n{empty_buttons}"),
        );
        assert!(matches!(
            ModuleConfig::parse(&oversized),
            Err(ConfigError::InvalidValue { key, value })
                if key.contains("[SEP001122334455].button")
                && value.contains("protocol limit is 42")
                    && value.contains("expected")
        ));
    }

    #[test]
    fn realtime_table_pair_normalizes_without_changing_file_sections() {
        let input = CONFIG.replace(
            "advertised_address = 192.0.2.10",
            "advertised_address = 192.0.2.10\n        devicetable = sccp_devices\n        linetable = sccp_lines",
        );
        let config = ModuleConfig::parse(&input).unwrap();

        assert_eq!(
            config.realtime_tables(),
            Some(&RealtimeTableConfig {
                device_family: "sccp_devices".into(),
                line_family: "sccp_lines".into(),
            })
        );
        assert_eq!(config.devices.len(), 1);
        assert_eq!(config.lines.len(), 1);
    }

    #[test]
    fn realtime_table_pair_is_complete_distinct_and_safely_named() {
        for settings in [
            "devicetable = sccp_devices",
            "linetable = sccp_lines",
            "devicetable = same\n        linetable = same",
            "devicetable = device-table\n        linetable = sccp_lines",
            "devicetable = \n        linetable = sccp_lines",
        ] {
            let input = CONFIG.replace(
                "advertised_address = 192.0.2.10",
                &format!("advertised_address = 192.0.2.10\n        {settings}"),
            );
            let error = ModuleConfig::parse(&input).unwrap_err().to_string();
            assert!(error.contains("line "), "{settings} produced {error}");
            assert!(error.contains("expected"), "{settings} produced {error}");
        }
    }

    #[test]
    fn canonical_schema_is_strict_while_runtime_matching_follows_asterisk_casing() {
        let mixed = CONFIG
            .replace("advertised_address =", "AdVeRtIsEd_AdDrEsS =")
            .replace("type = device", "TyPe = device")
            .replace("type = line", "TYPE = line");
        ModuleConfig::parse(&mixed).unwrap();
        let error = ModuleConfig::check_canonical(&mixed)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("canonical option name advertised_address"),
            "{error}"
        );

        let punctuation = CONFIG.replace(
            "advertised_address = 192.0.2.10",
            "advertised_address = 192.0.2.10\n        direct-media = yes",
        );
        let error = ModuleConfig::parse(&punctuation).unwrap_err().to_string();
        assert!(error.contains("unknown variant `direct-media`"), "{error}");
    }

    #[test]
    fn canonical_serialization_is_deterministic_semantic_and_quote_safe() {
        let source = CONFIG.replace("description = Reception", "description = \"Desk; west\"");
        let expected = ModuleConfig::parse(&source).unwrap();
        let first = ModuleConfig::to_canonical_string(&source).unwrap();
        let second = ModuleConfig::to_canonical_string(&first).unwrap();

        assert_eq!(first, second);
        assert_eq!(ModuleConfig::parse(&first).unwrap(), expected);
        assert!(first.contains("description = \"Desk; west\""));
        ModuleConfig::check_canonical(&first).unwrap();
    }
}
