//! Contract between the configuration schema and `docs/CONFIGURATION.md`.
//!
//! The reference is the only place an administrator can discover an option, so
//! it is held to the schema itself rather than to a hand-maintained list. Serde
//! is the authoritative spelling table, and it reports every accepted name in
//! its unknown-variant error, so the expected sets below are derived from the
//! same types the parser uses instead of being restated here.

use serde::Deserialize;
use serde::de::IntoDeserializer;
use serde::de::value::{Error as ValueError, StrDeserializer};

use super::*;

const REFERENCE: &str = include_str!("../../../../docs/CONFIGURATION.md");

/// Heading that opens each scope's option tables, and the alias table for it.
struct Scope {
    label: &'static str,
    options_heading: &'static str,
    aliases_heading: &'static str,
}

const SCOPES: [Scope; 3] = [
    Scope {
        label: "general",
        options_heading: "## `[general]` options",
        aliases_heading: "### `[general]` aliases",
    },
    Scope {
        label: "device",
        options_heading: "## Device options",
        aliases_heading: "### Device aliases",
    },
    Scope {
        label: "line",
        options_heading: "## Line options",
        aliases_heading: "### Line aliases",
    },
];

/// Every spelling Serde accepts for `K`, canonical names and aliases alike.
///
/// Serde lists them in the unknown-variant error, which makes the schema
/// enumerable without restating it or parsing the source.
fn accepted_names<K: for<'de> Deserialize<'de>>() -> Vec<String> {
    let deserializer: StrDeserializer<'_, ValueError> =
        "an_option_name_no_scope_defines".into_deserializer();
    let error = K::deserialize(deserializer)
        .err()
        .expect("a name no scope defines must be rejected")
        .to_string();
    let (_, expected) = error
        .split_once("expected one of ")
        .expect("Serde reports the accepted spellings for an unknown variant");
    expected
        .split(',')
        .map(|name| name.trim().trim_matches('`').to_owned())
        .filter(|name| !name.is_empty())
        .collect()
}

/// Canonical name for each accepted spelling, in schema order.
fn spellings<K>() -> Vec<(String, String)>
where
    K: for<'de> Deserialize<'de> + Serialize,
{
    accepted_names::<K>()
        .into_iter()
        .map(|name| {
            let deserializer: StrDeserializer<'_, ValueError> = name.as_str().into_deserializer();
            let option =
                K::deserialize(deserializer).expect("a name Serde listed must deserialize");
            let canonical = serialized_key(&option).expect("options serialize as strings");
            (name, canonical)
        })
        .collect()
}

/// Text of the reference between `heading` and the next heading of its level.
fn body(heading: &str) -> &'static str {
    let level = heading
        .split(' ')
        .next()
        .expect("a heading starts with its hashes");
    let start = REFERENCE
        .find(heading)
        .unwrap_or_else(|| panic!("the reference is missing the heading {heading:?}"));
    let rest = &REFERENCE[start + heading.len()..];
    let terminator = format!("\n{level} ");
    match rest.find(&terminator) {
        Some(index) => &rest[..index],
        None => rest,
    }
}

/// Option names in the first cell of every table row of a documentation body.
fn tabulated_names(body: &str) -> Vec<String> {
    body.lines()
        .filter_map(|line| {
            let line = line.trim();
            let cell = line.strip_prefix("| `")?;
            let (name, _) = cell.split_once('`')?;
            Some(name.to_owned())
        })
        .collect()
}

/// The vocabulary a `###` section opens with, before any explanatory prose.
///
/// Vocabulary sections lead with the accepted values and follow with notes, so
/// only the leading paragraph is the list under test.
fn vocabulary(heading: &str) -> Vec<String> {
    let body = body(heading);
    let paragraph = body
        .trim_start_matches('\n')
        .split("\n\n")
        .next()
        .unwrap_or_default();
    quoted_terms(paragraph)
}

/// Every ``literal`` mentioned anywhere in a documentation body.
fn quoted_terms(body: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut rest = body;
    while let Some(start) = rest.find('`') {
        rest = &rest[start + 1..];
        let Some(end) = rest.find('`') else {
            break;
        };
        terms.push(rest[..end].to_owned());
        rest = &rest[end + 1..];
    }
    terms
}

#[test]
fn the_reference_documents_every_option_of_every_scope() {
    for scope in &SCOPES {
        let documented = tabulated_names(body(scope.options_heading));
        let schema: Vec<_> = match scope.label {
            "general" => spellings::<GeneralOption>(),
            "device" => spellings::<DeviceOption>(),
            _ => spellings::<LineOption>(),
        };

        for (name, canonical) in &schema {
            if name != canonical {
                continue;
            }
            assert!(
                documented.iter().any(|entry| entry == canonical),
                "{} option `{canonical}` has no row under {:?}",
                scope.label,
                scope.options_heading
            );
        }

        for name in &documented {
            assert!(
                schema.iter().any(|(_, canonical)| canonical == name),
                "{:?} documents `{name}`, which is not a canonical {} option",
                scope.options_heading,
                scope.label
            );
        }
    }
}

#[test]
fn the_reference_lists_every_compatibility_alias() {
    for scope in &SCOPES {
        let table = body(scope.aliases_heading);
        let schema = match scope.label {
            "general" => spellings::<GeneralOption>(),
            "device" => spellings::<DeviceOption>(),
            _ => spellings::<LineOption>(),
        };

        let mut documented = Vec::new();
        for line in table.lines().filter(|line| line.trim().starts_with("| `")) {
            let mut cells = line.trim().trim_matches('|').split('|');
            let canonical = cells
                .next()
                .expect("an alias row names a canonical option")
                .trim()
                .trim_matches('`')
                .to_owned();
            for alias in cells.next().unwrap_or_default().split(',') {
                documented.push((alias.trim().trim_matches('`').to_owned(), canonical.clone()));
            }
        }

        for (alias, canonical) in schema.iter().filter(|(name, canonical)| name != canonical) {
            assert!(
                documented
                    .iter()
                    .any(|(name, target)| name == alias && target == canonical),
                "{:?} is missing the {} alias `{alias}` of `{canonical}`",
                scope.aliases_heading,
                scope.label
            );
        }

        for (alias, canonical) in &documented {
            assert!(
                schema
                    .iter()
                    .any(|(name, target)| name == alias && target == canonical),
                "{:?} claims `{alias}` is an alias of `{canonical}`, which the schema denies",
                scope.aliases_heading
            );
        }
    }
}

#[test]
fn the_reference_documents_every_soft_key_mode_and_action() {
    let modes = body("## Soft-key profiles");
    for mode in [
        "on_hook",
        "connected",
        "on_hold",
        "ring_in",
        "off_hook",
        "connected_transfer",
        "digits_following",
        "connected_conference",
        "ring_out",
        "off_hook_feature",
        "in_use_hint",
        "on_hook_stealable",
        "hold_conference",
        "empty",
    ] {
        assert!(
            quoted_terms(modes).iter().any(|term| term == mode),
            "the soft-key section does not document the `{mode}` mode"
        );
        assert!(
            soft_key_mode_is_accepted(mode),
            "`{mode}` is documented but the schema rejects it"
        );
    }

    let documented = vocabulary("### Soft-key actions");
    assert!(
        !documented.is_empty(),
        "the soft-key action vocabulary is empty"
    );
    for action in &documented {
        assert!(
            parse_soft_key(action).is_some(),
            "the reference lists the soft key `{action}`, which the parser rejects"
        );
    }
}

/// A soft-key mode is accepted when the profile schema deserializes it.
fn soft_key_mode_is_accepted(mode: &str) -> bool {
    let source = RawSection {
        name: "documented".into(),
        line: 1,
        is_template: false,
        parents: Vec::new(),
        values: vec![
            RawValue {
                key: "type".into(),
                value: "softkey_profile".into(),
                line: 1,
                section: "documented".into(),
            },
            RawValue {
                key: mode.into(),
                value: "end_call".into(),
                line: 2,
                section: "documented".into(),
            },
        ],
    };
    deserialize_section::<SoftKeyProfileSection>(&source).is_ok()
}

#[test]
fn the_reference_documents_every_feature_button_and_addon() {
    for feature in vocabulary("### Feature names") {
        assert!(
            parse_feature(&feature).is_ok(),
            "the reference lists the feature `{feature}`, which the parser rejects"
        );
    }
    for addon in vocabulary("### Addon types") {
        assert!(
            parse_addon_type(&addon).is_ok(),
            "the reference lists the addon `{addon}`, which the parser rejects"
        );
    }
}

/// Options the distributed sample cannot demonstrate as a live setting.
///
/// Both names are recognized only so an upgrade reports a migration error
/// instead of an unknown option, so a sample containing either would not load.
const REJECTED_OPTIONS: [&str; 2] = ["trust_phone_ip", "obsolete_dtmf_mode"];

const SAMPLE: &str = include_str!("../../../sccp.conf.example");

/// Every option name the sample sets or offers as a commented alternative.
fn sample_option_names() -> Vec<String> {
    SAMPLE
        .lines()
        .filter_map(|line| {
            let line = line.trim().trim_start_matches(';').trim();
            let (key, _) = line.split_once('=')?;
            let key = key.trim();
            key.chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '_')
                .then(|| key.to_ascii_lowercase())
        })
        .collect()
}

#[test]
fn the_distributed_sample_shows_every_option_it_claims_to() {
    let present = sample_option_names();
    for (scope, schema) in [
        ("general", spellings::<GeneralOption>()),
        ("device", spellings::<DeviceOption>()),
        ("line", spellings::<LineOption>()),
    ] {
        for (name, canonical) in &schema {
            if name != canonical || REJECTED_OPTIONS.contains(&canonical.as_str()) {
                continue;
            }
            assert!(
                present.iter().any(|entry| entry == canonical),
                "sccp.conf.example never shows the {scope} option `{canonical}`, \
                 as a setting or as a commented alternative"
            );
        }
    }
}

/// Anchor a Markdown heading resolves to, following GitHub's slug rules.
///
/// Punctuation is dropped, spaces become hyphens, and a slug already taken by
/// an earlier heading gains a numeric suffix. That suffix is why a duplicated
/// heading silently steals links written against the plain slug.
fn heading_slug(heading: &str) -> String {
    heading
        .replace('`', "")
        .chars()
        .filter(|character| character.is_alphanumeric() || matches!(character, ' ' | '-' | '_'))
        .collect::<String>()
        .trim()
        .replace(' ', "-")
        .to_lowercase()
}

/// Every heading of the reference outside fenced blocks, as (anchor, heading).
fn headings() -> Vec<(String, String)> {
    let mut anchors: Vec<(String, String)> = Vec::new();
    let mut fenced = false;
    for line in REFERENCE.lines() {
        if line.starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced || !line.starts_with('#') {
            continue;
        }
        let heading = line.trim_start_matches('#').trim();
        let base = heading_slug(heading);
        let taken = anchors
            .iter()
            .filter(|(_, existing)| heading_slug(existing) == base)
            .count();
        let anchor = if taken == 0 {
            base
        } else {
            format!("{base}-{taken}")
        };
        anchors.push((anchor, heading.to_owned()));
    }
    anchors
}

#[test]
fn no_two_reference_headings_claim_the_same_anchor() {
    let mut bases: Vec<String> = Vec::new();
    for (_, heading) in headings() {
        let base = heading_slug(&heading);
        assert!(
            !bases.contains(&base),
            "two headings both slug to `{base}`, so links written against it \
             reach whichever comes first; rename one"
        );
        bases.push(base);
    }
}

#[test]
fn every_reference_link_reaches_the_section_it_names() {
    let anchors = headings();
    let mut fenced = false;
    let mut checked = 0;
    for (number, line) in REFERENCE.lines().enumerate() {
        if line.starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        let mut rest = line;
        while let Some(open) = rest.find("](#") {
            let text_start = rest[..open].rfind('[').map(|index| index + 1);
            let Some(target_end) = rest[open + 3..].find(')') else {
                break;
            };
            let target = &rest[open + 3..open + 3 + target_end];
            if let Some(text_start) = text_start {
                let text = &rest[text_start..open];
                let (_, heading) = anchors
                    .iter()
                    .find(|(anchor, _)| anchor == target)
                    .unwrap_or_else(|| {
                        panic!("line {}: [{text}](#{target}) names no heading", number + 1)
                    });
                assert_eq!(
                    heading_slug(text),
                    heading_slug(heading),
                    "line {}: [{text}](#{target}) reaches {heading:?}, which the \
                     link text does not name",
                    number + 1
                );
                checked += 1;
            }
            rest = &rest[open + 3 + target_end..];
        }
    }
    assert!(
        checked >= 10,
        "the reference should cross-link its sections; checked {checked}"
    );
}
