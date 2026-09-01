use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use prost::Message as _;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use super::wire::ingest_response::Result as IngestResult;
use super::wire::{DiagnosticEvent, IngestResponse};

pub(super) const SCCPDEBUG_INGEST_API_KEY: &str =
    "a80d7f90c651170931a10f6c1fbe1599aa53fcf427bbfe4b94847205877c85f9";

const INGEST_ENDPOINT: &str = "wss://sccp.dbg.coral.works/v1/ingest";
const ACK_TIMEOUT: Duration = Duration::from_secs(10);
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_BODY_BYTES: usize = 1024 * 1024;
const MAX_DELIVERY_ATTEMPTS: usize = 8;
const INITIAL_RETRY_DELAY: Duration = Duration::from_secs(1);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);

type IngestSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub(super) struct PendingEvent {
    event_id: String,
    diagnostic_type: i32,
    content_type: &'static str,
    body: Vec<u8>,
}

impl PendingEvent {
    pub(super) fn new(
        event_id: String,
        diagnostic_type: i32,
        content_type: &'static str,
        body: Vec<u8>,
    ) -> Option<Self> {
        (body.len() <= MAX_BODY_BYTES).then_some(Self {
            event_id,
            diagnostic_type,
            content_type,
            body,
        })
    }
}

pub(super) struct PendingBatch {
    pub events: Vec<PendingEvent>,
}

pub(super) async fn upload(
    mut batches: mpsc::Receiver<PendingBatch>,
    module_version: &'static str,
    host_hash: [u8; 32],
) {
    let mut socket = None;
    while let Some(batch) = batches.recv().await {
        for pending in batch.events {
            let event = DiagnosticEvent {
                event_id: pending.event_id,
                module_version: module_version.to_owned(),
                host_hash: host_hash.to_vec(),
                r#type: pending.diagnostic_type,
                content_type: pending.content_type.to_owned(),
                body: pending.body,
            };
            let encoded = event.encode_to_vec();
            let mut retry_delay = INITIAL_RETRY_DELAY;
            let mut attempts = 0_usize;
            let delivered = loop {
                attempts = attempts.saturating_add(1);
                if socket.is_none() {
                    match tokio::time::timeout(ATTEMPT_TIMEOUT, connect()).await {
                        Ok(Ok(connected)) => socket = Some(connected),
                        Ok(Err(())) | Err(_) => {
                            if attempts == MAX_DELIVERY_ATTEMPTS {
                                break false;
                            }
                            retry(&mut retry_delay).await;
                            continue;
                        }
                    }
                }
                let outcome = match socket.as_mut() {
                    Some(socket) => tokio::time::timeout(
                        ATTEMPT_TIMEOUT,
                        deliver(socket, &event.event_id, &encoded),
                    )
                    .await
                    .unwrap_or(DeliveryOutcome::Retry),
                    None => DeliveryOutcome::Retry,
                };
                match outcome {
                    DeliveryOutcome::Accepted => break true,
                    DeliveryOutcome::Rejected => {
                        socket = None;
                        break false;
                    }
                    DeliveryOutcome::Retry => {
                        socket = None;
                        if attempts == MAX_DELIVERY_ATTEMPTS {
                            break false;
                        }
                        retry(&mut retry_delay).await;
                    }
                }
            };
            if !delivered {
                break;
            }
        }
    }
}

async fn retry(delay: &mut Duration) {
    tokio::time::sleep(*delay).await;
    *delay = next_retry_delay(*delay);
}

async fn connect() -> Result<IngestSocket, ()> {
    let mut request = INGEST_ENDPOINT.into_client_request().map_err(|_| ())?;
    let authorization =
        HeaderValue::from_str(&format!("Bearer {SCCPDEBUG_INGEST_API_KEY}")).map_err(|_| ())?;
    request.headers_mut().insert(AUTHORIZATION, authorization);
    connect_async(request)
        .await
        .map(|(socket, _)| socket)
        .map_err(|_| ())
}

async fn deliver(
    socket: &mut IngestSocket,
    expected_event_id: &str,
    encoded: &[u8],
) -> DeliveryOutcome {
    if socket
        .send(Message::Binary(encoded.to_vec().into()))
        .await
        .is_err()
    {
        return DeliveryOutcome::Retry;
    }
    let deadline = Instant::now() + ACK_TIMEOUT;
    loop {
        let message = match tokio::time::timeout_at(deadline, socket.next()).await {
            Ok(Some(Ok(message))) => message,
            Ok(Some(Err(_))) | Ok(None) | Err(_) => return DeliveryOutcome::Retry,
        };
        match message {
            Message::Binary(bytes) => {
                return response_outcome(expected_event_id, &bytes);
            }
            Message::Ping(bytes) => {
                if socket.send(Message::Pong(bytes)).await.is_err() {
                    return DeliveryOutcome::Retry;
                }
            }
            Message::Close(_) => return DeliveryOutcome::Retry,
            Message::Text(_) => return DeliveryOutcome::Retry,
            Message::Pong(_) | Message::Frame(_) => {}
        }
    }
}

fn response_outcome(expected_event_id: &str, bytes: &[u8]) -> DeliveryOutcome {
    let Ok(response) = IngestResponse::decode(bytes) else {
        return DeliveryOutcome::Retry;
    };
    match response.result {
        Some(IngestResult::Ack(ack)) if ack.event_id == expected_event_id => {
            DeliveryOutcome::Accepted
        }
        Some(IngestResult::Ack(_)) => DeliveryOutcome::Retry,
        Some(IngestResult::Error(error)) if error.event_id != expected_event_id => {
            DeliveryOutcome::Retry
        }
        Some(IngestResult::Error(error)) if error.retryable => DeliveryOutcome::Retry,
        Some(IngestResult::Error(_)) => DeliveryOutcome::Rejected,
        None => DeliveryOutcome::Retry,
    }
}

const fn next_retry_delay(current: Duration) -> Duration {
    match current.checked_mul(2) {
        Some(next) if next.as_secs() <= MAX_RETRY_DELAY.as_secs() => next,
        _ => MAX_RETRY_DELAY,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DeliveryOutcome {
    Accepted,
    Retry,
    Rejected,
}

#[cfg(test)]
mod tests {
    use super::super::wire::{DiagnosticType, IngestAck, IngestError, IngestErrorCode};
    use super::*;

    #[test]
    fn only_the_exact_ack_completes_delivery() {
        for duplicate in [false, true] {
            let accepted = IngestResponse {
                result: Some(IngestResult::Ack(IngestAck {
                    event_id: "one".into(),
                    duplicate,
                })),
            }
            .encode_to_vec();
            assert_eq!(
                response_outcome("one", &accepted),
                DeliveryOutcome::Accepted
            );
            assert_eq!(response_outcome("two", &accepted), DeliveryOutcome::Retry);
        }
    }

    #[test]
    fn server_errors_follow_the_retryable_contract() {
        for (retryable, expected) in [
            (true, DeliveryOutcome::Retry),
            (false, DeliveryOutcome::Rejected),
        ] {
            let response = IngestResponse {
                result: Some(IngestResult::Error(IngestError {
                    event_id: "one".into(),
                    code: IngestErrorCode::StorageUnavailable as i32,
                    message: String::new(),
                    retryable,
                })),
            }
            .encode_to_vec();
            assert_eq!(response_outcome("one", &response), expected);
        }
    }

    #[test]
    fn malformed_and_mismatched_responses_never_complete_delivery() {
        assert_eq!(response_outcome("one", &[0xff]), DeliveryOutcome::Retry);
        let mismatched = IngestResponse {
            result: Some(IngestResult::Error(IngestError {
                event_id: "two".into(),
                code: IngestErrorCode::InvalidEvent as i32,
                message: String::new(),
                retryable: false,
            })),
        }
        .encode_to_vec();
        assert_eq!(response_outcome("one", &mismatched), DeliveryOutcome::Retry);
    }

    #[test]
    fn pending_events_enforce_the_ingest_body_limit() {
        assert!(
            PendingEvent::new(
                "event".into(),
                DiagnosticType::Warning as i32,
                "application/json",
                vec![0; MAX_BODY_BYTES],
            )
            .is_some()
        );
        assert!(
            PendingEvent::new(
                "event".into(),
                DiagnosticType::Warning as i32,
                "application/json",
                vec![0; MAX_BODY_BYTES + 1],
            )
            .is_none()
        );
    }
}
