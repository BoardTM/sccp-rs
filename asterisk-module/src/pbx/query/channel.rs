//! Typed, secret-safe active-channel information exposed to the dialplan.

use std::fmt;

use sccp_protocol::{CallId, Codec, DeviceId};
use thiserror::Error;

use crate::media::codec_preference::render_audio_preferences;
use crate::media::formats::PbxAudioFormat;
use crate::pbx::dialplan::{
    DialplanBackend, DialplanCallbackError, DialplanError, DialplanEscalation,
    DialplanFunctionHandlers, DialplanLimits,
};
use crate::pbx::party::AsteriskChannel;
use crate::runtime::backend::PbxCallId;

pub const CHANNEL_QUERY_FUNCTION: &str = "SCCPChannel";

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum ChannelQueryTarget {
    Current,
    Pbx(PbxCallId),
    Call(CallId),
    Name(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChannelQueryField {
    PbxId,
    CallId,
    Name,
    Line,
    Context,
    State,
    Direction,
    DialedNumber,
    Ani,
    Dnid,
    Rdnis,
    AccountCodeSet,
    Language,
    VariableCount,
    Privacy,
    Device,
    LineInstance,
    Codec,
    CodecId,
    AudioState,
    VideoState,
    AudioPacketMs,
    AudioPreferences,
    AppearanceCount,
    AppearanceOrder,
    AppearanceSummary,
}

impl ChannelQueryField {
    fn parse(value: &str) -> Result<Self, ChannelQueryError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "pbx_id" | "backend_id" => Ok(Self::PbxId),
            "call_id" | "callid" | "id" => Ok(Self::CallId),
            "name" | "channel" | "channel_name" => Ok(Self::Name),
            "line" => Ok(Self::Line),
            "context" => Ok(Self::Context),
            "state" => Ok(Self::State),
            "direction" | "calltype" => Ok(Self::Direction),
            "dialed_number" | "digits" => Ok(Self::DialedNumber),
            "ani" => Ok(Self::Ani),
            "dnid" => Ok(Self::Dnid),
            "rdnis" => Ok(Self::Rdnis),
            "account_code_set" => Ok(Self::AccountCodeSet),
            "language" => Ok(Self::Language),
            "variable_count" => Ok(Self::VariableCount),
            "privacy" => Ok(Self::Privacy),
            "device" | "active_device" => Ok(Self::Device),
            "line_instance" | "active_line_instance" => Ok(Self::LineInstance),
            "codec" | "codecs" => Ok(Self::Codec),
            "codec_id" | "format" | "format_id" => Ok(Self::CodecId),
            "audio_state" => Ok(Self::AudioState),
            "video_state" | "videomode" => Ok(Self::VideoState),
            "audio_packet_ms" | "packet_ms" => Ok(Self::AudioPacketMs),
            "audio_preferences" | "codec_preferences" | "preferences" => Ok(Self::AudioPreferences),
            "appearance_count" => Ok(Self::AppearanceCount),
            "appearance_order" => Ok(Self::AppearanceOrder),
            "appearance_summary" | "call_state_summary" => Ok(Self::AppearanceSummary),
            _ => Err(ChannelQueryError::UnknownField),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChannelQueryRequest {
    pub target: ChannelQueryTarget,
    pub field: ChannelQueryField,
}

impl ChannelQueryRequest {
    pub fn parse(arguments: &str) -> Result<Self, ChannelQueryError> {
        let mut parts = arguments.split(',');
        let target = parts.next().map(str::trim).unwrap_or_default();
        let field = parts.next().map(str::trim).unwrap_or_default();
        if target.is_empty() || field.is_empty() || parts.next().is_some() {
            return Err(ChannelQueryError::InvalidArguments);
        }
        Ok(Self {
            target: parse_target(target)?,
            field: ChannelQueryField::parse(field)?,
        })
    }
}

fn parse_target(value: &str) -> Result<ChannelQueryTarget, ChannelQueryError> {
    if value.eq_ignore_ascii_case("current") {
        return Ok(ChannelQueryTarget::Current);
    }
    if let Some(value) = strip_ascii_prefix(value, "pbx:") {
        return parse_nonzero_id(value)
            .map(|id| ChannelQueryTarget::Pbx(PbxCallId(id)))
            .ok_or(ChannelQueryError::InvalidTarget);
    }
    if let Some(value) = strip_ascii_prefix(value, "call:") {
        return parse_nonzero_id(value)
            .map(|id| ChannelQueryTarget::Call(CallId(id)))
            .ok_or(ChannelQueryError::InvalidTarget);
    }
    if value.bytes().all(|byte| byte.is_ascii_digit()) {
        return parse_nonzero_id(value)
            .map(|id| ChannelQueryTarget::Call(CallId(id)))
            .ok_or(ChannelQueryError::InvalidTarget);
    }
    let name = strip_ascii_prefix(value, "name:").unwrap_or(value);
    if name.is_empty()
        || name.len() > 255
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && byte != b',')
    {
        return Err(ChannelQueryError::InvalidTarget);
    }
    Ok(ChannelQueryTarget::Name(name.to_owned()))
}

fn strip_ascii_prefix<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    value
        .get(..prefix.len())
        .filter(|candidate| candidate.eq_ignore_ascii_case(prefix))
        .and_then(|_| value.get(prefix.len()..))
}

fn parse_nonzero_id(value: &str) -> Option<u64> {
    (!value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| value.parse::<u64>().ok().filter(|id| *id != 0))
        .flatten()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChannelDirectionSummary {
    Inbound,
    Outbound,
}

impl fmt::Display for ChannelDirectionSummary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Inbound => "inbound",
            Self::Outbound => "outbound",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChannelStateSummary {
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

impl fmt::Display for ChannelStateSummary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Collecting => "collecting",
            Self::PickupCollecting => "pickup_collecting",
            Self::Ringing => "ringing",
            Self::Calling => "calling",
            Self::Connected => "connected",
            Self::Parking => "parking",
            Self::Retrieving => "retrieving",
            Self::Held => "held",
            Self::RemoteInUse => "remote_in_use",
            Self::SharedHeld => "shared_held",
            Self::Barged => "barged",
            Self::TransferCollecting => "transfer_collecting",
            Self::Ended => "ended",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChannelMediaStateSummary {
    Closed,
    Opening,
    Open,
}

impl fmt::Display for ChannelMediaStateSummary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Closed => "closed",
            Self::Opening => "opening",
            Self::Open => "open",
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChannelAppearanceSnapshot {
    pub call_id: CallId,
    pub device_id: DeviceId,
    pub line_instance: u32,
    pub state: ChannelStateSummary,
    pub privacy: bool,
    pub codec: Codec,
    pub audio: ChannelMediaStateSummary,
    pub video: ChannelMediaStateSummary,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChannelQuerySnapshot {
    pub pbx_id: PbxCallId,
    pub name: Option<String>,
    pub line: String,
    pub context: String,
    pub state: ChannelStateSummary,
    pub direction: ChannelDirectionSummary,
    pub dialed_number: String,
    /// Presentation-filtered ANI. Restricted identities are absent.
    pub ani: Option<String>,
    pub dnid: Option<String>,
    /// Presentation-filtered redirecting number. Restricted identities are absent.
    pub rdnis: Option<String>,
    /// Account codes remain opaque; diagnostics expose presence only.
    pub account_code_set: bool,
    pub language: Option<String>,
    /// Variable names and values remain opaque.
    pub variable_count: usize,
    pub privacy: bool,
    /// Exact appearance selected through a handset call ID, when applicable.
    pub selected_call_id: Option<CallId>,
    pub active_call_id: Option<CallId>,
    pub audio_packet_ms: Option<u32>,
    pub audio_preferences: Vec<PbxAudioFormat>,
    /// Sorted by device identity, then line instance, then handset call ID.
    pub appearances: Vec<ChannelAppearanceSnapshot>,
}

impl ChannelQuerySnapshot {
    fn sort_appearances(&mut self) {
        self.appearances.sort_by(|left, right| {
            (&left.device_id, left.line_instance, left.call_id.0).cmp(&(
                &right.device_id,
                right.line_instance,
                right.call_id.0,
            ))
        });
    }

    fn selected_appearance(&self) -> Option<&ChannelAppearanceSnapshot> {
        let call_id = self.selected_call_id.or(self.active_call_id)?;
        self.appearances
            .iter()
            .find(|appearance| appearance.call_id == call_id)
    }

    pub fn value(&self, field: ChannelQueryField) -> ChannelQueryValue {
        let selected = self.selected_appearance();
        match field {
            ChannelQueryField::PbxId => ChannelQueryValue::Unsigned(self.pbx_id.0),
            ChannelQueryField::CallId => ChannelQueryValue::OptionalUnsigned(
                self.selected_call_id.or(self.active_call_id).map(|id| id.0),
            ),
            ChannelQueryField::Name => ChannelQueryValue::OptionalText(self.name.clone()),
            ChannelQueryField::Line => ChannelQueryValue::Text(self.line.clone()),
            ChannelQueryField::Context => ChannelQueryValue::Text(self.context.clone()),
            ChannelQueryField::State => ChannelQueryValue::State(self.state),
            ChannelQueryField::Direction => ChannelQueryValue::Direction(self.direction),
            ChannelQueryField::DialedNumber => ChannelQueryValue::Text(self.dialed_number.clone()),
            ChannelQueryField::Ani => ChannelQueryValue::OptionalText(self.ani.clone()),
            ChannelQueryField::Dnid => ChannelQueryValue::OptionalText(self.dnid.clone()),
            ChannelQueryField::Rdnis => ChannelQueryValue::OptionalText(self.rdnis.clone()),
            ChannelQueryField::AccountCodeSet => ChannelQueryValue::Boolean(self.account_code_set),
            ChannelQueryField::Language => ChannelQueryValue::OptionalText(self.language.clone()),
            ChannelQueryField::VariableCount => {
                ChannelQueryValue::Unsigned(self.variable_count as u64)
            }
            ChannelQueryField::Privacy => ChannelQueryValue::Boolean(self.privacy),
            ChannelQueryField::Device => ChannelQueryValue::OptionalText(
                selected.map(|appearance| appearance.device_id.to_string()),
            ),
            ChannelQueryField::LineInstance => ChannelQueryValue::OptionalUnsigned(
                selected.map(|appearance| u64::from(appearance.line_instance)),
            ),
            ChannelQueryField::Codec => {
                ChannelQueryValue::OptionalCodec(selected.map(|appearance| appearance.codec))
            }
            ChannelQueryField::CodecId => ChannelQueryValue::OptionalUnsigned(
                selected.map(|appearance| u64::from(appearance.codec.wire_value())),
            ),
            ChannelQueryField::AudioState => {
                ChannelQueryValue::OptionalMediaState(selected.map(|appearance| appearance.audio))
            }
            ChannelQueryField::VideoState => {
                ChannelQueryValue::OptionalMediaState(selected.map(|appearance| appearance.video))
            }
            ChannelQueryField::AudioPacketMs => {
                ChannelQueryValue::OptionalUnsigned(self.audio_packet_ms.map(u64::from))
            }
            ChannelQueryField::AudioPreferences => {
                ChannelQueryValue::AudioPreferences(self.audio_preferences.clone())
            }
            ChannelQueryField::AppearanceCount => {
                ChannelQueryValue::Unsigned(self.appearances.len() as u64)
            }
            ChannelQueryField::AppearanceOrder => {
                ChannelQueryValue::AppearanceOrder(self.appearances.clone())
            }
            ChannelQueryField::AppearanceSummary => {
                ChannelQueryValue::AppearanceSummary(self.appearances.clone())
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ChannelQueryValue {
    Text(String),
    OptionalText(Option<String>),
    Boolean(bool),
    Unsigned(u64),
    OptionalUnsigned(Option<u64>),
    State(ChannelStateSummary),
    Direction(ChannelDirectionSummary),
    OptionalCodec(Option<Codec>),
    OptionalMediaState(Option<ChannelMediaStateSummary>),
    AudioPreferences(Vec<PbxAudioFormat>),
    AppearanceOrder(Vec<ChannelAppearanceSnapshot>),
    AppearanceSummary(Vec<ChannelAppearanceSnapshot>),
}

impl ChannelQueryValue {
    pub fn render(&self) -> String {
        match self {
            Self::Text(value) => value.clone(),
            Self::OptionalText(value) => value.clone().unwrap_or_default(),
            Self::Boolean(value) => value.to_string(),
            Self::Unsigned(value) => value.to_string(),
            Self::OptionalUnsigned(value) => {
                value.map_or_else(String::new, |value| value.to_string())
            }
            Self::State(value) => value.to_string(),
            Self::Direction(value) => value.to_string(),
            Self::OptionalCodec(value) => value.map_or_else(String::new, codec_name),
            Self::OptionalMediaState(value) => {
                value.map_or_else(String::new, |value| value.to_string())
            }
            Self::AudioPreferences(value) => render_audio_preferences(value),
            Self::AppearanceOrder(appearances) => appearances
                .iter()
                .map(|appearance| {
                    format!(
                        "{}@{}:{}",
                        appearance.call_id.0, appearance.device_id, appearance.line_instance
                    )
                })
                .collect::<Vec<_>>()
                .join(","),
            Self::AppearanceSummary(appearances) => appearances
                .iter()
                .map(|appearance| {
                    format!(
                        "{}@{}:{}:state={}:codec={}:audio={}:video={}:privacy={}",
                        appearance.call_id.0,
                        appearance.device_id,
                        appearance.line_instance,
                        appearance.state,
                        codec_name(appearance.codec),
                        appearance.audio,
                        appearance.video,
                        appearance.privacy,
                    )
                })
                .collect::<Vec<_>>()
                .join(","),
        }
    }
}

fn codec_name(codec: Codec) -> String {
    match codec {
        Codec::Pcmu => "g711-ulaw-64k".into(),
        Codec::G711Ulaw56k => "g711-ulaw-56k".into(),
        Codec::Pcma => "g711-alaw-64k".into(),
        Codec::G711Alaw56k => "g711-alaw-56k".into(),
        Codec::G72264k => "g722-64k".into(),
        Codec::G72256k => "g722-56k".into(),
        Codec::G72248k => "g722-48k".into(),
        codec => format!("codec-{}", codec.wire_value()),
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ChannelQueryLookupError {
    #[error("the current PBX channel is not owned by this driver")]
    CurrentChannelUnavailable,
    #[error("more than one active channel has the requested name")]
    AmbiguousChannelName,
    #[error("channel state is unavailable")]
    Unavailable,
}

pub trait ChannelQueryProvider: Send + Sync + 'static {
    fn snapshot(
        &self,
        target: &ChannelQueryTarget,
        channel: Option<&AsteriskChannel<'_>>,
    ) -> Result<Option<ChannelQuerySnapshot>, ChannelQueryLookupError>;
}

pub struct ChannelQuery<P> {
    provider: P,
}

impl<P: ChannelQueryProvider> ChannelQuery<P> {
    pub const fn new(provider: P) -> Self {
        Self { provider }
    }

    pub fn execute(
        &self,
        arguments: &str,
        channel: Option<&AsteriskChannel<'_>>,
    ) -> Result<ChannelQueryValue, ChannelQueryError> {
        let request = ChannelQueryRequest::parse(arguments)?;
        let mut snapshot = self
            .provider
            .snapshot(&request.target, channel)
            .map_err(ChannelQueryError::Lookup)?
            .ok_or(ChannelQueryError::UnknownChannel)?;
        snapshot.sort_appearances();
        Ok(snapshot.value(request.field))
    }
}

pub fn register_channel_query<P: ChannelQueryProvider, B: DialplanBackend>(
    provider: P,
    backend: B,
) -> Result<B::Registration, DialplanError> {
    let query = ChannelQuery::new(provider);
    backend.register_function(
        CHANNEL_QUERY_FUNCTION,
        "Read active channel state",
        "Read one allowlisted call, appearance, or media-state field",
        DialplanEscalation::None,
        DialplanLimits {
            max_arguments_bytes: 384,
            max_value_bytes: 1,
            max_output_bytes: 4096,
        },
        DialplanFunctionHandlers::new().with_read(move |request| {
            query
                .execute(&request.arguments, request.channel.as_ref())
                .map(|value| value.render())
                .map_err(|_| DialplanCallbackError::Failed)
        }),
    )
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ChannelQueryError {
    #[error("channel query expects exactly channel,field")]
    InvalidArguments,
    #[error("channel query contains an invalid current, call, PBX, or channel-name target")]
    InvalidTarget,
    #[error("channel query field is not allowlisted")]
    UnknownField,
    #[error("channel query target is unknown")]
    UnknownChannel,
    #[error(transparent)]
    Lookup(#[from] ChannelQueryLookupError),
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    struct FakeProvider {
        channels: HashMap<ChannelQueryTarget, ChannelQuerySnapshot>,
        current: Option<PbxCallId>,
        failure: Option<ChannelQueryLookupError>,
    }

    impl ChannelQueryProvider for FakeProvider {
        fn snapshot(
            &self,
            target: &ChannelQueryTarget,
            channel: Option<&AsteriskChannel<'_>>,
        ) -> Result<Option<ChannelQuerySnapshot>, ChannelQueryLookupError> {
            if let Some(error) = self.failure {
                return Err(error);
            }
            let target = match target {
                ChannelQueryTarget::Current => {
                    if channel.is_none() {
                        return Err(ChannelQueryLookupError::CurrentChannelUnavailable);
                    }
                    ChannelQueryTarget::Pbx(
                        self.current
                            .ok_or(ChannelQueryLookupError::CurrentChannelUnavailable)?,
                    )
                }
                target => target.clone(),
            };
            let mut snapshot = self.channels.get(&target).cloned();
            if let (ChannelQueryTarget::Call(call_id), Some(snapshot)) = (&target, &mut snapshot) {
                snapshot.selected_call_id = Some(*call_id);
            }
            Ok(snapshot)
        }
    }

    fn device(value: &str) -> DeviceId {
        DeviceId::new(value).unwrap()
    }

    fn snapshot() -> ChannelQuerySnapshot {
        let mut snapshot = ChannelQuerySnapshot {
            pbx_id: PbxCallId(42),
            name: Some("SCCP/1001-0000002a".into()),
            line: "1001".into(),
            context: "internal".into(),
            state: ChannelStateSummary::Connected,
            direction: ChannelDirectionSummary::Outbound,
            dialed_number: "12065550100".into(),
            ani: None,
            dnid: Some("12065550100".into()),
            rdnis: None,
            account_code_set: true,
            language: Some("en".into()),
            variable_count: 2,
            privacy: true,
            selected_call_id: None,
            active_call_id: Some(CallId(11)),
            audio_packet_ms: Some(20),
            audio_preferences: vec![PbxAudioFormat::G722, PbxAudioFormat::G711Ulaw],
            appearances: vec![
                ChannelAppearanceSnapshot {
                    call_id: CallId(11),
                    device_id: device("SEP001122334455"),
                    line_instance: 1,
                    state: ChannelStateSummary::Connected,
                    privacy: true,
                    codec: Codec::Pcmu,
                    audio: ChannelMediaStateSummary::Open,
                    video: ChannelMediaStateSummary::Closed,
                },
                ChannelAppearanceSnapshot {
                    call_id: CallId(12),
                    device_id: device("SEP112233445566"),
                    line_instance: 3,
                    state: ChannelStateSummary::RemoteInUse,
                    privacy: false,
                    codec: Codec::Pcma,
                    audio: ChannelMediaStateSummary::Closed,
                    video: ChannelMediaStateSummary::Closed,
                },
            ],
        };
        snapshot.appearances.reverse();
        snapshot
    }

    fn query() -> ChannelQuery<FakeProvider> {
        let snapshot = snapshot();
        ChannelQuery::new(FakeProvider {
            channels: [
                (ChannelQueryTarget::Pbx(PbxCallId(42)), snapshot.clone()),
                (ChannelQueryTarget::Call(CallId(11)), snapshot.clone()),
                (ChannelQueryTarget::Call(CallId(12)), snapshot.clone()),
                (
                    ChannelQueryTarget::Name("SCCP/1001-0000002a".into()),
                    snapshot,
                ),
            ]
            .into(),
            current: Some(PbxCallId(42)),
            failure: None,
        })
    }

    #[test]
    fn parser_distinguishes_current_call_pbx_and_name_targets() {
        for (arguments, target) in [
            ("current,state", ChannelQueryTarget::Current),
            ("11,state", ChannelQueryTarget::Call(CallId(11))),
            ("call:11,state", ChannelQueryTarget::Call(CallId(11))),
            ("PBX:42,state", ChannelQueryTarget::Pbx(PbxCallId(42))),
            (
                "SCCP/1001-0000002a,state",
                ChannelQueryTarget::Name("SCCP/1001-0000002a".into()),
            ),
            (
                "NAME:SCCP/1001-0000002a,state",
                ChannelQueryTarget::Name("SCCP/1001-0000002a".into()),
            ),
        ] {
            assert_eq!(
                ChannelQueryRequest::parse(arguments).unwrap().target,
                target
            );
        }
    }

    #[test]
    fn malformed_targets_and_non_allowlisted_fields_fail_closed() {
        for arguments in [
            "",
            "current",
            "current,state,extra",
            "pbx:0,state",
            "pbx:no,state",
            "call:,state",
            "name:,state",
            "bad channel,state",
            "current,password",
            "current,secret",
            "current,token",
            "current,private_key",
            "current,caller_password",
        ] {
            assert!(
                ChannelQueryRequest::parse(arguments).is_err(),
                "{arguments}"
            );
        }
        let oversized = format!("{},state", "x".repeat(256));
        assert_eq!(
            ChannelQueryRequest::parse(&oversized),
            Err(ChannelQueryError::InvalidTarget)
        );
        assert_eq!(
            ChannelQueryRequest::parse("bad\nname,state"),
            Err(ChannelQueryError::InvalidTarget)
        );
    }

    #[test]
    fn core_fields_are_typed_and_render_deterministically() {
        let query = query();
        for (field, expected) in [
            ("pbx_id", ChannelQueryValue::Unsigned(42)),
            ("callid", ChannelQueryValue::OptionalUnsigned(Some(11))),
            (
                "channel_name",
                ChannelQueryValue::OptionalText(Some("SCCP/1001-0000002a".into())),
            ),
            ("line", ChannelQueryValue::Text("1001".into())),
            ("context", ChannelQueryValue::Text("internal".into())),
            (
                "state",
                ChannelQueryValue::State(ChannelStateSummary::Connected),
            ),
            (
                "calltype",
                ChannelQueryValue::Direction(ChannelDirectionSummary::Outbound),
            ),
            ("privacy", ChannelQueryValue::Boolean(true)),
            (
                "dialed_number",
                ChannelQueryValue::Text("12065550100".into()),
            ),
            ("ani", ChannelQueryValue::OptionalText(None)),
            (
                "dnid",
                ChannelQueryValue::OptionalText(Some("12065550100".into())),
            ),
            ("rdnis", ChannelQueryValue::OptionalText(None)),
            ("account_code_set", ChannelQueryValue::Boolean(true)),
            (
                "language",
                ChannelQueryValue::OptionalText(Some("en".into())),
            ),
            ("variable_count", ChannelQueryValue::Unsigned(2)),
        ] {
            assert_eq!(
                query.execute(&format!("pbx:42,{field}"), None),
                Ok(expected)
            );
        }
    }

    #[test]
    fn active_appearance_media_and_codec_fields_are_typed() {
        let query = query();
        for (field, expected) in [
            (
                "device",
                ChannelQueryValue::OptionalText(Some("SEP001122334455".into())),
            ),
            (
                "line_instance",
                ChannelQueryValue::OptionalUnsigned(Some(1)),
            ),
            ("codec", ChannelQueryValue::OptionalCodec(Some(Codec::Pcmu))),
            ("codec_id", ChannelQueryValue::OptionalUnsigned(Some(4))),
            (
                "audio_state",
                ChannelQueryValue::OptionalMediaState(Some(ChannelMediaStateSummary::Open)),
            ),
            (
                "video_state",
                ChannelQueryValue::OptionalMediaState(Some(ChannelMediaStateSummary::Closed)),
            ),
            ("packet_ms", ChannelQueryValue::OptionalUnsigned(Some(20))),
            (
                "codec_preferences",
                ChannelQueryValue::AudioPreferences(vec![
                    PbxAudioFormat::G722,
                    PbxAudioFormat::G711Ulaw,
                ]),
            ),
        ] {
            assert_eq!(query.execute(&format!("11,{field}"), None), Ok(expected));
        }
        assert_eq!(
            query.execute("11,codec", None).unwrap().render(),
            "g711-ulaw-64k"
        );
        assert_eq!(
            query
                .execute("11,audio_preferences", None)
                .unwrap()
                .render(),
            "g722,ulaw"
        );
    }

    #[test]
    fn appearance_order_and_summary_sort_untrusted_provider_input() {
        let query = query();
        assert_eq!(
            query.execute("11,appearance_count", None),
            Ok(ChannelQueryValue::Unsigned(2))
        );
        assert_eq!(
            query.execute("11,appearance_order", None).unwrap().render(),
            "11@SEP001122334455:1,12@SEP112233445566:3"
        );
        assert_eq!(
            query
                .execute("11,appearance_summary", None)
                .unwrap()
                .render(),
            "11@SEP001122334455:1:state=connected:codec=g711-ulaw-64k:audio=open:video=closed:privacy=true,12@SEP112233445566:3:state=remote_in_use:codec=g711-alaw-64k:audio=closed:video=closed:privacy=false"
        );
    }

    #[test]
    fn explicit_target_forms_select_the_same_channel() {
        let query = query();
        for target in ["11", "call:11", "pbx:42", "SCCP/1001-0000002a"] {
            assert_eq!(
                query.execute(&format!("{target},pbx_id"), None),
                Ok(ChannelQueryValue::Unsigned(42))
            );
        }
        assert_eq!(
            query.execute("12,callid", None),
            Ok(ChannelQueryValue::OptionalUnsigned(Some(12)))
        );
        assert_eq!(
            query.execute("12,device", None),
            Ok(ChannelQueryValue::OptionalText(Some(
                "SEP112233445566".into()
            )))
        );
    }

    #[test]
    fn current_requires_callback_channel_and_preserves_lookup_failures() {
        assert_eq!(
            query().execute("current,state", None),
            Err(ChannelQueryError::Lookup(
                ChannelQueryLookupError::CurrentChannelUnavailable
            ))
        );
        let storage = 1_u8;
        let channel = unsafe {
            AsteriskChannel::from_raw(std::ptr::from_ref(&storage).cast_mut().cast()).unwrap()
        };
        assert_eq!(
            query().execute("current,state", Some(&channel)),
            Ok(ChannelQueryValue::State(ChannelStateSummary::Connected))
        );
        let unavailable = ChannelQuery::new(FakeProvider {
            channels: HashMap::new(),
            current: None,
            failure: Some(ChannelQueryLookupError::Unavailable),
        });
        assert_eq!(
            unavailable.execute("pbx:42,state", None),
            Err(ChannelQueryError::Lookup(
                ChannelQueryLookupError::Unavailable
            ))
        );
    }

    #[test]
    fn unknown_and_ambiguous_channels_are_distinct() {
        assert_eq!(
            query().execute("pbx:99,state", None),
            Err(ChannelQueryError::UnknownChannel)
        );
        let ambiguous = ChannelQuery::new(FakeProvider {
            channels: HashMap::new(),
            current: None,
            failure: Some(ChannelQueryLookupError::AmbiguousChannelName),
        });
        assert_eq!(
            ambiguous.execute("SCCP/1001-0000002a,state", None),
            Err(ChannelQueryError::Lookup(
                ChannelQueryLookupError::AmbiguousChannelName
            ))
        );
    }

    #[test]
    fn registration_is_unavailable_without_native_linkage() {
        assert!(matches!(
            register_channel_query(
                FakeProvider {
                    channels: HashMap::new(),
                    current: None,
                    failure: None,
                },
                crate::pbx::dialplan::UnavailableDialplan
            ),
            Err(DialplanError::Unavailable)
        ));
    }
}
