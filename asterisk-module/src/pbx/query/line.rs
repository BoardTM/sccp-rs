//! Typed, allowlisted logical-line and appearance information for the dialplan.

use std::fmt;

use sccp_protocol::DeviceId;
use thiserror::Error;

use crate::pbx::dialplan::{
    DialplanBackend, DialplanCallbackError, DialplanError, DialplanEscalation,
    DialplanFunctionHandlers, DialplanLimits,
};
use crate::pbx::party::AsteriskChannel;

pub const LINE_QUERY_FUNCTION: &str = "SCCPLine";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LineQueryTarget {
    Line(String),
    Current,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AppearanceSelector {
    Device(DeviceId),
    Ordered(usize),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LineQueryField {
    Number,
    Label,
    Context,
    CallerName,
    CallerNumber,
    Mailbox,
    AppearanceCount,
    RegisteredAppearanceCount,
    AppearanceOrder,
    CallCount,
    RingingCallCount,
    ConnectedCallCount,
    HeldCallCount,
    CallSummary,
    AppearanceId,
    AppearanceDevice,
    AppearanceInstance,
    AppearanceLabel,
    AppearanceRing,
    AppearancePrivacy,
    AppearanceSubscription,
    AppearanceRegistered,
    AppearanceCallCount,
    AppearanceCallSummary,
}

impl LineQueryField {
    fn parse(value: &str) -> Result<Self, LineQueryError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "id" | "number" => Ok(Self::Number),
            "label" | "name" => Ok(Self::Label),
            "context" => Ok(Self::Context),
            "caller_name" | "cid_name" => Ok(Self::CallerName),
            "caller_number" | "cid_num" => Ok(Self::CallerNumber),
            "mailbox" | "vmnum" => Ok(Self::Mailbox),
            "appearance_count" => Ok(Self::AppearanceCount),
            "registered_appearance_count" => Ok(Self::RegisteredAppearanceCount),
            "appearance_order" => Ok(Self::AppearanceOrder),
            "call_count" | "channel_count" => Ok(Self::CallCount),
            "ringing_call_count" => Ok(Self::RingingCallCount),
            "connected_call_count" => Ok(Self::ConnectedCallCount),
            "held_call_count" => Ok(Self::HeldCallCount),
            "call_summary" => Ok(Self::CallSummary),
            "appearance_id" => Ok(Self::AppearanceId),
            "appearance_device" | "device" => Ok(Self::AppearanceDevice),
            "appearance_instance" | "instance" => Ok(Self::AppearanceInstance),
            "appearance_label" => Ok(Self::AppearanceLabel),
            "appearance_ring" | "ring" => Ok(Self::AppearanceRing),
            "appearance_privacy" | "privacy" => Ok(Self::AppearancePrivacy),
            "appearance_subscription" | "subscription" => Ok(Self::AppearanceSubscription),
            "appearance_registered" | "registered" => Ok(Self::AppearanceRegistered),
            "appearance_call_count" => Ok(Self::AppearanceCallCount),
            "appearance_call_summary" | "call_state_summary" => Ok(Self::AppearanceCallSummary),
            _ => Err(LineQueryError::UnknownField),
        }
    }

    const fn is_appearance(self) -> bool {
        matches!(
            self,
            Self::AppearanceId
                | Self::AppearanceDevice
                | Self::AppearanceInstance
                | Self::AppearanceLabel
                | Self::AppearanceRing
                | Self::AppearancePrivacy
                | Self::AppearanceSubscription
                | Self::AppearanceRegistered
                | Self::AppearanceCallCount
                | Self::AppearanceCallSummary
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LineQueryRequest {
    pub target: LineQueryTarget,
    pub appearance: Option<AppearanceSelector>,
    pub field: LineQueryField,
}

impl LineQueryRequest {
    pub fn parse(arguments: &str) -> Result<Self, LineQueryError> {
        let parts: Vec<_> = arguments.split(',').map(str::trim).collect();
        let (target, appearance, field) = match parts.as_slice() {
            [target, field] if !target.is_empty() && !field.is_empty() => (*target, None, *field),
            [target, appearance, field]
                if !target.is_empty() && !appearance.is_empty() && !field.is_empty() =>
            {
                (*target, Some(parse_selector(appearance)?), *field)
            }
            _ => return Err(LineQueryError::InvalidArguments),
        };
        let target = if target.eq_ignore_ascii_case("current") {
            LineQueryTarget::Current
        } else {
            validate_line(target)?;
            LineQueryTarget::Line(target.to_owned())
        };
        let field = LineQueryField::parse(field)?;
        if appearance.is_some() && !field.is_appearance() {
            return Err(LineQueryError::UnexpectedAppearanceSelector);
        }
        Ok(Self {
            target,
            appearance,
            field,
        })
    }
}

fn validate_line(line: &str) -> Result<(), LineQueryError> {
    if line.is_empty()
        || line.len() > 128
        || line
            .bytes()
            .any(|byte| byte == 0 || byte == b',' || byte.is_ascii_control())
    {
        return Err(LineQueryError::InvalidLine);
    }
    Ok(())
}

fn parse_selector(value: &str) -> Result<AppearanceSelector, LineQueryError> {
    if let Some(index) = value.strip_prefix('#') {
        let index = index
            .parse::<usize>()
            .ok()
            .filter(|index| *index != 0)
            .ok_or(LineQueryError::InvalidAppearanceSelector)?;
        return Ok(AppearanceSelector::Ordered(index));
    }
    DeviceId::new(value)
        .map(AppearanceSelector::Device)
        .map_err(|_| LineQueryError::InvalidAppearanceSelector)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AppearanceRingSummary {
    #[default]
    Normal,
    Silent,
    Disabled,
}

impl fmt::Display for AppearanceRingSummary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Normal => "normal",
            Self::Silent => "silent",
            Self::Disabled => "disabled",
        })
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LineCallSummary {
    pub total: usize,
    pub ringing: usize,
    pub connected: usize,
    pub held: usize,
}

impl LineCallSummary {
    pub fn add(&mut self, other: Self) {
        self.total += other.total;
        self.ringing += other.ringing;
        self.connected += other.connected;
        self.held += other.held;
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LineAppearanceSnapshot {
    pub id: u32,
    pub device_id: DeviceId,
    pub instance: u32,
    pub label: String,
    pub ring: AppearanceRingSummary,
    pub privacy: bool,
    pub subscription: Option<String>,
    pub registered: bool,
    pub calls: LineCallSummary,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LineQuerySnapshot {
    pub number: String,
    pub label: String,
    pub context: String,
    pub caller_name: String,
    pub caller_number: String,
    pub mailbox: Option<String>,
    /// Logical PBX calls, counted once even when presented on several devices.
    pub calls: LineCallSummary,
    /// Sorted by device identity, then instance, then appearance identity.
    pub appearances: Vec<LineAppearanceSnapshot>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LineQueryValue {
    Text(String),
    OptionalText(Option<String>),
    Boolean(bool),
    Unsigned(usize),
    Ring(AppearanceRingSummary),
    CallSummary(LineCallSummary),
    AppearanceOrder(Vec<(DeviceId, u32)>),
}

impl LineQueryValue {
    pub fn render(&self) -> String {
        match self {
            Self::Text(value) => value.clone(),
            Self::OptionalText(value) => value.clone().unwrap_or_default(),
            Self::Boolean(value) => value.to_string(),
            Self::Unsigned(value) => value.to_string(),
            Self::Ring(value) => value.to_string(),
            Self::CallSummary(value) => format!(
                "total={};ringing={};connected={};held={}",
                value.total, value.ringing, value.connected, value.held
            ),
            Self::AppearanceOrder(appearances) => appearances
                .iter()
                .map(|(device, instance)| format!("{device}:{instance}"))
                .collect::<Vec<_>>()
                .join(","),
        }
    }
}

impl LineQuerySnapshot {
    fn sort_appearances(&mut self) {
        self.appearances.sort_by(|left, right| {
            (&left.device_id, left.instance, left.id).cmp(&(
                &right.device_id,
                right.instance,
                right.id,
            ))
        });
    }

    pub fn value(&self, request: &LineQueryRequest) -> Result<LineQueryValue, LineQueryError> {
        if request.field.is_appearance() {
            return self.appearance_value(request);
        }
        let calls = self.calls;
        Ok(match request.field {
            LineQueryField::Number => LineQueryValue::Text(self.number.clone()),
            LineQueryField::Label => LineQueryValue::Text(self.label.clone()),
            LineQueryField::Context => LineQueryValue::Text(self.context.clone()),
            LineQueryField::CallerName => LineQueryValue::Text(self.caller_name.clone()),
            LineQueryField::CallerNumber => LineQueryValue::Text(self.caller_number.clone()),
            LineQueryField::Mailbox => LineQueryValue::OptionalText(self.mailbox.clone()),
            LineQueryField::AppearanceCount => LineQueryValue::Unsigned(self.appearances.len()),
            LineQueryField::RegisteredAppearanceCount => LineQueryValue::Unsigned(
                self.appearances
                    .iter()
                    .filter(|appearance| appearance.registered)
                    .count(),
            ),
            LineQueryField::AppearanceOrder => LineQueryValue::AppearanceOrder(
                self.appearances
                    .iter()
                    .map(|appearance| (appearance.device_id.clone(), appearance.instance))
                    .collect(),
            ),
            LineQueryField::CallCount => LineQueryValue::Unsigned(calls.total),
            LineQueryField::RingingCallCount => LineQueryValue::Unsigned(calls.ringing),
            LineQueryField::ConnectedCallCount => LineQueryValue::Unsigned(calls.connected),
            LineQueryField::HeldCallCount => LineQueryValue::Unsigned(calls.held),
            LineQueryField::CallSummary => LineQueryValue::CallSummary(calls),
            _ => return Err(LineQueryError::UnknownField),
        })
    }

    fn selected_appearance(
        &self,
        selector: Option<&AppearanceSelector>,
    ) -> Result<&LineAppearanceSnapshot, LineQueryError> {
        match selector {
            Some(AppearanceSelector::Device(device)) => self
                .appearances
                .iter()
                .find(|appearance| &appearance.device_id == device)
                .ok_or(LineQueryError::UnknownAppearance),
            Some(AppearanceSelector::Ordered(index)) => self
                .appearances
                .get(index - 1)
                .ok_or(LineQueryError::UnknownAppearance),
            None => match self.appearances.as_slice() {
                [appearance] => Ok(appearance),
                [] => Err(LineQueryError::UnknownAppearance),
                _ => Err(LineQueryError::AmbiguousAppearance),
            },
        }
    }

    fn appearance_value(
        &self,
        request: &LineQueryRequest,
    ) -> Result<LineQueryValue, LineQueryError> {
        let appearance = self.selected_appearance(request.appearance.as_ref())?;
        Ok(match request.field {
            LineQueryField::AppearanceId => LineQueryValue::Unsigned(appearance.id as usize),
            LineQueryField::AppearanceDevice => {
                LineQueryValue::Text(appearance.device_id.to_string())
            }
            LineQueryField::AppearanceInstance => {
                LineQueryValue::Unsigned(appearance.instance as usize)
            }
            LineQueryField::AppearanceLabel => LineQueryValue::Text(appearance.label.clone()),
            LineQueryField::AppearanceRing => LineQueryValue::Ring(appearance.ring),
            LineQueryField::AppearancePrivacy => LineQueryValue::Boolean(appearance.privacy),
            LineQueryField::AppearanceSubscription => {
                LineQueryValue::OptionalText(appearance.subscription.clone())
            }
            LineQueryField::AppearanceRegistered => LineQueryValue::Boolean(appearance.registered),
            LineQueryField::AppearanceCallCount => LineQueryValue::Unsigned(appearance.calls.total),
            LineQueryField::AppearanceCallSummary => LineQueryValue::CallSummary(appearance.calls),
            _ => return Err(LineQueryError::UnknownField),
        })
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum LineQueryLookupError {
    #[error("the current channel has no associated logical line")]
    CurrentLineUnavailable,
    #[error("logical-line state is unavailable")]
    Unavailable,
}

pub trait LineQueryProvider: Send + Sync + 'static {
    fn snapshot(
        &self,
        target: &LineQueryTarget,
        channel: Option<&AsteriskChannel<'_>>,
    ) -> Result<Option<LineQuerySnapshot>, LineQueryLookupError>;
}

pub struct LineQuery<P> {
    provider: P,
}

impl<P: LineQueryProvider> LineQuery<P> {
    pub const fn new(provider: P) -> Self {
        Self { provider }
    }

    pub fn execute(
        &self,
        arguments: &str,
        channel: Option<&AsteriskChannel<'_>>,
    ) -> Result<LineQueryValue, LineQueryError> {
        let request = LineQueryRequest::parse(arguments)?;
        let mut snapshot = self
            .provider
            .snapshot(&request.target, channel)
            .map_err(LineQueryError::Lookup)?
            .ok_or(LineQueryError::UnknownLine)?;
        snapshot.sort_appearances();
        snapshot.value(&request)
    }
}

pub fn register_line_query<P: LineQueryProvider, B: DialplanBackend>(
    provider: P,
    backend: B,
) -> Result<B::Registration, DialplanError> {
    let query = LineQuery::new(provider);
    backend.register_function(
        LINE_QUERY_FUNCTION,
        "Read logical-line state",
        "Read an allowlisted logical-line or configured appearance field",
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
pub enum LineQueryError {
    #[error("line query expects line,field or line,appearance,field")]
    InvalidArguments,
    #[error("line query contains an invalid logical-line identifier")]
    InvalidLine,
    #[error("line query contains an invalid appearance selector")]
    InvalidAppearanceSelector,
    #[error("line query field is not allowlisted")]
    UnknownField,
    #[error("logical-line fields do not accept an appearance selector")]
    UnexpectedAppearanceSelector,
    #[error("line query target is unknown")]
    UnknownLine,
    #[error("line appearance is unknown")]
    UnknownAppearance,
    #[error("line has multiple appearances; select a device or ordered #index")]
    AmbiguousAppearance,
    #[error(transparent)]
    Lookup(#[from] LineQueryLookupError),
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    struct FakeProvider {
        lines: HashMap<String, LineQuerySnapshot>,
        current: Option<String>,
        failure: Option<LineQueryLookupError>,
    }

    impl LineQueryProvider for FakeProvider {
        fn snapshot(
            &self,
            target: &LineQueryTarget,
            channel: Option<&AsteriskChannel<'_>>,
        ) -> Result<Option<LineQuerySnapshot>, LineQueryLookupError> {
            if let Some(error) = self.failure {
                return Err(error);
            }
            let line = match target {
                LineQueryTarget::Line(line) => line,
                LineQueryTarget::Current => {
                    if channel.is_none() {
                        return Err(LineQueryLookupError::CurrentLineUnavailable);
                    }
                    self.current
                        .as_ref()
                        .ok_or(LineQueryLookupError::CurrentLineUnavailable)?
                }
            };
            Ok(self.lines.get(line).cloned())
        }
    }

    fn device(value: &str) -> DeviceId {
        DeviceId::new(value).unwrap()
    }

    fn snapshot() -> LineQuerySnapshot {
        let mut snapshot = LineQuerySnapshot {
            number: "1001".into(),
            label: "Main desk".into(),
            context: "internal".into(),
            caller_name: "Main".into(),
            caller_number: "1001".into(),
            mailbox: Some("1001@default".into()),
            calls: LineCallSummary {
                total: 3,
                ringing: 1,
                connected: 1,
                held: 1,
            },
            appearances: vec![
                LineAppearanceSnapshot {
                    id: 2,
                    device_id: device("SEP001122334455"),
                    instance: 1,
                    label: "Primary".into(),
                    ring: AppearanceRingSummary::Normal,
                    privacy: false,
                    subscription: Some("1001@internal".into()),
                    registered: true,
                    calls: LineCallSummary {
                        total: 2,
                        ringing: 1,
                        connected: 1,
                        held: 0,
                    },
                },
                LineAppearanceSnapshot {
                    id: 7,
                    device_id: device("SEP112233445566"),
                    instance: 3,
                    label: "Shared".into(),
                    ring: AppearanceRingSummary::Silent,
                    privacy: true,
                    subscription: None,
                    registered: false,
                    calls: LineCallSummary {
                        total: 1,
                        ringing: 0,
                        connected: 0,
                        held: 1,
                    },
                },
            ],
        };
        snapshot.appearances.reverse();
        snapshot
    }

    fn query() -> LineQuery<FakeProvider> {
        LineQuery::new(FakeProvider {
            lines: [("1001".into(), snapshot())].into(),
            current: Some("1001".into()),
            failure: None,
        })
    }

    #[test]
    fn parser_distinguishes_logical_device_and_ordered_selectors() {
        assert_eq!(
            LineQueryRequest::parse("1001,context").unwrap(),
            LineQueryRequest {
                target: LineQueryTarget::Line("1001".into()),
                appearance: None,
                field: LineQueryField::Context,
            }
        );
        assert_eq!(
            LineQueryRequest::parse("current,sep001122334455,ring").unwrap(),
            LineQueryRequest {
                target: LineQueryTarget::Current,
                appearance: Some(AppearanceSelector::Device(device("SEP001122334455"))),
                field: LineQueryField::AppearanceRing,
            }
        );
        assert_eq!(
            LineQueryRequest::parse("1001,#2,appearance_id")
                .unwrap()
                .appearance,
            Some(AppearanceSelector::Ordered(2))
        );
    }

    #[test]
    fn malformed_unknown_and_secret_fields_fail_closed() {
        for arguments in [
            "",
            "1001",
            ",id",
            "1001,,id",
            "1001,#0,ring",
            "1001,#x,ring",
        ] {
            assert!(LineQueryRequest::parse(arguments).is_err(), "{arguments}");
        }
        for field in ["password", "secret", "token", "permit", "private_key"] {
            assert_eq!(
                LineQueryRequest::parse(&format!("1001,{field}")),
                Err(LineQueryError::UnknownField)
            );
        }
        assert_eq!(
            LineQueryRequest::parse("1001,SEP001122334455,context"),
            Err(LineQueryError::UnexpectedAppearanceSelector)
        );
    }

    #[test]
    fn logical_fields_and_aggregate_call_summary_are_typed() {
        let query = query();
        for (field, expected) in [
            ("id", LineQueryValue::Text("1001".into())),
            ("label", LineQueryValue::Text("Main desk".into())),
            ("context", LineQueryValue::Text("internal".into())),
            ("caller_name", LineQueryValue::Text("Main".into())),
            ("caller_number", LineQueryValue::Text("1001".into())),
            (
                "mailbox",
                LineQueryValue::OptionalText(Some("1001@default".into())),
            ),
            ("appearance_count", LineQueryValue::Unsigned(2)),
            ("registered_appearance_count", LineQueryValue::Unsigned(1)),
            ("call_count", LineQueryValue::Unsigned(3)),
            ("ringing_call_count", LineQueryValue::Unsigned(1)),
            ("connected_call_count", LineQueryValue::Unsigned(1)),
            ("held_call_count", LineQueryValue::Unsigned(1)),
        ] {
            assert_eq!(query.execute(&format!("1001,{field}"), None), Ok(expected));
        }
        assert_eq!(
            query.execute("1001,call_summary", None).unwrap().render(),
            "total=3;ringing=1;connected=1;held=1"
        );
    }

    #[test]
    fn appearance_fields_use_exact_device_or_one_based_order() {
        let query = query();
        for (selector, field, expected) in [
            (
                "SEP001122334455",
                "appearance_device",
                LineQueryValue::Text("SEP001122334455".into()),
            ),
            ("SEP001122334455", "instance", LineQueryValue::Unsigned(1)),
            ("#2", "appearance_id", LineQueryValue::Unsigned(7)),
            (
                "#2",
                "appearance_label",
                LineQueryValue::Text("Shared".into()),
            ),
            (
                "#2",
                "ring",
                LineQueryValue::Ring(AppearanceRingSummary::Silent),
            ),
            (
                "#1",
                "subscription",
                LineQueryValue::OptionalText(Some("1001@internal".into())),
            ),
            ("#2", "privacy", LineQueryValue::Boolean(true)),
            ("#2", "registered", LineQueryValue::Boolean(false)),
            ("#2", "appearance_call_count", LineQueryValue::Unsigned(1)),
        ] {
            assert_eq!(
                query.execute(&format!("1001,{selector},{field}"), None),
                Ok(expected)
            );
        }
    }

    #[test]
    fn ordering_and_call_summaries_are_deterministic() {
        let query = query();
        assert_eq!(
            query
                .execute("1001,appearance_order", None)
                .unwrap()
                .render(),
            "SEP001122334455:1,SEP112233445566:3"
        );
        assert_eq!(
            query
                .execute("1001,#1,appearance_call_summary", None)
                .unwrap()
                .render(),
            "total=2;ringing=1;connected=1;held=0"
        );
    }

    #[test]
    fn ambiguous_unknown_line_and_unknown_appearance_are_distinct() {
        let query = query();
        assert_eq!(
            query.execute("1001,ring", None),
            Err(LineQueryError::AmbiguousAppearance)
        );
        assert_eq!(
            query.execute("9999,id", None),
            Err(LineQueryError::UnknownLine)
        );
        assert_eq!(
            query.execute("1001,#3,ring", None),
            Err(LineQueryError::UnknownAppearance)
        );
        assert_eq!(
            query.execute("1001,SEP999999999999,ring", None),
            Err(LineQueryError::UnknownAppearance)
        );

        let mut single = snapshot();
        single
            .appearances
            .retain(|appearance| appearance.device_id == device("SEP001122334455"));
        let single = LineQuery::new(FakeProvider {
            lines: [("1001".into(), single)].into(),
            current: None,
            failure: None,
        });
        assert_eq!(
            single.execute("1001,ring", None),
            Ok(LineQueryValue::Ring(AppearanceRingSummary::Normal))
        );
    }

    #[test]
    fn current_requires_a_callback_channel_and_provider_identity() {
        assert_eq!(
            query().execute("current,id", None),
            Err(LineQueryError::Lookup(
                LineQueryLookupError::CurrentLineUnavailable
            ))
        );
        let storage = 1_u8;
        let channel = unsafe {
            AsteriskChannel::from_raw(std::ptr::from_ref(&storage).cast_mut().cast()).unwrap()
        };
        assert_eq!(
            query().execute("current,id", Some(&channel)),
            Ok(LineQueryValue::Text("1001".into()))
        );
        let unavailable = LineQuery::new(FakeProvider {
            lines: HashMap::new(),
            current: None,
            failure: Some(LineQueryLookupError::Unavailable),
        });
        assert_eq!(
            unavailable.execute("1001,id", None),
            Err(LineQueryError::Lookup(LineQueryLookupError::Unavailable))
        );
    }

    #[test]
    fn registration_is_unavailable_without_native_linkage() {
        let result = register_line_query(
            FakeProvider {
                lines: HashMap::new(),
                current: None,
                failure: None,
            },
            crate::pbx::dialplan::UnavailableDialplan,
        );
        assert!(matches!(result, Err(DialplanError::Unavailable)));
    }
}
