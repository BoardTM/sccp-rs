//! Telemetry phone XML document family.

use super::*;

pub(super) const LAST_OUT_OF_SERVICE_ALARM: &str = "LastOutOfServiceInformation";

/// One named string value in a phone alarm parameter list.
#[derive(Clone, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CiscoIpPhoneAlarmString {
    #[serde(rename = "@name")]
    pub name: String,
    #[serde(rename = "$text", default)]
    /// Free-form value omitted from [`Debug`](std::fmt::Debug) output.
    pub value: String,
}

impl fmt::Debug for CiscoIpPhoneAlarmString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CiscoIpPhoneAlarmString")
            .field("name", &self.name)
            .field("value", &"<redacted>")
            .finish()
    }
}

/// One named numeric enumeration in a phone alarm parameter list.
#[derive(Clone, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CiscoIpPhoneAlarmEnum {
    #[serde(rename = "@name")]
    pub name: String,
    #[serde(rename = "$text")]
    pub value: i32,
}

impl fmt::Debug for CiscoIpPhoneAlarmEnum {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CiscoIpPhoneAlarmEnum")
            .field("name", &self.name)
            .field("value", &self.value)
            .finish()
    }
}

/// An ordered, typed alarm parameter.
#[derive(Clone, serde::Deserialize, Eq, PartialEq, Serialize)]
pub enum CiscoIpPhoneAlarmParameter {
    /// Textual parameter whose value remains redacted in diagnostics.
    #[serde(rename = "String")]
    String(CiscoIpPhoneAlarmString),
    /// Numeric parameter safe for typed summaries when explicitly allowlisted.
    #[serde(rename = "Enum")]
    Enum(CiscoIpPhoneAlarmEnum),
}

impl fmt::Debug for CiscoIpPhoneAlarmParameter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::String(value) => value.fmt(formatter),
            Self::Enum(value) => value.fmt(formatter),
        }
    }
}

/// Ordered parameters carried by a supported phone alarm.
#[derive(Clone, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CiscoIpPhoneAlarmParameterList {
    #[serde(rename = "$value", default)]
    pub parameters: Vec<CiscoIpPhoneAlarmParameter>,
}

impl fmt::Debug for CiscoIpPhoneAlarmParameterList {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CiscoIpPhoneAlarmParameterList")
            .field("parameter_count", &self.parameters.len())
            .finish()
    }
}

/// The single supported alarm entry within an alarm document.
#[derive(Clone, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CiscoIpPhoneAlarmEntry {
    #[serde(rename = "@Name")]
    pub name: String,
    #[serde(rename = "ParameterList")]
    pub parameter_list: CiscoIpPhoneAlarmParameterList,
}

impl fmt::Debug for CiscoIpPhoneAlarmEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CiscoIpPhoneAlarmEntry")
            .field("name", &self.name)
            .field("parameter_count", &self.parameter_list.parameters.len())
            .finish()
    }
}

/// A typed `LastOutOfServiceInformation` alarm document.
#[derive(Clone, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename = "x-cisco-alarm", deny_unknown_fields)]
pub struct CiscoIpPhoneAlarm {
    #[serde(rename = "Alarm")]
    pub alarm: CiscoIpPhoneAlarmEntry,
}

impl fmt::Debug for CiscoIpPhoneAlarm {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CiscoIpPhoneAlarm")
            .field("name", &self.alarm.name)
            .field(
                "parameter_count",
                &self.alarm.parameter_list.parameters.len(),
            )
            .finish()
    }
}

impl CiscoIpPhoneAlarm {
    /// Checks the supported alarm name and uniqueness/bounds of all parameters.
    pub fn validate(&self) -> Result<(), PhoneXmlError> {
        if self.alarm.name != LAST_OUT_OF_SERVICE_ALARM {
            return Err(PhoneXmlError::InvalidField {
                field: "phone alarm name",
                expected: "LastOutOfServiceInformation",
            });
        }
        let mut names = HashSet::new();
        for parameter in &self.alarm.parameter_list.parameters {
            let name = match parameter {
                CiscoIpPhoneAlarmParameter::String(value) => {
                    validate_optional_text(
                        "phone alarm string name",
                        Some(&value.name),
                        1,
                        PHONE_ALARM_MAX_BYTES,
                    )?;
                    validate_optional_text(
                        "phone alarm string value",
                        Some(&value.value),
                        0,
                        PHONE_ALARM_MAX_BYTES,
                    )?;
                    &value.name
                }
                CiscoIpPhoneAlarmParameter::Enum(value) => {
                    validate_optional_text(
                        "phone alarm enumeration name",
                        Some(&value.name),
                        1,
                        PHONE_ALARM_MAX_BYTES,
                    )?;
                    &value.name
                }
            };
            if !names.insert(name.as_str()) {
                return Err(PhoneXmlError::InvalidField {
                    field: "phone alarm parameter names",
                    expected: "unique across string and enumeration parameters",
                });
            }
        }
        Ok(())
    }

    /// Parses the supported alarm schema while redacting schema-error details.
    pub fn from_xml(document: &[u8]) -> Result<Self, PhoneXmlError> {
        #[derive(serde::Deserialize)]
        enum AlarmEnvelope {
            #[serde(rename = "x-cisco-alarm")]
            Alarm(CiscoIpPhoneAlarm),
        }
        let AlarmEnvelope::Alarm(document) =
            from_bytes(document, PHONE_ALARM_MAX_BYTES).map_err(redact_alarm_schema_error)?;
        document.validate()?;
        Ok(document)
    }

    /// Validates and serializes the alarm within [`PHONE_ALARM_MAX_BYTES`].
    pub fn to_xml(&self) -> Result<String, PhoneXmlError> {
        self.validate()?;
        to_string(self, PHONE_ALARM_MAX_BYTES)
    }

    /// Returns a textual parameter by name without exposing it through diagnostics.
    pub fn string(&self, name: &str) -> Option<&str> {
        self.alarm
            .parameter_list
            .parameters
            .iter()
            .find_map(|parameter| match parameter {
                CiscoIpPhoneAlarmParameter::String(value) if value.name == name => {
                    Some(value.value.as_str())
                }
                _ => None,
            })
    }

    /// Returns a numeric parameter by name.
    pub fn enumeration(&self, name: &str) -> Option<i32> {
        self.alarm
            .parameter_list
            .parameters
            .iter()
            .find_map(|parameter| match parameter {
                CiscoIpPhoneAlarmParameter::Enum(value) if value.name == name => Some(value.value),
                _ => None,
            })
    }

    /// Returns the allowlisted numeric out-of-service reason, when present.
    pub fn reason_for_out_of_service(&self) -> Option<i32> {
        self.enumeration("ReasonForOutOfService")
    }
}

/// Exact bounded bytes for a syntactically valid but unsupported alarm schema.
#[derive(Clone, Eq, PartialEq)]
pub struct OpaquePhoneAlarm(Vec<u8>);

impl OpaquePhoneAlarm {
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

impl fmt::Debug for OpaquePhoneAlarm {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpaquePhoneAlarm")
            .field("byte_count", &self.0.len())
            .finish_non_exhaustive()
    }
}

/// Parsed alarm telemetry or a bounded lossless unknown schema.
#[derive(Clone, Eq, PartialEq)]
pub enum PhoneAlarmTelemetry {
    /// The supported out-of-service schema, retained as typed parameters.
    LastOutOfService(CiscoIpPhoneAlarm),
    /// A syntactically valid unsupported schema retained losslessly.
    Opaque(OpaquePhoneAlarm),
}

/// Allowlisted alarm family that is safe to publish without parameter data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PhoneAlarmKind {
    LastOutOfService,
}

/// Secret-safe fields selected from a known alarm document.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhoneAlarmSummary {
    pub kind: PhoneAlarmKind,
    /// Optional numeric reason; no free-form parameter data is published.
    pub reason_for_out_of_service: Option<i32>,
}

impl fmt::Debug for PhoneAlarmTelemetry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LastOutOfService(alarm) => alarm.fmt(formatter),
            Self::Opaque(alarm) => alarm.fmt(formatter),
        }
    }
}

impl PhoneAlarmTelemetry {
    /// Returns only allowlisted numeric fields for known alarm schemas.
    /// Opaque schemas never produce a publishable summary.
    pub fn summary(&self) -> Option<PhoneAlarmSummary> {
        match self {
            Self::LastOutOfService(alarm) => Some(PhoneAlarmSummary {
                kind: PhoneAlarmKind::LastOutOfService,
                reason_for_out_of_service: alarm.reason_for_out_of_service(),
            }),
            Self::Opaque(_) => None,
        }
    }

    pub fn is_opaque(&self) -> bool {
        matches!(self, Self::Opaque(_))
    }
}

/// Parse one bounded alarm document without treating malformed known XML as
/// an opaque schema.
pub fn parse_phone_alarm(document: &[u8]) -> Result<PhoneAlarmTelemetry, PhoneXmlError> {
    #[derive(Debug, serde::Deserialize)]
    struct AlarmProbe {
        #[serde(rename = "Alarm", default)]
        alarms: Vec<AlarmNameProbe>,
    }

    #[derive(Debug, serde::Deserialize)]
    struct AlarmNameProbe {
        #[serde(rename = "@Name")]
        name: String,
    }

    #[derive(serde::Deserialize)]
    enum AlarmProbeEnvelope {
        #[serde(rename = "x-cisco-alarm")]
        Alarm(AlarmProbe),
        #[serde(other)]
        Unknown,
    }

    let supported = match from_bytes(document, PHONE_ALARM_MAX_BYTES)
        .map_err(redact_alarm_schema_error)?
    {
        AlarmProbeEnvelope::Alarm(probe) => {
            matches!(probe.alarms.as_slice(), [alarm] if alarm.name == LAST_OUT_OF_SERVICE_ALARM)
        }
        AlarmProbeEnvelope::Unknown => false,
    };
    if supported {
        CiscoIpPhoneAlarm::from_xml(document).map(PhoneAlarmTelemetry::LastOutOfService)
    } else {
        Ok(PhoneAlarmTelemetry::Opaque(OpaquePhoneAlarm(
            document.to_vec(),
        )))
    }
}

pub(super) fn redact_alarm_schema_error(error: PhoneXmlError) -> PhoneXmlError {
    match error {
        PhoneXmlError::Deserialize(_) => PhoneXmlError::InvalidAlarmSchema,
        error => error,
    }
}

/// A six-octet wireless basic-service-set address with redacted diagnostics.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct PhoneBssid([u8; 6]);

impl PhoneBssid {
    /// Wraps the exact six address octets.
    pub const fn from_octets(octets: [u8; 6]) -> Self {
        Self(octets)
    }

    pub const fn octets(self) -> [u8; 6] {
        self.0
    }

    /// Parses six colon-separated hexadecimal octets.
    pub fn parse(value: &str) -> Result<Self, PhoneXmlError> {
        parse_bssid(value).ok_or(PhoneXmlError::InvalidField {
            field: "phone location BSSID",
            expected: "six hexadecimal octets separated by colons",
        })
    }
}

impl fmt::Display for PhoneBssid {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
            self.0[0], self.0[1], self.0[2], self.0[3], self.0[4], self.0[5]
        )
    }
}

impl fmt::Debug for PhoneBssid {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PhoneBssid(<redacted>)")
    }
}

impl Serialize for PhoneBssid {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> serde::Deserialize<'de> for PhoneBssid {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        parse_bssid(&value).map_or_else(
            || {
                Err(serde::de::Error::custom(
                    "BSSID must contain six hexadecimal octets separated by colons",
                ))
            },
            Ok,
        )
    }
}

pub(super) fn parse_bssid(value: &str) -> Option<PhoneBssid> {
    let mut octets = [0u8; 6];
    let mut components = value.split(':');
    for octet in &mut octets {
        let component = components.next()?;
        if component.len() != 2 {
            return None;
        }
        *octet = u8::from_str_radix(component, 16).ok()?;
    }
    components.next().is_none().then_some(PhoneBssid(octets))
}

/// Wireless location fields reported for the phone's first interface.
///
/// Diagnostics expose only lengths and never the address or network names.
#[derive(Clone, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CiscoIpPhoneWifiLocation {
    #[serde(rename = "BSSID")]
    pub bssid: PhoneBssid,
    #[serde(rename = "SSID")]
    pub ssid: String,
    #[serde(rename = "APName")]
    pub access_point_name: String,
}

impl fmt::Debug for CiscoIpPhoneWifiLocation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CiscoIpPhoneWifiLocation")
            .field("bssid", &self.bssid)
            .field("ssid_byte_count", &self.ssid.len())
            .field(
                "access_point_name_char_count",
                &self.access_point_name.chars().count(),
            )
            .finish()
    }
}

/// Empty marker indicating that the phone considers itself off premises.
#[derive(Clone, Default, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CiscoIpPhoneOffPremises {
    #[serde(rename = "$text", default, skip_serializing_if = "String::is_empty")]
    marker: String,
}

impl fmt::Debug for CiscoIpPhoneOffPremises {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CiscoIpPhoneOffPremises")
    }
}

impl CiscoIpPhoneOffPremises {
    /// Creates the required empty marker element.
    pub const fn new() -> Self {
        Self {
            marker: String::new(),
        }
    }

    fn validate(&self) -> Result<(), PhoneXmlError> {
        if self.marker.is_empty() {
            Ok(())
        } else {
            Err(PhoneXmlError::InvalidField {
                field: "phone off-premises marker",
                expected: "an empty element",
            })
        }
    }
}

/// Typed wireless location-information document for interface one.
///
/// Diagnostics retain only the off-premises flag and redacted wireless data.
#[derive(Clone, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename = "Interface1", deny_unknown_fields)]
pub struct CiscoIpPhoneLocationInformation {
    #[serde(rename = "wifi")]
    pub wifi: CiscoIpPhoneWifiLocation,
    #[serde(rename = "OffPrem", default, skip_serializing_if = "Option::is_none")]
    pub off_premises: Option<CiscoIpPhoneOffPremises>,
}

impl fmt::Debug for CiscoIpPhoneLocationInformation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CiscoIpPhoneLocationInformation")
            .field("wifi", &self.wifi)
            .field("off_premises", &self.off_premises.is_some())
            .finish()
    }
}

impl CiscoIpPhoneLocationInformation {
    /// Validates network-name bounds and the empty off-premises marker.
    pub fn validate(&self) -> Result<(), PhoneXmlError> {
        validate_optional_text(
            "phone location SSID",
            Some(&self.wifi.ssid),
            0,
            PHONE_LOCATION_MAX_BYTES,
        )?;
        validate_optional_text(
            "phone location access-point name",
            Some(&self.wifi.access_point_name),
            0,
            PHONE_LOCATION_MAX_BYTES,
        )?;
        if self.wifi.ssid.len() > 32 {
            return Err(PhoneXmlError::InvalidField {
                field: "phone location SSID",
                expected: "at most 32 bytes",
            });
        }
        if let Some(off_premises) = &self.off_premises {
            off_premises.validate()?;
        }
        Ok(())
    }

    /// Parses the supported wireless-interface schema with redacted failures.
    pub fn from_xml(document: &[u8]) -> Result<Self, PhoneXmlError> {
        #[derive(serde::Deserialize)]
        enum LocationEnvelope {
            #[serde(rename = "Interface1")]
            Location(CiscoIpPhoneLocationInformation),
        }

        let LocationEnvelope::Location(location) =
            from_bytes(document, PHONE_LOCATION_MAX_BYTES).map_err(redact_location_schema_error)?;
        location.validate()?;
        Ok(location)
    }

    /// Validates and serializes location telemetry within [`PHONE_LOCATION_MAX_BYTES`].
    pub fn to_xml(&self) -> Result<String, PhoneXmlError> {
        self.validate()?;
        to_string(self, PHONE_LOCATION_MAX_BYTES)
    }

    pub const fn is_off_premises(&self) -> bool {
        self.off_premises.is_some()
    }
}

/// Exact bounded bytes for a syntactically valid unsupported location schema.
#[derive(Clone, Eq, PartialEq)]
pub struct OpaquePhoneLocation(Vec<u8>);

impl OpaquePhoneLocation {
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

impl fmt::Debug for OpaquePhoneLocation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpaquePhoneLocation")
            .field("byte_count", &self.0.len())
            .finish_non_exhaustive()
    }
}

/// Parsed location telemetry or a bounded lossless unsupported schema.
#[derive(Clone, Eq, PartialEq)]
pub enum PhoneLocationTelemetry {
    /// The supported wireless-interface schema.
    WirelessInterface(CiscoIpPhoneLocationInformation),
    /// A syntactically valid unsupported schema retained losslessly.
    Opaque(OpaquePhoneLocation),
}

impl fmt::Debug for PhoneLocationTelemetry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WirelessInterface(location) => location.fmt(formatter),
            Self::Opaque(location) => location.fmt(formatter),
        }
    }
}

/// Allowlisted location family that is safe to publish without location data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PhoneLocationKind {
    WirelessInterface,
}

/// Secret-safe location summary without network names or addresses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhoneLocationSummary {
    pub kind: PhoneLocationKind,
    /// Whether the marker was present; network names and addresses are omitted.
    pub off_premises: bool,
}

impl PhoneLocationTelemetry {
    /// Returns only allowlisted non-identifying fields for a known schema.
    pub fn summary(&self) -> Option<PhoneLocationSummary> {
        match self {
            Self::WirelessInterface(location) => Some(PhoneLocationSummary {
                kind: PhoneLocationKind::WirelessInterface,
                off_premises: location.is_off_premises(),
            }),
            Self::Opaque(_) => None,
        }
    }

    pub fn is_opaque(&self) -> bool {
        matches!(self, Self::Opaque(_))
    }
}

/// Parse one bounded location-information document without treating a malformed
/// supported root as an opaque schema.
pub fn parse_phone_location(document: &[u8]) -> Result<PhoneLocationTelemetry, PhoneXmlError> {
    #[derive(Debug, serde::Deserialize)]
    struct LocationProbe;

    #[derive(serde::Deserialize)]
    enum LocationProbeEnvelope {
        #[serde(rename = "Interface1")]
        Location(LocationProbe),
        #[serde(other)]
        Unknown,
    }

    let supported = matches!(
        from_bytes(document, PHONE_LOCATION_MAX_BYTES).map_err(redact_location_schema_error)?,
        LocationProbeEnvelope::Location(_)
    );
    if supported {
        CiscoIpPhoneLocationInformation::from_xml(document)
            .map(PhoneLocationTelemetry::WirelessInterface)
    } else {
        Ok(PhoneLocationTelemetry::Opaque(OpaquePhoneLocation(
            document.to_vec(),
        )))
    }
}

pub(super) fn redact_location_schema_error(error: PhoneXmlError) -> PhoneXmlError {
    match error {
        PhoneXmlError::Deserialize(_) => PhoneXmlError::InvalidLocationSchema,
        error => error,
    }
}
