//! Typed management actions for live calls, media, and conferences.
//!
//! Channel detail uses exact `PbxCallId`. Media list optionally filters that ID;
//! media detail requires `PbxCallId`, handset `CallId`, `Kind`, and explicit
//! `receive`/`transmit` `Direction`. Statistics list optionally filters
//! `DeviceId`; detail requires `DeviceId` and handset `CallId`. Conference and
//! participant detail use positive `ConferenceId` and `ParticipantId`.
//!
//! Providers copy controller/media/conference state before the AMI callback
//! writes output, so no controller lock is held across native I/O. Calls,
//! streams, statistics, conferences, and participants sort by their complete
//! typed identity and reject duplicates. Presentation filtering happens before
//! snapshot construction; private dialed and participant identity, RTP
//! endpoints, and raw quality reports are not exposed.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use sccp_protocol::{
    CallId, ConferenceId, DeviceId, MediaEndpoint, MediaStatisticsSnapshot, ParticipantId,
};
use thiserror::Error;

use crate::ami::inventory::InventoryValue;
use crate::ami::manager::{
    ManagerBackend, ManagerError, ManagerField, ManagerLimits, ManagerPrivilege, ManagerRequest,
    ManagerResponse, RequestFields, RequestFieldsError,
};
use crate::pbx::query::channel::{
    ChannelDirectionSummary, ChannelMediaStateSummary, ChannelStateSummary,
};
use crate::runtime::backend::{PbxBridgeId, PbxCallId};
use crate::runtime::controller::{ConferenceOrigin, ConferencePhase};

pub const SHOW_CHANNELS_ACTION: &str = "SCCPShowChannels";
pub const SHOW_CHANNEL_ACTION: &str = "SCCPShowChannel";
pub const SHOW_MEDIA_STREAMS_ACTION: &str = "SCCPShowMediaStreams";
pub const SHOW_MEDIA_STREAM_ACTION: &str = "SCCPShowMediaStream";
pub const SHOW_MEDIA_STATISTICS_ACTION: &str = "SCCPShowMediaStatistics";
pub const SHOW_MEDIA_STATISTIC_ACTION: &str = "SCCPShowMediaStatistic";
pub const SHOW_CONFERENCES_ACTION: &str = "SCCPShowConferences";
pub const SHOW_CONFERENCE_ACTION: &str = "SCCPShowConference";
pub const SHOW_CONFERENCE_PARTICIPANTS_ACTION: &str = "SCCPShowConferenceParticipants";
pub const SHOW_CONFERENCE_PARTICIPANT_ACTION: &str = "SCCPShowConferenceParticipant";

const MAX_CALL_ITEMS: usize = 40;
const MAX_MEDIA_ITEMS: usize = 24;
const MAX_CONFERENCE_ITEMS: usize = 36;
const MAX_PARTICIPANT_ITEMS: usize = 16;
const MAX_RESPONSE_FIELDS: usize = 512;
const MAX_FIELD_VALUE_BYTES: usize = 4096;
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

const RUNTIME_LIMITS: ManagerLimits = ManagerLimits {
    max_fields: MAX_RESPONSE_FIELDS,
    max_field_name_bytes: 64,
    max_field_value_bytes: MAX_FIELD_VALUE_BYTES,
    max_response_bytes: MAX_RESPONSE_BYTES,
};
const RUNTIME_PRIVILEGES: ManagerPrivilege = ManagerPrivilege::SYSTEM
    .union(ManagerPrivilege::CONFIG)
    .union(ManagerPrivilege::REPORTING);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CallStatus {
    pub pbx_id: PbxCallId,
    pub line: String,
    pub context: String,
    pub state: ChannelStateSummary,
    pub direction: ChannelDirectionSummary,
    pub dialed_number: InventoryValue,
    pub privacy: bool,
    pub active_call_id: Option<CallId>,
    pub appearance_count: usize,
    pub conference_id: Option<ConferenceId>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum MediaKind {
    Audio,
    Video,
}

impl MediaKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Audio => "audio",
            Self::Video => "video",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        if value.eq_ignore_ascii_case("audio") {
            Some(Self::Audio)
        } else if value.eq_ignore_ascii_case("video") {
            Some(Self::Video)
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum MediaDirection {
    Receive,
    Transmit,
}

impl MediaDirection {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Receive => "receive",
            Self::Transmit => "transmit",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        if value.eq_ignore_ascii_case("receive") {
            Some(Self::Receive)
        } else if value.eq_ignore_ascii_case("transmit") {
            Some(Self::Transmit)
        } else {
            None
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MediaStreamStatus {
    pub pbx_id: PbxCallId,
    pub call_id: CallId,
    pub device_id: DeviceId,
    pub line_instance: u32,
    pub kind: MediaKind,
    pub direction: MediaDirection,
    pub state: ChannelMediaStateSummary,
    pub privacy: bool,
    pub endpoint: Option<MediaEndpoint>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MediaStatisticsPrivacy {
    Public,
    #[default]
    Private,
}

impl MediaStatisticsPrivacy {
    pub(crate) const fn is_private(self) -> bool {
        matches!(self, Self::Private)
    }
}

impl From<Option<bool>> for MediaStatisticsPrivacy {
    fn from(private: Option<bool>) -> Self {
        match private {
            Some(false) => Self::Public,
            Some(true) | None => Self::Private,
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct MediaStatisticsStatus {
    pub device_id: DeviceId,
    pub privacy: MediaStatisticsPrivacy,
    pub snapshot: MediaStatisticsSnapshot,
}

impl MediaStatisticsStatus {
    pub fn new(
        device_id: DeviceId,
        privacy: MediaStatisticsPrivacy,
        snapshot: MediaStatisticsSnapshot,
    ) -> Self {
        let mut status = Self {
            device_id,
            privacy,
            snapshot,
        };
        status.enforce_privacy();
        status
    }

    pub(crate) fn enforce_privacy(&mut self) {
        if self.privacy.is_private() {
            self.snapshot.receive_peer = None;
            self.snapshot.transmit_peer = None;
        }
    }
}

impl fmt::Debug for MediaStatisticsStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut snapshot = self.snapshot.clone();
        snapshot.receive_peer = None;
        snapshot.transmit_peer = None;
        formatter
            .debug_struct("MediaStatisticsStatus")
            .field("device_id", &self.device_id)
            .field("privacy", &self.privacy)
            .field("snapshot", &snapshot)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConferenceStatus {
    pub id: ConferenceId,
    pub bridge_id: PbxBridgeId,
    pub owner_device_id: DeviceId,
    pub phase: ConferencePhase,
    pub origin: ConferenceOrigin,
    pub participant_count: usize,
    pub moderator_count: usize,
    pub pending_invite: bool,
    pub pending_mutation: bool,
    pub music_on_hold_class: Option<String>,
    pub general_announcements: bool,
    pub participant_announcements: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConferenceParticipantStatus {
    pub conference_id: ConferenceId,
    pub participant_id: ParticipantId,
    pub pbx_id: PbxCallId,
    pub call_id: CallId,
    pub device_id: DeviceId,
    pub display_name: InventoryValue,
    pub number: InventoryValue,
    pub identity_presented: bool,
    pub moderator: bool,
    pub muted: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RuntimeStatusSnapshot {
    pub calls: Vec<CallStatus>,
    pub media_streams: Vec<MediaStreamStatus>,
    pub media_statistics: Vec<MediaStatisticsStatus>,
    pub conferences: Vec<ConferenceStatus>,
    pub participants: Vec<ConferenceParticipantStatus>,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum RuntimeStatusProviderError {
    #[error("live management status is unavailable")]
    Unavailable,
}

pub trait RuntimeStatusProvider: Send + Sync + 'static {
    fn snapshot(&self) -> Result<RuntimeStatusSnapshot, RuntimeStatusProviderError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimeAction {
    ChannelList,
    ChannelDetail,
    MediaList,
    MediaDetail,
    MediaStatisticsList,
    MediaStatisticsDetail,
    ConferenceList,
    ConferenceDetail,
    ParticipantList,
    ParticipantDetail,
}

impl RuntimeAction {
    const ALL: [Self; 10] = [
        Self::ChannelList,
        Self::ChannelDetail,
        Self::MediaList,
        Self::MediaDetail,
        Self::MediaStatisticsList,
        Self::MediaStatisticsDetail,
        Self::ConferenceList,
        Self::ConferenceDetail,
        Self::ParticipantList,
        Self::ParticipantDetail,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::ChannelList => SHOW_CHANNELS_ACTION,
            Self::ChannelDetail => SHOW_CHANNEL_ACTION,
            Self::MediaList => SHOW_MEDIA_STREAMS_ACTION,
            Self::MediaDetail => SHOW_MEDIA_STREAM_ACTION,
            Self::MediaStatisticsList => SHOW_MEDIA_STATISTICS_ACTION,
            Self::MediaStatisticsDetail => SHOW_MEDIA_STATISTIC_ACTION,
            Self::ConferenceList => SHOW_CONFERENCES_ACTION,
            Self::ConferenceDetail => SHOW_CONFERENCE_ACTION,
            Self::ParticipantList => SHOW_CONFERENCE_PARTICIPANTS_ACTION,
            Self::ParticipantDetail => SHOW_CONFERENCE_PARTICIPANT_ACTION,
        }
    }

    const fn synopsis(self) -> &'static str {
        match self {
            Self::ChannelList => "List live channels",
            Self::ChannelDetail => "Show one live channel",
            Self::MediaList => "List live media streams",
            Self::MediaDetail => "Show one media stream",
            Self::MediaStatisticsList => "List station media statistics",
            Self::MediaStatisticsDetail => "Show station media snapshot",
            Self::ConferenceList => "List live conferences",
            Self::ConferenceDetail => "Show one live conference",
            Self::ParticipantList => "List conference members",
            Self::ParticipantDetail => "Show one conference member",
        }
    }

    const fn description(self) -> &'static str {
        match self {
            Self::ChannelList => "List live calls in deterministic backend identifier order.",
            Self::ChannelDetail => "Show allowlisted fields for one live call.",
            Self::MediaList => "List live media streams in deterministic identity order.",
            Self::MediaDetail => "Show one exact receive or transmit media stream.",
            Self::MediaStatisticsList => {
                "List the latest correlated station media statistics in device order."
            }
            Self::MediaStatisticsDetail => {
                "Show one exact device and call media-statistics snapshot."
            }
            Self::ConferenceList => "List live conferences in deterministic identifier order.",
            Self::ConferenceDetail => "Show allowlisted fields for one live conference.",
            Self::ParticipantList => "List one conference's participants by stable identifier.",
            Self::ParticipantDetail => "Show one conference participant with privacy enforcement.",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|action| action.name().eq_ignore_ascii_case(name))
    }
}

/// Register every live-status action as one RAII-owned lifecycle group.
pub fn register_runtime_status_actions<P: RuntimeStatusProvider, M: ManagerBackend>(
    provider: P,
    manager: M,
) -> Result<Vec<M::Registration>, ManagerError> {
    let provider = Arc::new(provider);
    let mut registrations = Vec::with_capacity(RuntimeAction::ALL.len());
    for action in RuntimeAction::ALL {
        let provider = Arc::clone(&provider);
        registrations.push(manager.register_action(
            action.name(),
            RUNTIME_PRIVILEGES,
            action.synopsis(),
            action.description(),
            RUNTIME_LIMITS,
            move |request| handle_runtime_status_request(provider.as_ref(), request),
        )?);
    }
    Ok(registrations)
}

pub fn handle_runtime_status_request<P: RuntimeStatusProvider + ?Sized>(
    provider: &P,
    request: ManagerRequest,
) -> ManagerResponse {
    match execute_runtime_status_request(provider, &request) {
        Ok(response) => response,
        Err(error) => ManagerResponse::error(error.response_message())
            .expect("fixed live-status error message is valid"),
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
enum RuntimeActionError {
    #[error("unknown live-status action")]
    UnknownAction,
    #[error("request field is not allowlisted")]
    UnknownField,
    #[error("request repeats a singleton field")]
    DuplicateField,
    #[error("request contains sensitive metadata")]
    SensitiveField,
    #[error("request selector is missing or malformed")]
    InvalidSelector,
    #[error("requested live object is absent")]
    NotFound,
    #[error("live status contains duplicate identities")]
    DuplicateObject,
    #[error("live-status result exceeds its bounded item limit")]
    TooManyItems,
    #[error("live-status response exceeds its bounded size limit")]
    ResponseTooLarge,
    #[error("live-status response cannot be represented safely")]
    InvalidOutput,
    #[error(transparent)]
    Provider(#[from] RuntimeStatusProviderError),
}

impl RuntimeActionError {
    const fn response_message(self) -> &'static str {
        match self {
            Self::UnknownAction => "Unknown live-status action",
            Self::UnknownField => "Request field is not allowlisted",
            Self::DuplicateField => "Request repeats a singleton field",
            Self::SensitiveField => "Sensitive request fields are not accepted",
            Self::InvalidSelector => "Request selector is missing or malformed",
            Self::NotFound => "Requested live object was not found",
            Self::DuplicateObject => "Live status contains duplicate identities",
            Self::TooManyItems => "Live-status result exceeds the bounded item limit",
            Self::ResponseTooLarge => "Live-status response exceeds the bounded size limit",
            Self::InvalidOutput => "Live-status response cannot be represented safely",
            Self::Provider(_) => "Live management status is unavailable",
        }
    }
}

fn execute_runtime_status_request<P: RuntimeStatusProvider + ?Sized>(
    provider: &P,
    request: &ManagerRequest,
) -> Result<ManagerResponse, RuntimeActionError> {
    let action = RuntimeAction::parse(&request.action).ok_or(RuntimeActionError::UnknownAction)?;
    let allowed = match action {
        RuntimeAction::ChannelList | RuntimeAction::ConferenceList => &[][..],
        RuntimeAction::ChannelDetail => &["pbxcallid"][..],
        RuntimeAction::MediaList => &["pbxcallid"][..],
        RuntimeAction::MediaDetail => &["pbxcallid", "callid", "kind", "direction"][..],
        RuntimeAction::MediaStatisticsList => &["deviceid"][..],
        RuntimeAction::MediaStatisticsDetail => &["deviceid", "callid"][..],
        RuntimeAction::ConferenceDetail | RuntimeAction::ParticipantList => &["conferenceid"][..],
        RuntimeAction::ParticipantDetail => &["conferenceid", "participantid"][..],
    };
    let selectors = parse_selectors(request, allowed)?;
    let mut snapshot = provider.snapshot()?;
    normalize_snapshot(&mut snapshot)?;
    let fields = match action {
        RuntimeAction::ChannelList => call_list_fields(&snapshot.calls)?,
        RuntimeAction::ChannelDetail => {
            let pbx_id = PbxCallId(parse_nonzero_u64(selector(&selectors, "pbxcallid")?)?);
            let item = snapshot
                .calls
                .iter()
                .find(|item| item.pbx_id == pbx_id)
                .ok_or(RuntimeActionError::NotFound)?;
            call_fields(item, None)?
        }
        RuntimeAction::MediaList => {
            let pbx_id = selectors
                .get("pbxcallid")
                .map(|value| parse_nonzero_u64(value).map(PbxCallId))
                .transpose()?;
            let items = snapshot
                .media_streams
                .iter()
                .filter(|item| pbx_id.is_none_or(|pbx_id| item.pbx_id == pbx_id))
                .collect::<Vec<_>>();
            media_list_fields(&items)?
        }
        RuntimeAction::MediaDetail => {
            let pbx_id = PbxCallId(parse_nonzero_u64(selector(&selectors, "pbxcallid")?)?);
            let call_id = CallId(parse_nonzero_u64(selector(&selectors, "callid")?)?);
            let kind = MediaKind::parse(selector(&selectors, "kind")?)
                .ok_or(RuntimeActionError::InvalidSelector)?;
            let direction = MediaDirection::parse(selector(&selectors, "direction")?)
                .ok_or(RuntimeActionError::InvalidSelector)?;
            let item = snapshot
                .media_streams
                .iter()
                .find(|item| {
                    item.pbx_id == pbx_id
                        && item.call_id == call_id
                        && item.kind == kind
                        && item.direction == direction
                })
                .ok_or(RuntimeActionError::NotFound)?;
            media_fields(item, None)?
        }
        RuntimeAction::MediaStatisticsList => {
            let device_id = optional_device_selector(&selectors, "deviceid")?;
            let items = snapshot
                .media_statistics
                .iter()
                .filter(|item| {
                    device_id
                        .as_ref()
                        .is_none_or(|device_id| item.device_id == *device_id)
                })
                .collect::<Vec<_>>();
            media_statistics_list_fields(&items)?
        }
        RuntimeAction::MediaStatisticsDetail => {
            let device_id = parse_device_selector(&selectors, "deviceid")?;
            let call_id = CallId(parse_nonzero_u64(selector(&selectors, "callid")?)?);
            let item = snapshot
                .media_statistics
                .iter()
                .find(|item| item.device_id == device_id && item.snapshot.call_id == call_id)
                .ok_or(RuntimeActionError::NotFound)?;
            media_statistics_fields(item, None)?
        }
        RuntimeAction::ConferenceList => conference_list_fields(&snapshot.conferences)?,
        RuntimeAction::ConferenceDetail => {
            let id = ConferenceId::new(parse_nonzero_u32(selector(&selectors, "conferenceid")?)?);
            let item = snapshot
                .conferences
                .iter()
                .find(|item| item.id == id)
                .ok_or(RuntimeActionError::NotFound)?;
            conference_fields(item, None)?
        }
        RuntimeAction::ParticipantList => {
            let id = ConferenceId::new(parse_nonzero_u32(selector(&selectors, "conferenceid")?)?);
            if !snapshot
                .conferences
                .iter()
                .any(|conference| conference.id == id)
            {
                return Err(RuntimeActionError::NotFound);
            }
            let items = snapshot
                .participants
                .iter()
                .filter(|item| item.conference_id == id)
                .collect::<Vec<_>>();
            participant_list_fields(&items)?
        }
        RuntimeAction::ParticipantDetail => {
            let conference_id =
                ConferenceId::new(parse_nonzero_u32(selector(&selectors, "conferenceid")?)?);
            let participant_id =
                ParticipantId::new(parse_nonzero_u32(selector(&selectors, "participantid")?)?);
            let item = snapshot
                .participants
                .iter()
                .find(|item| {
                    item.conference_id == conference_id && item.participant_id == participant_id
                })
                .ok_or(RuntimeActionError::NotFound)?;
            participant_fields(item, None)?
        }
    };
    bounded_success(fields)
}

fn normalize_snapshot(snapshot: &mut RuntimeStatusSnapshot) -> Result<(), RuntimeActionError> {
    for call in &mut snapshot.calls {
        if call.privacy {
            call.dialed_number = InventoryValue::Redacted;
        }
    }
    for participant in &mut snapshot.participants {
        if !participant.identity_presented {
            participant.display_name = InventoryValue::Redacted;
            participant.number = InventoryValue::Redacted;
        }
    }
    for statistics in &mut snapshot.media_statistics {
        statistics.enforce_privacy();
    }
    snapshot.calls.sort_by_key(|item| item.pbx_id.0);
    if snapshot
        .calls
        .windows(2)
        .any(|items| items[0].pbx_id == items[1].pbx_id)
    {
        return Err(RuntimeActionError::DuplicateObject);
    }
    snapshot.media_streams.sort_by(|left, right| {
        (
            left.pbx_id.0,
            left.call_id.0,
            left.kind,
            left.direction,
            &left.device_id,
        )
            .cmp(&(
                right.pbx_id.0,
                right.call_id.0,
                right.kind,
                right.direction,
                &right.device_id,
            ))
    });
    if snapshot.media_streams.windows(2).any(|items| {
        items[0].pbx_id == items[1].pbx_id
            && items[0].call_id == items[1].call_id
            && items[0].kind == items[1].kind
            && items[0].direction == items[1].direction
    }) {
        return Err(RuntimeActionError::DuplicateObject);
    }
    snapshot.media_statistics.sort_by(|left, right| {
        (&left.device_id, left.snapshot.request_generation)
            .cmp(&(&right.device_id, right.snapshot.request_generation))
    });
    if snapshot
        .media_statistics
        .windows(2)
        .any(|items| items[0].device_id == items[1].device_id)
    {
        return Err(RuntimeActionError::DuplicateObject);
    }
    snapshot.conferences.sort_by_key(|item| item.id.get());
    if snapshot
        .conferences
        .windows(2)
        .any(|items| items[0].id == items[1].id)
    {
        return Err(RuntimeActionError::DuplicateObject);
    }
    snapshot
        .participants
        .sort_by_key(|item| (item.conference_id.get(), item.participant_id.get()));
    if snapshot.participants.windows(2).any(|items| {
        items[0].conference_id == items[1].conference_id
            && items[0].participant_id == items[1].participant_id
    }) {
        return Err(RuntimeActionError::DuplicateObject);
    }
    Ok(())
}

fn parse_selectors(
    request: &ManagerRequest,
    allowed: &[&str],
) -> Result<BTreeMap<String, String>, RuntimeActionError> {
    RequestFields::new(request)
        .collect(allowed, &[])
        .map_err(|error| match error {
            RequestFieldsError::Sensitive => RuntimeActionError::SensitiveField,
            RequestFieldsError::Duplicate => RuntimeActionError::DuplicateField,
            RequestFieldsError::Unknown => RuntimeActionError::UnknownField,
            RequestFieldsError::ActionMismatch => RuntimeActionError::UnknownAction,
        })
}

fn selector<'a>(
    selectors: &'a BTreeMap<String, String>,
    name: &str,
) -> Result<&'a str, RuntimeActionError> {
    selectors
        .get(name)
        .map(String::as_str)
        .ok_or(RuntimeActionError::InvalidSelector)
}

fn parse_device_selector(
    selectors: &BTreeMap<String, String>,
    name: &str,
) -> Result<DeviceId, RuntimeActionError> {
    DeviceId::new(selector(selectors, name)?).map_err(|_| RuntimeActionError::InvalidSelector)
}

fn optional_device_selector(
    selectors: &BTreeMap<String, String>,
    name: &str,
) -> Result<Option<DeviceId>, RuntimeActionError> {
    selectors
        .get(name)
        .map(|value| DeviceId::new(value).map_err(|_| RuntimeActionError::InvalidSelector))
        .transpose()
}

fn parse_nonzero_u64(value: &str) -> Result<u64, RuntimeActionError> {
    value
        .parse::<u64>()
        .ok()
        .filter(|value| *value != 0)
        .ok_or(RuntimeActionError::InvalidSelector)
}

fn parse_nonzero_u32(value: &str) -> Result<u32, RuntimeActionError> {
    value
        .parse::<u32>()
        .ok()
        .filter(|value| *value != 0)
        .ok_or(RuntimeActionError::InvalidSelector)
}

fn call_list_fields(items: &[CallStatus]) -> Result<Vec<ManagerField>, RuntimeActionError> {
    ensure_item_bound(items.len(), MAX_CALL_ITEMS)?;
    let mut fields = vec![public("Count", items.len())?];
    for (index, item) in items.iter().enumerate() {
        fields.extend(call_fields(item, Some(index + 1))?);
    }
    Ok(fields)
}

fn call_fields(
    item: &CallStatus,
    index: Option<usize>,
) -> Result<Vec<ManagerField>, RuntimeActionError> {
    let mut fields = object_prefix("Channel", index)?;
    fields.extend([
        public("PbxCallId", item.pbx_id.0)?,
        public("Line", &item.line)?,
        public("Context", &item.context)?,
        public("State", item.state)?,
        public("Direction", item.direction)?,
        public("Privacy", yes_no(item.privacy))?,
        public(
            "ActiveCallId",
            item.active_call_id
                .map_or_else(String::new, |value| value.0.to_string()),
        )?,
        public("AppearanceCount", item.appearance_count)?,
        public(
            "ConferenceId",
            item.conference_id
                .map_or_else(String::new, |value| value.get().to_string()),
        )?,
    ]);
    append_value(&mut fields, "DialedNumber", Some(&item.dialed_number))?;
    Ok(fields)
}

fn media_list_fields(
    items: &[&MediaStreamStatus],
) -> Result<Vec<ManagerField>, RuntimeActionError> {
    ensure_item_bound(items.len(), MAX_MEDIA_ITEMS)?;
    let mut fields = vec![public("Count", items.len())?];
    for (index, item) in items.iter().enumerate() {
        fields.extend(media_fields(item, Some(index + 1))?);
    }
    Ok(fields)
}

fn media_fields(
    item: &MediaStreamStatus,
    index: Option<usize>,
) -> Result<Vec<ManagerField>, RuntimeActionError> {
    let mut fields = object_prefix("MediaStream", index)?;
    fields.extend([
        public("PbxCallId", item.pbx_id.0)?,
        public("CallId", item.call_id.0)?,
        public("DeviceId", item.device_id.as_str())?,
        public("LineInstance", item.line_instance)?,
        public("Kind", item.kind.as_str())?,
        public("Direction", item.direction.as_str())?,
        public("State", item.state)?,
        public("Privacy", yes_no(item.privacy))?,
    ]);
    if let Some(endpoint) = item.endpoint {
        fields.extend([
            public("Address", endpoint.address)?,
            public("RtpPort", endpoint.rtp_port)?,
            public("RtcpPort", endpoint.rtcp_port)?,
            public("CodecId", endpoint.codec.wire_value())?,
            public("PacketMs", endpoint.packet_ms)?,
            public("MaxFramesPerPacket", endpoint.max_frames_per_packet)?,
            public("TelephoneEventPayload", endpoint.telephone_event_payload)?,
        ]);
    } else {
        fields.extend([
            public("Address", "")?,
            public("RtpPort", "")?,
            public("RtcpPort", "")?,
            public("CodecId", "")?,
            public("PacketMs", "")?,
            public("MaxFramesPerPacket", "")?,
            public("TelephoneEventPayload", "")?,
        ]);
    }
    Ok(fields)
}

fn media_statistics_list_fields(
    items: &[&MediaStatisticsStatus],
) -> Result<Vec<ManagerField>, RuntimeActionError> {
    ensure_item_bound(items.len(), MAX_MEDIA_ITEMS)?;
    let mut fields = vec![public("Count", items.len())?];
    for (index, item) in items.iter().enumerate() {
        fields.extend(media_statistics_fields(item, Some(index + 1))?);
    }
    Ok(fields)
}

fn media_statistics_fields(
    item: &MediaStatisticsStatus,
    index: Option<usize>,
) -> Result<Vec<ManagerField>, RuntimeActionError> {
    let snapshot = &item.snapshot;
    let mut fields = object_prefix("MediaStatistics", index)?;
    fields.extend([
        public("DeviceId", item.device_id.as_str())?,
        public("Privacy", yes_no(item.privacy.is_private()))?,
        public("RequestGeneration", snapshot.request_generation)?,
        public("CallId", snapshot.call_id.0)?,
        public("LineInstance", snapshot.line_instance)?,
        public("CodecId", snapshot.codec.wire_value())?,
        public("PacketMs", snapshot.packet_ms)?,
        public("MaxFramesPerPacket", snapshot.max_frames_per_packet)?,
        public("PacketsSent", snapshot.packets_sent)?,
        public("OctetsSent", snapshot.octets_sent)?,
        public("PacketsReceived", snapshot.packets_received)?,
        public("OctetsReceived", snapshot.octets_received)?,
        public("PacketsLost", snapshot.packets_lost)?,
        public("JitterMillis", snapshot.jitter_millis)?,
        public("LatencyMillis", snapshot.latency_millis)?,
        public("QualityByteCount", snapshot.quality_byte_count)?,
    ]);
    if !item.privacy.is_private() {
        append_media_peer(
            &mut fields,
            snapshot.receive_peer,
            ("ReceiveAddress", "ReceiveRtpPort", "ReceiveRtcpPort"),
        )?;
        append_media_peer(
            &mut fields,
            snapshot.transmit_peer,
            ("TransmitAddress", "TransmitRtpPort", "TransmitRtcpPort"),
        )?;
    }
    Ok(fields)
}

fn append_media_peer(
    fields: &mut Vec<ManagerField>,
    endpoint: Option<MediaEndpoint>,
    names: (&'static str, &'static str, &'static str),
) -> Result<(), RuntimeActionError> {
    if let Some(endpoint) = endpoint {
        fields.extend([
            public(names.0, endpoint.address)?,
            public(names.1, endpoint.rtp_port)?,
            public(names.2, endpoint.rtcp_port)?,
        ]);
    } else {
        fields.extend([
            public(names.0, "")?,
            public(names.1, "")?,
            public(names.2, "")?,
        ]);
    }
    Ok(())
}

fn conference_list_fields(
    items: &[ConferenceStatus],
) -> Result<Vec<ManagerField>, RuntimeActionError> {
    ensure_item_bound(items.len(), MAX_CONFERENCE_ITEMS)?;
    let mut fields = vec![public("Count", items.len())?];
    for (index, item) in items.iter().enumerate() {
        fields.extend(conference_fields(item, Some(index + 1))?);
    }
    Ok(fields)
}

fn conference_fields(
    item: &ConferenceStatus,
    index: Option<usize>,
) -> Result<Vec<ManagerField>, RuntimeActionError> {
    let mut fields = object_prefix("Conference", index)?;
    fields.extend([
        public("ConferenceId", item.id.get())?,
        public("BridgeId", item.bridge_id.0)?,
        public("OwnerDeviceId", item.owner_device_id.as_str())?,
        public("Phase", conference_phase(item.phase))?,
        public("Origin", conference_origin(item.origin))?,
        public("ParticipantCount", item.participant_count)?,
        public("ModeratorCount", item.moderator_count)?,
        public("PendingInvite", yes_no(item.pending_invite))?,
        public("PendingMutation", yes_no(item.pending_mutation))?,
        public(
            "MusicOnHoldClass",
            item.music_on_hold_class.as_deref().unwrap_or(""),
        )?,
        public("GeneralAnnouncements", yes_no(item.general_announcements))?,
        public(
            "ParticipantAnnouncements",
            yes_no(item.participant_announcements),
        )?,
    ]);
    Ok(fields)
}

fn participant_list_fields(
    items: &[&ConferenceParticipantStatus],
) -> Result<Vec<ManagerField>, RuntimeActionError> {
    ensure_item_bound(items.len(), MAX_PARTICIPANT_ITEMS)?;
    let mut fields = vec![public("Count", items.len())?];
    for (index, item) in items.iter().enumerate() {
        fields.extend(participant_fields(item, Some(index + 1))?);
    }
    Ok(fields)
}

fn participant_fields(
    item: &ConferenceParticipantStatus,
    index: Option<usize>,
) -> Result<Vec<ManagerField>, RuntimeActionError> {
    let mut fields = object_prefix("ConferenceParticipant", index)?;
    fields.extend([
        public("ConferenceId", item.conference_id.get())?,
        public("ParticipantId", item.participant_id.get())?,
        public("PbxCallId", item.pbx_id.0)?,
        public("CallId", item.call_id.0)?,
        public("DeviceId", item.device_id.as_str())?,
        public("IdentityPresented", yes_no(item.identity_presented))?,
        public("Moderator", yes_no(item.moderator))?,
        public("Muted", yes_no(item.muted))?,
    ]);
    append_value(&mut fields, "DisplayName", Some(&item.display_name))?;
    append_value(&mut fields, "Number", Some(&item.number))?;
    Ok(fields)
}

fn object_prefix(
    object_type: &'static str,
    index: Option<usize>,
) -> Result<Vec<ManagerField>, RuntimeActionError> {
    let mut fields = vec![public("ObjectType", object_type)?];
    if let Some(index) = index {
        fields.push(public("ObjectIndex", index)?);
    }
    Ok(fields)
}

fn append_value(
    fields: &mut Vec<ManagerField>,
    name: &'static str,
    value: Option<&InventoryValue>,
) -> Result<(), RuntimeActionError> {
    match value {
        Some(InventoryValue::Public(value)) => fields.push(public(name, value)?),
        Some(InventoryValue::Redacted) => fields.push(redacted(name)?),
        None => fields.push(public(name, "")?),
    }
    Ok(())
}

fn ensure_item_bound(count: usize, maximum: usize) -> Result<(), RuntimeActionError> {
    if count > maximum {
        Err(RuntimeActionError::TooManyItems)
    } else {
        Ok(())
    }
}

fn public(name: &'static str, value: impl ToString) -> Result<ManagerField, RuntimeActionError> {
    ManagerField::public(name, value.to_string()).map_err(|_| RuntimeActionError::InvalidOutput)
}

fn redacted(name: &'static str) -> Result<ManagerField, RuntimeActionError> {
    ManagerField::redacted(name).map_err(|_| RuntimeActionError::InvalidOutput)
}

fn bounded_success(fields: Vec<ManagerField>) -> Result<ManagerResponse, RuntimeActionError> {
    if fields.len() > MAX_RESPONSE_FIELDS {
        return Err(RuntimeActionError::ResponseTooLarge);
    }
    let mut total = 2usize + 14 + "Success".len() + 11 + "Live status query complete".len();
    for field in &fields {
        let value_length = match field.public_value() {
            Some(value) if value.len() <= MAX_FIELD_VALUE_BYTES => value.len(),
            Some(_) => return Err(RuntimeActionError::InvalidOutput),
            None => "<redacted>".len(),
        };
        total = total
            .checked_add(field.name().len())
            .and_then(|total| total.checked_add(value_length))
            .and_then(|total| total.checked_add(4))
            .ok_or(RuntimeActionError::InvalidOutput)?;
    }
    if total > MAX_RESPONSE_BYTES {
        return Err(RuntimeActionError::ResponseTooLarge);
    }
    Ok(ManagerResponse::success("Live status query complete")
        .expect("fixed live-status success message is valid")
        .with_fields(fields))
}

const fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

const fn conference_phase(value: ConferencePhase) -> &'static str {
    match value {
        ConferencePhase::Consultation => "consultation",
        ConferencePhase::Merging => "merging",
        ConferencePhase::Active => "active",
    }
}

const fn conference_origin(value: ConferenceOrigin) -> &'static str {
    match value {
        ConferenceOrigin::Consultation => "consultation",
        ConferenceOrigin::Selection => "selection",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use std::net::{IpAddr, Ipv4Addr};

    use sccp_protocol::{Codec, LineInstance};

    use super::*;
    use crate::ami::manager::{ManagerRequestField, ManagerResponseKind};

    #[derive(Clone)]
    struct FakeProvider {
        snapshot: RuntimeStatusSnapshot,
        error: Option<RuntimeStatusProviderError>,
    }

    impl RuntimeStatusProvider for FakeProvider {
        fn snapshot(&self) -> Result<RuntimeStatusSnapshot, RuntimeStatusProviderError> {
            self.error.map_or_else(|| Ok(self.snapshot.clone()), Err)
        }
    }

    fn request(action: &str, fields: &[(&str, &str)]) -> ManagerRequest {
        let mut request_fields = vec![ManagerRequestField {
            name: "Action".into(),
            value: action.into(),
            sensitive: false,
        }];
        request_fields.extend(fields.iter().map(|(name, value)| ManagerRequestField {
            name: (*name).into(),
            value: (*value).into(),
            sensitive: false,
        }));
        ManagerRequest {
            action: action.into(),
            fields: request_fields,
        }
    }

    fn response_values(response: &ManagerResponse, name: &str) -> Vec<Option<String>> {
        response
            .fields()
            .iter()
            .filter(|field| field.name() == name)
            .map(|field| field.public_value().map(str::to_owned))
            .collect()
    }

    fn endpoint() -> MediaEndpoint {
        MediaEndpoint {
            address: IpAddr::V4(Ipv4Addr::new(192, 0, 2, 8)),
            rtp_port: 4000,
            rtcp_port: 4001,
            codec: Codec::Pcmu,
            packet_ms: 20,
            max_frames_per_packet: 2,
            telephone_event_payload: 101,
        }
    }

    fn snapshot() -> RuntimeStatusSnapshot {
        RuntimeStatusSnapshot {
            calls: vec![
                CallStatus {
                    pbx_id: PbxCallId(20),
                    line: "2000".into(),
                    context: "from-sccp".into(),
                    state: ChannelStateSummary::Connected,
                    direction: ChannelDirectionSummary::Inbound,
                    dialed_number: InventoryValue::Redacted,
                    privacy: true,
                    active_call_id: Some(CallId(12)),
                    appearance_count: 1,
                    conference_id: Some(ConferenceId::new(8)),
                },
                CallStatus {
                    pbx_id: PbxCallId(10),
                    line: "1000".into(),
                    context: "from-sccp".into(),
                    state: ChannelStateSummary::Calling,
                    direction: ChannelDirectionSummary::Outbound,
                    dialed_number: InventoryValue::Public("18005550199".into()),
                    privacy: false,
                    active_call_id: Some(CallId(11)),
                    appearance_count: 1,
                    conference_id: None,
                },
            ],
            media_streams: vec![
                MediaStreamStatus {
                    pbx_id: PbxCallId(20),
                    call_id: CallId(12),
                    device_id: DeviceId::new("SEP112233445566").unwrap(),
                    line_instance: 1,
                    kind: MediaKind::Audio,
                    direction: MediaDirection::Transmit,
                    state: ChannelMediaStateSummary::Opening,
                    privacy: true,
                    endpoint: None,
                },
                MediaStreamStatus {
                    pbx_id: PbxCallId(10),
                    call_id: CallId(11),
                    device_id: DeviceId::new("SEP001122334455").unwrap(),
                    line_instance: 2,
                    kind: MediaKind::Audio,
                    direction: MediaDirection::Receive,
                    state: ChannelMediaStateSummary::Open,
                    privacy: false,
                    endpoint: Some(endpoint()),
                },
            ],
            media_statistics: vec![
                MediaStatisticsStatus {
                    device_id: DeviceId::new("SEP112233445566").unwrap(),
                    privacy: MediaStatisticsPrivacy::Private,
                    snapshot: MediaStatisticsSnapshot {
                        request_generation: 8,
                        call_id: CallId(12),
                        line_instance: LineInstance::new(1),
                        codec: Codec::Pcma,
                        packet_ms: 30,
                        max_frames_per_packet: 2,
                        receive_peer: Some(MediaEndpoint {
                            address: "203.0.113.44".parse().unwrap(),
                            ..endpoint()
                        }),
                        transmit_peer: Some(MediaEndpoint {
                            address: "203.0.113.45".parse().unwrap(),
                            ..endpoint()
                        }),
                        packets_sent: 90,
                        octets_sent: 14_400,
                        packets_received: 88,
                        octets_received: 14_080,
                        packets_lost: 2,
                        jitter_millis: 12,
                        latency_millis: 40,
                        quality_byte_count: 64,
                    },
                },
                MediaStatisticsStatus {
                    device_id: DeviceId::new("SEP001122334455").unwrap(),
                    privacy: MediaStatisticsPrivacy::Public,
                    snapshot: MediaStatisticsSnapshot {
                        request_generation: 7,
                        call_id: CallId(11),
                        line_instance: LineInstance::new(2),
                        codec: Codec::Pcmu,
                        packet_ms: 20,
                        max_frames_per_packet: 1,
                        receive_peer: Some(endpoint()),
                        transmit_peer: Some(MediaEndpoint {
                            address: IpAddr::V6("2001:db8::20".parse().unwrap()),
                            rtp_port: 5000,
                            rtcp_port: 5001,
                            ..endpoint()
                        }),
                        packets_sent: 100,
                        octets_sent: 16_000,
                        packets_received: 98,
                        octets_received: 15_680,
                        packets_lost: 2,
                        jitter_millis: 9,
                        latency_millis: 35,
                        quality_byte_count: 48,
                    },
                },
            ],
            conferences: vec![ConferenceStatus {
                id: ConferenceId::new(8),
                bridge_id: PbxBridgeId(70),
                owner_device_id: DeviceId::new("SEP112233445566").unwrap(),
                phase: ConferencePhase::Active,
                origin: ConferenceOrigin::Consultation,
                participant_count: 2,
                moderator_count: 1,
                pending_invite: false,
                pending_mutation: false,
                music_on_hold_class: Some("default".into()),
                general_announcements: true,
                participant_announcements: true,
            }],
            participants: vec![
                ConferenceParticipantStatus {
                    conference_id: ConferenceId::new(8),
                    participant_id: ParticipantId::new(2),
                    pbx_id: PbxCallId(20),
                    call_id: CallId(12),
                    device_id: DeviceId::new("SEP112233445566").unwrap(),
                    display_name: InventoryValue::Redacted,
                    number: InventoryValue::Redacted,
                    identity_presented: false,
                    moderator: false,
                    muted: true,
                },
                ConferenceParticipantStatus {
                    conference_id: ConferenceId::new(8),
                    participant_id: ParticipantId::new(1),
                    pbx_id: PbxCallId(10),
                    call_id: CallId(11),
                    device_id: DeviceId::new("SEP001122334455").unwrap(),
                    display_name: InventoryValue::Public("Reception".into()),
                    number: InventoryValue::Public("1000".into()),
                    identity_presented: true,
                    moderator: true,
                    muted: false,
                },
            ],
        }
    }

    fn provider() -> FakeProvider {
        FakeProvider {
            snapshot: snapshot(),
            error: None,
        }
    }

    #[test]
    fn call_list_and_detail_are_sorted_typed_and_private() {
        let provider = provider();
        let list = handle_runtime_status_request(&provider, request(SHOW_CHANNELS_ACTION, &[]));
        assert_eq!(list.kind(), ManagerResponseKind::Success);
        assert_eq!(
            response_values(&list, "PbxCallId"),
            [Some("10".into()), Some("20".into())]
        );
        assert_eq!(response_values(&list, "DialedNumber")[1], None);

        let detail = handle_runtime_status_request(
            &provider,
            request(SHOW_CHANNEL_ACTION, &[("PbxCallId", "10")]),
        );
        assert_eq!(response_values(&detail, "Line"), [Some("1000".into())]);
        assert_eq!(
            response_values(&detail, "DialedNumber"),
            [Some("18005550199".into())]
        );
    }

    #[test]
    fn media_list_filter_and_detail_require_exact_receive_transmit_identity() {
        let provider = provider();
        let list = handle_runtime_status_request(
            &provider,
            request(SHOW_MEDIA_STREAMS_ACTION, &[("PbxCallId", "10")]),
        );
        assert_eq!(response_values(&list, "Count"), [Some("1".into())]);
        assert_eq!(
            response_values(&list, "Direction"),
            [Some("receive".into())]
        );

        let detail = handle_runtime_status_request(
            &provider,
            request(
                SHOW_MEDIA_STREAM_ACTION,
                &[
                    ("PbxCallId", "10"),
                    ("CallId", "11"),
                    ("Kind", "audio"),
                    ("Direction", "receive"),
                ],
            ),
        );
        assert_eq!(response_values(&detail, "State"), [Some("open".into())]);
        assert_eq!(response_values(&detail, "CodecId"), [Some("4".into())]);
        assert_eq!(response_values(&detail, "PacketMs"), [Some("20".into())]);

        let absent = handle_runtime_status_request(
            &provider,
            request(
                SHOW_MEDIA_STREAM_ACTION,
                &[
                    ("PbxCallId", "10"),
                    ("CallId", "11"),
                    ("Kind", "video"),
                    ("Direction", "transmit"),
                ],
            ),
        );
        assert_eq!(
            absent.message(),
            Some("Requested live object was not found")
        );
    }

    #[test]
    fn media_statistics_are_sorted_filtered_bounded_and_peer_aware() {
        let provider = provider();
        let list =
            handle_runtime_status_request(&provider, request(SHOW_MEDIA_STATISTICS_ACTION, &[]));
        assert_eq!(list.kind(), ManagerResponseKind::Success);
        assert_eq!(
            response_values(&list, "DeviceId"),
            [
                Some("SEP001122334455".into()),
                Some("SEP112233445566".into())
            ]
        );
        assert_eq!(
            response_values(&list, "PacketsSent"),
            [Some("100".into()), Some("90".into())]
        );
        assert_eq!(
            response_values(&list, "JitterMillis"),
            [Some("9".into()), Some("12".into())]
        );

        let filtered = handle_runtime_status_request(
            &provider,
            request(
                SHOW_MEDIA_STATISTICS_ACTION,
                &[("DeviceId", "sep112233445566")],
            ),
        );
        assert_eq!(response_values(&filtered, "Count"), [Some("1".into())]);
        assert_eq!(response_values(&filtered, "Privacy"), [Some("yes".into())]);
        assert_eq!(
            response_values(&filtered, "PacketsSent"),
            [Some("90".into())]
        );
        assert!(response_values(&filtered, "ReceiveAddress").is_empty());
        assert!(response_values(&filtered, "TransmitRtpPort").is_empty());

        let detail = handle_runtime_status_request(
            &provider,
            request(
                SHOW_MEDIA_STATISTIC_ACTION,
                &[("DeviceId", "SEP001122334455"), ("CallId", "11")],
            ),
        );
        assert_eq!(detail.kind(), ManagerResponseKind::Success);
        assert_eq!(response_values(&detail, "Privacy"), [Some("no".into())]);
        assert_eq!(response_values(&detail, "CodecId"), [Some("4".into())]);
        assert_eq!(
            response_values(&detail, "ReceiveAddress"),
            [Some("192.0.2.8".into())]
        );
        assert_eq!(
            response_values(&detail, "TransmitAddress"),
            [Some("2001:db8::20".into())]
        );
        assert_eq!(
            response_values(&detail, "QualityByteCount"),
            [Some("48".into())]
        );

        let replacement = handle_runtime_status_request(
            &provider,
            request(
                SHOW_MEDIA_STATISTIC_ACTION,
                &[("DeviceId", "SEP001122334455"), ("CallId", "10")],
            ),
        );
        assert_eq!(
            replacement.message(),
            Some("Requested live object was not found")
        );
    }

    #[test]
    fn conference_and_participant_actions_use_stable_ids_and_redaction() {
        let provider = provider();
        let conferences =
            handle_runtime_status_request(&provider, request(SHOW_CONFERENCES_ACTION, &[]));
        assert_eq!(
            response_values(&conferences, "ConferenceId"),
            [Some("8".into())]
        );

        let conference = handle_runtime_status_request(
            &provider,
            request(SHOW_CONFERENCE_ACTION, &[("ConferenceId", "8")]),
        );
        assert_eq!(
            response_values(&conference, "Phase"),
            [Some("active".into())]
        );

        let participants = handle_runtime_status_request(
            &provider,
            request(
                SHOW_CONFERENCE_PARTICIPANTS_ACTION,
                &[("ConferenceId", "8")],
            ),
        );
        assert_eq!(
            response_values(&participants, "ParticipantId"),
            [Some("1".into()), Some("2".into())]
        );
        assert_eq!(response_values(&participants, "DisplayName")[1], None);

        let participant = handle_runtime_status_request(
            &provider,
            request(
                SHOW_CONFERENCE_PARTICIPANT_ACTION,
                &[("ConferenceId", "8"), ("ParticipantId", "2")],
            ),
        );
        assert_eq!(response_values(&participant, "Number"), [None]);
        assert_eq!(
            response_values(&participant, "IdentityPresented"),
            [Some("no".into())]
        );
    }

    #[test]
    fn redacted_values_never_enter_snapshot_debug_output() {
        let mut snapshot = snapshot();
        let statistics_debug = format!("{:?}", snapshot.media_statistics[0]);
        assert!(!statistics_debug.contains("203.0.113.44"));
        assert!(!statistics_debug.contains("203.0.113.45"));
        snapshot.calls[0].dialed_number = InventoryValue::Public("private-called-number".into());
        snapshot.participants[0].display_name =
            InventoryValue::Public("private-participant".into());
        snapshot.participants[0].number = InventoryValue::Public("private-number".into());
        normalize_snapshot(&mut snapshot).unwrap();
        let rendered = format!("{snapshot:?}");
        assert!(rendered.contains("Redacted"));
        assert!(!rendered.contains("private-called-number"));
        assert!(!rendered.contains("private-participant"));
        assert!(!rendered.contains("private-number"));
        assert!(!rendered.contains("203.0.113.44"));
        assert!(!rendered.contains("203.0.113.45"));
        assert_eq!(snapshot.media_statistics[1].snapshot.receive_peer, None);
        assert_eq!(snapshot.media_statistics[1].snapshot.transmit_peer, None);
    }

    #[test]
    fn absent_statistics_privacy_correlation_fails_closed() {
        assert_eq!(
            MediaStatisticsPrivacy::from(None),
            MediaStatisticsPrivacy::Private
        );
        assert_eq!(
            MediaStatisticsPrivacy::from(Some(true)),
            MediaStatisticsPrivacy::Private
        );
        assert_eq!(
            MediaStatisticsPrivacy::from(Some(false)),
            MediaStatisticsPrivacy::Public
        );
    }

    #[test]
    fn provider_order_is_normalized_and_duplicate_identities_fail_closed() {
        let mut snapshot = snapshot();
        snapshot.calls.reverse();
        snapshot.media_streams.reverse();
        snapshot.participants.reverse();
        let ordered = handle_runtime_status_request(
            &FakeProvider {
                snapshot: snapshot.clone(),
                error: None,
            },
            request(SHOW_CHANNELS_ACTION, &[]),
        );
        assert_eq!(
            response_values(&ordered, "PbxCallId"),
            [Some("10".into()), Some("20".into())]
        );

        let mut duplicate_media = snapshot.clone();
        let mut repeated_stream = duplicate_media.media_streams[0].clone();
        repeated_stream.device_id = DeviceId::new("SEPFFEEDDCCBBAA").unwrap();
        duplicate_media.media_streams.push(repeated_stream);
        let duplicate = handle_runtime_status_request(
            &FakeProvider {
                snapshot: duplicate_media,
                error: None,
            },
            request(SHOW_MEDIA_STREAMS_ACTION, &[]),
        );
        assert_eq!(
            duplicate.message(),
            Some("Live status contains duplicate identities")
        );

        let mut duplicate_statistics = snapshot.clone();
        let mut repeated_statistics = duplicate_statistics.media_statistics[0].clone();
        repeated_statistics.snapshot.request_generation += 1;
        duplicate_statistics
            .media_statistics
            .push(repeated_statistics);
        let duplicate = handle_runtime_status_request(
            &FakeProvider {
                snapshot: duplicate_statistics,
                error: None,
            },
            request(SHOW_MEDIA_STATISTICS_ACTION, &[]),
        );
        assert_eq!(
            duplicate.message(),
            Some("Live status contains duplicate identities")
        );

        snapshot.calls.push(snapshot.calls[0].clone());
        let duplicate = handle_runtime_status_request(
            &FakeProvider {
                snapshot,
                error: None,
            },
            request(SHOW_CHANNELS_ACTION, &[]),
        );
        assert_eq!(
            duplicate.message(),
            Some("Live status contains duplicate identities")
        );
    }

    #[test]
    fn unknown_duplicate_sensitive_and_malformed_fields_fail_without_values() {
        let provider = provider();
        let cases = [
            request(SHOW_CHANNELS_ACTION, &[("Unexpected", "private-value")]),
            request(
                SHOW_CHANNEL_ACTION,
                &[("PbxCallId", "10"), ("PbxCallId", "20")],
            ),
            request(SHOW_CHANNEL_ACTION, &[("PbxCallId", "0")]),
            request(
                SHOW_MEDIA_STREAM_ACTION,
                &[
                    ("PbxCallId", "10"),
                    ("CallId", "11"),
                    ("Kind", "secret-kind"),
                    ("Direction", "receive"),
                ],
            ),
            request(
                SHOW_MEDIA_STATISTIC_ACTION,
                &[("DeviceId", "not-a-device"), ("CallId", "11")],
            ),
        ];
        for request in cases {
            let response = handle_runtime_status_request(&provider, request);
            assert_eq!(response.kind(), ManagerResponseKind::Error);
            assert!(!response.message().unwrap().contains("private-value"));
            assert!(!response.message().unwrap().contains("secret-kind"));
        }

        let mut sensitive = request(SHOW_CONFERENCES_ACTION, &[]);
        sensitive.fields.push(ManagerRequestField {
            name: "Authorization".into(),
            value: "do-not-disclose".into(),
            sensitive: true,
        });
        let response = handle_runtime_status_request(&provider, sensitive);
        assert_eq!(response.kind(), ManagerResponseKind::Error);
        assert!(!response.message().unwrap().contains("do-not-disclose"));
    }

    #[test]
    fn missing_conference_provider_failure_and_item_bounds_are_distinct() {
        let provider = provider();
        let missing = handle_runtime_status_request(
            &provider,
            request(SHOW_CONFERENCE_ACTION, &[("ConferenceId", "999")]),
        );
        assert_eq!(
            missing.message(),
            Some("Requested live object was not found")
        );

        let failed = handle_runtime_status_request(
            &FakeProvider {
                snapshot: RuntimeStatusSnapshot::default(),
                error: Some(RuntimeStatusProviderError::Unavailable),
            },
            request(SHOW_CHANNELS_ACTION, &[]),
        );
        assert_eq!(
            failed.message(),
            Some("Live management status is unavailable")
        );

        let call_snapshot = RuntimeStatusSnapshot {
            calls: (1..=MAX_CALL_ITEMS + 1)
                .map(|id| CallStatus {
                    pbx_id: PbxCallId(id as u64),
                    line: String::new(),
                    context: String::new(),
                    state: ChannelStateSummary::Connected,
                    direction: ChannelDirectionSummary::Inbound,
                    dialed_number: InventoryValue::Redacted,
                    privacy: true,
                    active_call_id: None,
                    appearance_count: 0,
                    conference_id: None,
                })
                .collect(),
            ..RuntimeStatusSnapshot::default()
        };
        let bounded = handle_runtime_status_request(
            &FakeProvider {
                snapshot: call_snapshot,
                error: None,
            },
            request(SHOW_CHANNELS_ACTION, &[]),
        );
        assert_eq!(
            bounded.message(),
            Some("Live-status result exceeds the bounded item limit")
        );

        let template = snapshot().media_statistics[0].clone();
        let media_statistics = (0..=MAX_MEDIA_ITEMS)
            .map(|index| {
                let mut item = template.clone();
                item.device_id = DeviceId::new(format!("SEP{index:012X}")).unwrap();
                item
            })
            .collect();
        let bounded = handle_runtime_status_request(
            &FakeProvider {
                snapshot: RuntimeStatusSnapshot {
                    media_statistics,
                    ..RuntimeStatusSnapshot::default()
                },
                error: None,
            },
            request(SHOW_MEDIA_STATISTICS_ACTION, &[]),
        );
        assert_eq!(
            bounded.message(),
            Some("Live-status result exceeds the bounded item limit")
        );
    }

    #[test]
    fn field_and_aggregate_response_byte_limits_fail_closed() {
        let call = |pbx_id, line: String| CallStatus {
            pbx_id: PbxCallId(pbx_id),
            line,
            context: String::new(),
            state: ChannelStateSummary::Connected,
            direction: ChannelDirectionSummary::Inbound,
            dialed_number: InventoryValue::Redacted,
            privacy: true,
            active_call_id: None,
            appearance_count: 0,
            conference_id: None,
        };
        let oversized = handle_runtime_status_request(
            &FakeProvider {
                snapshot: RuntimeStatusSnapshot {
                    calls: vec![call(1, "x".repeat(MAX_FIELD_VALUE_BYTES + 1))],
                    ..RuntimeStatusSnapshot::default()
                },
                error: None,
            },
            request(SHOW_CHANNELS_ACTION, &[]),
        );
        assert_eq!(
            oversized.message(),
            Some("Live-status response cannot be represented safely")
        );

        let aggregate = handle_runtime_status_request(
            &FakeProvider {
                snapshot: RuntimeStatusSnapshot {
                    calls: (1..=20)
                        .map(|id| call(id, "x".repeat(MAX_FIELD_VALUE_BYTES)))
                        .collect(),
                    ..RuntimeStatusSnapshot::default()
                },
                error: None,
            },
            request(SHOW_CHANNELS_ACTION, &[]),
        );
        assert_eq!(
            aggregate.message(),
            Some("Live-status response exceeds the bounded size limit")
        );
    }

    #[test]
    fn action_names_are_unique_and_registration_is_unavailable_in_development() {
        let names = RuntimeAction::ALL
            .into_iter()
            .map(RuntimeAction::name)
            .collect::<BTreeSet<_>>();
        assert_eq!(names.len(), RuntimeAction::ALL.len());
        for action in RuntimeAction::ALL {
            let synopsis = action.synopsis();
            assert!(!synopsis.is_empty());
            assert!(
                synopsis.len() <= 30,
                "{} synopsis is too long",
                action.name()
            );
            assert!(
                synopsis.is_ascii() && synopsis.bytes().all(|byte| !byte.is_ascii_control()),
                "{} synopsis is not printable ASCII",
                action.name()
            );
        }
        #[cfg(feature = "development")]
        assert!(matches!(
            register_runtime_status_actions(provider(), crate::ami::manager::UnavailableManager),
            Err(ManagerError::Unavailable)
        ));
    }
}
