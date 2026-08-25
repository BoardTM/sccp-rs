//! Status phone XML document family.

use super::*;

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
