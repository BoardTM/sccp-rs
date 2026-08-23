//! Typed, secret-safe device information exposed to the dialplan.

use std::fmt;
use std::net::{IpAddr, SocketAddr};

use sccp_protocol::{DeviceId, DeviceType, ProtocolVersion};
use thiserror::Error;

use crate::pbx::dialplan::{
    DialplanBackend, DialplanCallbackError, DialplanError, DialplanEscalation,
    DialplanFunctionHandlers, DialplanLimits,
};
use crate::pbx::party::AsteriskChannel;

pub const DEVICE_QUERY_FUNCTION: &str = "SCCPDevice";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeviceQueryTarget {
    Device(DeviceId),
    Current,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DeviceQueryField {
    Id,
    Configured,
    Registered,
    Description,
    Model,
    ModelId,
    Protocol,
    Address,
    ReportedAddress,
    Firmware,
    LineCount,
    ButtonCount,
    CapabilityCount,
    Dnd,
    Privacy,
    ForwardAll,
    ForwardBusy,
    ForwardNoAnswer,
    EnabledFeatureButtons,
    FeatureSummary,
    CallCount,
    RingingCallCount,
    ConnectedCallCount,
    HeldCallCount,
    SelectedCallCount,
    SelectedLine,
    CallSummary,
}

impl DeviceQueryField {
    fn parse(value: &str) -> Result<Self, DeviceQueryError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "id" => Ok(Self::Id),
            "configured" => Ok(Self::Configured),
            "registered" | "registration_state" | "status" => Ok(Self::Registered),
            "description" => Ok(Self::Description),
            "model" => Ok(Self::Model),
            "model_id" => Ok(Self::ModelId),
            "protocol" | "protocol_version" => Ok(Self::Protocol),
            "address" | "ip" => Ok(Self::Address),
            "reported_address" => Ok(Self::ReportedAddress),
            "firmware" | "image_version" => Ok(Self::Firmware),
            "line_count" | "lines_count" => Ok(Self::LineCount),
            "button_count" => Ok(Self::ButtonCount),
            "capability_count" => Ok(Self::CapabilityCount),
            "dnd" | "dnd_state" => Ok(Self::Dnd),
            "privacy" => Ok(Self::Privacy),
            "forward_all" => Ok(Self::ForwardAll),
            "forward_busy" => Ok(Self::ForwardBusy),
            "forward_no_answer" => Ok(Self::ForwardNoAnswer),
            "enabled_feature_buttons" => Ok(Self::EnabledFeatureButtons),
            "feature_summary" => Ok(Self::FeatureSummary),
            "call_count" => Ok(Self::CallCount),
            "ringing_call_count" => Ok(Self::RingingCallCount),
            "connected_call_count" => Ok(Self::ConnectedCallCount),
            "held_call_count" => Ok(Self::HeldCallCount),
            "selected_call_count" => Ok(Self::SelectedCallCount),
            "selected_line" => Ok(Self::SelectedLine),
            "call_summary" => Ok(Self::CallSummary),
            _ => Err(DeviceQueryError::UnknownField),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceQueryRequest {
    pub target: DeviceQueryTarget,
    pub field: DeviceQueryField,
}

impl DeviceQueryRequest {
    pub fn parse(arguments: &str) -> Result<Self, DeviceQueryError> {
        let mut parts = arguments.split(',');
        let target = parts.next().map(str::trim).unwrap_or_default();
        let field = parts.next().map(str::trim).unwrap_or_default();
        if target.is_empty() || field.is_empty() || parts.next().is_some() {
            return Err(DeviceQueryError::InvalidArguments);
        }
        let target = if target.eq_ignore_ascii_case("current") {
            DeviceQueryTarget::Current
        } else {
            DeviceQueryTarget::Device(
                DeviceId::new(target).map_err(|_| DeviceQueryError::InvalidDevice)?,
            )
        };
        Ok(Self {
            target,
            field: DeviceQueryField::parse(field)?,
        })
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DeviceDndSummary {
    #[default]
    Off,
    Silent,
    Reject,
}

impl fmt::Display for DeviceDndSummary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Off => "off",
            Self::Silent => "silent",
            Self::Reject => "reject",
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegisteredDeviceSummary {
    pub model: DeviceType,
    pub protocol: ProtocolVersion,
    pub address: SocketAddr,
    pub reported_address: Option<IpAddr>,
    pub firmware: String,
    pub capability_count: usize,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DeviceFeatureSummary {
    pub dnd: DeviceDndSummary,
    pub privacy: bool,
    pub forward_all: Option<String>,
    pub forward_busy: Option<String>,
    pub forward_no_answer: Option<String>,
    pub enabled_feature_buttons: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DeviceCallSummary {
    pub total: usize,
    pub ringing: usize,
    pub connected: usize,
    pub held: usize,
    pub selected: usize,
    pub selected_line: Option<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceQuerySnapshot {
    pub id: DeviceId,
    pub configured: bool,
    pub description: Option<String>,
    pub line_count: usize,
    pub button_count: usize,
    pub registration: Option<RegisteredDeviceSummary>,
    pub features: DeviceFeatureSummary,
    pub calls: DeviceCallSummary,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeviceQueryValue {
    Text(String),
    OptionalText(Option<String>),
    Boolean(bool),
    Unsigned(usize),
    Dnd(DeviceDndSummary),
    FeatureSummary(DeviceFeatureSummary),
    CallSummary(DeviceCallSummary),
}

impl DeviceQueryValue {
    pub fn render(&self) -> String {
        match self {
            Self::Text(value) => value.clone(),
            Self::OptionalText(value) => value.clone().unwrap_or_default(),
            Self::Boolean(value) => value.to_string(),
            Self::Unsigned(value) => value.to_string(),
            Self::Dnd(value) => value.to_string(),
            Self::FeatureSummary(value) => format!(
                "dnd={};privacy={};forward_all={};forward_busy={};forward_no_answer={};enabled_buttons={}",
                value.dnd,
                value.privacy,
                value.forward_all.is_some(),
                value.forward_busy.is_some(),
                value.forward_no_answer.is_some(),
                value.enabled_feature_buttons,
            ),
            Self::CallSummary(value) => format!(
                "total={};ringing={};connected={};held={};selected={};selected_line={}",
                value.total,
                value.ringing,
                value.connected,
                value.held,
                value.selected,
                value
                    .selected_line
                    .map_or_else(String::new, |line| line.to_string()),
            ),
        }
    }
}

impl DeviceQuerySnapshot {
    pub fn value(&self, field: DeviceQueryField) -> DeviceQueryValue {
        let registration = self.registration.as_ref();
        match field {
            DeviceQueryField::Id => DeviceQueryValue::Text(self.id.to_string()),
            DeviceQueryField::Configured => DeviceQueryValue::Boolean(self.configured),
            DeviceQueryField::Registered => DeviceQueryValue::Boolean(self.registration.is_some()),
            DeviceQueryField::Description => {
                DeviceQueryValue::OptionalText(self.description.clone())
            }
            DeviceQueryField::Model => DeviceQueryValue::OptionalText(
                registration.map(|registration| format!("{:?}", registration.model)),
            ),
            DeviceQueryField::ModelId => DeviceQueryValue::OptionalText(
                registration.map(|registration| registration.model.wire_value().to_string()),
            ),
            DeviceQueryField::Protocol => DeviceQueryValue::OptionalText(
                registration.map(|registration| registration.protocol.to_string()),
            ),
            DeviceQueryField::Address => DeviceQueryValue::OptionalText(
                registration.map(|registration| registration.address.to_string()),
            ),
            DeviceQueryField::ReportedAddress => {
                DeviceQueryValue::OptionalText(registration.and_then(|registration| {
                    registration.reported_address.map(|ip| ip.to_string())
                }))
            }
            DeviceQueryField::Firmware => DeviceQueryValue::OptionalText(
                registration.map(|registration| registration.firmware.clone()),
            ),
            DeviceQueryField::LineCount => DeviceQueryValue::Unsigned(self.line_count),
            DeviceQueryField::ButtonCount => DeviceQueryValue::Unsigned(self.button_count),
            DeviceQueryField::CapabilityCount => DeviceQueryValue::Unsigned(
                registration.map_or(0, |registration| registration.capability_count),
            ),
            DeviceQueryField::Dnd => DeviceQueryValue::Dnd(self.features.dnd),
            DeviceQueryField::Privacy => DeviceQueryValue::Boolean(self.features.privacy),
            DeviceQueryField::ForwardAll => {
                DeviceQueryValue::OptionalText(self.features.forward_all.clone())
            }
            DeviceQueryField::ForwardBusy => {
                DeviceQueryValue::OptionalText(self.features.forward_busy.clone())
            }
            DeviceQueryField::ForwardNoAnswer => {
                DeviceQueryValue::OptionalText(self.features.forward_no_answer.clone())
            }
            DeviceQueryField::EnabledFeatureButtons => {
                DeviceQueryValue::Unsigned(self.features.enabled_feature_buttons)
            }
            DeviceQueryField::FeatureSummary => {
                DeviceQueryValue::FeatureSummary(self.features.clone())
            }
            DeviceQueryField::CallCount => DeviceQueryValue::Unsigned(self.calls.total),
            DeviceQueryField::RingingCallCount => DeviceQueryValue::Unsigned(self.calls.ringing),
            DeviceQueryField::ConnectedCallCount => {
                DeviceQueryValue::Unsigned(self.calls.connected)
            }
            DeviceQueryField::HeldCallCount => DeviceQueryValue::Unsigned(self.calls.held),
            DeviceQueryField::SelectedCallCount => DeviceQueryValue::Unsigned(self.calls.selected),
            DeviceQueryField::SelectedLine => DeviceQueryValue::OptionalText(
                self.calls.selected_line.map(|line| line.to_string()),
            ),
            DeviceQueryField::CallSummary => DeviceQueryValue::CallSummary(self.calls),
        }
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum DeviceQueryLookupError {
    #[error("the current channel has no associated device")]
    CurrentDeviceUnavailable,
    #[error("device state is unavailable")]
    Unavailable,
}

pub trait DeviceQueryProvider: Send + Sync + 'static {
    fn snapshot(
        &self,
        target: &DeviceQueryTarget,
        channel: Option<&AsteriskChannel<'_>>,
    ) -> Result<Option<DeviceQuerySnapshot>, DeviceQueryLookupError>;
}

pub struct DeviceQuery<P> {
    provider: P,
}

impl<P: DeviceQueryProvider> DeviceQuery<P> {
    pub const fn new(provider: P) -> Self {
        Self { provider }
    }

    pub fn execute(
        &self,
        arguments: &str,
        channel: Option<&AsteriskChannel<'_>>,
    ) -> Result<DeviceQueryValue, DeviceQueryError> {
        let request = DeviceQueryRequest::parse(arguments)?;
        let snapshot = self
            .provider
            .snapshot(&request.target, channel)
            .map_err(DeviceQueryError::Lookup)?
            .ok_or(DeviceQueryError::UnknownDevice)?;
        Ok(snapshot.value(request.field))
    }
}

pub fn register_device_query<P: DeviceQueryProvider, B: DialplanBackend>(
    provider: P,
    backend: B,
) -> Result<B::Registration, DialplanError> {
    let query = DeviceQuery::new(provider);
    backend.register_function(
        DEVICE_QUERY_FUNCTION,
        "Read device state",
        "Read an allowlisted configured or registered device field",
        DialplanEscalation::None,
        DialplanLimits {
            max_arguments_bytes: 256,
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
pub enum DeviceQueryError {
    #[error("device query expects exactly device,field")]
    InvalidArguments,
    #[error("device query contains an invalid device identifier")]
    InvalidDevice,
    #[error("device query field is not allowlisted")]
    UnknownField,
    #[error("device query target is unknown")]
    UnknownDevice,
    #[error(transparent)]
    Lookup(#[from] DeviceQueryLookupError),
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use super::*;

    struct FakeProvider {
        devices: HashMap<DeviceId, DeviceQuerySnapshot>,
        current: Option<DeviceId>,
        failure: Option<DeviceQueryLookupError>,
    }

    impl DeviceQueryProvider for FakeProvider {
        fn snapshot(
            &self,
            target: &DeviceQueryTarget,
            channel: Option<&AsteriskChannel<'_>>,
        ) -> Result<Option<DeviceQuerySnapshot>, DeviceQueryLookupError> {
            if let Some(error) = self.failure {
                return Err(error);
            }
            let id = match target {
                DeviceQueryTarget::Device(id) => id,
                DeviceQueryTarget::Current => {
                    if channel.is_none() {
                        return Err(DeviceQueryLookupError::CurrentDeviceUnavailable);
                    }
                    self.current
                        .as_ref()
                        .ok_or(DeviceQueryLookupError::CurrentDeviceUnavailable)?
                }
            };
            Ok(self.devices.get(id).cloned())
        }
    }

    fn id() -> DeviceId {
        DeviceId::new("SEP001122334455").unwrap()
    }

    fn snapshot() -> DeviceQuerySnapshot {
        DeviceQuerySnapshot {
            id: id(),
            configured: true,
            description: Some("Lobby phone".into()),
            line_count: 2,
            button_count: 5,
            registration: Some(RegisteredDeviceSummary {
                model: DeviceType::Cisco7960,
                protocol: ProtocolVersion::V17,
                address: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10)), 2000),
                reported_address: Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 20))),
                firmware: "SCCP45.9-4-2SR3-1S".into(),
                capability_count: 3,
            }),
            features: DeviceFeatureSummary {
                dnd: DeviceDndSummary::Reject,
                privacy: true,
                forward_all: Some("1000".into()),
                forward_busy: None,
                forward_no_answer: Some("2000".into()),
                enabled_feature_buttons: 2,
            },
            calls: DeviceCallSummary {
                total: 4,
                ringing: 1,
                connected: 1,
                held: 1,
                selected: 2,
                selected_line: Some(3),
            },
        }
    }

    fn query() -> DeviceQuery<FakeProvider> {
        DeviceQuery::new(FakeProvider {
            devices: [(id(), snapshot())].into(),
            current: Some(id()),
            failure: None,
        })
    }

    #[test]
    fn parses_canonical_targets_fields_and_safe_aliases() {
        assert_eq!(
            DeviceQueryRequest::parse("sep001122334455, protocol_version").unwrap(),
            DeviceQueryRequest {
                target: DeviceQueryTarget::Device(id()),
                field: DeviceQueryField::Protocol,
            }
        );
        assert_eq!(
            DeviceQueryRequest::parse("current,ip").unwrap(),
            DeviceQueryRequest {
                target: DeviceQueryTarget::Current,
                field: DeviceQueryField::Address,
            }
        );
    }

    #[test]
    fn rejects_malformed_invalid_and_non_allowlisted_queries() {
        for arguments in ["", "SEP001122334455", ",id", "SEP001,id,extra"] {
            assert_eq!(
                DeviceQueryRequest::parse(arguments),
                Err(DeviceQueryError::InvalidArguments)
            );
        }
        assert_eq!(
            DeviceQueryRequest::parse("bad-device,id"),
            Err(DeviceQueryError::InvalidDevice)
        );
        for field in ["password", "token", "private_key", "acl", "permit"] {
            assert_eq!(
                DeviceQueryRequest::parse(&format!("SEP001122334455,{field}")),
                Err(DeviceQueryError::UnknownField)
            );
        }
    }

    #[test]
    fn returns_typed_identity_registration_and_configuration_values() {
        let query = query();
        for (field, expected) in [
            ("id", DeviceQueryValue::Text(id().to_string())),
            ("configured", DeviceQueryValue::Boolean(true)),
            ("registered", DeviceQueryValue::Boolean(true)),
            (
                "description",
                DeviceQueryValue::OptionalText(Some("Lobby phone".into())),
            ),
            (
                "model",
                DeviceQueryValue::OptionalText(Some("Cisco7960".into())),
            ),
            ("model_id", DeviceQueryValue::OptionalText(Some("7".into()))),
            (
                "protocol",
                DeviceQueryValue::OptionalText(Some("v17".into())),
            ),
            (
                "address",
                DeviceQueryValue::OptionalText(Some("192.0.2.10:2000".into())),
            ),
            (
                "reported_address",
                DeviceQueryValue::OptionalText(Some("10.0.0.20".into())),
            ),
            (
                "firmware",
                DeviceQueryValue::OptionalText(Some("SCCP45.9-4-2SR3-1S".into())),
            ),
            ("line_count", DeviceQueryValue::Unsigned(2)),
            ("button_count", DeviceQueryValue::Unsigned(5)),
            ("capability_count", DeviceQueryValue::Unsigned(3)),
        ] {
            assert_eq!(
                query.execute(&format!("SEP001122334455,{field}"), None),
                Ok(expected)
            );
        }
    }

    #[test]
    fn feature_and_call_queries_are_deterministic_and_summaries_omit_destinations() {
        let query = query();
        for (field, expected) in [
            ("dnd", DeviceQueryValue::Dnd(DeviceDndSummary::Reject)),
            ("privacy", DeviceQueryValue::Boolean(true)),
            (
                "forward_all",
                DeviceQueryValue::OptionalText(Some("1000".into())),
            ),
            ("forward_busy", DeviceQueryValue::OptionalText(None)),
            (
                "forward_no_answer",
                DeviceQueryValue::OptionalText(Some("2000".into())),
            ),
            ("enabled_feature_buttons", DeviceQueryValue::Unsigned(2)),
            ("call_count", DeviceQueryValue::Unsigned(4)),
            ("ringing_call_count", DeviceQueryValue::Unsigned(1)),
            ("connected_call_count", DeviceQueryValue::Unsigned(1)),
            ("held_call_count", DeviceQueryValue::Unsigned(1)),
            ("selected_call_count", DeviceQueryValue::Unsigned(2)),
            (
                "selected_line",
                DeviceQueryValue::OptionalText(Some("3".into())),
            ),
        ] {
            assert_eq!(
                query.execute(&format!("SEP001122334455,{field}"), None),
                Ok(expected)
            );
        }
        let features = query
            .execute("SEP001122334455,feature_summary", None)
            .unwrap()
            .render();
        assert_eq!(
            features,
            "dnd=reject;privacy=true;forward_all=true;forward_busy=false;forward_no_answer=true;enabled_buttons=2"
        );
        assert!(!features.contains("1000"));
        assert!(!features.contains("2000"));
        assert_eq!(
            query
                .execute("SEP001122334455,call_summary", None)
                .unwrap()
                .render(),
            "total=4;ringing=1;connected=1;held=1;selected=2;selected_line=3"
        );
    }

    #[test]
    fn absent_registration_fields_are_empty_without_hiding_configuration() {
        let mut snapshot = snapshot();
        snapshot.registration = None;
        let query = DeviceQuery::new(FakeProvider {
            devices: [(id(), snapshot)].into(),
            current: None,
            failure: None,
        });
        assert_eq!(
            query.execute("SEP001122334455,configured", None),
            Ok(DeviceQueryValue::Boolean(true))
        );
        assert_eq!(
            query.execute("SEP001122334455,registered", None),
            Ok(DeviceQueryValue::Boolean(false))
        );
        assert_eq!(
            query.execute("SEP001122334455,address", None),
            Ok(DeviceQueryValue::OptionalText(None))
        );
    }

    #[test]
    fn unknown_current_and_provider_failures_remain_distinct() {
        let query = query();
        assert_eq!(
            query.execute("SEP999999999999,id", None),
            Err(DeviceQueryError::UnknownDevice)
        );
        assert_eq!(
            query.execute("current,id", None),
            Err(DeviceQueryError::Lookup(
                DeviceQueryLookupError::CurrentDeviceUnavailable
            ))
        );
        let query = DeviceQuery::new(FakeProvider {
            devices: HashMap::new(),
            current: None,
            failure: Some(DeviceQueryLookupError::Unavailable),
        });
        assert_eq!(
            query.execute("SEP001122334455,id", None),
            Err(DeviceQueryError::Lookup(
                DeviceQueryLookupError::Unavailable
            ))
        );
    }

    #[test]
    fn current_target_uses_the_callback_channel() {
        let storage = 1_u8;
        let channel = unsafe {
            AsteriskChannel::from_raw(std::ptr::from_ref(&storage).cast_mut().cast()).unwrap()
        };
        assert_eq!(
            query().execute("current,id", Some(&channel)),
            Ok(DeviceQueryValue::Text(id().to_string()))
        );
    }

    #[test]
    fn registration_is_explicitly_unavailable_without_native_linkage() {
        let result = register_device_query(
            FakeProvider {
                devices: HashMap::new(),
                current: None,
                failure: None,
            },
            crate::pbx::dialplan::UnavailableDialplan,
        );
        assert!(matches!(result, Err(DialplanError::Unavailable)));
    }
}
