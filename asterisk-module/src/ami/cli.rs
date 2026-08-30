//! Bounded, privacy-filtered native CLI inventory views.
//!
//! The parser and renderer are independent of the native CLI ABI. Callbacks
//! copy bounded arguments into Rust, obtain one immutable snapshot, and pass
//! both here. Lists and completion candidates use the same normalized order.

use std::fmt::{self, Write as _};

use sccp_protocol::{Codec, DeviceId};
use thiserror::Error;

use crate::ami::inventory::{
    InventoryAppearance, InventoryButton, InventoryButtonKind, InventoryDevice, InventoryLine,
    InventorySnapshot, InventoryValue,
};
use crate::pbx::query::channel::{ChannelDirectionSummary, ChannelStateSummary};
use crate::runtime::backend::PbxCallId;

pub const MAX_CLI_ARGUMENTS: usize = 3;
pub const MAX_CLI_ARGUMENT_BYTES: usize = 128;

const MAX_LIST_ITEMS: usize = 40;
const MAX_VALUE_BYTES: usize = 1024;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CliInventoryCommand {
    Devices,
    Lines,
    Channels,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CliCapability {
    pub codec: Codec,
    pub max_frames_per_packet: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CliCapabilityStatus {
    Unavailable,
    Pending,
    ReportedEmpty,
    Ready,
}

impl CliCapabilityStatus {
    const fn label(self) -> &'static str {
        match self {
            Self::Unavailable => "unavailable",
            Self::Pending => "pending",
            Self::ReportedEmpty => "reported empty",
            Self::Ready => "ready",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CliFeature {
    pub name: String,
    pub value: InventoryValue,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CliDeviceRuntime {
    pub device_id: DeviceId,
    pub capability_status: CliCapabilityStatus,
    pub capabilities: Vec<CliCapability>,
    pub features: Vec<CliFeature>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CliChannel {
    pub pbx_id: PbxCallId,
    pub call_id: Option<u64>,
    pub line: String,
    pub context: String,
    pub state: ChannelStateSummary,
    pub direction: ChannelDirectionSummary,
    pub dialed_number: InventoryValue,
    pub privacy: bool,
    pub appearance_count: usize,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CliInventorySnapshot {
    pub inventory: InventorySnapshot,
    pub device_runtime: Vec<CliDeviceRuntime>,
    pub channels: Vec<CliChannel>,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum CliInventoryError {
    #[error("invalid inventory selector")]
    InvalidSelector,
    #[error("requested inventory object was not found")]
    NotFound,
    #[error("inventory result exceeds the bounded item limit")]
    TooManyItems,
    #[error("inventory output exceeds the bounded size limit")]
    OutputTooLarge,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum CliInventoryRequest {
    DeviceList,
    DeviceDetail(DeviceId),
    DeviceAppearances(DeviceId, Option<AppearanceIdentity>),
    DeviceButtons(DeviceId, Option<usize>),
    DeviceCapabilities(DeviceId, Option<usize>),
    DeviceFeatures(DeviceId, Option<String>),
    LineList,
    LineDetail(String),
    LineAppearances(String, Option<AppearanceIdentity>),
    ChannelList,
    ChannelDetail(PbxCallId),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AppearanceIdentity {
    device_id: DeviceId,
    line_instance: u32,
}

pub fn render_cli_inventory(
    command: CliInventoryCommand,
    arguments: &[&str],
    snapshot: &CliInventorySnapshot,
) -> Result<String, CliInventoryError> {
    validate_arguments(arguments)?;
    let request = CliInventoryRequest::parse(command, arguments)?;
    let mut snapshot = snapshot.clone();
    snapshot.normalize();
    request.render(&snapshot)
}

pub fn complete_cli_inventory(
    command: CliInventoryCommand,
    arguments: &[&str],
    prefix: &str,
    ordinal: usize,
    snapshot: &CliInventorySnapshot,
) -> Option<String> {
    if validate_arguments(arguments).is_err()
        || prefix.len() > MAX_CLI_ARGUMENT_BYTES
        || prefix.chars().any(char::is_control)
    {
        return None;
    }
    let mut snapshot = snapshot.clone();
    snapshot.normalize();
    completion_candidates(command, arguments, &snapshot)
        .into_iter()
        .filter(|candidate| starts_with_ignore_ascii_case(candidate, prefix))
        .take(MAX_LIST_ITEMS)
        .nth(ordinal)
}

impl CliInventoryRequest {
    fn parse(command: CliInventoryCommand, arguments: &[&str]) -> Result<Self, CliInventoryError> {
        match command {
            CliInventoryCommand::Devices => Self::parse_devices(arguments),
            CliInventoryCommand::Lines => Self::parse_lines(arguments),
            CliInventoryCommand::Channels => Self::parse_channels(arguments),
        }
    }

    fn parse_devices(arguments: &[&str]) -> Result<Self, CliInventoryError> {
        let Some(device) = arguments.first() else {
            return Ok(Self::DeviceList);
        };
        let device = parse_device(device)?;
        match arguments.get(1).map(|value| value.to_ascii_lowercase()) {
            None => Ok(Self::DeviceDetail(device)),
            Some(section) if section == "appearances" => Ok(Self::DeviceAppearances(
                device,
                arguments
                    .get(2)
                    .map(|value| parse_appearance(value))
                    .transpose()?,
            )),
            Some(section) if section == "buttons" => Ok(Self::DeviceButtons(
                device,
                arguments
                    .get(2)
                    .map(|value| parse_positive(value))
                    .transpose()?,
            )),
            Some(section) if section == "capabilities" => Ok(Self::DeviceCapabilities(
                device,
                arguments
                    .get(2)
                    .map(|value| parse_positive(value))
                    .transpose()?,
            )),
            Some(section) if section == "features" => Ok(Self::DeviceFeatures(
                device,
                arguments.get(2).map(|value| (*value).to_owned()),
            )),
            Some(_) => Err(CliInventoryError::InvalidSelector),
        }
    }

    fn parse_lines(arguments: &[&str]) -> Result<Self, CliInventoryError> {
        let Some(line) = arguments.first() else {
            return Ok(Self::LineList);
        };
        validate_text_selector(line)?;
        let line = (*line).to_owned();
        match arguments.get(1).map(|value| value.to_ascii_lowercase()) {
            None => Ok(Self::LineDetail(line)),
            Some(section) if section == "appearances" => Ok(Self::LineAppearances(
                line,
                arguments
                    .get(2)
                    .map(|value| parse_appearance(value))
                    .transpose()?,
            )),
            Some(_) => Err(CliInventoryError::InvalidSelector),
        }
    }

    fn parse_channels(arguments: &[&str]) -> Result<Self, CliInventoryError> {
        match arguments {
            [] => Ok(Self::ChannelList),
            [pbx_id] => Ok(Self::ChannelDetail(PbxCallId(parse_positive::<u64>(
                pbx_id,
            )?))),
            _ => Err(CliInventoryError::InvalidSelector),
        }
    }

    fn render(self, snapshot: &CliInventorySnapshot) -> Result<String, CliInventoryError> {
        let mut output = CliOutput::default();
        match self {
            Self::DeviceList => render_device_list(&mut output, &snapshot.inventory.devices)?,
            Self::DeviceDetail(device) => {
                let item = find_device(snapshot, &device)?;
                let runtime = find_device_runtime(snapshot, &device);
                render_device(&mut output, item, runtime)?;
            }
            Self::DeviceAppearances(device, selector) => {
                find_device(snapshot, &device)?;
                let items = snapshot
                    .inventory
                    .appearances
                    .iter()
                    .filter(|item| item.device_id == device)
                    .filter(|item| {
                        selector
                            .as_ref()
                            .is_none_or(|selector| selector.matches(item))
                    })
                    .collect::<Vec<_>>();
                render_selected_appearances(&mut output, &items, selector.is_some())?;
            }
            Self::DeviceButtons(device, position) => {
                find_device(snapshot, &device)?;
                let items = snapshot
                    .inventory
                    .buttons
                    .iter()
                    .filter(|item| item.device_id == device)
                    .filter(|item| position.is_none_or(|position| item.position == position))
                    .collect::<Vec<_>>();
                render_selected_buttons(&mut output, &items, position.is_some())?;
            }
            Self::DeviceCapabilities(device, index) => {
                find_device(snapshot, &device)?;
                let runtime = find_device_runtime(snapshot, &device);
                let status = runtime.map_or(CliCapabilityStatus::Unavailable, |runtime| {
                    runtime.capability_status
                });
                let capabilities = runtime
                    .map(|runtime| runtime.capabilities.as_slice())
                    .unwrap_or_default();
                render_selected_capabilities(&mut output, status, capabilities, index)?;
            }
            Self::DeviceFeatures(device, name) => {
                find_device(snapshot, &device)?;
                let features = find_device_runtime(snapshot, &device)
                    .map(|runtime| runtime.features.as_slice())
                    .unwrap_or_default();
                render_selected_features(&mut output, features, name.as_deref())?;
            }
            Self::LineList => render_line_list(&mut output, &snapshot.inventory.lines)?,
            Self::LineDetail(line) => {
                let item = find_line(snapshot, &line)?;
                render_line(&mut output, item)?;
            }
            Self::LineAppearances(line, selector) => {
                find_line(snapshot, &line)?;
                let items = snapshot
                    .inventory
                    .appearances
                    .iter()
                    .filter(|item| item.line == line)
                    .filter(|item| {
                        selector
                            .as_ref()
                            .is_none_or(|selector| selector.matches(item))
                    })
                    .collect::<Vec<_>>();
                render_selected_appearances(&mut output, &items, selector.is_some())?;
            }
            Self::ChannelList => render_channel_list(&mut output, &snapshot.channels)?,
            Self::ChannelDetail(pbx_id) => {
                let item = snapshot
                    .channels
                    .iter()
                    .find(|item| item.pbx_id == pbx_id)
                    .ok_or(CliInventoryError::NotFound)?;
                render_channel(&mut output, item)?;
            }
        }
        output.finish()
    }
}

impl CliInventorySnapshot {
    fn normalize(&mut self) {
        self.inventory
            .devices
            .sort_by(|left, right| left.id.cmp(&right.id));
        self.inventory
            .lines
            .sort_by(|left, right| left.number.cmp(&right.number));
        self.inventory.appearances.sort_by(|left, right| {
            (&left.device_id, left.line_instance, left.appearance_id).cmp(&(
                &right.device_id,
                right.line_instance,
                right.appearance_id,
            ))
        });
        self.inventory.buttons.sort_by(|left, right| {
            (&left.device_id, left.position).cmp(&(&right.device_id, right.position))
        });
        self.device_runtime
            .sort_by(|left, right| left.device_id.cmp(&right.device_id));
        for runtime in &mut self.device_runtime {
            runtime.capabilities.sort_by_key(|capability| {
                (
                    capability.codec.wire_value(),
                    capability.max_frames_per_packet,
                )
            });
            runtime
                .features
                .sort_by(|left, right| left.name.cmp(&right.name));
        }
        self.channels.sort_by_key(|channel| channel.pbx_id.0);
    }
}

impl AppearanceIdentity {
    fn matches(&self, appearance: &InventoryAppearance) -> bool {
        self.device_id == appearance.device_id && self.line_instance == appearance.line_instance
    }

    fn render(&self) -> String {
        format!("{}:{}", self.device_id, self.line_instance)
    }
}

#[derive(Default)]
struct CliOutput {
    text: String,
}

impl CliOutput {
    fn line(&mut self, arguments: fmt::Arguments<'_>) -> Result<(), CliInventoryError> {
        self.text
            .write_fmt(arguments)
            .map_err(|_| CliInventoryError::OutputTooLarge)?;
        self.text.push('\n');
        if self.text.len() > MAX_OUTPUT_BYTES {
            return Err(CliInventoryError::OutputTooLarge);
        }
        Ok(())
    }

    fn field(&mut self, name: &str, value: impl fmt::Display) -> Result<(), CliInventoryError> {
        let value = clean_value(&value.to_string());
        self.line(format_args!("{name}: {value}"))
    }

    fn inventory_value(
        &mut self,
        name: &str,
        value: &InventoryValue,
    ) -> Result<(), CliInventoryError> {
        match value {
            InventoryValue::Public(value) => self.field(name, value),
            InventoryValue::Redacted => self.field(name, "<redacted>"),
        }
    }

    fn finish(self) -> Result<String, CliInventoryError> {
        (self.text.len() <= MAX_OUTPUT_BYTES)
            .then_some(self.text)
            .ok_or(CliInventoryError::OutputTooLarge)
    }
}

fn validate_arguments(arguments: &[&str]) -> Result<(), CliInventoryError> {
    if arguments.len() > MAX_CLI_ARGUMENTS
        || arguments.iter().any(|argument| {
            argument.is_empty()
                || argument.len() > MAX_CLI_ARGUMENT_BYTES
                || argument.chars().any(char::is_control)
        })
    {
        Err(CliInventoryError::InvalidSelector)
    } else {
        Ok(())
    }
}

fn validate_text_selector(value: &str) -> Result<(), CliInventoryError> {
    if value.len() > MAX_CLI_ARGUMENT_BYTES || value.chars().any(char::is_control) {
        Err(CliInventoryError::InvalidSelector)
    } else {
        Ok(())
    }
}

fn parse_device(value: &str) -> Result<DeviceId, CliInventoryError> {
    super::cli_support::parse_device(value, || CliInventoryError::InvalidSelector)
}

fn parse_positive<T>(value: &str) -> Result<T, CliInventoryError>
where
    T: std::str::FromStr + Default + PartialEq,
{
    super::cli_support::parse_positive(value, || CliInventoryError::InvalidSelector)
}

fn parse_appearance(value: &str) -> Result<AppearanceIdentity, CliInventoryError> {
    let (device, instance) = value
        .rsplit_once(':')
        .ok_or(CliInventoryError::InvalidSelector)?;
    Ok(AppearanceIdentity {
        device_id: parse_device(device)?,
        line_instance: parse_positive(instance)?,
    })
}

fn starts_with_ignore_ascii_case(value: &str, prefix: &str) -> bool {
    value
        .get(..prefix.len())
        .is_some_and(|candidate| candidate.eq_ignore_ascii_case(prefix))
}

fn completion_candidates(
    command: CliInventoryCommand,
    arguments: &[&str],
    snapshot: &CliInventorySnapshot,
) -> Vec<String> {
    match (command, arguments) {
        (CliInventoryCommand::Devices, []) => snapshot
            .inventory
            .devices
            .iter()
            .map(|device| device.id.to_string())
            .collect(),
        (CliInventoryCommand::Devices, [device]) if parse_device(device).is_ok() => {
            ["appearances", "buttons", "capabilities", "features"]
                .into_iter()
                .map(str::to_owned)
                .collect()
        }
        (CliInventoryCommand::Devices, [device, section]) => {
            nested_device_candidates(device, section, snapshot)
        }
        (CliInventoryCommand::Lines, []) => snapshot
            .inventory
            .lines
            .iter()
            .map(|line| line.number.clone())
            .collect(),
        (CliInventoryCommand::Lines, [line])
            if snapshot
                .inventory
                .lines
                .iter()
                .any(|item| item.number == *line) =>
        {
            vec!["appearances".to_owned()]
        }
        (CliInventoryCommand::Lines, [line, section])
            if section.eq_ignore_ascii_case("appearances") =>
        {
            appearance_candidates(
                snapshot
                    .inventory
                    .appearances
                    .iter()
                    .filter(|appearance| appearance.line == *line),
            )
        }
        (CliInventoryCommand::Channels, []) => snapshot
            .channels
            .iter()
            .map(|channel| channel.pbx_id.0.to_string())
            .collect(),
        _ => Vec::new(),
    }
}

fn nested_device_candidates(
    device: &str,
    section: &str,
    snapshot: &CliInventorySnapshot,
) -> Vec<String> {
    let Ok(device) = parse_device(device) else {
        return Vec::new();
    };
    if section.eq_ignore_ascii_case("appearances") {
        appearance_candidates(
            snapshot
                .inventory
                .appearances
                .iter()
                .filter(|appearance| appearance.device_id == device),
        )
    } else if section.eq_ignore_ascii_case("buttons") {
        snapshot
            .inventory
            .buttons
            .iter()
            .filter(|button| button.device_id == device)
            .map(|button| button.position.to_string())
            .collect()
    } else if section.eq_ignore_ascii_case("capabilities") {
        find_device_runtime(snapshot, &device)
            .map(|runtime| {
                (1..=runtime.capabilities.len())
                    .map(|index| index.to_string())
                    .collect()
            })
            .unwrap_or_default()
    } else if section.eq_ignore_ascii_case("features") {
        find_device_runtime(snapshot, &device)
            .map(|runtime| {
                runtime
                    .features
                    .iter()
                    .map(|item| item.name.clone())
                    .collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    }
}

fn appearance_candidates<'a>(
    appearances: impl Iterator<Item = &'a InventoryAppearance>,
) -> Vec<String> {
    appearances
        .map(|appearance| AppearanceIdentity {
            device_id: appearance.device_id.clone(),
            line_instance: appearance.line_instance,
        })
        .map(|identity| identity.render())
        .collect()
}

fn find_device<'a>(
    snapshot: &'a CliInventorySnapshot,
    device: &DeviceId,
) -> Result<&'a InventoryDevice, CliInventoryError> {
    snapshot
        .inventory
        .devices
        .iter()
        .find(|item| item.id == *device)
        .ok_or(CliInventoryError::NotFound)
}

fn find_device_runtime<'a>(
    snapshot: &'a CliInventorySnapshot,
    device: &DeviceId,
) -> Option<&'a CliDeviceRuntime> {
    snapshot
        .device_runtime
        .iter()
        .find(|item| item.device_id == *device)
}

fn find_line<'a>(
    snapshot: &'a CliInventorySnapshot,
    line: &str,
) -> Result<&'a InventoryLine, CliInventoryError> {
    snapshot
        .inventory
        .lines
        .iter()
        .find(|item| item.number == line)
        .ok_or(CliInventoryError::NotFound)
}

fn ensure_list_bound(count: usize) -> Result<(), CliInventoryError> {
    (count <= MAX_LIST_ITEMS)
        .then_some(())
        .ok_or(CliInventoryError::TooManyItems)
}

fn render_device_list(
    output: &mut CliOutput,
    items: &[InventoryDevice],
) -> Result<(), CliInventoryError> {
    ensure_list_bound(items.len())?;
    output.line(format_args!("Device\tState\tAddress\tLines\tButtons"))?;
    for item in items {
        let (state, address) = item
            .registration
            .as_ref()
            .map_or(("unavailable", "-"), |item| {
                ("registered", item.address.as_str())
            });
        output.line(format_args!(
            "{}\t{state}\t{}\t{}\t{}",
            clean_value(item.id.as_str()),
            clean_value(address),
            item.line_count,
            item.button_count,
        ))?;
    }
    Ok(())
}

fn render_device(
    output: &mut CliOutput,
    item: &InventoryDevice,
    runtime: Option<&CliDeviceRuntime>,
) -> Result<(), CliInventoryError> {
    output.field("Device", &item.id)?;
    output.field("Description", &item.description)?;
    output.field("Registered", item.registration.is_some())?;
    output.field("Lines", item.line_count)?;
    output.field("Buttons", item.button_count)?;
    output.field(
        "Capabilities",
        runtime.map_or(0, |runtime| runtime.capabilities.len()),
    )?;
    output.field(
        "Capability status",
        runtime.map_or("unavailable", |runtime| runtime.capability_status.label()),
    )?;
    output.field(
        "Features",
        runtime.map_or(0, |runtime| runtime.features.len()),
    )?;
    if let Some(registration) = &item.registration {
        output.field("Model", &registration.model)?;
        output.field("Model ID", registration.model_id)?;
        output.field("Protocol", &registration.protocol)?;
        output.field("Address", &registration.address)?;
    }
    Ok(())
}

fn render_line_list(
    output: &mut CliOutput,
    items: &[InventoryLine],
) -> Result<(), CliInventoryError> {
    ensure_list_bound(items.len())?;
    output.line(format_args!(
        "Line\tLabel\tContext\tAppearances\tRegistered"
    ))?;
    for item in items {
        output.line(format_args!(
            "{}\t{}\t{}\t{}\t{}",
            clean_value(&item.number),
            clean_value(&item.label),
            clean_value(&item.context),
            item.appearance_count,
            item.registered_appearance_count,
        ))?;
    }
    Ok(())
}

fn render_line(output: &mut CliOutput, item: &InventoryLine) -> Result<(), CliInventoryError> {
    output.field("Line", &item.number)?;
    output.field("Label", &item.label)?;
    output.field("Context", &item.context)?;
    output.field("Caller name", &item.caller_name)?;
    output.field("Caller number", &item.caller_number)?;
    output.field("Mailbox", item.mailbox.as_deref().unwrap_or(""))?;
    output.field("Appearances", item.appearance_count)?;
    output.field("Registered appearances", item.registered_appearance_count)
}

fn render_selected_appearances(
    output: &mut CliOutput,
    items: &[&InventoryAppearance],
    detail: bool,
) -> Result<(), CliInventoryError> {
    if detail {
        let item = only_item(items)?;
        return render_appearance(output, item);
    }
    ensure_list_bound(items.len())?;
    output.line(format_args!(
        "Appearance\tLine\tLabel\tRing\tPrivacy\tRegistered"
    ))?;
    for item in items {
        output.line(format_args!(
            "{}:{}\t{}\t{}\t{:?}\t{}\t{}",
            item.device_id,
            item.line_instance,
            clean_value(&item.line),
            clean_value(&item.label),
            item.ring,
            item.privacy,
            item.registered,
        ))?;
    }
    Ok(())
}

fn render_appearance(
    output: &mut CliOutput,
    item: &InventoryAppearance,
) -> Result<(), CliInventoryError> {
    output.field("Device", &item.device_id)?;
    output.field("Line instance", item.line_instance)?;
    output.field("Appearance ID", item.appearance_id)?;
    output.field("Line", &item.line)?;
    output.field("Label", &item.label)?;
    output.field("Ring", format_args!("{:?}", item.ring))?;
    output.field("Privacy", item.privacy)?;
    output.field("Registered", item.registered)?;
    if let Some(subscription) = &item.subscription {
        output.inventory_value("Subscription", subscription)?;
    }
    Ok(())
}

fn render_selected_buttons(
    output: &mut CliOutput,
    items: &[&InventoryButton],
    detail: bool,
) -> Result<(), CliInventoryError> {
    if detail {
        return render_button(output, only_item(items)?);
    }
    ensure_list_bound(items.len())?;
    output.line(format_args!("Position\tKind\tInstance\tLabel\tTarget"))?;
    for item in items {
        output.line(format_args!(
            "{}\t{}\t{}\t{}\t{}",
            item.position,
            button_kind_name(item.kind),
            item.instance
                .map_or_else(String::new, |value| value.to_string()),
            clean_value(&item.label),
            display_inventory_value(item.target.as_ref()),
        ))?;
    }
    Ok(())
}

fn render_button(output: &mut CliOutput, item: &InventoryButton) -> Result<(), CliInventoryError> {
    output.field("Device", &item.device_id)?;
    output.field("Position", item.position)?;
    output.field("Kind", button_kind_name(item.kind))?;
    output.field(
        "Instance",
        item.instance
            .map_or_else(String::new, |value| value.to_string()),
    )?;
    output.field("Label", &item.label)?;
    if let Some(value) = &item.target {
        output.inventory_value("Target", value)?;
    }
    if let Some(value) = &item.hint {
        output.inventory_value("Hint", value)?;
    }
    if let Some(value) = &item.argument {
        output.inventory_value("Argument", value)?;
    }
    Ok(())
}

fn render_selected_capabilities(
    output: &mut CliOutput,
    status: CliCapabilityStatus,
    capabilities: &[CliCapability],
    index: Option<usize>,
) -> Result<(), CliInventoryError> {
    output.field("Status", status.label())?;
    if let Some(index) = index {
        let item = capabilities
            .get(index - 1)
            .ok_or(CliInventoryError::NotFound)?;
        output.field("Position", index)?;
        output.field("Codec", format_args!("{:?}", item.codec))?;
        output.field("Codec ID", item.codec.wire_value())?;
        return output.field("Max frames per packet", item.max_frames_per_packet);
    }
    ensure_list_bound(capabilities.len())?;
    output.line(format_args!(
        "Position\tCodec\tCodec ID\tMax frames per packet"
    ))?;
    for (index, item) in capabilities.iter().enumerate() {
        output.line(format_args!(
            "{}\t{:?}\t{}\t{}",
            index + 1,
            item.codec,
            item.codec.wire_value(),
            item.max_frames_per_packet,
        ))?;
    }
    Ok(())
}

fn render_selected_features(
    output: &mut CliOutput,
    features: &[CliFeature],
    name: Option<&str>,
) -> Result<(), CliInventoryError> {
    if let Some(name) = name {
        let item = features
            .iter()
            .find(|item| item.name.eq_ignore_ascii_case(name))
            .ok_or(CliInventoryError::NotFound)?;
        output.field("Feature", &item.name)?;
        return output.inventory_value("Value", &item.value);
    }
    ensure_list_bound(features.len())?;
    output.line(format_args!("Feature\tValue"))?;
    for item in features {
        output.line(format_args!(
            "{}\t{}",
            clean_value(&item.name),
            display_inventory_value(Some(&item.value)),
        ))?;
    }
    Ok(())
}

fn render_channel_list(
    output: &mut CliOutput,
    channels: &[CliChannel],
) -> Result<(), CliInventoryError> {
    ensure_list_bound(channels.len())?;
    output.line(format_args!(
        "PBX ID\tCall ID\tLine\tState\tDirection\tAppearances"
    ))?;
    for item in channels {
        output.line(format_args!(
            "{}\t{}\t{}\t{}\t{}\t{}",
            item.pbx_id.0,
            item.call_id
                .map_or_else(String::new, |value| value.to_string()),
            clean_value(&item.line),
            item.state,
            item.direction,
            item.appearance_count,
        ))?;
    }
    Ok(())
}

fn render_channel(output: &mut CliOutput, item: &CliChannel) -> Result<(), CliInventoryError> {
    output.field("PBX ID", item.pbx_id.0)?;
    output.field(
        "Call ID",
        item.call_id
            .map_or_else(String::new, |value| value.to_string()),
    )?;
    output.field("Line", &item.line)?;
    output.field("Context", &item.context)?;
    output.field("State", item.state)?;
    output.field("Direction", item.direction)?;
    if item.privacy {
        output.field("Dialed number", "<redacted>")?;
    } else {
        output.inventory_value("Dialed number", &item.dialed_number)?;
    }
    output.field("Privacy", item.privacy)?;
    output.field("Appearances", item.appearance_count)
}

fn only_item<'a, T>(items: &[&'a T]) -> Result<&'a T, CliInventoryError> {
    match items {
        [item] => Ok(*item),
        _ => Err(CliInventoryError::NotFound),
    }
}

fn display_inventory_value(value: Option<&InventoryValue>) -> String {
    match value {
        Some(InventoryValue::Public(value)) => clean_value(value),
        Some(InventoryValue::Redacted) => "<redacted>".to_owned(),
        None => String::new(),
    }
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

const fn button_kind_name(kind: InventoryButtonKind) -> &'static str {
    match kind {
        InventoryButtonKind::Line => "line",
        InventoryButtonKind::SpeedDial => "speed-dial",
        InventoryButtonKind::BlfSpeedDial => "blf-speed-dial",
        InventoryButtonKind::Feature => "feature",
        InventoryButtonKind::Service => "service",
        InventoryButtonKind::AddonModule => "addon-module",
        InventoryButtonKind::Unused => "unused",
    }
}

#[cfg(test)]
mod tests {
    use sccp_protocol::{AppearanceRingMode, DeviceType, ProtocolVersion};

    use super::*;
    use crate::ami::inventory::InventoryRegistration;

    fn device(value: &str) -> DeviceId {
        DeviceId::new(value).unwrap()
    }

    fn snapshot() -> CliInventorySnapshot {
        let first = device("SEP001122334455");
        let second = device("SEP112233445566");
        CliInventorySnapshot {
            inventory: InventorySnapshot {
                devices: vec![
                    InventoryDevice {
                        id: second,
                        description: "Second".into(),
                        line_count: 1,
                        button_count: 0,
                        registration: None,
                    },
                    InventoryDevice {
                        id: first.clone(),
                        description: "First".into(),
                        line_count: 1,
                        button_count: 2,
                        registration: Some(InventoryRegistration {
                            model: format!("{:?}", DeviceType::Cisco7961),
                            model_id: DeviceType::Cisco7961.wire_value(),
                            protocol: ProtocolVersion::new(22).unwrap().to_string(),
                            address: "192.0.2.10:2000".into(),
                        }),
                    },
                ],
                lines: vec![InventoryLine {
                    number: "1000".into(),
                    label: "Private\nLine".into(),
                    context: "default".into(),
                    caller_name: "Alice".into(),
                    caller_number: "1000".into(),
                    mailbox: Some("1000".into()),
                    appearance_count: 1,
                    registered_appearance_count: 1,
                }],
                appearances: vec![InventoryAppearance {
                    device_id: first.clone(),
                    line_instance: 1,
                    appearance_id: 7,
                    line: "1000".into(),
                    label: "Private".into(),
                    ring: AppearanceRingMode::Silent,
                    privacy: true,
                    subscription: Some(InventoryValue::Redacted),
                    registered: true,
                }],
                buttons: vec![InventoryButton {
                    device_id: first.clone(),
                    position: 2,
                    kind: InventoryButtonKind::Service,
                    instance: Some(1),
                    label: "Directory".into(),
                    target: Some(InventoryValue::Redacted),
                    hint: None,
                    argument: Some(InventoryValue::Redacted),
                }],
            },
            device_runtime: vec![CliDeviceRuntime {
                device_id: first,
                capability_status: CliCapabilityStatus::Ready,
                capabilities: vec![
                    CliCapability {
                        codec: Codec::G729,
                        max_frames_per_packet: 2,
                    },
                    CliCapability {
                        codec: Codec::Pcmu,
                        max_frames_per_packet: 1,
                    },
                ],
                features: vec![
                    CliFeature {
                        name: "privacy".into(),
                        value: InventoryValue::Public("on".into()),
                    },
                    CliFeature {
                        name: "forward-all".into(),
                        value: InventoryValue::Redacted,
                    },
                ],
            }],
            channels: vec![CliChannel {
                pbx_id: PbxCallId(9),
                call_id: Some(44),
                line: "1000".into(),
                context: "default".into(),
                state: ChannelStateSummary::Connected,
                direction: ChannelDirectionSummary::Outbound,
                dialed_number: InventoryValue::Public("5551212".into()),
                privacy: true,
                appearance_count: 1,
            }],
        }
    }

    #[test]
    fn list_and_detail_views_are_deterministic_and_privacy_filtered() {
        let snapshot = snapshot();
        let devices = render_cli_inventory(CliInventoryCommand::Devices, &[], &snapshot).unwrap();
        assert!(
            devices.find("SEP001122334455").unwrap() < devices.find("SEP112233445566").unwrap()
        );

        let device_detail = render_cli_inventory(
            CliInventoryCommand::Devices,
            &["SEP001122334455"],
            &snapshot,
        )
        .unwrap();
        assert_eq!(
            device_detail.matches("Device: SEP001122334455\n").count(),
            1
        );
        assert_eq!(device_detail.matches("Description: First\n").count(), 1);
        assert!(device_detail.contains("Capability status: ready"));

        let button = render_cli_inventory(
            CliInventoryCommand::Devices,
            &["SEP001122334455", "buttons", "2"],
            &snapshot,
        )
        .unwrap();
        assert!(button.contains("Target: <redacted>"));
        assert!(button.contains("Argument: <redacted>"));

        let appearance = render_cli_inventory(
            CliInventoryCommand::Lines,
            &["1000", "appearances", "SEP001122334455:1"],
            &snapshot,
        )
        .unwrap();
        assert!(appearance.contains("Subscription: <redacted>"));

        let channel =
            render_cli_inventory(CliInventoryCommand::Channels, &["9"], &snapshot).unwrap();
        assert!(channel.contains("Dialed number: <redacted>"));
        assert!(!channel.contains("5551212"));
    }

    #[test]
    fn nested_lists_and_details_share_stable_selectors() {
        let snapshot = snapshot();
        let capabilities = render_cli_inventory(
            CliInventoryCommand::Devices,
            &["SEP001122334455", "capabilities"],
            &snapshot,
        )
        .unwrap();
        assert!(capabilities.contains("Status: ready"));
        assert!(capabilities.contains("1\tPcmu"));
        assert!(capabilities.contains("2\tG729"));

        let detail = render_cli_inventory(
            CliInventoryCommand::Devices,
            &["SEP001122334455", "capabilities", "2"],
            &snapshot,
        )
        .unwrap();
        assert!(detail.contains("Codec: G729"));

        let feature = render_cli_inventory(
            CliInventoryCommand::Devices,
            &["SEP001122334455", "features", "forward-all"],
            &snapshot,
        )
        .unwrap();
        assert!(feature.contains("Value: <redacted>"));
    }

    #[test]
    fn capability_views_distinguish_unavailable_pending_empty_and_ready_states() {
        for (status, expected) in [
            (CliCapabilityStatus::Unavailable, "unavailable"),
            (CliCapabilityStatus::Pending, "pending"),
            (CliCapabilityStatus::ReportedEmpty, "reported empty"),
            (CliCapabilityStatus::Ready, "ready"),
        ] {
            let mut snapshot = snapshot();
            let runtime = snapshot.device_runtime.first_mut().unwrap();
            runtime.capability_status = status;
            if status != CliCapabilityStatus::Ready {
                runtime.capabilities.clear();
            }
            let capabilities = render_cli_inventory(
                CliInventoryCommand::Devices,
                &["SEP001122334455", "capabilities"],
                &snapshot,
            )
            .unwrap();
            assert!(capabilities.contains(&format!("Status: {expected}")));
        }
    }

    #[test]
    fn completion_uses_normalized_ids_sections_and_nested_selectors() {
        let snapshot = snapshot();
        assert_eq!(
            complete_cli_inventory(CliInventoryCommand::Devices, &[], "SEP", 0, &snapshot),
            Some("SEP001122334455".into())
        );
        assert_eq!(
            complete_cli_inventory(
                CliInventoryCommand::Devices,
                &["SEP001122334455"],
                "c",
                0,
                &snapshot,
            ),
            Some("capabilities".into())
        );
        assert_eq!(
            complete_cli_inventory(
                CliInventoryCommand::Devices,
                &["SEP001122334455", "capabilities"],
                "",
                1,
                &snapshot,
            ),
            Some("2".into())
        );
        assert_eq!(
            complete_cli_inventory(
                CliInventoryCommand::Lines,
                &["1000", "appearances"],
                "SEP",
                0,
                &snapshot,
            ),
            Some("SEP001122334455:1".into())
        );
    }

    #[test]
    fn completion_applies_its_bound_after_prefix_filtering() {
        let mut snapshot = snapshot();
        snapshot.inventory.devices = (0..MAX_LIST_ITEMS)
            .map(|index| InventoryDevice {
                id: device(&format!("SEP{index:012}")),
                description: String::new(),
                line_count: 0,
                button_count: 0,
                registration: None,
            })
            .chain(std::iter::once(InventoryDevice {
                id: device("SEP999999999999"),
                description: String::new(),
                line_count: 0,
                button_count: 0,
                registration: None,
            }))
            .collect();

        assert_eq!(
            complete_cli_inventory(CliInventoryCommand::Devices, &[], "SEP999", 0, &snapshot,),
            Some("SEP999999999999".into())
        );
    }

    #[test]
    fn malformed_and_over_limit_requests_fail_closed() {
        let snapshot = snapshot();
        assert_eq!(
            render_cli_inventory(
                CliInventoryCommand::Devices,
                &["SEP001122334455", "secrets"],
                &snapshot,
            ),
            Err(CliInventoryError::InvalidSelector)
        );
        assert_eq!(
            render_cli_inventory(CliInventoryCommand::Channels, &["0"], &snapshot),
            Err(CliInventoryError::InvalidSelector)
        );

        let mut oversized = snapshot;
        oversized.inventory.devices = (0..=MAX_LIST_ITEMS)
            .map(|index| InventoryDevice {
                id: device(&format!("SEP{index:012}")),
                description: String::new(),
                line_count: 0,
                button_count: 0,
                registration: None,
            })
            .collect();
        assert_eq!(
            render_cli_inventory(CliInventoryCommand::Devices, &[], &oversized),
            Err(CliInventoryError::TooManyItems)
        );
    }
}
