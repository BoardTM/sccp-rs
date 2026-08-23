//! Bounded reservation ownership for service-node QoS messages.
//!
//! This state machine has no station-session dependency. Callers own the
//! service-node transport, encode messages returned in [`QosTransition`], and
//! feed decoded reservation notifications and errors back through
//! [`QosReservationController::handle_message`].

use std::collections::{HashMap, HashSet};
use std::net::Ipv4Addr;
use std::num::NonZeroU64;
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::message::values::{QosDirection, QosErrorCode, QosReservationStyle, RsvpErrorCode};
use crate::message::{ControlMessage, QosApplicationIdentifier, QosFlow, QosTrafficSpecification};

/// Resource and policy bounds applied before a reservation enters live state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QosReservationLimits {
    pub maximum_live_reservations: usize,
    /// Lifetime limit for unique flow/direction generations. Retired wire
    /// identities remain reserved so a late notification cannot settle a
    /// replacement generation.
    pub maximum_generations: usize,
    pub maximum_retries: u32,
    pub maximum_retry_timer: u32,
    pub maximum_response_timeout: Duration,
}

impl QosReservationLimits {
    /// Returns the unchanged limits after proving that live state and its
    /// non-evicting correlation history can both remain bounded.
    pub fn validate(self) -> Result<Self, QosReservationError> {
        if self.maximum_live_reservations == 0 {
            return Err(QosReservationError::InvalidLimits(
                "maximum live reservations must be nonzero",
            ));
        }
        if self.maximum_generations < self.maximum_live_reservations {
            return Err(QosReservationError::InvalidLimits(
                "generation capacity must cover every live reservation",
            ));
        }
        if self.maximum_retry_timer == 0 {
            return Err(QosReservationError::InvalidLimits(
                "maximum retry timer must be nonzero",
            ));
        }
        if self.maximum_response_timeout.is_zero() {
            return Err(QosReservationError::InvalidLimits(
                "maximum response timeout must be nonzero",
            ));
        }
        Ok(self)
    }
}

/// Wire setup form selected for one reservation generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QosReservationSetup {
    Listen { confirmation_required: bool },
    Path,
}

/// Shared admission and traffic policy for listen and path setup messages.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QosReservationPolicy {
    pub reservation_style: QosReservationStyle,
    pub maximum_retries: u32,
    pub retry_timer: u32,
    pub preemption_priority: u32,
    pub defending_priority: u32,
    pub traffic: QosTrafficSpecification,
    pub application: QosApplicationIdentifier,
}

/// Complete request for one exactly correlated service-node reservation.
///
/// `direction` identifies the notification/error key expected from the
/// service node. `response_timeout` is an application-selected local deadline;
/// it does not assign units to the separate wire `retry_timer` quantity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QosReservationRequest {
    pub flow: QosFlow,
    pub direction: QosDirection,
    pub setup: QosReservationSetup,
    pub policy: QosReservationPolicy,
    pub response_timeout: Duration,
}

/// Monotonic controller identity for one non-reusable wire generation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct QosReservationId(NonZeroU64);

impl QosReservationId {
    /// Returns the wire-independent monotonic generation number.
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Externally observable reservation phase.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QosReservationState {
    Establishing,
    Active,
    Modifying,
}

/// Exact service-node failure fields retained for policy decisions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QosReservationFailure {
    pub error_code: QosErrorCode,
    pub failure_node: Ipv4Addr,
    pub rsvp_error_code: RsvpErrorCode,
    pub rsvp_error_subcode: u32,
    pub rsvp_error_flags: u32,
}

/// State change emitted after exact flow/direction correlation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum QosReservationEvent {
    Established {
        id: QosReservationId,
    },
    ActiveWithoutConfirmation {
        id: QosReservationId,
    },
    EstablishmentTimedOut {
        id: QosReservationId,
    },
    Failed {
        id: QosReservationId,
        failure: QosReservationFailure,
    },
    Preempted {
        id: QosReservationId,
        failure: QosReservationFailure,
    },
    ModificationFailed {
        id: QosReservationId,
        failure: QosReservationFailure,
    },
    /// The local observation window elapsed without an explicit failure. The
    /// wire contract does not define a modification-success response.
    ModificationOutcomeUnknown {
        id: QosReservationId,
    },
    TornDown {
        id: QosReservationId,
    },
}

/// Ordered service-node writes and state events produced by one transition.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct QosTransition {
    messages: Vec<ControlMessage>,
    events: Vec<QosReservationEvent>,
}

impl QosTransition {
    /// Borrows service-node writes in their required emission order.
    pub fn messages(&self) -> &[ControlMessage] {
        &self.messages
    }

    /// Borrows events in the order produced by the corresponding state change.
    pub fn events(&self) -> &[QosReservationEvent] {
        &self.events
    }

    /// Transfers both ordered output queues to a transport/event owner.
    pub fn into_parts(self) -> (Vec<ControlMessage>, Vec<QosReservationEvent>) {
        (self.messages, self.events)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct ReservationKey {
    flow: QosFlow,
    direction: QosDirection,
}

#[derive(Clone, Debug)]
enum ReservationPhase {
    Establishing { deadline: Instant },
    Active(ModificationCorrelation),
}

#[derive(Clone, Debug)]
enum ModificationCorrelation {
    Available,
    Pending { deadline: Instant },
    OutcomeUnknown,
    FailureReported,
}

#[derive(Clone, Debug)]
struct Reservation {
    key: ReservationKey,
    phase: ReservationPhase,
}

/// Failure returned before a state transition or service-node write occurs.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum QosReservationError {
    #[error("invalid QoS reservation limits: {0}")]
    InvalidLimits(&'static str),
    #[error("invalid QoS reservation: {0}")]
    InvalidRequest(&'static str),
    #[error("QoS live-reservation capacity is exhausted")]
    LiveCapacityExhausted,
    #[error("QoS reservation generation capacity is exhausted")]
    GenerationCapacityExhausted,
    #[error("QoS wire flow identity was already used")]
    FlowIdentityReused,
    #[error("QoS reservation {0:?} does not exist")]
    UnknownReservation(QosReservationId),
    #[error("QoS reservation {0:?} already issued its one correlatable modification")]
    ModificationAlreadyIssued(QosReservationId),
    #[error("QoS reservation {id:?} cannot {operation} while {state:?}")]
    InvalidState {
        id: QosReservationId,
        operation: &'static str,
        state: QosReservationState,
    },
}

/// Owns bounded QoS reservation generations independently of station calls.
#[derive(Debug)]
pub struct QosReservationController {
    limits: QosReservationLimits,
    next_id: Option<NonZeroU64>,
    by_id: HashMap<QosReservationId, Reservation>,
    by_key: HashMap<ReservationKey, QosReservationId>,
    retired: HashSet<ReservationKey>,
}

impl QosReservationController {
    /// Creates an empty controller after validating every resource bound.
    pub fn new(limits: QosReservationLimits) -> Result<Self, QosReservationError> {
        Ok(Self {
            limits: limits.validate()?,
            next_id: NonZeroU64::new(1),
            by_id: HashMap::new(),
            by_key: HashMap::new(),
            retired: HashSet::new(),
        })
    }

    /// Reserves a fresh lifetime identity and returns the exact setup message.
    /// A flow/direction pair can never be reused by this controller.
    pub fn start(
        &mut self,
        request: QosReservationRequest,
        now: Instant,
    ) -> Result<(QosReservationId, QosTransition), QosReservationError> {
        self.validate_request(&request)?;
        if self.by_id.len() >= self.limits.maximum_live_reservations {
            return Err(QosReservationError::LiveCapacityExhausted);
        }
        if self.retired.len()
            >= self
                .limits
                .maximum_generations
                .saturating_sub(self.by_id.len())
        {
            return Err(QosReservationError::GenerationCapacityExhausted);
        }
        let key = ReservationKey {
            flow: request.flow,
            direction: request.direction,
        };
        if self.by_key.contains_key(&key) || self.retired.contains(&key) {
            return Err(QosReservationError::FlowIdentityReused);
        }
        let id = self.allocate_id()?;
        let message = setup_message(&request);
        let confirmed = !matches!(
            request.setup,
            QosReservationSetup::Listen {
                confirmation_required: false
            }
        );
        let phase = if confirmed {
            ReservationPhase::Establishing {
                deadline: deadline(now, request.response_timeout)?,
            }
        } else {
            ReservationPhase::Active(ModificationCorrelation::Available)
        };
        let mut transition = QosTransition {
            messages: vec![message],
            events: Vec::new(),
        };
        if !confirmed {
            transition
                .events
                .push(QosReservationEvent::ActiveWithoutConfirmation { id });
        }
        self.by_key.insert(key, id);
        self.by_id.insert(id, Reservation { key, phase });
        Ok((id, transition))
    }

    /// Returns `None` for unknown and already retired generations.
    pub fn state(&self, id: QosReservationId) -> Option<QosReservationState> {
        self.by_id
            .get(&id)
            .map(|reservation| state(&reservation.phase))
    }

    /// Counts live generations without discarding retained correlation history.
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    /// Reports whether live state is empty; retired wire identities remain reserved.
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// Sends the sole correlatable modification for an active generation.
    ///
    /// The message family has no transaction identity or explicit success
    /// response, so repeated modifications would make late failures ambiguous.
    pub fn modify(
        &mut self,
        id: QosReservationId,
        traffic: QosTrafficSpecification,
        application: QosApplicationIdentifier,
        response_timeout: Duration,
        now: Instant,
    ) -> Result<QosTransition, QosReservationError> {
        validate_application(&application)?;
        self.validate_response_timeout(response_timeout)?;
        let reservation = self
            .by_id
            .get_mut(&id)
            .ok_or(QosReservationError::UnknownReservation(id))?;
        match reservation.phase {
            ReservationPhase::Active(ModificationCorrelation::Available) => {}
            ReservationPhase::Active(_) => {
                return Err(QosReservationError::ModificationAlreadyIssued(id));
            }
            ReservationPhase::Establishing { .. } => {
                return Err(QosReservationError::InvalidState {
                    id,
                    operation: "modify",
                    state: state(&reservation.phase),
                });
            }
        }
        let message = ControlMessage::QosModify {
            flow: reservation.key.flow,
            direction: reservation.key.direction,
            traffic,
            application,
        };
        reservation.phase = ReservationPhase::Active(ModificationCorrelation::Pending {
            deadline: deadline(now, response_timeout)?,
        });
        Ok(QosTransition {
            messages: vec![message],
            events: Vec::new(),
        })
    }

    /// Builds a six-bit DSCP update for an active generation.
    pub fn update_dscp(
        &self,
        id: QosReservationId,
        dscp: u8,
    ) -> Result<QosTransition, QosReservationError> {
        if dscp > 63 {
            return Err(QosReservationError::InvalidRequest(
                "DSCP must fit six bits",
            ));
        }
        let reservation = self
            .by_id
            .get(&id)
            .ok_or(QosReservationError::UnknownReservation(id))?;
        if state(&reservation.phase) != QosReservationState::Active {
            return Err(QosReservationError::InvalidState {
                id,
                operation: "update DSCP",
                state: state(&reservation.phase),
            });
        }
        Ok(QosTransition {
            messages: vec![ControlMessage::UpdateDscp {
                flow: reservation.key.flow,
                dscp,
            }],
            events: Vec::new(),
        })
    }

    /// Retires the generation before returning its teardown message.
    pub fn teardown(&mut self, id: QosReservationId) -> Result<QosTransition, QosReservationError> {
        let reservation = self.retire(id)?;
        Ok(QosTransition {
            messages: vec![teardown_message(reservation.key)],
            events: vec![QosReservationEvent::TornDown { id }],
        })
    }

    /// Handles only service-node reservation notifications and errors. Other
    /// typed control messages leave state unchanged.
    pub fn handle_message(&mut self, message: &ControlMessage) -> QosTransition {
        match message {
            ControlMessage::QosReservationNotify { flow, direction } => self
                .handle_reservation_notify(ReservationKey {
                    flow: *flow,
                    direction: *direction,
                }),
            ControlMessage::QosErrorNotify {
                flow,
                direction,
                error_code,
                failure_node,
                rsvp_error_code,
                rsvp_error_subcode,
                rsvp_error_flags,
            } => self.handle_error(
                ReservationKey {
                    flow: *flow,
                    direction: *direction,
                },
                QosReservationFailure {
                    error_code: *error_code,
                    failure_node: *failure_node,
                    rsvp_error_code: *rsvp_error_code,
                    rsvp_error_subcode: *rsvp_error_subcode,
                    rsvp_error_flags: *rsvp_error_flags,
                },
            ),
            _ => QosTransition::default(),
        }
    }

    /// Expires every elapsed setup or modification observation in ID order.
    pub fn poll(&mut self, now: Instant) -> QosTransition {
        let mut ids = self
            .by_id
            .iter()
            .filter_map(|(&id, reservation)| match reservation.phase {
                ReservationPhase::Establishing { deadline }
                | ReservationPhase::Active(ModificationCorrelation::Pending { deadline })
                    if deadline <= now =>
                {
                    Some(id)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        ids.sort_unstable();

        let mut transition = QosTransition::default();
        for id in ids {
            match self
                .by_id
                .get(&id)
                .map(|reservation| state(&reservation.phase))
            {
                Some(QosReservationState::Establishing) => {
                    if let Ok(reservation) = self.retire(id) {
                        transition.messages.push(teardown_message(reservation.key));
                        transition
                            .events
                            .push(QosReservationEvent::EstablishmentTimedOut { id });
                    }
                }
                Some(QosReservationState::Modifying) => {
                    if let Some(reservation) = self.by_id.get_mut(&id) {
                        reservation.phase =
                            ReservationPhase::Active(ModificationCorrelation::OutcomeUnknown);
                        transition
                            .events
                            .push(QosReservationEvent::ModificationOutcomeUnknown { id });
                    }
                }
                _ => {}
            }
        }
        transition
    }

    /// Retires all live generations and returns teardowns in ID order.
    pub fn drain(&mut self) -> QosTransition {
        let mut ids = self.by_id.keys().copied().collect::<Vec<_>>();
        ids.sort_unstable();
        let mut transition = QosTransition::default();
        for id in ids {
            if let Ok(reservation) = self.retire(id) {
                transition.messages.push(teardown_message(reservation.key));
                transition.events.push(QosReservationEvent::TornDown { id });
            }
        }
        transition
    }

    fn handle_reservation_notify(&mut self, key: ReservationKey) -> QosTransition {
        let Some(id) = self.by_key.get(&key).copied() else {
            return QosTransition::default();
        };
        let Some(reservation) = self.by_id.get_mut(&id) else {
            return QosTransition::default();
        };
        let event = match &reservation.phase {
            ReservationPhase::Establishing { .. } => QosReservationEvent::Established { id },
            ReservationPhase::Active(_) => {
                return QosTransition::default();
            }
        };
        reservation.phase = ReservationPhase::Active(ModificationCorrelation::Available);
        QosTransition {
            messages: Vec::new(),
            events: vec![event],
        }
    }

    fn handle_error(
        &mut self,
        key: ReservationKey,
        failure: QosReservationFailure,
    ) -> QosTransition {
        let Some(id) = self.by_key.get(&key).copied() else {
            return QosTransition::default();
        };
        if is_modify_failure(failure.error_code) {
            let correlates_modification = self.by_id.get(&id).is_some_and(|reservation| {
                matches!(
                    reservation.phase,
                    ReservationPhase::Active(ModificationCorrelation::Pending { .. })
                )
            });
            if !correlates_modification {
                return QosTransition::default();
            }
            if let Some(reservation) = self.by_id.get_mut(&id) {
                reservation.phase =
                    ReservationPhase::Active(ModificationCorrelation::FailureReported);
            }
            return QosTransition {
                messages: Vec::new(),
                events: vec![QosReservationEvent::ModificationFailed { id, failure }],
            };
        }

        let Ok(reservation) = self.retire(id) else {
            return QosTransition::default();
        };
        if failure.error_code == QosErrorCode::ReservationTornDown {
            return QosTransition {
                messages: Vec::new(),
                events: vec![QosReservationEvent::TornDown { id }],
            };
        }
        let event = if is_preemption(failure.error_code) {
            QosReservationEvent::Preempted { id, failure }
        } else {
            QosReservationEvent::Failed { id, failure }
        };
        QosTransition {
            messages: vec![teardown_message(reservation.key)],
            events: vec![event],
        }
    }

    fn retire(&mut self, id: QosReservationId) -> Result<Reservation, QosReservationError> {
        let reservation = self
            .by_id
            .remove(&id)
            .ok_or(QosReservationError::UnknownReservation(id))?;
        self.by_key.remove(&reservation.key);
        self.retired.insert(reservation.key);
        Ok(reservation)
    }

    fn allocate_id(&mut self) -> Result<QosReservationId, QosReservationError> {
        let value = self
            .next_id
            .take()
            .ok_or(QosReservationError::GenerationCapacityExhausted)?;
        self.next_id = value.get().checked_add(1).and_then(NonZeroU64::new);
        Ok(QosReservationId(value))
    }

    fn validate_request(&self, request: &QosReservationRequest) -> Result<(), QosReservationError> {
        validate_flow(request.flow)?;
        if !request.direction.is_known() {
            return Err(QosReservationError::InvalidRequest(
                "reservation direction is unknown",
            ));
        }
        if !request.policy.reservation_style.is_known() {
            return Err(QosReservationError::InvalidRequest(
                "reservation style is unknown",
            ));
        }
        if request.policy.maximum_retries > self.limits.maximum_retries {
            return Err(QosReservationError::InvalidRequest(
                "retry count exceeds controller policy",
            ));
        }
        if request.policy.retry_timer == 0
            || request.policy.retry_timer > self.limits.maximum_retry_timer
        {
            return Err(QosReservationError::InvalidRequest(
                "retry timer is outside controller policy",
            ));
        }
        self.validate_response_timeout(request.response_timeout)?;
        validate_application(&request.policy.application)
    }

    fn validate_response_timeout(&self, timeout: Duration) -> Result<(), QosReservationError> {
        if timeout.is_zero() || timeout > self.limits.maximum_response_timeout {
            return Err(QosReservationError::InvalidRequest(
                "response timeout is outside controller policy",
            ));
        }
        Ok(())
    }
}

fn setup_message(request: &QosReservationRequest) -> ControlMessage {
    let policy = &request.policy;
    match request.setup {
        QosReservationSetup::Listen {
            confirmation_required,
        } => ControlMessage::QosListen {
            flow: request.flow,
            reservation_style: policy.reservation_style,
            maximum_retries: policy.maximum_retries,
            retry_timer: policy.retry_timer,
            confirmation_required,
            preemption_priority: policy.preemption_priority,
            defending_priority: policy.defending_priority,
            traffic: policy.traffic,
            application: policy.application.clone(),
        },
        QosReservationSetup::Path => ControlMessage::QosPath {
            flow: request.flow,
            reservation_style: policy.reservation_style,
            maximum_retries: policy.maximum_retries,
            retry_timer: policy.retry_timer,
            preemption_priority: policy.preemption_priority,
            defending_priority: policy.defending_priority,
            traffic: policy.traffic,
            application: policy.application.clone(),
        },
    }
}

fn validate_flow(flow: QosFlow) -> Result<(), QosReservationError> {
    if flow.conference_id.get() == 0
        || flow.call_reference.get() == 0
        || flow.passthrough_party_id.get() == 0
    {
        return Err(QosReservationError::InvalidRequest(
            "flow identities must be nonzero",
        ));
    }
    if flow.address.is_unspecified() || flow.address.is_multicast() || flow.port == 0 {
        return Err(QosReservationError::InvalidRequest(
            "flow endpoint must be usable unicast",
        ));
    }
    Ok(())
}

fn validate_application(application: &QosApplicationIdentifier) -> Result<(), QosReservationError> {
    for (value, maximum) in [
        (&application.vendor_id, 31),
        (&application.version, 15),
        (&application.application_name, 31),
        (&application.sub_application_id, 31),
    ] {
        if value.len() > maximum || value.contains('\0') {
            return Err(QosReservationError::InvalidRequest(
                "application identity exceeds its fixed text field",
            ));
        }
    }
    Ok(())
}

fn deadline(now: Instant, timeout: Duration) -> Result<Instant, QosReservationError> {
    now.checked_add(timeout)
        .ok_or(QosReservationError::InvalidRequest(
            "response deadline overflows the monotonic clock",
        ))
}

fn teardown_message(key: ReservationKey) -> ControlMessage {
    ControlMessage::QosTeardown {
        flow: key.flow,
        direction: key.direction,
    }
}

fn state(phase: &ReservationPhase) -> QosReservationState {
    match phase {
        ReservationPhase::Establishing { .. } => QosReservationState::Establishing,
        ReservationPhase::Active(ModificationCorrelation::Pending { .. }) => {
            QosReservationState::Modifying
        }
        ReservationPhase::Active(_) => QosReservationState::Active,
    }
}

fn is_modify_failure(error: QosErrorCode) -> bool {
    matches!(
        error,
        QosErrorCode::ReservationModifyFailed | QosErrorCode::PathModifyFailed
    )
}

fn is_preemption(error: QosErrorCode) -> bool {
    matches!(
        error,
        QosErrorCode::ReservationPreempted | QosErrorCode::PathPreempted
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::catalog::{MessageId, MessageRoute, RuntimeUse};
    use crate::message::values::{Codec, ProtocolVersion};
    use crate::message::wire::FrameDecoder;
    use crate::types::{CallReference, ConferenceId, PassthroughPartyId};

    fn limits() -> QosReservationLimits {
        QosReservationLimits {
            maximum_live_reservations: 4,
            maximum_generations: 8,
            maximum_retries: 3,
            maximum_retry_timer: 10,
            maximum_response_timeout: Duration::from_secs(30),
        }
    }

    fn flow(token: u32) -> QosFlow {
        QosFlow {
            conference_id: ConferenceId::new(40),
            call_reference: CallReference::new(41),
            passthrough_party_id: PassthroughPartyId::new(token),
            address: "192.0.2.40".parse().unwrap(),
            port: 16_000,
        }
    }

    fn application() -> QosApplicationIdentifier {
        QosApplicationIdentifier {
            vendor_id: "vendor".into(),
            version: "1".into(),
            application_name: "audio".into(),
            sub_application_id: "primary".into(),
        }
    }

    fn traffic(average_bit_rate: u32) -> QosTrafficSpecification {
        QosTrafficSpecification {
            codec: Codec::Pcmu,
            average_bit_rate,
            burst_size: 1_200,
            peak_rate: average_bit_rate * 2,
        }
    }

    fn request(token: u32, setup: QosReservationSetup) -> QosReservationRequest {
        QosReservationRequest {
            flow: flow(token),
            direction: QosDirection::Receive,
            setup,
            policy: QosReservationPolicy {
                reservation_style: QosReservationStyle::SharedExplicit,
                maximum_retries: 3,
                retry_timer: 4,
                preemption_priority: 5,
                defending_priority: 6,
                traffic: traffic(64_000),
                application: application(),
            },
            response_timeout: Duration::from_secs(12),
        }
    }

    fn reservation_notify(request: &QosReservationRequest) -> ControlMessage {
        ControlMessage::QosReservationNotify {
            flow: request.flow,
            direction: request.direction,
        }
    }

    fn error_notify(request: &QosReservationRequest, error_code: QosErrorCode) -> ControlMessage {
        let failure = failure(error_code);
        ControlMessage::QosErrorNotify {
            flow: request.flow,
            direction: request.direction,
            error_code: failure.error_code,
            failure_node: failure.failure_node,
            rsvp_error_code: failure.rsvp_error_code,
            rsvp_error_subcode: failure.rsvp_error_subcode,
            rsvp_error_flags: failure.rsvp_error_flags,
        }
    }

    fn failure(error_code: QosErrorCode) -> QosReservationFailure {
        QosReservationFailure {
            error_code,
            failure_node: "198.51.100.4".parse().unwrap(),
            rsvp_error_code: RsvpErrorCode::ServicePreempted,
            rsvp_error_subcode: 7,
            rsvp_error_flags: 8,
        }
    }

    #[test]
    fn setup_routes_exact_policy_and_correlates_only_the_live_key() {
        assert_eq!(
            MessageId::QosListen.contract().unwrap().route,
            MessageRoute::ControlToServiceNode
        );
        assert_eq!(
            MessageId::QosReservationNotify.contract().unwrap().route,
            MessageRoute::ServiceNodeToControl
        );
        assert_eq!(
            MessageId::QosListen.contract().unwrap().runtime_use,
            RuntimeUse::ConditionalServiceNodeOutput
        );
        assert_eq!(
            MessageId::QosReservationNotify
                .contract()
                .unwrap()
                .runtime_use,
            RuntimeUse::ServiceNodeInput
        );
        let now = Instant::now();
        let request = request(
            42,
            QosReservationSetup::Listen {
                confirmation_required: true,
            },
        );
        let mut controller = QosReservationController::new(limits()).unwrap();
        let (id, transition) = controller.start(request.clone(), now).unwrap();
        assert_eq!(
            controller.state(id),
            Some(QosReservationState::Establishing)
        );
        assert_eq!(
            transition.messages(),
            &[ControlMessage::QosListen {
                flow: request.flow,
                reservation_style: request.policy.reservation_style,
                maximum_retries: 3,
                retry_timer: 4,
                confirmation_required: true,
                preemption_priority: 5,
                defending_priority: 6,
                traffic: request.policy.traffic,
                application: request.policy.application.clone(),
            }]
        );

        let mut wrong = request.clone();
        wrong.flow.passthrough_party_id = PassthroughPartyId::new(43);
        assert_eq!(
            controller.handle_message(&reservation_notify(&wrong)),
            QosTransition::default()
        );
        assert_eq!(
            controller
                .handle_message(&reservation_notify(&request))
                .events(),
            &[QosReservationEvent::Established { id }]
        );
        assert_eq!(controller.state(id), Some(QosReservationState::Active));
        assert_eq!(
            controller.handle_message(&reservation_notify(&request)),
            QosTransition::default()
        );
    }

    #[test]
    fn retry_policy_and_deadlines_are_bounded_without_assigning_wire_timer_units() {
        let now = Instant::now();
        let mut controller = QosReservationController::new(limits()).unwrap();
        let mut invalid = request(44, QosReservationSetup::Path);
        invalid.policy.maximum_retries = 4;
        assert!(matches!(
            controller.start(invalid, now),
            Err(QosReservationError::InvalidRequest(_))
        ));
        let mut invalid = request(44, QosReservationSetup::Path);
        invalid.policy.retry_timer = 11;
        assert!(matches!(
            controller.start(invalid, now),
            Err(QosReservationError::InvalidRequest(_))
        ));

        let request = request(44, QosReservationSetup::Path);
        let (id, _) = controller.start(request.clone(), now).unwrap();
        assert!(
            controller
                .poll(now + request.response_timeout - Duration::from_nanos(1))
                .events()
                .is_empty()
        );
        let expired = controller.poll(now + request.response_timeout);
        assert_eq!(
            expired.messages(),
            &[ControlMessage::QosTeardown {
                flow: request.flow,
                direction: request.direction,
            }]
        );
        assert_eq!(
            expired.events(),
            &[QosReservationEvent::EstablishmentTimedOut { id }]
        );
        assert!(controller.is_empty());
        assert!(matches!(
            controller.start(request, now),
            Err(QosReservationError::FlowIdentityReused)
        ));
    }

    #[test]
    fn modification_failure_preserves_active_state_and_preemption_retires_it() {
        let now = Instant::now();
        let request = request(
            45,
            QosReservationSetup::Listen {
                confirmation_required: true,
            },
        );
        let mut controller = QosReservationController::new(limits()).unwrap();
        let (id, _) = controller.start(request.clone(), now).unwrap();
        controller.handle_message(&reservation_notify(&request));

        let modified_traffic = traffic(96_000);
        let modify = controller
            .modify(
                id,
                modified_traffic,
                application(),
                Duration::from_secs(5),
                now,
            )
            .unwrap();
        assert_eq!(
            modify.messages(),
            &[ControlMessage::QosModify {
                flow: request.flow,
                direction: request.direction,
                traffic: modified_traffic,
                application: application(),
            }]
        );
        assert_eq!(
            controller.handle_message(&reservation_notify(&request)),
            QosTransition::default()
        );
        assert_eq!(controller.state(id), Some(QosReservationState::Modifying));
        let failed = controller.handle_message(&error_notify(
            &request,
            QosErrorCode::ReservationModifyFailed,
        ));
        assert_eq!(
            failed.events(),
            &[QosReservationEvent::ModificationFailed {
                id,
                failure: failure(QosErrorCode::ReservationModifyFailed),
            }]
        );
        assert_eq!(controller.state(id), Some(QosReservationState::Active));
        assert_eq!(
            controller.handle_message(&error_notify(
                &request,
                QosErrorCode::ReservationModifyFailed,
            )),
            QosTransition::default()
        );
        assert!(matches!(
            controller.modify(
                id,
                traffic(112_000),
                application(),
                Duration::from_secs(5),
                now,
            ),
            Err(QosReservationError::ModificationAlreadyIssued(actual)) if actual == id
        ));

        let preempted =
            controller.handle_message(&error_notify(&request, QosErrorCode::ReservationPreempted));
        assert_eq!(
            preempted.messages(),
            &[ControlMessage::QosTeardown {
                flow: request.flow,
                direction: request.direction,
            }]
        );
        assert_eq!(
            preempted.events(),
            &[QosReservationEvent::Preempted {
                id,
                failure: failure(QosErrorCode::ReservationPreempted),
            }]
        );
        assert!(controller.is_empty());
        assert_eq!(
            controller.handle_message(&reservation_notify(&request)),
            QosTransition::default()
        );
    }

    #[test]
    fn terminal_errors_teardown_once_and_remote_teardown_does_not_echo() {
        let now = Instant::now();
        let mut controller = QosReservationController::new(limits()).unwrap();
        let failed_request = request(
            48,
            QosReservationSetup::Listen {
                confirmation_required: true,
            },
        );
        let (failed_id, _) = controller.start(failed_request.clone(), now).unwrap();
        let failed = controller.handle_message(&error_notify(
            &failed_request,
            QosErrorCode::ResourceUnavailable,
        ));
        assert_eq!(
            failed.messages(),
            &[ControlMessage::QosTeardown {
                flow: failed_request.flow,
                direction: failed_request.direction,
            }]
        );
        assert_eq!(
            failed.events(),
            &[QosReservationEvent::Failed {
                id: failed_id,
                failure: failure(QosErrorCode::ResourceUnavailable),
            }]
        );
        assert_eq!(
            controller.handle_message(&error_notify(
                &failed_request,
                QosErrorCode::ResourceUnavailable,
            )),
            QosTransition::default()
        );

        let torn_down_request = request(
            49,
            QosReservationSetup::Listen {
                confirmation_required: true,
            },
        );
        let (torn_down_id, _) = controller.start(torn_down_request.clone(), now).unwrap();
        let torn_down = controller.handle_message(&error_notify(
            &torn_down_request,
            QosErrorCode::ReservationTornDown,
        ));
        assert!(torn_down.messages().is_empty());
        assert_eq!(
            torn_down.events(),
            &[QosReservationEvent::TornDown { id: torn_down_id }]
        );
        assert!(controller.is_empty());
    }

    #[test]
    fn fragmented_and_coalesced_service_frames_keep_exact_reservation_ownership() {
        let now = Instant::now();
        let protocol = ProtocolVersion::V22;
        let mut controller = QosReservationController::new(limits()).unwrap();
        let first = request(
            50,
            QosReservationSetup::Listen {
                confirmation_required: true,
            },
        );
        let second = request(
            51,
            QosReservationSetup::Listen {
                confirmation_required: true,
            },
        );
        let (first_id, _) = controller.start(first.clone(), now).unwrap();
        let (second_id, _) = controller.start(second.clone(), now).unwrap();
        let mut wrong = first.clone();
        wrong.direction = QosDirection::Send;

        let bytes = [
            reservation_notify(&wrong).encode(protocol).unwrap(),
            reservation_notify(&first).encode(protocol).unwrap(),
            reservation_notify(&second).encode(protocol).unwrap(),
        ]
        .concat();
        let mut decoder = FrameDecoder::new();
        let mut events = Vec::new();
        for fragment in bytes.chunks(7) {
            for frame in decoder.push(fragment).unwrap() {
                let message = ControlMessage::decode(frame, protocol).unwrap();
                events.extend(controller.handle_message(&message).into_parts().1);
            }
        }
        assert_eq!(
            events,
            [
                QosReservationEvent::Established { id: first_id },
                QosReservationEvent::Established { id: second_id },
            ]
        );
        assert_eq!(
            controller.state(first_id),
            Some(QosReservationState::Active)
        );
        assert_eq!(
            controller.state(second_id),
            Some(QosReservationState::Active)
        );
    }

    #[test]
    fn generation_capacity_never_evicts_a_stale_correlation_tombstone() {
        let now = Instant::now();
        let mut bounded_limits = limits();
        bounded_limits.maximum_live_reservations = 1;
        bounded_limits.maximum_generations = 2;
        let mut controller = QosReservationController::new(bounded_limits).unwrap();
        let first = request(
            60,
            QosReservationSetup::Listen {
                confirmation_required: false,
            },
        );
        let (first_id, _) = controller.start(first.clone(), now).unwrap();
        controller.teardown(first_id).unwrap();
        assert!(matches!(
            controller.start(first, now),
            Err(QosReservationError::FlowIdentityReused)
        ));

        let second = request(
            61,
            QosReservationSetup::Listen {
                confirmation_required: false,
            },
        );
        let (second_id, _) = controller.start(second, now).unwrap();
        controller.teardown(second_id).unwrap();
        assert!(matches!(
            controller.start(
                request(
                    62,
                    QosReservationSetup::Listen {
                        confirmation_required: false,
                    },
                ),
                now,
            ),
            Err(QosReservationError::GenerationCapacityExhausted)
        ));
    }

    #[test]
    fn modification_timeout_and_drain_are_deterministic_and_idempotent() {
        let now = Instant::now();
        let mut controller = QosReservationController::new(limits()).unwrap();
        let first = request(
            46,
            QosReservationSetup::Listen {
                confirmation_required: false,
            },
        );
        let second = request(
            47,
            QosReservationSetup::Listen {
                confirmation_required: false,
            },
        );
        let (first_id, first_start) = controller.start(first.clone(), now).unwrap();
        let (second_id, _) = controller.start(second.clone(), now).unwrap();
        assert_eq!(
            first_start.events(),
            &[QosReservationEvent::ActiveWithoutConfirmation { id: first_id }]
        );
        controller
            .modify(
                first_id,
                traffic(80_000),
                application(),
                Duration::from_secs(5),
                now,
            )
            .unwrap();
        let timed_out = controller.poll(now + Duration::from_secs(5));
        assert_eq!(
            timed_out.events(),
            &[QosReservationEvent::ModificationOutcomeUnknown { id: first_id }]
        );
        assert!(timed_out.messages().is_empty());
        assert_eq!(
            controller.state(first_id),
            Some(QosReservationState::Active)
        );

        let drained = controller.drain();
        assert_eq!(
            drained.messages(),
            &[
                ControlMessage::QosTeardown {
                    flow: first.flow,
                    direction: first.direction,
                },
                ControlMessage::QosTeardown {
                    flow: second.flow,
                    direction: second.direction,
                },
            ]
        );
        assert_eq!(
            drained.events(),
            &[
                QosReservationEvent::TornDown { id: first_id },
                QosReservationEvent::TornDown { id: second_id },
            ]
        );
        assert_eq!(controller.drain(), QosTransition::default());
    }
}
