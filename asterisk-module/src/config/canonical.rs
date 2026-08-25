//! Formatting and identity rules for canonical configuration output.

use serde::Serialize;

use super::{
    ConfigError, DeviceOption, GeneralOption, LineOption, RawSection, SoftKeyProfileSection,
    deserialize_entries, deserialize_section, invalid_option, section_values, serialized_key,
};

pub(super) fn value(value: &str) -> String {
    if value.trim() != value || value.contains(';') || value.starts_with('#') {
        format!("\"{}\"", value.replace('"', "\\\""))
    } else {
        value.to_owned()
    }
}

pub(super) fn profile_name(raw: &str) -> String {
    raw.trim().to_ascii_lowercase()
}

#[derive(Debug)]
pub(super) struct CanonicalEntry<'a> {
    pub(super) key: String,
    pub(super) value: &'a str,
}

fn canonical_section_kind(section: &RawSection) -> Result<&str, ConfigError> {
    if section.name.eq_ignore_ascii_case("general") {
        return Ok("general");
    }
    super::value(section, "type")
        .ok_or_else(|| ConfigError::MissingSectionType(section.name.clone()))
}

pub(super) fn canonical_section_rank(section: &RawSection) -> u8 {
    match canonical_section_kind(section)
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "general" => 0,
        "softkey_profile" => 1,
        "device" => 2,
        "line" => 3,
        _ => 4,
    }
}

pub(super) fn canonical_section_entries(
    section: &RawSection,
) -> Result<Vec<CanonicalEntry<'_>>, ConfigError> {
    fn typed<'a, K>(section: &'a RawSection) -> Result<Vec<CanonicalEntry<'a>>, ConfigError>
    where
        K: serde::de::DeserializeOwned + Serialize,
    {
        deserialize_entries::<K>(section)?
            .into_iter()
            .map(|entry| {
                Ok(CanonicalEntry {
                    key: serialized_key(&entry.key)?,
                    value: entry.value(),
                })
            })
            .collect()
    }

    match canonical_section_kind(section)?
        .to_ascii_lowercase()
        .as_str()
    {
        "general" => typed::<GeneralOption>(section),
        "device" => typed::<DeviceOption>(section),
        "line" => typed::<LineOption>(section),
        "softkey_profile" => {
            let _: SoftKeyProfileSection = deserialize_section(section)?;
            Ok(section
                .values
                .iter()
                .map(|entry| CanonicalEntry {
                    key: entry.key.to_ascii_lowercase(),
                    value: entry.value.as_str(),
                })
                .collect())
        }
        kind => Err(ConfigError::UnknownSectionType {
            section: section.name.clone(),
            kind: kind.to_owned(),
        }),
    }
}

pub(super) fn source_section_kind(
    section: &RawSection,
    sections: &[RawSection],
) -> Result<String, ConfigError> {
    if section.name.eq_ignore_ascii_case("general") {
        return Ok("general".into());
    }
    if let Some(kind) = super::value(section, "type") {
        return Ok(kind.to_ascii_lowercase());
    }
    for parent in &section.parents {
        let parent = sections
            .iter()
            .find(|candidate| candidate.name.eq_ignore_ascii_case(parent))
            .ok_or_else(|| ConfigError::MissingTemplate {
                section: section.name.clone(),
                parent: parent.clone(),
            })?;
        if let Ok(kind) = source_section_kind(parent, sections) {
            return Ok(kind);
        }
    }
    Err(ConfigError::MissingSectionType(section.name.clone()))
}

pub(super) fn check_canonical_section(section: &RawSection, kind: &str) -> Result<(), ConfigError> {
    let canonical_entries = match kind {
        "general" => canonical_typed_entries::<GeneralOption>(section)?,
        "device" => canonical_typed_entries::<DeviceOption>(section)?,
        "line" => canonical_typed_entries::<LineOption>(section)?,
        "softkey_profile" => {
            let _: SoftKeyProfileSection = deserialize_section(section)?;
            section
                .values
                .iter()
                .map(|entry| CanonicalEntry {
                    key: entry.key.to_ascii_lowercase(),
                    value: entry.value.as_str(),
                })
                .collect()
        }
        other => {
            return Err(ConfigError::UnknownSectionType {
                section: section.name.clone(),
                kind: other.to_owned(),
            });
        }
    };
    for (entry, canonical) in section.values.iter().zip(canonical_entries) {
        if entry.key != canonical.key {
            return Err(invalid_option(
                entry.diagnostic_key(),
                &entry.value,
                &format!("canonical option name {}", canonical.key),
                section_values::sensitive_option_name(&entry.key),
            ));
        }
    }
    Ok(())
}

fn canonical_typed_entries<K>(section: &RawSection) -> Result<Vec<CanonicalEntry<'_>>, ConfigError>
where
    K: serde::de::DeserializeOwned + Serialize,
{
    deserialize_entries::<K>(section)?
        .into_iter()
        .map(|entry| {
            Ok(CanonicalEntry {
                key: serialized_key(&entry.key)?,
                value: entry.value(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_text_quotes_only_ambiguous_values() {
        assert_eq!(value("plain"), "plain");
        assert_eq!(value(" leading"), "\" leading\"");
        assert_eq!(value("a;b"), "\"a;b\"");
        assert_eq!(value("#comment"), "\"#comment\"");
        assert_eq!(profile_name(" Main "), "main");
    }
}
