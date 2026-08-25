//! Typed management inventory actions for configured stations and lines.
//!
//! List actions have no selectors except optional `DeviceName`/`LineName`
//! filters for appearances and optional `DeviceName` for buttons. Detail
//! selectors are `DeviceName`, `LineName`, `DeviceName` + positive
//! `LineInstance`, or `DeviceName` + positive `Position`, matching the action's
//! object type. Configuration and live registration are copied into one
//! immutable snapshot and normalized into device ID, line name,
//! device/instance, and device/position order before AMI output.
//!
//! Duplicate identities, unknown/duplicate/sensitive selectors, missing
//! objects, more than 40 list items, and the shared 512-field/64-KiB limits fail
//! the complete action. No password, PIN, channel variable value, or raw backend
//! row is an inventory field.

use std::collections::BTreeMap;

use sccp_protocol::{AppearanceRingMode, ButtonDefinition, DeviceId};
use thiserror::Error;

use crate::ami::manager::{
    ActionDefinition, ManagerBackend, ManagerError, ManagerField, ManagerLimits, ManagerPrivilege,
    ManagerRequest, ManagerResponse, RequestFields, register_action_group,
};
use crate::config::ModuleConfig;

pub const SHOW_DEVICES_ACTION: &str = "SCCPShowDevices";
pub const SHOW_DEVICE_ACTION: &str = "SCCPShowDevice";
pub const SHOW_LINES_ACTION: &str = "SCCPShowLines";
pub const SHOW_LINE_ACTION: &str = "SCCPShowLine";
pub const SHOW_APPEARANCES_ACTION: &str = "SCCPShowAppearances";
pub const SHOW_APPEARANCE_ACTION: &str = "SCCPShowAppearance";
pub const SHOW_BUTTONS_ACTION: &str = "SCCPShowButtons";
pub const SHOW_BUTTON_ACTION: &str = "SCCPShowButton";

const MAX_LIST_ITEMS: usize = 40;
const MAX_RESPONSE_FIELDS: usize = 512;
const MAX_FIELD_VALUE_BYTES: usize = 4096;
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

const INVENTORY_LIMITS: ManagerLimits = ManagerLimits {
    max_fields: MAX_RESPONSE_FIELDS,
    max_field_name_bytes: 64,
    max_field_value_bytes: MAX_FIELD_VALUE_BYTES,
    max_response_bytes: MAX_RESPONSE_BYTES,
};
const INVENTORY_PRIVILEGES: ManagerPrivilege = ManagerPrivilege::SYSTEM
    .union(ManagerPrivilege::CONFIG)
    .union(ManagerPrivilege::REPORTING);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InventoryRegistration {
    pub model: String,
    pub model_id: u32,
    pub protocol: String,
    pub address: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InventoryDevice {
    pub id: DeviceId,
    pub description: String,
    pub line_count: usize,
    pub button_count: usize,
    pub registration: Option<InventoryRegistration>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InventoryLine {
    pub number: String,
    pub label: String,
    pub context: String,
    pub caller_name: String,
    pub caller_number: String,
    pub mailbox: Option<String>,
    pub appearance_count: usize,
    pub registered_appearance_count: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InventoryAppearance {
    pub device_id: DeviceId,
    pub line_instance: u32,
    pub appearance_id: u32,
    pub line: String,
    pub label: String,
    pub ring: AppearanceRingMode,
    pub privacy: bool,
    pub subscription: Option<InventoryValue>,
    pub registered: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InventoryButtonKind {
    Line,
    SpeedDial,
    BlfSpeedDial,
    Feature,
    Service,
    AddonModule,
    Unused,
}

impl InventoryButtonKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Line => "line",
            Self::SpeedDial => "speed-dial",
            Self::BlfSpeedDial => "blf-speed-dial",
            Self::Feature => "feature",
            Self::Service => "service",
            Self::AddonModule => "addon-module",
            Self::Unused => "unused",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InventoryValue {
    Public(String),
    Redacted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InventoryButton {
    pub device_id: DeviceId,
    /// One-based physical position in the configured button vector.
    pub position: usize,
    pub kind: InventoryButtonKind,
    pub instance: Option<u32>,
    pub label: String,
    pub target: Option<InventoryValue>,
    pub hint: Option<InventoryValue>,
    pub argument: Option<InventoryValue>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct InventorySnapshot {
    pub devices: Vec<InventoryDevice>,
    pub lines: Vec<InventoryLine>,
    pub appearances: Vec<InventoryAppearance>,
    pub buttons: Vec<InventoryButton>,
}

/// Build an immutable, deterministically ordered inventory from normalized
/// configuration and a runtime registration snapshot.
pub fn configured_inventory(
    config: &ModuleConfig,
    registrations: &BTreeMap<DeviceId, InventoryRegistration>,
) -> InventorySnapshot {
    let mut device_ids = config.devices.keys().cloned().collect::<Vec<_>>();
    device_ids.sort();
    let devices = device_ids
        .iter()
        .filter_map(|id| {
            config.devices.get(id).map(|device| InventoryDevice {
                id: id.clone(),
                description: device.description.clone(),
                line_count: device.lines.len(),
                button_count: device.buttons.len(),
                registration: registrations.get(id).cloned(),
            })
        })
        .collect();

    let mut appearances = Vec::new();
    let mut buttons = Vec::new();
    for device_id in &device_ids {
        let Some(device) = config.devices.get(device_id) else {
            continue;
        };
        let registered = registrations.contains_key(device_id);
        for binding in config.appearances_for_device(device_id) {
            appearances.push(InventoryAppearance {
                device_id: device_id.clone(),
                line_instance: binding.line_instance,
                appearance_id: binding.appearance.id.get(),
                line: binding.line.number.clone(),
                label: binding.appearance.display_label().to_owned(),
                ring: binding.appearance.ring_mode,
                privacy: binding.appearance.privacy,
                subscription: binding
                    .appearance
                    .subscription_identity
                    .as_ref()
                    .map(|value| {
                        if binding.appearance.privacy {
                            InventoryValue::Redacted
                        } else {
                            InventoryValue::Public(value.clone())
                        }
                    }),
                registered,
            });
        }
        for (index, button) in device.buttons.iter().enumerate() {
            let mut configured = inventory_button(
                device_id,
                index + 1,
                button,
                match button {
                    ButtonDefinition::BlfSpeedDial(speed_dial) => {
                        device.blf_targets.get(&speed_dial.instance)
                    }
                    _ => None,
                },
            );
            if let ButtonDefinition::Feature(feature) = button
                && device.feature_arguments.contains_key(&feature.instance)
            {
                configured.argument = Some(InventoryValue::Redacted);
            }
            buttons.push(configured);
        }
    }
    appearances.sort_by(|left, right| {
        (&left.device_id, left.line_instance, left.appearance_id).cmp(&(
            &right.device_id,
            right.line_instance,
            right.appearance_id,
        ))
    });

    let mut line_numbers = config.lines.keys().cloned().collect::<Vec<_>>();
    line_numbers.sort();
    let lines = line_numbers
        .into_iter()
        .filter_map(|number| {
            let line = config.lines.get(&number)?;
            let line_appearances = appearances
                .iter()
                .filter(|appearance| appearance.line == number)
                .collect::<Vec<_>>();
            Some(InventoryLine {
                number,
                label: line.label.clone(),
                context: line.context.clone(),
                caller_name: line.caller_name.clone(),
                caller_number: line.caller_number.clone(),
                mailbox: line.mailbox.clone(),
                appearance_count: line_appearances.len(),
                registered_appearance_count: line_appearances
                    .iter()
                    .filter(|appearance| appearance.registered)
                    .count(),
            })
        })
        .collect();

    InventorySnapshot {
        devices,
        lines,
        appearances,
        buttons,
    }
}

fn inventory_button(
    device_id: &DeviceId,
    position: usize,
    button: &ButtonDefinition,
    blf_target: Option<&crate::config::HintTarget>,
) -> InventoryButton {
    let (kind, instance, label, target) = match button {
        ButtonDefinition::Line(line) => (
            InventoryButtonKind::Line,
            Some(line.instance),
            line.display_label().to_owned(),
            Some(InventoryValue::Public(line.number.clone())),
        ),
        ButtonDefinition::SpeedDial(speed_dial) => (
            InventoryButtonKind::SpeedDial,
            Some(speed_dial.instance),
            speed_dial.display_name.clone(),
            Some(InventoryValue::Public(speed_dial.number.clone())),
        ),
        ButtonDefinition::BlfSpeedDial(speed_dial) => (
            InventoryButtonKind::BlfSpeedDial,
            Some(speed_dial.instance),
            speed_dial.display_name.clone(),
            Some(InventoryValue::Public(speed_dial.number.clone())),
        ),
        ButtonDefinition::Feature(feature) => (
            InventoryButtonKind::Feature,
            Some(feature.instance),
            feature.label.clone(),
            Some(InventoryValue::Public(
                feature.feature.wire_value().to_string(),
            )),
        ),
        ButtonDefinition::Service(service) => (
            InventoryButtonKind::Service,
            Some(service.instance),
            service.label.clone(),
            Some(InventoryValue::Redacted),
        ),
        ButtonDefinition::AddonModule(addon) => (
            InventoryButtonKind::AddonModule,
            Some(addon.slot),
            String::new(),
            Some(InventoryValue::Public(
                addon.device_type.wire_value().to_string(),
            )),
        ),
        ButtonDefinition::Unused => (InventoryButtonKind::Unused, None, String::new(), None),
    };
    let hint = blf_target.map(|target| InventoryValue::Public(target.to_string()));
    InventoryButton {
        device_id: device_id.clone(),
        position,
        kind,
        instance,
        label,
        target,
        hint,
        argument: None,
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum InventoryProviderError {
    #[error("management inventory is unavailable")]
    Unavailable,
}

pub trait InventoryProvider: Send + Sync + 'static {
    fn snapshot(&self) -> Result<InventorySnapshot, InventoryProviderError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InventoryAction {
    DeviceList,
    DeviceDetail,
    LineList,
    LineDetail,
    AppearanceList,
    AppearanceDetail,
    ButtonList,
    ButtonDetail,
}

impl InventoryAction {
    const ALL: [Self; 8] = [
        Self::DeviceList,
        Self::DeviceDetail,
        Self::LineList,
        Self::LineDetail,
        Self::AppearanceList,
        Self::AppearanceDetail,
        Self::ButtonList,
        Self::ButtonDetail,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::DeviceList => SHOW_DEVICES_ACTION,
            Self::DeviceDetail => SHOW_DEVICE_ACTION,
            Self::LineList => SHOW_LINES_ACTION,
            Self::LineDetail => SHOW_LINE_ACTION,
            Self::AppearanceList => SHOW_APPEARANCES_ACTION,
            Self::AppearanceDetail => SHOW_APPEARANCE_ACTION,
            Self::ButtonList => SHOW_BUTTONS_ACTION,
            Self::ButtonDetail => SHOW_BUTTON_ACTION,
        }
    }

    const fn synopsis(self) -> &'static str {
        match self {
            Self::DeviceList => "List configured devices",
            Self::DeviceDetail => "Show one configured device",
            Self::LineList => "List configured lines",
            Self::LineDetail => "Show one configured line",
            Self::AppearanceList => "List configured appearances",
            Self::AppearanceDetail => "Show one line appearance",
            Self::ButtonList => "List configured buttons",
            Self::ButtonDetail => "Show one configured button",
        }
    }

    const fn description(self) -> &'static str {
        match self {
            Self::DeviceList => "List configured devices in deterministic identifier order.",
            Self::DeviceDetail => "Show allowlisted fields for one configured device.",
            Self::LineList => "List logical lines in deterministic number order.",
            Self::LineDetail => "Show allowlisted fields for one logical line.",
            Self::AppearanceList => {
                "List configured line appearances in deterministic device and instance order."
            }
            Self::AppearanceDetail => "Show allowlisted fields for one configured appearance.",
            Self::ButtonList => {
                "List physical button definitions in deterministic configured order."
            }
            Self::ButtonDetail => "Show one physical button definition by one-based position.",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|action| action.name().eq_ignore_ascii_case(name))
    }
}

impl<P: InventoryProvider> ActionDefinition<P> for InventoryAction {
    fn name(self) -> &'static str {
        self.name()
    }

    fn synopsis(self) -> &'static str {
        self.synopsis()
    }

    fn description(self) -> &'static str {
        self.description()
    }

    fn privileges(self) -> ManagerPrivilege {
        INVENTORY_PRIVILEGES
    }

    fn limits(self) -> ManagerLimits {
        INVENTORY_LIMITS
    }

    fn handle(self, provider: &P, request: ManagerRequest) -> ManagerResponse {
        handle_inventory_request(provider, request)
    }
}

/// Register all inventory actions as one RAII-owned lifecycle group. A failure
/// drops every action already registered by this call.
pub fn register_inventory_actions<P: InventoryProvider, M: ManagerBackend>(
    provider: P,
    manager: M,
) -> Result<Vec<M::Registration>, ManagerError> {
    register_action_group(provider, manager, &InventoryAction::ALL)
}

pub fn handle_inventory_request<P: InventoryProvider + ?Sized>(
    provider: &P,
    request: ManagerRequest,
) -> ManagerResponse {
    match execute_inventory_request(provider, &request) {
        Ok(response) => response,
        Err(error) => ManagerResponse::error(error.response_message())
            .expect("fixed inventory error message is valid"),
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
enum InventoryActionError {
    #[error("unknown inventory action")]
    UnknownAction,
    #[error("request field is not allowlisted")]
    UnknownField,
    #[error("request repeats a singleton field")]
    DuplicateField,
    #[error("request contains sensitive metadata")]
    SensitiveField,
    #[error("request selector is missing or malformed")]
    InvalidSelector,
    #[error("requested inventory object is absent")]
    NotFound,
    #[error("management inventory contains duplicate identities")]
    DuplicateObject,
    #[error("inventory result exceeds its bounded item limit")]
    TooManyItems,
    #[error("inventory response exceeds its bounded size limit")]
    ResponseTooLarge,
    #[error("inventory response cannot be represented safely")]
    InvalidOutput,
    #[error(transparent)]
    Provider(#[from] InventoryProviderError),
}

crate::ami::manager::impl_request_fields_error!(InventoryActionError);

impl InventoryActionError {
    const fn response_message(self) -> &'static str {
        match self {
            Self::UnknownAction => "Unknown inventory action",
            Self::UnknownField => "Request field is not allowlisted",
            Self::DuplicateField => "Request repeats a singleton field",
            Self::SensitiveField => "Sensitive request fields are not accepted",
            Self::InvalidSelector => "Request selector is missing or malformed",
            Self::NotFound => "Requested inventory object was not found",
            Self::DuplicateObject => "Management inventory contains duplicate identities",
            Self::TooManyItems => "Inventory result exceeds the bounded item limit",
            Self::ResponseTooLarge => "Inventory response exceeds the bounded size limit",
            Self::InvalidOutput => "Inventory response cannot be represented safely",
            Self::Provider(_) => "Management inventory is unavailable",
        }
    }
}

fn execute_inventory_request<P: InventoryProvider + ?Sized>(
    provider: &P,
    request: &ManagerRequest,
) -> Result<ManagerResponse, InventoryActionError> {
    let action =
        InventoryAction::parse(&request.action).ok_or(InventoryActionError::UnknownAction)?;
    let allowed = match action {
        InventoryAction::DeviceList | InventoryAction::LineList => &[][..],
        InventoryAction::DeviceDetail => &["devicename"][..],
        InventoryAction::LineDetail => &["linename"][..],
        InventoryAction::AppearanceList => &["devicename", "linename"][..],
        InventoryAction::AppearanceDetail => &["devicename", "lineinstance"][..],
        InventoryAction::ButtonList => &["devicename"][..],
        InventoryAction::ButtonDetail => &["devicename", "position"][..],
    };
    let selectors = parse_selectors(request, allowed)?;
    let mut snapshot = provider.snapshot()?;
    normalize_snapshot(&mut snapshot)?;
    let fields = match action {
        InventoryAction::DeviceList => device_list_fields(&snapshot.devices)?,
        InventoryAction::DeviceDetail => {
            let device = parse_device_selector(&selectors, "devicename")?;
            let item = snapshot
                .devices
                .iter()
                .find(|item| item.id == device)
                .ok_or(InventoryActionError::NotFound)?;
            device_fields(item, None)?
        }
        InventoryAction::LineList => line_list_fields(&snapshot.lines)?,
        InventoryAction::LineDetail => {
            let number = selector(&selectors, "linename")?;
            validate_text_selector(number)?;
            let item = snapshot
                .lines
                .iter()
                .find(|item| item.number == number)
                .ok_or(InventoryActionError::NotFound)?;
            line_fields(item, None)?
        }
        InventoryAction::AppearanceList => {
            let device = optional_device_selector(&selectors, "devicename")?;
            let line = selectors.get("linename").map(String::as_str);
            if let Some(line) = line {
                validate_text_selector(line)?;
            }
            let items = snapshot
                .appearances
                .iter()
                .filter(|item| {
                    device
                        .as_ref()
                        .is_none_or(|device| item.device_id == *device)
                })
                .filter(|item| line.is_none_or(|line| item.line == line))
                .collect::<Vec<_>>();
            appearance_list_fields(&items)?
        }
        InventoryAction::AppearanceDetail => {
            let device = parse_device_selector(&selectors, "devicename")?;
            let line_instance = parse_positive_u32(selector(&selectors, "lineinstance")?)?;
            let item = snapshot
                .appearances
                .iter()
                .find(|item| item.device_id == device && item.line_instance == line_instance)
                .ok_or(InventoryActionError::NotFound)?;
            appearance_fields(item, None)?
        }
        InventoryAction::ButtonList => {
            let device = optional_device_selector(&selectors, "devicename")?;
            let items = snapshot
                .buttons
                .iter()
                .filter(|item| {
                    device
                        .as_ref()
                        .is_none_or(|device| item.device_id == *device)
                })
                .collect::<Vec<_>>();
            button_list_fields(&items)?
        }
        InventoryAction::ButtonDetail => {
            let device = parse_device_selector(&selectors, "devicename")?;
            let position = parse_positive_usize(selector(&selectors, "position")?)?;
            let item = snapshot
                .buttons
                .iter()
                .find(|item| item.device_id == device && item.position == position)
                .ok_or(InventoryActionError::NotFound)?;
            button_fields(item, None)?
        }
    };
    bounded_success(fields)
}

fn normalize_snapshot(snapshot: &mut InventorySnapshot) -> Result<(), InventoryActionError> {
    snapshot
        .devices
        .sort_by(|left, right| left.id.cmp(&right.id));
    if snapshot
        .devices
        .windows(2)
        .any(|items| items[0].id == items[1].id)
    {
        return Err(InventoryActionError::DuplicateObject);
    }

    snapshot
        .lines
        .sort_by(|left, right| left.number.cmp(&right.number));
    if snapshot
        .lines
        .windows(2)
        .any(|items| items[0].number == items[1].number)
    {
        return Err(InventoryActionError::DuplicateObject);
    }

    snapshot.appearances.sort_by(|left, right| {
        (&left.device_id, left.line_instance, left.appearance_id).cmp(&(
            &right.device_id,
            right.line_instance,
            right.appearance_id,
        ))
    });
    if snapshot.appearances.windows(2).any(|items| {
        items[0].device_id == items[1].device_id && items[0].line_instance == items[1].line_instance
    }) {
        return Err(InventoryActionError::DuplicateObject);
    }

    snapshot.buttons.sort_by(|left, right| {
        (&left.device_id, left.position).cmp(&(&right.device_id, right.position))
    });
    if snapshot.buttons.windows(2).any(|items| {
        items[0].device_id == items[1].device_id && items[0].position == items[1].position
    }) {
        return Err(InventoryActionError::DuplicateObject);
    }
    Ok(())
}

fn parse_selectors(
    request: &ManagerRequest,
    allowed: &[&str],
) -> Result<BTreeMap<String, String>, InventoryActionError> {
    RequestFields::new(request)
        .collect(allowed, &[])
        .map_err(Into::into)
}

fn selector<'a>(
    selectors: &'a BTreeMap<String, String>,
    name: &str,
) -> Result<&'a str, InventoryActionError> {
    selectors
        .get(name)
        .map(String::as_str)
        .ok_or(InventoryActionError::InvalidSelector)
}

fn parse_device_selector(
    selectors: &BTreeMap<String, String>,
    name: &str,
) -> Result<DeviceId, InventoryActionError> {
    DeviceId::new(selector(selectors, name)?).map_err(|_| InventoryActionError::InvalidSelector)
}

fn optional_device_selector(
    selectors: &BTreeMap<String, String>,
    name: &str,
) -> Result<Option<DeviceId>, InventoryActionError> {
    selectors
        .get(name)
        .map(|value| DeviceId::new(value).map_err(|_| InventoryActionError::InvalidSelector))
        .transpose()
}

fn validate_text_selector(value: &str) -> Result<(), InventoryActionError> {
    if value.is_empty() || value.len() > 80 || value.chars().any(char::is_control) {
        Err(InventoryActionError::InvalidSelector)
    } else {
        Ok(())
    }
}

fn parse_positive_u32(value: &str) -> Result<u32, InventoryActionError> {
    value
        .parse::<u32>()
        .ok()
        .filter(|value| *value != 0)
        .ok_or(InventoryActionError::InvalidSelector)
}

fn parse_positive_usize(value: &str) -> Result<usize, InventoryActionError> {
    value
        .parse::<usize>()
        .ok()
        .filter(|value| *value != 0)
        .ok_or(InventoryActionError::InvalidSelector)
}

fn device_list_fields(
    items: &[InventoryDevice],
) -> Result<Vec<ManagerField>, InventoryActionError> {
    ensure_list_bound(items.len())?;
    let mut fields = vec![public("Count", items.len())?];
    for (index, item) in items.iter().enumerate() {
        fields.extend(device_fields(item, Some(index + 1))?);
    }
    Ok(fields)
}

fn device_fields(
    item: &InventoryDevice,
    index: Option<usize>,
) -> Result<Vec<ManagerField>, InventoryActionError> {
    let mut fields = object_prefix("Device", index)?;
    fields.extend([
        public("DeviceId", item.id.as_str())?,
        public("Description", &item.description)?,
        public("Registered", yes_no(item.registration.is_some()))?,
        public("LineCount", item.line_count)?,
        public("ButtonCount", item.button_count)?,
        public(
            "Model",
            item.registration
                .as_ref()
                .map_or("", |registration| registration.model.as_str()),
        )?,
        public(
            "ModelId",
            item.registration
                .as_ref()
                .map_or_else(String::new, |registration| {
                    registration.model_id.to_string()
                }),
        )?,
        public(
            "Protocol",
            item.registration
                .as_ref()
                .map_or("", |registration| registration.protocol.as_str()),
        )?,
        public(
            "Address",
            item.registration
                .as_ref()
                .map_or("", |registration| registration.address.as_str()),
        )?,
    ]);
    Ok(fields)
}

fn line_list_fields(items: &[InventoryLine]) -> Result<Vec<ManagerField>, InventoryActionError> {
    ensure_list_bound(items.len())?;
    let mut fields = vec![public("Count", items.len())?];
    for (index, item) in items.iter().enumerate() {
        fields.extend(line_fields(item, Some(index + 1))?);
    }
    Ok(fields)
}

fn line_fields(
    item: &InventoryLine,
    index: Option<usize>,
) -> Result<Vec<ManagerField>, InventoryActionError> {
    let mut fields = object_prefix("Line", index)?;
    fields.extend([
        public("LineName", &item.number)?,
        public("Label", &item.label)?,
        public("Context", &item.context)?,
        public("CallerName", &item.caller_name)?,
        public("CallerNumber", &item.caller_number)?,
        public("Mailbox", item.mailbox.as_deref().unwrap_or(""))?,
        public("AppearanceCount", item.appearance_count)?,
        public(
            "RegisteredAppearanceCount",
            item.registered_appearance_count,
        )?,
    ]);
    Ok(fields)
}

fn appearance_list_fields(
    items: &[&InventoryAppearance],
) -> Result<Vec<ManagerField>, InventoryActionError> {
    ensure_list_bound(items.len())?;
    let mut fields = vec![public("Count", items.len())?];
    for (index, item) in items.iter().enumerate() {
        fields.extend(appearance_fields(item, Some(index + 1))?);
    }
    Ok(fields)
}

fn appearance_fields(
    item: &InventoryAppearance,
    index: Option<usize>,
) -> Result<Vec<ManagerField>, InventoryActionError> {
    let mut fields = object_prefix("Appearance", index)?;
    fields.extend([
        public("DeviceId", item.device_id.as_str())?,
        public("LineInstance", item.line_instance)?,
        public("AppearanceId", item.appearance_id)?,
        public("LineName", &item.line)?,
        public("Label", &item.label)?,
        public("Ring", ring_name(item.ring))?,
        public("Privacy", yes_no(item.privacy))?,
        public("Registered", yes_no(item.registered))?,
    ]);
    append_inventory_value(&mut fields, "Subscription", item.subscription.as_ref())?;
    Ok(fields)
}

fn button_list_fields(
    items: &[&InventoryButton],
) -> Result<Vec<ManagerField>, InventoryActionError> {
    ensure_list_bound(items.len())?;
    let mut fields = vec![public("Count", items.len())?];
    for (index, item) in items.iter().enumerate() {
        fields.extend(button_fields(item, Some(index + 1))?);
    }
    Ok(fields)
}

fn button_fields(
    item: &InventoryButton,
    index: Option<usize>,
) -> Result<Vec<ManagerField>, InventoryActionError> {
    let mut fields = object_prefix("Button", index)?;
    fields.extend([
        public("DeviceId", item.device_id.as_str())?,
        public("Position", item.position)?,
        public("Kind", item.kind.as_str())?,
        public(
            "Instance",
            item.instance
                .map_or_else(String::new, |value| value.to_string()),
        )?,
        public("Label", &item.label)?,
    ]);
    match &item.target {
        Some(InventoryValue::Public(value)) => fields.push(public("Target", value)?),
        Some(InventoryValue::Redacted) => fields.push(redacted("Target")?),
        None => fields.push(public("Target", "")?),
    }
    append_inventory_value(&mut fields, "Hint", item.hint.as_ref())?;
    append_inventory_value(&mut fields, "Argument", item.argument.as_ref())?;
    Ok(fields)
}

fn append_inventory_value(
    fields: &mut Vec<ManagerField>,
    name: &'static str,
    value: Option<&InventoryValue>,
) -> Result<(), InventoryActionError> {
    match value {
        Some(InventoryValue::Public(value)) => fields.push(public(name, value)?),
        Some(InventoryValue::Redacted) => fields.push(redacted(name)?),
        None => fields.push(public(name, "")?),
    }
    Ok(())
}

fn object_prefix(
    object_type: &'static str,
    index: Option<usize>,
) -> Result<Vec<ManagerField>, InventoryActionError> {
    let mut fields = Vec::with_capacity(2);
    fields.push(public("ObjectType", object_type)?);
    if let Some(index) = index {
        fields.push(public("ObjectIndex", index)?);
    }
    Ok(fields)
}

fn ensure_list_bound(count: usize) -> Result<(), InventoryActionError> {
    if count > MAX_LIST_ITEMS {
        Err(InventoryActionError::TooManyItems)
    } else {
        Ok(())
    }
}

fn public(name: &'static str, value: impl ToString) -> Result<ManagerField, InventoryActionError> {
    ManagerField::public(name, value.to_string()).map_err(|_| InventoryActionError::InvalidOutput)
}

fn redacted(name: &'static str) -> Result<ManagerField, InventoryActionError> {
    ManagerField::redacted(name).map_err(|_| InventoryActionError::InvalidOutput)
}

fn bounded_success(fields: Vec<ManagerField>) -> Result<ManagerResponse, InventoryActionError> {
    if fields.len() > MAX_RESPONSE_FIELDS {
        return Err(InventoryActionError::ResponseTooLarge);
    }
    let mut total = 2usize + 14 + "Success".len() + 11 + "Inventory query complete".len();
    for field in &fields {
        let value_length = match field.public_value() {
            Some(value) if value.len() <= MAX_FIELD_VALUE_BYTES => value.len(),
            Some(_) => return Err(InventoryActionError::InvalidOutput),
            None => "<redacted>".len(),
        };
        total = total
            .checked_add(field.name().len())
            .and_then(|total| total.checked_add(value_length))
            .and_then(|total| total.checked_add(4))
            .ok_or(InventoryActionError::InvalidOutput)?;
    }
    if total > MAX_RESPONSE_BYTES {
        return Err(InventoryActionError::ResponseTooLarge);
    }
    Ok(ManagerResponse::success("Inventory query complete")
        .expect("fixed inventory success message is valid")
        .with_fields(fields))
}

const fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

const fn ring_name(mode: AppearanceRingMode) -> &'static str {
    match mode {
        AppearanceRingMode::Normal => "normal",
        AppearanceRingMode::Silent => "silent",
        AppearanceRingMode::Disabled => "disabled",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use sccp_protocol::{DeviceType, ProtocolVersion};

    use super::*;
    use crate::ami::manager::{ManagerRequestField, ManagerResponseKind};

    #[derive(Clone)]
    struct FakeProvider {
        snapshot: InventorySnapshot,
        error: Option<InventoryProviderError>,
    }

    impl InventoryProvider for FakeProvider {
        fn snapshot(&self) -> Result<InventorySnapshot, InventoryProviderError> {
            self.error.map_or_else(|| Ok(self.snapshot.clone()), Err)
        }
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

    fn response_values(response: &ManagerResponse, name: &str) -> Vec<Option<String>> {
        response
            .fields()
            .iter()
            .filter(|field| field.name() == name)
            .map(|field| field.public_value().map(str::to_owned))
            .collect()
    }

    fn parsed_inventory() -> InventorySnapshot {
        let config = ModuleConfig::parse(
            r#"
            [general]
            advertised_address = 192.0.2.10

            [1002]
            type = line
            label = Sales

            [1001]
            type = line
            label = Reception

            [SEP112233445566]
            type = device
            description = Second
            line = 1002

            [SEP001122334455]
            type = device
            description = First
            line = 1001,label=Private Desk,privacy=yes,subscription=secret-suffix
            button = speed_dial,Helpdesk,2000
            button = blf,Manager,2001,2001@internal
            button = feature,Do Not Disturb,DND,silent
            button = service,Directory,http://user:password@example.invalid/menu
            button = addon,1,7914
            button = unused
            "#,
        )
        .unwrap();
        let device = DeviceId::new("SEP001122334455").unwrap();
        let registrations = BTreeMap::from([(
            device,
            InventoryRegistration {
                model: format!("{:?}", DeviceType::Cisco7961),
                model_id: DeviceType::Cisco7961.wire_value(),
                protocol: ProtocolVersion::V17.to_string(),
                address: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 20)), 2000)
                    .to_string(),
            },
        )]);
        configured_inventory(&config, &registrations)
    }

    fn provider() -> FakeProvider {
        FakeProvider {
            snapshot: parsed_inventory(),
            error: None,
        }
    }

    #[test]
    fn normalized_runtime_inventory_is_sorted_and_never_exposes_service_urls() {
        let snapshot = parsed_inventory();
        assert_eq!(
            snapshot
                .devices
                .iter()
                .map(|device| device.id.as_str())
                .collect::<Vec<_>>(),
            ["SEP001122334455", "SEP112233445566"]
        );
        assert_eq!(
            snapshot
                .lines
                .iter()
                .map(|line| line.number.as_str())
                .collect::<Vec<_>>(),
            ["1001", "1002"]
        );
        let first_buttons = snapshot
            .buttons
            .iter()
            .filter(|button| button.device_id.as_str() == "SEP001122334455")
            .collect::<Vec<_>>();
        assert_eq!(
            first_buttons
                .iter()
                .map(|button| button.kind)
                .collect::<Vec<_>>(),
            [
                InventoryButtonKind::Line,
                InventoryButtonKind::SpeedDial,
                InventoryButtonKind::BlfSpeedDial,
                InventoryButtonKind::Feature,
                InventoryButtonKind::Service,
                InventoryButtonKind::AddonModule,
                InventoryButtonKind::Unused,
            ]
        );
        assert_eq!(first_buttons[4].target, Some(InventoryValue::Redacted));
        assert_eq!(
            first_buttons[2].hint,
            Some(InventoryValue::Public("2001@internal".into()))
        );
        assert_eq!(first_buttons[3].argument, Some(InventoryValue::Redacted));
        assert!(
            format!("{snapshot:?}").contains("Redacted")
                && !format!("{snapshot:?}").contains("password")
                && !format!("{snapshot:?}").contains("silent")
                && !format!("{snapshot:?}").contains("secret-suffix")
        );
    }

    #[test]
    fn device_and_line_lists_are_deterministic_typed_fields() {
        let provider = provider();
        let devices = handle_inventory_request(&provider, request(SHOW_DEVICES_ACTION, &[]));
        assert_eq!(devices.kind(), ManagerResponseKind::Success);
        assert_eq!(
            response_values(&devices, "DeviceId"),
            [
                Some("SEP001122334455".into()),
                Some("SEP112233445566".into()),
            ]
        );
        assert_eq!(
            response_values(&devices, "Registered")[0],
            Some("yes".into())
        );

        let lines = handle_inventory_request(&provider, request(SHOW_LINES_ACTION, &[]));
        assert_eq!(
            response_values(&lines, "LineName"),
            [Some("1001".into()), Some("1002".into())]
        );
        assert_eq!(
            response_values(&lines, "RegisteredAppearanceCount"),
            [Some("1".into()), Some("0".into())]
        );
    }

    #[test]
    fn detail_actions_select_exact_objects_and_preserve_redaction() {
        let provider = provider();
        let device = handle_inventory_request(
            &provider,
            request(SHOW_DEVICE_ACTION, &[("DeviceName", "sep001122334455")]),
        );
        assert_eq!(response_values(&device, "Protocol"), [Some("v17".into())]);

        let line = handle_inventory_request(
            &provider,
            request(SHOW_LINE_ACTION, &[("LineName", "1001")]),
        );
        assert_eq!(
            response_values(&line, "Context"),
            [Some("from-sccp".into())]
        );

        let appearance = handle_inventory_request(
            &provider,
            request(
                SHOW_APPEARANCE_ACTION,
                &[("DeviceName", "SEP001122334455"), ("LineInstance", "1")],
            ),
        );
        assert_eq!(
            response_values(&appearance, "Privacy"),
            [Some("yes".into())]
        );
        assert_eq!(response_values(&appearance, "Subscription"), [None]);

        let service = handle_inventory_request(
            &provider,
            request(
                SHOW_BUTTON_ACTION,
                &[("DeviceName", "SEP001122334455"), ("Position", "5")],
            ),
        );
        assert_eq!(response_values(&service, "Kind"), [Some("service".into())]);
        assert_eq!(response_values(&service, "Target"), [None]);

        let feature = handle_inventory_request(
            &provider,
            request(
                SHOW_BUTTON_ACTION,
                &[("DeviceName", "SEP001122334455"), ("Position", "4")],
            ),
        );
        assert_eq!(response_values(&feature, "Argument"), [None]);
    }

    #[test]
    fn filtered_appearance_and_button_lists_keep_configured_order() {
        let provider = provider();
        let appearances = handle_inventory_request(
            &provider,
            request(SHOW_APPEARANCES_ACTION, &[("LineName", "1002")]),
        );
        assert_eq!(
            response_values(&appearances, "LineName"),
            [Some("1002".into())]
        );

        let buttons = handle_inventory_request(
            &provider,
            request(SHOW_BUTTONS_ACTION, &[("DeviceName", "SEP001122334455")]),
        );
        assert_eq!(
            response_values(&buttons, "Position"),
            (1..=7)
                .map(|value| Some(value.to_string()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn provider_order_is_normalized_and_duplicate_identities_fail_closed() {
        let mut snapshot = parsed_inventory();
        snapshot.devices.reverse();
        snapshot.lines.reverse();
        snapshot.appearances.reverse();
        snapshot.buttons.reverse();
        let provider = FakeProvider {
            snapshot: snapshot.clone(),
            error: None,
        };
        let devices = handle_inventory_request(&provider, request(SHOW_DEVICES_ACTION, &[]));
        assert_eq!(
            response_values(&devices, "DeviceId"),
            [
                Some("SEP001122334455".into()),
                Some("SEP112233445566".into()),
            ]
        );
        let buttons = handle_inventory_request(
            &provider,
            request(SHOW_BUTTONS_ACTION, &[("DeviceName", "SEP001122334455")]),
        );
        assert_eq!(
            response_values(&buttons, "Position"),
            (1..=7)
                .map(|value| Some(value.to_string()))
                .collect::<Vec<_>>()
        );

        snapshot.devices.push(snapshot.devices[0].clone());
        let duplicate = handle_inventory_request(
            &FakeProvider {
                snapshot,
                error: None,
            },
            request(SHOW_DEVICES_ACTION, &[]),
        );
        assert_eq!(
            duplicate.message(),
            Some("Management inventory contains duplicate identities")
        );
    }

    #[test]
    fn unknown_duplicate_sensitive_and_malformed_fields_fail_without_values() {
        let provider = provider();
        let cases = [
            request(SHOW_DEVICES_ACTION, &[("Unexpected", "private-value")]),
            request(
                SHOW_DEVICE_ACTION,
                &[("DeviceName", "first"), ("DeviceName", "second")],
            ),
            request(SHOW_APPEARANCE_ACTION, &[("DeviceName", "bad")]),
            request(
                SHOW_BUTTON_ACTION,
                &[("DeviceName", "SEP001122334455"), ("Position", "0")],
            ),
        ];
        for request in cases {
            let response = handle_inventory_request(&provider, request);
            assert_eq!(response.kind(), ManagerResponseKind::Error);
            let message = response.message().unwrap();
            assert!(!message.contains("private-value"));
            assert!(!message.contains("first"));
            assert!(!message.contains("second"));
        }

        let mut sensitive = request(SHOW_DEVICES_ACTION, &[]);
        sensitive.fields.push(ManagerRequestField {
            name: "Authorization".into(),
            value: "do-not-disclose".into(),
            sensitive: true,
        });
        let response = handle_inventory_request(&provider, sensitive);
        assert_eq!(response.kind(), ManagerResponseKind::Error);
        assert!(!response.message().unwrap().contains("do-not-disclose"));
    }

    #[test]
    fn unknown_objects_provider_failure_and_list_bounds_are_distinct_and_safe() {
        let provider = provider();
        let missing = handle_inventory_request(
            &provider,
            request(SHOW_LINE_ACTION, &[("LineName", "9999")]),
        );
        assert_eq!(
            missing.message(),
            Some("Requested inventory object was not found")
        );

        let failed = handle_inventory_request(
            &FakeProvider {
                snapshot: InventorySnapshot::default(),
                error: Some(InventoryProviderError::Unavailable),
            },
            request(SHOW_DEVICES_ACTION, &[]),
        );
        assert_eq!(
            failed.message(),
            Some("Management inventory is unavailable")
        );

        let snapshot = InventorySnapshot {
            devices: (0..=MAX_LIST_ITEMS)
                .map(|index| InventoryDevice {
                    id: DeviceId::new(format!("SEP{index:012}")).unwrap(),
                    description: String::new(),
                    line_count: 0,
                    button_count: 0,
                    registration: None,
                })
                .collect(),
            ..InventorySnapshot::default()
        };
        let bounded = handle_inventory_request(
            &FakeProvider {
                snapshot,
                error: None,
            },
            request(SHOW_DEVICES_ACTION, &[]),
        );
        assert_eq!(
            bounded.message(),
            Some("Inventory result exceeds the bounded item limit")
        );
    }

    #[test]
    fn field_and_aggregate_response_byte_limits_fail_closed() {
        let device = |index, description: String| InventoryDevice {
            id: DeviceId::new(format!("SEP{index:012}")).unwrap(),
            description,
            line_count: 0,
            button_count: 0,
            registration: None,
        };
        let oversized_field = handle_inventory_request(
            &FakeProvider {
                snapshot: InventorySnapshot {
                    devices: vec![device(1, "x".repeat(MAX_FIELD_VALUE_BYTES + 1))],
                    ..InventorySnapshot::default()
                },
                error: None,
            },
            request(SHOW_DEVICES_ACTION, &[]),
        );
        assert_eq!(
            oversized_field.message(),
            Some("Inventory response cannot be represented safely")
        );

        let aggregate = handle_inventory_request(
            &FakeProvider {
                snapshot: InventorySnapshot {
                    devices: (0..20)
                        .map(|index| device(index, "x".repeat(MAX_FIELD_VALUE_BYTES)))
                        .collect(),
                    ..InventorySnapshot::default()
                },
                error: None,
            },
            request(SHOW_DEVICES_ACTION, &[]),
        );
        assert_eq!(
            aggregate.message(),
            Some("Inventory response exceeds the bounded size limit")
        );
    }

    #[test]
    fn action_names_and_response_fields_are_unique_allowlisted_contracts() {
        let names = InventoryAction::ALL
            .into_iter()
            .map(InventoryAction::name)
            .collect::<BTreeSet<_>>();
        assert_eq!(names.len(), InventoryAction::ALL.len());
        assert!(names.iter().all(|name| name.starts_with("SCCPShow")));
    }

    #[test]
    fn registration_is_unavailable_without_native_linkage() {
        #[cfg(feature = "development")]
        assert!(matches!(
            register_inventory_actions(provider(), crate::ami::manager::UnavailableManager),
            Err(ManagerError::Unavailable)
        ));
        let manager = crate::ami::manager::RollbackManager::fail_on(2);
        assert!(matches!(
            register_inventory_actions(provider(), manager.clone()),
            Err(ManagerError::RegistrationFailed)
        ));
        manager.assert_partial_rollback(3, 2);
    }
}
