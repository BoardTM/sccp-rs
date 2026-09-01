use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;

use base64::Engine as _;
use sccp_protocol::{
    MessageId, ObservationConnectionId, SignalingDirection, SignalingFidelity,
    SignalingObservation, StationTransport,
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
pub(super) struct PacketCaptureSnapshot {
    pub(super) packets: Vec<CapturedPacket>,
    pub(super) dropped_after_last_packet: u64,
}

impl PacketCaptureSnapshot {
    pub(super) fn is_empty(&self) -> bool {
        self.packets.is_empty() && self.dropped_after_last_packet == 0
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
    retained_bytes: usize,
    pending_dropped: u64,
}

impl PacketCapture {
    pub(super) fn new() -> Self {
        Self {
            connections: HashMap::new(),
            packets: VecDeque::new(),
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

    pub(super) fn disconnected(&mut self, connection_id: ObservationConnectionId) {
        self.connections.remove(&connection_id);
    }

    pub(super) fn record_dropped(&mut self, dropped: u64) {
        self.pending_dropped = self.pending_dropped.saturating_add(dropped);
    }

    pub(super) fn snapshot(&self) -> PacketCaptureSnapshot {
        PacketCaptureSnapshot {
            packets: self.packets.iter().cloned().collect(),
            dropped_after_last_packet: self.pending_dropped,
        }
    }

    fn retain_packet(&mut self, packet: CapturedPacket) {
        self.retained_bytes = self.retained_bytes.saturating_add(packet.size_bytes());
        self.packets.push_back(packet);
        while self.packets.len() > MAX_CAPTURE_ITEMS || self.retained_bytes > MAX_CAPTURE_BYTES {
            let Some(removed) = self.packets.pop_front() else {
                break;
            };
            self.retained_bytes = self.retained_bytes.saturating_sub(removed.size_bytes());
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
