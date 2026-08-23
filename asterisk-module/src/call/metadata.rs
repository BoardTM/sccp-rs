//! Bounded, backend-neutral metadata carried by one PBX channel.

use std::collections::BTreeSet;
use std::fmt;

use thiserror::Error;

use sccp_protocol::CallDirection;

use crate::pbx::party::{NumberPlan, PartyIdentity, Presentation};

pub const MAX_PARTY_TEXT_BYTES: usize = 79;
pub const MAX_ACCOUNT_CODE_BYTES: usize = 79;
pub const MAX_LANGUAGE_BYTES: usize = 63;
pub const MAX_VARIABLES: usize = 32;
pub const MAX_VARIABLE_NAME_BYTES: usize = 79;
pub const MAX_VARIABLE_VALUE_BYTES: usize = 1024;
pub const MAX_VARIABLE_AGGREGATE_BYTES: usize = 8192;

#[derive(Clone, Eq, PartialEq)]
pub struct ChannelVariable {
    name: String,
    value: String,
}

impl ChannelVariable {
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Result<Self, MetadataError> {
        let name = name.into();
        let value = value.into();
        validate_variable_name(&name)?;
        if value.is_empty() {
            return Err(MetadataError::InvalidText(MetadataField::VariableValue));
        }
        validate_text(
            &value,
            MAX_VARIABLE_VALUE_BYTES,
            MetadataField::VariableValue,
        )?;
        Ok(Self { name, value })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn value(&self) -> &str {
        &self.value
    }
}

impl fmt::Debug for ChannelVariable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ChannelVariable")
            .field("name", &"<redacted>")
            .field("value", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Default, Eq, PartialEq)]
pub struct CallMetadata {
    pub ani: PartyIdentity,
    /// Asterisk treats a null and empty DNID as equivalent, but the boundary
    /// keeps the distinction so snapshots remain lossless.
    pub dnid: Option<String>,
    pub dnid_plan: NumberPlan,
    pub rdnis: PartyIdentity,
    pub account_code: Option<String>,
    pub language: Option<String>,
    pub variables: Vec<ChannelVariable>,
}

impl CallMetadata {
    pub fn validate(&self) -> Result<(), MetadataError> {
        validate_party(&self.ani, MetadataField::Ani)?;
        validate_optional(&self.dnid, MAX_PARTY_TEXT_BYTES, MetadataField::Dnid)?;
        validate_party(&self.rdnis, MetadataField::Rdnis)?;
        validate_optional(
            &self.account_code,
            MAX_ACCOUNT_CODE_BYTES,
            MetadataField::AccountCode,
        )?;
        validate_optional(&self.language, MAX_LANGUAGE_BYTES, MetadataField::Language)?;
        for (value, field) in [
            (&self.account_code, MetadataField::AccountCode),
            (&self.language, MetadataField::Language),
        ] {
            if value.as_deref() == Some("") {
                return Err(MetadataError::InvalidText(field));
            }
        }
        if self.variables.len() > MAX_VARIABLES {
            return Err(MetadataError::TooManyVariables);
        }
        let mut names = BTreeSet::new();
        let mut aggregate = 0_usize;
        for variable in &self.variables {
            validate_variable_name(variable.name())?;
            if variable.value().is_empty() {
                return Err(MetadataError::InvalidText(MetadataField::VariableValue));
            }
            validate_text(
                variable.value(),
                MAX_VARIABLE_VALUE_BYTES,
                MetadataField::VariableValue,
            )?;
            if !names.insert(variable.name()) {
                return Err(MetadataError::DuplicateVariable);
            }
            aggregate = aggregate
                .checked_add(variable.name().len())
                .and_then(|size| size.checked_add(variable.value().len()))
                .ok_or(MetadataError::VariablesTooLarge)?;
        }
        if aggregate > MAX_VARIABLE_AGGREGATE_BYTES {
            return Err(MetadataError::VariablesTooLarge);
        }
        Ok(())
    }

    pub fn visible_ani_number(&self) -> Option<&str> {
        self.ani.visible_number()
    }

    pub fn visible_rdnis_number(&self) -> Option<&str> {
        self.rdnis.visible_number()
    }
}

pub struct ConfiguredChannelMetadata<'a> {
    pub direction: CallDirection,
    pub caller_number: &'a str,
    pub dialed_number: Option<&'a str>,
    pub account_code: Option<&'a str>,
    pub language: &'a str,
    pub device_variables: &'a [ChannelVariable],
    pub line_variables: &'a [ChannelVariable],
}

pub fn configured_channel_metadata(
    mut metadata: CallMetadata,
    configured: ConfiguredChannelMetadata<'_>,
) -> Result<CallMetadata, MetadataError> {
    if configured.direction == CallDirection::Outbound {
        metadata.ani = PartyIdentity {
            number: Some(configured.caller_number.to_owned()),
            number_presentation: Presentation::ALLOWED_NOT_SCREENED,
            ..PartyIdentity::default()
        };
        metadata.dnid = configured.dialed_number.map(str::to_owned);
    }
    metadata.account_code = configured.account_code.map(str::to_owned);
    metadata.language = Some(configured.language.to_owned());
    metadata.variables = if configured.direction == CallDirection::Outbound {
        let mut variables = configured.device_variables.to_vec();
        for line_variable in configured.line_variables {
            variables.retain(|variable| variable.name() != line_variable.name());
            variables.push(line_variable.clone());
        }
        variables
    } else {
        configured.line_variables.to_vec()
    };
    metadata.validate()?;
    Ok(metadata)
}

impl fmt::Debug for CallMetadata {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CallMetadata")
            .field("ani", &redacted_party(&self.ani))
            .field("dnid", &self.dnid.as_ref().map(|_| "<redacted>"))
            .field("rdnis", &redacted_party(&self.rdnis))
            .field(
                "account_code",
                &self.account_code.as_ref().map(|_| "<redacted>"),
            )
            .field("language", &self.language)
            .field("variable_count", &self.variables.len())
            .finish()
    }
}

fn redacted_party(party: &PartyIdentity) -> Option<&'static str> {
    (party.name.is_some() || party.number.is_some()).then_some(if party.is_restricted() {
        "<restricted>"
    } else {
        "<redacted>"
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetadataField {
    Ani,
    Dnid,
    Rdnis,
    AccountCode,
    Language,
    VariableName,
    VariableValue,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum MetadataError {
    #[error("{0:?} exceeds its bounded length")]
    TooLong(MetadataField),
    #[error("{0:?} contains invalid text")]
    InvalidText(MetadataField),
    #[error("channel variable name is invalid")]
    InvalidVariableName,
    #[error("sensitive channel variable names are not accepted")]
    SensitiveVariableName,
    #[error("channel variable name is duplicated")]
    DuplicateVariable,
    #[error("too many channel variables")]
    TooManyVariables,
    #[error("channel variable aggregate exceeds its bound")]
    VariablesTooLarge,
}

fn validate_party(party: &PartyIdentity, field: MetadataField) -> Result<(), MetadataError> {
    validate_optional(&party.name, MAX_PARTY_TEXT_BYTES, field)?;
    validate_optional(&party.number, MAX_PARTY_TEXT_BYTES, field)
}

fn validate_optional(
    value: &Option<String>,
    max_bytes: usize,
    field: MetadataField,
) -> Result<(), MetadataError> {
    value
        .as_deref()
        .map(|value| validate_text(value, max_bytes, field))
        .transpose()
        .map(|_| ())
}

fn validate_text(value: &str, max_bytes: usize, field: MetadataField) -> Result<(), MetadataError> {
    if value.len() > max_bytes {
        return Err(MetadataError::TooLong(field));
    }
    if value
        .chars()
        .any(|character| character == '\0' || (character.is_control() && character != '\t'))
    {
        return Err(MetadataError::InvalidText(field));
    }
    Ok(())
}

fn validate_variable_name(name: &str) -> Result<(), MetadataError> {
    if name.is_empty() || name.len() > MAX_VARIABLE_NAME_BYTES {
        return Err(MetadataError::InvalidVariableName);
    }
    let mut characters = name.chars();
    if !characters
        .next()
        .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
        || characters.any(|character| character != '_' && !character.is_ascii_alphanumeric())
    {
        return Err(MetadataError::InvalidVariableName);
    }
    let normalized = name.trim_start_matches('_').to_ascii_lowercase();
    if [
        "password",
        "passwd",
        "secret",
        "token",
        "authorization",
        "credential",
    ]
    .iter()
    .any(|word| normalized.contains(word))
    {
        return Err(MetadataError::SensitiveVariableName);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pbx::party::Presentation;

    #[test]
    fn preserves_absent_and_empty_dnid_but_debug_redacts_values() {
        let absent = CallMetadata::default();
        let empty = CallMetadata {
            dnid: Some(String::new()),
            account_code: Some("billing-private".into()),
            variables: vec![ChannelVariable::new("PUBLIC_ID", "private-value").unwrap()],
            ..CallMetadata::default()
        };
        assert_ne!(absent, empty);
        assert!(empty.validate().is_ok());
        let debug = format!("{empty:?}");
        assert!(!debug.contains("billing-private"));
        assert!(!debug.contains("PUBLIC_ID"));
        assert!(!debug.contains("private-value"));
    }

    #[test]
    fn presentation_controls_diagnostic_visibility() {
        let metadata = CallMetadata {
            ani: PartyIdentity {
                number: Some("12065550100".into()),
                number_presentation: Presentation::RESTRICTED_NOT_SCREENED,
                ..PartyIdentity::default()
            },
            rdnis: PartyIdentity {
                number: Some("12065550101".into()),
                number_presentation: Presentation::ALLOWED_PASSED_SCREEN,
                ..PartyIdentity::default()
            },
            ..CallMetadata::default()
        };
        assert_eq!(metadata.visible_ani_number(), None);
        assert_eq!(metadata.visible_rdnis_number(), Some("12065550101"));
        assert!(!format!("{metadata:?}").contains("12065550101"));
    }

    #[test]
    fn variables_reject_duplicates_functions_sensitive_names_and_bounds() {
        for name in ["", "FUNC(foo)", "MY-PARAM", "AUTHORIZATION_TOKEN"] {
            assert!(ChannelVariable::new(name, "value").is_err(), "{name}");
        }
        let duplicate = CallMetadata {
            variables: vec![
                ChannelVariable::new("PARAM", "one").unwrap(),
                ChannelVariable::new("PARAM", "two").unwrap(),
            ],
            ..CallMetadata::default()
        };
        assert_eq!(duplicate.validate(), Err(MetadataError::DuplicateVariable));
        assert!(matches!(
            ChannelVariable::new("PARAM", "x".repeat(MAX_VARIABLE_VALUE_BYTES + 1)),
            Err(MetadataError::TooLong(MetadataField::VariableValue))
        ));
        assert!(matches!(
            ChannelVariable::new("PARAM", ""),
            Err(MetadataError::InvalidText(MetadataField::VariableValue))
        ));
        for invalid in [
            CallMetadata {
                account_code: Some(String::new()),
                ..CallMetadata::default()
            },
            CallMetadata {
                language: Some(String::new()),
                ..CallMetadata::default()
            },
        ] {
            assert!(matches!(
                invalid.validate(),
                Err(MetadataError::InvalidText(_))
            ));
        }
    }

    #[test]
    fn configured_metadata_preserves_inbound_party_fields_and_orders_outbound_overrides() {
        let inherited = CallMetadata {
            ani: PartyIdentity {
                number: Some("12065550100".into()),
                ..PartyIdentity::default()
            },
            dnid: Some("1001".into()),
            rdnis: PartyIdentity {
                number: Some("1000".into()),
                ..PartyIdentity::default()
            },
            ..CallMetadata::default()
        };
        let device = [
            ChannelVariable::new("CLASS", "device").unwrap(),
            ChannelVariable::new("DEVICE_ONLY", "yes").unwrap(),
        ];
        let line = [ChannelVariable::new("CLASS", "line").unwrap()];

        let inbound = configured_channel_metadata(
            inherited.clone(),
            ConfiguredChannelMetadata {
                direction: CallDirection::Inbound,
                caller_number: "ignored",
                dialed_number: Some("ignored"),
                account_code: Some("billing"),
                language: "sv",
                device_variables: &device,
                line_variables: &line,
            },
        )
        .unwrap();
        assert_eq!(inbound.ani, inherited.ani);
        assert_eq!(inbound.dnid, inherited.dnid);
        assert_eq!(inbound.rdnis, inherited.rdnis);
        assert_eq!(inbound.variables, line);

        let outbound = configured_channel_metadata(
            CallMetadata::default(),
            ConfiguredChannelMetadata {
                direction: CallDirection::Outbound,
                caller_number: "1001",
                dialed_number: Some("12065550102"),
                account_code: None,
                language: "en",
                device_variables: &device,
                line_variables: &line,
            },
        )
        .unwrap();
        assert_eq!(outbound.visible_ani_number(), Some("1001"));
        assert_eq!(outbound.dnid.as_deref(), Some("12065550102"));
        assert_eq!(
            outbound
                .variables
                .iter()
                .map(|variable| (variable.name(), variable.value()))
                .collect::<Vec<_>>(),
            [("DEVICE_ONLY", "yes"), ("CLASS", "line")]
        );
    }
}
