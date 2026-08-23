use std::fs;
use std::path::{Path, PathBuf};

const FORBIDDEN_PRODUCTION_PATTERNS: &[(&str, &str)] = &[
    ("format!(\"<", "formatted XML construction"),
    ("format!(r#\"<", "formatted raw XML construction"),
    ("push_str(\"<", "incremental XML construction"),
    ("write!(\"<", "formatted XML writer construction"),
    (".find(\"<", "manual XML tag search"),
    (".split(\"<", "manual XML tokenization"),
    (
        ".contains(\"<CiscoIPPhone",
        "manual known-document detection",
    ),
    (".replace(\"&\"", "manual XML escaping"),
];

const FORBIDDEN_COMPACT_PATTERNS: &[(&str, &str)] = &[
    ("format!(\"<", "formatted XML construction"),
    ("format!(r#\"<", "formatted raw XML construction"),
    ("push_str(\"<", "incremental XML construction"),
    (",\"<CiscoIPPhone", "formatted XML writer construction"),
    (".find(\"<", "manual XML tag search"),
    (".split(\"<", "manual XML tokenization"),
    (
        ".contains(\"<CiscoIPPhone",
        "manual known-document detection",
    ),
    (".replace(\"&\"", "manual XML escaping"),
];

fn rust_sources(root: &Path, output: &mut Vec<PathBuf>) {
    let mut entries = fs::read_dir(root)
        .unwrap_or_else(|error| panic!("unable to read {}: {error}", root.display()))
        .collect::<Result<Vec<_>, _>>()
        .unwrap_or_else(|error| panic!("unable to enumerate {}: {error}", root.display()));
    entries.sort_by_key(|entry| entry.path());
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            rust_sources(&path, output);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            output.push(path);
        }
    }
}

#[test]
fn production_code_uses_the_typed_xml_boundary() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest
        .parent()
        .expect("sccp-protocol crate must have a workspace parent");
    let mut sources = Vec::new();
    for root in [manifest.join("src"), workspace.join("asterisk-module/src")] {
        rust_sources(&root, &mut sources);
    }

    let mut violations = Vec::new();
    for path in sources {
        let source = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("unable to read {}: {error}", path.display()));
        let production = source
            .split_once("#[cfg(test)]")
            .map_or(source.as_str(), |(production, _)| production);
        for (line_index, line) in production.lines().enumerate() {
            for (pattern, reason) in FORBIDDEN_PRODUCTION_PATTERNS {
                if line.contains(pattern) {
                    violations.push(format!(
                        "{}:{}: {reason}: {line}",
                        path.display(),
                        line_index + 1,
                    ));
                }
            }
        }
        let compact = production
            .chars()
            .filter(|character| !character.is_ascii_whitespace())
            .collect::<String>();
        for (pattern, reason) in FORBIDDEN_COMPACT_PATTERNS {
            if compact.contains(pattern) {
                violations.push(format!("{}: {reason}", path.display()));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "known phone XML must use typed Serde models and the shared XML codec:\n{}",
        violations.join("\n"),
    );
}
