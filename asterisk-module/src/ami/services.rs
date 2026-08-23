//! Typed management controls for microphone, recording, parking, and conferences.
//!
//! `SCCPMicrophone` requires `DeviceId` and `OnOff`. `SCCPRecording` requires
//! `Command` and positive PBX `CallId`: start also requires a bounded
//! `Filename`, stop accepts no start/mute options, and mute/unmute require
//! `Direction=read|write|both`. `SCCPPark` uses exact `DeviceId`; park requires
//! a handset `CallId`, while retrieve requires a positive `ParkingSpace` and
//! may select `LineInstance`/`ParkingLot` only where the command permits.
//! `SCCPConference` requires `ConferenceId`; participant commands also require
//! `ParticipantId`, while end forbids it.
//!
//! Each provider reuses the controller's ownership, parking, conference and
//! recording transactions. Native/handset operations occur without controller
//! locks, typed failures retain their exact cause, and partial compensation is
//! reflected in the bounded response. Recording filenames are always redacted.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::Arc;

use sccp_protocol::{CallId, ConferenceId, DeviceId, ParticipantId};
use thiserror::Error;

use crate::ami::manager::{
    ManagerBackend, ManagerError, ManagerField, ManagerLimits, ManagerPrivilege, ManagerRequest,
    ManagerResponse, RequestFields, RequestFieldsError,
};
use crate::media::recording::{RecordingDirection, RecordingSessionControl};
use crate::runtime::backend::PbxCallId;

pub const MICROPHONE_ACTION: &str = "SCCPMicrophone";
pub const RECORDING_ACTION: &str = "SCCPRecording";
pub const PARK_ACTION: &str = "SCCPPark";
pub const CONFERENCE_ACTION: &str = "SCCPConference";

pub const MAX_RECORDING_FILENAME_BYTES: usize = 255;
pub const MAX_PARKING_LOT_BYTES: usize = 80;

const ACTION_LIMITS: ManagerLimits = ManagerLimits {
    max_fields: 9,
    max_field_name_bytes: 64,
    max_field_value_bytes: 256,
    max_response_bytes: 4096,
};
const ACTION_PRIVILEGES: ManagerPrivilege = ManagerPrivilege::SYSTEM
    .union(ManagerPrivilege::CONFIG)
    .union(ManagerPrivilege::REPORTING);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordingCommand {
    Start,
    Stop,
    Mute,
    Unmute,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParkingCommand {
    Park,
    Retrieve,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConferenceCommand {
    End,
    Kick,
    Mute,
    Invite,
    Moderate,
}

#[derive(Clone, Eq, PartialEq)]
pub enum ServiceOperation {
    Microphone {
        device_id: DeviceId,
        enabled: bool,
    },
    Recording {
        command: RecordingCommand,
        call_id: PbxCallId,
        filename: Option<String>,
        append: bool,
        bridged_only: bool,
        direction: Option<RecordingDirection>,
    },
    Parking {
        command: ParkingCommand,
        device_id: DeviceId,
        call_id: Option<CallId>,
        line_instance: Option<u32>,
        lot: Option<String>,
        slot: Option<u32>,
    },
    Conference {
        command: ConferenceCommand,
        conference_id: ConferenceId,
        participant_id: Option<ParticipantId>,
    },
}

impl fmt::Debug for ServiceOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Microphone { device_id, enabled } => formatter
                .debug_struct("Microphone")
                .field("device_id", device_id)
                .field("enabled", enabled)
                .finish(),
            Self::Recording {
                command,
                call_id,
                filename,
                append,
                bridged_only,
                direction,
            } => formatter
                .debug_struct("Recording")
                .field("command", command)
                .field("call_id", call_id)
                .field("filename", &filename.as_ref().map(|_| "<redacted>"))
                .field("append", append)
                .field("bridged_only", bridged_only)
                .field("direction", direction)
                .finish(),
            Self::Parking {
                command,
                device_id,
                call_id,
                line_instance,
                lot,
                slot,
            } => formatter
                .debug_struct("Parking")
                .field("command", command)
                .field("device_id", device_id)
                .field("call_id", call_id)
                .field("line_instance", line_instance)
                .field("lot", lot)
                .field("slot", slot)
                .finish(),
            Self::Conference {
                command,
                conference_id,
                participant_id,
            } => formatter
                .debug_struct("Conference")
                .field("command", command)
                .field("conference_id", conference_id)
                .field("participant_id", participant_id)
                .finish(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ServiceOutcome {
    Microphone {
        device_id: DeviceId,
        call_id: CallId,
        enabled: bool,
    },
    Recording {
        command: RecordingCommand,
        call_id: PbxCallId,
        active: bool,
        muted: bool,
        affected: usize,
    },
    Parking {
        command: ParkingCommand,
        device_id: DeviceId,
        call_id: CallId,
        lot: Option<String>,
        slot: Option<u32>,
    },
    Conference {
        command: ConferenceCommand,
        conference_id: ConferenceId,
        participant_id: Option<ParticipantId>,
    },
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ServiceProviderError {
    #[error("management service control is unavailable")]
    Unavailable,
    #[error("the requested device does not exist")]
    DeviceNotFound,
    #[error("the requested device is not registered")]
    DeviceNotRegistered,
    #[error("the requested call does not exist")]
    CallNotFound,
    #[error("the requested call belongs to another device")]
    CallOwnership,
    #[error("the requested call is not in a controllable state")]
    CallState,
    #[error("the requested recording already exists")]
    RecordingExists,
    #[error("the requested recording does not exist")]
    RecordingNotFound,
    #[error("the recording operation failed")]
    RecordingFailed,
    #[error("the requested parking lot or space does not exist")]
    ParkingNotFound,
    #[error("the parking operation is disabled")]
    ParkingDisabled,
    #[error("the parking operation conflicts with another transition")]
    ParkingConflict,
    #[error("the conference does not exist")]
    ConferenceNotFound,
    #[error("the conference operation requires moderator ownership")]
    ConferenceAuthorization,
    #[error("the conference participant does not exist")]
    ParticipantNotFound,
    #[error("the conference operation conflicts with another transition")]
    ConferenceConflict,
    #[error("the requested operation is documented but unavailable")]
    Unsupported,
    #[error("the handset or PBX rejected the operation")]
    Delivery,
}

pub trait ServiceControlProvider: Send + Sync + 'static {
    fn execute(&self, operation: ServiceOperation) -> Result<ServiceOutcome, ServiceProviderError>;
}

/// Owns at most one recording session per PBX call and closes sessions before
/// removing them from the registry.
pub struct OwnedRecordingSessions<S> {
    sessions: HashMap<PbxCallId, S>,
}

impl<S> Default for OwnedRecordingSessions<S> {
    fn default() -> Self {
        Self {
            sessions: HashMap::new(),
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum RecordingRegistryError<E> {
    Exists,
    NotFound,
    Session(E),
}

impl<S: RecordingSessionControl> OwnedRecordingSessions<S> {
    pub fn insert(
        &mut self,
        call_id: PbxCallId,
        session: S,
    ) -> Result<(), RecordingRegistryError<S::Error>> {
        if self.sessions.contains_key(&call_id) {
            return Err(RecordingRegistryError::Exists);
        }
        self.sessions.insert(call_id, session);
        Ok(())
    }

    pub fn insert_owned(
        &mut self,
        call_id: PbxCallId,
        session: S,
    ) -> Result<(), (RecordingRegistryError<S::Error>, S)> {
        if self.sessions.contains_key(&call_id) {
            return Err((RecordingRegistryError::Exists, session));
        }
        self.sessions.insert(call_id, session);
        Ok(())
    }

    pub fn stop(&mut self, call_id: PbxCallId) -> Result<(), RecordingRegistryError<S::Error>> {
        let session = self
            .sessions
            .get_mut(&call_id)
            .ok_or(RecordingRegistryError::NotFound)?;
        session.stop().map_err(RecordingRegistryError::Session)?;
        self.sessions.remove(&call_id);
        Ok(())
    }

    pub fn take(&mut self, call_id: PbxCallId) -> Result<S, RecordingRegistryError<S::Error>> {
        self.sessions
            .remove(&call_id)
            .ok_or(RecordingRegistryError::NotFound)
    }

    pub fn extract_if(
        &mut self,
        mut predicate: impl FnMut(PbxCallId, &S) -> bool,
    ) -> Vec<(PbxCallId, S)> {
        let call_ids = self
            .sessions
            .iter()
            .filter_map(|(call_id, session)| predicate(*call_id, session).then_some(*call_id))
            .collect::<Vec<_>>();
        call_ids
            .into_iter()
            .filter_map(|call_id| {
                self.sessions
                    .remove(&call_id)
                    .map(|session| (call_id, session))
            })
            .collect()
    }

    pub fn set_muted(
        &mut self,
        call_id: PbxCallId,
        direction: RecordingDirection,
        muted: bool,
    ) -> Result<usize, RecordingRegistryError<S::Error>> {
        self.sessions
            .get_mut(&call_id)
            .ok_or(RecordingRegistryError::NotFound)?
            .set_muted(direction, muted)
            .map_err(RecordingRegistryError::Session)
    }

    pub fn prune(&mut self, mut retain: impl FnMut(PbxCallId, &S) -> bool) {
        self.sessions.retain(|call_id, session| {
            if retain(*call_id, session) {
                true
            } else {
                let _ = session.stop();
                false
            }
        });
    }

    pub fn contains(&self, call_id: PbxCallId) -> bool {
        self.sessions.contains_key(&call_id)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ServiceAction {
    Microphone,
    Recording,
    Parking,
    Conference,
}

impl ServiceAction {
    const ALL: [Self; 4] = [
        Self::Microphone,
        Self::Recording,
        Self::Parking,
        Self::Conference,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::Microphone => MICROPHONE_ACTION,
            Self::Recording => RECORDING_ACTION,
            Self::Parking => PARK_ACTION,
            Self::Conference => CONFERENCE_ACTION,
        }
    }

    const fn synopsis(self) -> &'static str {
        match self {
            Self::Microphone => "Control handset microphone",
            Self::Recording => "Control an owned recording",
            Self::Parking => "Park or retrieve one call",
            Self::Conference => "Control an active conference",
        }
    }

    const fn description(self) -> &'static str {
        match self {
            Self::Microphone => "Turn the microphone on or off for the device's active call.",
            Self::Recording => "Start, stop, mute, or unmute one driver-owned recording session.",
            Self::Parking => "Park an owned call or retrieve one known parking space.",
            Self::Conference => {
                "End a conference or kick, mute, invite, or moderate one participant."
            }
        }
    }

    fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|action| action.name().eq_ignore_ascii_case(name))
    }
}

/// Register all service-control actions as one RAII-owned lifecycle group.
pub fn register_service_control_actions<P: ServiceControlProvider, M: ManagerBackend>(
    provider: P,
    manager: M,
) -> Result<Vec<M::Registration>, ManagerError> {
    let provider = Arc::new(provider);
    let mut registrations = Vec::with_capacity(ServiceAction::ALL.len());
    for action in ServiceAction::ALL {
        let provider = Arc::clone(&provider);
        match manager.register_action(
            action.name(),
            ACTION_PRIVILEGES,
            action.synopsis(),
            action.description(),
            ACTION_LIMITS,
            move |request| handle_service_control_request(provider.as_ref(), request),
        ) {
            Ok(registration) => registrations.push(registration),
            Err(error) => {
                drop(registrations);
                return Err(error);
            }
        }
    }
    Ok(registrations)
}

pub fn handle_service_control_request<P: ServiceControlProvider + ?Sized>(
    provider: &P,
    request: ManagerRequest,
) -> ManagerResponse {
    match execute_service_control_request(provider, &request) {
        Ok(outcome) => success_response(outcome),
        Err(error) => ManagerResponse::error(error.response_message())
            .expect("fixed service-control error is valid"),
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
enum ServiceActionError {
    #[error("unknown service-control action")]
    UnknownAction,
    #[error("request field is not allowlisted")]
    UnknownField,
    #[error("request repeats a singleton field")]
    DuplicateField,
    #[error("request contains sensitive metadata")]
    SensitiveField,
    #[error("request is missing a required field")]
    MissingField,
    #[error("request contains an invalid selector")]
    InvalidSelector,
    #[error("request contains an invalid command")]
    InvalidCommand,
    #[error("request contains an invalid boolean")]
    InvalidBoolean,
    #[error("request contains an invalid recording filename")]
    InvalidFilename,
    #[error("request contains conflicting fields")]
    ConflictingFields,
    #[error("service-control output is invalid")]
    InvalidOutput,
    #[error(transparent)]
    Provider(#[from] ServiceProviderError),
}

impl ServiceActionError {
    const fn response_message(self) -> &'static str {
        match self {
            Self::UnknownAction => "Unknown service-control action",
            Self::UnknownField => "Request field is not allowed",
            Self::DuplicateField => "Request field must not be repeated",
            Self::SensitiveField => "Sensitive request fields are not accepted",
            Self::MissingField => "Required request field is missing",
            Self::InvalidSelector => "Request selector is invalid",
            Self::InvalidCommand => "Requested command is invalid",
            Self::InvalidBoolean => "Boolean field is invalid",
            Self::InvalidFilename => "Recording filename is invalid",
            Self::ConflictingFields => "Request fields conflict with the selected command",
            Self::InvalidOutput => "Service-control result is unavailable",
            Self::Provider(ServiceProviderError::Unavailable) => "Service control is unavailable",
            Self::Provider(ServiceProviderError::DeviceNotFound) => "Device was not found",
            Self::Provider(ServiceProviderError::DeviceNotRegistered) => "Device is not registered",
            Self::Provider(ServiceProviderError::CallNotFound) => "Call was not found",
            Self::Provider(ServiceProviderError::CallOwnership) => {
                "Call is not owned by the selected device"
            }
            Self::Provider(ServiceProviderError::CallState) => {
                "Call is not in a controllable state"
            }
            Self::Provider(ServiceProviderError::RecordingExists) => {
                "A recording already exists for the call"
            }
            Self::Provider(ServiceProviderError::RecordingNotFound) => "Recording was not found",
            Self::Provider(ServiceProviderError::RecordingFailed) => "Recording operation failed",
            Self::Provider(ServiceProviderError::ParkingNotFound) => "Parking target was not found",
            Self::Provider(ServiceProviderError::ParkingDisabled) => "Parking is disabled",
            Self::Provider(ServiceProviderError::ParkingConflict) => {
                "Parking operation conflicts with another transition"
            }
            Self::Provider(ServiceProviderError::ConferenceNotFound) => "Conference was not found",
            Self::Provider(ServiceProviderError::ConferenceAuthorization) => {
                "Conference moderator ownership is required"
            }
            Self::Provider(ServiceProviderError::ParticipantNotFound) => {
                "Conference participant was not found"
            }
            Self::Provider(ServiceProviderError::ConferenceConflict) => {
                "Conference operation conflicts with another transition"
            }
            Self::Provider(ServiceProviderError::Unsupported) => {
                "Requested operation is unavailable"
            }
            Self::Provider(ServiceProviderError::Delivery) => {
                "Handset or PBX rejected the operation"
            }
        }
    }
}

fn execute_service_control_request<P: ServiceControlProvider + ?Sized>(
    provider: &P,
    request: &ManagerRequest,
) -> Result<ServiceOutcome, ServiceActionError> {
    let action = ServiceAction::parse(&request.action).ok_or(ServiceActionError::UnknownAction)?;
    let allowed = match action {
        ServiceAction::Microphone => &["deviceid", "onoff"][..],
        ServiceAction::Recording => &[
            "command",
            "callid",
            "filename",
            "append",
            "bridgedonly",
            "direction",
        ][..],
        ServiceAction::Parking => &[
            "command",
            "deviceid",
            "callid",
            "lineinstance",
            "parkinglot",
            "parkingspace",
        ][..],
        ServiceAction::Conference => &["command", "conferenceid", "participantid"][..],
    };
    let fields = parse_fields(request, allowed)?;
    let operation = match action {
        ServiceAction::Microphone => ServiceOperation::Microphone {
            device_id: device_id(required(&fields, "deviceid")?)?,
            enabled: parse_bool(required(&fields, "onoff")?)?,
        },
        ServiceAction::Recording => parse_recording(&fields)?,
        ServiceAction::Parking => parse_parking(&fields)?,
        ServiceAction::Conference => parse_conference(&fields)?,
    };
    provider.execute(operation).map_err(Into::into)
}

fn parse_recording(
    fields: &BTreeMap<String, String>,
) -> Result<ServiceOperation, ServiceActionError> {
    let command = parse_recording_command(required(fields, "command")?)?;
    let call_id = PbxCallId(positive_u64(required(fields, "callid")?)?);
    let filename = fields
        .get("filename")
        .map(|value| validate_filename(value))
        .transpose()?;
    let append = optional_bool(fields, "append")?.unwrap_or(false);
    let bridged_only = optional_bool(fields, "bridgedonly")?.unwrap_or(false);
    let direction = fields
        .get("direction")
        .map(|value| parse_direction(value))
        .transpose()?;
    match command {
        RecordingCommand::Start if filename.is_none() || direction.is_some() => {
            return Err(ServiceActionError::ConflictingFields);
        }
        RecordingCommand::Stop
            if filename.is_some() || append || bridged_only || direction.is_some() =>
        {
            return Err(ServiceActionError::ConflictingFields);
        }
        RecordingCommand::Mute | RecordingCommand::Unmute
            if filename.is_some() || append || bridged_only || direction.is_none() =>
        {
            return Err(ServiceActionError::ConflictingFields);
        }
        _ => {}
    }
    Ok(ServiceOperation::Recording {
        command,
        call_id,
        filename,
        append,
        bridged_only,
        direction,
    })
}

fn parse_parking(
    fields: &BTreeMap<String, String>,
) -> Result<ServiceOperation, ServiceActionError> {
    let command = parse_parking_command(required(fields, "command")?)?;
    let device_id = device_id(required(fields, "deviceid")?)?;
    let call_id = fields
        .get("callid")
        .map(|value| positive_u64(value).map(CallId))
        .transpose()?;
    let line_instance = fields
        .get("lineinstance")
        .map(|value| positive_u32(value))
        .transpose()?;
    let lot = fields
        .get("parkinglot")
        .map(|value| validate_lot(value))
        .transpose()?;
    let slot = fields
        .get("parkingspace")
        .map(|value| positive_u32(value))
        .transpose()?;
    match command {
        ParkingCommand::Park if call_id.is_none() || line_instance.is_some() || slot.is_some() => {
            return Err(ServiceActionError::ConflictingFields);
        }
        ParkingCommand::Retrieve if call_id.is_some() || slot.is_none() => {
            return Err(ServiceActionError::ConflictingFields);
        }
        _ => {}
    }
    Ok(ServiceOperation::Parking {
        command,
        device_id,
        call_id,
        line_instance,
        lot,
        slot,
    })
}

fn parse_conference(
    fields: &BTreeMap<String, String>,
) -> Result<ServiceOperation, ServiceActionError> {
    let command = parse_conference_command(required(fields, "command")?)?;
    let conference_id = ConferenceId::new(positive_u32(required(fields, "conferenceid")?)?);
    let participant_id = fields
        .get("participantid")
        .map(|value| positive_u32(value).map(ParticipantId::new))
        .transpose()?;
    if matches!(command, ConferenceCommand::End) != participant_id.is_none() {
        return Err(ServiceActionError::ConflictingFields);
    }
    Ok(ServiceOperation::Conference {
        command,
        conference_id,
        participant_id,
    })
}

fn parse_fields(
    request: &ManagerRequest,
    allowed: &[&str],
) -> Result<BTreeMap<String, String>, ServiceActionError> {
    RequestFields::new(request)
        .collect(allowed, &[])
        .map_err(|error| match error {
            RequestFieldsError::Sensitive => ServiceActionError::SensitiveField,
            RequestFieldsError::Duplicate => ServiceActionError::DuplicateField,
            RequestFieldsError::Unknown => ServiceActionError::UnknownField,
            RequestFieldsError::ActionMismatch => ServiceActionError::UnknownAction,
        })
}

fn required<'a>(
    fields: &'a BTreeMap<String, String>,
    name: &str,
) -> Result<&'a str, ServiceActionError> {
    fields
        .get(name)
        .map(String::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(ServiceActionError::MissingField)
}

fn device_id(value: &str) -> Result<DeviceId, ServiceActionError> {
    DeviceId::new(value).map_err(|_| ServiceActionError::InvalidSelector)
}

fn positive_u32(value: &str) -> Result<u32, ServiceActionError> {
    value
        .parse::<u32>()
        .ok()
        .filter(|value| *value != 0)
        .ok_or(ServiceActionError::InvalidSelector)
}

fn positive_u64(value: &str) -> Result<u64, ServiceActionError> {
    value
        .parse::<u64>()
        .ok()
        .filter(|value| *value != 0)
        .ok_or(ServiceActionError::InvalidSelector)
}

fn optional_bool(
    fields: &BTreeMap<String, String>,
    name: &str,
) -> Result<Option<bool>, ServiceActionError> {
    fields.get(name).map(|value| parse_bool(value)).transpose()
}

fn parse_bool(value: &str) -> Result<bool, ServiceActionError> {
    if ["yes", "true", "on", "1"]
        .iter()
        .any(|candidate| value.eq_ignore_ascii_case(candidate))
    {
        Ok(true)
    } else if ["no", "false", "off", "0"]
        .iter()
        .any(|candidate| value.eq_ignore_ascii_case(candidate))
    {
        Ok(false)
    } else {
        Err(ServiceActionError::InvalidBoolean)
    }
}

fn parse_recording_command(value: &str) -> Result<RecordingCommand, ServiceActionError> {
    if value.eq_ignore_ascii_case("start") {
        Ok(RecordingCommand::Start)
    } else if value.eq_ignore_ascii_case("stop") {
        Ok(RecordingCommand::Stop)
    } else if value.eq_ignore_ascii_case("mute") {
        Ok(RecordingCommand::Mute)
    } else if value.eq_ignore_ascii_case("unmute") {
        Ok(RecordingCommand::Unmute)
    } else {
        Err(ServiceActionError::InvalidCommand)
    }
}

fn parse_parking_command(value: &str) -> Result<ParkingCommand, ServiceActionError> {
    if value.eq_ignore_ascii_case("park") {
        Ok(ParkingCommand::Park)
    } else if value.eq_ignore_ascii_case("retrieve") {
        Ok(ParkingCommand::Retrieve)
    } else {
        Err(ServiceActionError::InvalidCommand)
    }
}

fn parse_conference_command(value: &str) -> Result<ConferenceCommand, ServiceActionError> {
    if value.eq_ignore_ascii_case("endconf") {
        Ok(ConferenceCommand::End)
    } else if value.eq_ignore_ascii_case("kick") {
        Ok(ConferenceCommand::Kick)
    } else if value.eq_ignore_ascii_case("mute") {
        Ok(ConferenceCommand::Mute)
    } else if value.eq_ignore_ascii_case("invite") {
        Ok(ConferenceCommand::Invite)
    } else if value.eq_ignore_ascii_case("moderate") {
        Ok(ConferenceCommand::Moderate)
    } else {
        Err(ServiceActionError::InvalidCommand)
    }
}

fn parse_direction(value: &str) -> Result<RecordingDirection, ServiceActionError> {
    if value.eq_ignore_ascii_case("read") {
        Ok(RecordingDirection::Read)
    } else if value.eq_ignore_ascii_case("write") {
        Ok(RecordingDirection::Write)
    } else if value.eq_ignore_ascii_case("both") {
        Ok(RecordingDirection::Both)
    } else {
        Err(ServiceActionError::InvalidCommand)
    }
}

fn validate_filename(value: &str) -> Result<String, ServiceActionError> {
    if value.is_empty()
        || value.len() > MAX_RECORDING_FILENAME_BYTES
        || value == "."
        || value == ".."
        || value.starts_with('.')
        || value
            .bytes()
            .any(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')))
    {
        Err(ServiceActionError::InvalidFilename)
    } else {
        Ok(value.to_owned())
    }
}

fn validate_lot(value: &str) -> Result<String, ServiceActionError> {
    if value.is_empty()
        || value.len() > MAX_PARKING_LOT_BYTES
        || value.chars().any(|character| character.is_control())
    {
        Err(ServiceActionError::InvalidSelector)
    } else {
        Ok(value.to_owned())
    }
}

fn success_response(outcome: ServiceOutcome) -> ManagerResponse {
    let fields = match outcome {
        ServiceOutcome::Microphone {
            device_id,
            call_id,
            enabled,
        } => vec![
            public("DeviceId", device_id.as_str()),
            public("CallId", call_id.0),
            public("OnOff", yes_no(enabled)),
        ],
        ServiceOutcome::Recording {
            command,
            call_id,
            active,
            muted,
            affected,
        } => vec![
            public("Command", recording_command_name(command)),
            public("CallId", call_id.0),
            public("Active", yes_no(active)),
            public("Muted", yes_no(muted)),
            public("Affected", affected),
        ],
        ServiceOutcome::Parking {
            command,
            device_id,
            call_id,
            lot,
            slot,
        } => {
            let mut fields = vec![
                public("Command", parking_command_name(command)),
                public("DeviceId", device_id.as_str()),
                public("CallId", call_id.0),
            ];
            if let Some(lot) = lot {
                fields.push(public("ParkingLot", lot));
            }
            if let Some(slot) = slot {
                fields.push(public("ParkingSpace", slot));
            }
            fields
        }
        ServiceOutcome::Conference {
            command,
            conference_id,
            participant_id,
        } => {
            let mut fields = vec![
                public("Command", conference_command_name(command)),
                public("ConferenceId", conference_id.get()),
            ];
            if let Some(participant_id) = participant_id {
                fields.push(public("ParticipantId", participant_id.get()));
            }
            fields
        }
    };
    let fields = fields
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap_or_default();
    if fields.is_empty() {
        return ManagerResponse::error(ServiceActionError::InvalidOutput.response_message())
            .expect("fixed service-control error is valid");
    }
    ManagerResponse::success("Service control completed")
        .expect("fixed service-control success is valid")
        .with_fields(fields)
}

const fn recording_command_name(command: RecordingCommand) -> &'static str {
    match command {
        RecordingCommand::Start => "start",
        RecordingCommand::Stop => "stop",
        RecordingCommand::Mute => "mute",
        RecordingCommand::Unmute => "unmute",
    }
}

const fn parking_command_name(command: ParkingCommand) -> &'static str {
    match command {
        ParkingCommand::Park => "park",
        ParkingCommand::Retrieve => "retrieve",
    }
}

const fn conference_command_name(command: ConferenceCommand) -> &'static str {
    match command {
        ConferenceCommand::End => "EndConf",
        ConferenceCommand::Kick => "Kick",
        ConferenceCommand::Mute => "Mute",
        ConferenceCommand::Invite => "Invite",
        ConferenceCommand::Moderate => "Moderate",
    }
}

fn public(name: &'static str, value: impl ToString) -> Result<ManagerField, ManagerError> {
    ManagerField::public(name, value.to_string())
}

const fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use std::sync::{Arc, Barrier, Mutex};
    use std::thread;

    use crate::ami::manager::{ManagerRequestField, ManagerResponseKind};

    use super::*;

    struct FakeSession {
        id: String,
        state: crate::media::recording::RecordingState,
        fail_stop: bool,
        events: Arc<Mutex<Vec<String>>>,
    }

    impl RecordingSessionControl for FakeSession {
        type Error = &'static str;

        fn id(&self) -> Result<String, Self::Error> {
            Ok(self.id.clone())
        }

        fn state(&self) -> Result<crate::media::recording::RecordingState, Self::Error> {
            Ok(self.state)
        }

        fn stop(&mut self) -> Result<(), Self::Error> {
            self.events
                .lock()
                .unwrap()
                .push(format!("stop:{}", self.id));
            if self.fail_stop {
                self.fail_stop = false;
                Err("stop")
            } else {
                self.state = crate::media::recording::RecordingState::Stopped;
                Ok(())
            }
        }

        fn set_muted(
            &mut self,
            direction: RecordingDirection,
            muted: bool,
        ) -> Result<usize, Self::Error> {
            self.events
                .lock()
                .unwrap()
                .push(format!("mute:{}:{direction:?}:{muted}", self.id));
            self.state = if muted {
                crate::media::recording::RecordingState::Muted
            } else {
                crate::media::recording::RecordingState::Active
            };
            Ok(1)
        }
    }

    impl Drop for FakeSession {
        fn drop(&mut self) {
            self.events
                .lock()
                .unwrap()
                .push(format!("drop:{}", self.id));
        }
    }

    fn fake_session(id: &str, fail_stop: bool, events: &Arc<Mutex<Vec<String>>>) -> FakeSession {
        FakeSession {
            id: id.into(),
            state: crate::media::recording::RecordingState::Active,
            fail_stop,
            events: Arc::clone(events),
        }
    }

    #[derive(Clone, Default)]
    struct FakeProvider {
        operations: Arc<Mutex<Vec<ServiceOperation>>>,
        failure: Arc<Mutex<Option<ServiceProviderError>>>,
    }

    impl ServiceControlProvider for FakeProvider {
        fn execute(
            &self,
            operation: ServiceOperation,
        ) -> Result<ServiceOutcome, ServiceProviderError> {
            if let Some(error) = *self.failure.lock().unwrap() {
                return Err(error);
            }
            self.operations.lock().unwrap().push(operation.clone());
            Ok(outcome(operation))
        }
    }

    fn outcome(operation: ServiceOperation) -> ServiceOutcome {
        match operation {
            ServiceOperation::Microphone { device_id, enabled } => ServiceOutcome::Microphone {
                device_id,
                call_id: CallId(7),
                enabled,
            },
            ServiceOperation::Recording {
                command, call_id, ..
            } => ServiceOutcome::Recording {
                command,
                call_id,
                active: command != RecordingCommand::Stop,
                muted: command == RecordingCommand::Mute,
                affected: usize::from(matches!(
                    command,
                    RecordingCommand::Mute | RecordingCommand::Unmute
                )),
            },
            ServiceOperation::Parking {
                command,
                device_id,
                call_id,
                lot,
                slot,
                ..
            } => ServiceOutcome::Parking {
                command,
                device_id,
                call_id: call_id.unwrap_or(CallId(88)),
                lot,
                slot,
            },
            ServiceOperation::Conference {
                command,
                conference_id,
                participant_id,
            } => ServiceOutcome::Conference {
                command,
                conference_id,
                participant_id,
            },
        }
    }

    fn request(action: &str, fields: &[(&str, &str)]) -> ManagerRequest {
        let mut owned = vec![ManagerRequestField {
            name: "Action".into(),
            value: action.into(),
            sensitive: false,
        }];
        owned.extend(fields.iter().map(|(name, value)| ManagerRequestField {
            name: (*name).into(),
            value: (*value).into(),
            sensitive: false,
        }));
        ManagerRequest {
            action: action.into(),
            fields: owned,
        }
    }

    fn value<'a>(response: &'a ManagerResponse, name: &str) -> Option<&'a str> {
        response
            .fields()
            .iter()
            .find(|field| field.name() == name)
            .and_then(ManagerField::public_value)
    }

    #[test]
    fn microphone_contract_is_exact_and_typed() {
        let provider = FakeProvider::default();
        let response = handle_service_control_request(
            &provider,
            request(
                MICROPHONE_ACTION,
                &[("DeviceId", "SEP001122334455"), ("OnOff", "ON")],
            ),
        );
        assert_eq!(response.kind(), ManagerResponseKind::Success);
        assert_eq!(value(&response, "OnOff"), Some("yes"));
        assert!(matches!(
            provider.operations.lock().unwrap().as_slice(),
            [ServiceOperation::Microphone { enabled: true, .. }]
        ));
    }

    #[test]
    fn recording_safe_subset_is_typed_and_filename_is_redacted() {
        let provider = FakeProvider::default();
        let response = handle_service_control_request(
            &provider,
            request(
                RECORDING_ACTION,
                &[
                    ("Command", "start"),
                    ("CallId", "42"),
                    ("Filename", "call-42.wav"),
                    ("Append", "yes"),
                    ("BridgedOnly", "true"),
                ],
            ),
        );
        assert_eq!(response.kind(), ManagerResponseKind::Success);
        assert!(value(&response, "Filename").is_none());
        let operations = provider.operations.lock().unwrap();
        assert!(!format!("{:?}", operations[0]).contains("call-42.wav"));
        assert!(matches!(
            &operations[0],
            ServiceOperation::Recording {
                command: RecordingCommand::Start,
                call_id: PbxCallId(42),
                append: true,
                bridged_only: true,
                ..
            }
        ));
    }

    #[test]
    fn recording_muting_requires_an_exact_direction() {
        let provider = FakeProvider::default();
        let ok = handle_service_control_request(
            &provider,
            request(
                RECORDING_ACTION,
                &[
                    ("Command", "mute"),
                    ("CallId", "42"),
                    ("Direction", "write"),
                ],
            ),
        );
        assert_eq!(ok.kind(), ManagerResponseKind::Success);
        assert_eq!(value(&ok, "Affected"), Some("1"));
        for fields in [
            vec![("Command", "mute"), ("CallId", "42")],
            vec![("Command", "stop"), ("CallId", "42"), ("Direction", "both")],
        ] {
            assert_eq!(
                handle_service_control_request(&provider, request(RECORDING_ACTION, &fields))
                    .kind(),
                ManagerResponseKind::Error
            );
        }
    }

    #[test]
    fn recording_rejects_paths_and_command_like_text() {
        let provider = FakeProvider::default();
        for filename in [
            "../private.wav",
            "/tmp/private.wav",
            ".hidden",
            "call $(id).wav",
        ] {
            let response = handle_service_control_request(
                &provider,
                request(
                    RECORDING_ACTION,
                    &[
                        ("Command", "start"),
                        ("CallId", "42"),
                        ("Filename", filename),
                    ],
                ),
            );
            assert_eq!(response.kind(), ManagerResponseKind::Error);
            assert!(!response.message().unwrap().contains(filename));
        }
    }

    #[test]
    fn parking_park_and_retrieve_have_disjoint_selectors() {
        let provider = FakeProvider::default();
        let park = handle_service_control_request(
            &provider,
            request(
                PARK_ACTION,
                &[
                    ("Command", "park"),
                    ("DeviceId", "SEP001122334455"),
                    ("CallId", "17"),
                    ("ParkingLot", "default"),
                ],
            ),
        );
        let retrieve = handle_service_control_request(
            &provider,
            request(
                PARK_ACTION,
                &[
                    ("Command", "retrieve"),
                    ("DeviceId", "SEP001122334455"),
                    ("LineInstance", "2"),
                    ("ParkingLot", "default"),
                    ("ParkingSpace", "701"),
                ],
            ),
        );
        assert_eq!(park.kind(), ManagerResponseKind::Success);
        assert_eq!(retrieve.kind(), ManagerResponseKind::Success);
        assert_eq!(value(&retrieve, "CallId"), Some("88"));
        assert_eq!(provider.operations.lock().unwrap().len(), 2);
    }

    #[test]
    fn conference_contract_requires_participant_except_for_end() {
        let provider = FakeProvider::default();
        for command in ["Kick", "Mute", "Invite", "Moderate"] {
            let response = handle_service_control_request(
                &provider,
                request(
                    CONFERENCE_ACTION,
                    &[
                        ("Command", command),
                        ("ConferenceId", "3"),
                        ("ParticipantId", "4"),
                    ],
                ),
            );
            assert_eq!(response.kind(), ManagerResponseKind::Success);
        }
        assert_eq!(
            handle_service_control_request(
                &provider,
                request(
                    CONFERENCE_ACTION,
                    &[("Command", "EndConf"), ("ConferenceId", "3")],
                ),
            )
            .kind(),
            ManagerResponseKind::Success
        );
        assert_eq!(
            handle_service_control_request(
                &provider,
                request(
                    CONFERENCE_ACTION,
                    &[("Command", "Kick"), ("ConferenceId", "3")],
                ),
            )
            .kind(),
            ManagerResponseKind::Error
        );
    }

    #[test]
    fn duplicate_unknown_sensitive_and_malformed_fields_fail_closed() {
        let provider = FakeProvider::default();
        let cases = [
            request(
                MICROPHONE_ACTION,
                &[
                    ("DeviceId", "SEP001122334455"),
                    ("OnOff", "on"),
                    ("onoff", "off"),
                ],
            ),
            request(
                MICROPHONE_ACTION,
                &[("DeviceId", "SEP001122334455"), ("OnOff", "private-value")],
            ),
            request(
                PARK_ACTION,
                &[
                    ("Command", "retrieve"),
                    ("DeviceId", "SEP001122334455"),
                    ("ParkingSpace", "0"),
                ],
            ),
        ];
        for request in cases {
            let response = handle_service_control_request(&provider, request);
            assert_eq!(response.kind(), ManagerResponseKind::Error);
            assert!(!response.message().unwrap().contains("private"));
        }
        let mut sensitive = request(
            MICROPHONE_ACTION,
            &[("DeviceId", "SEP001122334455"), ("OnOff", "on")],
        );
        sensitive.fields.push(ManagerRequestField {
            name: "Authorization".into(),
            value: "private-token".into(),
            sensitive: true,
        });
        assert_eq!(
            handle_service_control_request(&provider, sensitive).kind(),
            ManagerResponseKind::Error
        );
    }

    #[test]
    fn provider_errors_are_stable_and_do_not_echo_input() {
        let provider = FakeProvider::default();
        *provider.failure.lock().unwrap() = Some(ServiceProviderError::RecordingFailed);
        let response = handle_service_control_request(
            &provider,
            request(
                RECORDING_ACTION,
                &[
                    ("Command", "start"),
                    ("CallId", "42"),
                    ("Filename", "private-recording.wav"),
                ],
            ),
        );
        assert_eq!(response.kind(), ManagerResponseKind::Error);
        assert!(!response.message().unwrap().contains("private-recording"));
    }

    #[test]
    fn concurrent_requests_reach_the_provider_without_aliasing() {
        let provider = FakeProvider::default();
        let barrier = Arc::new(Barrier::new(9));
        let mut threads = Vec::new();
        for index in 1..=8 {
            let provider = provider.clone();
            let barrier = Arc::clone(&barrier);
            threads.push(thread::spawn(move || {
                barrier.wait();
                handle_service_control_request(
                    &provider,
                    request(
                        MICROPHONE_ACTION,
                        &[
                            ("DeviceId", "SEP001122334455"),
                            ("OnOff", if index % 2 == 0 { "on" } else { "off" }),
                        ],
                    ),
                )
            }));
        }
        barrier.wait();
        for thread in threads {
            assert_eq!(thread.join().unwrap().kind(), ManagerResponseKind::Success);
        }
        assert_eq!(provider.operations.lock().unwrap().len(), 8);
    }

    #[test]
    fn action_names_are_unique_and_raii_registration_is_unavailable_in_development() {
        let names = ServiceAction::ALL
            .into_iter()
            .map(ServiceAction::name)
            .collect::<BTreeSet<_>>();
        assert_eq!(names.len(), ServiceAction::ALL.len());
        assert!(matches!(
            register_service_control_actions(
                FakeProvider::default(),
                crate::ami::manager::UnavailableManager,
            ),
            Err(ManagerError::Unavailable)
        ));
    }

    #[test]
    fn owned_recordings_preserve_failed_stop_and_cleanup_pruned_or_dropped_sessions() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut recordings = OwnedRecordingSessions::default();
        recordings
            .insert(PbxCallId(1), fake_session("one", true, &events))
            .unwrap();
        assert!(matches!(
            recordings.insert(PbxCallId(1), fake_session("duplicate", false, &events)),
            Err(RecordingRegistryError::Exists)
        ));
        assert_eq!(
            recordings.set_muted(PbxCallId(1), RecordingDirection::Both, true),
            Ok(1)
        );
        assert_eq!(
            recordings.stop(PbxCallId(1)),
            Err(RecordingRegistryError::Session("stop"))
        );
        assert!(recordings.contains(PbxCallId(1)));
        assert_eq!(recordings.stop(PbxCallId(1)), Ok(()));

        recordings
            .insert(PbxCallId(2), fake_session("two", false, &events))
            .unwrap();
        recordings
            .insert(PbxCallId(3), fake_session("three", false, &events))
            .unwrap();
        recordings.prune(|call_id, _| call_id == PbxCallId(3));
        assert!(!recordings.contains(PbxCallId(2)));
        assert!(recordings.contains(PbxCallId(3)));
        drop(recordings);

        let events = events.lock().unwrap();
        assert!(events.contains(&"drop:duplicate".into()));
        assert_eq!(
            events.iter().filter(|event| *event == "stop:one").count(),
            2
        );
        assert!(events.contains(&"stop:two".into()));
        assert!(events.contains(&"drop:three".into()));
    }
}
