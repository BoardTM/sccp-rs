//! Typed management controls for handset messages, resets, and calls.
//!
//! The registered names are `SCCPMessageDevices`, `SCCPMessageDevice`,
//! `SCCPSystemMessage`, `SCCPDeviceRestart`, `SCCPAnswerCall`,
//! `SCCPAnswerCall1`, `SCCPHangupCall`, and `SCCPStartCall`. Messages accept
//! `MessageText` plus optional `Beep`/`Timeout` and are bounded to
//! [`MAX_MESSAGE_BYTES`]. Device restart accepts `Devicename` and optional
//! `Type=reset|restart|applyConfig`. Call control uses positive handset
//! `ChannelId`; the explicit answer form additionally requires `Devicename`.
//! Originate accepts exactly one of `DeviceId`/`Devicename`, optional
//! `LineName`, required `Number`, and optional assigned `ChannelId`, with the
//! exported per-field bounds.
//!
//! Providers verify current registration, exact call/line ownership, ringing
//! state, compatible codec, and assigned-ID uniqueness. Native/handset work is
//! outside controller locks. Multi-device message delivery reports attempted,
//! delivered, and partial counts without returning message text.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use sccp_protocol::{CallId, DeviceId};
use thiserror::Error;

use crate::ami::manager::{
    ManagerBackend, ManagerError, ManagerField, ManagerLimits, ManagerPrivilege, ManagerRequest,
    ManagerResponse, RequestFields, RequestFieldsError,
};

pub const MESSAGE_DEVICES_ACTION: &str = "SCCPMessageDevices";
pub const MESSAGE_DEVICE_ACTION: &str = "SCCPMessageDevice";
pub const SYSTEM_MESSAGE_ACTION: &str = "SCCPSystemMessage";
pub const DEVICE_RESTART_ACTION: &str = "SCCPDeviceRestart";
pub const ANSWER_CALL_ACTION: &str = "SCCPAnswerCall";
pub const ANSWER_CALL_CURRENT_ACTION: &str = "SCCPAnswerCall1";
pub const END_CALL_ACTION: &str = "SCCPHangupCall";
pub const ORIGINATE_ACTION: &str = "SCCPStartCall";

pub const MAX_MESSAGE_BYTES: usize = 96;
pub const MAX_DEVICE_SELECTOR_BYTES: usize = 15;
pub const MAX_LINE_SELECTOR_BYTES: usize = 24;
pub const MAX_DIAL_DESTINATION_BYTES: usize = 79;
pub const MAX_ASSIGNED_CHANNEL_ID_BYTES: usize = 149;
pub const MAX_CALL_ID_BYTES: usize = 20;
pub const MAX_BOOLEAN_BYTES: usize = 5;
pub const MAX_TIMEOUT_BYTES: usize = 3;

const DEFAULT_MESSAGE_TIMEOUT_SECONDS: u8 = 10;
const ACTION_LIMITS: ManagerLimits = ManagerLimits {
    max_fields: 8,
    max_field_name_bytes: 64,
    max_field_value_bytes: 256,
    max_response_bytes: 4096,
};
const ACTION_PRIVILEGES: ManagerPrivilege = ManagerPrivilege::SYSTEM
    .union(ManagerPrivilege::CONFIG)
    .union(ManagerPrivilege::REPORTING);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MessageTarget {
    Device(DeviceId),
    RegisteredDevices,
    System,
}

impl MessageTarget {
    const fn as_str(&self) -> &'static str {
        match self {
            Self::Device(_) => "device",
            Self::RegisteredDevices => "registered-devices",
            Self::System => "system",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResetMode {
    Reset,
    Restart,
    ApplyConfiguration,
}

impl ResetMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Reset => "reset",
            Self::Restart => "restart",
            Self::ApplyConfiguration => "applyConfig",
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub enum ControlOperation {
    Message {
        target: MessageTarget,
        text: String,
        beep: bool,
        timeout_seconds: u8,
    },
    Reset {
        device_id: DeviceId,
        mode: ResetMode,
    },
    Answer {
        call_id: CallId,
        device_id: Option<DeviceId>,
    },
    End {
        call_id: CallId,
    },
    Originate {
        device_id: DeviceId,
        line: Option<String>,
        destination: String,
        assigned_channel_id: Option<String>,
    },
}

impl fmt::Debug for ControlOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Message {
                target,
                beep,
                timeout_seconds,
                ..
            } => formatter
                .debug_struct("Message")
                .field("target", target)
                .field("text", &"<redacted>")
                .field("beep", beep)
                .field("timeout_seconds", timeout_seconds)
                .finish(),
            Self::Reset { device_id, mode } => formatter
                .debug_struct("Reset")
                .field("device_id", device_id)
                .field("mode", mode)
                .finish(),
            Self::Answer { call_id, device_id } => formatter
                .debug_struct("Answer")
                .field("call_id", call_id)
                .field("device_id", device_id)
                .finish(),
            Self::End { call_id } => formatter
                .debug_struct("End")
                .field("call_id", call_id)
                .finish(),
            Self::Originate {
                device_id, line, ..
            } => formatter
                .debug_struct("Originate")
                .field("device_id", device_id)
                .field("line", line)
                .field("destination", &"<redacted>")
                .field("assigned_channel_id", &"<redacted>")
                .finish(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ControlOutcome {
    Message {
        target: MessageTarget,
        attempted: usize,
        delivered: usize,
        persistent: bool,
    },
    Reset {
        device_id: DeviceId,
        mode: ResetMode,
    },
    Answer {
        device_id: DeviceId,
        call_id: CallId,
    },
    End {
        device_id: DeviceId,
        call_id: CallId,
    },
    Originate {
        device_id: DeviceId,
        line: String,
        call_id: CallId,
    },
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ControlProviderError {
    #[error("management controls are unavailable")]
    Unavailable,
    #[error("the requested device does not exist")]
    DeviceNotFound,
    #[error("the requested device is not registered")]
    DeviceNotRegistered,
    #[error("the requested line is not an appearance on the device")]
    LineNotFound,
    #[error("the requested call does not exist")]
    CallNotFound,
    #[error("the requested call belongs to another device")]
    CallOwnership,
    #[error("the requested call is not ringing")]
    CallNotRinging,
    #[error("no compatible audio codec is available")]
    NoCompatibleCodec,
    #[error("the handset rejected or did not confirm the command")]
    HandsetDelivery,
    #[error("the PBX rejected the call operation")]
    Backend,
    #[error("the assigned channel identity is already in use")]
    AssignedChannelIdConflict,
}

pub trait ControlProvider: Send + Sync + 'static {
    fn execute(&self, operation: ControlOperation) -> Result<ControlOutcome, ControlProviderError>;
}

/// Failure to construct or execute a typed control from the CLI.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum CliControlError {
    #[error("device selector is malformed")]
    InvalidDevice,
    #[error("message target is malformed")]
    InvalidMessageTarget,
    #[error("message text is missing, unsafe, or too long")]
    InvalidMessage,
    #[error("beep must be a boolean value")]
    InvalidBoolean,
    #[error("timeout must be between 0 and 255 seconds")]
    InvalidTimeout,
    #[error("call identifier must be a positive integer")]
    InvalidCallId,
    #[error("line selector is malformed")]
    InvalidLine,
    #[error("dial destination is missing, unsafe, or too long")]
    InvalidDestination,
    #[error("assigned channel identity is unsafe or too long")]
    InvalidAssignedChannelId,
    #[error(transparent)]
    Provider(#[from] ControlProviderError),
}

/// Validate a CLI device selector and execute the corresponding typed control.
pub fn execute_cli_device_control<P: ControlProvider + ?Sized>(
    provider: &P,
    device: &str,
    mode: ResetMode,
) -> Result<ControlOutcome, CliControlError> {
    let device_id = parse_cli_device(device)?;
    provider
        .execute(ControlOperation::Reset { device_id, mode })
        .map_err(Into::into)
}

/// Execute a bounded device, fleet, or persistent-system message.
pub fn execute_cli_message<P: ControlProvider + ?Sized>(
    provider: &P,
    target: &str,
    text: &str,
    beep: Option<&str>,
    timeout: Option<&str>,
) -> Result<ControlOutcome, CliControlError> {
    let target = if target.eq_ignore_ascii_case("all") {
        MessageTarget::RegisteredDevices
    } else if target.eq_ignore_ascii_case("system") {
        MessageTarget::System
    } else {
        MessageTarget::Device(
            parse_cli_device(target).map_err(|_| CliControlError::InvalidMessageTarget)?,
        )
    };
    let beep = beep
        .map(parse_bool)
        .transpose()
        .map_err(|_| CliControlError::InvalidBoolean)?
        .unwrap_or(false);
    let timeout_seconds = timeout
        .map(parse_timeout)
        .transpose()
        .map_err(|_| CliControlError::InvalidTimeout)?
        .unwrap_or(if target == MessageTarget::System {
            0
        } else {
            DEFAULT_MESSAGE_TIMEOUT_SECONDS
        });
    let text = validate_message(text).map_err(|_| CliControlError::InvalidMessage)?;
    provider
        .execute(ControlOperation::Message {
            target,
            text,
            beep,
            timeout_seconds,
        })
        .map_err(Into::into)
}

/// Execute a bounded answer request with an optional ownership assertion.
pub fn execute_cli_answer<P: ControlProvider + ?Sized>(
    provider: &P,
    call_id: &str,
    device: Option<&str>,
) -> Result<ControlOutcome, CliControlError> {
    provider
        .execute(ControlOperation::Answer {
            call_id: parse_cli_call_id(call_id)?,
            device_id: device.map(parse_cli_device).transpose()?,
        })
        .map_err(Into::into)
}

/// Execute a bounded call termination request.
pub fn execute_cli_end<P: ControlProvider + ?Sized>(
    provider: &P,
    call_id: &str,
) -> Result<ControlOutcome, CliControlError> {
    provider
        .execute(ControlOperation::End {
            call_id: parse_cli_call_id(call_id)?,
        })
        .map_err(Into::into)
}

/// Execute a bounded originate request without exposing dial data in output.
pub fn execute_cli_originate<P: ControlProvider + ?Sized>(
    provider: &P,
    device: &str,
    destination: &str,
    line: Option<&str>,
    assigned_channel_id: Option<&str>,
) -> Result<ControlOutcome, CliControlError> {
    let destination =
        validate_destination(destination).map_err(|_| CliControlError::InvalidDestination)?;
    let line = line
        .map(validate_line)
        .transpose()
        .map_err(|_| CliControlError::InvalidLine)?;
    let assigned_channel_id = assigned_channel_id
        .map(validate_assigned_channel_id)
        .transpose()
        .map_err(|_| CliControlError::InvalidAssignedChannelId)?;
    provider
        .execute(ControlOperation::Originate {
            device_id: parse_cli_device(device)?,
            line,
            destination,
            assigned_channel_id,
        })
        .map_err(Into::into)
}

/// Return one sorted, deduplicated, prefix-matching CLI completion.
pub fn complete_cli_value<S: AsRef<str>>(
    values: impl IntoIterator<Item = S>,
    prefix: &str,
    ordinal: usize,
    maximum_bytes: usize,
) -> Option<String> {
    if prefix.len() > maximum_bytes || prefix.chars().any(char::is_control) {
        return None;
    }
    let mut values = values
        .into_iter()
        .map(|value| value.as_ref().to_owned())
        .filter(|value| value.len() <= maximum_bytes)
        .filter(|value| {
            value
                .get(..prefix.len())
                .is_some_and(|candidate| candidate.eq_ignore_ascii_case(prefix))
        })
        .collect::<Vec<_>>();
    values.sort_unstable();
    values.dedup();
    values.into_iter().nth(ordinal)
}

/// Return the requested deterministic completion for a partial device selector.
pub fn complete_cli_device<'a>(
    devices: impl IntoIterator<Item = &'a DeviceId>,
    prefix: &str,
    ordinal: usize,
) -> Option<String> {
    complete_cli_value(
        devices.into_iter().map(DeviceId::as_str),
        prefix,
        ordinal,
        MAX_DEVICE_SELECTOR_BYTES,
    )
}

fn parse_cli_device(device: &str) -> Result<DeviceId, CliControlError> {
    if device.is_empty()
        || device.len() > MAX_DEVICE_SELECTOR_BYTES
        || !device.bytes().all(|byte| byte.is_ascii_alphanumeric())
    {
        return Err(CliControlError::InvalidDevice);
    }
    parse_device(device).map_err(|_| CliControlError::InvalidDevice)
}

fn parse_cli_call_id(value: &str) -> Result<CallId, CliControlError> {
    if value.is_empty()
        || value.len() > MAX_CALL_ID_BYTES
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(CliControlError::InvalidCallId);
    }
    parse_call_id(value).map_err(|_| CliControlError::InvalidCallId)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ControlAction {
    MessageDevices,
    MessageDevice,
    SystemMessage,
    DeviceRestart,
    AnswerCall,
    AnswerCallCurrent,
    EndCall,
    Originate,
}

impl ControlAction {
    const ALL: [Self; 8] = [
        Self::MessageDevices,
        Self::MessageDevice,
        Self::SystemMessage,
        Self::DeviceRestart,
        Self::AnswerCall,
        Self::AnswerCallCurrent,
        Self::EndCall,
        Self::Originate,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::MessageDevices => MESSAGE_DEVICES_ACTION,
            Self::MessageDevice => MESSAGE_DEVICE_ACTION,
            Self::SystemMessage => SYSTEM_MESSAGE_ACTION,
            Self::DeviceRestart => DEVICE_RESTART_ACTION,
            Self::AnswerCall => ANSWER_CALL_ACTION,
            Self::AnswerCallCurrent => ANSWER_CALL_CURRENT_ACTION,
            Self::EndCall => END_CALL_ACTION,
            Self::Originate => ORIGINATE_ACTION,
        }
    }

    const fn synopsis(self) -> &'static str {
        match self {
            Self::MessageDevices => "Message registered devices",
            Self::MessageDevice => "Message one device",
            Self::SystemMessage => "Set the system message",
            Self::DeviceRestart => "Reset or restart a device",
            Self::AnswerCall | Self::AnswerCallCurrent => "Answer a ringing call",
            Self::EndCall => "End a call",
            Self::Originate => "Originate a handset call",
        }
    }

    const fn description(self) -> &'static str {
        match self {
            Self::MessageDevices => "Display one bounded message on every registered device.",
            Self::MessageDevice => "Display one bounded message on a registered device.",
            Self::SystemMessage => {
                "Set a bounded system message and display it on registered devices."
            }
            Self::DeviceRestart => "Send reset, restart, or apply-configuration to one device.",
            Self::AnswerCall => "Answer a ringing call using the legacy required device selector.",
            Self::AnswerCallCurrent => {
                "Answer a ringing call with an optional device ownership assertion."
            }
            Self::EndCall => "End the call identified by its handset call identifier.",
            Self::Originate => {
                "Originate a call on a registered device and optional line appearance."
            }
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|action| action.name().eq_ignore_ascii_case(value))
    }
}

/// Register every call-control action as one RAII-owned lifecycle group.
pub fn register_control_actions<P: ControlProvider, M: ManagerBackend>(
    provider: P,
    manager: M,
) -> Result<Vec<M::Registration>, ManagerError> {
    let provider = Arc::new(provider);
    let mut registrations = Vec::with_capacity(ControlAction::ALL.len());
    for action in ControlAction::ALL {
        let provider = Arc::clone(&provider);
        registrations.push(manager.register_action(
            action.name(),
            ACTION_PRIVILEGES,
            action.synopsis(),
            action.description(),
            ACTION_LIMITS,
            move |request| handle_control_request(provider.as_ref(), request),
        )?);
    }
    Ok(registrations)
}

pub fn handle_control_request<P: ControlProvider + ?Sized>(
    provider: &P,
    request: ManagerRequest,
) -> ManagerResponse {
    match execute_control_request(provider, &request) {
        Ok(outcome) => outcome_response(outcome),
        Err(error) => ManagerResponse::error(error.response_message())
            .expect("fixed management-control error is valid"),
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
enum ControlActionError {
    #[error("unknown control action")]
    UnknownAction,
    #[error("request field is not allowlisted")]
    UnknownField,
    #[error("request repeats a singleton field")]
    DuplicateField,
    #[error("request contains sensitive metadata")]
    SensitiveField,
    #[error("request is missing a required field")]
    MissingField,
    #[error("request contains a malformed device selector")]
    InvalidDevice,
    #[error("request contains a malformed line selector")]
    InvalidLine,
    #[error("request contains a malformed call identifier")]
    InvalidCallId,
    #[error("request contains an invalid boolean")]
    InvalidBoolean,
    #[error("request contains an invalid timeout")]
    InvalidTimeout,
    #[error("request contains an invalid message")]
    InvalidMessage,
    #[error("request contains an invalid reset mode")]
    InvalidResetMode,
    #[error("request contains an invalid dial destination")]
    InvalidDestination,
    #[error("request contains an invalid assigned channel identity")]
    InvalidAssignedChannelId,
    #[error("request specifies conflicting device fields")]
    ConflictingDevice,
    #[error("management response cannot be represented safely")]
    InvalidOutput,
    #[error(transparent)]
    Provider(#[from] ControlProviderError),
}

impl ControlActionError {
    const fn response_message(self) -> &'static str {
        match self {
            Self::UnknownAction => "Unknown management-control action",
            Self::UnknownField => "Request field is not allowlisted",
            Self::DuplicateField => "Request repeats a singleton field",
            Self::SensitiveField => "Sensitive request fields are not accepted",
            Self::MissingField => "Request is missing a required field",
            Self::InvalidDevice => "Device selector is malformed",
            Self::InvalidLine => "Line selector is malformed",
            Self::InvalidCallId => "ChannelId must be a positive handset call identifier",
            Self::InvalidBoolean => "Beep must be a boolean value",
            Self::InvalidTimeout => "Timeout must be between 0 and 255 seconds",
            Self::InvalidMessage => "MessageText is missing, unsafe, or too long",
            Self::InvalidResetMode => "Type must be reset, full, restart, or applyConfig",
            Self::InvalidDestination => "Number is missing, unsafe, or too long",
            Self::InvalidAssignedChannelId => "ChannelId is unsafe or too long",
            Self::ConflictingDevice => "DeviceId and Devicename must not both be supplied",
            Self::InvalidOutput => "Management-control response cannot be represented safely",
            Self::Provider(ControlProviderError::Unavailable) => {
                "Management controls are unavailable"
            }
            Self::Provider(ControlProviderError::DeviceNotFound) => {
                "Requested device was not found"
            }
            Self::Provider(ControlProviderError::DeviceNotRegistered) => {
                "Requested device is not registered"
            }
            Self::Provider(ControlProviderError::LineNotFound) => {
                "Requested line is not an appearance on the device"
            }
            Self::Provider(ControlProviderError::CallNotFound) => "Requested call was not found",
            Self::Provider(ControlProviderError::CallOwnership) => {
                "Requested call does not belong to the selected device"
            }
            Self::Provider(ControlProviderError::CallNotRinging) => "Requested call is not ringing",
            Self::Provider(ControlProviderError::NoCompatibleCodec) => {
                "No compatible audio codec is available"
            }
            Self::Provider(ControlProviderError::HandsetDelivery) => {
                "Handset command was not confirmed"
            }
            Self::Provider(ControlProviderError::Backend) => "PBX call operation failed",
            Self::Provider(ControlProviderError::AssignedChannelIdConflict) => {
                "Assigned channel identity is already in use"
            }
        }
    }
}

fn execute_control_request<P: ControlProvider + ?Sized>(
    provider: &P,
    request: &ManagerRequest,
) -> Result<ControlOutcome, ControlActionError> {
    let action = ControlAction::parse(&request.action).ok_or(ControlActionError::UnknownAction)?;
    let allowed = match action {
        ControlAction::MessageDevices | ControlAction::SystemMessage => {
            &["messagetext", "beep", "timeout"][..]
        }
        ControlAction::MessageDevice => &["deviceid", "messagetext", "beep", "timeout"][..],
        ControlAction::DeviceRestart => &["devicename", "type"][..],
        ControlAction::AnswerCall => &["devicename", "channelid"][..],
        ControlAction::AnswerCallCurrent => &["channelid", "deviceid"][..],
        ControlAction::EndCall => &["channelid"][..],
        ControlAction::Originate => {
            &["deviceid", "devicename", "linename", "number", "channelid"][..]
        }
    };
    let fields = parse_fields(request, allowed)?;
    let operation = match action {
        ControlAction::MessageDevices
        | ControlAction::MessageDevice
        | ControlAction::SystemMessage => {
            let target = match action {
                ControlAction::MessageDevices => MessageTarget::RegisteredDevices,
                ControlAction::MessageDevice => {
                    MessageTarget::Device(parse_device(required(&fields, "deviceid")?)?)
                }
                ControlAction::SystemMessage => MessageTarget::System,
                _ => unreachable!(),
            };
            ControlOperation::Message {
                target,
                text: validate_message(required(&fields, "messagetext")?)?,
                beep: fields
                    .get("beep")
                    .map(|value| parse_bool(value))
                    .transpose()?
                    .unwrap_or(false),
                timeout_seconds: fields
                    .get("timeout")
                    .map(|value| parse_timeout(value))
                    .transpose()?
                    .unwrap_or(if action == ControlAction::SystemMessage {
                        0
                    } else {
                        DEFAULT_MESSAGE_TIMEOUT_SECONDS
                    }),
            }
        }
        ControlAction::DeviceRestart => ControlOperation::Reset {
            device_id: parse_device(required(&fields, "devicename")?)?,
            mode: fields
                .get("type")
                .map(|value| parse_reset_mode(value))
                .transpose()?
                .unwrap_or(ResetMode::Restart),
        },
        ControlAction::AnswerCall | ControlAction::AnswerCallCurrent => ControlOperation::Answer {
            call_id: parse_call_id(required(&fields, "channelid")?)?,
            device_id: if action == ControlAction::AnswerCall {
                Some(parse_device(required(&fields, "devicename")?)?)
            } else {
                fields
                    .get("deviceid")
                    .map(|value| parse_device(value))
                    .transpose()?
            },
        },
        ControlAction::EndCall => ControlOperation::End {
            call_id: parse_call_id(required(&fields, "channelid")?)?,
        },
        ControlAction::Originate => {
            let device_id = parse_device_alias(&fields)?;
            ControlOperation::Originate {
                device_id,
                line: fields
                    .get("linename")
                    .map(|value| validate_line(value))
                    .transpose()?,
                destination: validate_destination(required(&fields, "number")?)?,
                assigned_channel_id: fields
                    .get("channelid")
                    .map(|value| validate_assigned_channel_id(value))
                    .transpose()?,
            }
        }
    };
    provider.execute(operation).map_err(Into::into)
}

fn parse_fields(
    request: &ManagerRequest,
    allowed: &[&str],
) -> Result<BTreeMap<String, String>, ControlActionError> {
    RequestFields::new(request)
        .collect(allowed, &[])
        .map_err(|error| match error {
            RequestFieldsError::Sensitive => ControlActionError::SensitiveField,
            RequestFieldsError::Duplicate => ControlActionError::DuplicateField,
            RequestFieldsError::Unknown => ControlActionError::UnknownField,
            RequestFieldsError::ActionMismatch => ControlActionError::UnknownAction,
        })
}

fn required<'a>(
    fields: &'a BTreeMap<String, String>,
    name: &str,
) -> Result<&'a str, ControlActionError> {
    fields
        .get(name)
        .map(String::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(ControlActionError::MissingField)
}

fn parse_device(value: &str) -> Result<DeviceId, ControlActionError> {
    DeviceId::new(value).map_err(|_| ControlActionError::InvalidDevice)
}

fn parse_device_alias(fields: &BTreeMap<String, String>) -> Result<DeviceId, ControlActionError> {
    match (fields.get("deviceid"), fields.get("devicename")) {
        (Some(_), Some(_)) => Err(ControlActionError::ConflictingDevice),
        (Some(value), None) | (None, Some(value)) => parse_device(value),
        (None, None) => Err(ControlActionError::MissingField),
    }
}

fn parse_call_id(value: &str) -> Result<CallId, ControlActionError> {
    value
        .parse::<u64>()
        .ok()
        .filter(|value| *value != 0)
        .map(CallId)
        .ok_or(ControlActionError::InvalidCallId)
}

fn parse_bool(value: &str) -> Result<bool, ControlActionError> {
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
        Err(ControlActionError::InvalidBoolean)
    }
}

fn parse_timeout(value: &str) -> Result<u8, ControlActionError> {
    value
        .parse::<u8>()
        .map_err(|_| ControlActionError::InvalidTimeout)
}

fn parse_reset_mode(value: &str) -> Result<ResetMode, ControlActionError> {
    if value.eq_ignore_ascii_case("reset") || value.eq_ignore_ascii_case("full") {
        Ok(ResetMode::Reset)
    } else if value.eq_ignore_ascii_case("restart") {
        Ok(ResetMode::Restart)
    } else if value.eq_ignore_ascii_case("applyconfig") {
        Ok(ResetMode::ApplyConfiguration)
    } else {
        Err(ControlActionError::InvalidResetMode)
    }
}

fn validate_message(value: &str) -> Result<String, ControlActionError> {
    validate_text(value, MAX_MESSAGE_BYTES)
        .map(str::to_owned)
        .ok_or(ControlActionError::InvalidMessage)
}

fn validate_line(value: &str) -> Result<String, ControlActionError> {
    validate_text(value, MAX_LINE_SELECTOR_BYTES)
        .map(str::to_owned)
        .ok_or(ControlActionError::InvalidLine)
}

fn validate_destination(value: &str) -> Result<String, ControlActionError> {
    validate_text(value, MAX_DIAL_DESTINATION_BYTES)
        .map(str::to_owned)
        .ok_or(ControlActionError::InvalidDestination)
}

fn validate_assigned_channel_id(value: &str) -> Result<String, ControlActionError> {
    let value = validate_text(value, MAX_ASSIGNED_CHANNEL_ID_BYTES)
        .ok_or(ControlActionError::InvalidAssignedChannelId)?;
    if value.bytes().any(|byte| byte.is_ascii_whitespace()) {
        Err(ControlActionError::InvalidAssignedChannelId)
    } else {
        Ok(value.to_owned())
    }
}

fn validate_text(value: &str, max_bytes: usize) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty() && value.len() <= max_bytes && !value.chars().any(char::is_control))
        .then_some(value)
}

fn outcome_response(outcome: ControlOutcome) -> ManagerResponse {
    let (message, partial, fields) = match outcome {
        ControlOutcome::Message {
            target,
            attempted,
            delivered,
            persistent,
        } => {
            let failed = attempted.saturating_sub(delivered);
            let mut fields = vec![
                public("Scope", target.as_str()),
                public("Attempted", attempted),
                public("Delivered", delivered),
                public("Failed", failed),
                public("Persistent", yes_no(persistent)),
            ];
            if let MessageTarget::Device(device_id) = target {
                fields.push(public("DeviceId", device_id.as_str()));
            }
            ("Message delivered", failed != 0, fields)
        }
        ControlOutcome::Reset { device_id, mode } => (
            "Device command delivered",
            false,
            vec![
                public("DeviceId", device_id.as_str()),
                public("Type", mode.as_str()),
            ],
        ),
        ControlOutcome::Answer { device_id, call_id } => (
            "Call answered",
            false,
            call_fields(device_id, call_id, "answered"),
        ),
        ControlOutcome::End { device_id, call_id } => (
            "Call ended",
            false,
            call_fields(device_id, call_id, "ended"),
        ),
        ControlOutcome::Originate {
            device_id,
            line,
            call_id,
        } => (
            "Call originated",
            false,
            vec![
                public("DeviceId", device_id.as_str()),
                public("Line", line),
                public("CallId", call_id.0),
                public("State", "originating"),
            ],
        ),
    };
    let fields = fields
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap_or_default();
    if fields.is_empty() {
        return ManagerResponse::error(ControlActionError::InvalidOutput.response_message())
            .expect("fixed management-control error is valid");
    }
    let response = if partial {
        ManagerResponse::error("Message delivery was incomplete")
    } else {
        ManagerResponse::success(message)
    };
    response
        .expect("fixed management-control response is valid")
        .with_fields(fields)
}

fn call_fields(
    device_id: DeviceId,
    call_id: CallId,
    state: &'static str,
) -> Vec<Result<ManagerField, ManagerError>> {
    vec![
        public("DeviceId", device_id.as_str()),
        public("CallId", call_id.0),
        public("State", state),
    ]
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

    use std::sync::{Arc, Mutex};

    use crate::ami::manager::{ManagerRequestField, ManagerResponseKind};

    use super::*;

    #[derive(Clone, Default)]
    struct FakeProvider {
        operations: Arc<Mutex<Vec<ControlOperation>>>,
        error: Arc<Mutex<Option<ControlProviderError>>>,
        outcome: Arc<Mutex<Option<ControlOutcome>>>,
    }

    impl ControlProvider for FakeProvider {
        fn execute(
            &self,
            operation: ControlOperation,
        ) -> Result<ControlOutcome, ControlProviderError> {
            if let Some(error) = *self.error.lock().unwrap() {
                return Err(error);
            }
            self.operations.lock().unwrap().push(operation.clone());
            if let Some(outcome) = self.outcome.lock().unwrap().clone() {
                return Ok(outcome);
            }
            Ok(default_outcome(operation))
        }
    }

    fn default_outcome(operation: ControlOperation) -> ControlOutcome {
        match operation {
            ControlOperation::Message { target, .. } => ControlOutcome::Message {
                target,
                attempted: 1,
                delivered: 1,
                persistent: false,
            },
            ControlOperation::Reset { device_id, mode } => {
                ControlOutcome::Reset { device_id, mode }
            }
            ControlOperation::Answer { call_id, device_id } => ControlOutcome::Answer {
                device_id: device_id.unwrap_or_else(device),
                call_id,
            },
            ControlOperation::End { call_id } => ControlOutcome::End {
                device_id: device(),
                call_id,
            },
            ControlOperation::Originate {
                device_id, line, ..
            } => ControlOutcome::Originate {
                device_id,
                line: line.unwrap_or_else(|| "1001".into()),
                call_id: CallId(44),
            },
        }
    }

    fn device() -> DeviceId {
        DeviceId::new("SEP001122334455").unwrap()
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

    fn response_value<'a>(response: &'a ManagerResponse, name: &str) -> Option<&'a str> {
        response
            .fields()
            .iter()
            .find(|field| field.name() == name)
            .and_then(ManagerField::public_value)
    }

    #[test]
    fn all_message_forms_apply_documented_defaults_and_redact_text() {
        let provider = FakeProvider::default();
        for (action, fields, expected_target, timeout) in [
            (
                MESSAGE_DEVICES_ACTION,
                vec![("MessageText", "private fleet notice")],
                MessageTarget::RegisteredDevices,
                10,
            ),
            (
                MESSAGE_DEVICE_ACTION,
                vec![
                    ("DeviceId", "SEP001122334455"),
                    ("MessageText", "private device notice"),
                    ("Beep", "yes"),
                    ("Timeout", "23"),
                ],
                MessageTarget::Device(device()),
                23,
            ),
            (
                SYSTEM_MESSAGE_ACTION,
                vec![("MessageText", "private system notice")],
                MessageTarget::System,
                0,
            ),
        ] {
            let response = handle_control_request(&provider, request(action, &fields));
            assert_eq!(response.kind(), ManagerResponseKind::Success);
            let operation = provider.operations.lock().unwrap().last().cloned().unwrap();
            assert!(matches!(
                operation,
                ControlOperation::Message {
                    ref target,
                    timeout_seconds,
                    ..
                } if *target == expected_target && timeout_seconds == timeout
            ));
            assert!(!format!("{operation:?}").contains("private"));
            assert!(response_value(&response, "MessageText").is_none());
        }
    }

    #[test]
    fn restart_modes_are_typed_and_unknown_modes_fail_closed() {
        for (value, expected) in [
            ("full", ResetMode::Reset),
            ("RESET", ResetMode::Reset),
            ("restart", ResetMode::Restart),
            ("applyConfig", ResetMode::ApplyConfiguration),
        ] {
            let provider = FakeProvider::default();
            let response = handle_control_request(
                &provider,
                request(
                    DEVICE_RESTART_ACTION,
                    &[("DeviceName", "SEP001122334455"), ("Type", value)],
                ),
            );
            assert_eq!(response.kind(), ManagerResponseKind::Success);
            assert!(matches!(
                provider.operations.lock().unwrap().as_slice(),
                [ControlOperation::Reset { mode, .. }] if *mode == expected
            ));
        }
        let provider = FakeProvider::default();
        let response = handle_control_request(
            &provider,
            request(
                DEVICE_RESTART_ACTION,
                &[("DeviceName", "SEP001122334455"), ("Type", "invented")],
            ),
        );
        assert_eq!(response.kind(), ManagerResponseKind::Error);
        assert!(provider.operations.lock().unwrap().is_empty());
    }

    #[test]
    fn cli_reset_and_restart_use_the_typed_control_provider() {
        let provider = FakeProvider::default();
        for (device_id, mode) in [
            ("sep001122334455", ResetMode::Reset),
            ("SEP556677889900", ResetMode::Restart),
        ] {
            assert_eq!(
                execute_cli_device_control(&provider, device_id, mode).unwrap(),
                ControlOutcome::Reset {
                    device_id: DeviceId::new(device_id).unwrap(),
                    mode,
                }
            );
        }
        assert!(matches!(
            provider.operations.lock().unwrap().as_slice(),
            [
                ControlOperation::Reset {
                    mode: ResetMode::Reset,
                    ..
                },
                ControlOperation::Reset {
                    mode: ResetMode::Restart,
                    ..
                }
            ]
        ));
    }

    #[test]
    fn cli_device_controls_reject_malformed_selectors_before_dispatch() {
        let provider = FakeProvider::default();
        for selector in [
            "",
            "SEP0011223344559",
            "SEP00112233-455",
            "SEP00112233\n455",
            " SEP001122334455",
        ] {
            assert_eq!(
                execute_cli_device_control(&provider, selector, ResetMode::Reset),
                Err(CliControlError::InvalidDevice)
            );
        }
        assert!(provider.operations.lock().unwrap().is_empty());

        *provider.error.lock().unwrap() = Some(ControlProviderError::DeviceNotRegistered);
        assert_eq!(
            execute_cli_device_control(&provider, "SEP001122334455", ResetMode::Restart),
            Err(CliControlError::Provider(
                ControlProviderError::DeviceNotRegistered
            ))
        );
    }

    #[test]
    fn cli_device_completion_is_bounded_filtered_and_deterministic() {
        let devices = [
            DeviceId::new("SEP556677889900").unwrap(),
            DeviceId::new("SEP001122334466").unwrap(),
            DeviceId::new("SEP001122334455").unwrap(),
        ];
        assert_eq!(
            complete_cli_device(devices.iter(), "sep0011", 0).as_deref(),
            Some("SEP001122334455")
        );
        assert_eq!(
            complete_cli_device(devices.iter(), "sep0011", 1).as_deref(),
            Some("SEP001122334466")
        );
        assert_eq!(complete_cli_device(devices.iter(), "sep0011", 2), None);
        assert_eq!(complete_cli_device(devices.iter(), "SEP-", 0), None);
        assert_eq!(
            complete_cli_device(devices.iter(), "SEP0011223344559", 0),
            None
        );
        assert_eq!(
            complete_cli_value(["silent", "off", "reject", "off"], "", 0, 6).as_deref(),
            Some("off")
        );
        assert_eq!(
            complete_cli_value(["silent", "off", "reject", "off"], "", 1, 6).as_deref(),
            Some("reject")
        );
        assert_eq!(complete_cli_value(["off"], "off\n", 0, 6), None);
    }

    #[test]
    fn cli_message_targets_defaults_and_private_text_use_the_control_provider() {
        let provider = FakeProvider::default();
        for (target, expected, timeout) in [
            ("all", MessageTarget::RegisteredDevices, 10),
            ("system", MessageTarget::System, 0),
            ("sep001122334455", MessageTarget::Device(device()), 10),
        ] {
            let outcome =
                execute_cli_message(&provider, target, "private maintenance notice", None, None)
                    .unwrap();
            assert!(matches!(
                outcome,
                ControlOutcome::Message {
                    target: ref actual,
                    ..
                } if actual == &expected
            ));
            let operation = provider.operations.lock().unwrap().last().cloned().unwrap();
            assert!(matches!(
                operation,
                ControlOperation::Message {
                    target: ref actual,
                    timeout_seconds,
                    beep: false,
                    ..
                } if actual == &expected && timeout_seconds == timeout
            ));
            assert!(!format!("{operation:?}").contains("private maintenance notice"));
        }

        execute_cli_message(
            &provider,
            "SEP001122334455",
            "notice",
            Some("yes"),
            Some("23"),
        )
        .unwrap();
        assert!(matches!(
            provider.operations.lock().unwrap().last(),
            Some(ControlOperation::Message {
                beep: true,
                timeout_seconds: 23,
                ..
            })
        ));
    }

    #[test]
    fn cli_message_rejects_invalid_fields_before_dispatch() {
        for (target, text, beep, timeout, expected) in [
            (
                "not-a-device",
                "notice".to_owned(),
                None,
                None,
                CliControlError::InvalidMessageTarget,
            ),
            (
                "all",
                "x".repeat(MAX_MESSAGE_BYTES + 1),
                None,
                None,
                CliControlError::InvalidMessage,
            ),
            (
                "all",
                "notice".to_owned(),
                Some("sometimes"),
                None,
                CliControlError::InvalidBoolean,
            ),
            (
                "all",
                "notice".to_owned(),
                None,
                Some("256"),
                CliControlError::InvalidTimeout,
            ),
        ] {
            let provider = FakeProvider::default();
            assert_eq!(
                execute_cli_message(&provider, target, &text, beep, timeout),
                Err(expected)
            );
            assert!(provider.operations.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn cli_answer_end_and_originate_are_typed_bounded_and_redacted() {
        let provider = FakeProvider::default();
        assert!(matches!(
            execute_cli_answer(&provider, "42", Some("sep001122334455")).unwrap(),
            ControlOutcome::Answer {
                call_id: CallId(42),
                ..
            }
        ));
        assert!(matches!(
            execute_cli_end(&provider, "77").unwrap(),
            ControlOutcome::End {
                call_id: CallId(77),
                ..
            }
        ));
        let originated = execute_cli_originate(
            &provider,
            "SEP001122334455",
            "private-18005551212",
            Some("1001"),
            Some("private-assigned-id"),
        )
        .unwrap();
        assert!(matches!(
            originated,
            ControlOutcome::Originate {
                call_id: CallId(44),
                ..
            }
        ));
        let operation = provider.operations.lock().unwrap().last().cloned().unwrap();
        assert!(!format!("{operation:?}").contains("private"));

        for result in [
            execute_cli_answer(&provider, "0", None),
            execute_cli_end(&provider, "000000000000000000001"),
            execute_cli_originate(&provider, "bad-device", "1001", None, None),
            execute_cli_originate(
                &provider,
                "SEP001122334455",
                &"9".repeat(MAX_DIAL_DESTINATION_BYTES + 1),
                None,
                None,
            ),
        ] {
            assert!(result.is_err());
        }
    }

    #[test]
    fn legacy_and_current_answer_forms_preserve_ownership_semantics() {
        let provider = FakeProvider::default();
        for (action, fields, expected_device) in [
            (
                ANSWER_CALL_ACTION,
                vec![("DeviceName", "SEP001122334455"), ("ChannelId", "42")],
                Some(device()),
            ),
            (ANSWER_CALL_CURRENT_ACTION, vec![("ChannelId", "42")], None),
            (
                ANSWER_CALL_CURRENT_ACTION,
                vec![("ChannelId", "42"), ("DeviceId", "SEP001122334455")],
                Some(device()),
            ),
        ] {
            assert_eq!(
                handle_control_request(&provider, request(action, &fields)).kind(),
                ManagerResponseKind::Success
            );
            assert!(matches!(
                provider.operations.lock().unwrap().last().unwrap(),
                ControlOperation::Answer {
                    call_id: CallId(42),
                    device_id,
                } if *device_id == expected_device
            ));
        }
    }

    #[test]
    fn end_and_originate_have_typed_ids_aliases_and_redacted_dial_data() {
        let provider = FakeProvider::default();
        assert_eq!(
            handle_control_request(&provider, request(END_CALL_ACTION, &[("ChannelId", "77")]),)
                .kind(),
            ManagerResponseKind::Success
        );
        let originate = handle_control_request(
            &provider,
            request(
                ORIGINATE_ACTION,
                &[
                    ("Devicename", "SEP001122334455"),
                    ("LineName", "1001"),
                    ("Number", "private-18005551212"),
                    ("ChannelId", "assigned-private-id"),
                ],
            ),
        );
        assert_eq!(originate.kind(), ManagerResponseKind::Success);
        let operation = provider.operations.lock().unwrap().last().cloned().unwrap();
        assert!(matches!(
            operation,
            ControlOperation::Originate {
                line: Some(ref line),
                ..
            } if line == "1001"
        ));
        let debug = format!("{operation:?}");
        assert!(!debug.contains("private-18005551212"));
        assert!(!debug.contains("assigned-private-id"));
        assert!(response_value(&originate, "Number").is_none());
        assert!(response_value(&originate, "ChannelId").is_none());
        assert_eq!(response_value(&originate, "CallId"), Some("44"));
    }

    #[test]
    fn duplicate_unknown_sensitive_conflicting_and_missing_fields_are_rejected() {
        let provider = FakeProvider::default();
        let cases = [
            request(
                MESSAGE_DEVICE_ACTION,
                &[
                    ("DeviceId", "SEP001122334455"),
                    ("MessageText", "one"),
                    ("messagetext", "two"),
                ],
            ),
            request(
                MESSAGE_DEVICES_ACTION,
                &[("MessageText", "notice"), ("Secret", "private")],
            ),
            request(MESSAGE_DEVICE_ACTION, &[("DeviceId", "SEP001122334455")]),
            request(
                ORIGINATE_ACTION,
                &[
                    ("DeviceId", "SEP001122334455"),
                    ("Devicename", "SEP556677889900"),
                    ("Number", "1001"),
                ],
            ),
        ];
        for request in cases {
            let response = handle_control_request(&provider, request);
            assert_eq!(response.kind(), ManagerResponseKind::Error);
            assert!(!response.message().unwrap().contains("private"));
        }
        let mut sensitive = request(
            ORIGINATE_ACTION,
            &[("DeviceId", "SEP001122334455"), ("Number", "secret")],
        );
        sensitive.fields[2].sensitive = true;
        assert_eq!(
            handle_control_request(&provider, sensitive).kind(),
            ManagerResponseKind::Error
        );
        assert!(provider.operations.lock().unwrap().is_empty());
    }

    #[test]
    fn message_call_and_assigned_identity_bounds_are_strict() {
        let provider = FakeProvider::default();
        for (action, fields) in [
            (
                MESSAGE_DEVICES_ACTION,
                vec![("MessageText", "x".repeat(MAX_MESSAGE_BYTES + 1))],
            ),
            (
                MESSAGE_DEVICES_ACTION,
                vec![("MessageText", "ok".into()), ("Timeout", "256".into())],
            ),
            (END_CALL_ACTION, vec![("ChannelId", "0".into())]),
            (
                ORIGINATE_ACTION,
                vec![
                    ("DeviceId", "SEP001122334455".into()),
                    ("Number", "x".repeat(MAX_DIAL_DESTINATION_BYTES + 1)),
                ],
            ),
            (
                ORIGINATE_ACTION,
                vec![
                    ("DeviceId", "SEP001122334455".into()),
                    ("Number", "1001".into()),
                    ("ChannelId", "x".repeat(MAX_ASSIGNED_CHANNEL_ID_BYTES + 1)),
                ],
            ),
        ] {
            let borrowed = fields
                .iter()
                .map(|(name, value)| (*name, value.as_str()))
                .collect::<Vec<_>>();
            assert_eq!(
                handle_control_request(&provider, request(action, &borrowed)).kind(),
                ManagerResponseKind::Error
            );
        }
        assert!(provider.operations.lock().unwrap().is_empty());
    }

    #[test]
    fn partial_delivery_is_deterministic_and_never_discloses_text() {
        let provider = FakeProvider::default();
        *provider.outcome.lock().unwrap() = Some(ControlOutcome::Message {
            target: MessageTarget::RegisteredDevices,
            attempted: 4,
            delivered: 2,
            persistent: false,
        });
        let response = handle_control_request(
            &provider,
            request(
                MESSAGE_DEVICES_ACTION,
                &[("MessageText", "private partial notice")],
            ),
        );
        assert_eq!(response.kind(), ManagerResponseKind::Error);
        assert_eq!(response_value(&response, "Attempted"), Some("4"));
        assert_eq!(response_value(&response, "Delivered"), Some("2"));
        assert_eq!(response_value(&response, "Failed"), Some("2"));
        assert!(!format!("{response:?}").contains("private partial notice"));
    }

    #[test]
    fn provider_failures_are_fixed_secret_safe_and_do_not_disclose_selectors() {
        for error in [
            ControlProviderError::Unavailable,
            ControlProviderError::DeviceNotFound,
            ControlProviderError::DeviceNotRegistered,
            ControlProviderError::LineNotFound,
            ControlProviderError::CallNotFound,
            ControlProviderError::CallOwnership,
            ControlProviderError::CallNotRinging,
            ControlProviderError::NoCompatibleCodec,
            ControlProviderError::HandsetDelivery,
            ControlProviderError::Backend,
            ControlProviderError::AssignedChannelIdConflict,
        ] {
            let provider = FakeProvider::default();
            *provider.error.lock().unwrap() = Some(error);
            let response = handle_control_request(
                &provider,
                request(
                    ORIGINATE_ACTION,
                    &[
                        ("DeviceId", "SEP001122334455"),
                        ("Number", "private-18005551212"),
                    ],
                ),
            );
            assert_eq!(response.kind(), ManagerResponseKind::Error);
            let message = response.message().unwrap();
            assert!(!message.contains("SEP001122334455"));
            assert!(!message.contains("private"));
        }
    }

    #[test]
    fn concurrent_callbacks_keep_owned_requests_and_exact_results_separate() {
        let provider = Arc::new(FakeProvider::default());
        let mut workers = Vec::new();
        for call_id in 1..=16_u64 {
            let provider = Arc::clone(&provider);
            workers.push(std::thread::spawn(move || {
                let call_id = call_id.to_string();
                handle_control_request(
                    provider.as_ref(),
                    request(END_CALL_ACTION, &[("ChannelId", call_id.as_str())]),
                )
            }));
        }
        for response in workers.into_iter().map(|worker| worker.join().unwrap()) {
            assert_eq!(response.kind(), ManagerResponseKind::Success);
        }
        let mut call_ids = provider
            .operations
            .lock()
            .unwrap()
            .iter()
            .filter_map(|operation| match operation {
                ControlOperation::End { call_id } => Some(call_id.0),
                _ => None,
            })
            .collect::<Vec<_>>();
        call_ids.sort_unstable();
        assert_eq!(call_ids, (1..=16).collect::<Vec<_>>());
    }

    #[test]
    fn action_names_are_unique_and_raii_registration_is_unavailable_in_development() {
        let names = ControlAction::ALL
            .into_iter()
            .map(ControlAction::name)
            .collect::<BTreeSet<_>>();
        assert_eq!(names.len(), ControlAction::ALL.len());
        assert!(matches!(
            register_control_actions(
                FakeProvider::default(),
                crate::ami::manager::UnavailableManager,
            ),
            Err(ManagerError::Unavailable)
        ));
    }
}
