use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;

use base64::Engine as _;
use sccp_protocol::{
    MessageId, ObservationConnectionId, SignalingDirection, SignalingFidelity,
    SignalingObservation, StationDisconnectReason, StationTransport,
};
use serde::Serialize;

const MAX_CAPTURE_ITEMS: usize = 256;
const MAX_CAPTURE_BYTES: usize = 512 * 1024;
const MAX_CONNECTION_METADATA_ITEMS: usize = 512;

#[derive(Clone, Serialize)]
pub(super) struct CapturedPacket {
    observation_id: u64,
    observed_at_unix_ms: u64,
    dropped_observations: u64,
    connection_id: u64,
    peer: String,
    local: String,
    transport: &'static str,
    direction: &'static str,
    device_id: Option<String>,
    session_generation: Option<u64>,
    protocol_header: Option<u32>,
    message_id: Option<u32>,
    message_name: Option<&'static str>,
    fidelity: &'static str,
    classification: &'static str,
    wire_bytes_base64: String,
}

#[derive(Clone, Serialize)]
pub(super) struct CapturedDisconnect {
    observation_id: u64,
    observed_at_unix_ms: u64,
    dropped_observations: u64,
    connection_id: u64,
    peer: Option<String>,
    local: Option<String>,
    transport: Option<&'static str>,
    device_id: Option<String>,
    session_generation: Option<u64>,
    reason: &'static str,
}

#[derive(Clone, Serialize)]
pub(super) struct PacketCaptureSnapshot {
    pub(super) packets: Vec<CapturedPacket>,
    pub(super) disconnects: Vec<CapturedDisconnect>,
    pub(super) dropped_after_last_packet: u64,
}

impl PacketCaptureSnapshot {
    pub(super) fn is_empty(&self) -> bool {
        self.packets.is_empty()
            && self.disconnects.is_empty()
            && self.dropped_after_last_packet == 0
    }

    pub(super) fn discard_oldest(&mut self) -> bool {
        let packet_id = self.packets.first().map(|packet| packet.observation_id);
        let disconnect_id = self
            .disconnects
            .first()
            .map(|disconnect| disconnect.observation_id);
        match (packet_id, disconnect_id) {
            (Some(packet_id), Some(disconnect_id)) if packet_id <= disconnect_id => {
                self.packets.remove(0);
            }
            (Some(_), Some(_)) | (None, Some(_)) => {
                self.disconnects.remove(0);
            }
            (Some(_), None) => {
                self.packets.remove(0);
            }
            (None, None) => return false,
        }
        true
    }
}

impl CapturedPacket {
    fn size_bytes(&self) -> usize {
        self.wire_bytes_base64.len()
            + self.peer.len()
            + self.local.len()
            + self.device_id.as_ref().map_or(0, String::len)
            + 224
    }
}

impl CapturedDisconnect {
    fn size_bytes(&self) -> usize {
        self.peer.as_ref().map_or(0, String::len)
            + self.local.as_ref().map_or(0, String::len)
            + self.device_id.as_ref().map_or(0, String::len)
            + 160
    }
}

#[derive(Clone)]
struct ConnectionMetadata {
    peer: SocketAddr,
    local: SocketAddr,
    transport: StationTransport,
    device_id: Option<String>,
    session_generation: Option<u64>,
}

pub(super) struct PacketCapture {
    connections: HashMap<ObservationConnectionId, ConnectionMetadata>,
    packets: VecDeque<CapturedPacket>,
    disconnects: VecDeque<CapturedDisconnect>,
    retained_bytes: usize,
    pending_dropped: u64,
}

impl PacketCapture {
    pub(super) fn new() -> Self {
        Self {
            connections: HashMap::new(),
            packets: VecDeque::new(),
            disconnects: VecDeque::new(),
            retained_bytes: 0,
            pending_dropped: 0,
        }
    }

    pub(super) fn connected(
        &mut self,
        connection_id: ObservationConnectionId,
        peer: SocketAddr,
        local: SocketAddr,
        transport: StationTransport,
    ) {
        self.upsert_connection(
            connection_id,
            ConnectionMetadata {
                peer,
                local,
                transport,
                device_id: None,
                session_generation: None,
            },
        );
    }

    pub(super) fn identified(
        &mut self,
        connection_id: ObservationConnectionId,
        device_id: String,
        session_generation: u64,
    ) {
        if let Some(metadata) = self.connections.get_mut(&connection_id) {
            metadata.device_id = Some(device_id);
            metadata.session_generation = Some(session_generation);
        }
    }

    pub(super) fn signaling(
        &mut self,
        observation_id: u64,
        observed_at_unix_ms: u64,
        observation: SignalingObservation,
    ) {
        if !self.connections.contains_key(&observation.connection_id) {
            self.upsert_connection(
                observation.connection_id,
                ConnectionMetadata {
                    peer: observation.peer,
                    local: observation.local,
                    transport: observation.transport,
                    device_id: None,
                    session_generation: None,
                },
            );
        }
        let Some(metadata) = self.connections.get(&observation.connection_id) else {
            return;
        };
        let message = observation.message_id.map(MessageId::from);
        let packet = CapturedPacket {
            observation_id,
            observed_at_unix_ms,
            dropped_observations: std::mem::take(&mut self.pending_dropped),
            connection_id: observation.connection_id.get(),
            peer: metadata.peer.to_string(),
            local: metadata.local.to_string(),
            transport: transport_name(metadata.transport),
            direction: direction_name(observation.direction),
            device_id: metadata.device_id.clone(),
            session_generation: metadata.session_generation,
            protocol_header: observation.protocol_header,
            message_id: observation.message_id,
            message_name: message
                .filter(|message| message.is_known())
                .map(MessageId::name),
            fidelity: fidelity_name(observation.fidelity),
            classification: classification(observation.fidelity, message),
            wire_bytes_base64: base64::engine::general_purpose::STANDARD.encode(observation.bytes),
        };
        self.retain_packet(packet);
    }

    pub(super) fn disconnected(
        &mut self,
        observation_id: u64,
        observed_at_unix_ms: u64,
        connection_id: ObservationConnectionId,
        reason: StationDisconnectReason,
    ) {
        let metadata = self.connections.remove(&connection_id);
        let disconnect = CapturedDisconnect {
            observation_id,
            observed_at_unix_ms,
            dropped_observations: std::mem::take(&mut self.pending_dropped),
            connection_id: connection_id.get(),
            peer: metadata.as_ref().map(|metadata| metadata.peer.to_string()),
            local: metadata.as_ref().map(|metadata| metadata.local.to_string()),
            transport: metadata
                .as_ref()
                .map(|metadata| transport_name(metadata.transport)),
            device_id: metadata
                .as_ref()
                .and_then(|metadata| metadata.device_id.clone()),
            session_generation: metadata.and_then(|metadata| metadata.session_generation),
            reason: disconnect_reason_name(reason),
        };
        self.retained_bytes = self.retained_bytes.saturating_add(disconnect.size_bytes());
        self.disconnects.push_back(disconnect);
        self.enforce_bounds();
    }

    pub(super) fn record_dropped(&mut self, dropped: u64) {
        self.pending_dropped = self.pending_dropped.saturating_add(dropped);
    }

    pub(super) fn snapshot(&self) -> PacketCaptureSnapshot {
        PacketCaptureSnapshot {
            packets: self.packets.iter().cloned().collect(),
            disconnects: self.disconnects.iter().cloned().collect(),
            dropped_after_last_packet: self.pending_dropped,
        }
    }

    fn retain_packet(&mut self, packet: CapturedPacket) {
        self.retained_bytes = self.retained_bytes.saturating_add(packet.size_bytes());
        self.packets.push_back(packet);
        self.enforce_bounds();
    }

    fn enforce_bounds(&mut self) {
        while self.packets.len().saturating_add(self.disconnects.len()) > MAX_CAPTURE_ITEMS
            || self.retained_bytes > MAX_CAPTURE_BYTES
        {
            let packet_id = self.packets.front().map(|packet| packet.observation_id);
            let disconnect_id = self
                .disconnects
                .front()
                .map(|disconnect| disconnect.observation_id);
            match (packet_id, disconnect_id) {
                (Some(packet_id), Some(disconnect_id)) if packet_id <= disconnect_id => {
                    let removed = self
                        .packets
                        .pop_front()
                        .expect("front packet disappeared while enforcing capture bounds");
                    self.retained_bytes = self.retained_bytes.saturating_sub(removed.size_bytes());
                }
                (Some(_), Some(_)) | (None, Some(_)) => {
                    let removed = self
                        .disconnects
                        .pop_front()
                        .expect("front disconnect disappeared while enforcing capture bounds");
                    self.retained_bytes = self.retained_bytes.saturating_sub(removed.size_bytes());
                }
                (Some(_), None) => {
                    let removed = self
                        .packets
                        .pop_front()
                        .expect("front packet disappeared while enforcing capture bounds");
                    self.retained_bytes = self.retained_bytes.saturating_sub(removed.size_bytes());
                }
                (None, None) => break,
            }
        }
    }

    fn upsert_connection(
        &mut self,
        connection_id: ObservationConnectionId,
        metadata: ConnectionMetadata,
    ) {
        if self.connections.len() >= MAX_CONNECTION_METADATA_ITEMS
            && !self.connections.contains_key(&connection_id)
            && let Some(oldest) = self.connections.keys().min_by_key(|id| id.get()).copied()
        {
            self.connections.remove(&oldest);
        }
        self.connections.insert(connection_id, metadata);
    }
}

const fn direction_name(direction: SignalingDirection) -> &'static str {
    match direction {
        SignalingDirection::StationToServer => "station_to_server",
        SignalingDirection::ServerToStation => "server_to_station",
    }
}

const fn transport_name(transport: StationTransport) -> &'static str {
    match transport {
        StationTransport::Clear => "clear",
        StationTransport::Secure => "secure",
    }
}

const fn disconnect_reason_name(reason: StationDisconnectReason) -> &'static str {
    match reason {
        StationDisconnectReason::PeerClosure => "peer_closure",
        StationDisconnectReason::IoFailure => "io_failure",
        StationDisconnectReason::KeepaliveExpiry => "keepalive_expiry",
        StationDisconnectReason::ServerRetirement => "server_retirement",
        StationDisconnectReason::StationRequest => "station_request",
        StationDisconnectReason::RegistrationRejected => "registration_rejected",
        StationDisconnectReason::ProtocolFailure => "protocol_failure",
        StationDisconnectReason::ServerFailure => "server_failure",
        _ => "unknown",
    }
}

const fn fidelity_name(fidelity: SignalingFidelity) -> &'static str {
    match fidelity {
        SignalingFidelity::Exact => "exact",
        SignalingFidelity::SecretsRedacted => "secrets_redacted",
        SignalingFidelity::PayloadSuppressed => "payload_suppressed",
        SignalingFidelity::IncompletePayloadSuppressed => "incomplete_payload_suppressed",
    }
}

fn classification(fidelity: SignalingFidelity, message: Option<MessageId>) -> &'static str {
    match fidelity {
        SignalingFidelity::Exact if message.is_some_and(MessageId::is_known) => "known",
        SignalingFidelity::Exact => "unclassified",
        SignalingFidelity::SecretsRedacted | SignalingFidelity::PayloadSuppressed => {
            "known_secret_bearing"
        }
        SignalingFidelity::IncompletePayloadSuppressed
            if message.is_some_and(MessageId::is_known) =>
        {
            "known_incomplete"
        }
        SignalingFidelity::IncompletePayloadSuppressed => "unclassified_fragment",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disconnect_retains_reason_and_connection_identity() {
        let connection_id = ObservationConnectionId::new(7).unwrap();
        let mut capture = PacketCapture::new();
        capture.connected(
            connection_id,
            "192.0.2.10:41000".parse().unwrap(),
            "192.0.2.1:2000".parse().unwrap(),
            StationTransport::Clear,
        );
        capture.identified(connection_id, "SEP001122334455".into(), 3);
        capture.record_dropped(2);
        capture.disconnected(
            11,
            1_700_000_000_000,
            connection_id,
            StationDisconnectReason::KeepaliveExpiry,
        );

        assert!(!capture.connections.contains_key(&connection_id));
        let snapshot = capture.snapshot();
        assert!(!snapshot.is_empty());
        let value = serde_json::to_value(snapshot).unwrap();
        assert_eq!(value["packets"], serde_json::json!([]));
        assert_eq!(value["disconnects"][0]["observation_id"], 11);
        assert_eq!(value["disconnects"][0]["dropped_observations"], 2);
        assert_eq!(value["disconnects"][0]["connection_id"], 7);
        assert_eq!(value["disconnects"][0]["peer"], "192.0.2.10:41000");
        assert_eq!(value["disconnects"][0]["local"], "192.0.2.1:2000");
        assert_eq!(value["disconnects"][0]["transport"], "clear");
        assert_eq!(value["disconnects"][0]["device_id"], "SEP001122334455");
        assert_eq!(value["disconnects"][0]["session_generation"], 3);
        assert_eq!(value["disconnects"][0]["reason"], "keepalive_expiry");
    }

    #[test]
    fn requested_disconnect_classes_have_stable_telemetry_names() {
        assert_eq!(
            disconnect_reason_name(StationDisconnectReason::PeerClosure),
            "peer_closure"
        );
        assert_eq!(
            disconnect_reason_name(StationDisconnectReason::IoFailure),
            "io_failure"
        );
        assert_eq!(
            disconnect_reason_name(StationDisconnectReason::KeepaliveExpiry),
            "keepalive_expiry"
        );
        assert_eq!(
            disconnect_reason_name(StationDisconnectReason::ServerRetirement),
            "server_retirement"
        );
    }
}
