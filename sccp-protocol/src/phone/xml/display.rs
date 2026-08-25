//! Display phone XML document family.

use super::*;

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
}
