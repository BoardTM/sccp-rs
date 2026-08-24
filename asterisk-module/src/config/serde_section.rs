//! Serde data-format adapter for one ordered Asterisk configuration section.
//!
//! A section is deliberately not converted to a generic map. Repeated values
//! retain their source order and are exposed as a Serde sequence, while scalar
//! fields reject duplicates. Key matching follows Asterisk: ASCII
//! case-insensitive, with punctuation left significant.

use std::collections::HashMap;
use std::fmt;

use serde::Serialize;
use serde::de::{
    self, DeserializeOwned, DeserializeSeed, EnumAccess, IntoDeserializer, MapAccess, SeqAccess,
    VariantAccess, Visitor,
};

use super::{ConfigError, RawSection, RawValue};

const REDACTED: &str = "<redacted>";

pub(super) struct TypedEntry<'a, K> {
    pub key: K,
    pub source: &'a RawValue,
}

impl<'a, K> TypedEntry<'a, K> {
    pub fn value(&self) -> &'a str {
        &self.source.value
    }
}

/// Decode option identifiers through a Serde-derived enum while preserving
/// every occurrence and its original order.
pub(super) fn deserialize_entries<'a, K>(
    section: &'a RawSection,
) -> Result<Vec<TypedEntry<'a, K>>, ConfigError>
where
    K: DeserializeOwned,
{
    section
        .values
        .iter()
        .map(|entry| {
            let canonical = entry.key.to_ascii_lowercase();
            K::deserialize(canonical.into_deserializer())
                .map(|key| TypedEntry { key, source: entry })
                .map_err(|error: SectionSerdeError| ConfigError::InvalidValue {
                    key: entry.diagnostic_key(),
                    value: if super::section_values::sensitive_option_name(&entry.key) {
                        format!("{REDACTED}; {error}")
                    } else {
                        format!("{:?}; {error}", entry.value)
                    },
                })
        })
        .collect()
}

pub(super) fn serialized_key<K: Serialize>(key: &K) -> Result<String, ConfigError> {
    let value = serde_json::to_value(key).map_err(|error| ConfigError::InvalidValue {
        key: "configuration schema".into(),
        value: error.to_string(),
    })?;
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| ConfigError::InvalidValue {
            key: "configuration schema".into(),
            value: "Serde option identifiers must serialize as strings".into(),
        })
}

#[derive(Debug)]
struct SectionSerdeError {
    location: Option<String>,
    message: String,
}

impl fmt::Display for SectionSerdeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for SectionSerdeError {}

impl de::Error for SectionSerdeError {
    fn custom<T: fmt::Display>(message: T) -> Self {
        Self {
            location: None,
            message: message.to_string(),
        }
    }
}

pub(super) fn deserialize_section<T>(section: &RawSection) -> Result<T, ConfigError>
where
    T: DeserializeOwned,
{
    T::deserialize(SectionDeserializer::new(section)).map_err(|error| {
        let key = error.location.unwrap_or_else(|| {
            let unknown = error
                .message
                .strip_prefix("unknown field `")
                .and_then(|message| message.split_once('`'))
                .map(|(field, _)| field);
            unknown
                .and_then(|field| {
                    section
                        .values
                        .iter()
                        .find(|entry| entry.key.eq_ignore_ascii_case(field))
                })
                .map_or_else(|| section.section_location(), RawValue::diagnostic_key)
        });
        ConfigError::InvalidValue {
            key,
            value: error.message,
        }
    })
}

struct EntryGroup<'a> {
    canonical_key: String,
    entries: Vec<&'a RawValue>,
}

struct SectionDeserializer<'a> {
    section: &'a RawSection,
}

impl<'a> SectionDeserializer<'a> {
    fn new(section: &'a RawSection) -> Self {
        Self { section }
    }

    fn groups(&self) -> Vec<EntryGroup<'a>> {
        let mut positions = HashMap::<String, usize>::new();
        let mut groups = Vec::<EntryGroup<'a>>::new();
        for entry in &self.section.values {
            let canonical_key = entry.key.to_ascii_lowercase();
            if let Some(position) = positions.get(&canonical_key).copied() {
                groups[position].entries.push(entry);
            } else {
                positions.insert(canonical_key.clone(), groups.len());
                groups.push(EntryGroup {
                    canonical_key,
                    entries: vec![entry],
                });
            }
        }
        groups
    }
}

impl<'de> de::Deserializer<'de> for SectionDeserializer<'de> {
    type Error = SectionSerdeError;

    fn deserialize_any<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_map(visitor)
    }

    fn deserialize_map<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_map(SectionMapAccess {
            groups: self.groups().into_iter(),
            current: None,
        })
    }

    fn deserialize_struct<V>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_map(visitor)
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple tuple_struct
        enum identifier ignored_any
    }
}

struct SectionMapAccess<'a> {
    groups: std::vec::IntoIter<EntryGroup<'a>>,
    current: Option<EntryGroup<'a>>,
}

impl<'de> MapAccess<'de> for SectionMapAccess<'de> {
    type Error = SectionSerdeError;

    fn next_key_seed<K>(&mut self, seed: K) -> Result<Option<K::Value>, Self::Error>
    where
        K: DeserializeSeed<'de>,
    {
        let Some(group) = self.groups.next() else {
            return Ok(None);
        };
        let key = group.canonical_key.clone();
        self.current = Some(group);
        seed.deserialize(key.into_deserializer()).map(Some)
    }

    fn next_value_seed<V>(&mut self, seed: V) -> Result<V::Value, Self::Error>
    where
        V: DeserializeSeed<'de>,
    {
        let group = self
            .current
            .take()
            .expect("Serde asks for a value after accepting a section key");
        seed.deserialize(EntryValueDeserializer {
            key: group.canonical_key,
            entries: group.entries,
        })
    }
}

struct EntryValueDeserializer<'a> {
    key: String,
    entries: Vec<&'a RawValue>,
}

impl<'a> EntryValueDeserializer<'a> {
    fn scalar(self) -> Result<&'a str, SectionSerdeError> {
        if self.entries.len() != 1 {
            let location = self
                .entries
                .last()
                .map_or_else(|| self.key.clone(), |entry| entry.diagnostic_key());
            return Err(SectionSerdeError {
                location: Some(location),
                message: format!("duplicate scalar option {}; expected one value", self.key),
            });
        }
        Ok(self.entries[0].value.as_str())
    }
}

impl<'de> de::Deserializer<'de> for EntryValueDeserializer<'de> {
    type Error = SectionSerdeError;

    fn deserialize_any<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        if self.entries.len() == 1 {
            visitor.visit_borrowed_str(self.entries[0].value.as_str())
        } else {
            self.deserialize_seq(visitor)
        }
    }

    fn deserialize_str<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_borrowed_str(self.scalar()?)
    }

    fn deserialize_string<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_string(self.scalar()?.to_owned())
    }

    fn deserialize_option<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_some(self)
    }

    fn deserialize_seq<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_seq(EntrySeqAccess {
            entries: self.entries.into_iter(),
        })
    }

    fn deserialize_enum<V>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_enum(StringEnumAccess(self.scalar()?))
    }

    fn deserialize_newtype_struct<V>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_newtype_struct(self)
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char bytes byte_buf
        unit unit_struct tuple tuple_struct map struct identifier ignored_any
    }
}

struct EntrySeqAccess<'a> {
    entries: std::vec::IntoIter<&'a RawValue>,
}

impl<'de> SeqAccess<'de> for EntrySeqAccess<'de> {
    type Error = SectionSerdeError;

    fn next_element_seed<T>(&mut self, seed: T) -> Result<Option<T::Value>, Self::Error>
    where
        T: DeserializeSeed<'de>,
    {
        let Some(entry) = self.entries.next() else {
            return Ok(None);
        };
        seed.deserialize(entry.value.as_str().into_deserializer())
            .map(Some)
    }
}

struct StringEnumAccess<'a>(&'a str);

impl<'de> EnumAccess<'de> for StringEnumAccess<'de> {
    type Error = SectionSerdeError;
    type Variant = StringVariantAccess;

    fn variant_seed<V>(self, seed: V) -> Result<(V::Value, Self::Variant), Self::Error>
    where
        V: DeserializeSeed<'de>,
    {
        seed.deserialize(self.0.into_deserializer())
            .map(|value| (value, StringVariantAccess))
    }
}

struct StringVariantAccess;

impl<'de> VariantAccess<'de> for StringVariantAccess {
    type Error = SectionSerdeError;

    fn unit_variant(self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn newtype_variant_seed<T>(self, _seed: T) -> Result<T::Value, Self::Error>
    where
        T: DeserializeSeed<'de>,
    {
        Err(SectionSerdeError {
            location: None,
            message: "expected a unit enum variant".into(),
        })
    }

    fn tuple_variant<V>(self, _len: usize, _visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        Err(SectionSerdeError {
            location: None,
            message: "expected a unit enum variant".into(),
        })
    }

    fn struct_variant<V>(
        self,
        _fields: &'static [&'static str],
        _visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        Err(SectionSerdeError {
            location: None,
            message: "expected a unit enum variant".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    #[derive(Debug, Deserialize, Eq, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct Example {
        scalar_value: Option<String>,
        repeated: Vec<String>,
    }

    fn section(values: &[(&str, &str)]) -> RawSection {
        RawSection {
            name: "example".into(),
            line: 1,
            values: values
                .iter()
                .enumerate()
                .map(|(index, (key, value))| RawValue {
                    key: (*key).into(),
                    value: (*value).into(),
                    line: index + 2,
                    section: "example".into(),
                })
                .collect(),
            ..RawSection::default()
        }
    }

    #[test]
    fn matching_is_case_insensitive_but_punctuation_is_significant() {
        let actual: Example = deserialize_section(&section(&[
            ("ScAlAr_VaLuE", "one"),
            ("repeated", "two"),
            ("REPEATED", "three"),
        ]))
        .unwrap();
        assert_eq!(
            actual,
            Example {
                scalar_value: Some("one".into()),
                repeated: vec!["two".into(), "three".into()],
            }
        );

        let error = deserialize_section::<Example>(&section(&[("scalar-value", "one")]))
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown field"), "{error}");
    }

    #[test]
    fn duplicate_scalars_are_rejected_at_the_source_location() {
        let error = deserialize_section::<Example>(&section(&[
            ("scalar_value", "one"),
            ("SCALAR_VALUE", "two"),
        ]))
        .unwrap_err()
        .to_string();
        assert!(error.contains("line 3 [example].SCALAR_VALUE"), "{error}");
        assert!(error.contains("duplicate scalar"), "{error}");
    }
}
