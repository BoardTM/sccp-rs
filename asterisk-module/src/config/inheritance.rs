//! Template merge policy, kept separate from final configuration assembly.

use std::collections::{HashMap, HashSet};

use super::{ConfigError, RawSection, RawValue, normalize_name, value};

pub(super) fn merge_values(merged: &mut Vec<RawValue>, incoming: &[RawValue], kind: TemplateKind) {
    let mut overridden = HashSet::new();
    for value in incoming {
        let identity = option_identity(kind, &value.key);
        let repeated = matches!(
            identity.as_str(),
            "allow" | "disallow" | "deny" | "permit" | "permithost" | "setvar"
        ) || kind == TemplateKind::Device
            && matches!(identity.as_str(), "button" | "line" | "featuredefault");
        if !repeated && overridden.insert(identity.clone()) {
            merged.retain(|candidate| option_identity(kind, &candidate.key) != identity);
        }
        merged.push(value.clone());
    }
}

pub(super) fn option_identity(kind: TemplateKind, key: &str) -> String {
    let normalized = normalize_name(key);
    match (kind, normalized.as_str()) {
        (TemplateKind::Device, "forwardallenabled") => "cfwdall".into(),
        (TemplateKind::Device, "forwardbusyenabled") => "cfwdbusy".into(),
        (TemplateKind::Device, "forwardnoanswerenabled") => "cfwdnoanswer".into(),
        (TemplateKind::Device, "forwardnoanswertimeout") => "cfwdnoanswertimeout".into(),
        (TemplateKind::Device, "privacyfeature") => "private".into(),
        (TemplateKind::Device, "transportrequirement") => "transport".into(),
        (TemplateKind::Device, "signalingtos" | "sccpdscp" | "signalingdscp") => "sccptos".into(),
        (TemplateKind::Device, "signalingcos") => "sccpcos".into(),
        (TemplateKind::Device, "audiodscp") => "audiotos".into(),
        (TemplateKind::Device, "videodscp") => "videotos".into(),
        (TemplateKind::Line, "voicemailnumber") => "vmnum".into(),
        (TemplateKind::Line, "voicemailtransfer" | "transfertovoicemail") => "trnsfvm".into(),
        (TemplateKind::Line, "directedpickupmodeanswer") => "pickupmodeanswer".into(),
        _ => normalized,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TemplateKind {
    Device,
    Line,
}

impl TemplateKind {
    fn from_name(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "device" => Some(Self::Device),
            "line" => Some(Self::Line),
            _ => None,
        }
    }

    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Device => "device",
            Self::Line => "line",
        }
    }
}

pub(super) fn resolve_inheritance(
    sections: Vec<RawSection>,
) -> Result<Vec<RawSection>, ConfigError> {
    let indexes: HashMap<_, _> = sections
        .iter()
        .enumerate()
        .map(|(index, section)| (section.name.to_ascii_lowercase(), index))
        .collect();
    let mut states = vec![0_u8; sections.len()];
    let mut resolved = vec![None; sections.len()];
    let mut stack = Vec::new();

    for index in 0..sections.len() {
        resolve_section(
            index,
            &sections,
            &indexes,
            &mut states,
            &mut resolved,
            &mut stack,
        )?;
    }

    Ok(sections
        .iter()
        .enumerate()
        .filter(|(_, section)| !section.is_template)
        .filter_map(|(index, _)| resolved[index].clone())
        .collect())
}

fn resolve_section(
    index: usize,
    sections: &[RawSection],
    indexes: &HashMap<String, usize>,
    states: &mut [u8],
    resolved: &mut [Option<RawSection>],
    stack: &mut Vec<usize>,
) -> Result<RawSection, ConfigError> {
    if states[index] == 2 {
        return Ok(resolved[index]
            .as_ref()
            .expect("resolved inheritance state has a value")
            .clone());
    }
    if states[index] == 1 {
        let start = stack
            .iter()
            .position(|candidate| *candidate == index)
            .unwrap_or(0);
        let mut cycle: Vec<_> = stack[start..]
            .iter()
            .map(|candidate| sections[*candidate].name.as_str())
            .collect();
        cycle.push(&sections[index].name);
        return Err(ConfigError::InheritanceCycle(cycle.join(" -> ")));
    }

    states[index] = 1;
    stack.push(index);
    let section = &sections[index];
    let own_kind_name = value(section, "type").map(str::trim);
    let own_kind = own_kind_name.and_then(TemplateKind::from_name);
    let mut inherited_kind = own_kind;
    let mut values = Vec::new();

    for parent_name in &section.parents {
        let canonical = parent_name.to_ascii_lowercase();
        let parent_index =
            indexes
                .get(&canonical)
                .copied()
                .ok_or_else(|| ConfigError::MissingTemplate {
                    section: section.name.clone(),
                    parent: parent_name.clone(),
                })?;
        let parent_source = &sections[parent_index];
        if !parent_source.is_template {
            return Err(ConfigError::ParentIsNotTemplate {
                section: section.name.clone(),
                parent: parent_source.name.clone(),
            });
        }
        let parent = resolve_section(parent_index, sections, indexes, states, resolved, stack)?;
        let parent_kind = resolved_template_kind(&parent)?;
        if let Some(child_kind) = inherited_kind {
            if child_kind != parent_kind {
                return Err(ConfigError::WrongTemplateKind {
                    section: section.name.clone(),
                    child_kind: child_kind.as_str().into(),
                    parent: parent.name.clone(),
                    parent_kind: parent_kind.as_str().into(),
                });
            }
        } else if let Some(kind) = own_kind_name {
            return Err(ConfigError::WrongTemplateKind {
                section: section.name.clone(),
                child_kind: kind.to_owned(),
                parent: parent.name.clone(),
                parent_kind: parent_kind.as_str().into(),
            });
        } else {
            inherited_kind = Some(parent_kind);
        }
        merge_values(&mut values, &parent.values, parent_kind);
    }

    if section.parents.is_empty() {
        values.clone_from(&section.values);
    } else if let Some(kind) = inherited_kind {
        merge_values(&mut values, &section.values, kind);
    } else {
        values.clone_from(&section.values);
    }
    let result = RawSection {
        name: section.name.clone(),
        line: section.line,
        is_template: section.is_template,
        parents: section.parents.clone(),
        values,
    };
    if result.is_template {
        resolved_template_kind(&result)?;
    }

    stack.pop();
    states[index] = 2;
    resolved[index] = Some(result.clone());
    Ok(result)
}

fn resolved_template_kind(section: &RawSection) -> Result<TemplateKind, ConfigError> {
    let raw = value(section, "type").unwrap_or("missing");
    TemplateKind::from_name(raw).ok_or_else(|| ConfigError::InvalidTemplateKind {
        section: section.name.clone(),
        kind: raw.to_owned(),
    })
}
