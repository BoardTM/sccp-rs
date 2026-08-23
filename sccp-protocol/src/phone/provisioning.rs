//! Bounded station provisioning documents.
//!
//! Phone-service XML and boot configuration XML are distinct protocols. This
//! module models the bootable `device` and `Default` roots without accepting
//! arbitrary XML maps, while retaining open string values where firmware
//! vocabularies vary by model.

use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use super::xml::{PhoneXmlError, from_bytes, to_string, to_writer as write_document};

/// Maximum encoded size of a provisioning XML document, in bytes.
pub const PROVISIONING_MAX_BYTES: usize = 128 * 1_024;
/// Maximum number of prioritized signaling servers in one device pool.
pub const PROVISIONING_MAX_CALL_MANAGERS: usize = 5;
/// Maximum number of time servers in one device pool.
pub const PROVISIONING_MAX_NTP_SERVERS: usize = 5;
/// Maximum number of model-specific firmware selections in one document.
pub const PROVISIONING_MAX_FIRMWARE_LOADS: usize = 8;
const PROVISIONING_MAX_TEXT_CHARS: usize = 256;

/// Secret provisioning material whose diagnostics never reveal its value.
#[derive(Clone, Eq, PartialEq, Deserialize, Serialize)]
#[serde(transparent)]
pub struct ProvisioningSecret(String);

impl ProvisioningSecret {
    /// Validates and wraps non-empty secret material for provisioning output.
    pub fn new(value: impl Into<String>) -> Result<Self, PhoneXmlError> {
        let value = value.into();
        validate_text(
            "provisioning secret",
            &value,
            1,
            PROVISIONING_MAX_TEXT_CHARS,
        )?;
        Ok(Self(value))
    }

    /// Exposes the value only to code that must consume or transmit the secret.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ProvisioningSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ProvisioningSecret(<redacted>)")
    }
}

impl TryFrom<String> for ProvisioningSecret {
    type Error = PhoneXmlError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

/// Signaling protocol selected by a boot configuration.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
pub enum ProvisioningProtocol {
    #[default]
    #[serde(rename = "SCCP")]
    Sccp,
}

/// Transport required for the signaling connection.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
pub enum ProvisioningTransport {
    #[default]
    #[serde(rename = "TCP")]
    Clear,
    #[serde(rename = "TLS")]
    Tls,
}

/// Numeric boolean representation used by provisioning XML.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
pub enum ProvisioningBoolean {
    /// Serializes as `0`; also the default when a value is omitted.
    #[default]
    #[serde(rename = "0")]
    Disabled,
    /// Serializes as `1`.
    #[serde(rename = "1")]
    Enabled,
}

/// IPv4, IPv6, or DNS endpoint used by call-control and NTP entries.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProvisioningHost(String);

impl ProvisioningHost {
    /// Validates an IP address or bounded DNS hostname.
    pub fn new(value: impl Into<String>) -> Result<Self, PhoneXmlError> {
        let value = value.into();
        validate_host(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for ProvisioningHost {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl TryFrom<String> for ProvisioningHost {
    type Error = PhoneXmlError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl FromStr for ProvisioningHost {
    type Err = PhoneXmlError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for ProvisioningHost {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ProvisioningHost {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// Clear and optional secure signaling ports for one server.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProvisioningPorts {
    #[serde(rename = "ethernetPhonePort")]
    /// Required nonzero clear-signaling port.
    pub signaling: u16,
    #[serde(
        rename = "securedEthernetPhonePort",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    /// Nonzero TLS port; required when [`ProvisioningTransport::Tls`] is selected.
    pub secure_signaling: Option<u16>,
}

/// Host and transport endpoints for one signaling server.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProvisioningCallManager {
    #[serde(rename = "processNodeName")]
    pub host: ProvisioningHost,
    pub ports: ProvisioningPorts,
}

/// One server plus its unique failover priority.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProvisioningCallManagerMember {
    #[serde(rename = "@priority")]
    /// Unique priority within the containing server group.
    pub priority: u8,
    #[serde(rename = "callManager")]
    pub call_manager: ProvisioningCallManager,
}

/// XML wrapper around a bounded list of prioritized servers.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProvisioningCallManagerMembers {
    #[serde(rename = "member", default)]
    pub entries: Vec<ProvisioningCallManagerMember>,
}

/// Server failover group assigned to a device pool.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProvisioningCallManagerGroup {
    pub members: ProvisioningCallManagerMembers,
}

/// One network time source and its query mode.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProvisioningNtpServer {
    pub name: ProvisioningHost,
    #[serde(rename = "ntpMode", default)]
    pub mode: ProvisioningNtpMode,
}

/// Request mode used for a configured network time source.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
pub enum ProvisioningNtpMode {
    #[default]
    Unicast,
    DirectedBroadcast,
}

/// XML wrapper around a bounded list of network time sources.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProvisioningNtpServers {
    #[serde(rename = "ntp", default)]
    pub entries: Vec<ProvisioningNtpServer>,
}

/// Display date/time policy and network time sources for a device pool.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProvisioningDateTime {
    #[serde(rename = "dateTemplate")]
    /// Display-format token validated to at most 16 characters.
    pub date_template: String,
    #[serde(rename = "timeZone")]
    /// Time-zone identifier validated to at most 64 characters.
    pub time_zone: String,
    pub ntps: ProvisioningNtpServers,
}

/// Shared locale, time, and signaling-server assignment.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProvisioningDevicePool {
    #[serde(rename = "dateTimeSetting")]
    pub date_time: ProvisioningDateTime,
    #[serde(rename = "callManagerGroup")]
    pub call_managers: ProvisioningCallManagerGroup,
}

/// Optional user and network locale names and package versions.
#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProvisioningLocales {
    #[serde(
        rename = "userLocale",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub user: Option<String>,
    #[serde(
        rename = "networkLocale",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub network: Option<String>,
    #[serde(
        rename = "userLocaleVersion",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub user_version: Option<String>,
    #[serde(
        rename = "networkLocaleVersion",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub network_version: Option<String>,
}

/// Optional HTTP endpoints exposed by the phone's service menu and actions.
#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProvisioningServiceUrls {
    #[serde(
        rename = "authenticationURL",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub authentication: Option<String>,
    #[serde(
        rename = "directoryURL",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub directory: Option<String>,
    #[serde(rename = "idleURL", default, skip_serializing_if = "Option::is_none")]
    pub idle: Option<String>,
    #[serde(
        rename = "informationURL",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub information: Option<String>,
    #[serde(
        rename = "messagesURL",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub messages: Option<String>,
    #[serde(
        rename = "proxyServerURL",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub proxy: Option<String>,
    #[serde(
        rename = "servicesURL",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub services: Option<String>,
}

/// Optional six-bit DSCP values for signaling and media traffic classes.
#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProvisioningDscp {
    #[serde(
        rename = "dscpForCallControl",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    /// Signaling DSCP in the inclusive range `0..=63`.
    pub signaling: Option<u8>,
    #[serde(
        rename = "dscpForAudio",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    /// Audio DSCP in the inclusive range `0..=63`.
    pub audio: Option<u8>,
    #[serde(
        rename = "dscpForVideo",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    /// Video DSCP in the inclusive range `0..=63`.
    pub video: Option<u8>,
}

/// Optional feature switches; omitted entries defer to the device default.
#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProvisioningFeatures {
    #[serde(
        rename = "enblocDialing",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub enbloc_dialing: Option<ProvisioningBoolean>,
    #[serde(
        rename = "dndControl",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub do_not_disturb: Option<ProvisioningBoolean>,
    #[serde(
        rename = "joinAcrossLines",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub join_across_lines: Option<ProvisioningBoolean>,
    #[serde(
        rename = "callPickup",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub call_pickup: Option<ProvisioningBoolean>,
    #[serde(rename = "barge", default, skip_serializing_if = "Option::is_none")]
    pub barge: Option<ProvisioningBoolean>,
    #[serde(rename = "blf", default, skip_serializing_if = "Option::is_none")]
    pub blf: Option<ProvisioningBoolean>,
    #[serde(rename = "mwi", default, skip_serializing_if = "Option::is_none")]
    pub mwi: Option<ProvisioningBoolean>,
    #[serde(rename = "mobility", default, skip_serializing_if = "Option::is_none")]
    pub mobility: Option<ProvisioningBoolean>,
}

/// Preferred codec name and optional per-codec enablement switches.
#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProvisioningCodecPolicy {
    #[serde(
        rename = "preferredCodec",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub preferred: Option<String>,
    #[serde(
        rename = "g722CodecSupport",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub g722: Option<ProvisioningBoolean>,
    #[serde(
        rename = "g729CodecSupport",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub g729: Option<ProvisioningBoolean>,
}

/// Certificate-enrollment endpoint and optional authentication material.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProvisioningCapf {
    #[serde(rename = "phonePort")]
    /// Nonzero enrollment-service port.
    pub phone_port: u16,
    #[serde(rename = "processNodeName")]
    pub host: ProvisioningHost,
    #[serde(
        rename = "authenticationMode",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub authentication_mode: Option<String>,
    #[serde(
        rename = "authenticationToken",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    /// Optional secret token, redacted by its [`Debug`](std::fmt::Debug) implementation.
    pub authentication_token: Option<ProvisioningSecret>,
    #[serde(
        rename = "certificateOperation",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub certificate_operation: Option<String>,
}

/// Signaling transport and optional certificate/trust configuration.
#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProvisioningSecurity {
    #[serde(rename = "transportLayerProtocol", default)]
    /// Selects the transport and determines whether every server needs a secure port.
    pub transport: ProvisioningTransport,
    #[serde(
        rename = "deviceSecurityMode",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub device_security_mode: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Optional enrollment configuration; its port must be nonzero.
    pub capf: Option<ProvisioningCapf>,
    #[serde(
        rename = "encryptedConfig",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    /// Opaque protected configuration material omitted when unavailable.
    pub encrypted_config: Option<ProvisioningSecret>,
    #[serde(rename = "trustList", default, skip_serializing_if = "Option::is_none")]
    /// Opaque trust material omitted when unavailable.
    pub trust_list: Option<ProvisioningSecret>,
}

/// Firmware filename, optionally restricted to one model identifier.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProvisioningFirmwareLoad {
    #[serde(rename = "@model", default, skip_serializing_if = "Option::is_none")]
    /// Optional model selector; duplicate selectors are rejected by validation.
    pub model: Option<String>,
    #[serde(rename = "$text")]
    pub file: String,
}

/// Device-specific quality-of-service, codec, and feature settings.
#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProvisioningVendorConfig {
    #[serde(default)]
    pub dscp: ProvisioningDscp,
    #[serde(default)]
    pub codecs: ProvisioningCodecPolicy,
    #[serde(default)]
    pub features: ProvisioningFeatures,
}

/// Complete per-device boot configuration rooted at the `device` XML element.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename = "device", deny_unknown_fields)]
pub struct DeviceProvisioning {
    #[serde(rename = "deviceProtocol", default)]
    pub protocol: ProvisioningProtocol,
    #[serde(rename = "devicePool")]
    pub device_pool: ProvisioningDevicePool,
    #[serde(default)]
    pub locales: ProvisioningLocales,
    #[serde(rename = "phoneServices", default)]
    pub service_urls: ProvisioningServiceUrls,
    #[serde(rename = "securityProfile", default)]
    pub security: ProvisioningSecurity,
    #[serde(rename = "vendorConfig", default)]
    pub vendor: ProvisioningVendorConfig,
    #[serde(rename = "loadInformation", default)]
    /// Model-specific firmware entries, bounded by [`PROVISIONING_MAX_FIRMWARE_LOADS`].
    pub firmware_loads: Vec<ProvisioningFirmwareLoad>,
    #[serde(rename = "sshUserId", default, skip_serializing_if = "Option::is_none")]
    /// Optional remote-access user identifier; never treated as secret diagnostics.
    pub ssh_user_id: Option<String>,
    #[serde(
        rename = "sshPassword",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    /// Optional remote-access password with redacted diagnostics.
    pub ssh_password: Option<ProvisioningSecret>,
}

impl DeviceProvisioning {
    /// Parses, bounds, and validates a complete per-device XML document.
    pub fn from_xml(document: &[u8]) -> Result<Self, PhoneXmlError> {
        let value: Self = from_bytes(document, PROVISIONING_MAX_BYTES)?;
        value.validate()?;
        Ok(value)
    }

    /// Validates and serializes the document within [`PROVISIONING_MAX_BYTES`].
    pub fn to_xml(&self) -> Result<String, PhoneXmlError> {
        self.validate()?;
        to_string(self, PROVISIONING_MAX_BYTES)
    }

    /// Validates and writes the document to a formatting sink.
    pub fn write_xml(&self, writer: impl fmt::Write) -> Result<(), PhoneXmlError> {
        self.validate()?;
        write_document(writer, self, PROVISIONING_MAX_BYTES)
    }

    /// Checks collection bounds, endpoint consistency, and all textual invariants.
    pub fn validate(&self) -> Result<(), PhoneXmlError> {
        validate_device_pool(&self.device_pool)?;
        validate_transport_endpoints(&self.device_pool, self.security.transport)?;
        validate_locales(&self.locales)?;
        validate_service_urls(&self.service_urls)?;
        validate_dscp(&self.vendor.dscp)?;
        validate_codec_policy(&self.vendor.codecs)?;
        validate_security(&self.security)?;
        validate_count(
            "firmware load entries",
            self.firmware_loads.len(),
            PROVISIONING_MAX_FIRMWARE_LOADS,
        )?;
        validate_firmware_loads(&self.firmware_loads)?;
        validate_optional_text("SSH user ID", self.ssh_user_id.as_deref(), 1, 64)
    }
}

fn validate_transport_endpoints(
    pool: &ProvisioningDevicePool,
    transport: ProvisioningTransport,
) -> Result<(), PhoneXmlError> {
    if transport == ProvisioningTransport::Tls
        && !pool
            .call_managers
            .members
            .entries
            .iter()
            .all(|member| member.call_manager.ports.secure_signaling.is_some())
    {
        return Err(PhoneXmlError::InvalidField {
            field: "secure signaling endpoint",
            expected: "a secure signaling port for every configured call manager",
        });
    }
    Ok(())
}

/// Shared fallback configuration rooted at the `Default` XML element.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename = "Default", deny_unknown_fields)]
pub struct DefaultProvisioning {
    #[serde(rename = "devicePool")]
    pub device_pool: ProvisioningDevicePool,
    #[serde(rename = "loadInformation", default)]
    pub firmware_loads: Vec<ProvisioningFirmwareLoad>,
}

impl DefaultProvisioning {
    /// Parses, bounds, and validates a complete fallback XML document.
    pub fn from_xml(document: &[u8]) -> Result<Self, PhoneXmlError> {
        let value: Self = from_bytes(document, PROVISIONING_MAX_BYTES)?;
        value.validate()?;
        Ok(value)
    }

    /// Validates and serializes the document within [`PROVISIONING_MAX_BYTES`].
    pub fn to_xml(&self) -> Result<String, PhoneXmlError> {
        self.validate()?;
        to_string(self, PROVISIONING_MAX_BYTES)
    }

    /// Validates and writes the document to a formatting sink.
    pub fn write_xml(&self, writer: impl fmt::Write) -> Result<(), PhoneXmlError> {
        self.validate()?;
        write_document(writer, self, PROVISIONING_MAX_BYTES)
    }

    /// Checks device-pool and firmware-list invariants.
    pub fn validate(&self) -> Result<(), PhoneXmlError> {
        validate_device_pool(&self.device_pool)?;
        validate_firmware_loads(&self.firmware_loads)
    }
}

fn validate_device_pool(pool: &ProvisioningDevicePool) -> Result<(), PhoneXmlError> {
    validate_text("date template", &pool.date_time.date_template, 1, 16)?;
    validate_text("time zone", &pool.date_time.time_zone, 1, 64)?;
    validate_count(
        "NTP servers",
        pool.date_time.ntps.entries.len(),
        PROVISIONING_MAX_NTP_SERVERS,
    )?;
    let members = &pool.call_managers.members.entries;
    if members.is_empty() {
        return Err(PhoneXmlError::InvalidField {
            field: "call-manager group",
            expected: "at least one server",
        });
    }
    validate_count(
        "call-manager servers",
        members.len(),
        PROVISIONING_MAX_CALL_MANAGERS,
    )?;
    let mut priorities = members
        .iter()
        .map(|member| member.priority)
        .collect::<Vec<_>>();
    priorities.sort_unstable();
    if priorities.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(PhoneXmlError::InvalidField {
            field: "call-manager priority",
            expected: "unique priorities",
        });
    }
    if members.iter().any(|member| {
        member.call_manager.ports.signaling == 0
            || member.call_manager.ports.secure_signaling == Some(0)
    }) {
        return Err(PhoneXmlError::InvalidField {
            field: "call-manager port",
            expected: "between 1 and 65535",
        });
    }
    Ok(())
}

fn validate_firmware_loads(loads: &[ProvisioningFirmwareLoad]) -> Result<(), PhoneXmlError> {
    validate_count(
        "firmware load entries",
        loads.len(),
        PROVISIONING_MAX_FIRMWARE_LOADS,
    )?;
    for load in loads {
        validate_optional_text("firmware model", load.model.as_deref(), 1, 64)?;
        validate_text("firmware load", &load.file, 1, 128)?;
    }
    Ok(())
}

fn validate_locales(locales: &ProvisioningLocales) -> Result<(), PhoneXmlError> {
    for value in [
        locales.user.as_deref(),
        locales.network.as_deref(),
        locales.user_version.as_deref(),
        locales.network_version.as_deref(),
    ] {
        validate_optional_text("locale", value, 1, 64)?;
    }
    Ok(())
}

fn validate_service_urls(urls: &ProvisioningServiceUrls) -> Result<(), PhoneXmlError> {
    for value in [
        urls.authentication.as_deref(),
        urls.directory.as_deref(),
        urls.idle.as_deref(),
        urls.information.as_deref(),
        urls.messages.as_deref(),
        urls.proxy.as_deref(),
        urls.services.as_deref(),
    ] {
        validate_optional_text("provisioning service URL", value, 1, 256)?;
    }
    Ok(())
}

fn validate_dscp(dscp: &ProvisioningDscp) -> Result<(), PhoneXmlError> {
    if [dscp.signaling, dscp.audio, dscp.video]
        .into_iter()
        .flatten()
        .any(|value| value > 63)
    {
        return Err(PhoneXmlError::InvalidField {
            field: "provisioning DSCP",
            expected: "between 0 and 63",
        });
    }
    Ok(())
}

fn validate_codec_policy(policy: &ProvisioningCodecPolicy) -> Result<(), PhoneXmlError> {
    validate_optional_text("preferred codec", policy.preferred.as_deref(), 1, 32)
}

fn validate_security(security: &ProvisioningSecurity) -> Result<(), PhoneXmlError> {
    if let Some(capf) = &security.capf {
        if capf.phone_port == 0 {
            return Err(PhoneXmlError::InvalidField {
                field: "CAPF port",
                expected: "between 1 and 65535",
            });
        }
        validate_optional_text(
            "CAPF authentication mode",
            capf.authentication_mode.as_deref(),
            1,
            64,
        )?;
        validate_optional_text(
            "CAPF certificate operation",
            capf.certificate_operation.as_deref(),
            1,
            64,
        )?;
    }
    Ok(())
}

fn validate_host(value: &str) -> Result<(), PhoneXmlError> {
    if value.parse::<IpAddr>().is_ok() {
        return Ok(());
    }
    validate_text("provisioning host", value, 1, 253)?;
    if value.split('.').any(|label| {
        label.is_empty()
            || label.len() > 63
            || label.starts_with('-')
            || label.ends_with('-')
            || !label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    }) {
        return Err(PhoneXmlError::InvalidField {
            field: "provisioning host",
            expected: "an IPv4, IPv6, or DNS name",
        });
    }
    Ok(())
}

fn validate_count(kind: &'static str, actual: usize, maximum: usize) -> Result<(), PhoneXmlError> {
    if actual > maximum {
        Err(PhoneXmlError::LimitExceeded {
            kind,
            actual,
            maximum,
        })
    } else {
        Ok(())
    }
}

fn validate_optional_text(
    field: &'static str,
    value: Option<&str>,
    minimum: usize,
    maximum: usize,
) -> Result<(), PhoneXmlError> {
    match value {
        Some(value) => validate_text(field, value, minimum, maximum),
        None => Ok(()),
    }
}

fn validate_text(
    field: &'static str,
    value: &str,
    minimum: usize,
    maximum: usize,
) -> Result<(), PhoneXmlError> {
    let length = value.chars().count();
    if !(minimum..=maximum).contains(&length)
        || value.chars().any(|character| character.is_control())
    {
        Err(PhoneXmlError::InvalidField {
            field,
            expected: "within the provisioning text bounds",
        })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device() -> DeviceProvisioning {
        DeviceProvisioning {
            protocol: ProvisioningProtocol::Sccp,
            device_pool: ProvisioningDevicePool {
                date_time: ProvisioningDateTime {
                    date_template: "D/M/Ya".into(),
                    time_zone: "Pacific Standard/Daylight Time".into(),
                    ntps: ProvisioningNtpServers {
                        entries: vec![ProvisioningNtpServer {
                            name: ProvisioningHost::new("192.0.2.10").unwrap(),
                            mode: ProvisioningNtpMode::Unicast,
                        }],
                    },
                },
                call_managers: ProvisioningCallManagerGroup {
                    members: ProvisioningCallManagerMembers {
                        entries: vec![ProvisioningCallManagerMember {
                            priority: 0,
                            call_manager: ProvisioningCallManager {
                                host: ProvisioningHost::new("pbx.example.test").unwrap(),
                                ports: ProvisioningPorts {
                                    signaling: 2000,
                                    secure_signaling: Some(2443),
                                },
                            },
                        }],
                    },
                },
            },
            locales: ProvisioningLocales {
                user: Some("English_United_States".into()),
                network: Some("United_States".into()),
                ..Default::default()
            },
            service_urls: ProvisioningServiceUrls {
                directory: Some("https://pbx.example.test/sccp/directory".into()),
                services: Some("https://pbx.example.test/sccp/services".into()),
                ..Default::default()
            },
            security: ProvisioningSecurity {
                transport: ProvisioningTransport::Tls,
                capf: Some(ProvisioningCapf {
                    phone_port: 3804,
                    host: ProvisioningHost::new("198.51.100.8").unwrap(),
                    authentication_mode: Some("ByAuthenticationString".into()),
                    authentication_token: Some(ProvisioningSecret::new("secret-token").unwrap()),
                    certificate_operation: Some("InstallUpgrade".into()),
                }),
                encrypted_config: Some(ProvisioningSecret::new("encrypted-config-token").unwrap()),
                ..Default::default()
            },
            vendor: ProvisioningVendorConfig {
                dscp: ProvisioningDscp {
                    signaling: Some(24),
                    audio: Some(46),
                    video: Some(34),
                },
                codecs: ProvisioningCodecPolicy {
                    preferred: Some("g711alaw".into()),
                    g722: Some(ProvisioningBoolean::Enabled),
                    g729: Some(ProvisioningBoolean::Disabled),
                },
                features: ProvisioningFeatures {
                    enbloc_dialing: Some(ProvisioningBoolean::Enabled),
                    do_not_disturb: Some(ProvisioningBoolean::Enabled),
                    call_pickup: Some(ProvisioningBoolean::Enabled),
                    barge: Some(ProvisioningBoolean::Enabled),
                    blf: Some(ProvisioningBoolean::Enabled),
                    mwi: Some(ProvisioningBoolean::Enabled),
                    mobility: Some(ProvisioningBoolean::Enabled),
                    ..Default::default()
                },
            },
            firmware_loads: vec![ProvisioningFirmwareLoad {
                model: Some("Cisco 7961".into()),
                file: "term61.default.loads".into(),
            }],
            ssh_user_id: Some("phone-admin".into()),
            ssh_password: Some(ProvisioningSecret::new("ssh-password").unwrap()),
        }
    }

    #[test]
    fn device_and_default_roots_round_trip_bootable_subset() {
        let mut expected = device();
        expected
            .device_pool
            .call_managers
            .members
            .entries
            .push(ProvisioningCallManagerMember {
                priority: 1,
                call_manager: ProvisioningCallManager {
                    host: ProvisioningHost::new("2001:db8::20").unwrap(),
                    ports: ProvisioningPorts {
                        signaling: 2000,
                        secure_signaling: Some(2443),
                    },
                },
            });
        expected.service_urls.information =
            Some("https://pbx.example.test/info?name=A&B=<all>".into());
        let xml = expected.to_xml().unwrap();
        assert!(xml.starts_with("<device>"));
        assert!(xml.contains("A&amp;B=&lt;all&gt;"));
        assert!(
            xml.find("pbx.example.test").unwrap() < xml.find("2001:db8::20").unwrap(),
            "call-manager order must remain deterministic"
        );
        assert_eq!(
            DeviceProvisioning::from_xml(xml.as_bytes()).unwrap(),
            expected
        );

        let default = DefaultProvisioning {
            device_pool: expected.device_pool.clone(),
            firmware_loads: expected.firmware_loads.clone(),
        };
        let xml = default.to_xml().unwrap();
        assert_eq!(
            DefaultProvisioning::from_xml(xml.as_bytes()).unwrap(),
            default
        );
    }

    #[test]
    fn provisioning_rejects_ambiguous_endpoints_priorities_and_dscp() {
        assert!(ProvisioningHost::new("bad host").is_err());
        let mut invalid = device();
        invalid
            .device_pool
            .call_managers
            .members
            .entries
            .push(invalid.device_pool.call_managers.members.entries[0].clone());
        assert!(invalid.validate().is_err());

        let mut invalid = device();
        let mut clear_only = invalid.device_pool.call_managers.members.entries[0].clone();
        clear_only.priority = 1;
        clear_only.call_manager.ports.secure_signaling = None;
        invalid
            .device_pool
            .call_managers
            .members
            .entries
            .push(clear_only);
        assert!(invalid.validate().is_err());

        let mut invalid = device();
        invalid
            .device_pool
            .call_managers
            .members
            .entries
            .iter_mut()
            .for_each(|member| member.call_manager.ports.secure_signaling = None);
        assert!(invalid.validate().is_err());

        let mut invalid = device();
        invalid.vendor.dscp.audio = Some(64);
        assert!(invalid.validate().is_err());

        let mut invalid = device();
        invalid.device_pool.call_managers.members.entries[0]
            .call_manager
            .ports
            .secure_signaling = Some(0);
        assert!(invalid.validate().is_err());

        let mut invalid = DefaultProvisioning {
            device_pool: device().device_pool,
            firmware_loads: vec![ProvisioningFirmwareLoad {
                model: None,
                file: String::new(),
            }],
        };
        assert!(invalid.validate().is_err());
        invalid.firmware_loads[0].file = "term.default.loads".into();
        assert!(invalid.validate().is_ok());
    }

    #[test]
    fn provisioning_debug_redacts_every_secret() {
        let debug = format!("{:?}", device());
        assert!(!debug.contains("secret-token"));
        assert!(!debug.contains("encrypted-config-token"));
        assert!(!debug.contains("ssh-password"));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn provisioning_rejects_wrong_unsafe_unknown_malformed_and_oversized_xml() {
        assert!(DeviceProvisioning::from_xml(b"<Default/>").is_err());
        assert!(DeviceProvisioning::from_xml(b"<device><unknown/></device>").is_err());
        assert!(DeviceProvisioning::from_xml(b"<device>").is_err());
        assert!(matches!(
            DeviceProvisioning::from_xml(
                br#"<!DOCTYPE device [<!ENTITY host "pbx.example.test">]><device/>"#,
            ),
            Err(PhoneXmlError::DocumentTypeForbidden)
        ));
        assert!(matches!(
            DeviceProvisioning::from_xml(&vec![b'x'; PROVISIONING_MAX_BYTES + 1]),
            Err(PhoneXmlError::LimitExceeded { .. })
        ));

        #[derive(Debug)]
        struct FailingWriter;
        impl fmt::Write for FailingWriter {
            fn write_str(&mut self, _value: &str) -> fmt::Result {
                Err(fmt::Error)
            }
        }
        assert!(matches!(
            device().write_xml(FailingWriter),
            Err(PhoneXmlError::Write(_))
        ));
    }
}
