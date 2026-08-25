//! Bounded native CLI views over live media and station-session snapshots.
//!
//! Rendering is independent of the native CLI ABI. Callbacks copy bounded
//! arguments into Rust and obtain one immutable inventory/runtime snapshot
//! before calling this module. Media endpoints are hidden for private calls,
//! and retained statistics expose only typed counters plus the size of an
//! opaque quality report.

use std::collections::BTreeSet;
use std::fmt::{self, Write as _};

use sccp_protocol::{CallId, DeviceId, MediaEndpoint};
use thiserror::Error;

use crate::ami::inventory::{InventoryDevice, InventorySnapshot};
use crate::ami::runtime::{
    MediaDirection, MediaKind, MediaStatisticsStatus, MediaStreamStatus, RuntimeStatusSnapshot,
};
use crate::runtime::backend::PbxCallId;

pub const MAX_CLI_DIAGNOSTIC_ARGUMENTS: usize = 4;
pub const MAX_CLI_DIAGNOSTIC_ARGUMENT_BYTES: usize = 128;

const MAX_MEDIA_ITEMS: usize = 24;
const MAX_SESSION_ITEMS: usize = 40;
const MAX_VALUE_BYTES: usize = 1024;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CliDiagnosticCommand {
    Media,
    MediaStatistics,
    Sessions,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CliDiagnosticSnapshot {
    pub inventory: InventorySnapshot,
    pub runtime: RuntimeStatusSnapshot,
    pub session_calls: Vec<CliSessionCall>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CliSessionCall {
    pub device_id: DeviceId,
    pub pbx_id: PbxCallId,
    pub call_id: CallId,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum CliDiagnosticError {
    #[error("invalid diagnostic selector")]
    InvalidSelector,
    #[error("requested diagnostic object was not found")]
    NotFound,
    #[error("diagnostic snapshot contains duplicate identities")]
    DuplicateObject,
    #[error("diagnostic result exceeds the bounded item limit")]
    TooManyItems,
    #[error("diagnostic output exceeds the bounded size limit")]
    OutputTooLarge,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum DiagnosticRequest {
    Media {
        pbx_id: Option<PbxCallId>,
        call_id: Option<u64>,
        kind: Option<MediaKind>,
        direction: Option<MediaDirection>,
    },
    MediaStatistics {
        device_id: Option<DeviceId>,
        call_id: Option<u64>,
    },
    Sessions {
        device_id: Option<DeviceId>,
    },
}

pub fn render_cli_diagnostics(
    command: CliDiagnosticCommand,
    arguments: &[&str],
    snapshot: &CliDiagnosticSnapshot,
) -> Result<String, CliDiagnosticError> {
    validate_arguments(arguments)?;
    let request = DiagnosticRequest::parse(command, arguments)?;
    let mut snapshot = snapshot.clone();
    snapshot.normalize()?;
    request.render(&snapshot)
}

pub fn complete_cli_diagnostics(
    command: CliDiagnosticCommand,
    arguments: &[&str],
    prefix: &str,
    ordinal: usize,
    snapshot: &CliDiagnosticSnapshot,
) -> Option<String> {
    if validate_arguments(arguments).is_err()
        || prefix.len() > MAX_CLI_DIAGNOSTIC_ARGUMENT_BYTES
        || prefix.chars().any(char::is_control)
    {
        return None;
    }
    let mut snapshot = snapshot.clone();
    snapshot.normalize().ok()?;
    completion_candidates(command, arguments, &snapshot)
        .into_iter()
        .filter(|candidate| starts_with_ignore_ascii_case(candidate, prefix))
        .take(MAX_SESSION_ITEMS)
        .nth(ordinal)
}

impl CliDiagnosticSnapshot {
    fn normalize(&mut self) -> Result<(), CliDiagnosticError> {
        self.inventory
            .devices
            .sort_by(|left, right| left.id.cmp(&right.id));
        if self
            .inventory
            .devices
            .windows(2)
            .any(|items| items[0].id == items[1].id)
        {
            return Err(CliDiagnosticError::DuplicateObject);
        }
        self.runtime
            .media_streams
            .sort_by(|left, right| media_identity(left).cmp(&media_identity(right)));
        if self
            .runtime
            .media_streams
            .windows(2)
            .any(|items| media_identity(&items[0]) == media_identity(&items[1]))
        {
            return Err(CliDiagnosticError::DuplicateObject);
        }
        self.runtime.media_statistics.sort_by(|left, right| {
            (&left.device_id, left.snapshot.request_generation)
                .cmp(&(&right.device_id, right.snapshot.request_generation))
        });
        if self
            .runtime
            .media_statistics
            .windows(2)
            .any(|items| items[0].device_id == items[1].device_id)
        {
            return Err(CliDiagnosticError::DuplicateObject);
        }
        for statistics in &mut self.runtime.media_statistics {
            statistics.enforce_privacy();
        }
        self.session_calls.sort_by(|left, right| {
            (&left.device_id, left.pbx_id.0, left.call_id.0).cmp(&(
                &right.device_id,
                right.pbx_id.0,
                right.call_id.0,
            ))
        });
        if self.session_calls.windows(2).any(|items| {
            items[0].device_id == items[1].device_id
                && items[0].pbx_id == items[1].pbx_id
                && items[0].call_id == items[1].call_id
        }) {
            return Err(CliDiagnosticError::DuplicateObject);
        }
        Ok(())
    }
}

impl DiagnosticRequest {
    fn parse(
        command: CliDiagnosticCommand,
        arguments: &[&str],
    ) -> Result<Self, CliDiagnosticError> {
        match command {
            CliDiagnosticCommand::Media => Self::parse_media(arguments),
            CliDiagnosticCommand::MediaStatistics => Self::parse_statistics(arguments),
            CliDiagnosticCommand::Sessions => Self::parse_sessions(arguments),
        }
    }

    fn parse_media(arguments: &[&str]) -> Result<Self, CliDiagnosticError> {
        if arguments.len() > MAX_CLI_DIAGNOSTIC_ARGUMENTS {
            return Err(CliDiagnosticError::InvalidSelector);
        }
        let pbx_id = arguments
            .first()
            .map(|value| parse_positive(value).map(PbxCallId))
            .transpose()?;
        let call_id = arguments
            .get(1)
            .map(|value| parse_positive(value))
            .transpose()?;
        let kind = arguments
            .get(2)
            .map(|value| parse_media_kind(value))
            .transpose()?;
        let direction = arguments
            .get(3)
            .map(|value| parse_media_direction(value))
            .transpose()?;
        Ok(Self::Media {
            pbx_id,
            call_id,
            kind,
            direction,
        })
    }

    fn parse_statistics(arguments: &[&str]) -> Result<Self, CliDiagnosticError> {
        let device_id = arguments
            .first()
            .map(|value| parse_device(value))
            .transpose()?;
        let call_id = arguments
            .get(1)
            .map(|value| parse_positive(value))
            .transpose()?;
        if arguments.len() > 2 {
            return Err(CliDiagnosticError::InvalidSelector);
        }
        Ok(Self::MediaStatistics { device_id, call_id })
    }

    fn parse_sessions(arguments: &[&str]) -> Result<Self, CliDiagnosticError> {
        let device_id = arguments
            .first()
            .map(|value| parse_device(value))
            .transpose()?;
        if arguments.len() > 1 {
            return Err(CliDiagnosticError::InvalidSelector);
        }
        Ok(Self::Sessions { device_id })
    }

    fn render(self, snapshot: &CliDiagnosticSnapshot) -> Result<String, CliDiagnosticError> {
        let mut output = DiagnosticOutput::default();
        match self {
            Self::Media {
                pbx_id,
                call_id,
                kind,
                direction,
            } => {
                let items = snapshot
                    .runtime
                    .media_streams
                    .iter()
                    .filter(|item| pbx_id.is_none_or(|value| item.pbx_id == value))
                    .filter(|item| call_id.is_none_or(|value| item.call_id.0 == value))
                    .filter(|item| kind.is_none_or(|value| item.kind == value))
                    .filter(|item| direction.is_none_or(|value| item.direction == value))
                    .collect::<Vec<_>>();
                let detail = direction.is_some();
                require_selected(&items, pbx_id.is_some())?;
                if detail {
                    render_media_detail(&mut output, only_item(&items)?)?;
                } else {
                    render_media_list(&mut output, &items)?;
                }
            }
            Self::MediaStatistics { device_id, call_id } => {
                let items = snapshot
                    .runtime
                    .media_statistics
                    .iter()
                    .filter(|item| {
                        device_id
                            .as_ref()
                            .is_none_or(|value| item.device_id == *value)
                    })
                    .filter(|item| call_id.is_none_or(|value| item.snapshot.call_id.0 == value))
                    .collect::<Vec<_>>();
                require_selected(&items, device_id.is_some())?;
                if device_id.is_some() {
                    render_statistics_detail(&mut output, only_item(&items)?)?;
                } else {
                    render_statistics_list(&mut output, &items)?;
                }
            }
            Self::Sessions { device_id } => {
                let items = registered_sessions(&snapshot.inventory)
                    .filter(|item| device_id.as_ref().is_none_or(|value| item.id == *value))
                    .collect::<Vec<_>>();
                require_selected(&items, device_id.is_some())?;
                if device_id.is_some() {
                    render_session_detail(&mut output, only_item(&items)?, snapshot)?;
                } else {
                    render_session_list(&mut output, &items, snapshot)?;
                }
            }
        }
        Ok(output.finish())
    }
}

fn render_media_list(
    output: &mut DiagnosticOutput,
    items: &[&MediaStreamStatus],
) -> Result<(), CliDiagnosticError> {
    ensure_item_bound(items.len(), MAX_MEDIA_ITEMS)?;
    output.line(format_args!(
        "PBX\tCall\tDevice\tLine\tKind\tDirection\tState\tPrivacy\tCodec\tPacket ms"
    ))?;
    for item in items {
        let endpoint = item.endpoint;
        output.line(format_args!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            item.pbx_id.0,
            item.call_id.0,
            clean_value(item.device_id.as_str()),
            item.line_instance,
            media_kind(item.kind),
            media_direction(item.direction),
            item.state,
            yes_no(item.privacy),
            endpoint.map_or_else(
                || "-".to_owned(),
                |value| value.codec.wire_value().to_string()
            ),
            endpoint.map_or_else(|| "-".to_owned(), |value| value.packet_ms.to_string()),
        ))?;
    }
    Ok(())
}

fn render_media_detail(
    output: &mut DiagnosticOutput,
    item: &MediaStreamStatus,
) -> Result<(), CliDiagnosticError> {
    output.field("PBX call ID", item.pbx_id.0)?;
    output.field("Call ID", item.call_id.0)?;
    output.field("Device", item.device_id.as_str())?;
    output.field("Line instance", item.line_instance)?;
    output.field("Kind", media_kind(item.kind))?;
    output.field("Direction", media_direction(item.direction))?;
    output.field("State", item.state)?;
    output.field("Privacy", yes_no(item.privacy))?;
    render_endpoint(output, item.endpoint, item.privacy)
}

fn render_endpoint(
    output: &mut DiagnosticOutput,
    endpoint: Option<MediaEndpoint>,
    private: bool,
) -> Result<(), CliDiagnosticError> {
    let Some(endpoint) = endpoint else {
        output.field("Address", "-")?;
        output.field("RTP port", "-")?;
        output.field("RTCP port", "-")?;
        output.field("Codec ID", "-")?;
        output.field("Packet ms", "-")?;
        output.field("Max frames", "-")?;
        output.field("Telephone-event payload", "-")?;
        return Ok(());
    };
    if private {
        output.field("Address", "<redacted>")?;
        output.field("RTP port", "<redacted>")?;
        output.field("RTCP port", "<redacted>")?;
    } else {
        output.field("Address", endpoint.address)?;
        output.field("RTP port", endpoint.rtp_port)?;
        output.field("RTCP port", endpoint.rtcp_port)?;
    }
    output.field("Codec ID", endpoint.codec.wire_value())?;
    output.field("Packet ms", endpoint.packet_ms)?;
    output.field("Max frames", endpoint.max_frames_per_packet)?;
    output.field("Telephone-event payload", endpoint.telephone_event_payload)
}

fn render_statistics_list(
    output: &mut DiagnosticOutput,
    items: &[&MediaStatisticsStatus],
) -> Result<(), CliDiagnosticError> {
    ensure_item_bound(items.len(), MAX_MEDIA_ITEMS)?;
    output.line(format_args!(
        "Device\tGeneration\tCall\tCodec\tSent\tReceived\tLost\tJitter ms\tLatency ms\tQuality bytes"
    ))?;
    for item in items {
        let value = &item.snapshot;
        output.line(format_args!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            clean_value(item.device_id.as_str()),
            value.request_generation,
            value.call_id.0,
            value.codec.wire_value(),
            value.packets_sent,
            value.packets_received,
            value.packets_lost,
            value.jitter_millis,
            value.latency_millis,
            value.quality_byte_count,
        ))?;
    }
    Ok(())
}

fn render_statistics_detail(
    output: &mut DiagnosticOutput,
    item: &MediaStatisticsStatus,
) -> Result<(), CliDiagnosticError> {
    let value = &item.snapshot;
    output.field("Device", item.device_id.as_str())?;
    output.field("Privacy", yes_no(item.privacy.is_private()))?;
    output.field("Request generation", value.request_generation)?;
    output.field("Call ID", value.call_id.0)?;
    output.field("Line instance", value.line_instance)?;
    output.field("Codec ID", value.codec.wire_value())?;
    output.field("Packet ms", value.packet_ms)?;
    output.field("Max frames", value.max_frames_per_packet)?;
    output.field("Packets sent", value.packets_sent)?;
    output.field("Octets sent", value.octets_sent)?;
    output.field("Packets received", value.packets_received)?;
    output.field("Octets received", value.octets_received)?;
    output.field("Packets lost", value.packets_lost)?;
    output.field("Jitter ms", value.jitter_millis)?;
    output.field("Latency ms", value.latency_millis)?;
    if !item.privacy.is_private() {
        output.field("Receive peer", present(value.receive_peer))?;
        output.field("Transmit peer", present(value.transmit_peer))?;
    }
    output.field("Opaque quality bytes", value.quality_byte_count)
}

fn render_session_list(
    output: &mut DiagnosticOutput,
    items: &[&InventoryDevice],
    snapshot: &CliDiagnosticSnapshot,
) -> Result<(), CliDiagnosticError> {
    ensure_item_bound(items.len(), MAX_SESSION_ITEMS)?;
    output.line(format_args!(
        "Device\tAddress\tProtocol\tModel\tCalls\tMedia streams\tStatistics"
    ))?;
    for item in items {
        let registration = item
            .registration
            .as_ref()
            .ok_or(CliDiagnosticError::NotFound)?;
        let summary = session_summary(item, snapshot);
        output.line(format_args!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
            clean_value(item.id.as_str()),
            clean_value(&registration.address),
            clean_value(&registration.protocol),
            clean_value(&registration.model),
            summary.call_count,
            summary.media_stream_count,
            yes_no(summary.has_statistics),
        ))?;
    }
    Ok(())
}

fn render_session_detail(
    output: &mut DiagnosticOutput,
    item: &InventoryDevice,
    snapshot: &CliDiagnosticSnapshot,
) -> Result<(), CliDiagnosticError> {
    let registration = item
        .registration
        .as_ref()
        .ok_or(CliDiagnosticError::NotFound)?;
    let summary = session_summary(item, snapshot);
    output.field("Device", item.id.as_str())?;
    output.field("Address", &registration.address)?;
    output.field("Protocol", &registration.protocol)?;
    output.field("Model", &registration.model)?;
    output.field("Model ID", registration.model_id)?;
    output.field("Calls", summary.call_count)?;
    output.field("Media streams", summary.media_stream_count)?;
    output.field("Latest statistics", yes_no(summary.has_statistics))?;
    if let Some(statistics) = snapshot
        .runtime
        .media_statistics
        .iter()
        .find(|statistics| statistics.device_id == item.id)
    {
        output.field(
            "Statistics generation",
            statistics.snapshot.request_generation,
        )?;
        output.field("Statistics call ID", statistics.snapshot.call_id.0)?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SessionSummary {
    call_count: usize,
    media_stream_count: usize,
    has_statistics: bool,
}

fn session_summary(item: &InventoryDevice, snapshot: &CliDiagnosticSnapshot) -> SessionSummary {
    let streams = snapshot
        .runtime
        .media_streams
        .iter()
        .filter(|stream| stream.device_id == item.id)
        .collect::<Vec<_>>();
    let call_count = snapshot
        .session_calls
        .iter()
        .filter(|call| call.device_id == item.id)
        .map(|call| (call.pbx_id.0, call.call_id.0))
        .collect::<BTreeSet<_>>()
        .len();
    SessionSummary {
        call_count,
        media_stream_count: streams.len(),
        has_statistics: snapshot
            .runtime
            .media_statistics
            .iter()
            .any(|statistics| statistics.device_id == item.id),
    }
}

fn completion_candidates(
    command: CliDiagnosticCommand,
    arguments: &[&str],
    snapshot: &CliDiagnosticSnapshot,
) -> Vec<String> {
    match (command, arguments) {
        (CliDiagnosticCommand::Media, []) => snapshot
            .runtime
            .media_streams
            .iter()
            .map(|item| item.pbx_id.0.to_string())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
        (CliDiagnosticCommand::Media, [pbx_id]) => parse_positive::<u64>(pbx_id)
            .ok()
            .map(|pbx_id| {
                snapshot
                    .runtime
                    .media_streams
                    .iter()
                    .filter(|item| item.pbx_id.0 == pbx_id)
                    .map(|item| item.call_id.0.to_string())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect()
            })
            .unwrap_or_default(),
        (CliDiagnosticCommand::Media, [pbx_id, call_id]) => {
            let Ok((pbx_id, call_id)) = parse_identity(pbx_id, call_id) else {
                return Vec::new();
            };
            snapshot
                .runtime
                .media_streams
                .iter()
                .filter(|item| item.pbx_id.0 == pbx_id && item.call_id.0 == call_id)
                .map(|item| media_kind(item.kind).to_owned())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect()
        }
        (CliDiagnosticCommand::Media, [pbx_id, call_id, kind]) => {
            let Ok((pbx_id, call_id)) = parse_identity(pbx_id, call_id) else {
                return Vec::new();
            };
            let Ok(kind) = parse_media_kind(kind) else {
                return Vec::new();
            };
            snapshot
                .runtime
                .media_streams
                .iter()
                .filter(|item| {
                    item.pbx_id.0 == pbx_id && item.call_id.0 == call_id && item.kind == kind
                })
                .map(|item| media_direction(item.direction).to_owned())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect()
        }
        (CliDiagnosticCommand::MediaStatistics, []) => snapshot
            .runtime
            .media_statistics
            .iter()
            .map(|item| item.device_id.as_str().to_owned())
            .collect(),
        (CliDiagnosticCommand::MediaStatistics, [device]) => parse_device(device)
            .ok()
            .and_then(|device| {
                snapshot
                    .runtime
                    .media_statistics
                    .iter()
                    .find(|item| item.device_id == device)
            })
            .map(|item| vec![item.snapshot.call_id.0.to_string()])
            .unwrap_or_default(),
        (CliDiagnosticCommand::Sessions, []) => registered_sessions(&snapshot.inventory)
            .map(|item| item.id.as_str().to_owned())
            .collect(),
        _ => Vec::new(),
    }
}

fn registered_sessions(snapshot: &InventorySnapshot) -> impl Iterator<Item = &InventoryDevice> {
    snapshot
        .devices
        .iter()
        .filter(|device| device.registration.is_some())
}

fn media_identity(item: &MediaStreamStatus) -> (u64, u64, MediaKind, MediaDirection, &DeviceId) {
    (
        item.pbx_id.0,
        item.call_id.0,
        item.kind,
        item.direction,
        &item.device_id,
    )
}

fn validate_arguments(arguments: &[&str]) -> Result<(), CliDiagnosticError> {
    if arguments.len() > MAX_CLI_DIAGNOSTIC_ARGUMENTS
        || arguments.iter().any(|value| {
            value.is_empty()
                || value.len() > MAX_CLI_DIAGNOSTIC_ARGUMENT_BYTES
                || value.chars().any(char::is_control)
        })
    {
        Err(CliDiagnosticError::InvalidSelector)
    } else {
        Ok(())
    }
}

fn parse_device(value: &str) -> Result<DeviceId, CliDiagnosticError> {
    super::cli_support::parse_device(value, || CliDiagnosticError::InvalidSelector)
}

fn parse_positive<T>(value: &str) -> Result<T, CliDiagnosticError>
where
    T: std::str::FromStr + PartialEq + Default,
{
    super::cli_support::parse_positive(value, || CliDiagnosticError::InvalidSelector)
}

fn parse_identity(pbx_id: &str, call_id: &str) -> Result<(u64, u64), CliDiagnosticError> {
    Ok((parse_positive(pbx_id)?, parse_positive(call_id)?))
}

fn parse_media_kind(value: &str) -> Result<MediaKind, CliDiagnosticError> {
    if value.eq_ignore_ascii_case("audio") {
        Ok(MediaKind::Audio)
    } else if value.eq_ignore_ascii_case("video") {
        Ok(MediaKind::Video)
    } else {
        Err(CliDiagnosticError::InvalidSelector)
    }
}

fn parse_media_direction(value: &str) -> Result<MediaDirection, CliDiagnosticError> {
    if value.eq_ignore_ascii_case("receive") {
        Ok(MediaDirection::Receive)
    } else if value.eq_ignore_ascii_case("transmit") {
        Ok(MediaDirection::Transmit)
    } else {
        Err(CliDiagnosticError::InvalidSelector)
    }
}

const fn media_kind(value: MediaKind) -> &'static str {
    match value {
        MediaKind::Audio => "audio",
        MediaKind::Video => "video",
    }
}

const fn media_direction(value: MediaDirection) -> &'static str {
    match value {
        MediaDirection::Receive => "receive",
        MediaDirection::Transmit => "transmit",
    }
}

fn only_item<'a, T>(items: &[&'a T]) -> Result<&'a T, CliDiagnosticError> {
    match items {
        [item] => Ok(*item),
        _ => Err(CliDiagnosticError::NotFound),
    }
}

fn require_selected<T>(items: &[T], selected: bool) -> Result<(), CliDiagnosticError> {
    if selected && items.is_empty() {
        Err(CliDiagnosticError::NotFound)
    } else {
        Ok(())
    }
}

fn ensure_item_bound(count: usize, maximum: usize) -> Result<(), CliDiagnosticError> {
    if count > maximum {
        Err(CliDiagnosticError::TooManyItems)
    } else {
        Ok(())
    }
}

fn starts_with_ignore_ascii_case(value: &str, prefix: &str) -> bool {
    value
        .get(..prefix.len())
        .is_some_and(|candidate| candidate.eq_ignore_ascii_case(prefix))
}

fn clean_value(value: &str) -> String {
    let mut clean = String::with_capacity(value.len().min(MAX_VALUE_BYTES));
    for character in value.chars() {
        let character = if character.is_control() {
            ' '
        } else {
            character
        };
        if clean.len() + character.len_utf8() > MAX_VALUE_BYTES {
            break;
        }
        clean.push(character);
    }
    clean
}

const fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn present<T>(value: Option<T>) -> &'static str {
    yes_no(value.is_some())
}

#[derive(Default)]
struct DiagnosticOutput {
    value: String,
}

impl DiagnosticOutput {
    fn line(&mut self, arguments: fmt::Arguments<'_>) -> Result<(), CliDiagnosticError> {
        self.value
            .write_fmt(arguments)
            .map_err(|_| CliDiagnosticError::OutputTooLarge)?;
        self.value.push('\n');
        self.check_bound()
    }

    fn field(&mut self, name: &str, value: impl fmt::Display) -> Result<(), CliDiagnosticError> {
        self.line(format_args!("{name}: {value}"))
    }

    fn check_bound(&self) -> Result<(), CliDiagnosticError> {
        if self.value.len() > MAX_OUTPUT_BYTES {
            Err(CliDiagnosticError::OutputTooLarge)
        } else {
            Ok(())
        }
    }

    fn finish(self) -> String {
        self.value
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use sccp_protocol::{CallId, Codec, LineInstance, MediaEndpoint, MediaStatisticsSnapshot};

    use super::*;
    use crate::ami::inventory::InventoryRegistration;
    use crate::ami::runtime::MediaStatisticsPrivacy;
    use crate::pbx::query::channel::ChannelMediaStateSummary;

    fn device(value: &str) -> DeviceId {
        DeviceId::new(value).unwrap()
    }

    fn endpoint(address: [u8; 4], port: u16) -> MediaEndpoint {
        MediaEndpoint {
            address: IpAddr::V4(Ipv4Addr::from(address)),
            rtp_port: port,
            rtcp_port: port + 1,
            codec: Codec::Pcmu,
            packet_ms: 20,
            max_frames_per_packet: 1,
            telephone_event_payload: 101,
        }
    }

    fn snapshot() -> CliDiagnosticSnapshot {
        let first = device("SEP001122334455");
        let second = device("SEP112233445566");
        CliDiagnosticSnapshot {
            inventory: InventorySnapshot {
                devices: vec![
                    InventoryDevice {
                        id: second,
                        description: "offline".into(),
                        line_count: 1,
                        button_count: 1,
                        registration: None,
                    },
                    InventoryDevice {
                        id: first.clone(),
                        description: "desk".into(),
                        line_count: 1,
                        button_count: 2,
                        registration: Some(InventoryRegistration {
                            model: "CP-7961G".into(),
                            model_id: 30018,
                            protocol: "v22".into(),
                            address: "192.0.2.10:2000".into(),
                        }),
                    },
                ],
                ..InventorySnapshot::default()
            },
            runtime: RuntimeStatusSnapshot {
                media_streams: vec![
                    MediaStreamStatus {
                        pbx_id: PbxCallId(9),
                        call_id: CallId(22),
                        device_id: first.clone(),
                        line_instance: 1,
                        kind: MediaKind::Audio,
                        direction: MediaDirection::Transmit,
                        state: ChannelMediaStateSummary::Open,
                        privacy: false,
                        endpoint: Some(endpoint([192, 0, 2, 20], 19000)),
                    },
                    MediaStreamStatus {
                        pbx_id: PbxCallId(9),
                        call_id: CallId(22),
                        device_id: first.clone(),
                        line_instance: 1,
                        kind: MediaKind::Audio,
                        direction: MediaDirection::Receive,
                        state: ChannelMediaStateSummary::Open,
                        privacy: true,
                        endpoint: Some(endpoint([192, 0, 2, 21], 20000)),
                    },
                ],
                media_statistics: vec![MediaStatisticsStatus {
                    device_id: first.clone(),
                    privacy: MediaStatisticsPrivacy::Private,
                    snapshot: MediaStatisticsSnapshot {
                        request_generation: 3,
                        call_id: CallId(22),
                        line_instance: LineInstance::new(1),
                        codec: Codec::Pcmu,
                        packet_ms: 20,
                        max_frames_per_packet: 1,
                        receive_peer: Some(endpoint([198, 51, 100, 8], 20000)),
                        transmit_peer: Some(endpoint([198, 51, 100, 9], 21000)),
                        packets_sent: 12,
                        octets_sent: 1_920,
                        packets_received: 10,
                        octets_received: 1_600,
                        packets_lost: 2,
                        jitter_millis: 4,
                        latency_millis: 9,
                        quality_byte_count: 87,
                    },
                }],
                ..RuntimeStatusSnapshot::default()
            },
            session_calls: vec![CliSessionCall {
                device_id: first,
                pbx_id: PbxCallId(9),
                call_id: CallId(22),
            }],
        }
    }

    #[test]
    fn media_list_and_exact_detail_are_deterministic_and_private() {
        let snapshot = snapshot();
        let list = render_cli_diagnostics(CliDiagnosticCommand::Media, &[], &snapshot).unwrap();
        let receive = list.find("receive").unwrap();
        let transmit = list.find("transmit").unwrap();
        assert!(receive < transmit);

        let detail = render_cli_diagnostics(
            CliDiagnosticCommand::Media,
            &["9", "22", "audio", "receive"],
            &snapshot,
        )
        .unwrap();
        assert!(detail.contains("Address: <redacted>"));
        assert!(!detail.contains("192.0.2.21"));
        assert!(
            render_cli_diagnostics(
                CliDiagnosticCommand::Media,
                &["9", "22", "audio", "transmit"],
                &snapshot,
            )
            .unwrap()
            .contains("Address: 192.0.2.20")
        );
    }

    #[test]
    fn private_statistics_omit_peers_but_keep_scalar_counters() {
        let output = render_cli_diagnostics(
            CliDiagnosticCommand::MediaStatistics,
            &["SEP001122334455", "22"],
            &snapshot(),
        )
        .unwrap();
        assert!(output.contains("Privacy: yes"));
        assert!(!output.contains("Receive peer:"));
        assert!(!output.contains("Transmit peer:"));
        assert!(output.contains("Packets sent: 12"));
        assert!(output.contains("Opaque quality bytes: 87"));
        assert!(!output.contains("198.51.100"));
    }

    #[test]
    fn public_statistics_report_peer_presence_without_rendering_addresses() {
        let mut snapshot = snapshot();
        snapshot.runtime.media_statistics[0].privacy = MediaStatisticsPrivacy::Public;
        let output = render_cli_diagnostics(
            CliDiagnosticCommand::MediaStatistics,
            &["SEP001122334455", "22"],
            &snapshot,
        )
        .unwrap();
        assert!(output.contains("Privacy: no"));
        assert!(output.contains("Receive peer: yes"));
        assert!(output.contains("Transmit peer: yes"));
        assert!(!output.contains("198.51.100"));
    }

    #[test]
    fn sessions_include_only_registered_devices_and_correlated_counts() {
        let output =
            render_cli_diagnostics(CliDiagnosticCommand::Sessions, &[], &snapshot()).unwrap();
        assert!(output.contains("SEP001122334455"));
        assert!(!output.contains("SEP112233445566"));
        assert!(output.contains("\t1\t2\tyes"));
    }

    #[test]
    fn session_call_count_does_not_depend_on_media_streams() {
        let mut snapshot = snapshot();
        snapshot.runtime.media_streams.clear();
        let output = render_cli_diagnostics(
            CliDiagnosticCommand::Sessions,
            &["SEP001122334455"],
            &snapshot,
        )
        .unwrap();
        assert!(output.contains("Calls: 1"));
        assert!(output.contains("Media streams: 0"));
    }

    #[test]
    fn completions_follow_normalized_snapshot_identities() {
        let snapshot = snapshot();
        assert_eq!(
            complete_cli_diagnostics(CliDiagnosticCommand::Media, &[], "", 0, &snapshot),
            Some("9".into())
        );
        assert_eq!(
            complete_cli_diagnostics(CliDiagnosticCommand::Media, &["9", "22"], "a", 0, &snapshot),
            Some("audio".into())
        );
        assert_eq!(
            complete_cli_diagnostics(
                CliDiagnosticCommand::MediaStatistics,
                &[],
                "sep",
                0,
                &snapshot,
            ),
            Some("SEP001122334455".into())
        );
    }

    #[test]
    fn completion_filters_before_applying_its_result_bound() {
        let mut snapshot = snapshot();
        snapshot.inventory.devices = (0..41)
            .map(|index| InventoryDevice {
                id: device(&format!("SEP{index:012}")),
                description: String::new(),
                line_count: 0,
                button_count: 0,
                registration: Some(InventoryRegistration {
                    model: "station".into(),
                    model_id: 1,
                    protocol: "v22".into(),
                    address: "192.0.2.1:2000".into(),
                }),
            })
            .collect();
        assert_eq!(
            complete_cli_diagnostics(
                CliDiagnosticCommand::Sessions,
                &[],
                "SEP000000000040",
                0,
                &snapshot,
            ),
            Some("SEP000000000040".into())
        );
    }

    #[test]
    fn malformed_missing_duplicate_and_oversized_snapshots_fail_closed() {
        let base = snapshot();
        assert_eq!(
            render_cli_diagnostics(CliDiagnosticCommand::Media, &["0"], &base),
            Err(CliDiagnosticError::InvalidSelector)
        );
        assert_eq!(
            render_cli_diagnostics(CliDiagnosticCommand::Sessions, &["SEP000000000000"], &base),
            Err(CliDiagnosticError::NotFound)
        );
        let mut duplicate = base;
        duplicate
            .runtime
            .media_streams
            .push(duplicate.runtime.media_streams[0].clone());
        assert_eq!(
            render_cli_diagnostics(CliDiagnosticCommand::Media, &[], &duplicate),
            Err(CliDiagnosticError::DuplicateObject)
        );

        let mut too_many = snapshot();
        let template = too_many.runtime.media_streams[0].clone();
        too_many.runtime.media_streams = (1..=MAX_MEDIA_ITEMS + 1)
            .map(|identity| MediaStreamStatus {
                pbx_id: PbxCallId(identity as u64),
                call_id: CallId(identity as u64),
                ..template.clone()
            })
            .collect();
        assert_eq!(
            render_cli_diagnostics(CliDiagnosticCommand::Media, &[], &too_many),
            Err(CliDiagnosticError::TooManyItems)
        );

        let mut excessive_output = snapshot();
        excessive_output.inventory.devices = (0..MAX_SESSION_ITEMS)
            .map(|index| InventoryDevice {
                id: device(&format!("SEP{index:012}")),
                description: String::new(),
                line_count: 0,
                button_count: 0,
                registration: Some(InventoryRegistration {
                    model: "m".repeat(MAX_VALUE_BYTES),
                    model_id: 1,
                    protocol: "p".repeat(MAX_VALUE_BYTES),
                    address: "a".repeat(MAX_VALUE_BYTES),
                }),
            })
            .collect();
        assert_eq!(
            render_cli_diagnostics(CliDiagnosticCommand::Sessions, &[], &excessive_output),
            Err(CliDiagnosticError::OutputTooLarge)
        );
    }
}
