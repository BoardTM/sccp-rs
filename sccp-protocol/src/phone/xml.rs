//! Typed XML documents exchanged with phone services.
//!
//! Known document schemas go through this Serde boundary so size, encoding,
//! and document-type policy is applied consistently before a model is used.

use std::collections::HashSet;
use std::fmt;
use std::str::FromStr;

use quick_xml::XmlVersion;
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;

use crate::{ConferenceId, ParticipantId};

mod document;
pub use document::PhoneXmlDocument;

/// Maximum participants represented by a conference menu document.
pub const CONFERENCE_LIST_MAX_PARTICIPANTS: usize = 16;
/// Maximum encoded conference menu size, in bytes.
pub const CONFERENCE_LIST_MAX_BYTES: usize = 2_000;
/// Maximum entries accepted in a directory document.
pub const PHONE_DIRECTORY_MAX_ENTRIES: usize = 32;
/// Maximum encoded directory document size, in bytes.
pub const PHONE_DIRECTORY_MAX_BYTES: usize = 8_192;
/// Maximum selectable entries in a plain menu document.
pub const PHONE_MENU_MAX_ITEMS: usize = 100;
/// Maximum selectable entries in an icon menu document.
pub const PHONE_ICON_MENU_MAX_ITEMS: usize = 32;
/// Maximum embedded or referenced icons in an icon menu document.
pub const PHONE_ICON_MENU_MAX_ICONS: usize = 10;
/// Maximum encoded size shared by menu document families, in bytes.
pub const PHONE_MENU_MAX_BYTES: usize = 64 * 1_024;
/// Maximum Unicode character count for a text document body.
pub const PHONE_TEXT_MAX_CHARS: usize = 4_000;
/// Maximum encoded text document size, in bytes.
pub const PHONE_TEXT_MAX_BYTES: usize = 32 * 1_024;
/// Compatibility character bound for display profiles with smaller text capacity.
pub const PHONE_TEXT_LEGACY_MAX_CHARS: usize = 1_024;
/// Reserved application identifier used by text-display workflows.
pub const PHONE_TEXT_APPLICATION_ID: u32 = 9_089;
/// Maximum input controls in one input document.
pub const PHONE_INPUT_MAX_ITEMS: usize = 5;
/// Maximum encoded input document size, in bytes.
pub const PHONE_INPUT_MAX_BYTES: usize = 32 * 1_024;
/// Maximum actions in one execute document.
pub const PHONE_EXECUTE_MAX_ITEMS: usize = 3;
/// Maximum encoded execute document size, in bytes.
pub const PHONE_EXECUTE_MAX_BYTES: usize = 8 * 1_024;
/// Maximum decoded bitmap data in an inline image, in bytes.
pub const PHONE_IMAGE_BITMAP_MAX_BYTES: usize = 2_162;
/// Maximum touch entries in an inline-bitmap graphic menu.
pub const PHONE_GRAPHIC_MENU_MAX_ITEMS: usize = 12;
/// Maximum touch entries in an image-file graphic menu.
pub const PHONE_GRAPHIC_FILE_MENU_MAX_ITEMS: usize = 32;
/// Maximum encoded size shared by image document families, in bytes.
pub const PHONE_IMAGE_MAX_BYTES: usize = 64 * 1_024;
/// Maximum decoded bitmap data in a status document, in bytes.
pub const PHONE_STATUS_BITMAP_MAX_BYTES: usize = 557;
/// Maximum encoded size shared by status document families, in bytes.
pub const PHONE_STATUS_MAX_BYTES: usize = 8 * 1_024;
/// Maximum encoded alarm telemetry size, in bytes.
pub const PHONE_ALARM_MAX_BYTES: usize = 2_048;
/// Maximum encoded location telemetry size, in bytes.
pub const PHONE_LOCATION_MAX_BYTES: usize = 2_404;
/// Reserved application identifier used by background-image workflows.
pub const PHONE_BACKGROUND_APPLICATION_ID: u32 = 9_086;
/// Maximum choices in a background-image list.
pub const PHONE_BACKGROUND_LIST_MAX_ITEMS: usize = 50;
/// Maximum encoded background-image list size, in bytes.
pub const PHONE_BACKGROUND_LIST_MAX_BYTES: usize = 32 * 1_024;
/// Maximum encoded background-control document size, in bytes.
pub const PHONE_BACKGROUND_CONTROL_MAX_BYTES: usize = 2_000;
/// Reserved application identifier used by ringtone workflows.
pub const PHONE_RINGTONE_APPLICATION_ID: u32 = 9_087;
/// Maximum encoded ringtone-control document size, in bytes.
pub const PHONE_RINGTONE_MAX_BYTES: usize = 2_000;
/// Maximum element nesting accepted before deserialization.
pub const PHONE_XML_MAX_NESTING_DEPTH: usize = 32;
const PHONE_DIRECTORY_TEXT_MAX_CHARS: usize = 32;
const PHONE_XML_URL_MAX_CHARS: usize = 256;

/// Schema, resource-limit, and serialization failures at the XML boundary.
#[derive(Debug, Error)]
pub enum PhoneXmlError {
    /// A collection or encoded document crossed its explicit resource bound.
    #[error("{kind} has {actual} entries or bytes; maximum is {maximum}")]
    LimitExceeded {
        kind: &'static str,
        actual: usize,
        maximum: usize,
    },
    /// Undeclared non-UTF-8 input reached a UTF-8-only boundary.
    #[error("phone XML is not valid UTF-8: {0}")]
    InvalidUtf8(#[source] std::str::Utf8Error),
    /// A document-type declaration was rejected before entity expansion.
    #[error("phone XML document types and entity declarations are not allowed")]
    DocumentTypeForbidden,
    /// An entity reference was undeclared or resolved to an invalid XML character.
    #[error("phone XML contains an invalid or undeclared entity reference")]
    InvalidEntity,
    /// A recognized alarm root did not satisfy its typed schema.
    #[error("supported phone alarm does not match its typed schema")]
    InvalidAlarmSchema,
    /// A recognized location root did not satisfy its typed schema.
    #[error("supported phone location information does not match its typed schema")]
    InvalidLocationSchema,
    /// Element depth crossed [`PHONE_XML_MAX_NESTING_DEPTH`].
    #[error("phone XML nesting exceeds the maximum depth of {maximum}")]
    NestingTooDeep { maximum: usize },
    /// Tokenization failed before typed deserialization.
    #[error("phone XML is malformed: {0}")]
    Malformed(#[source] quick_xml::Error),
    /// The XML was well-formed but did not match the selected model.
    #[error("phone XML does not match its typed schema: {0}")]
    Deserialize(#[source] quick_xml::DeError),
    /// The typed model could not be converted to XML.
    #[error("phone XML could not be serialized: {0}")]
    Serialize(#[source] quick_xml::SeError),
    /// A formatting sink failed while receiving a bounded serialized document.
    #[error("phone XML could not be written: {0}")]
    Write(#[source] fmt::Error),
    /// A model value violated a schema invariant beyond its Rust type.
    #[error("{field} must be {expected}")]
    InvalidField {
        field: &'static str,
        expected: &'static str,
    },
}

/// The application focus that receives keypad events for a displayable phone
/// service document.
#[derive(Clone, Copy, Debug, Default, serde::Deserialize, Eq, PartialEq, Serialize)]
pub enum PhoneKeypadTarget {
    #[default]
    #[serde(rename = "application")]
    Application,
    #[serde(rename = "applicationCall")]
    ApplicationCall,
    #[serde(rename = "activeCall")]
    ActiveCall,
}

/// A physical key event routed by a displayable phone service document.
#[derive(Clone, Copy, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
pub enum PhoneXmlKey {
    KeyPad0,
    KeyPad1,
    KeyPad2,
    KeyPad3,
    KeyPad4,
    KeyPad5,
    KeyPad6,
    KeyPad7,
    KeyPad8,
    KeyPad9,
    KeyPadStar,
    KeyPadPound,
    NavUp,
    NavDown,
    NavLeft,
    NavRight,
    NavSelect,
    NavBack,
    PushToTalk,
}

/// Optional label and press/release actions assigned to one soft-key slot.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CiscoIpPhoneSoftKeyItem {
    #[serde(rename = "Name", default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(rename = "Position")]
    pub position: PhoneSoftKeyPosition,
    #[serde(rename = "URL", default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(rename = "URLDown", default, skip_serializing_if = "Option::is_none")]
    pub url_down: Option<String>,
}

/// A phone soft-key slot. `-1` is the documented application/settings slot;
/// physical rows use positions 1 through 16. Device-profile code may impose a
/// smaller maximum without weakening the shared XML model.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PhoneSoftKeyPosition(i8);

impl PhoneSoftKeyPosition {
    /// Sentinel slot used for the application or settings action.
    pub const APPLICATION: Self = Self(-1);

    /// Accepts the application sentinel or a physical slot in `1..=16`.
    pub fn new(value: i16) -> Result<Self, PhoneXmlError> {
        if value == -1 || (1..=16).contains(&value) {
            i8::try_from(value)
                .map(Self)
                .map_err(|_| PhoneXmlError::InvalidField {
                    field: "phone soft-key position",
                    expected: "-1 or between 1 and 16",
                })
        } else {
            Err(PhoneXmlError::InvalidField {
                field: "phone soft-key position",
                expected: "-1 or between 1 and 16",
            })
        }
    }

    /// Returns the signed wire value, including `-1` for [`Self::APPLICATION`].
    pub const fn get(self) -> i8 {
        self.0
    }
}

impl Serialize for PhoneSoftKeyPosition {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_i8(self.0)
    }
}

impl<'de> serde::Deserialize<'de> for PhoneSoftKeyPosition {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = <i16 as serde::Deserialize>::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// Press and optional release actions bound to one physical key.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CiscoIpPhoneKeyItem {
    #[serde(rename = "Key")]
    pub key: PhoneXmlKey,
    #[serde(rename = "URL", default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(rename = "URLDown", default, skip_serializing_if = "Option::is_none")]
    pub url_down: Option<String>,
}

/// Valid display priority for a phone-service application-data envelope.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PhoneServicePriority(u8);

impl PhoneServicePriority {
    pub const LOW: Self = Self(0);
    pub const NORMAL: Self = Self(1);
    pub const HIGH: Self = Self(2);

    /// Validates a numeric envelope priority in the inclusive range `0..=2`.
    pub fn new(value: u32) -> Result<Self, PhoneXmlError> {
        if value > u32::from(Self::HIGH.0) {
            return Err(PhoneXmlError::InvalidField {
                field: "phone-service display priority",
                expected: "between 0 and 2",
            });
        }
        u8::try_from(value)
            .map(Self)
            .map_err(|_| PhoneXmlError::InvalidField {
                field: "phone-service display priority",
                expected: "between 0 and 2",
            })
    }

    pub fn wire(self) -> u32 {
        u32::from(self.0)
    }
}

impl Default for PhoneServicePriority {
    fn default() -> Self {
        Self::NORMAL
    }
}

/// Typed HTTP refresh metadata for a pulled phone-service document.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PhoneXmlRefresh {
    delay_seconds: u32,
    url: String,
}

impl PhoneXmlRefresh {
    /// Validates an ASCII refresh URL and associates its delay in seconds.
    pub fn new(delay_seconds: u32, url: impl Into<String>) -> Result<Self, PhoneXmlError> {
        let refresh = Self {
            delay_seconds,
            url: url.into(),
        };
        validate_optional_text(
            "phone XML refresh URL",
            Some(&refresh.url),
            1,
            PHONE_XML_URL_MAX_CHARS,
        )?;
        if !refresh.url.is_ascii()
            || refresh
                .url
                .chars()
                .any(|character| character.is_ascii_whitespace() || character.is_ascii_control())
        {
            return Err(PhoneXmlError::InvalidField {
                field: "phone XML refresh URL",
                expected: "an ASCII URL between 1 and 256 characters",
            });
        }
        Ok(refresh)
    }

    pub const fn delay_seconds(&self) -> u32 {
        self.delay_seconds
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// Formats the value for an HTTP `Refresh` response header.
    pub fn http_header_value(&self) -> String {
        format!("{};url={}", self.delay_seconds, self.url)
    }
}

/// A complete, schema-ordered text display document.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename = "CiscoIPPhoneText", deny_unknown_fields)]
pub struct CiscoIpPhoneText {
    #[serde(
        rename = "@keypadTarget",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub keypad_target: Option<PhoneKeypadTarget>,
    #[serde(rename = "@appId", default, skip_serializing_if = "Option::is_none")]
    pub application_id: Option<String>,
    #[serde(
        rename = "@onAppFocusLost",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_focus_lost: Option<String>,
    #[serde(
        rename = "@onAppFocusGained",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_focus_gained: Option<String>,
    #[serde(
        rename = "@onAppMinimized",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_minimized: Option<String>,
    #[serde(
        rename = "@onAppClosed",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_closed: Option<String>,
    #[serde(rename = "Title", default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(rename = "Prompt", default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(rename = "SoftKeyItem", default)]
    pub soft_keys: Vec<CiscoIpPhoneSoftKeyItem>,
    #[serde(rename = "KeyItem", default)]
    pub key_items: Vec<CiscoIpPhoneKeyItem>,
    #[serde(rename = "Text", default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

impl CiscoIpPhoneText {
    /// Builds and validates a text page with no optional lifecycle actions.
    pub fn new(
        title: impl Into<String>,
        prompt: impl Into<String>,
        text: impl Into<String>,
    ) -> Result<Self, PhoneXmlError> {
        let document = Self {
            keypad_target: None,
            application_id: None,
            on_focus_lost: None,
            on_focus_gained: None,
            on_minimized: None,
            on_closed: None,
            title: Some(title.into()),
            prompt: Some(prompt.into()),
            soft_keys: Vec::new(),
            key_items: Vec::new(),
            text: Some(text.into()),
        };
        document.validate()?;
        Ok(document)
    }

    /// Validates display metadata and the text-body character bound.
    pub fn validate(&self) -> Result<(), PhoneXmlError> {
        validate_displayable(
            self.title.as_deref(),
            self.prompt.as_deref(),
            self.application_id.as_deref(),
            [
                self.on_focus_lost.as_deref(),
                self.on_focus_gained.as_deref(),
                self.on_minimized.as_deref(),
                self.on_closed.as_deref(),
            ],
            &self.soft_keys,
            &self.key_items,
        )?;
        validate_optional_text(
            "phone text body",
            self.text.as_deref(),
            0,
            PHONE_TEXT_MAX_CHARS,
        )
    }

    /// Parses a complete text page using [`PHONE_TEXT_MAX_BYTES`].
    pub fn from_xml(document: &[u8]) -> Result<Self, PhoneXmlError> {
        <Self as PhoneXmlDocument>::parse_xml(document)
    }

    /// Parses a text page with an additional caller-selected byte limit.
    pub fn from_xml_with_limit(
        document: &[u8],
        maximum_bytes: usize,
    ) -> Result<Self, PhoneXmlError> {
        <Self as PhoneXmlDocument>::parse_xml_with_limit(document, maximum_bytes)
    }

    /// Validates and serializes a text page using [`PHONE_TEXT_MAX_BYTES`].
    pub fn to_xml(&self) -> Result<String, PhoneXmlError> {
        <Self as PhoneXmlDocument>::serialize_xml(self)
    }

    /// Validates and serializes a text page within a caller-selected byte limit.
    pub fn to_xml_with_limit(&self, maximum_bytes: usize) -> Result<String, PhoneXmlError> {
        <Self as PhoneXmlDocument>::serialize_xml_with_limit(self, maximum_bytes)
    }
}

/// One of the input modes accepted by a phone input-service field.
///
/// The password-modified variants preserve whether `P` precedes or follows
/// the base mode because both forms are distinct accepted schema values.
#[derive(Clone, Copy, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
pub enum PhoneInputFlags {
    #[serde(rename = "A")]
    Alphabetic,
    #[serde(rename = "T")]
    Telephone,
    #[serde(rename = "N")]
    Numeric,
    #[serde(rename = "E")]
    Equation,
    #[serde(rename = "U")]
    Uppercase,
    #[serde(rename = "L")]
    Lowercase,
    #[serde(rename = "AP")]
    AlphabeticPassword,
    #[serde(rename = "TP")]
    TelephonePassword,
    #[serde(rename = "NP")]
    NumericPassword,
    #[serde(rename = "EP")]
    EquationPassword,
    #[serde(rename = "UP")]
    UppercasePassword,
    #[serde(rename = "LP")]
    LowercasePassword,
    #[serde(rename = "PA")]
    PasswordAlphabetic,
    #[serde(rename = "PT")]
    PasswordTelephone,
    #[serde(rename = "PN")]
    PasswordNumeric,
    #[serde(rename = "PE")]
    PasswordEquation,
    #[serde(rename = "PU")]
    PasswordUppercase,
    #[serde(rename = "PL")]
    PasswordLowercase,
}

impl PhoneInputFlags {
    /// Every accepted schema spelling, including both password modifier orders.
    pub const ALL: [Self; 18] = [
        Self::Alphabetic,
        Self::Telephone,
        Self::Numeric,
        Self::Equation,
        Self::Uppercase,
        Self::Lowercase,
        Self::AlphabeticPassword,
        Self::TelephonePassword,
        Self::NumericPassword,
        Self::EquationPassword,
        Self::UppercasePassword,
        Self::LowercasePassword,
        Self::PasswordAlphabetic,
        Self::PasswordTelephone,
        Self::PasswordNumeric,
        Self::PasswordEquation,
        Self::PasswordUppercase,
        Self::PasswordLowercase,
    ];
}

/// A schema-bounded name used for one submitted query parameter.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct PhoneInputParameterName(String);

impl PhoneInputParameterName {
    /// Validates a non-empty parameter name of at most 32 characters.
    pub fn new(value: impl Into<String>) -> Result<Self, PhoneXmlError> {
        let value = value.into();
        validate_optional_text("phone input parameter name", Some(&value), 1, 32)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> serde::Deserialize<'de> for PhoneInputParameterName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = <String as serde::Deserialize>::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// One field in a phone input-service document.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CiscoIpPhoneInputItem {
    #[serde(
        rename = "DisplayName",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub display_name: Option<String>,
    #[serde(rename = "QueryStringParam")]
    pub parameter: PhoneInputParameterName,
    #[serde(rename = "InputFlags")]
    pub flags: PhoneInputFlags,
    #[serde(
        rename = "DefaultValue",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub default_value: Option<String>,
}

/// A complete, schema-ordered interactive input document.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename = "CiscoIPPhoneInput", deny_unknown_fields)]
pub struct CiscoIpPhoneInput {
    #[serde(
        rename = "@keypadTarget",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub keypad_target: Option<PhoneKeypadTarget>,
    #[serde(rename = "@appId", default, skip_serializing_if = "Option::is_none")]
    pub application_id: Option<String>,
    #[serde(
        rename = "@onAppFocusLost",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_focus_lost: Option<String>,
    #[serde(
        rename = "@onAppFocusGained",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_focus_gained: Option<String>,
    #[serde(
        rename = "@onAppMinimized",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_minimized: Option<String>,
    #[serde(
        rename = "@onAppClosed",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_closed: Option<String>,
    #[serde(rename = "Title", default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(rename = "Prompt", default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(rename = "SoftKeyItem", default)]
    pub soft_keys: Vec<CiscoIpPhoneSoftKeyItem>,
    #[serde(rename = "KeyItem", default)]
    pub key_items: Vec<CiscoIpPhoneKeyItem>,
    #[serde(rename = "URL")]
    pub url: String,
    #[serde(rename = "InputItem", default)]
    pub items: Vec<CiscoIpPhoneInputItem>,
}

impl CiscoIpPhoneInput {
    /// Builds and validates an input form with no optional lifecycle actions.
    pub fn new(
        title: impl Into<String>,
        prompt: impl Into<String>,
        url: impl Into<String>,
        items: Vec<CiscoIpPhoneInputItem>,
    ) -> Result<Self, PhoneXmlError> {
        let document = Self {
            keypad_target: None,
            application_id: None,
            on_focus_lost: None,
            on_focus_gained: None,
            on_minimized: None,
            on_closed: None,
            title: Some(title.into()),
            prompt: Some(prompt.into()),
            soft_keys: Vec::new(),
            key_items: Vec::new(),
            url: url.into(),
            items,
        };
        document.validate()?;
        Ok(document)
    }

    /// Validates display metadata, submission URL, field count, and field text.
    pub fn validate(&self) -> Result<(), PhoneXmlError> {
        validate_displayable(
            self.title.as_deref(),
            self.prompt.as_deref(),
            self.application_id.as_deref(),
            [
                self.on_focus_lost.as_deref(),
                self.on_focus_gained.as_deref(),
                self.on_minimized.as_deref(),
                self.on_closed.as_deref(),
            ],
            &self.soft_keys,
            &self.key_items,
        )?;
        validate_optional_text(
            "phone input submission URL",
            Some(&self.url),
            1,
            PHONE_XML_URL_MAX_CHARS,
        )?;
        validate_count(
            "phone input fields",
            self.items.len(),
            PHONE_INPUT_MAX_ITEMS,
        )?;
        for item in &self.items {
            validate_optional_text(
                "phone input display name",
                item.display_name.as_deref(),
                0,
                32,
            )?;
            validate_optional_text(
                "phone input default value",
                item.default_value.as_deref(),
                0,
                32,
            )?;
        }
        Ok(())
    }

    /// Parses a complete input form using [`PHONE_INPUT_MAX_BYTES`].
    pub fn from_xml(document: &[u8]) -> Result<Self, PhoneXmlError> {
        <Self as PhoneXmlDocument>::parse_xml(document)
    }

    /// Parses an input form with an additional caller-selected byte limit.
    pub fn from_xml_with_limit(
        document: &[u8],
        maximum_bytes: usize,
    ) -> Result<Self, PhoneXmlError> {
        <Self as PhoneXmlDocument>::parse_xml_with_limit(document, maximum_bytes)
    }

    /// Validates and serializes an input form using [`PHONE_INPUT_MAX_BYTES`].
    pub fn to_xml(&self) -> Result<String, PhoneXmlError> {
        <Self as PhoneXmlDocument>::serialize_xml(self)
    }

    /// Validates and serializes an input form within a caller-selected byte limit.
    pub fn to_xml_with_limit(&self, maximum_bytes: usize) -> Result<String, PhoneXmlError> {
        <Self as PhoneXmlDocument>::serialize_xml_with_limit(self, maximum_bytes)
    }
}

/// Valid priority for one phone execute action.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PhoneExecutePriority(u8);

impl PhoneExecutePriority {
    pub const LOW: Self = Self(0);
    pub const NORMAL: Self = Self(1);
    pub const HIGH: Self = Self(2);

    /// Validates a numeric action priority in the inclusive range `0..=2`.
    pub fn new(value: u8) -> Result<Self, PhoneXmlError> {
        if value > Self::HIGH.0 {
            return Err(PhoneXmlError::InvalidField {
                field: "phone execute priority",
                expected: "between 0 and 2",
            });
        }
        Ok(Self(value))
    }

    pub const fn wire(self) -> u8 {
        self.0
    }
}

impl Serialize for PhoneExecutePriority {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u8(self.0)
    }
}

impl<'de> serde::Deserialize<'de> for PhoneExecutePriority {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = <u8 as serde::Deserialize>::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// The two execution domains understood by phone services.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PhoneActionKind {
    /// An HTTP or HTTPS action; only one is permitted per execute document.
    Http,
    /// A device-local action using a non-HTTP scheme.
    Internal,
}

/// A schema-bounded action in a phone execute document.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct PhoneExecuteUrl {
    value: String,
    kind: PhoneActionKind,
}

impl PhoneExecuteUrl {
    /// Validates the URL length and classifies its execution domain.
    pub fn new(value: impl Into<String>) -> Result<Self, PhoneXmlError> {
        let value = value.into();
        validate_optional_text(
            "phone execute URL",
            Some(&value),
            1,
            PHONE_XML_URL_MAX_CHARS,
        )?;
        let kind = action_kind(&value);
        Ok(Self { value, kind })
    }

    pub fn as_str(&self) -> &str {
        &self.value
    }

    pub const fn kind(&self) -> PhoneActionKind {
        self.kind
    }
}

impl Serialize for PhoneExecuteUrl {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.value)
    }
}

impl<'de> serde::Deserialize<'de> for PhoneExecuteUrl {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = <String as serde::Deserialize>::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// One ordered URL action in an execute document.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CiscoIpPhoneExecuteItem {
    #[serde(rename = "@Priority", default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<PhoneExecutePriority>,
    #[serde(rename = "@URL")]
    pub url: PhoneExecuteUrl,
}

impl CiscoIpPhoneExecuteItem {
    /// Creates a validated action with no explicit priority.
    pub fn new(url: impl Into<String>) -> Result<Self, PhoneXmlError> {
        Ok(Self {
            priority: None,
            url: PhoneExecuteUrl::new(url)?,
        })
    }

    /// Creates a validated action with an explicit scheduling priority.
    pub fn with_priority(
        url: impl Into<String>,
        priority: PhoneExecutePriority,
    ) -> Result<Self, PhoneXmlError> {
        Ok(Self {
            priority: Some(priority),
            url: PhoneExecuteUrl::new(url)?,
        })
    }
}

/// A complete, schema-ordered execute document.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename = "CiscoIPPhoneExecute", deny_unknown_fields)]
pub struct CiscoIpPhoneExecute {
    #[serde(rename = "ExecuteItem", default)]
    pub items: Vec<CiscoIpPhoneExecuteItem>,
}

impl CiscoIpPhoneExecute {
    /// Builds a document containing one to [`PHONE_EXECUTE_MAX_ITEMS`] actions.
    pub fn new(items: Vec<CiscoIpPhoneExecuteItem>) -> Result<Self, PhoneXmlError> {
        let document = Self { items };
        document.validate()?;
        Ok(document)
    }

    /// Checks action count and the single-HTTP-action invariant.
    pub fn validate(&self) -> Result<(), PhoneXmlError> {
        if self.items.is_empty() {
            return Err(PhoneXmlError::InvalidField {
                field: "phone execute actions",
                expected: "between 1 and 3 entries",
            });
        }
        validate_count(
            "phone execute actions",
            self.items.len(),
            PHONE_EXECUTE_MAX_ITEMS,
        )?;
        if self
            .items
            .iter()
            .filter(|item| item.url.kind() == PhoneActionKind::Http)
            .count()
            > 1
        {
            return Err(PhoneXmlError::InvalidField {
                field: "phone execute HTTP actions",
                expected: "at most one HTTP or HTTPS action",
            });
        }
        Ok(())
    }

    /// Parses a complete execute document using [`PHONE_EXECUTE_MAX_BYTES`].
    pub fn from_xml(document: &[u8]) -> Result<Self, PhoneXmlError> {
        <Self as PhoneXmlDocument>::parse_xml(document)
    }

    /// Parses an execute document with an additional caller-selected byte limit.
    pub fn from_xml_with_limit(
        document: &[u8],
        maximum_bytes: usize,
    ) -> Result<Self, PhoneXmlError> {
        <Self as PhoneXmlDocument>::parse_xml_with_limit(document, maximum_bytes)
    }

    /// Validates and serializes an execute document using [`PHONE_EXECUTE_MAX_BYTES`].
    pub fn to_xml(&self) -> Result<String, PhoneXmlError> {
        <Self as PhoneXmlDocument>::serialize_xml(self)
    }

    /// Validates and serializes an execute document within a caller-selected byte limit.
    pub fn to_xml_with_limit(&self, maximum_bytes: usize) -> Result<String, PhoneXmlError> {
        <Self as PhoneXmlDocument>::serialize_xml_with_limit(self, maximum_bytes)
    }
}

/// Validated hexadecimal bitmap data used by image-service documents.
///
/// The public value is binary. Serde converts it to and from the XML Schema
/// `hexBinary` lexical representation, accepting schema whitespace and either
/// letter case while emitting one stable uppercase representation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PhoneBitmapData(Vec<u8>);

impl PhoneBitmapData {
    /// Validates decoded bitmap bytes against [`PHONE_IMAGE_BITMAP_MAX_BYTES`].
    pub fn new(bytes: impl Into<Vec<u8>>) -> Result<Self, PhoneXmlError> {
        let bytes = bytes.into();
        validate_count(
            "bitmap image data bytes",
            bytes.len(),
            PHONE_IMAGE_BITMAP_MAX_BYTES,
        )?;
        Ok(Self(bytes))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

impl Serialize for PhoneBitmapData {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        const HEX: &[u8; 16] = b"0123456789ABCDEF";
        let mut encoded = String::with_capacity(self.0.len().saturating_mul(2));
        for byte in &self.0 {
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        serializer.serialize_str(&encoded)
    }
}

impl<'de> serde::Deserialize<'de> for PhoneBitmapData {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let encoded = <String as serde::Deserialize>::deserialize(deserializer)?;
        let digits = encoded
            .bytes()
            .filter(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
            .count();
        let digit_count = encoded.len().saturating_sub(digits);
        if digit_count % 2 != 0 {
            return Err(serde::de::Error::custom(
                "bitmap data must contain complete hexadecimal bytes",
            ));
        }
        let mut decoded = Vec::with_capacity(digit_count / 2);
        let mut high = None;
        for byte in encoded
            .bytes()
            .filter(|byte| !matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
        {
            let value = match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                b'A'..=b'F' => byte - b'A' + 10,
                _ => {
                    return Err(serde::de::Error::custom("bitmap data must be hexadecimal"));
                }
            };
            if let Some(high) = high.take() {
                decoded.push((high << 4) | value);
            } else {
                high = Some(value);
            }
        }
        Self::new(decoded).map_err(serde::de::Error::custom)
    }
}

/// A schema-bounded image resource URL.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct PhoneImageUrl(String);

impl PhoneImageUrl {
    /// Validates a non-empty image URL of at most 256 characters.
    pub fn new(value: impl Into<String>) -> Result<Self, PhoneXmlError> {
        let value = value.into();
        validate_optional_text("phone image URL", Some(&value), 1, PHONE_XML_URL_MAX_CHARS)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> serde::Deserialize<'de> for PhoneImageUrl {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = <String as serde::Deserialize>::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// A TFTP URI accepted in a background-image selection list.
///
/// The phone's selection-list schema uses the opaque `TFTP:path` form.  The
/// authority-bearing `tftp://host/path` form and HTTP URLs are deliberately
/// rejected because they are not interchangeable on the handset.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct PhoneBackgroundTftpUrl(String);

impl PhoneBackgroundTftpUrl {
    /// Validates an opaque `TFTP:path` URI naming a PNG resource.
    pub fn new(value: impl Into<String>) -> Result<Self, PhoneXmlError> {
        let value = value.into();
        validate_optional_text(
            "background image TFTP URI",
            Some(&value),
            1,
            PHONE_XML_URL_MAX_CHARS,
        )?;
        let parsed = url::Url::parse(&value).map_err(|_| PhoneXmlError::InvalidField {
            field: "background image TFTP URI",
            expected: "a TFTP:path URI to a PNG image",
        })?;
        let has_tftp_prefix = value
            .get(..5)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("tftp:"));
        let path = value.get(5..).unwrap_or_default();
        let is_png = path
            .rsplit_once('.')
            .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("png"));
        if !has_tftp_prefix
            || parsed.scheme() != "tftp"
            || !parsed.cannot_be_a_base()
            || parsed.host_str().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || !valid_background_tftp_path(path)
            || !is_png
        {
            return Err(PhoneXmlError::InvalidField {
                field: "background image TFTP URI",
                expected: "a TFTP:path URI to a PNG image",
            });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Debug for PhoneBackgroundTftpUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PhoneBackgroundTftpUrl(<redacted>)")
    }
}

impl Serialize for PhoneBackgroundTftpUrl {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for PhoneBackgroundTftpUrl {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

fn valid_background_tftp_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains(['?', '#', '\\'])
        && path.split('/').all(|component| {
            valid_percent_encoding(component)
                && percent_encoding::percent_decode_str(component)
                    .decode_utf8()
                    .is_ok_and(|decoded| {
                        !decoded.is_empty()
                            && decoded != "."
                            && decoded != ".."
                            && !decoded.contains(['/', '\\'])
                            && !decoded.chars().any(char::is_control)
                    })
        })
}

fn valid_percent_encoding(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit()
            {
                return false;
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    true
}

/// An HTTP URL accepted by the background selection and preview application.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct PhoneBackgroundHttpUrl(String);

impl PhoneBackgroundHttpUrl {
    /// Validates an absolute HTTP URL without credentials or a fragment.
    pub fn new(value: impl Into<String>) -> Result<Self, PhoneXmlError> {
        let value = value.into();
        validate_http_resource_url(
            "background image HTTP URL",
            "an absolute HTTP URL without credentials or a fragment",
            &value,
        )?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

fn validate_http_resource_url(
    field: &'static str,
    expected: &'static str,
    value: &str,
) -> Result<(), PhoneXmlError> {
    validate_optional_text(field, Some(value), 1, PHONE_XML_URL_MAX_CHARS)?;
    if value
        .chars()
        .any(|character| character.is_ascii_whitespace() || character.is_ascii_control())
        || value.contains('\\')
        || !valid_percent_encoding(value)
    {
        return Err(PhoneXmlError::InvalidField { field, expected });
    }
    let parsed =
        url::Url::parse(value).map_err(|_| PhoneXmlError::InvalidField { field, expected })?;
    if parsed.scheme() != "http"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.fragment().is_some()
    {
        return Err(PhoneXmlError::InvalidField { field, expected });
    }
    Ok(())
}

impl fmt::Debug for PhoneBackgroundHttpUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PhoneBackgroundHttpUrl(<redacted>)")
    }
}

impl Serialize for PhoneBackgroundHttpUrl {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for PhoneBackgroundHttpUrl {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// One ordered full-size/thumbnail pair in a background image list.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CiscoIpPhoneImageListItem {
    #[serde(rename = "@Image")]
    pub thumbnail_url: PhoneBackgroundTftpUrl,
    #[serde(rename = "@URL")]
    pub image_url: PhoneBackgroundTftpUrl,
}

/// The ordered background choices pulled from a desktop `List.xml` resource.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename = "CiscoIPPhoneImageList", deny_unknown_fields)]
pub struct CiscoIpPhoneImageList {
    #[serde(rename = "ImageItem", default)]
    pub items: Vec<CiscoIpPhoneImageListItem>,
}

impl CiscoIpPhoneImageList {
    /// Builds and validates an ordered list of background choices.
    pub fn new(items: Vec<CiscoIpPhoneImageListItem>) -> Result<Self, PhoneXmlError> {
        let document = Self { items };
        document.validate()?;
        Ok(document)
    }

    /// Enforces [`PHONE_BACKGROUND_LIST_MAX_ITEMS`].
    pub fn validate(&self) -> Result<(), PhoneXmlError> {
        validate_count(
            "background image choices",
            self.items.len(),
            PHONE_BACKGROUND_LIST_MAX_ITEMS,
        )
    }

    /// Parses and validates a background list using its default byte bound.
    pub fn from_xml(document: &[u8]) -> Result<Self, PhoneXmlError> {
        <Self as PhoneXmlDocument>::parse_xml(document)
    }

    /// Validates and serializes a background list using its default byte bound.
    pub fn to_xml(&self) -> Result<String, PhoneXmlError> {
        <Self as PhoneXmlDocument>::serialize_xml(self)
    }

    /// Validates and serializes a background list within a caller-selected limit.
    pub fn to_xml_with_limit(&self, maximum_bytes: usize) -> Result<String, PhoneXmlError> {
        <Self as PhoneXmlDocument>::serialize_xml_with_limit(self, maximum_bytes)
    }
}

/// Full-size and thumbnail URLs installed as the phone background.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CiscoIpPhoneBackground {
    #[serde(rename = "image")]
    pub image_url: PhoneBackgroundHttpUrl,
    #[serde(rename = "icon")]
    pub thumbnail_url: PhoneBackgroundHttpUrl,
}

/// Background-selection application document.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename = "setBackground", deny_unknown_fields)]
pub struct CiscoIpPhoneSetBackground {
    #[serde(rename = "background")]
    pub background: CiscoIpPhoneBackground,
}

impl CiscoIpPhoneSetBackground {
    /// Creates a background installation request from validated resource URLs.
    pub fn new(image_url: PhoneBackgroundHttpUrl, thumbnail_url: PhoneBackgroundHttpUrl) -> Self {
        Self {
            background: CiscoIpPhoneBackground {
                image_url,
                thumbnail_url,
            },
        }
    }

    /// Parses an installation request using [`PHONE_BACKGROUND_CONTROL_MAX_BYTES`].
    pub fn from_xml(document: &[u8]) -> Result<Self, PhoneXmlError> {
        #[derive(serde::Deserialize)]
        enum SetBackgroundEnvelope {
            #[serde(rename = "setBackground")]
            SetBackground(CiscoIpPhoneSetBackground),
        }

        let SetBackgroundEnvelope::SetBackground(document) =
            from_bytes(document, PHONE_BACKGROUND_CONTROL_MAX_BYTES)?;
        Ok(document)
    }

    /// Serializes an installation request using [`PHONE_BACKGROUND_CONTROL_MAX_BYTES`].
    pub fn to_xml(&self) -> Result<String, PhoneXmlError> {
        to_string(self, PHONE_BACKGROUND_CONTROL_MAX_BYTES)
    }
}

/// Background-preview application document.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename = "setBackgroundPreview", deny_unknown_fields)]
pub struct CiscoIpPhoneSetBackgroundPreview {
    #[serde(rename = "image")]
    pub image_url: PhoneBackgroundHttpUrl,
}

impl CiscoIpPhoneSetBackgroundPreview {
    /// Creates a preview request from a validated image URL.
    pub const fn new(image_url: PhoneBackgroundHttpUrl) -> Self {
        Self { image_url }
    }

    /// Parses a preview request using [`PHONE_BACKGROUND_CONTROL_MAX_BYTES`].
    pub fn from_xml(document: &[u8]) -> Result<Self, PhoneXmlError> {
        #[derive(serde::Deserialize)]
        enum PreviewEnvelope {
            #[serde(rename = "setBackgroundPreview")]
            Preview(CiscoIpPhoneSetBackgroundPreview),
        }

        let PreviewEnvelope::Preview(document) =
            from_bytes(document, PHONE_BACKGROUND_CONTROL_MAX_BYTES)?;
        Ok(document)
    }

    /// Serializes a preview request using [`PHONE_BACKGROUND_CONTROL_MAX_BYTES`].
    pub fn to_xml(&self) -> Result<String, PhoneXmlError> {
        to_string(self, PHONE_BACKGROUND_CONTROL_MAX_BYTES)
    }
}

/// Either application document accepted by the background-control service.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PhoneBackgroundControlDocument {
    Set(CiscoIpPhoneSetBackground),
    Preview(CiscoIpPhoneSetBackgroundPreview),
}

impl PhoneBackgroundControlDocument {
    /// Detects and parses either supported background-control root.
    pub fn from_xml(document: &[u8]) -> Result<Self, PhoneXmlError> {
        #[derive(serde::Deserialize)]
        enum BackgroundEnvelope {
            #[serde(rename = "setBackground")]
            Set(CiscoIpPhoneSetBackground),
            #[serde(rename = "setBackgroundPreview")]
            Preview(CiscoIpPhoneSetBackgroundPreview),
        }

        Ok(
            match from_bytes(document, PHONE_BACKGROUND_CONTROL_MAX_BYTES)? {
                BackgroundEnvelope::Set(document) => Self::Set(document),
                BackgroundEnvelope::Preview(document) => Self::Preview(document),
            },
        )
    }

    /// Serializes the selected document using the default control byte bound.
    pub fn to_xml(&self) -> Result<String, PhoneXmlError> {
        self.to_xml_with_limit(PHONE_BACKGROUND_CONTROL_MAX_BYTES)
    }

    /// Serializes the selected document within a caller-selected byte limit.
    pub fn to_xml_with_limit(&self, maximum_bytes: usize) -> Result<String, PhoneXmlError> {
        match self {
            Self::Set(document) => to_string(document, maximum_bytes),
            Self::Preview(document) => to_string(document, maximum_bytes),
        }
    }
}

/// An HTTP resource URL accepted by the ringtone-selection application.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct PhoneRingtoneUrl(String);

impl PhoneRingtoneUrl {
    /// Validates an absolute lowercase-scheme HTTP URL without credentials or a fragment.
    pub fn new(value: impl Into<String>) -> Result<Self, PhoneXmlError> {
        let value = value.into();
        if !value.starts_with("http://") {
            return Err(PhoneXmlError::InvalidField {
                field: "ringtone HTTP URL",
                expected: "an absolute lowercase HTTP URL without credentials or a fragment",
            });
        }
        validate_http_resource_url(
            "ringtone HTTP URL",
            "an absolute lowercase HTTP URL without credentials or a fragment",
            &value,
        )?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Debug for PhoneRingtoneUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PhoneRingtoneUrl(<redacted>)")
    }
}

impl Serialize for PhoneRingtoneUrl {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for PhoneRingtoneUrl {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// Ringtone-selection application document.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename = "setRingTone", deny_unknown_fields)]
pub struct CiscoIpPhoneSetRingTone {
    #[serde(rename = "ringTone")]
    pub ringtone_url: PhoneRingtoneUrl,
}

impl CiscoIpPhoneSetRingTone {
    /// Creates a ringtone request from a validated HTTP resource URL.
    pub const fn new(ringtone_url: PhoneRingtoneUrl) -> Self {
        Self { ringtone_url }
    }

    /// Parses a ringtone request using [`PHONE_RINGTONE_MAX_BYTES`].
    pub fn from_xml(document: &[u8]) -> Result<Self, PhoneXmlError> {
        #[derive(serde::Deserialize)]
        enum RingToneEnvelope {
            #[serde(rename = "setRingTone")]
            RingTone(CiscoIpPhoneSetRingTone),
        }

        let RingToneEnvelope::RingTone(document) = from_bytes(document, PHONE_RINGTONE_MAX_BYTES)?;
        Ok(document)
    }

    /// Serializes a ringtone request using [`PHONE_RINGTONE_MAX_BYTES`].
    pub fn to_xml(&self) -> Result<String, PhoneXmlError> {
        self.to_xml_with_limit(PHONE_RINGTONE_MAX_BYTES)
    }

    /// Serializes a ringtone request within a caller-selected byte limit.
    pub fn to_xml_with_limit(&self, maximum_bytes: usize) -> Result<String, PhoneXmlError> {
        to_string(self, maximum_bytes)
    }
}

/// A rectangular selection region in a graphic-file menu.
#[derive(Clone, Copy, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PhoneTouchArea {
    #[serde(rename = "@X1")]
    pub x1: u16,
    #[serde(rename = "@Y1")]
    pub y1: u16,
    #[serde(rename = "@X2")]
    pub x2: u16,
    #[serde(rename = "@Y2")]
    pub y2: u16,
}

/// One optional label, action, and selection region in a graphic-file menu.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CiscoIpPhoneTouchAreaMenuItem {
    #[serde(rename = "Name", default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(rename = "URL", default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(rename = "TouchArea", default, skip_serializing_if = "Option::is_none")]
    pub touch_area: Option<PhoneTouchArea>,
}

/// A complete inline-bitmap image document.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename = "CiscoIPPhoneImage", deny_unknown_fields)]
pub struct CiscoIpPhoneImage {
    #[serde(
        rename = "@keypadTarget",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub keypad_target: Option<PhoneKeypadTarget>,
    #[serde(rename = "@appId", default, skip_serializing_if = "Option::is_none")]
    pub application_id: Option<String>,
    #[serde(
        rename = "@onAppFocusLost",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_focus_lost: Option<String>,
    #[serde(
        rename = "@onAppFocusGained",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_focus_gained: Option<String>,
    #[serde(
        rename = "@onAppMinimized",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_minimized: Option<String>,
    #[serde(
        rename = "@onAppClosed",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_closed: Option<String>,
    #[serde(rename = "Title", default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(rename = "Prompt", default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(rename = "SoftKeyItem", default)]
    pub soft_keys: Vec<CiscoIpPhoneSoftKeyItem>,
    #[serde(rename = "KeyItem", default)]
    pub key_items: Vec<CiscoIpPhoneKeyItem>,
    #[serde(rename = "LocationX", default, skip_serializing_if = "Option::is_none")]
    /// Horizontal origin in `-1..=132`; `-1` requests automatic placement.
    pub location_x: Option<i16>,
    #[serde(rename = "LocationY", default, skip_serializing_if = "Option::is_none")]
    /// Vertical origin in `-1..=64`; `-1` requests automatic placement.
    pub location_y: Option<i16>,
    #[serde(rename = "Width")]
    /// Bitmap width in pixels, constrained to `1..=133`.
    pub width: u16,
    #[serde(rename = "Height")]
    /// Bitmap height in pixels, constrained to `1..=65`.
    pub height: u16,
    #[serde(rename = "Depth")]
    /// Bitmap bit depth, constrained to `1..=2`.
    pub depth: u16,
    #[serde(rename = "Data", default, skip_serializing_if = "Option::is_none")]
    pub data: Option<PhoneBitmapData>,
}

impl CiscoIpPhoneImage {
    /// Validates display metadata, pixel geometry, and decoded bitmap size.
    pub fn validate(&self) -> Result<(), PhoneXmlError> {
        validate_image_display(
            self.title.as_deref(),
            self.prompt.as_deref(),
            self.application_id.as_deref(),
            [
                self.on_focus_lost.as_deref(),
                self.on_focus_gained.as_deref(),
                self.on_minimized.as_deref(),
                self.on_closed.as_deref(),
            ],
            &self.soft_keys,
            &self.key_items,
        )?;
        validate_bitmap_image(
            self.location_x,
            self.location_y,
            self.width,
            self.height,
            self.depth,
            self.data.as_ref(),
        )
    }

    /// Detects, parses, and validates an image root within [`PHONE_IMAGE_MAX_BYTES`].
    pub fn from_xml(document: &[u8]) -> Result<Self, PhoneXmlError> {
        #[derive(serde::Deserialize)]
        enum ImageEnvelope {
            #[serde(rename = "CiscoIPPhoneImage")]
            Image(CiscoIpPhoneImage),
        }
        let ImageEnvelope::Image(document) = from_bytes(document, PHONE_IMAGE_MAX_BYTES)?;
        document.validate()?;
        Ok(document)
    }

    /// Validates and serializes the image within [`PHONE_IMAGE_MAX_BYTES`].
    pub fn to_xml(&self) -> Result<String, PhoneXmlError> {
        self.validate()?;
        to_string(self, PHONE_IMAGE_MAX_BYTES)
    }
}

/// A complete URL-backed image document.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename = "CiscoIPPhoneImageFile", deny_unknown_fields)]
pub struct CiscoIpPhoneImageFile {
    #[serde(
        rename = "@keypadTarget",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub keypad_target: Option<PhoneKeypadTarget>,
    #[serde(rename = "@appId", default, skip_serializing_if = "Option::is_none")]
    pub application_id: Option<String>,
    #[serde(
        rename = "@onAppFocusLost",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_focus_lost: Option<String>,
    #[serde(
        rename = "@onAppFocusGained",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_focus_gained: Option<String>,
    #[serde(
        rename = "@onAppMinimized",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_minimized: Option<String>,
    #[serde(
        rename = "@onAppClosed",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_closed: Option<String>,
    #[serde(rename = "Title", default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(rename = "Prompt", default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(rename = "SoftKeyItem", default)]
    pub soft_keys: Vec<CiscoIpPhoneSoftKeyItem>,
    #[serde(rename = "KeyItem", default)]
    pub key_items: Vec<CiscoIpPhoneKeyItem>,
    #[serde(rename = "LocationX", default, skip_serializing_if = "Option::is_none")]
    /// Horizontal origin in `-1..=297`; `-1` requests automatic placement.
    pub location_x: Option<i16>,
    #[serde(rename = "LocationY", default, skip_serializing_if = "Option::is_none")]
    /// Vertical origin in `-1..=167`; `-1` requests automatic placement.
    pub location_y: Option<i16>,
    #[serde(rename = "URL")]
    pub url: PhoneImageUrl,
}

impl CiscoIpPhoneImageFile {
    /// Validates display metadata and the optional image origin.
    pub fn validate(&self) -> Result<(), PhoneXmlError> {
        validate_image_display(
            self.title.as_deref(),
            self.prompt.as_deref(),
            self.application_id.as_deref(),
            [
                self.on_focus_lost.as_deref(),
                self.on_focus_gained.as_deref(),
                self.on_minimized.as_deref(),
                self.on_closed.as_deref(),
            ],
            &self.soft_keys,
            &self.key_items,
        )?;
        validate_file_image_location(self.location_x, self.location_y)
    }

    /// Detects, parses, and validates an image-file root within [`PHONE_IMAGE_MAX_BYTES`].
    pub fn from_xml(document: &[u8]) -> Result<Self, PhoneXmlError> {
        #[derive(serde::Deserialize)]
        enum ImageFileEnvelope {
            #[serde(rename = "CiscoIPPhoneImageFile")]
            ImageFile(CiscoIpPhoneImageFile),
        }
        let ImageFileEnvelope::ImageFile(document) = from_bytes(document, PHONE_IMAGE_MAX_BYTES)?;
        document.validate()?;
        Ok(document)
    }

    /// Validates and serializes the image-file document within its default bound.
    pub fn to_xml(&self) -> Result<String, PhoneXmlError> {
        self.validate()?;
        to_string(self, PHONE_IMAGE_MAX_BYTES)
    }
}

/// A complete inline-bitmap graphic menu document.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename = "CiscoIPPhoneGraphicMenu", deny_unknown_fields)]
pub struct CiscoIpPhoneGraphicMenu {
    #[serde(
        rename = "@keypadTarget",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub keypad_target: Option<PhoneKeypadTarget>,
    #[serde(rename = "@appId", default, skip_serializing_if = "Option::is_none")]
    pub application_id: Option<String>,
    #[serde(
        rename = "@onAppFocusLost",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_focus_lost: Option<String>,
    #[serde(
        rename = "@onAppFocusGained",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_focus_gained: Option<String>,
    #[serde(
        rename = "@onAppMinimized",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_minimized: Option<String>,
    #[serde(
        rename = "@onAppClosed",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_closed: Option<String>,
    #[serde(rename = "Title", default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(rename = "Prompt", default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(rename = "SoftKeyItem", default)]
    pub soft_keys: Vec<CiscoIpPhoneSoftKeyItem>,
    #[serde(rename = "KeyItem", default)]
    pub key_items: Vec<CiscoIpPhoneKeyItem>,
    #[serde(rename = "LocationX", default, skip_serializing_if = "Option::is_none")]
    /// Horizontal origin in `-1..=132`; `-1` requests automatic placement.
    pub location_x: Option<i16>,
    #[serde(rename = "LocationY", default, skip_serializing_if = "Option::is_none")]
    /// Vertical origin in `-1..=64`; `-1` requests automatic placement.
    pub location_y: Option<i16>,
    #[serde(rename = "Width")]
    /// Bitmap width in pixels, constrained to `1..=133`.
    pub width: u16,
    #[serde(rename = "Height")]
    /// Bitmap height in pixels, constrained to `1..=65`.
    pub height: u16,
    #[serde(rename = "Depth")]
    /// Bitmap bit depth, constrained to `1..=2`.
    pub depth: u16,
    #[serde(rename = "Data", default, skip_serializing_if = "Option::is_none")]
    pub data: Option<PhoneBitmapData>,
    #[serde(rename = "MenuItem", default)]
    pub items: Vec<CiscoIpPhoneMenuItem>,
}

impl CiscoIpPhoneGraphicMenu {
    /// Validates display metadata, bitmap geometry, and selectable-item bounds.
    pub fn validate(&self) -> Result<(), PhoneXmlError> {
        validate_image_display(
            self.title.as_deref(),
            self.prompt.as_deref(),
            self.application_id.as_deref(),
            [
                self.on_focus_lost.as_deref(),
                self.on_focus_gained.as_deref(),
                self.on_minimized.as_deref(),
                self.on_closed.as_deref(),
            ],
            &self.soft_keys,
            &self.key_items,
        )?;
        validate_bitmap_image(
            self.location_x,
            self.location_y,
            self.width,
            self.height,
            self.depth,
            self.data.as_ref(),
        )?;
        validate_count(
            "graphic menu items",
            self.items.len(),
            PHONE_GRAPHIC_MENU_MAX_ITEMS,
        )?;
        for item in &self.items {
            validate_optional_text("graphic menu item name", item.name.as_deref(), 0, 64)?;
            validate_optional_text(
                "graphic menu item URL",
                item.url.as_deref(),
                0,
                PHONE_XML_URL_MAX_CHARS,
            )?;
        }
        Ok(())
    }

    /// Detects, parses, and validates a graphic menu within [`PHONE_IMAGE_MAX_BYTES`].
    pub fn from_xml(document: &[u8]) -> Result<Self, PhoneXmlError> {
        #[derive(serde::Deserialize)]
        enum GraphicMenuEnvelope {
            #[serde(rename = "CiscoIPPhoneGraphicMenu")]
            GraphicMenu(CiscoIpPhoneGraphicMenu),
        }
        let GraphicMenuEnvelope::GraphicMenu(document) =
            from_bytes(document, PHONE_IMAGE_MAX_BYTES)?;
        document.validate()?;
        Ok(document)
    }

    /// Validates and serializes the graphic menu within its default bound.
    pub fn to_xml(&self) -> Result<String, PhoneXmlError> {
        self.validate()?;
        to_string(self, PHONE_IMAGE_MAX_BYTES)
    }
}

/// A complete URL-backed graphic menu document.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename = "CiscoIPPhoneGraphicFileMenu", deny_unknown_fields)]
pub struct CiscoIpPhoneGraphicFileMenu {
    #[serde(
        rename = "@keypadTarget",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub keypad_target: Option<PhoneKeypadTarget>,
    #[serde(rename = "@appId", default, skip_serializing_if = "Option::is_none")]
    pub application_id: Option<String>,
    #[serde(
        rename = "@onAppFocusLost",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_focus_lost: Option<String>,
    #[serde(
        rename = "@onAppFocusGained",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_focus_gained: Option<String>,
    #[serde(
        rename = "@onAppMinimized",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_minimized: Option<String>,
    #[serde(
        rename = "@onAppClosed",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_closed: Option<String>,
    #[serde(rename = "Title", default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(rename = "Prompt", default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(rename = "SoftKeyItem", default)]
    pub soft_keys: Vec<CiscoIpPhoneSoftKeyItem>,
    #[serde(rename = "KeyItem", default)]
    pub key_items: Vec<CiscoIpPhoneKeyItem>,
    #[serde(rename = "LocationX", default, skip_serializing_if = "Option::is_none")]
    /// Horizontal origin in `-1..=297`; `-1` requests automatic placement.
    pub location_x: Option<i16>,
    #[serde(rename = "LocationY", default, skip_serializing_if = "Option::is_none")]
    /// Vertical origin in `-1..=167`; `-1` requests automatic placement.
    pub location_y: Option<i16>,
    #[serde(rename = "URL")]
    pub url: PhoneImageUrl,
    #[serde(rename = "MenuItem", default)]
    pub items: Vec<CiscoIpPhoneTouchAreaMenuItem>,
}

impl CiscoIpPhoneGraphicFileMenu {
    /// Validates display metadata, image origin, and touch-area item bounds.
    pub fn validate(&self) -> Result<(), PhoneXmlError> {
        validate_image_display(
            self.title.as_deref(),
            self.prompt.as_deref(),
            self.application_id.as_deref(),
            [
                self.on_focus_lost.as_deref(),
                self.on_focus_gained.as_deref(),
                self.on_minimized.as_deref(),
                self.on_closed.as_deref(),
            ],
            &self.soft_keys,
            &self.key_items,
        )?;
        validate_file_image_location(self.location_x, self.location_y)?;
        validate_count(
            "graphic-file menu items",
            self.items.len(),
            PHONE_GRAPHIC_FILE_MENU_MAX_ITEMS,
        )?;
        for item in &self.items {
            validate_optional_text("graphic-file menu item name", item.name.as_deref(), 0, 32)?;
            validate_optional_text(
                "graphic-file menu item URL",
                item.url.as_deref(),
                0,
                PHONE_XML_URL_MAX_CHARS,
            )?;
        }
        Ok(())
    }

    /// Detects, parses, and validates a graphic-file menu within its default bound.
    pub fn from_xml(document: &[u8]) -> Result<Self, PhoneXmlError> {
        #[derive(serde::Deserialize)]
        enum GraphicFileMenuEnvelope {
            #[serde(rename = "CiscoIPPhoneGraphicFileMenu")]
            GraphicFileMenu(CiscoIpPhoneGraphicFileMenu),
        }
        let GraphicFileMenuEnvelope::GraphicFileMenu(document) =
            from_bytes(document, PHONE_IMAGE_MAX_BYTES)?;
        document.validate()?;
        Ok(document)
    }

    /// Validates and serializes the graphic-file menu within its default bound.
    pub fn to_xml(&self) -> Result<String, PhoneXmlError> {
        self.validate()?;
        to_string(self, PHONE_IMAGE_MAX_BYTES)
    }
}

/// Any accepted image-service document family.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PhoneImageDocument {
    Image(CiscoIpPhoneImage),
    ImageFile(CiscoIpPhoneImageFile),
    GraphicMenu(CiscoIpPhoneGraphicMenu),
    GraphicFileMenu(CiscoIpPhoneGraphicFileMenu),
}

impl PhoneImageDocument {
    /// Applies the invariants for the selected image family.
    pub fn validate(&self) -> Result<(), PhoneXmlError> {
        match self {
            Self::Image(document) => document.validate(),
            Self::ImageFile(document) => document.validate(),
            Self::GraphicMenu(document) => document.validate(),
            Self::GraphicFileMenu(document) => document.validate(),
        }
    }

    /// Detects the root and parses any supported image family.
    pub fn from_xml(document: &[u8]) -> Result<Self, PhoneXmlError> {
        #[derive(serde::Deserialize)]
        enum ImageDocumentEnvelope {
            #[serde(rename = "CiscoIPPhoneImage")]
            Image(CiscoIpPhoneImage),
            #[serde(rename = "CiscoIPPhoneImageFile")]
            ImageFile(CiscoIpPhoneImageFile),
            #[serde(rename = "CiscoIPPhoneGraphicMenu")]
            GraphicMenu(CiscoIpPhoneGraphicMenu),
            #[serde(rename = "CiscoIPPhoneGraphicFileMenu")]
            GraphicFileMenu(CiscoIpPhoneGraphicFileMenu),
        }
        let document = match from_bytes(document, PHONE_IMAGE_MAX_BYTES)? {
            ImageDocumentEnvelope::Image(document) => Self::Image(document),
            ImageDocumentEnvelope::ImageFile(document) => Self::ImageFile(document),
            ImageDocumentEnvelope::GraphicMenu(document) => Self::GraphicMenu(document),
            ImageDocumentEnvelope::GraphicFileMenu(document) => Self::GraphicFileMenu(document),
        };
        document.validate()?;
        Ok(document)
    }

    /// Validates and serializes the selected family using [`PHONE_IMAGE_MAX_BYTES`].
    pub fn to_xml(&self) -> Result<String, PhoneXmlError> {
        self.to_xml_with_limit(PHONE_IMAGE_MAX_BYTES)
    }

    /// Validates and serializes the selected family within a caller-selected limit.
    pub fn to_xml_with_limit(&self, maximum_bytes: usize) -> Result<String, PhoneXmlError> {
        self.validate()?;
        match self {
            Self::Image(document) => to_string(document, maximum_bytes),
            Self::ImageFile(document) => to_string(document, maximum_bytes),
            Self::GraphicMenu(document) => to_string(document, maximum_bytes),
            Self::GraphicFileMenu(document) => to_string(document, maximum_bytes),
        }
    }
}

/// A bitmap-backed status item displayed in the phone's status area.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename = "CiscoIPPhoneStatus", deny_unknown_fields)]
pub struct CiscoIpPhoneStatus {
    #[serde(rename = "Text", default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(rename = "Timer", default, skip_serializing_if = "Option::is_none")]
    /// Duration in seconds; omission leaves display lifetime device-defined.
    pub timer_seconds: Option<u16>,
    #[serde(rename = "LocationX", default, skip_serializing_if = "Option::is_none")]
    /// Horizontal origin in `-1..=105`; `-1` requests automatic placement.
    pub location_x: Option<i16>,
    #[serde(rename = "LocationY", default, skip_serializing_if = "Option::is_none")]
    /// Vertical origin in `-1..=20`; `-1` requests automatic placement.
    pub location_y: Option<i16>,
    #[serde(rename = "Width")]
    /// Bitmap width in pixels, constrained to `1..=106`.
    pub width: u16,
    #[serde(rename = "Height")]
    /// Bitmap height in pixels, constrained to `1..=21`.
    pub height: u16,
    #[serde(rename = "Depth")]
    /// Bitmap bit depth, constrained to `1..=2`.
    pub depth: u16,
    #[serde(rename = "Data", default, skip_serializing_if = "Option::is_none")]
    pub data: Option<PhoneBitmapData>,
}

impl CiscoIpPhoneStatus {
    /// Validates text, display geometry, and the status-specific bitmap bound.
    pub fn validate(&self) -> Result<(), PhoneXmlError> {
        validate_optional_text("phone status text", self.text.as_deref(), 0, 32)?;
        if self
            .location_x
            .is_some_and(|value| !(-1..=105).contains(&value))
        {
            return Err(PhoneXmlError::InvalidField {
                field: "phone status horizontal location",
                expected: "between -1 and 105",
            });
        }
        if self
            .location_y
            .is_some_and(|value| !(-1..=20).contains(&value))
        {
            return Err(PhoneXmlError::InvalidField {
                field: "phone status vertical location",
                expected: "between -1 and 20",
            });
        }
        if !(1..=106).contains(&self.width) {
            return Err(PhoneXmlError::InvalidField {
                field: "phone status width",
                expected: "between 1 and 106",
            });
        }
        if !(1..=21).contains(&self.height) {
            return Err(PhoneXmlError::InvalidField {
                field: "phone status height",
                expected: "between 1 and 21",
            });
        }
        if !(1..=2).contains(&self.depth) {
            return Err(PhoneXmlError::InvalidField {
                field: "phone status depth",
                expected: "between 1 and 2",
            });
        }
        if let Some(data) = &self.data {
            validate_count(
                "phone status bitmap bytes",
                data.as_bytes().len(),
                PHONE_STATUS_BITMAP_MAX_BYTES,
            )?;
        }
        Ok(())
    }

    /// Parses and validates an inline status item using [`PHONE_STATUS_MAX_BYTES`].
    pub fn from_xml(document: &[u8]) -> Result<Self, PhoneXmlError> {
        <Self as PhoneXmlDocument>::parse_xml(document)
    }

    /// Parses an inline status item with an additional caller-selected byte limit.
    pub fn from_xml_with_limit(
        document: &[u8],
        maximum_bytes: usize,
    ) -> Result<Self, PhoneXmlError> {
        <Self as PhoneXmlDocument>::parse_xml_with_limit(document, maximum_bytes)
    }

    /// Validates and serializes the status item using [`PHONE_STATUS_MAX_BYTES`].
    pub fn to_xml(&self) -> Result<String, PhoneXmlError> {
        <Self as PhoneXmlDocument>::serialize_xml(self)
    }

    /// Validates and serializes the status item within a caller-selected limit.
    pub fn to_xml_with_limit(&self, maximum_bytes: usize) -> Result<String, PhoneXmlError> {
        <Self as PhoneXmlDocument>::serialize_xml_with_limit(self, maximum_bytes)
    }
}

/// A URL-backed status item displayed in the phone's status area.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename = "CiscoIPPhoneStatusFile", deny_unknown_fields)]
pub struct CiscoIpPhoneStatusFile {
    #[serde(rename = "Text", default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(rename = "Timer", default, skip_serializing_if = "Option::is_none")]
    /// Duration in seconds; omission leaves display lifetime device-defined.
    pub timer_seconds: Option<u16>,
    #[serde(rename = "LocationX", default, skip_serializing_if = "Option::is_none")]
    /// Horizontal origin in `-1..=261`; `-1` requests automatic placement.
    pub location_x: Option<i16>,
    #[serde(rename = "LocationY", default, skip_serializing_if = "Option::is_none")]
    /// Vertical origin in `-1..=49`; `-1` requests automatic placement.
    pub location_y: Option<i16>,
    #[serde(rename = "URL")]
    pub url: PhoneImageUrl,
}

impl CiscoIpPhoneStatusFile {
    /// Validates text and the optional referenced-image origin.
    pub fn validate(&self) -> Result<(), PhoneXmlError> {
        validate_optional_text("phone status text", self.text.as_deref(), 0, 32)?;
        if self
            .location_x
            .is_some_and(|value| !(-1..=261).contains(&value))
        {
            return Err(PhoneXmlError::InvalidField {
                field: "phone status-file horizontal location",
                expected: "between -1 and 261",
            });
        }
        if self
            .location_y
            .is_some_and(|value| !(-1..=49).contains(&value))
        {
            return Err(PhoneXmlError::InvalidField {
                field: "phone status-file vertical location",
                expected: "between -1 and 49",
            });
        }
        Ok(())
    }

    /// Parses and validates a referenced status item using its default byte bound.
    pub fn from_xml(document: &[u8]) -> Result<Self, PhoneXmlError> {
        <Self as PhoneXmlDocument>::parse_xml(document)
    }

    /// Parses a referenced status item with an additional caller-selected limit.
    pub fn from_xml_with_limit(
        document: &[u8],
        maximum_bytes: usize,
    ) -> Result<Self, PhoneXmlError> {
        <Self as PhoneXmlDocument>::parse_xml_with_limit(document, maximum_bytes)
    }

    /// Validates and serializes the status item using [`PHONE_STATUS_MAX_BYTES`].
    pub fn to_xml(&self) -> Result<String, PhoneXmlError> {
        <Self as PhoneXmlDocument>::serialize_xml(self)
    }

    /// Validates and serializes the status item within a caller-selected limit.
    pub fn to_xml_with_limit(&self, maximum_bytes: usize) -> Result<String, PhoneXmlError> {
        <Self as PhoneXmlDocument>::serialize_xml_with_limit(self, maximum_bytes)
    }
}

/// Either accepted status-service document family.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PhoneStatusDocument {
    Bitmap(CiscoIpPhoneStatus),
    File(CiscoIpPhoneStatusFile),
}

impl PhoneStatusDocument {
    /// Applies the invariants for the selected status family.
    pub fn validate(&self) -> Result<(), PhoneXmlError> {
        match self {
            Self::Bitmap(document) => document.validate(),
            Self::File(document) => document.validate(),
        }
    }

    /// Detects the root and parses either supported status family.
    pub fn from_xml(document: &[u8]) -> Result<Self, PhoneXmlError> {
        #[derive(serde::Deserialize)]
        enum StatusDocumentEnvelope {
            #[serde(rename = "CiscoIPPhoneStatus")]
            Bitmap(CiscoIpPhoneStatus),
            #[serde(rename = "CiscoIPPhoneStatusFile")]
            File(CiscoIpPhoneStatusFile),
        }
        let document = match from_bytes(document, PHONE_STATUS_MAX_BYTES)? {
            StatusDocumentEnvelope::Bitmap(document) => Self::Bitmap(document),
            StatusDocumentEnvelope::File(document) => Self::File(document),
        };
        document.validate()?;
        Ok(document)
    }

    /// Validates and serializes the selected family using [`PHONE_STATUS_MAX_BYTES`].
    pub fn to_xml(&self) -> Result<String, PhoneXmlError> {
        self.to_xml_with_limit(PHONE_STATUS_MAX_BYTES)
    }

    /// Validates and serializes the selected family within a caller-selected limit.
    pub fn to_xml_with_limit(&self, maximum_bytes: usize) -> Result<String, PhoneXmlError> {
        self.validate()?;
        match self {
            Self::Bitmap(document) => to_string(document, maximum_bytes),
            Self::File(document) => to_string(document, maximum_bytes),
        }
    }
}

const LAST_OUT_OF_SERVICE_ALARM: &str = "LastOutOfServiceInformation";

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

fn redact_alarm_schema_error(error: PhoneXmlError) -> PhoneXmlError {
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

fn parse_bssid(value: &str) -> Option<PhoneBssid> {
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

fn redact_location_schema_error(error: PhoneXmlError) -> PhoneXmlError {
    match error {
        PhoneXmlError::Deserialize(_) => PhoneXmlError::InvalidLocationSchema,
        error => error,
    }
}

/// A directory entry containing an optional display name and dialable value.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CiscoIpPhoneDirectoryEntry {
    #[serde(rename = "Name", default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(rename = "Telephone", default, skip_serializing_if = "Option::is_none")]
    pub telephone: Option<String>,
}

/// A complete, schema-ordered phone directory response.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename = "CiscoIPPhoneDirectory", deny_unknown_fields)]
pub struct CiscoIpPhoneDirectory {
    #[serde(
        rename = "@keypadTarget",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub keypad_target: Option<PhoneKeypadTarget>,
    #[serde(rename = "@appId", default, skip_serializing_if = "Option::is_none")]
    pub application_id: Option<String>,
    #[serde(
        rename = "@onAppFocusLost",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_focus_lost: Option<String>,
    #[serde(
        rename = "@onAppFocusGained",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_focus_gained: Option<String>,
    #[serde(
        rename = "@onAppMinimized",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_minimized: Option<String>,
    #[serde(
        rename = "@onAppClosed",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_closed: Option<String>,
    #[serde(rename = "Title", default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(rename = "Prompt", default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(rename = "SoftKeyItem", default)]
    pub soft_keys: Vec<CiscoIpPhoneSoftKeyItem>,
    #[serde(rename = "KeyItem", default)]
    pub key_items: Vec<CiscoIpPhoneKeyItem>,
    #[serde(rename = "DirectoryEntry", default)]
    pub entries: Vec<CiscoIpPhoneDirectoryEntry>,
}

impl CiscoIpPhoneDirectory {
    /// Builds and validates a directory with no optional lifecycle actions.
    pub fn new(
        title: impl Into<String>,
        prompt: impl Into<String>,
        entries: Vec<CiscoIpPhoneDirectoryEntry>,
    ) -> Result<Self, PhoneXmlError> {
        let document = Self {
            keypad_target: None,
            application_id: None,
            on_focus_lost: None,
            on_focus_gained: None,
            on_minimized: None,
            on_closed: None,
            title: Some(title.into()),
            prompt: Some(prompt.into()),
            soft_keys: Vec::new(),
            key_items: Vec::new(),
            entries,
        };
        document.validate()?;
        Ok(document)
    }

    /// Parses and validates a directory using [`PHONE_DIRECTORY_MAX_BYTES`].
    pub fn from_xml(document: &[u8]) -> Result<Self, PhoneXmlError> {
        <Self as PhoneXmlDocument>::parse_xml(document)
    }

    /// Validates and serializes a directory using [`PHONE_DIRECTORY_MAX_BYTES`].
    pub fn to_xml(&self) -> Result<String, PhoneXmlError> {
        <Self as PhoneXmlDocument>::serialize_xml(self)
    }

    /// Checks entry counts, text bounds, lifecycle actions, and key bindings.
    pub fn validate(&self) -> Result<(), PhoneXmlError> {
        validate_count(
            "directory entries",
            self.entries.len(),
            PHONE_DIRECTORY_MAX_ENTRIES,
        )?;
        validate_count("directory soft keys", self.soft_keys.len(), 16)?;
        validate_count("directory key items", self.key_items.len(), 32)?;
        validate_optional_text("directory title", self.title.as_deref(), 0, 32)?;
        validate_optional_text("directory prompt", self.prompt.as_deref(), 0, 32)?;
        validate_optional_text(
            "directory application id",
            self.application_id.as_deref(),
            1,
            64,
        )?;
        for value in [
            self.on_focus_lost.as_deref(),
            self.on_focus_gained.as_deref(),
            self.on_minimized.as_deref(),
            self.on_closed.as_deref(),
        ] {
            validate_optional_text("directory lifecycle URL", value, 1, PHONE_XML_URL_MAX_CHARS)?;
        }
        validate_internal_action("directory onAppClosed action", self.on_closed.as_deref())?;
        for entry in &self.entries {
            validate_optional_text(
                "directory entry name",
                entry.name.as_deref(),
                0,
                PHONE_DIRECTORY_TEXT_MAX_CHARS,
            )?;
            validate_optional_text(
                "directory entry telephone",
                entry.telephone.as_deref(),
                0,
                PHONE_DIRECTORY_TEXT_MAX_CHARS,
            )?;
        }
        for soft_key in &self.soft_keys {
            validate_optional_text("directory soft-key name", soft_key.name.as_deref(), 0, 32)?;
            validate_optional_text(
                "directory soft-key URL",
                soft_key.url.as_deref(),
                0,
                PHONE_XML_URL_MAX_CHARS,
            )?;
            validate_optional_text(
                "directory soft-key down URL",
                soft_key.url_down.as_deref(),
                0,
                PHONE_XML_URL_MAX_CHARS,
            )?;
            validate_internal_action("directory soft-key URLDown", soft_key.url_down.as_deref())?;
        }
        for key_item in &self.key_items {
            validate_optional_text(
                "directory key URL",
                key_item.url.as_deref(),
                0,
                PHONE_XML_URL_MAX_CHARS,
            )?;
            validate_optional_text(
                "directory key down URL",
                key_item.url_down.as_deref(),
                0,
                PHONE_XML_URL_MAX_CHARS,
            )?;
            validate_internal_action("directory key URLDown", key_item.url_down.as_deref())?;
        }
        Ok(())
    }
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
    let Some(value) = value else {
        return Ok(());
    };
    let length = value.chars().count();
    if !(minimum..=maximum).contains(&length) {
        return Err(PhoneXmlError::InvalidField {
            field,
            expected: match (minimum, maximum) {
                (0, 32) => "at most 32 characters",
                (0, 256) => "at most 256 characters",
                (1, 64) => "between 1 and 64 characters",
                (1, 256) => "between 1 and 256 characters",
                _ => "within the schema length bounds",
            },
        });
    }
    if !has_only_xml_characters(value) {
        return Err(PhoneXmlError::InvalidField {
            field,
            expected: "valid XML text without forbidden control characters",
        });
    }
    Ok(())
}

fn has_only_xml_characters(value: &str) -> bool {
    value.chars().all(|character| {
        matches!(
            character as u32,
            0x9 | 0xA | 0xD | 0x20..=0xD7FF | 0xE000..=0xFFFD | 0x10000..=0x10FFFF
        )
    })
}

fn action_kind(value: &str) -> PhoneActionKind {
    match value.split_once(':').map(|(scheme, _)| scheme) {
        Some(scheme)
            if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") =>
        {
            PhoneActionKind::Http
        }
        _ => PhoneActionKind::Internal,
    }
}

fn validate_internal_action(field: &'static str, value: Option<&str>) -> Result<(), PhoneXmlError> {
    if value.is_some_and(|value| action_kind(value) == PhoneActionKind::Http) {
        Err(PhoneXmlError::InvalidField {
            field,
            expected: "an internal phone action, not HTTP or HTTPS",
        })
    } else {
        Ok(())
    }
}

fn validate_displayable(
    title: Option<&str>,
    prompt: Option<&str>,
    application_id: Option<&str>,
    lifecycle_urls: [Option<&str>; 4],
    soft_keys: &[CiscoIpPhoneSoftKeyItem],
    key_items: &[CiscoIpPhoneKeyItem],
) -> Result<(), PhoneXmlError> {
    validate_optional_text("display title", title, 0, 32)?;
    validate_optional_text("display prompt", prompt, 0, 32)?;
    validate_optional_text("display application id", application_id, 1, 64)?;
    let [on_focus_lost, on_focus_gained, on_minimized, on_closed] = lifecycle_urls;
    for url in [on_focus_lost, on_focus_gained, on_minimized, on_closed] {
        validate_optional_text("display lifecycle URL", url, 1, PHONE_XML_URL_MAX_CHARS)?;
    }
    validate_internal_action("display onAppClosed action", on_closed)?;
    validate_count("display soft keys", soft_keys.len(), 16)?;
    for soft_key in soft_keys {
        validate_optional_text("display soft-key name", soft_key.name.as_deref(), 0, 32)?;
        validate_optional_text(
            "display soft-key URL",
            soft_key.url.as_deref(),
            0,
            PHONE_XML_URL_MAX_CHARS,
        )?;
        validate_optional_text(
            "display soft-key down URL",
            soft_key.url_down.as_deref(),
            0,
            PHONE_XML_URL_MAX_CHARS,
        )?;
        validate_internal_action("display soft-key URLDown", soft_key.url_down.as_deref())?;
    }
    validate_count("display key items", key_items.len(), 32)?;
    for key_item in key_items {
        validate_optional_text(
            "display key URL",
            key_item.url.as_deref(),
            0,
            PHONE_XML_URL_MAX_CHARS,
        )?;
        validate_optional_text(
            "display key down URL",
            key_item.url_down.as_deref(),
            0,
            PHONE_XML_URL_MAX_CHARS,
        )?;
        validate_internal_action("display key URLDown", key_item.url_down.as_deref())?;
    }
    Ok(())
}

fn validate_image_display(
    title: Option<&str>,
    prompt: Option<&str>,
    application_id: Option<&str>,
    lifecycle_urls: [Option<&str>; 4],
    soft_keys: &[CiscoIpPhoneSoftKeyItem],
    key_items: &[CiscoIpPhoneKeyItem],
) -> Result<(), PhoneXmlError> {
    validate_displayable(
        title,
        prompt,
        application_id,
        lifecycle_urls,
        soft_keys,
        key_items,
    )
}

fn validate_bitmap_image(
    location_x: Option<i16>,
    location_y: Option<i16>,
    width: u16,
    height: u16,
    depth: u16,
    data: Option<&PhoneBitmapData>,
) -> Result<(), PhoneXmlError> {
    if location_x.is_some_and(|value| !(-1..=132).contains(&value)) {
        return Err(PhoneXmlError::InvalidField {
            field: "bitmap image horizontal location",
            expected: "between -1 and 132",
        });
    }
    if location_y.is_some_and(|value| !(-1..=64).contains(&value)) {
        return Err(PhoneXmlError::InvalidField {
            field: "bitmap image vertical location",
            expected: "between -1 and 64",
        });
    }
    if !(1..=133).contains(&width) {
        return Err(PhoneXmlError::InvalidField {
            field: "bitmap image width",
            expected: "between 1 and 133",
        });
    }
    if !(1..=65).contains(&height) {
        return Err(PhoneXmlError::InvalidField {
            field: "bitmap image height",
            expected: "between 1 and 65",
        });
    }
    if !(1..=2).contains(&depth) {
        return Err(PhoneXmlError::InvalidField {
            field: "bitmap image depth",
            expected: "between 1 and 2",
        });
    }
    if let Some(data) = data {
        validate_count(
            "bitmap image data bytes",
            data.as_bytes().len(),
            PHONE_IMAGE_BITMAP_MAX_BYTES,
        )?;
    }
    Ok(())
}

fn validate_file_image_location(
    location_x: Option<i16>,
    location_y: Option<i16>,
) -> Result<(), PhoneXmlError> {
    if location_x.is_some_and(|value| !(-1..=297).contains(&value)) {
        return Err(PhoneXmlError::InvalidField {
            field: "image-file horizontal location",
            expected: "between -1 and 297",
        });
    }
    if location_y.is_some_and(|value| !(-1..=167).contains(&value)) {
        return Err(PhoneXmlError::InvalidField {
            field: "image-file vertical location",
            expected: "between -1 and 167",
        });
    }
    Ok(())
}

fn validate_icon_menu_items(items: &[CiscoIpPhoneIconMenuItem]) -> Result<(), PhoneXmlError> {
    validate_count("icon menu items", items.len(), PHONE_ICON_MENU_MAX_ITEMS)?;
    for item in items {
        validate_optional_text("icon menu item name", item.name.as_deref(), 0, 64)?;
        validate_optional_text(
            "icon menu item URL",
            item.url.as_deref(),
            0,
            PHONE_XML_URL_MAX_CHARS,
        )?;
        if item.icon_index.is_some_and(|index| index > 9) {
            return Err(PhoneXmlError::InvalidField {
                field: "icon menu item index",
                expected: "between 0 and 9",
            });
        }
    }
    Ok(())
}

fn validate_bitmap_icon(icon: &CiscoIpPhoneIconItem) -> Result<(), PhoneXmlError> {
    if !(1..=16).contains(&icon.width) {
        return Err(PhoneXmlError::InvalidField {
            field: "bitmap icon width",
            expected: "between 1 and 16",
        });
    }
    if !(1..=10).contains(&icon.height) {
        return Err(PhoneXmlError::InvalidField {
            field: "bitmap icon height",
            expected: "between 1 and 10",
        });
    }
    if !(1..=2).contains(&icon.depth) {
        return Err(PhoneXmlError::InvalidField {
            field: "bitmap icon depth",
            expected: "between 1 and 2",
        });
    }
    if let Some(data) = &icon.data
        && (data.len() > 80
            || data.len() % 2 != 0
            || !data.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        return Err(PhoneXmlError::InvalidField {
            field: "bitmap icon data",
            expected: "at most 40 hexadecimal bytes",
        });
    }
    Ok(())
}

/// One optional label/action pair in a plain menu.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CiscoIpPhoneMenuItem {
    #[serde(rename = "Name", default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(rename = "URL", default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// A complete plain menu with optional lifecycle and physical-key actions.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename = "CiscoIPPhoneMenu", deny_unknown_fields)]
pub struct CiscoIpPhoneMenu {
    #[serde(
        rename = "@keypadTarget",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub keypad_target: Option<PhoneKeypadTarget>,
    #[serde(rename = "@appId", default, skip_serializing_if = "Option::is_none")]
    pub application_id: Option<String>,
    #[serde(
        rename = "@onAppFocusLost",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_focus_lost: Option<String>,
    #[serde(
        rename = "@onAppFocusGained",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_focus_gained: Option<String>,
    #[serde(
        rename = "@onAppMinimized",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_minimized: Option<String>,
    #[serde(
        rename = "@onAppClosed",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_closed: Option<String>,
    #[serde(rename = "Title", default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(rename = "Prompt", default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(rename = "SoftKeyItem", default)]
    pub soft_keys: Vec<CiscoIpPhoneSoftKeyItem>,
    #[serde(rename = "KeyItem", default)]
    pub key_items: Vec<CiscoIpPhoneKeyItem>,
    #[serde(rename = "MenuItem", default)]
    pub items: Vec<CiscoIpPhoneMenuItem>,
}

impl CiscoIpPhoneMenu {
    /// Builds and validates a menu with no optional lifecycle actions.
    pub fn new(
        title: impl Into<String>,
        prompt: impl Into<String>,
        items: Vec<CiscoIpPhoneMenuItem>,
    ) -> Result<Self, PhoneXmlError> {
        let document = Self {
            keypad_target: None,
            application_id: None,
            on_focus_lost: None,
            on_focus_gained: None,
            on_minimized: None,
            on_closed: None,
            title: Some(title.into()),
            prompt: Some(prompt.into()),
            soft_keys: Vec::new(),
            key_items: Vec::new(),
            items,
        };
        document.validate()?;
        Ok(document)
    }

    /// Validates display metadata and the bounded list of menu choices.
    pub fn validate(&self) -> Result<(), PhoneXmlError> {
        validate_displayable(
            self.title.as_deref(),
            self.prompt.as_deref(),
            self.application_id.as_deref(),
            [
                self.on_focus_lost.as_deref(),
                self.on_focus_gained.as_deref(),
                self.on_minimized.as_deref(),
                self.on_closed.as_deref(),
            ],
            &self.soft_keys,
            &self.key_items,
        )?;
        validate_count("menu items", self.items.len(), PHONE_MENU_MAX_ITEMS)?;
        for item in &self.items {
            validate_optional_text("menu item name", item.name.as_deref(), 0, 64)?;
            validate_optional_text(
                "menu item URL",
                item.url.as_deref(),
                0,
                PHONE_XML_URL_MAX_CHARS,
            )?;
        }
        Ok(())
    }

    /// Parses and validates a menu using [`PHONE_MENU_MAX_BYTES`].
    pub fn from_xml(document: &[u8]) -> Result<Self, PhoneXmlError> {
        <Self as PhoneXmlDocument>::parse_xml(document)
    }

    /// Parses a menu with an additional caller-selected byte limit.
    pub fn from_xml_with_limit(
        document: &[u8],
        maximum_bytes: usize,
    ) -> Result<Self, PhoneXmlError> {
        <Self as PhoneXmlDocument>::parse_xml_with_limit(document, maximum_bytes)
    }

    /// Validates and serializes a menu using [`PHONE_MENU_MAX_BYTES`].
    pub fn to_xml(&self) -> Result<String, PhoneXmlError> {
        <Self as PhoneXmlDocument>::serialize_xml(self)
    }

    /// Validates and serializes a menu within a caller-selected byte limit.
    pub fn to_xml_with_limit(&self, maximum_bytes: usize) -> Result<String, PhoneXmlError> {
        <Self as PhoneXmlDocument>::serialize_xml_with_limit(self, maximum_bytes)
    }
}

/// One indexed inline bitmap icon used by an icon menu.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CiscoIpPhoneIconItem {
    #[serde(rename = "Index")]
    pub index: u16,
    #[serde(rename = "Width")]
    /// Icon width in pixels, constrained to `1..=16`.
    pub width: u16,
    #[serde(rename = "Height")]
    /// Icon height in pixels, constrained to `1..=10`.
    pub height: u16,
    #[serde(rename = "Depth")]
    /// Icon bit depth, constrained to `1..=2`.
    pub depth: u16,
    #[serde(rename = "Data", default, skip_serializing_if = "Option::is_none")]
    /// Optional hexadecimal bitmap containing at most 40 bytes.
    pub data: Option<String>,
}

/// One indexed referenced icon used by an icon-file menu.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CiscoIpPhoneIconFileItem {
    #[serde(rename = "Index")]
    pub index: u16,
    #[serde(rename = "URL")]
    /// Resource URL constrained to at most 256 characters.
    pub url: String,
}

/// One optional label/action pair with an optional icon association.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CiscoIpPhoneIconMenuItem {
    #[serde(rename = "Name", default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(rename = "URL", default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(rename = "IconIndex", default, skip_serializing_if = "Option::is_none")]
    /// Icon index in `0..=9`; omission displays no icon.
    pub icon_index: Option<u16>,
}

/// Icon-bearing title used by an icon-file menu.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CiscoIpPhoneIconTitle {
    #[serde(
        rename = "@IconIndex",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    /// Icon index in `0..=9`; omission displays only title text.
    pub icon_index: Option<u16>,
    #[serde(rename = "$text", default)]
    pub text: String,
}

/// A complete menu whose icons are inline hexadecimal bitmaps.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename = "CiscoIPPhoneIconMenu", deny_unknown_fields)]
pub struct CiscoIpPhoneIconMenu {
    #[serde(
        rename = "@keypadTarget",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub keypad_target: Option<PhoneKeypadTarget>,
    #[serde(rename = "@appId", default, skip_serializing_if = "Option::is_none")]
    pub application_id: Option<String>,
    #[serde(
        rename = "@onAppFocusLost",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_focus_lost: Option<String>,
    #[serde(
        rename = "@onAppFocusGained",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_focus_gained: Option<String>,
    #[serde(
        rename = "@onAppMinimized",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_minimized: Option<String>,
    #[serde(
        rename = "@onAppClosed",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_closed: Option<String>,
    #[serde(rename = "Title", default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(rename = "Prompt", default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(rename = "SoftKeyItem", default)]
    pub soft_keys: Vec<CiscoIpPhoneSoftKeyItem>,
    #[serde(rename = "KeyItem", default)]
    pub key_items: Vec<CiscoIpPhoneKeyItem>,
    #[serde(rename = "MenuItem", default)]
    pub items: Vec<CiscoIpPhoneIconMenuItem>,
    #[serde(rename = "IconItem", default)]
    pub icons: Vec<CiscoIpPhoneIconItem>,
}

impl CiscoIpPhoneIconMenu {
    /// Builds and validates an inline-icon menu without lifecycle actions.
    pub fn new(
        title: impl Into<String>,
        prompt: impl Into<String>,
        items: Vec<CiscoIpPhoneIconMenuItem>,
        icons: Vec<CiscoIpPhoneIconItem>,
    ) -> Result<Self, PhoneXmlError> {
        let document = Self {
            keypad_target: None,
            application_id: None,
            on_focus_lost: None,
            on_focus_gained: None,
            on_minimized: None,
            on_closed: None,
            title: Some(title.into()),
            prompt: Some(prompt.into()),
            soft_keys: Vec::new(),
            key_items: Vec::new(),
            items,
            icons,
        };
        document.validate()?;
        Ok(document)
    }

    /// Validates display metadata, choice bounds, and bitmap icon geometry.
    pub fn validate(&self) -> Result<(), PhoneXmlError> {
        validate_displayable(
            self.title.as_deref(),
            self.prompt.as_deref(),
            self.application_id.as_deref(),
            [
                self.on_focus_lost.as_deref(),
                self.on_focus_gained.as_deref(),
                self.on_minimized.as_deref(),
                self.on_closed.as_deref(),
            ],
            &self.soft_keys,
            &self.key_items,
        )?;
        validate_icon_menu_items(&self.items)?;
        validate_count(
            "icon menu icons",
            self.icons.len(),
            PHONE_ICON_MENU_MAX_ICONS,
        )?;
        for icon in &self.icons {
            validate_bitmap_icon(icon)?;
        }
        Ok(())
    }

    /// Parses and validates an inline-icon menu using [`PHONE_MENU_MAX_BYTES`].
    pub fn from_xml(document: &[u8]) -> Result<Self, PhoneXmlError> {
        <Self as PhoneXmlDocument>::parse_xml(document)
    }

    /// Parses an inline-icon menu with an additional caller-selected limit.
    pub fn from_xml_with_limit(
        document: &[u8],
        maximum_bytes: usize,
    ) -> Result<Self, PhoneXmlError> {
        <Self as PhoneXmlDocument>::parse_xml_with_limit(document, maximum_bytes)
    }

    /// Validates and serializes an inline-icon menu using its default bound.
    pub fn to_xml(&self) -> Result<String, PhoneXmlError> {
        <Self as PhoneXmlDocument>::serialize_xml(self)
    }

    /// Validates and serializes an inline-icon menu within a caller-selected limit.
    pub fn to_xml_with_limit(&self, maximum_bytes: usize) -> Result<String, PhoneXmlError> {
        <Self as PhoneXmlDocument>::serialize_xml_with_limit(self, maximum_bytes)
    }
}

/// A complete menu whose icons are loaded from resource URLs.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename = "CiscoIPPhoneIconFileMenu", deny_unknown_fields)]
pub struct CiscoIpPhoneIconFileMenu {
    #[serde(
        rename = "@keypadTarget",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub keypad_target: Option<PhoneKeypadTarget>,
    #[serde(rename = "@appId", default, skip_serializing_if = "Option::is_none")]
    pub application_id: Option<String>,
    #[serde(
        rename = "@onAppFocusLost",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_focus_lost: Option<String>,
    #[serde(
        rename = "@onAppFocusGained",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_focus_gained: Option<String>,
    #[serde(
        rename = "@onAppMinimized",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_minimized: Option<String>,
    #[serde(
        rename = "@onAppClosed",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub on_closed: Option<String>,
    #[serde(
        rename = "@IconIndex",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    /// Optional icon index in `0..=9` displayed beside the title.
    pub icon_index: Option<u16>,
    #[serde(rename = "Title", default, skip_serializing_if = "Option::is_none")]
    pub title: Option<CiscoIpPhoneIconTitle>,
    #[serde(rename = "Prompt", default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(rename = "SoftKeyItem", default)]
    pub soft_keys: Vec<CiscoIpPhoneSoftKeyItem>,
    #[serde(rename = "KeyItem", default)]
    pub key_items: Vec<CiscoIpPhoneKeyItem>,
    #[serde(rename = "MenuItem", default)]
    pub items: Vec<CiscoIpPhoneIconMenuItem>,
    #[serde(rename = "IconItem", default)]
    pub icons: Vec<CiscoIpPhoneIconFileItem>,
}

impl CiscoIpPhoneIconFileMenu {
    /// Validates display metadata, choice bounds, and referenced icons.
    pub fn validate(&self) -> Result<(), PhoneXmlError> {
        validate_displayable(
            self.title.as_ref().map(|title| title.text.as_str()),
            self.prompt.as_deref(),
            self.application_id.as_deref(),
            [
                self.on_focus_lost.as_deref(),
                self.on_focus_gained.as_deref(),
                self.on_minimized.as_deref(),
                self.on_closed.as_deref(),
            ],
            &self.soft_keys,
            &self.key_items,
        )?;
        validate_icon_menu_items(&self.items)?;
        validate_count(
            "icon-file menu icons",
            self.icons.len(),
            PHONE_ICON_MENU_MAX_ICONS,
        )?;
        for icon in &self.icons {
            if icon.index > 9 {
                return Err(PhoneXmlError::InvalidField {
                    field: "icon-file index",
                    expected: "between 0 and 9",
                });
            }
            validate_optional_text("icon-file URL", Some(&icon.url), 1, PHONE_XML_URL_MAX_CHARS)?;
        }
        Ok(())
    }

    /// Parses and validates a referenced-icon menu using [`PHONE_MENU_MAX_BYTES`].
    pub fn from_xml(document: &[u8]) -> Result<Self, PhoneXmlError> {
        <Self as PhoneXmlDocument>::parse_xml(document)
    }

    /// Validates and serializes a referenced-icon menu using its default bound.
    pub fn to_xml(&self) -> Result<String, PhoneXmlError> {
        <Self as PhoneXmlDocument>::serialize_xml(self)
    }
}

/// Display schema used when rendering conference workflows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConferenceMenuFamily {
    Menu,
    IconMenu,
}

/// Participant state needed to render conference menus and allowed actions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConferenceListEntry {
    pub participant_id: ParticipantId,
    pub name: String,
    pub number: String,
    pub moderator: bool,
    pub muted: bool,
}

/// Typed callback encoded into conference menu action URLs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConferenceListAction {
    /// Open the action menu for one participant.
    Participant {
        conference_id: ConferenceId,
        participant_id: ParticipantId,
    },
    Mute {
        conference_id: ConferenceId,
        participant_id: ParticipantId,
    },
    Unmute {
        conference_id: ConferenceId,
        participant_id: ParticipantId,
    },
    Remove {
        conference_id: ConferenceId,
        participant_id: ParticipantId,
    },
    Promote {
        conference_id: ConferenceId,
        participant_id: ParticipantId,
    },
    Demote {
        conference_id: ConferenceId,
        participant_id: ParticipantId,
    },
    End {
        conference_id: ConferenceId,
    },
}

impl ConferenceListAction {
    /// Application identifier embedded in generated callback URLs.
    pub const APPLICATION_ID: u32 = 9091;

    /// Encodes the action as a device-local callback URL.
    pub fn url(self) -> String {
        match self {
            Self::Participant {
                conference_id,
                participant_id,
            } => format!(
                "UserData:{}:0:conference/{}/participant/{}",
                Self::APPLICATION_ID,
                conference_id.get(),
                participant_id.get()
            ),
            Self::Mute {
                conference_id,
                participant_id,
            } => format!(
                "UserData:{}:0:conference/{}/participant/{}/mute",
                Self::APPLICATION_ID,
                conference_id.get(),
                participant_id.get()
            ),
            Self::Unmute {
                conference_id,
                participant_id,
            } => format!(
                "UserData:{}:0:conference/{}/participant/{}/unmute",
                Self::APPLICATION_ID,
                conference_id.get(),
                participant_id.get()
            ),
            Self::Remove {
                conference_id,
                participant_id,
            } => format!(
                "UserData:{}:0:conference/{}/participant/{}/remove",
                Self::APPLICATION_ID,
                conference_id.get(),
                participant_id.get()
            ),
            Self::Promote {
                conference_id,
                participant_id,
            } => format!(
                "UserData:{}:0:conference/{}/participant/{}/promote",
                Self::APPLICATION_ID,
                conference_id.get(),
                participant_id.get()
            ),
            Self::Demote {
                conference_id,
                participant_id,
            } => format!(
                "UserData:{}:0:conference/{}/participant/{}/demote",
                Self::APPLICATION_ID,
                conference_id.get(),
                participant_id.get()
            ),
            Self::End { conference_id } => format!(
                "UserData:{}:0:conference/{}/end",
                Self::APPLICATION_ID,
                conference_id.get()
            ),
        }
    }

    /// Parses a complete callback URL or its bare `conference/...` path.
    pub fn parse(value: &str) -> Option<Self> {
        let path = value
            .trim_matches(['\0', ' ', '\r', '\n'])
            .strip_prefix(&format!("UserData:{}:0:", Self::APPLICATION_ID))
            .unwrap_or(value)
            .strip_prefix("conference/")?;
        let segments: Vec<_> = path.split('/').collect();
        let [conference_id, action, rest @ ..] = segments.as_slice() else {
            return None;
        };
        let conference_id = ConferenceId::new(conference_id.parse().ok()?);
        match (*action, rest) {
            ("participant", [participant]) => Some(Self::Participant {
                conference_id,
                participant_id: ParticipantId::new(participant.parse().ok()?),
            }),
            ("participant", [participant, "mute"]) => Some(Self::Mute {
                conference_id,
                participant_id: ParticipantId::new(participant.parse().ok()?),
            }),
            ("participant", [participant, "unmute"]) => Some(Self::Unmute {
                conference_id,
                participant_id: ParticipantId::new(participant.parse().ok()?),
            }),
            ("participant", [participant, "remove"]) => Some(Self::Remove {
                conference_id,
                participant_id: ParticipantId::new(participant.parse().ok()?),
            }),
            ("participant", [participant, "promote"]) => Some(Self::Promote {
                conference_id,
                participant_id: ParticipantId::new(participant.parse().ok()?),
            }),
            ("participant", [participant, "demote"]) => Some(Self::Demote {
                conference_id,
                participant_id: ParticipantId::new(participant.parse().ok()?),
            }),
            ("end", []) => Some(Self::End { conference_id }),
            _ => None,
        }
    }

    /// Parses the percent-decoded route produced by a service submission.
    pub fn from_route(route: &[String]) -> Option<Self> {
        let [conference, conference_id, action, rest @ ..] = route else {
            return None;
        };
        if conference != "conference" {
            return None;
        }
        let conference_id = ConferenceId::new(conference_id.parse().ok()?);
        match (action.as_str(), rest) {
            ("participant", [participant]) => Some(Self::Participant {
                conference_id,
                participant_id: ParticipantId::new(participant.parse().ok()?),
            }),
            ("participant", [participant, operation]) if operation == "mute" => Some(Self::Mute {
                conference_id,
                participant_id: ParticipantId::new(participant.parse().ok()?),
            }),
            ("participant", [participant, operation]) if operation == "unmute" => {
                Some(Self::Unmute {
                    conference_id,
                    participant_id: ParticipantId::new(participant.parse().ok()?),
                })
            }
            ("participant", [participant, operation]) if operation == "remove" => {
                Some(Self::Remove {
                    conference_id,
                    participant_id: ParticipantId::new(participant.parse().ok()?),
                })
            }
            ("participant", [participant, operation]) if operation == "promote" => {
                Some(Self::Promote {
                    conference_id,
                    participant_id: ParticipantId::new(participant.parse().ok()?),
                })
            }
            ("participant", [participant, operation]) if operation == "demote" => {
                Some(Self::Demote {
                    conference_id,
                    participant_id: ParticipantId::new(participant.parse().ok()?),
                })
            }
            ("end", []) => Some(Self::End { conference_id }),
            _ => None,
        }
    }
}

/// Rendered conference overview in either supported menu family.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConferenceListDocument {
    Menu(CiscoIpPhoneMenu),
    IconMenu(CiscoIpPhoneIconMenu),
}

impl ConferenceListDocument {
    /// Renders a conference overview and validates all menu and byte bounds.
    pub fn new(
        conference_id: ConferenceId,
        participants: &[ConferenceListEntry],
        family: ConferenceMenuFamily,
    ) -> Result<Self, PhoneXmlError> {
        if participants.len() > CONFERENCE_LIST_MAX_PARTICIPANTS {
            return Err(PhoneXmlError::LimitExceeded {
                kind: "conference participants",
                actual: participants.len(),
                maximum: CONFERENCE_LIST_MAX_PARTICIPANTS,
            });
        }
        let title = format!("Conference {}", conference_id.get());
        let prompt = if participants.is_empty() {
            "No participants".to_owned()
        } else {
            "Select a participant".to_owned()
        };
        match family {
            ConferenceMenuFamily::Menu => CiscoIpPhoneMenu::new(
                title,
                prompt,
                participants
                    .iter()
                    .map(|participant| CiscoIpPhoneMenuItem {
                        name: Some(conference_participant_label(participant)),
                        url: Some(
                            ConferenceListAction::Participant {
                                conference_id,
                                participant_id: participant.participant_id,
                            }
                            .url(),
                        ),
                    })
                    .chain(std::iter::once(CiscoIpPhoneMenuItem {
                        name: Some("End conference".into()),
                        url: Some(ConferenceListAction::End { conference_id }.url()),
                    }))
                    .collect(),
            )
            .map(Self::Menu),
            ConferenceMenuFamily::IconMenu => CiscoIpPhoneIconMenu::new(
                title,
                prompt,
                participants
                    .iter()
                    .map(|participant| CiscoIpPhoneIconMenuItem {
                        name: Some(conference_participant_label(participant)),
                        url: Some(
                            ConferenceListAction::Participant {
                                conference_id,
                                participant_id: participant.participant_id,
                            }
                            .url(),
                        ),
                        icon_index: Some(u16::from(participant.moderator)),
                    })
                    .chain(std::iter::once(CiscoIpPhoneIconMenuItem {
                        name: Some("End conference".into()),
                        url: Some(ConferenceListAction::End { conference_id }.url()),
                        icon_index: Some(0),
                    }))
                    .collect(),
                conference_icons(),
            )
            .map(Self::IconMenu),
        }
    }

    /// Serializes the rendered menu within [`CONFERENCE_LIST_MAX_BYTES`].
    pub fn to_xml(&self) -> Result<String, PhoneXmlError> {
        match self {
            Self::Menu(document) => document.to_xml_with_limit(CONFERENCE_LIST_MAX_BYTES),
            Self::IconMenu(document) => document.to_xml_with_limit(CONFERENCE_LIST_MAX_BYTES),
        }
    }

    /// Parses a conference overview using the caller-selected menu family.
    pub fn from_xml(document: &[u8], family: ConferenceMenuFamily) -> Result<Self, PhoneXmlError> {
        match family {
            ConferenceMenuFamily::Menu => {
                CiscoIpPhoneMenu::from_xml_with_limit(document, CONFERENCE_LIST_MAX_BYTES)
                    .map(Self::Menu)
            }
            ConferenceMenuFamily::IconMenu => {
                CiscoIpPhoneIconMenu::from_xml_with_limit(document, CONFERENCE_LIST_MAX_BYTES)
                    .map(Self::IconMenu)
            }
        }
    }

    /// Iterates recognized callback actions in display order.
    pub fn actions(&self) -> impl Iterator<Item = ConferenceListAction> + '_ {
        let urls: Box<dyn Iterator<Item = &str>> = match self {
            Self::Menu(document) => {
                Box::new(document.items.iter().filter_map(|item| item.url.as_deref()))
            }
            Self::IconMenu(document) => {
                Box::new(document.items.iter().filter_map(|item| item.url.as_deref()))
            }
        };
        urls.filter_map(ConferenceListAction::parse)
    }
}

/// Participant-specific conference actions in either supported menu family.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConferenceParticipantActionsDocument {
    Menu(CiscoIpPhoneMenu),
    IconMenu(CiscoIpPhoneIconMenu),
}

impl ConferenceParticipantActionsDocument {
    /// Renders the actions currently permitted for one participant.
    ///
    /// `removable` and `demotable` let session policy suppress actions even
    /// when the participant state would otherwise permit them.
    pub fn new(
        conference_id: ConferenceId,
        participant: &ConferenceListEntry,
        removable: bool,
        demotable: bool,
        family: ConferenceMenuFamily,
    ) -> Result<Self, PhoneXmlError> {
        let mut actions = Vec::new();
        if participant.moderator {
            if demotable {
                actions.push((
                    "Demote",
                    ConferenceListAction::Demote {
                        conference_id,
                        participant_id: participant.participant_id,
                    },
                ));
            }
        } else {
            let (toggle_name, toggle) = if participant.muted {
                (
                    "Unmute",
                    ConferenceListAction::Unmute {
                        conference_id,
                        participant_id: participant.participant_id,
                    },
                )
            } else {
                (
                    "Mute",
                    ConferenceListAction::Mute {
                        conference_id,
                        participant_id: participant.participant_id,
                    },
                )
            };
            actions.push((toggle_name, toggle));
            if removable {
                actions.push((
                    "Remove",
                    ConferenceListAction::Remove {
                        conference_id,
                        participant_id: participant.participant_id,
                    },
                ));
            }
            actions.push((
                "Promote",
                ConferenceListAction::Promote {
                    conference_id,
                    participant_id: participant.participant_id,
                },
            ));
        }
        let title = format!("Participant {}", participant.participant_id.get());
        match family {
            ConferenceMenuFamily::Menu => CiscoIpPhoneMenu::new(
                title,
                "Choose an action",
                actions
                    .into_iter()
                    .map(|(name, action)| CiscoIpPhoneMenuItem {
                        name: Some(name.into()),
                        url: Some(action.url()),
                    })
                    .collect(),
            )
            .map(Self::Menu),
            ConferenceMenuFamily::IconMenu => CiscoIpPhoneIconMenu::new(
                title,
                "Choose an action",
                actions
                    .into_iter()
                    .map(|(name, action)| CiscoIpPhoneIconMenuItem {
                        name: Some(name.into()),
                        url: Some(action.url()),
                        icon_index: None,
                    })
                    .collect(),
                Vec::new(),
            )
            .map(Self::IconMenu),
        }
    }

    /// Serializes the rendered menu within [`CONFERENCE_LIST_MAX_BYTES`].
    pub fn to_xml(&self) -> Result<String, PhoneXmlError> {
        match self {
            Self::Menu(document) => document.to_xml_with_limit(CONFERENCE_LIST_MAX_BYTES),
            Self::IconMenu(document) => document.to_xml_with_limit(CONFERENCE_LIST_MAX_BYTES),
        }
    }

    /// Parses participant actions using the caller-selected menu family.
    pub fn from_xml(document: &[u8], family: ConferenceMenuFamily) -> Result<Self, PhoneXmlError> {
        match family {
            ConferenceMenuFamily::Menu => {
                CiscoIpPhoneMenu::from_xml_with_limit(document, CONFERENCE_LIST_MAX_BYTES)
                    .map(Self::Menu)
            }
            ConferenceMenuFamily::IconMenu => {
                CiscoIpPhoneIconMenu::from_xml_with_limit(document, CONFERENCE_LIST_MAX_BYTES)
                    .map(Self::IconMenu)
            }
        }
    }

    /// Iterates recognized callback actions in display order.
    pub fn actions(&self) -> impl Iterator<Item = ConferenceListAction> + '_ {
        let urls: Box<dyn Iterator<Item = &str>> = match self {
            Self::Menu(document) => {
                Box::new(document.items.iter().filter_map(|item| item.url.as_deref()))
            }
            Self::IconMenu(document) => {
                Box::new(document.items.iter().filter_map(|item| item.url.as_deref()))
            }
        };
        urls.filter_map(ConferenceListAction::parse)
    }
}

fn conference_participant_label(participant: &ConferenceListEntry) -> String {
    let identity = if !participant.name.trim().is_empty() {
        participant.name.trim()
    } else if !participant.number.trim().is_empty() {
        participant.number.trim()
    } else {
        "Unknown participant"
    };
    let role = if participant.moderator {
        "Moderator"
    } else {
        "Participant"
    };
    let mute = if participant.muted { ", muted" } else { "" };
    format!("{identity} ({role}{mute})")
}

fn conference_icons() -> Vec<CiscoIpPhoneIconItem> {
    vec![
        CiscoIpPhoneIconItem {
            index: 0,
            width: 10,
            height: 10,
            depth: 2,
            data: Some("00000000000000000000000000".into()),
        },
        CiscoIpPhoneIconItem {
            index: 1,
            width: 10,
            height: 10,
            depth: 2,
            data: Some("00000155415555554155000000".into()),
        },
    ]
}

/// Parses bounded UTF-8, US-ASCII, or ISO-8859-1 XML using the encoding named
/// in the declaration. The XML reader, rather than an ad-hoc byte scan, owns
/// decoding, entity policy, and nesting validation.
pub fn from_bytes<T: DeserializeOwned>(
    document: &[u8],
    maximum_bytes: usize,
) -> Result<T, PhoneXmlError> {
    if document.len() > maximum_bytes {
        return Err(PhoneXmlError::LimitExceeded {
            kind: "phone XML document",
            actual: document.len(),
            maximum: maximum_bytes,
        });
    }
    // Without an explicit legacy declaration, raw non-UTF-8 input remains a
    // typed UTF-8 failure. quick-xml handles the declared ASCII/ISO decoder;
    // this guard prevents arbitrary malformed bytes from being reported as a
    // less useful generic deserialization error.
    if let Err(error) = std::str::from_utf8(document)
        && !declares_iso_8859_1(document)
    {
        return Err(PhoneXmlError::InvalidUtf8(error));
    }
    reject_document_type(document)?;
    quick_xml::de::from_reader(decoding_reader(document)).map_err(PhoneXmlError::Deserialize)
}

fn decoding_reader(document: &[u8]) -> quick_xml::encoding::DecodingReader<&[u8]> {
    let mut decoder = quick_xml::encoding::DecodingReader::new(document);
    let mut declaration_reader = Reader::from_reader(document);
    if let Ok(Event::Decl(declaration)) = declaration_reader.read_event()
        && declaration
            .encoding()
            .and_then(Result::ok)
            .is_some_and(|encoding| encoding.eq_ignore_ascii_case("iso-8859-1"))
        && let Some(encoding) = declaration.encoder()
    {
        decoder.set_encoding(encoding);
    }
    decoder
}

fn declares_iso_8859_1(document: &[u8]) -> bool {
    let mut reader = Reader::from_reader(document);
    let Ok(Event::Decl(declaration)) = reader.read_event() else {
        return false;
    };
    declaration
        .encoding()
        .and_then(Result::ok)
        .is_some_and(|encoding| encoding.eq_ignore_ascii_case("iso-8859-1"))
}

/// Serializes a known Serde model and rejects an oversized result.
pub fn to_string<T: Serialize>(
    document: &T,
    maximum_bytes: usize,
) -> Result<String, PhoneXmlError> {
    let xml = quick_xml::se::to_string(document).map_err(PhoneXmlError::Serialize)?;
    if xml.len() > maximum_bytes {
        return Err(PhoneXmlError::LimitExceeded {
            kind: "phone XML document",
            actual: xml.len(),
            maximum: maximum_bytes,
        });
    }
    Ok(xml)
}

/// Serializes through the bounded string boundary before touching the writer.
pub fn to_writer<T: Serialize>(
    mut writer: impl fmt::Write,
    document: &T,
    maximum_bytes: usize,
) -> Result<(), PhoneXmlError> {
    let xml = to_string(document, maximum_bytes)?;
    writer.write_str(&xml).map_err(PhoneXmlError::Write)
}

fn reject_document_type(document: &[u8]) -> Result<(), PhoneXmlError> {
    let mut reader = Reader::from_reader(decoding_reader(document));
    let mut buffer = Vec::new();
    let mut depth = 0usize;
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::DocType(_)) => return Err(PhoneXmlError::DocumentTypeForbidden),
            Ok(Event::Start(element)) => {
                validate_xml_attributes(&element)?;
                depth = depth.saturating_add(1);
                if depth > PHONE_XML_MAX_NESTING_DEPTH {
                    return Err(PhoneXmlError::NestingTooDeep {
                        maximum: PHONE_XML_MAX_NESTING_DEPTH,
                    });
                }
            }
            Ok(Event::Empty(element)) => validate_xml_attributes(&element)?,
            Ok(Event::GeneralRef(reference)) => {
                let reference = reference.xml_content(XmlVersion::Implicit1_0);
                let escaped = format!("&{reference};");
                let resolved = quick_xml::escape::unescape(&escaped)
                    .map_err(|_| PhoneXmlError::InvalidEntity)?;
                if !has_only_xml_characters(&resolved) {
                    return Err(PhoneXmlError::InvalidEntity);
                }
            }
            Ok(Event::Text(text)) => {
                let text = text.xml_content(XmlVersion::Implicit1_0);
                if !has_only_xml_characters(&text) {
                    return Err(PhoneXmlError::InvalidEntity);
                }
            }
            Ok(Event::CData(text)) => {
                let text = text.xml_content(XmlVersion::Implicit1_0);
                if !has_only_xml_characters(&text) {
                    return Err(PhoneXmlError::InvalidEntity);
                }
            }
            Ok(Event::End(_)) => depth = depth.saturating_sub(1),
            Ok(Event::Eof) => return Ok(()),
            Ok(_) => {}
            Err(error) => return Err(PhoneXmlError::Malformed(error)),
        }
        buffer.clear();
    }
}

fn validate_xml_attributes(
    element: &quick_xml::events::BytesStart<'_>,
) -> Result<(), PhoneXmlError> {
    for attribute in element.attributes() {
        let attribute = attribute
            .map_err(quick_xml::Error::from)
            .map_err(PhoneXmlError::Malformed)?;
        let value = attribute
            .normalized_value(XmlVersion::Implicit1_0)
            .map_err(|_| PhoneXmlError::InvalidEntity)?;
        if !has_only_xml_characters(&value) {
            return Err(PhoneXmlError::InvalidEntity);
        }
    }
    Ok(())
}

macro_rules! impl_validated_string_value {
    ($($value:ty),+ $(,)?) => {
        $(
            impl AsRef<str> for $value {
                fn as_ref(&self) -> &str {
                    self.as_str()
                }
            }

            impl TryFrom<String> for $value {
                type Error = PhoneXmlError;

                fn try_from(value: String) -> Result<Self, Self::Error> {
                    Self::new(value)
                }
            }

            impl FromStr for $value {
                type Err = PhoneXmlError;

                fn from_str(value: &str) -> Result<Self, Self::Err> {
                    Self::new(value)
                }
            }
        )+
    };
}

impl_validated_string_value!(
    PhoneInputParameterName,
    PhoneExecuteUrl,
    PhoneImageUrl,
    PhoneBackgroundTftpUrl,
    PhoneBackgroundHttpUrl,
    PhoneRingtoneUrl,
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_contract_rejects_a_valid_but_wrong_schema_root() {
        let menu =
            br#"<CiscoIPPhoneMenu><Title>Menu</Title><Prompt>Choose</Prompt></CiscoIPPhoneMenu>"#;

        assert!(matches!(
            CiscoIpPhoneText::from_xml(menu),
            Err(PhoneXmlError::InvalidField {
                field: "phone XML document root",
                ..
            })
        ));
    }

    #[test]
    fn typed_boundary_round_trips_escaped_menu_text() {
        let expected = CiscoIpPhoneMenu::new(
            "Support <East> & West",
            "Choose \"one\"",
            vec![CiscoIpPhoneMenuItem {
                name: Some("Alice & Bob".into()),
                url: Some("UserData:1:0:select/701?lot=east&side=west".into()),
            }],
        )
        .unwrap();
        let xml = to_string(&expected, 2_000).unwrap();
        assert!(xml.contains("Support &lt;East&gt; &amp; West"));
        assert_eq!(
            from_bytes::<CiscoIpPhoneMenu>(xml.as_bytes(), 2_000).unwrap(),
            expected
        );
    }

    #[test]
    fn typed_boundary_rejects_size_utf8_doctype_entities_and_malformed_xml() {
        assert!(matches!(
            from_bytes::<CiscoIpPhoneMenu>(b"<CiscoIPPhoneMenu/>", 5),
            Err(PhoneXmlError::LimitExceeded { .. })
        ));
        assert!(matches!(
            from_bytes::<CiscoIpPhoneMenu>(&[0xff], 5),
            Err(PhoneXmlError::InvalidUtf8(_))
        ));
        let dtd = br#"<!DOCTYPE menu [<!ENTITY name "caller">]><CiscoIPPhoneMenu><Title>&name;</Title><Prompt/></CiscoIPPhoneMenu>"#;
        assert!(matches!(
            from_bytes::<CiscoIpPhoneMenu>(dtd, 2_000),
            Err(PhoneXmlError::DocumentTypeForbidden)
        ));
        let external = br#"<!DOCTYPE menu SYSTEM "file:///untrusted/menu.dtd"><CiscoIPPhoneMenu><Title/><Prompt/></CiscoIPPhoneMenu>"#;
        assert!(matches!(
            from_bytes::<CiscoIpPhoneMenu>(external, 2_000),
            Err(PhoneXmlError::DocumentTypeForbidden)
        ));
        assert!(matches!(
            from_bytes::<CiscoIpPhoneMenu>(
                b"<CiscoIPPhoneMenu><Title>&custom;</Title><Prompt/></CiscoIPPhoneMenu>",
                2_000,
            ),
            Err(PhoneXmlError::InvalidEntity)
        ));
        assert!(from_bytes::<CiscoIpPhoneMenu>(b"<CiscoIPPhoneMenu>", 2_000).is_err());

        let mut oversized = CiscoIpPhoneMenu::new("Menu", "Choose", Vec::new()).unwrap();
        oversized.title = Some("x".repeat(100));
        assert!(matches!(
            to_string(&oversized, 10),
            Err(PhoneXmlError::LimitExceeded { .. })
        ));
    }

    fn complete_text_document() -> CiscoIpPhoneText {
        CiscoIpPhoneText {
            keypad_target: Some(PhoneKeypadTarget::ApplicationCall),
            application_id: Some("text-service".into()),
            on_focus_lost: Some("Notify:focus?state=lost&view=text".into()),
            on_focus_gained: Some("Notify:focus?state=gained".into()),
            on_minimized: Some("Notify:minimized".into()),
            on_closed: Some("Notify:closed".into()),
            title: Some("Message <East> & West".into()),
            prompt: Some("Read & refresh".into()),
            soft_keys: vec![CiscoIpPhoneSoftKeyItem {
                name: Some("Refresh".into()),
                position: PhoneSoftKeyPosition::new(1).unwrap(),
                url: Some("https://pbx.example/text?id=7&view=full".into()),
                url_down: Some("SoftKey:Update".into()),
            }],
            key_items: vec![CiscoIpPhoneKeyItem {
                key: PhoneXmlKey::NavBack,
                url: Some("SoftKey:Exit".into()),
                url_down: None,
            }],
            text: Some("Line one\nCafé <ready> & waiting\t✓".into()),
        }
    }

    #[test]
    fn text_document_round_trips_controls_order_utf8_and_escaping() {
        let expected = complete_text_document();
        let xml = expected.to_xml().unwrap();
        assert!(xml.contains("Message &lt;East&gt; &amp; West"));
        assert!(xml.contains("Café &lt;ready&gt; &amp; waiting"));
        assert!(xml.contains("id=7&amp;view=full"));
        assert!(xml.find("<SoftKeyItem>").unwrap() < xml.find("<KeyItem>").unwrap());
        assert!(xml.find("<KeyItem>").unwrap() < xml.find("<Text>").unwrap());
        assert_eq!(
            CiscoIpPhoneText::from_xml(xml.as_bytes()).unwrap(),
            expected
        );

        let minimal = CiscoIpPhoneText::from_xml(b"<CiscoIPPhoneText/>").unwrap();
        assert!(minimal.text.is_none());
        let empty =
            CiscoIpPhoneText::from_xml(b"<CiscoIPPhoneText><Text></Text></CiscoIPPhoneText>")
                .unwrap();
        assert_eq!(empty.text.as_deref(), Some(""));
    }

    #[test]
    fn text_document_enforces_body_control_soft_key_and_refresh_bounds() {
        let exact = CiscoIpPhoneText::new("Title", "Prompt", "é".repeat(PHONE_TEXT_MAX_CHARS));
        assert!(exact.is_ok());
        assert!(matches!(
            CiscoIpPhoneText::new("Title", "Prompt", "x".repeat(PHONE_TEXT_MAX_CHARS + 1),),
            Err(PhoneXmlError::InvalidField {
                field: "phone text body",
                ..
            })
        ));
        let mut invalid = complete_text_document();
        invalid.text = Some("not\u{1} XML".into());
        assert!(matches!(
            invalid.to_xml(),
            Err(PhoneXmlError::InvalidField {
                field: "phone text body",
                ..
            })
        ));
        invalid = complete_text_document();
        invalid.soft_keys[0].position = PhoneSoftKeyPosition::new(16).unwrap();
        assert!(invalid.to_xml().is_ok());
        invalid = complete_text_document();
        invalid.soft_keys[0].url = Some("x".repeat(PHONE_XML_URL_MAX_CHARS + 1));
        assert!(matches!(
            invalid.to_xml(),
            Err(PhoneXmlError::InvalidField { .. })
        ));

        assert_eq!(PhoneServicePriority::LOW.wire(), 0);
        assert_eq!(PhoneServicePriority::NORMAL.wire(), 1);
        assert_eq!(PhoneServicePriority::HIGH.wire(), 2);
        assert_eq!(
            PhoneServicePriority::default(),
            PhoneServicePriority::NORMAL
        );
        assert!(PhoneServicePriority::new(3).is_err());
        let refresh = PhoneXmlRefresh::new(15, "https://pbx.example/text?page=2").unwrap();
        assert_eq!(refresh.delay_seconds(), 15);
        assert_eq!(refresh.url(), "https://pbx.example/text?page=2");
        assert_eq!(
            refresh.http_header_value(),
            "15;url=https://pbx.example/text?page=2"
        );
        assert!(PhoneXmlRefresh::new(0, "").is_err());
        assert!(PhoneXmlRefresh::new(0, "x".repeat(PHONE_XML_URL_MAX_CHARS + 1)).is_err());
        assert!(PhoneXmlRefresh::new(0, "https://example.test/é").is_err());
        assert!(PhoneXmlRefresh::new(0, "https://example.test/not encoded").is_err());
        assert!(PhoneXmlRefresh::new(0, "https://example.test/\r\nInjected: yes").is_err());
    }

    #[test]
    fn text_parser_rejects_wrong_root_malformed_oversize_nesting_dtd_and_entities() {
        assert!(CiscoIpPhoneText::from_xml(b"<CiscoIPPhoneMenu/>").is_err());
        assert!(
            CiscoIpPhoneText::from_xml(b"<CiscoIPPhoneText><Unknown/></CiscoIPPhoneText>",)
                .is_err()
        );
        assert!(CiscoIpPhoneText::from_xml(b"<CiscoIPPhoneText><Text>").is_err());
        assert!(matches!(
            CiscoIpPhoneText::from_xml(&[0xff]),
            Err(PhoneXmlError::InvalidUtf8(_))
        ));
        assert!(matches!(
            CiscoIpPhoneText::from_xml(
                b"<!DOCTYPE text [<!ENTITY value 'secret'>]><CiscoIPPhoneText><Text>&value;</Text></CiscoIPPhoneText>",
            ),
            Err(PhoneXmlError::DocumentTypeForbidden)
        ));
        assert!(
            CiscoIpPhoneText::from_xml(
                b"<CiscoIPPhoneText><Text>&unknown;</Text></CiscoIPPhoneText>",
            )
            .is_err()
        );
        assert!(matches!(
            complete_text_document().to_xml_with_limit(10),
            Err(PhoneXmlError::LimitExceeded { .. })
        ));

        let nested = format!(
            "<CiscoIPPhoneText>{}<Text>body</Text>{}</CiscoIPPhoneText>",
            "<Nested>".repeat(PHONE_XML_MAX_NESTING_DEPTH),
            "</Nested>".repeat(PHONE_XML_MAX_NESTING_DEPTH),
        );
        assert!(matches!(
            CiscoIpPhoneText::from_xml(nested.as_bytes()),
            Err(PhoneXmlError::NestingTooDeep { .. })
        ));

        #[derive(Debug)]
        struct FailingWriter;
        impl fmt::Write for FailingWriter {
            fn write_str(&mut self, _value: &str) -> fmt::Result {
                Err(fmt::Error)
            }
        }
        assert!(matches!(
            to_writer(
                FailingWriter,
                &complete_text_document(),
                PHONE_TEXT_MAX_BYTES,
            ),
            Err(PhoneXmlError::Write(_))
        ));
    }

    fn complete_input_document() -> CiscoIpPhoneInput {
        CiscoIpPhoneInput {
            keypad_target: Some(PhoneKeypadTarget::ApplicationCall),
            application_id: Some("conference-invite".into()),
            on_focus_lost: Some("Notify:input?focus=lost&view=invite".into()),
            on_focus_gained: Some("Notify:input?focus=gained".into()),
            on_minimized: Some("Notify:input?state=minimized".into()),
            on_closed: Some("Notify:input?state=closed".into()),
            title: Some("Invite <guest>".into()),
            prompt: Some("Enter name & number".into()),
            soft_keys: vec![CiscoIpPhoneSoftKeyItem {
                name: Some("Submit".into()),
                position: PhoneSoftKeyPosition::new(1).unwrap(),
                url: Some("SoftKey:Submit".into()),
                url_down: Some("Notify:submit?state=down".into()),
            }],
            key_items: vec![CiscoIpPhoneKeyItem {
                key: PhoneXmlKey::NavBack,
                url: Some("SoftKey:Exit".into()),
                url_down: None,
            }],
            url: "UserData:9091:0:conference/7/invite?source=phone&mode=full".into(),
            items: vec![
                CiscoIpPhoneInputItem {
                    display_name: Some("Number".into()),
                    parameter: PhoneInputParameterName::new("NUMBER").unwrap(),
                    flags: PhoneInputFlags::Telephone,
                    default_value: Some("+1 555 0100".into()),
                },
                CiscoIpPhoneInputItem {
                    display_name: Some("Name & team".into()),
                    parameter: PhoneInputParameterName::new("NAME&TEAM").unwrap(),
                    flags: PhoneInputFlags::AlphabeticPassword,
                    default_value: Some("Café <guest>".into()),
                },
            ],
        }
    }

    #[test]
    fn input_document_round_trips_every_control_in_schema_order_and_escapes_values() {
        let expected = complete_input_document();
        let xml = expected.to_xml().unwrap();
        assert!(xml.contains("Invite &lt;guest&gt;"));
        assert!(xml.contains("Enter name &amp; number"));
        assert!(xml.contains("NAME&amp;TEAM"));
        assert!(xml.contains("Café &lt;guest&gt;"));
        assert!(xml.contains("source=phone&amp;mode=full"));
        assert!(xml.find("<SoftKeyItem>").unwrap() < xml.find("<KeyItem>").unwrap());
        let submission = xml.find("<URL>UserData:").unwrap();
        assert!(xml.find("<KeyItem>").unwrap() < submission);
        assert!(submission < xml.find("<InputItem>").unwrap());
        assert_eq!(
            CiscoIpPhoneInput::from_xml(xml.as_bytes()).unwrap(),
            expected
        );

        let minimal = CiscoIpPhoneInput::from_xml(
            b"<CiscoIPPhoneInput><URL>submit</URL></CiscoIPPhoneInput>",
        )
        .unwrap();
        assert!(minimal.items.is_empty());
        assert_eq!(minimal.url, "submit");
    }

    #[test]
    fn input_flags_round_trip_every_accepted_schema_value() {
        let codes = [
            "A", "T", "N", "E", "U", "L", "AP", "TP", "NP", "EP", "UP", "LP", "PA", "PT", "PN",
            "PE", "PU", "PL",
        ];
        for (flags, code) in PhoneInputFlags::ALL.into_iter().zip(codes) {
            let document = CiscoIpPhoneInput::new(
                "Input",
                "Enter value",
                "submit",
                vec![CiscoIpPhoneInputItem {
                    display_name: None,
                    parameter: PhoneInputParameterName::new("VALUE").unwrap(),
                    flags,
                    default_value: Some(String::new()),
                }],
            )
            .unwrap();
            let xml = document.to_xml().unwrap();
            assert!(xml.contains(&format!("<InputFlags>{code}</InputFlags>")));
            assert_eq!(
                CiscoIpPhoneInput::from_xml(xml.as_bytes()).unwrap(),
                document
            );
        }
    }

    #[test]
    fn input_document_enforces_field_collection_and_display_bounds() {
        assert!(PhoneInputParameterName::new("").is_err());
        assert!(PhoneInputParameterName::new("x".repeat(33)).is_err());
        assert!(PhoneInputParameterName::new("not\u{1}xml").is_err());

        let exact = CiscoIpPhoneInput::new(
            "t".repeat(32),
            "p".repeat(32),
            "u".repeat(PHONE_XML_URL_MAX_CHARS),
            vec![CiscoIpPhoneInputItem {
                display_name: Some("n".repeat(32)),
                parameter: PhoneInputParameterName::new("q".repeat(32)).unwrap(),
                flags: PhoneInputFlags::Numeric,
                default_value: Some("d".repeat(32)),
            }],
        );
        assert!(exact.is_ok());

        let too_many = (0..=PHONE_INPUT_MAX_ITEMS)
            .map(|index| CiscoIpPhoneInputItem {
                display_name: None,
                parameter: PhoneInputParameterName::new(format!("VALUE{index}")).unwrap(),
                flags: PhoneInputFlags::Alphabetic,
                default_value: None,
            })
            .collect();
        assert!(matches!(
            CiscoIpPhoneInput::new("Input", "Prompt", "submit", too_many),
            Err(PhoneXmlError::LimitExceeded {
                kind: "phone input fields",
                maximum: PHONE_INPUT_MAX_ITEMS,
                ..
            })
        ));

        for invalid in [
            CiscoIpPhoneInput::new("x".repeat(33), "Prompt", "submit", Vec::new()),
            CiscoIpPhoneInput::new("Input", "x".repeat(33), "submit", Vec::new()),
            CiscoIpPhoneInput::new("Input", "Prompt", "", Vec::new()),
            CiscoIpPhoneInput::new(
                "Input",
                "Prompt",
                "x".repeat(PHONE_XML_URL_MAX_CHARS + 1),
                Vec::new(),
            ),
        ] {
            assert!(invalid.is_err());
        }

        let mut invalid = complete_input_document();
        invalid.items[0].display_name = Some("x".repeat(33));
        assert!(invalid.to_xml().is_err());
        invalid = complete_input_document();
        invalid.items[0].default_value = Some("x".repeat(33));
        assert!(invalid.to_xml().is_err());
        assert!(PhoneSoftKeyPosition::new(0).is_err());
    }

    #[test]
    fn input_parser_rejects_wrong_root_unknown_flag_malformed_and_unsafe_documents() {
        assert!(CiscoIpPhoneInput::from_xml(b"<CiscoIPPhoneText/>").is_err());
        assert!(CiscoIpPhoneInput::from_xml(b"<CiscoIPPhoneInput/>").is_err());
        assert!(
            CiscoIpPhoneInput::from_xml(
                b"<CiscoIPPhoneInput><Unknown/><URL>submit</URL></CiscoIPPhoneInput>"
            )
            .is_err()
        );
        assert!(CiscoIpPhoneInput::from_xml(
            b"<CiscoIPPhoneInput><URL>submit</URL><InputItem><QueryStringParam>q</QueryStringParam><InputFlags>Q</InputFlags></InputItem></CiscoIPPhoneInput>"
        )
        .is_err());
        assert!(CiscoIpPhoneInput::from_xml(b"<CiscoIPPhoneInput><URL>").is_err());
        assert!(matches!(
            CiscoIpPhoneInput::from_xml(&[0xff]),
            Err(PhoneXmlError::InvalidUtf8(_))
        ));
        assert!(matches!(
            CiscoIpPhoneInput::from_xml(
                b"<!DOCTYPE input [<!ENTITY value 'secret'>]><CiscoIPPhoneInput><URL>&value;</URL></CiscoIPPhoneInput>",
            ),
            Err(PhoneXmlError::DocumentTypeForbidden)
        ));
        assert!(
            CiscoIpPhoneInput::from_xml(
                b"<CiscoIPPhoneInput><URL>&unknown;</URL></CiscoIPPhoneInput>"
            )
            .is_err()
        );
        assert!(matches!(
            complete_input_document().to_xml_with_limit(10),
            Err(PhoneXmlError::LimitExceeded { .. })
        ));
        let encoded = complete_input_document().to_xml().unwrap();
        assert!(matches!(
            CiscoIpPhoneInput::from_xml_with_limit(encoded.as_bytes(), 10),
            Err(PhoneXmlError::LimitExceeded { .. })
        ));

        let nested = format!(
            "<CiscoIPPhoneInput>{}<URL>submit</URL>{}</CiscoIPPhoneInput>",
            "<Nested>".repeat(PHONE_XML_MAX_NESTING_DEPTH),
            "</Nested>".repeat(PHONE_XML_MAX_NESTING_DEPTH),
        );
        assert!(matches!(
            CiscoIpPhoneInput::from_xml(nested.as_bytes()),
            Err(PhoneXmlError::NestingTooDeep { .. })
        ));

        #[derive(Debug)]
        struct FailingWriter;
        impl fmt::Write for FailingWriter {
            fn write_str(&mut self, _value: &str) -> fmt::Result {
                Err(fmt::Error)
            }
        }
        assert!(matches!(
            to_writer(
                FailingWriter,
                &complete_input_document(),
                PHONE_INPUT_MAX_BYTES,
            ),
            Err(PhoneXmlError::Write(_))
        ));
    }

    fn complete_execute_document() -> CiscoIpPhoneExecute {
        CiscoIpPhoneExecute::new(vec![
            CiscoIpPhoneExecuteItem::with_priority(
                "Key:Directories?name=Café&view=<all>",
                PhoneExecutePriority::LOW,
            )
            .unwrap(),
            CiscoIpPhoneExecuteItem::with_priority(
                "Application:PlacedCalls",
                PhoneExecutePriority::HIGH,
            )
            .unwrap(),
            CiscoIpPhoneExecuteItem::new("Init:Services").unwrap(),
        ])
        .unwrap()
    }

    #[test]
    fn execute_document_round_trips_order_optional_priority_utf8_and_escaping() {
        let expected = complete_execute_document();
        let xml = expected.to_xml().unwrap();
        assert!(xml.starts_with("<CiscoIPPhoneExecute>"));
        assert!(xml.contains(
            r#"<ExecuteItem Priority="0" URL="Key:Directories?name=Café&amp;view=&lt;all&gt;"/>"#
        ));
        assert!(xml.contains(r#"<ExecuteItem Priority="2" URL="Application:PlacedCalls"/>"#));
        assert!(xml.contains(r#"<ExecuteItem URL="Init:Services"/>"#));
        assert_eq!(
            CiscoIpPhoneExecute::from_xml(xml.as_bytes()).unwrap(),
            expected
        );
        assert_eq!(
            expected
                .items
                .iter()
                .map(|item| item.url.as_str())
                .collect::<Vec<_>>(),
            [
                "Key:Directories?name=Café&view=<all>",
                "Application:PlacedCalls",
                "Init:Services",
            ]
        );
    }

    #[test]
    fn execute_document_enforces_action_priority_url_and_collection_bounds() {
        assert_eq!(PhoneExecutePriority::LOW.wire(), 0);
        assert_eq!(PhoneExecutePriority::NORMAL.wire(), 1);
        assert_eq!(PhoneExecutePriority::HIGH.wire(), 2);
        assert!(PhoneExecutePriority::new(3).is_err());
        assert!(PhoneExecuteUrl::new("").is_err());
        assert!(PhoneExecuteUrl::new("x".repeat(PHONE_XML_URL_MAX_CHARS + 1)).is_err());
        assert!(PhoneExecuteUrl::new("not\u{1}xml").is_err());

        assert!(matches!(
            CiscoIpPhoneExecute::new(Vec::new()),
            Err(PhoneXmlError::InvalidField {
                field: "phone execute actions",
                ..
            })
        ));
        let maximum = (0..PHONE_EXECUTE_MAX_ITEMS)
            .map(|index| CiscoIpPhoneExecuteItem::new(format!("Key:KeyPad{index}")).unwrap())
            .collect();
        assert!(CiscoIpPhoneExecute::new(maximum).is_ok());
        assert!(matches!(
            CiscoIpPhoneExecute::new(vec![
                CiscoIpPhoneExecuteItem::new("https://example.test/one").unwrap(),
                CiscoIpPhoneExecuteItem::new("http://example.test/two").unwrap(),
            ]),
            Err(PhoneXmlError::InvalidField {
                field: "phone execute HTTP actions",
                ..
            })
        ));
        let too_many = (0..=PHONE_EXECUTE_MAX_ITEMS)
            .map(|index| CiscoIpPhoneExecuteItem::new(format!("Key:KeyPad{index}")).unwrap())
            .collect();
        assert!(matches!(
            CiscoIpPhoneExecute::new(too_many),
            Err(PhoneXmlError::LimitExceeded {
                kind: "phone execute actions",
                maximum: PHONE_EXECUTE_MAX_ITEMS,
                ..
            })
        ));
    }

    #[test]
    fn execute_parser_rejects_wrong_root_malformed_unsafe_and_oversized_documents() {
        assert!(CiscoIpPhoneExecute::from_xml(b"<CiscoIPPhoneMenu/>").is_err());
        assert!(CiscoIpPhoneExecute::from_xml(b"<CiscoIPPhoneExecute/>").is_err());
        assert!(CiscoIpPhoneExecute::from_xml(
            br#"<CiscoIPPhoneExecute><ExecuteItem Priority="3" URL="Init:Services"/></CiscoIPPhoneExecute>"#
        )
        .is_err());
        assert!(
            CiscoIpPhoneExecute::from_xml(
                br#"<CiscoIPPhoneExecute><ExecuteItem Priority="0"/></CiscoIPPhoneExecute>"#
            )
            .is_err()
        );
        assert!(
            CiscoIpPhoneExecute::from_xml(
                br#"<CiscoIPPhoneExecute><ExecuteItem URL=""/></CiscoIPPhoneExecute>"#
            )
            .is_err()
        );
        let oversized_url = format!(
            "<CiscoIPPhoneExecute><ExecuteItem URL=\"{}\"/></CiscoIPPhoneExecute>",
            "x".repeat(PHONE_XML_URL_MAX_CHARS + 1),
        );
        assert!(CiscoIpPhoneExecute::from_xml(oversized_url.as_bytes()).is_err());
        let too_many_actions = format!(
            "<CiscoIPPhoneExecute>{}</CiscoIPPhoneExecute>",
            r#"<ExecuteItem URL="Init:Services"/>"#.repeat(PHONE_EXECUTE_MAX_ITEMS + 1),
        );
        assert!(matches!(
            CiscoIpPhoneExecute::from_xml(too_many_actions.as_bytes()),
            Err(PhoneXmlError::LimitExceeded {
                kind: "phone execute actions",
                maximum: PHONE_EXECUTE_MAX_ITEMS,
                ..
            })
        ));
        assert!(CiscoIpPhoneExecute::from_xml(
            br#"<CiscoIPPhoneExecute><ExecuteItem Unknown="yes" URL="Init:Services"/></CiscoIPPhoneExecute>"#
        )
        .is_err());
        assert!(
            CiscoIpPhoneExecute::from_xml(
                b"<CiscoIPPhoneExecute><ExecuteItem URL=\"Init:Services\"></CiscoIPPhoneExecute>"
            )
            .is_err()
        );
        assert!(matches!(
            CiscoIpPhoneExecute::from_xml(&[0xff]),
            Err(PhoneXmlError::InvalidUtf8(_))
        ));
        assert!(matches!(
            CiscoIpPhoneExecute::from_xml(
                br#"<!DOCTYPE execute [<!ENTITY action "Init:Services">]><CiscoIPPhoneExecute><ExecuteItem URL="&action;"/></CiscoIPPhoneExecute>"#,
            ),
            Err(PhoneXmlError::DocumentTypeForbidden)
        ));
        assert!(
            CiscoIpPhoneExecute::from_xml(
                br#"<CiscoIPPhoneExecute><ExecuteItem URL="&unknown;"/></CiscoIPPhoneExecute>"#
            )
            .is_err()
        );
        let encoded = complete_execute_document().to_xml().unwrap();
        assert!(matches!(
            CiscoIpPhoneExecute::from_xml_with_limit(encoded.as_bytes(), 10),
            Err(PhoneXmlError::LimitExceeded { .. })
        ));
        assert!(matches!(
            complete_execute_document().to_xml_with_limit(10),
            Err(PhoneXmlError::LimitExceeded { .. })
        ));

        let nested = format!(
            "<CiscoIPPhoneExecute>{}<ExecuteItem URL=\"Init:Services\"/>{}</CiscoIPPhoneExecute>",
            "<Nested>".repeat(PHONE_XML_MAX_NESTING_DEPTH),
            "</Nested>".repeat(PHONE_XML_MAX_NESTING_DEPTH),
        );
        assert!(matches!(
            CiscoIpPhoneExecute::from_xml(nested.as_bytes()),
            Err(PhoneXmlError::NestingTooDeep { .. })
        ));

        #[derive(Debug)]
        struct FailingWriter;
        impl fmt::Write for FailingWriter {
            fn write_str(&mut self, _value: &str) -> fmt::Result {
                Err(fmt::Error)
            }
        }
        assert!(matches!(
            to_writer(
                FailingWriter,
                &complete_execute_document(),
                PHONE_EXECUTE_MAX_BYTES,
            ),
            Err(PhoneXmlError::Write(_))
        ));
    }

    #[test]
    fn declared_iso_8859_1_input_decodes_before_schema_validation() {
        let mut document = br#"<?xml version="1.0" encoding = 'ISO-8859-1'?><CiscoIPPhoneExecute><ExecuteItem URL="Key:Caf"#
            .to_vec();
        document.push(0xe9);
        document.extend_from_slice(br#""/></CiscoIPPhoneExecute>"#);
        let parsed = CiscoIpPhoneExecute::from_xml(&document).unwrap();
        assert_eq!(parsed.items[0].url.as_str(), "Key:Café");
        assert!(matches!(
            CiscoIpPhoneExecute::from_xml(&[b'<', 0xe9, b'>']),
            Err(PhoneXmlError::InvalidUtf8(_))
        ));
    }

    fn background_list_item(name: &str) -> CiscoIpPhoneImageListItem {
        CiscoIpPhoneImageListItem {
            thumbnail_url: PhoneBackgroundTftpUrl::new(format!(
                "TFTP:Desktops/320x212x16/TN-{name}.png"
            ))
            .unwrap(),
            image_url: PhoneBackgroundTftpUrl::new(format!("TFTP:Desktops/320x212x16/{name}.png"))
                .unwrap(),
        }
    }

    #[test]
    fn background_image_list_round_trips_order_attributes_and_escaping() {
        let expected = CiscoIpPhoneImageList::new(vec![
            background_list_item("Fountain"),
            background_list_item("Moon&Stars"),
        ])
        .unwrap();
        let xml = expected.to_xml().unwrap();
        assert!(xml.starts_with("<CiscoIPPhoneImageList>"));
        assert!(xml.contains(
            r#"<ImageItem Image="TFTP:Desktops/320x212x16/TN-Fountain.png" URL="TFTP:Desktops/320x212x16/Fountain.png"/>"#
        ));
        assert!(xml.contains("TN-Moon&amp;Stars.png"));
        assert!(xml.find("Fountain.png").unwrap() < xml.find("Moon&amp;Stars.png").unwrap());
        assert_eq!(
            CiscoIpPhoneImageList::from_xml(xml.as_bytes()).unwrap(),
            expected
        );

        let empty = CiscoIpPhoneImageList::from_xml(b"<CiscoIPPhoneImageList/>").unwrap();
        assert!(empty.items.is_empty());
    }

    #[test]
    fn background_control_documents_round_trip_exact_evidenced_roots_and_order() {
        let image =
            PhoneBackgroundHttpUrl::new("http://pbx.example/background.png?site=east&screen=main")
                .unwrap();
        let thumbnail =
            PhoneBackgroundHttpUrl::new("http://pbx.example/background-thumb.png").unwrap();
        let set = CiscoIpPhoneSetBackground::new(image.clone(), thumbnail);
        let xml = set.to_xml().unwrap();
        assert_eq!(
            xml,
            "<setBackground><background><image>http://pbx.example/background.png?site=east&amp;screen=main</image><icon>http://pbx.example/background-thumb.png</icon></background></setBackground>"
        );
        assert_eq!(
            CiscoIpPhoneSetBackground::from_xml(xml.as_bytes()).unwrap(),
            set
        );
        assert_eq!(
            PhoneBackgroundControlDocument::from_xml(xml.as_bytes()).unwrap(),
            PhoneBackgroundControlDocument::Set(set)
        );

        let preview = CiscoIpPhoneSetBackgroundPreview::new(image);
        let xml = preview.to_xml().unwrap();
        assert_eq!(
            xml,
            "<setBackgroundPreview><image>http://pbx.example/background.png?site=east&amp;screen=main</image></setBackgroundPreview>"
        );
        assert_eq!(
            CiscoIpPhoneSetBackgroundPreview::from_xml(xml.as_bytes()).unwrap(),
            preview
        );
        assert_eq!(
            PhoneBackgroundControlDocument::from_xml(xml.as_bytes()).unwrap(),
            PhoneBackgroundControlDocument::Preview(preview)
        );
    }

    #[test]
    fn background_urls_enforce_transport_shape_length_and_secret_safe_errors() {
        assert_eq!(
            PhoneBackgroundTftpUrl::new("TFTP:Desktops/800x480x24/Picture.PNG")
                .unwrap()
                .as_str(),
            "TFTP:Desktops/800x480x24/Picture.PNG"
        );
        assert_eq!(
            PhoneBackgroundHttpUrl::new("http://[2001:db8::1]:8080/image.png?size=full")
                .unwrap()
                .as_str(),
            "http://[2001:db8::1]:8080/image.png?size=full"
        );
        for invalid in [
            "",
            "HTTP:Desktops/320x212x16/image.png",
            "TFTP://server/Desktops/image.png",
            "TFTP:/Desktops/image.png",
            "TFTP:Desktops/../image.png",
            "TFTP:Desktops/%2e%2e/image.png",
            "TFTP:Desktops/%2Fprivate/image.png",
            "TFTP:Desktops/%00private.png",
            "TFTP:Desktops/%Q0private.png",
            "TFTP:Desktops/image.jpg",
            "TFTP:Desktops/image.png?token=private",
            "TFTP:Desktops/image.png#private",
        ] {
            let error = PhoneBackgroundTftpUrl::new(invalid).unwrap_err();
            if !invalid.is_empty() {
                assert!(!error.to_string().contains(invalid));
            }
        }
        for invalid in [
            "",
            "https://pbx.example/private.png",
            "TFTP:Desktops/image.png",
            "background.png",
            "http://user:secret@pbx.example/private.png",
            "http://pbx.example/private.png#token",
        ] {
            let error = PhoneBackgroundHttpUrl::new(invalid).unwrap_err();
            if !invalid.is_empty() {
                assert!(!error.to_string().contains(invalid));
            }
        }
        assert!(PhoneBackgroundTftpUrl::new("x".repeat(PHONE_XML_URL_MAX_CHARS + 1)).is_err());
        assert!(PhoneBackgroundHttpUrl::new("x".repeat(PHONE_XML_URL_MAX_CHARS + 1)).is_err());
        assert!(PhoneBackgroundTftpUrl::new("TFTP:Desktops/not\u{1}xml.png").is_err());
        assert!(PhoneBackgroundHttpUrl::new("http://pbx.example/not\u{1}xml.png").is_err());
        assert!(
            !format!(
                "{:?}",
                PhoneBackgroundHttpUrl::new("http://private.example/secret.png").unwrap()
            )
            .contains("private.example")
        );
    }

    #[test]
    fn background_image_list_enforces_collection_and_document_bounds() {
        let maximum = (0..PHONE_BACKGROUND_LIST_MAX_ITEMS)
            .map(|index| background_list_item(&format!("image-{index}")))
            .collect();
        assert!(CiscoIpPhoneImageList::new(maximum).is_ok());

        let too_many = (0..=PHONE_BACKGROUND_LIST_MAX_ITEMS)
            .map(|index| background_list_item(&format!("image-{index}")))
            .collect();
        assert!(matches!(
            CiscoIpPhoneImageList::new(too_many),
            Err(PhoneXmlError::LimitExceeded {
                kind: "background image choices",
                maximum: PHONE_BACKGROUND_LIST_MAX_ITEMS,
                ..
            })
        ));

        let document = CiscoIpPhoneImageList::new(vec![background_list_item("image")]).unwrap();
        assert!(matches!(
            document.to_xml_with_limit(10),
            Err(PhoneXmlError::LimitExceeded { .. })
        ));
        assert!(matches!(
            CiscoIpPhoneImageList::from_xml(&vec![b'x'; PHONE_BACKGROUND_LIST_MAX_BYTES + 1]),
            Err(PhoneXmlError::LimitExceeded { .. })
        ));
        let preview =
            PhoneBackgroundControlDocument::Preview(CiscoIpPhoneSetBackgroundPreview::new(
                PhoneBackgroundHttpUrl::new("http://pbx.example/image.png").unwrap(),
            ));
        assert!(matches!(
            preview.to_xml_with_limit(10),
            Err(PhoneXmlError::LimitExceeded { .. })
        ));
    }

    #[test]
    fn background_parser_rejects_wrong_roots_unknowns_malformed_and_unsafe_xml() {
        for invalid in [
            b"<CiscoIPPhoneMenu/>".as_slice(),
            b"<CiscoIPPhoneImageList><ImageItem Image=\"TFTP:Desktops/TN.png\"/></CiscoIPPhoneImageList>".as_slice(),
            b"<CiscoIPPhoneImageList><ImageItem Image=\"TFTP:Desktops/TN.png\" URL=\"TFTP:Desktops/image.png\" Unknown=\"yes\"/></CiscoIPPhoneImageList>".as_slice(),
            b"<CiscoIPPhoneImageList>".as_slice(),
        ] {
            assert!(CiscoIpPhoneImageList::from_xml(invalid).is_err());
        }
        assert!(CiscoIpPhoneSetBackground::from_xml(
            b"<setBackgroundPreview><image>http://pbx.example/image.png</image></setBackgroundPreview>"
        )
        .is_err());
        assert!(CiscoIpPhoneSetBackgroundPreview::from_xml(
            b"<setBackgroundPreview><image>https://pbx.example/image.png</image></setBackgroundPreview>"
        )
        .is_err());
        assert!(PhoneBackgroundControlDocument::from_xml(b"<getDeviceCaps/>").is_err());
        assert!(matches!(
            CiscoIpPhoneImageList::from_xml(&[0xff]),
            Err(PhoneXmlError::InvalidUtf8(_))
        ));
        assert!(matches!(
            CiscoIpPhoneImageList::from_xml(
                br#"<!DOCTYPE images [<!ENTITY path "private">]><CiscoIPPhoneImageList><ImageItem Image="TFTP:Desktops/&path;-TN.png" URL="TFTP:Desktops/&path;.png"/></CiscoIPPhoneImageList>"#,
            ),
            Err(PhoneXmlError::DocumentTypeForbidden)
        ));
        assert!(CiscoIpPhoneImageList::from_xml(
            br#"<CiscoIPPhoneImageList><ImageItem Image="TFTP:Desktops/&unknown;-TN.png" URL="TFTP:Desktops/image.png"/></CiscoIPPhoneImageList>"#,
        )
        .is_err());
        let nested = format!(
            "<CiscoIPPhoneImageList>{}<ImageItem Image=\"TFTP:Desktops/TN.png\" URL=\"TFTP:Desktops/image.png\"/>{}</CiscoIPPhoneImageList>",
            "<Nested>".repeat(PHONE_XML_MAX_NESTING_DEPTH),
            "</Nested>".repeat(PHONE_XML_MAX_NESTING_DEPTH),
        );
        assert!(matches!(
            CiscoIpPhoneImageList::from_xml(nested.as_bytes()),
            Err(PhoneXmlError::NestingTooDeep { .. })
        ));

        #[derive(Debug)]
        struct FailingWriter;
        impl fmt::Write for FailingWriter {
            fn write_str(&mut self, _value: &str) -> fmt::Result {
                Err(fmt::Error)
            }
        }
        assert!(matches!(
            to_writer(
                FailingWriter,
                &CiscoIpPhoneImageList::new(vec![background_list_item("image")]).unwrap(),
                PHONE_BACKGROUND_LIST_MAX_BYTES,
            ),
            Err(PhoneXmlError::Write(_))
        ));
    }

    #[test]
    fn ringtone_document_round_trips_exact_root_child_order_and_escaping() {
        let url =
            PhoneRingtoneUrl::new("http://pbx.example/ringtones/Classic.raw?site=east&set=primary")
                .unwrap();
        assert_eq!(
            url.as_str(),
            "http://pbx.example/ringtones/Classic.raw?site=east&set=primary"
        );
        assert_eq!(
            url.clone().into_string(),
            "http://pbx.example/ringtones/Classic.raw?site=east&set=primary"
        );
        let expected = CiscoIpPhoneSetRingTone::new(url);
        let xml = expected.to_xml().unwrap();
        assert_eq!(
            xml,
            "<setRingTone><ringTone>http://pbx.example/ringtones/Classic.raw?site=east&amp;set=primary</ringTone></setRingTone>"
        );
        assert_eq!(
            CiscoIpPhoneSetRingTone::from_xml(xml.as_bytes()).unwrap(),
            expected
        );
    }

    #[test]
    fn ringtone_url_enforces_transport_shape_length_and_secret_safe_errors() {
        assert_eq!(
            PhoneRingtoneUrl::new("http://[2001:db8::1]:8080/ringtones/Office.raw?locale=sv")
                .unwrap()
                .as_str(),
            "http://[2001:db8::1]:8080/ringtones/Office.raw?locale=sv"
        );
        for invalid in [
            "",
            "HTTP://pbx.example/ringtone.raw",
            "https://pbx.example/ringtone.raw",
            "TFTP:Ringlist.xml",
            "ringtone.raw",
            "http://user:secret@pbx.example/private.raw",
            "http://pbx.example/private.raw#secret",
            "http://pbx.example/not allowed.raw",
            "http://pbx.example/not\tallowed.raw",
            "http://pbx.example/not\\allowed.raw",
            "http://pbx.example/not%Q0allowed.raw",
        ] {
            let error = PhoneRingtoneUrl::new(invalid).unwrap_err();
            if !invalid.is_empty() {
                assert!(!error.to_string().contains(invalid));
            }
        }
        assert!(PhoneRingtoneUrl::new("x".repeat(PHONE_XML_URL_MAX_CHARS + 1)).is_err());
        assert!(PhoneRingtoneUrl::new("http://pbx.example/not\u{1}xml.raw").is_err());
        assert!(
            !format!(
                "{:?}",
                PhoneRingtoneUrl::new("http://private.example/secret.raw").unwrap()
            )
            .contains("private.example")
        );
    }

    #[test]
    fn ringtone_parser_rejects_wrong_root_unknown_malformed_unsafe_and_bounded_xml() {
        for invalid in [
            b"<setBackground><ringTone>http://pbx.example/r.raw</ringTone></setBackground>"
                .as_slice(),
            b"<setRingTone/>".as_slice(),
            b"<setRingTone unknown=\"yes\"><ringTone>http://pbx.example/r.raw</ringTone></setRingTone>"
                .as_slice(),
            b"<setRingTone><ringTone>http://pbx.example/r.raw</ringTone><Unknown/></setRingTone>"
                .as_slice(),
            b"<setRingTone><ringTone>https://pbx.example/r.raw</ringTone></setRingTone>"
                .as_slice(),
            b"<setRingTone><ringTone>".as_slice(),
        ] {
            assert!(CiscoIpPhoneSetRingTone::from_xml(invalid).is_err());
        }
        assert!(matches!(
            CiscoIpPhoneSetRingTone::from_xml(&[0xff]),
            Err(PhoneXmlError::InvalidUtf8(_))
        ));
        assert!(matches!(
            CiscoIpPhoneSetRingTone::from_xml(
                br#"<!DOCTYPE ringtone [<!ENTITY host "private.example">]><setRingTone><ringTone>http://&host;/r.raw</ringTone></setRingTone>"#,
            ),
            Err(PhoneXmlError::DocumentTypeForbidden)
        ));
        assert!(
            CiscoIpPhoneSetRingTone::from_xml(
                b"<setRingTone><ringTone>http://&unknown;/r.raw</ringTone></setRingTone>",
            )
            .is_err()
        );
        assert!(matches!(
            CiscoIpPhoneSetRingTone::from_xml(&vec![b'x'; PHONE_RINGTONE_MAX_BYTES + 1]),
            Err(PhoneXmlError::LimitExceeded {
                maximum: PHONE_RINGTONE_MAX_BYTES,
                ..
            })
        ));

        let nested = format!(
            "<setRingTone>{}<ringTone>http://pbx.example/r.raw</ringTone>{}</setRingTone>",
            "<Nested>".repeat(PHONE_XML_MAX_NESTING_DEPTH),
            "</Nested>".repeat(PHONE_XML_MAX_NESTING_DEPTH),
        );
        assert!(matches!(
            CiscoIpPhoneSetRingTone::from_xml(nested.as_bytes()),
            Err(PhoneXmlError::NestingTooDeep { .. })
        ));

        let document = CiscoIpPhoneSetRingTone::new(
            PhoneRingtoneUrl::new("http://pbx.example/r.raw").unwrap(),
        );
        assert!(matches!(
            document.to_xml_with_limit(10),
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
            to_writer(FailingWriter, &document, PHONE_RINGTONE_MAX_BYTES),
            Err(PhoneXmlError::Write(_))
        ));
    }

    fn image_soft_keys() -> Vec<CiscoIpPhoneSoftKeyItem> {
        vec![CiscoIpPhoneSoftKeyItem {
            name: Some("Select & view".into()),
            position: PhoneSoftKeyPosition::new(1).unwrap(),
            url: Some("SoftKey:Select?view=image&side=west".into()),
            url_down: Some("Notify:select?state=down".into()),
        }]
    }

    fn image_key_items() -> Vec<CiscoIpPhoneKeyItem> {
        vec![CiscoIpPhoneKeyItem {
            key: PhoneXmlKey::NavSelect,
            url: Some("Key:Select?view=image&side=west".into()),
            url_down: None,
        }]
    }

    fn complete_bitmap_image() -> CiscoIpPhoneImage {
        CiscoIpPhoneImage {
            keypad_target: Some(PhoneKeypadTarget::ApplicationCall),
            application_id: Some("image-service".into()),
            on_focus_lost: Some("Notify:image?focus=lost".into()),
            on_focus_gained: Some("Notify:image?focus=gained".into()),
            on_minimized: Some("Notify:image?state=minimized".into()),
            on_closed: Some("Notify:image?state=closed".into()),
            title: Some("Café <map> & menu".into()),
            prompt: Some("Choose & inspect".into()),
            soft_keys: image_soft_keys(),
            key_items: image_key_items(),
            location_x: Some(-1),
            location_y: Some(64),
            width: 133,
            height: 65,
            depth: 2,
            data: Some(PhoneBitmapData::new(vec![0x00, 0xab, 0xff]).unwrap()),
        }
    }

    fn complete_image_file() -> CiscoIpPhoneImageFile {
        CiscoIpPhoneImageFile {
            keypad_target: Some(PhoneKeypadTarget::Application),
            application_id: Some("image-file-service".into()),
            on_focus_lost: None,
            on_focus_gained: None,
            on_minimized: None,
            on_closed: Some("Notify:image-file?state=closed".into()),
            title: Some("Image <file>".into()),
            prompt: Some("Open & inspect".into()),
            soft_keys: image_soft_keys(),
            key_items: image_key_items(),
            location_x: Some(297),
            location_y: Some(-1),
            url: PhoneImageUrl::new("https://pbx.example/image.png?id=7&view=full").unwrap(),
        }
    }

    fn complete_graphic_menu() -> CiscoIpPhoneGraphicMenu {
        CiscoIpPhoneGraphicMenu {
            keypad_target: Some(PhoneKeypadTarget::ActiveCall),
            application_id: Some("graphic-menu".into()),
            on_focus_lost: None,
            on_focus_gained: None,
            on_minimized: None,
            on_closed: None,
            title: Some("Graphic menu".into()),
            prompt: Some("Choose a region".into()),
            soft_keys: image_soft_keys(),
            key_items: image_key_items(),
            location_x: Some(132),
            location_y: Some(-1),
            width: 1,
            height: 1,
            depth: 1,
            data: Some(PhoneBitmapData::new(vec![0x12, 0x34]).unwrap()),
            items: vec![CiscoIpPhoneMenuItem {
                name: Some("West <wing>".into()),
                url: Some("UserData:9095:0:image/west?floor=1&open=true".into()),
            }],
        }
    }

    fn complete_graphic_file_menu() -> CiscoIpPhoneGraphicFileMenu {
        CiscoIpPhoneGraphicFileMenu {
            keypad_target: None,
            application_id: Some("graphic-file-menu".into()),
            on_focus_lost: None,
            on_focus_gained: None,
            on_minimized: None,
            on_closed: None,
            title: Some("Floor plan".into()),
            prompt: Some("Touch a room".into()),
            soft_keys: image_soft_keys(),
            key_items: image_key_items(),
            location_x: Some(-1),
            location_y: Some(167),
            url: PhoneImageUrl::new("https://pbx.example/floor.png?site=east&floor=2").unwrap(),
            items: vec![CiscoIpPhoneTouchAreaMenuItem {
                name: Some("Room A & B".into()),
                url: Some("UserData:9095:0/room/a?mode=open&floor=2".into()),
                touch_area: Some(PhoneTouchArea {
                    x1: 4,
                    y1: 8,
                    x2: 90,
                    y2: 120,
                }),
            }],
        }
    }

    #[test]
    fn image_documents_round_trip_schema_order_hex_utf8_and_escaping() {
        let image = complete_bitmap_image();
        let xml = image.to_xml().unwrap();
        assert!(xml.contains("Café &lt;map&gt; &amp; menu"));
        assert!(xml.contains("<Data>00ABFF</Data>"));
        assert!(xml.find("<SoftKeyItem>").unwrap() < xml.find("<KeyItem>").unwrap());
        assert!(xml.find("<KeyItem>").unwrap() < xml.find("<LocationX>").unwrap());
        assert!(xml.find("<Depth>").unwrap() < xml.find("<Data>").unwrap());
        assert_eq!(CiscoIpPhoneImage::from_xml(xml.as_bytes()).unwrap(), image);
        assert_eq!(
            PhoneImageDocument::from_xml(xml.as_bytes()).unwrap(),
            PhoneImageDocument::Image(image)
        );

        let spaced_hex = b"<CiscoIPPhoneImage><Width>1</Width><Height>1</Height><Depth>1</Depth><Data>00 ab\nFF</Data></CiscoIPPhoneImage>";
        let parsed = CiscoIpPhoneImage::from_xml(spaced_hex).unwrap();
        assert_eq!(parsed.data.unwrap().as_bytes(), [0x00, 0xab, 0xff]);

        let image_file = complete_image_file();
        let xml = image_file.to_xml().unwrap();
        assert!(xml.contains("Image &lt;file&gt;"));
        assert!(xml.contains("id=7&amp;view=full"));
        assert!(xml.find("<KeyItem>").unwrap() < xml.find("<LocationX>").unwrap());
        let controls_end = xml.find("</KeyItem>").unwrap();
        let image_url = controls_end + xml[controls_end..].find("<URL>").unwrap();
        assert!(xml.find("<LocationY>").unwrap() < image_url);
        assert_eq!(
            PhoneImageDocument::from_xml(xml.as_bytes()).unwrap(),
            PhoneImageDocument::ImageFile(image_file)
        );

        let graphic = complete_graphic_menu();
        let xml = graphic.to_xml().unwrap();
        assert!(xml.contains("West &lt;wing&gt;"));
        assert!(xml.find("<Data>").unwrap() < xml.find("<MenuItem>").unwrap());
        assert_eq!(
            PhoneImageDocument::from_xml(xml.as_bytes()).unwrap(),
            PhoneImageDocument::GraphicMenu(graphic)
        );

        let graphic_file = complete_graphic_file_menu();
        let xml = graphic_file.to_xml().unwrap();
        assert!(xml.contains("Room A &amp; B"));
        assert!(xml.contains(r#"<TouchArea X1="4" Y1="8" X2="90" Y2="120"/>"#));
        let controls_end = xml.find("</KeyItem>").unwrap();
        let image_url = controls_end + xml[controls_end..].find("<URL>").unwrap();
        assert!(image_url < xml.find("<MenuItem>").unwrap());
        assert_eq!(
            PhoneImageDocument::from_xml(xml.as_bytes()).unwrap(),
            PhoneImageDocument::GraphicFileMenu(graphic_file)
        );
    }

    #[test]
    fn image_documents_enforce_exact_geometry_data_url_and_collection_bounds() {
        let mut image = complete_bitmap_image();
        assert!(image.validate().is_ok());
        image.location_x = Some(-2);
        assert!(image.validate().is_err());
        image.location_x = Some(133);
        assert!(image.validate().is_err());
        image.location_x = Some(0);
        image.location_y = Some(-2);
        assert!(image.validate().is_err());
        image.location_y = Some(65);
        assert!(image.validate().is_err());
        image.location_y = None;
        for (width, height, depth) in [
            (0, 1, 1),
            (134, 1, 1),
            (1, 0, 1),
            (1, 66, 1),
            (1, 1, 0),
            (1, 1, 3),
        ] {
            image.width = width;
            image.height = height;
            image.depth = depth;
            assert!(image.validate().is_err());
        }
        image.width = 1;
        image.height = 1;
        image.depth = 1;
        image.data = Some(PhoneBitmapData::new(vec![0; PHONE_IMAGE_BITMAP_MAX_BYTES]).unwrap());
        assert!(image.validate().is_ok());
        assert!(matches!(
            PhoneBitmapData::new(vec![0; PHONE_IMAGE_BITMAP_MAX_BYTES + 1]),
            Err(PhoneXmlError::LimitExceeded {
                kind: "bitmap image data bytes",
                maximum: PHONE_IMAGE_BITMAP_MAX_BYTES,
                ..
            })
        ));

        let mut image_file = complete_image_file();
        for x in [-2, 298] {
            image_file.location_x = Some(x);
            assert!(image_file.validate().is_err());
        }
        image_file.location_x = None;
        for y in [-2, 168] {
            image_file.location_y = Some(y);
            assert!(image_file.validate().is_err());
        }
        assert!(PhoneImageUrl::new("").is_err());
        assert!(PhoneImageUrl::new("x".repeat(PHONE_XML_URL_MAX_CHARS + 1)).is_err());
        assert!(PhoneImageUrl::new("not\u{1}xml").is_err());

        let mut graphic = complete_graphic_menu();
        graphic.items = (0..PHONE_GRAPHIC_MENU_MAX_ITEMS)
            .map(|_| CiscoIpPhoneMenuItem {
                name: Some("x".repeat(64)),
                url: Some("x".repeat(PHONE_XML_URL_MAX_CHARS)),
            })
            .collect();
        assert!(graphic.validate().is_ok());
        graphic.items.push(CiscoIpPhoneMenuItem {
            name: None,
            url: None,
        });
        assert!(graphic.validate().is_err());
        graphic.items.truncate(1);
        graphic.items[0].name = Some("x".repeat(65));
        assert!(graphic.validate().is_err());

        let mut graphic_file = complete_graphic_file_menu();
        graphic_file.items = (0..PHONE_GRAPHIC_FILE_MENU_MAX_ITEMS)
            .map(|_| CiscoIpPhoneTouchAreaMenuItem {
                name: Some("x".repeat(32)),
                url: Some("x".repeat(PHONE_XML_URL_MAX_CHARS)),
                touch_area: Some(PhoneTouchArea {
                    x1: u16::MIN,
                    y1: u16::MIN,
                    x2: u16::MAX,
                    y2: u16::MAX,
                }),
            })
            .collect();
        assert!(graphic_file.validate().is_ok());
        graphic_file.items.push(CiscoIpPhoneTouchAreaMenuItem {
            name: None,
            url: None,
            touch_area: None,
        });
        assert!(graphic_file.validate().is_err());
        graphic_file.items.truncate(1);
        graphic_file.items[0].name = Some("x".repeat(33));
        assert!(graphic_file.validate().is_err());
    }

    #[test]
    fn image_parsers_reject_wrong_roots_malformed_unsafe_nested_and_oversized_input() {
        assert!(
            CiscoIpPhoneImage::from_xml(
                b"<CiscoIPPhoneImageFile><URL>x</URL></CiscoIPPhoneImageFile>"
            )
            .is_err()
        );
        assert!(PhoneImageDocument::from_xml(b"<CiscoIPPhoneMenu/>").is_err());
        assert!(CiscoIpPhoneImage::from_xml(b"<CiscoIPPhoneImage><Width>1</Width><Height>1</Height><Depth>1</Depth><Unknown/></CiscoIPPhoneImage>").is_err());
        assert!(CiscoIpPhoneImage::from_xml(b"<CiscoIPPhoneImage><Width>1</Width><Height>1</Height><Depth>1</Depth><Data>123</Data></CiscoIPPhoneImage>").is_err());
        assert!(CiscoIpPhoneImage::from_xml(b"<CiscoIPPhoneImage><Width>1</Width><Height>1</Height><Depth>1</Depth><Data>zz</Data></CiscoIPPhoneImage>").is_err());
        assert!(CiscoIpPhoneGraphicFileMenu::from_xml(b"<CiscoIPPhoneGraphicFileMenu><URL>x</URL><MenuItem><TouchArea X1=\"bad\" Y1=\"0\" X2=\"1\" Y2=\"1\"/></MenuItem></CiscoIPPhoneGraphicFileMenu>").is_err());
        assert!(CiscoIpPhoneImage::from_xml(b"<CiscoIPPhoneImage>").is_err());
        assert!(matches!(
            CiscoIpPhoneImage::from_xml(&[0xff]),
            Err(PhoneXmlError::InvalidUtf8(_))
        ));
        assert!(matches!(
            CiscoIpPhoneImage::from_xml(b"<!DOCTYPE image [<!ENTITY bits '00'>]><CiscoIPPhoneImage><Width>1</Width><Height>1</Height><Depth>1</Depth><Data>&bits;</Data></CiscoIPPhoneImage>"),
            Err(PhoneXmlError::DocumentTypeForbidden)
        ));
        assert!(CiscoIpPhoneImage::from_xml(b"<CiscoIPPhoneImage><Width>1</Width><Height>1</Height><Depth>1</Depth><Data>&unknown;</Data></CiscoIPPhoneImage>").is_err());

        let nested = format!(
            "<CiscoIPPhoneImage>{}<Width>1</Width><Height>1</Height><Depth>1</Depth>{}</CiscoIPPhoneImage>",
            "<Nested>".repeat(PHONE_XML_MAX_NESTING_DEPTH),
            "</Nested>".repeat(PHONE_XML_MAX_NESTING_DEPTH),
        );
        assert!(matches!(
            CiscoIpPhoneImage::from_xml(nested.as_bytes()),
            Err(PhoneXmlError::NestingTooDeep { .. })
        ));
        assert!(matches!(
            PhoneImageDocument::from_xml(&vec![b'x'; PHONE_IMAGE_MAX_BYTES + 1]),
            Err(PhoneXmlError::LimitExceeded { .. })
        ));
        assert!(matches!(
            PhoneImageDocument::Image(complete_bitmap_image()).to_xml_with_limit(10),
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
            to_writer(
                FailingWriter,
                &complete_graphic_file_menu(),
                PHONE_IMAGE_MAX_BYTES
            ),
            Err(PhoneXmlError::Write(_))
        ));
    }

    fn complete_bitmap_status() -> CiscoIpPhoneStatus {
        CiscoIpPhoneStatus {
            text: Some("Café <ready> & active".into()),
            timer_seconds: Some(15),
            location_x: Some(-1),
            location_y: Some(20),
            width: 106,
            height: 21,
            depth: 2,
            data: Some(PhoneBitmapData::new(vec![0x00, 0xab, 0xff]).unwrap()),
        }
    }

    fn complete_file_status() -> CiscoIpPhoneStatusFile {
        CiscoIpPhoneStatusFile {
            text: Some("Status <file> & refresh".into()),
            timer_seconds: Some(u16::MAX),
            location_x: Some(261),
            location_y: Some(-1),
            url: PhoneImageUrl::new("https://pbx.example/status.png?id=7&view=compact").unwrap(),
        }
    }

    #[test]
    fn status_documents_round_trip_icons_timers_order_utf8_and_escaping() {
        let bitmap = complete_bitmap_status();
        let xml = bitmap.to_xml().unwrap();
        assert!(xml.contains("Café &lt;ready&gt; &amp; active"));
        assert!(xml.contains("<Timer>15</Timer>"));
        assert!(xml.contains("<Data>00ABFF</Data>"));
        assert!(xml.find("<Text>").unwrap() < xml.find("<Timer>").unwrap());
        assert!(xml.find("<Timer>").unwrap() < xml.find("<LocationX>").unwrap());
        assert!(xml.find("<Depth>").unwrap() < xml.find("<Data>").unwrap());
        assert_eq!(
            CiscoIpPhoneStatus::from_xml(xml.as_bytes()).unwrap(),
            bitmap
        );
        assert_eq!(
            PhoneStatusDocument::from_xml(xml.as_bytes()).unwrap(),
            PhoneStatusDocument::Bitmap(bitmap)
        );

        let file = complete_file_status();
        let xml = file.to_xml().unwrap();
        assert!(xml.contains("Status &lt;file&gt; &amp; refresh"));
        assert!(xml.contains(&format!("<Timer>{}</Timer>", u16::MAX)));
        assert!(xml.contains("id=7&amp;view=compact"));
        assert!(xml.find("<LocationY>").unwrap() < xml.find("<URL>").unwrap());
        assert_eq!(
            PhoneStatusDocument::from_xml(xml.as_bytes()).unwrap(),
            PhoneStatusDocument::File(file)
        );

        let zero_timer = CiscoIpPhoneStatus::from_xml(
            b"<CiscoIPPhoneStatus><Timer>0</Timer><Width>1</Width><Height>1</Height><Depth>1</Depth><Data></Data></CiscoIPPhoneStatus>",
        )
        .unwrap();
        assert_eq!(zero_timer.timer_seconds, Some(0));
        assert_eq!(zero_timer.data.unwrap().as_bytes(), []);
        let absent_data = CiscoIpPhoneStatus::from_xml(
            b"<CiscoIPPhoneStatus><Width>1</Width><Height>1</Height><Depth>1</Depth></CiscoIPPhoneStatus>",
        )
        .unwrap();
        assert!(absent_data.timer_seconds.is_none());
        assert!(absent_data.data.is_none());
    }

    #[test]
    fn status_documents_enforce_exact_text_geometry_icon_and_url_bounds() {
        let mut bitmap = complete_bitmap_status();
        bitmap.text = Some("x".repeat(32));
        assert!(bitmap.validate().is_ok());
        bitmap.text = Some("x".repeat(33));
        assert!(bitmap.validate().is_err());
        bitmap.text = None;
        for x in [-2, 106] {
            bitmap.location_x = Some(x);
            assert!(bitmap.validate().is_err());
        }
        bitmap.location_x = None;
        for y in [-2, 21] {
            bitmap.location_y = Some(y);
            assert!(bitmap.validate().is_err());
        }
        bitmap.location_y = None;
        for (width, height, depth) in [
            (0, 1, 1),
            (107, 1, 1),
            (1, 0, 1),
            (1, 22, 1),
            (1, 1, 0),
            (1, 1, 3),
        ] {
            bitmap.width = width;
            bitmap.height = height;
            bitmap.depth = depth;
            assert!(bitmap.validate().is_err());
        }
        bitmap.width = 1;
        bitmap.height = 1;
        bitmap.depth = 1;
        bitmap.data = Some(PhoneBitmapData::new(vec![0; PHONE_STATUS_BITMAP_MAX_BYTES]).unwrap());
        assert!(bitmap.validate().is_ok());
        bitmap.data =
            Some(PhoneBitmapData::new(vec![0; PHONE_STATUS_BITMAP_MAX_BYTES + 1]).unwrap());
        assert!(matches!(
            bitmap.validate(),
            Err(PhoneXmlError::LimitExceeded {
                kind: "phone status bitmap bytes",
                maximum: PHONE_STATUS_BITMAP_MAX_BYTES,
                ..
            })
        ));

        let mut file = complete_file_status();
        for x in [-2, 262] {
            file.location_x = Some(x);
            assert!(file.validate().is_err());
        }
        file.location_x = None;
        for y in [-2, 50] {
            file.location_y = Some(y);
            assert!(file.validate().is_err());
        }
        assert!(PhoneImageUrl::new("").is_err());
        assert!(PhoneImageUrl::new("x".repeat(PHONE_XML_URL_MAX_CHARS + 1)).is_err());
    }

    #[test]
    fn status_parsers_reject_wrong_roots_malformed_unsafe_nested_and_oversized_input() {
        assert!(
            CiscoIpPhoneStatus::from_xml(
                b"<CiscoIPPhoneStatusFile><URL>x</URL></CiscoIPPhoneStatusFile>"
            )
            .is_err()
        );
        assert!(PhoneStatusDocument::from_xml(b"<CiscoIPPhoneText/>").is_err());
        assert!(CiscoIpPhoneStatus::from_xml(b"<CiscoIPPhoneStatus><Width>1</Width><Height>1</Height><Depth>1</Depth><Unknown/></CiscoIPPhoneStatus>").is_err());
        assert!(
            CiscoIpPhoneStatus::from_xml(
                b"<CiscoIPPhoneStatus><Height>1</Height><Depth>1</Depth></CiscoIPPhoneStatus>"
            )
            .is_err()
        );
        assert!(CiscoIpPhoneStatus::from_xml(b"<CiscoIPPhoneStatus><Width>1</Width><Height>1</Height><Depth>1</Depth><Data>f</Data></CiscoIPPhoneStatus>").is_err());
        assert!(CiscoIpPhoneStatus::from_xml(b"<CiscoIPPhoneStatus><Width>1</Width><Height>1</Height><Depth>1</Depth><Data>zz</Data></CiscoIPPhoneStatus>").is_err());
        assert!(
            CiscoIpPhoneStatusFile::from_xml(
                b"<CiscoIPPhoneStatusFile><URL></URL></CiscoIPPhoneStatusFile>"
            )
            .is_err()
        );
        assert!(CiscoIpPhoneStatus::from_xml(b"<CiscoIPPhoneStatus>").is_err());
        assert!(matches!(
            CiscoIpPhoneStatus::from_xml(&[0xff]),
            Err(PhoneXmlError::InvalidUtf8(_))
        ));
        assert!(matches!(
            CiscoIpPhoneStatus::from_xml(b"<!DOCTYPE status [<!ENTITY bits '00'>]><CiscoIPPhoneStatus><Width>1</Width><Height>1</Height><Depth>1</Depth><Data>&bits;</Data></CiscoIPPhoneStatus>"),
            Err(PhoneXmlError::DocumentTypeForbidden)
        ));
        assert!(CiscoIpPhoneStatus::from_xml(b"<CiscoIPPhoneStatus><Width>1</Width><Height>1</Height><Depth>1</Depth><Data>&unknown;</Data></CiscoIPPhoneStatus>").is_err());

        let nested = format!(
            "<CiscoIPPhoneStatus>{}<Width>1</Width><Height>1</Height><Depth>1</Depth>{}</CiscoIPPhoneStatus>",
            "<Nested>".repeat(PHONE_XML_MAX_NESTING_DEPTH),
            "</Nested>".repeat(PHONE_XML_MAX_NESTING_DEPTH),
        );
        assert!(matches!(
            CiscoIpPhoneStatus::from_xml(nested.as_bytes()),
            Err(PhoneXmlError::NestingTooDeep { .. })
        ));
        assert!(matches!(
            PhoneStatusDocument::from_xml(&vec![b'x'; PHONE_STATUS_MAX_BYTES + 1]),
            Err(PhoneXmlError::LimitExceeded { .. })
        ));
        assert!(matches!(
            PhoneStatusDocument::Bitmap(complete_bitmap_status()).to_xml_with_limit(10),
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
            to_writer(
                FailingWriter,
                &complete_file_status(),
                PHONE_STATUS_MAX_BYTES,
            ),
            Err(PhoneXmlError::Write(_))
        ));
    }

    fn complete_alarm() -> CiscoIpPhoneAlarm {
        CiscoIpPhoneAlarm {
            alarm: CiscoIpPhoneAlarmEntry {
                name: LAST_OUT_OF_SERVICE_ALARM.into(),
                parameter_list: CiscoIpPhoneAlarmParameterList {
                    parameters: vec![
                        CiscoIpPhoneAlarmParameter::String(CiscoIpPhoneAlarmString {
                            name: "DeviceName".into(),
                            value: "SEP001122334455".into(),
                        }),
                        CiscoIpPhoneAlarmParameter::Enum(CiscoIpPhoneAlarmEnum {
                            name: "DHCPv4Status".into(),
                            value: 1,
                        }),
                        CiscoIpPhoneAlarmParameter::Enum(CiscoIpPhoneAlarmEnum {
                            name: "ReasonForOutOfService".into(),
                            value: 25,
                        }),
                        CiscoIpPhoneAlarmParameter::String(CiscoIpPhoneAlarmString {
                            name: "LastProtocolEventSent".into(),
                            value: "Sent:REGISTER <call-id> & route".into(),
                        }),
                        CiscoIpPhoneAlarmParameter::String(CiscoIpPhoneAlarmString {
                            name: "LastProtocolEventReceived".into(),
                            value: String::new(),
                        }),
                    ],
                },
            },
        }
    }

    #[test]
    fn alarm_schema_round_trips_ordered_typed_parameters_and_accessors() {
        let expected = complete_alarm();
        let xml = expected.to_xml().unwrap();
        assert!(xml.starts_with(
            "<x-cisco-alarm><Alarm Name=\"LastOutOfServiceInformation\"><ParameterList>"
        ));
        assert!(xml.contains("Sent:REGISTER &lt;call-id&gt; &amp; route"));
        assert!(xml.find("DeviceName").unwrap() < xml.find("DHCPv4Status").unwrap());
        assert!(
            xml.find("ReasonForOutOfService").unwrap() < xml.find("LastProtocolEventSent").unwrap()
        );
        let decoded = CiscoIpPhoneAlarm::from_xml(xml.as_bytes()).unwrap();
        assert_eq!(decoded, expected);
        assert_eq!(decoded.reason_for_out_of_service(), Some(25));
        assert_eq!(decoded.enumeration("DHCPv4Status"), Some(1));
        assert_eq!(decoded.string("DeviceName"), Some("SEP001122334455"));
        assert_eq!(decoded.string("LastProtocolEventReceived"), Some(""));
        assert_eq!(decoded.string("Unknown"), None);
        let telemetry = parse_phone_alarm(xml.as_bytes()).unwrap();
        assert!(matches!(
            &telemetry,
            PhoneAlarmTelemetry::LastOutOfService(alarm) if alarm == &expected
        ));
        assert_eq!(
            telemetry.summary(),
            Some(PhoneAlarmSummary {
                kind: PhoneAlarmKind::LastOutOfService,
                reason_for_out_of_service: Some(25),
            })
        );
    }

    #[test]
    fn unknown_alarm_schemas_remain_bounded_lossless_and_secret_safe() {
        for unknown in [
            b"<x-cisco-alarm/>".as_slice(),
            b"<x-cisco-alarm><Alarm Name=\"DeviceTroubleshootingReport\"><ParameterList><String name=\"Token\">secret-value</String></ParameterList></Alarm></x-cisco-alarm>".as_slice(),
            b"<vendor-alarm><Credential>secret-value</Credential></vendor-alarm>".as_slice(),
        ] {
            let PhoneAlarmTelemetry::Opaque(opaque) = parse_phone_alarm(unknown).unwrap() else {
                panic!("unknown alarm schema must remain opaque");
            };
            assert_eq!(opaque.as_bytes(), unknown);
            let debug = format!("{opaque:?}");
            assert!(!debug.contains("secret-value"));
            assert!(debug.contains(&unknown.len().to_string()));
            assert_eq!(opaque.clone().into_bytes(), unknown);
        }

        let opaque = parse_phone_alarm(b"<vendor-alarm/>").unwrap();
        assert!(opaque.is_opaque());
        assert_eq!(opaque.summary(), None);

        let known = complete_alarm();
        let debug = format!("{known:?}");
        assert!(!debug.contains("SEP001122334455"));
        assert!(!debug.contains("call-id"));
        assert!(debug.contains(LAST_OUT_OF_SERVICE_ALARM));
        assert_eq!(
            format!("{:?}", known.alarm.parameter_list),
            "CiscoIpPhoneAlarmParameterList { parameter_count: 5 }"
        );
    }

    #[test]
    fn known_alarm_validation_rejects_ambiguity_unsafe_values_and_size_overflow() {
        let mut alarm = complete_alarm();
        alarm
            .alarm
            .parameter_list
            .parameters
            .push(CiscoIpPhoneAlarmParameter::Enum(CiscoIpPhoneAlarmEnum {
                name: "DeviceName".into(),
                value: 2,
            }));
        assert!(matches!(
            alarm.validate(),
            Err(PhoneXmlError::InvalidField {
                field: "phone alarm parameter names",
                ..
            })
        ));

        alarm = complete_alarm();
        match &mut alarm.alarm.parameter_list.parameters[0] {
            CiscoIpPhoneAlarmParameter::String(device) => device.name.clear(),
            CiscoIpPhoneAlarmParameter::Enum(_) => panic!("first parameter must be a string"),
        }
        assert!(alarm.validate().is_err());
        match &mut alarm.alarm.parameter_list.parameters[0] {
            CiscoIpPhoneAlarmParameter::String(device) => {
                device.name = "DeviceName".into();
                device.value = "not\u{1}xml".into();
            }
            CiscoIpPhoneAlarmParameter::Enum(_) => panic!("first parameter must be a string"),
        }
        assert!(alarm.validate().is_err());
        match &mut alarm.alarm.parameter_list.parameters[0] {
            CiscoIpPhoneAlarmParameter::String(device) => {
                device.value = "sensitive-value".repeat(PHONE_ALARM_MAX_BYTES);
            }
            CiscoIpPhoneAlarmParameter::Enum(_) => panic!("first parameter must be a string"),
        }
        let error = alarm.to_xml().unwrap_err();
        assert!(!error.to_string().contains("sensitive-value"));

        let duplicate = b"<x-cisco-alarm><Alarm Name=\"LastOutOfServiceInformation\"><ParameterList><String name=\"DeviceName\">first-secret</String><String name=\"DeviceName\">second-secret</String></ParameterList></Alarm></x-cisco-alarm>";
        let error = parse_phone_alarm(duplicate).unwrap_err();
        assert!(!error.to_string().contains("first-secret"));
        assert!(!error.to_string().contains("second-secret"));
    }

    #[test]
    fn alarm_parser_rejects_malformed_known_unsafe_and_oversized_documents() {
        assert!(parse_phone_alarm(b"<x-cisco-alarm>").is_err());
        assert!(matches!(
            parse_phone_alarm(&[0xff]),
            Err(PhoneXmlError::InvalidUtf8(_))
        ));
        assert!(matches!(
            parse_phone_alarm(b"<!DOCTYPE alarm [<!ENTITY value 'secret'>]><x-cisco-alarm><Alarm Name=\"Unknown\"><ParameterList><String name=\"Value\">&value;</String></ParameterList></Alarm></x-cisco-alarm>"),
            Err(PhoneXmlError::DocumentTypeForbidden)
        ));
        assert!(parse_phone_alarm(b"<x-cisco-alarm><Alarm Name=\"Unknown\"><ParameterList><String name=\"Value\">&unknown;</String></ParameterList></Alarm></x-cisco-alarm>").is_err());
        assert!(parse_phone_alarm(b"<vendor-alarm><Value>&#1;</Value></vendor-alarm>").is_err());
        assert!(parse_phone_alarm(b"<vendor-alarm value=\"&#1;\"/>").is_err());
        assert!(parse_phone_alarm(b"<vendor-alarm>not\x01xml</vendor-alarm>").is_err());
        assert!(parse_phone_alarm(b"<x-cisco-alarm><Alarm Name=\"LastOutOfServiceInformation\"><ParameterList><Binary name=\"Value\">00</Binary></ParameterList></Alarm></x-cisco-alarm>").is_err());
        assert!(
            parse_phone_alarm(
                b"<x-cisco-alarm><Alarm Name=\"LastOutOfServiceInformation\"/></x-cisco-alarm>"
            )
            .is_err()
        );
        let invalid_enum = b"<x-cisco-alarm><Alarm Name=\"LastOutOfServiceInformation\"><ParameterList><Enum name=\"ReasonForOutOfService\">secret-enum</Enum></ParameterList></Alarm></x-cisco-alarm>";
        let error = parse_phone_alarm(invalid_enum).unwrap_err();
        assert!(!error.to_string().contains("secret-enum"));

        let nested = format!(
            "<x-cisco-alarm>{}<Alarm Name=\"Unknown\"/>{}</x-cisco-alarm>",
            "<Nested>".repeat(PHONE_XML_MAX_NESTING_DEPTH),
            "</Nested>".repeat(PHONE_XML_MAX_NESTING_DEPTH),
        );
        assert!(matches!(
            parse_phone_alarm(nested.as_bytes()),
            Err(PhoneXmlError::NestingTooDeep { .. })
        ));
        assert!(matches!(
            parse_phone_alarm(&vec![b'x'; PHONE_ALARM_MAX_BYTES + 1]),
            Err(PhoneXmlError::LimitExceeded {
                maximum: PHONE_ALARM_MAX_BYTES,
                ..
            })
        ));

        #[derive(Debug)]
        struct FailingWriter;
        impl fmt::Write for FailingWriter {
            fn write_str(&mut self, _value: &str) -> fmt::Result {
                Err(fmt::Error)
            }
        }
        assert!(matches!(
            to_writer(FailingWriter, &complete_alarm(), PHONE_ALARM_MAX_BYTES),
            Err(PhoneXmlError::Write(_))
        ));
    }

    fn complete_location() -> CiscoIpPhoneLocationInformation {
        CiscoIpPhoneLocationInformation {
            wifi: CiscoIpPhoneWifiLocation {
                bssid: PhoneBssid::parse("e8:ed:f3:10:29:fd").unwrap(),
                ssid: "Café <voice> & data".into(),
                access_point_name: "West wing <3>".into(),
            },
            off_premises: Some(CiscoIpPhoneOffPremises::new()),
        }
    }

    #[test]
    fn location_schema_round_trips_typed_address_fields_order_and_escaping() {
        let expected = complete_location();
        let xml = expected.to_xml().unwrap();
        assert!(xml.starts_with("<Interface1><wifi><BSSID>E8:ED:F3:10:29:FD</BSSID>"));
        assert!(xml.contains("<SSID>Café &lt;voice&gt; &amp; data</SSID>"));
        assert!(xml.contains("<APName>West wing &lt;3&gt;</APName>"));
        assert!(xml.find("</wifi>").unwrap() < xml.find("<OffPrem").unwrap());
        assert_eq!(
            CiscoIpPhoneLocationInformation::from_xml(xml.as_bytes()).unwrap(),
            expected
        );
        assert_eq!(
            expected.wifi.bssid.octets(),
            [0xe8, 0xed, 0xf3, 0x10, 0x29, 0xfd]
        );
        assert_eq!(expected.wifi.bssid.to_string(), "E8:ED:F3:10:29:FD");
        assert!(expected.is_off_premises());

        let telemetry = parse_phone_location(xml.as_bytes()).unwrap();
        assert_eq!(
            telemetry.summary(),
            Some(PhoneLocationSummary {
                kind: PhoneLocationKind::WirelessInterface,
                off_premises: true,
            })
        );

        let on_premises = CiscoIpPhoneLocationInformation::from_xml(
            b"<Interface1><wifi><BSSID>00:11:22:33:44:55</BSSID><SSID></SSID><APName/></wifi></Interface1>",
        )
        .unwrap();
        assert!(!on_premises.is_off_premises());
        assert_eq!(on_premises.wifi.ssid, "");
        assert_eq!(on_premises.wifi.access_point_name, "");
    }

    #[test]
    fn location_models_enforce_address_marker_text_and_document_bounds() {
        for invalid in [
            "00:11:22:33:44",
            "00:11:22:33:44:555",
            "00-11-22-33-44-55",
            "00:11:22:33:44:gg",
            "private-address",
        ] {
            let error = PhoneBssid::parse(invalid).unwrap_err();
            assert!(!error.to_string().contains(invalid));
        }

        let mut location = complete_location();
        location.wifi.ssid = "é".repeat(16);
        assert!(location.validate().is_ok());
        location.wifi.ssid.push('é');
        assert!(matches!(
            location.validate(),
            Err(PhoneXmlError::InvalidField {
                field: "phone location SSID",
                expected: "at most 32 bytes",
            })
        ));

        location = complete_location();
        location.wifi.access_point_name = "private-name".repeat(PHONE_LOCATION_MAX_BYTES);
        let error = location.to_xml().unwrap_err();
        assert!(!error.to_string().contains("private-name"));

        let nonempty_marker = b"<Interface1><wifi><BSSID>00:11:22:33:44:55</BSSID><SSID>voice</SSID><APName>west</APName></wifi><OffPrem>private-location</OffPrem></Interface1>";
        let error = parse_phone_location(nonempty_marker).unwrap_err();
        assert!(!error.to_string().contains("private-location"));
    }

    #[test]
    fn unknown_location_schemas_are_bounded_lossless_and_secret_safe() {
        for unknown in [
            b"<Interface2><wifi><BSSID>00:11:22:33:44:55</BSSID></wifi></Interface2>".as_slice(),
            b"<DeviceLocation><CivicAddress>private-building</CivicAddress></DeviceLocation>"
                .as_slice(),
        ] {
            let telemetry = parse_phone_location(unknown).unwrap();
            let PhoneLocationTelemetry::Opaque(opaque) = &telemetry else {
                panic!("unsupported location schema must remain opaque");
            };
            assert_eq!(opaque.as_bytes(), unknown);
            assert_eq!(opaque.clone().into_bytes(), unknown);
            assert_eq!(telemetry.summary(), None);
            assert!(telemetry.is_opaque());
            let debug = format!("{telemetry:?}");
            assert!(!debug.contains("private-building"));
            assert!(!debug.contains("00:11:22:33:44:55"));
            assert!(debug.contains(&unknown.len().to_string()));
        }

        let debug = format!("{:?}", complete_location());
        assert!(!debug.contains("Café"));
        assert!(!debug.contains("West wing"));
        assert!(!debug.contains("E8:ED:F3:10:29:FD"));
    }

    #[test]
    fn location_parser_rejects_malformed_known_unsafe_and_oversized_documents() {
        for invalid in [
            b"<Interface1>".as_slice(),
            b"<Interface1><wifi><BSSID>private-address</BSSID><SSID>private-network</SSID><APName>private-access-point</APName></wifi></Interface1>".as_slice(),
            b"<Interface1><wifi><BSSID>00:11:22:33:44:55</BSSID><SSID>voice</SSID><APName>west</APName><Credential>private-secret</Credential></wifi></Interface1>".as_slice(),
            b"<Interface1><OffPrem/></Interface1>".as_slice(),
            b"<Interface1><wifi><BSSID>00:11:22:33:44:55</BSSID><SSID>one</SSID><SSID>two</SSID><APName>west</APName></wifi></Interface1>".as_slice(),
        ] {
            let error = parse_phone_location(invalid).unwrap_err();
            let error = error.to_string();
            assert!(!error.contains("private-address"));
            assert!(!error.contains("private-network"));
            assert!(!error.contains("private-access-point"));
            assert!(!error.contains("private-secret"));
        }
        assert!(matches!(
            parse_phone_location(&[0xff]),
            Err(PhoneXmlError::InvalidUtf8(_))
        ));
        assert!(matches!(
            parse_phone_location(b"<!DOCTYPE Interface2 [<!ENTITY location 'private'>]><Interface2>&location;</Interface2>"),
            Err(PhoneXmlError::DocumentTypeForbidden)
        ));
        assert!(matches!(
            parse_phone_location(b"<Interface2>&undeclared;</Interface2>"),
            Err(PhoneXmlError::InvalidEntity)
        ));
        assert!(parse_phone_location(b"<Interface2>&#1;</Interface2>").is_err());
        assert!(parse_phone_location(b"<Interface2>not\x01xml</Interface2>").is_err());

        let nested = format!(
            "<Interface2>{}{}</Interface2>",
            "<Nested>".repeat(PHONE_XML_MAX_NESTING_DEPTH),
            "</Nested>".repeat(PHONE_XML_MAX_NESTING_DEPTH),
        );
        assert!(matches!(
            parse_phone_location(nested.as_bytes()),
            Err(PhoneXmlError::NestingTooDeep { .. })
        ));
        assert!(matches!(
            parse_phone_location(&vec![b'x'; PHONE_LOCATION_MAX_BYTES + 1]),
            Err(PhoneXmlError::LimitExceeded {
                maximum: PHONE_LOCATION_MAX_BYTES,
                ..
            })
        ));

        #[derive(Debug)]
        struct FailingWriter;
        impl fmt::Write for FailingWriter {
            fn write_str(&mut self, _value: &str) -> fmt::Result {
                Err(fmt::Error)
            }
        }
        assert!(matches!(
            to_writer(
                FailingWriter,
                &complete_location(),
                PHONE_LOCATION_MAX_BYTES,
            ),
            Err(PhoneXmlError::Write(_))
        ));
    }

    fn complete_menu() -> CiscoIpPhoneMenu {
        CiscoIpPhoneMenu {
            keypad_target: Some(PhoneKeypadTarget::ApplicationCall),
            application_id: Some("menu-west".into()),
            on_focus_lost: Some("Notify:focus?state=lost&side=west".into()),
            on_focus_gained: Some("Notify:focus?state=gained".into()),
            on_minimized: Some("Notify:minimized".into()),
            on_closed: Some("Notify:closed".into()),
            title: Some("Support <East> & West".into()),
            prompt: Some("Choose A & B".into()),
            soft_keys: vec![CiscoIpPhoneSoftKeyItem {
                name: Some("Open & inspect".into()),
                position: PhoneSoftKeyPosition::new(1).unwrap(),
                url: Some("SoftKey:Select?a=1&b=2".into()),
                url_down: Some("SoftKey:SelectDown".into()),
            }],
            key_items: vec![CiscoIpPhoneKeyItem {
                key: PhoneXmlKey::NavBack,
                url: Some("SoftKey:Cancel".into()),
                url_down: None,
            }],
            items: vec![CiscoIpPhoneMenuItem {
                name: Some("Alice <Admin> & Bob".into()),
                url: Some("UserData:7:0:open/a?x=1&y=2".into()),
            }],
        }
    }

    #[test]
    fn basic_menu_round_trips_complete_display_controls_in_schema_order() {
        let expected = complete_menu();
        let xml = expected.to_xml().unwrap();
        assert!(xml.contains("Support &lt;East&gt; &amp; West"));
        assert!(xml.contains("Alice &lt;Admin&gt; &amp; Bob"));
        assert!(xml.contains("x=1&amp;y=2"));
        assert!(xml.find("<SoftKeyItem>").unwrap() < xml.find("<KeyItem>").unwrap());
        assert!(xml.find("<KeyItem>").unwrap() < xml.find("<MenuItem>").unwrap());
        assert_eq!(
            CiscoIpPhoneMenu::from_xml(xml.as_bytes()).unwrap(),
            expected
        );

        let minimal = CiscoIpPhoneMenu::from_xml(b"<CiscoIPPhoneMenu/>").unwrap();
        assert!(minimal.title.is_none());
        assert!(minimal.items.is_empty());
    }

    #[test]
    fn bitmap_and_resource_icon_menus_round_trip_exact_icon_families() {
        let bitmap = CiscoIpPhoneIconMenu::new(
            "Conference & staff",
            "Choose <one>",
            vec![CiscoIpPhoneIconMenuItem {
                name: Some("Taylor & team".into()),
                url: Some("UserData:1:0:participant/7?view=a&b=c".into()),
                icon_index: Some(2),
            }],
            vec![CiscoIpPhoneIconItem {
                index: 2,
                width: 16,
                height: 10,
                depth: 2,
                data: Some("000FF0".into()),
            }],
        )
        .unwrap();
        let xml = bitmap.to_xml().unwrap();
        assert!(xml.find("<MenuItem>").unwrap() < xml.find("<IconItem>").unwrap());
        assert!(xml.find("<Width>").unwrap() < xml.find("<Height>").unwrap());
        assert!(xml.contains("Conference &amp; staff"));
        assert_eq!(
            CiscoIpPhoneIconMenu::from_xml(xml.as_bytes()).unwrap(),
            bitmap
        );

        let resources = CiscoIpPhoneIconFileMenu {
            keypad_target: Some(PhoneKeypadTarget::ActiveCall),
            application_id: Some("conference-list".into()),
            on_focus_lost: None,
            on_focus_gained: Some("Notify:focus".into()),
            on_minimized: None,
            on_closed: Some("SoftKey:Exit".into()),
            icon_index: Some(4),
            title: Some(CiscoIpPhoneIconTitle {
                icon_index: Some(5),
                text: "Locked & secure".into(),
            }),
            prompt: Some("Choose a participant".into()),
            soft_keys: Vec::new(),
            key_items: Vec::new(),
            items: vec![CiscoIpPhoneIconMenuItem {
                name: Some("Alex <Host>".into()),
                url: Some("UserData:1:0:participant/1".into()),
                icon_index: Some(5),
            }],
            icons: vec![CiscoIpPhoneIconFileItem {
                index: 5,
                url: "Resource:Icon.SecureCall?shade=dark&size=small".into(),
            }],
        };
        let xml = resources.to_xml().unwrap();
        assert!(xml.contains("<Title IconIndex=\"5\">Locked &amp; secure</Title>"));
        assert!(xml.contains("shade=dark&amp;size=small"));
        assert_eq!(
            CiscoIpPhoneIconFileMenu::from_xml(xml.as_bytes()).unwrap(),
            resources
        );
    }

    #[test]
    fn menu_models_reject_every_collection_text_url_position_and_icon_bound() {
        let mut basic = complete_menu();
        basic.items = vec![basic.items[0].clone(); PHONE_MENU_MAX_ITEMS + 1];
        assert!(matches!(
            basic.to_xml(),
            Err(PhoneXmlError::LimitExceeded {
                kind: "menu items",
                ..
            })
        ));

        let mut invalid = complete_menu();
        invalid.items[0].name = Some("x".repeat(65));
        assert!(matches!(
            invalid.to_xml(),
            Err(PhoneXmlError::InvalidField { .. })
        ));
        invalid = complete_menu();
        invalid.items[0].url = Some("x".repeat(PHONE_XML_URL_MAX_CHARS + 1));
        assert!(matches!(
            invalid.to_xml(),
            Err(PhoneXmlError::InvalidField { .. })
        ));
        invalid = complete_menu();
        invalid.application_id = Some(String::new());
        assert!(matches!(
            invalid.to_xml(),
            Err(PhoneXmlError::InvalidField { .. })
        ));
        invalid = complete_menu();
        invalid.on_closed = Some(String::new());
        assert!(matches!(
            invalid.to_xml(),
            Err(PhoneXmlError::InvalidField { .. })
        ));
        invalid = complete_menu();
        invalid.soft_keys[0].position = PhoneSoftKeyPosition::new(16).unwrap();
        assert!(invalid.to_xml().is_ok());
        invalid = complete_menu();
        invalid.soft_keys = vec![invalid.soft_keys[0].clone(); 17];
        assert!(matches!(
            invalid.to_xml(),
            Err(PhoneXmlError::LimitExceeded { .. })
        ));
        invalid = complete_menu();
        invalid.key_items = vec![invalid.key_items[0].clone(); 33];
        assert!(matches!(
            invalid.to_xml(),
            Err(PhoneXmlError::LimitExceeded { .. })
        ));

        let item = CiscoIpPhoneIconMenuItem {
            name: Some("Item".into()),
            url: Some("SoftKey:Select".into()),
            icon_index: Some(0),
        };
        let icon = CiscoIpPhoneIconItem {
            index: 0,
            width: 1,
            height: 1,
            depth: 1,
            data: Some("00".into()),
        };
        let mut icon_menu =
            CiscoIpPhoneIconMenu::new("Icons", "Choose", vec![item.clone()], vec![icon.clone()])
                .unwrap();
        icon_menu.items = vec![item.clone(); PHONE_ICON_MENU_MAX_ITEMS + 1];
        assert!(matches!(
            icon_menu.to_xml(),
            Err(PhoneXmlError::LimitExceeded { .. })
        ));
        icon_menu =
            CiscoIpPhoneIconMenu::new("Icons", "Choose", vec![item.clone()], vec![icon.clone()])
                .unwrap();
        icon_menu.icons = vec![icon.clone(); PHONE_ICON_MENU_MAX_ICONS + 1];
        assert!(matches!(
            icon_menu.to_xml(),
            Err(PhoneXmlError::LimitExceeded { .. })
        ));

        for invalid_icon in [
            CiscoIpPhoneIconItem {
                width: 0,
                ..icon.clone()
            },
            CiscoIpPhoneIconItem {
                height: 11,
                ..icon.clone()
            },
            CiscoIpPhoneIconItem {
                depth: 3,
                ..icon.clone()
            },
            CiscoIpPhoneIconItem {
                data: Some("0".into()),
                ..icon.clone()
            },
            CiscoIpPhoneIconItem {
                data: Some("GG".into()),
                ..icon.clone()
            },
            CiscoIpPhoneIconItem {
                data: Some("00".repeat(41)),
                ..icon
            },
        ] {
            assert!(
                CiscoIpPhoneIconMenu::new(
                    "Icons",
                    "Choose",
                    vec![item.clone()],
                    vec![invalid_icon]
                )
                .is_err()
            );
        }
        let mut invalid_item = item;
        invalid_item.icon_index = Some(10);
        assert!(
            CiscoIpPhoneIconMenu::new("Icons", "Choose", vec![invalid_item], vec![icon]).is_err()
        );

        let mut file_menu = CiscoIpPhoneIconFileMenu {
            keypad_target: None,
            application_id: None,
            on_focus_lost: None,
            on_focus_gained: None,
            on_minimized: None,
            on_closed: None,
            icon_index: None,
            title: None,
            prompt: None,
            soft_keys: Vec::new(),
            key_items: Vec::new(),
            items: Vec::new(),
            icons: vec![CiscoIpPhoneIconFileItem {
                index: 10,
                url: "Resource:Icon.Hold".into(),
            }],
        };
        assert!(matches!(
            file_menu.to_xml(),
            Err(PhoneXmlError::InvalidField { .. })
        ));
        file_menu.icons[0].index = 0;
        file_menu.icons[0].url.clear();
        assert!(matches!(
            file_menu.to_xml(),
            Err(PhoneXmlError::InvalidField { .. })
        ));
    }

    #[test]
    fn menu_parsers_reject_wrong_roots_unknown_fields_malformed_input_and_writer_failure() {
        assert!(CiscoIpPhoneMenu::from_xml(b"<CiscoIPPhoneIconMenu/>").is_err());
        assert!(CiscoIpPhoneIconMenu::from_xml(b"<CiscoIPPhoneMenu/>").is_err());
        assert!(CiscoIpPhoneIconFileMenu::from_xml(b"<CiscoIPPhoneIconMenu/>").is_err());
        assert!(
            CiscoIpPhoneMenu::from_xml(b"<CiscoIPPhoneMenu><Unknown/></CiscoIPPhoneMenu>",)
                .is_err()
        );
        assert!(CiscoIpPhoneIconMenu::from_xml(b"<CiscoIPPhoneIconMenu>").is_err());
        assert!(
            CiscoIpPhoneIconFileMenu::from_xml(b"<!DOCTYPE menu><CiscoIPPhoneIconFileMenu/>",)
                .is_err()
        );
        assert!(matches!(
            CiscoIpPhoneMenu::from_xml(&[0xff]),
            Err(PhoneXmlError::InvalidUtf8(_))
        ));
        assert!(matches!(
            complete_menu().to_xml_with_limit(10),
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
            to_writer(FailingWriter, &complete_menu(), PHONE_MENU_MAX_BYTES),
            Err(PhoneXmlError::Write(_))
        ));
    }

    #[test]
    fn conference_lists_round_trip_menu_and_icon_families_with_typed_actions() {
        let conference_id = ConferenceId::new(41);
        let participants = [
            ConferenceListEntry {
                participant_id: ParticipantId::new(7),
                name: "Alex <Host> & Co".into(),
                number: "2100".into(),
                moderator: true,
                muted: false,
            },
            ConferenceListEntry {
                participant_id: ParticipantId::new(8),
                name: String::new(),
                number: "2200".into(),
                moderator: false,
                muted: true,
            },
            ConferenceListEntry {
                participant_id: ParticipantId::new(9),
                name: "Casey".into(),
                number: "2300".into(),
                moderator: false,
                muted: false,
            },
        ];
        for family in [ConferenceMenuFamily::Menu, ConferenceMenuFamily::IconMenu] {
            let expected =
                ConferenceListDocument::new(conference_id, &participants, family).unwrap();
            let xml = expected.to_xml().unwrap();
            assert!(xml.contains("Alex &lt;Host&gt; &amp; Co"));
            let decoded = ConferenceListDocument::from_xml(xml.as_bytes(), family).unwrap();
            assert_eq!(decoded, expected);
            assert_eq!(
                decoded.actions().collect::<Vec<_>>(),
                [
                    ConferenceListAction::Participant {
                        conference_id,
                        participant_id: ParticipantId::new(7),
                    },
                    ConferenceListAction::Participant {
                        conference_id,
                        participant_id: ParticipantId::new(8),
                    },
                    ConferenceListAction::Participant {
                        conference_id,
                        participant_id: ParticipantId::new(9),
                    },
                    ConferenceListAction::End { conference_id },
                ]
            );
        }
    }

    #[test]
    fn conference_participant_actions_round_trip_both_families_and_removal_policy() {
        let conference_id = ConferenceId::new(41);
        let mut participant = ConferenceListEntry {
            participant_id: ParticipantId::new(8),
            name: "Alex <Admin> & Co".into(),
            number: "2200".into(),
            moderator: false,
            muted: false,
        };
        for family in [ConferenceMenuFamily::Menu, ConferenceMenuFamily::IconMenu] {
            let expected = ConferenceParticipantActionsDocument::new(
                conference_id,
                &participant,
                true,
                false,
                family,
            )
            .unwrap();
            let xml = expected.to_xml().unwrap();
            let decoded =
                ConferenceParticipantActionsDocument::from_xml(xml.as_bytes(), family).unwrap();
            assert_eq!(decoded, expected);
            assert_eq!(
                decoded.actions().collect::<Vec<_>>(),
                [
                    ConferenceListAction::Mute {
                        conference_id,
                        participant_id: participant.participant_id,
                    },
                    ConferenceListAction::Remove {
                        conference_id,
                        participant_id: participant.participant_id,
                    },
                    ConferenceListAction::Promote {
                        conference_id,
                        participant_id: participant.participant_id,
                    },
                ]
            );

            participant.muted = true;
            let not_removable = ConferenceParticipantActionsDocument::new(
                conference_id,
                &participant,
                false,
                false,
                family,
            )
            .unwrap();
            assert_eq!(
                not_removable.actions().collect::<Vec<_>>(),
                [
                    ConferenceListAction::Unmute {
                        conference_id,
                        participant_id: participant.participant_id,
                    },
                    ConferenceListAction::Promote {
                        conference_id,
                        participant_id: participant.participant_id,
                    },
                ]
            );
            participant.moderator = true;
            let demotable = ConferenceParticipantActionsDocument::new(
                conference_id,
                &participant,
                false,
                true,
                family,
            )
            .unwrap();
            assert_eq!(
                demotable.actions().collect::<Vec<_>>(),
                [ConferenceListAction::Demote {
                    conference_id,
                    participant_id: participant.participant_id,
                }]
            );
            let sole_moderator = ConferenceParticipantActionsDocument::new(
                conference_id,
                &participant,
                false,
                false,
                family,
            )
            .unwrap();
            assert!(sole_moderator.actions().next().is_none());
            participant.moderator = false;
            participant.muted = false;
        }
    }

    #[test]
    fn conference_lists_reject_limits_malformed_actions_and_wrong_family() {
        let participants = vec![
            ConferenceListEntry {
                participant_id: ParticipantId::new(1),
                name: "Participant".into(),
                number: String::new(),
                moderator: false,
                muted: false,
            };
            CONFERENCE_LIST_MAX_PARTICIPANTS + 1
        ];
        assert!(matches!(
            ConferenceListDocument::new(
                ConferenceId::new(1),
                &participants,
                ConferenceMenuFamily::Menu,
            ),
            Err(PhoneXmlError::LimitExceeded {
                kind: "conference participants",
                ..
            })
        ));
        assert!(ConferenceListAction::parse("conference/1/participant/not-a-number").is_none());
        assert!(ConferenceListAction::parse("conference/1/remove/7").is_none());
        assert_eq!(
            ConferenceListAction::parse("conference/1/participant/7/remove"),
            Some(ConferenceListAction::Remove {
                conference_id: ConferenceId::new(1),
                participant_id: ParticipantId::new(7),
            })
        );
        assert_eq!(
            ConferenceListAction::from_route(&[
                "conference".into(),
                "1".into(),
                "participant".into(),
                "7".into(),
                "remove".into(),
            ]),
            Some(ConferenceListAction::Remove {
                conference_id: ConferenceId::new(1),
                participant_id: ParticipantId::new(7),
            })
        );
        for (operation, expected) in [
            (
                "promote",
                ConferenceListAction::Promote {
                    conference_id: ConferenceId::new(1),
                    participant_id: ParticipantId::new(7),
                },
            ),
            (
                "demote",
                ConferenceListAction::Demote {
                    conference_id: ConferenceId::new(1),
                    participant_id: ParticipantId::new(7),
                },
            ),
        ] {
            let route = [
                "conference".into(),
                "1".into(),
                "participant".into(),
                "7".into(),
                operation.into(),
            ];
            assert_eq!(ConferenceListAction::from_route(&route), Some(expected));
        }

        let menu = ConferenceListDocument::new(
            ConferenceId::new(1),
            &participants[..1],
            ConferenceMenuFamily::Menu,
        )
        .unwrap()
        .to_xml()
        .unwrap();
        assert!(
            ConferenceListDocument::from_xml(menu.as_bytes(), ConferenceMenuFamily::IconMenu)
                .is_err()
        );
        assert!(
            ConferenceListDocument::from_xml(
                b"<!DOCTYPE menu><CiscoIPPhoneMenu/>",
                ConferenceMenuFamily::Menu,
            )
            .is_err()
        );
    }

    #[test]
    fn directory_schema_round_trips_entries_controls_attributes_and_escaping() {
        let expected = CiscoIpPhoneDirectory {
            keypad_target: Some(PhoneKeypadTarget::ApplicationCall),
            application_id: Some("directory-west".into()),
            on_focus_lost: Some("Notify:focus?state=lost&view=all".into()),
            on_focus_gained: None,
            on_minimized: None,
            on_closed: Some("SoftKey:Exit".into()),
            title: Some("R&D <West>".into()),
            prompt: Some("Choose A & B".into()),
            soft_keys: vec![CiscoIpPhoneSoftKeyItem {
                name: Some("Next".into()),
                position: PhoneSoftKeyPosition::new(3).unwrap(),
                url: Some("http://pbx.test/directory?page=2&query=R%26D".into()),
                url_down: None,
            }],
            key_items: vec![CiscoIpPhoneKeyItem {
                key: PhoneXmlKey::NavBack,
                url: Some("SoftKey:Cancel".into()),
                url_down: None,
            }],
            entries: vec![CiscoIpPhoneDirectoryEntry {
                name: Some("Alice <Admin> & Bob".into()),
                telephone: Some("1001&2".into()),
            }],
        };

        let xml = expected.to_xml().unwrap();
        assert!(xml.starts_with("<CiscoIPPhoneDirectory"));
        assert!(xml.contains("keypadTarget=\"applicationCall\""));
        assert!(xml.contains("R&amp;D &lt;West&gt;"));
        assert!(xml.contains("Alice &lt;Admin&gt; &amp; Bob"));
        assert_eq!(
            CiscoIpPhoneDirectory::from_xml(xml.as_bytes()).unwrap(),
            expected
        );
    }

    #[test]
    fn directory_schema_accepts_the_minimal_document_and_optionally_empty_fields() {
        let xml = b"<CiscoIPPhoneDirectory><Title/><Prompt/><DirectoryEntry><Name/><Telephone/></DirectoryEntry></CiscoIPPhoneDirectory>";
        let document = CiscoIpPhoneDirectory::from_xml(xml).unwrap();
        assert_eq!(document.title.as_deref(), Some(""));
        assert_eq!(document.prompt.as_deref(), Some(""));
        assert_eq!(document.entries.len(), 1);
        assert_eq!(document.entries[0].name.as_deref(), Some(""));
        assert_eq!(document.entries[0].telephone.as_deref(), Some(""));
    }

    #[test]
    fn directory_schema_enforces_entry_text_control_and_document_bounds() {
        let too_many = vec![
            CiscoIpPhoneDirectoryEntry {
                name: Some("Name".into()),
                telephone: Some("1000".into()),
            };
            PHONE_DIRECTORY_MAX_ENTRIES + 1
        ];
        assert!(matches!(
            CiscoIpPhoneDirectory::new("Directory", "Choose", too_many),
            Err(PhoneXmlError::LimitExceeded {
                kind: "directory entries",
                ..
            })
        ));

        let invalid = CiscoIpPhoneDirectory::new(
            "Directory",
            "Choose",
            vec![CiscoIpPhoneDirectoryEntry {
                name: Some("x".repeat(PHONE_DIRECTORY_TEXT_MAX_CHARS + 1)),
                telephone: Some("1000".into()),
            }],
        )
        .unwrap_err();
        assert!(matches!(invalid, PhoneXmlError::InvalidField { .. }));

        assert!(PhoneSoftKeyPosition::new(0).is_err());
        assert!(PhoneSoftKeyPosition::new(-1).is_ok());
        assert!(PhoneSoftKeyPosition::new(16).is_ok());
        assert!(PhoneSoftKeyPosition::new(17).is_err());

        assert!(
            CiscoIpPhoneDirectory::from_xml(b"<!DOCTYPE directory><CiscoIPPhoneDirectory/>",)
                .is_err()
        );
        assert!(matches!(
            CiscoIpPhoneDirectory::from_xml(&vec![b'x'; PHONE_DIRECTORY_MAX_BYTES + 1]),
            Err(PhoneXmlError::LimitExceeded { .. })
        ));
    }
}
