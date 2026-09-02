use std::ffi::{OsStr, OsString};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use chan_sccp2::config::ModuleConfig;
use chan_sccp2::config::provider::FileConfigurationProvider;

const SUCCESS: u8 = 0;
const INVALID_CONFIGURATION: u8 = 1;
const INVOCATION_ERROR: u8 = 2;
const USAGE: &str = "Usage:\n  chan-sccp2-config-checker <chan_sccp2.conf>\n  chan-sccp2-config-checker --canonical <chan_sccp2.conf>\n  chan-sccp2-config-checker normalize <chan_sccp2.conf>\n";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Command {
    Check,
    CheckCanonical,
    Normalize,
}

#[derive(Debug, Eq, PartialEq)]
struct Summary {
    devices: usize,
    lines: usize,
    buttons: usize,
    soft_key_profiles: usize,
}

fn summary(config: &ModuleConfig) -> Summary {
    Summary {
        devices: config.devices.len(),
        lines: config.lines.len(),
        buttons: config
            .devices
            .values()
            .map(|device| device.buttons.len())
            .sum(),
        soft_key_profiles: config.soft_key_profiles.len(),
    }
}

fn write_usage(mut output: impl Write) -> io::Result<()> {
    output.write_all(USAGE.as_bytes())
}

fn run(
    arguments: impl IntoIterator<Item = OsString>,
    mut stdout: impl Write,
    mut stderr: impl Write,
) -> u8 {
    let mut arguments = arguments.into_iter();
    let Some(first) = arguments.next() else {
        let _ = write!(stderr, "error: missing configuration path\n{USAGE}");
        return INVOCATION_ERROR;
    };

    if first == OsStr::new("-h") || first == OsStr::new("--help") {
        if arguments.next().is_some() {
            let _ = write!(stderr, "error: --help does not accept arguments\n{USAGE}");
            return INVOCATION_ERROR;
        }
        let _ = write_usage(&mut stdout);
        return SUCCESS;
    }

    let (command, argument) = if first == OsStr::new("--canonical") {
        let Some(path) = arguments.next() else {
            let _ = write!(
                stderr,
                "error: --canonical requires a configuration path\n{USAGE}"
            );
            return INVOCATION_ERROR;
        };
        (Command::CheckCanonical, path)
    } else if first == OsStr::new("normalize") {
        let Some(path) = arguments.next() else {
            let _ = write!(
                stderr,
                "error: normalize requires a configuration path\n{USAGE}"
            );
            return INVOCATION_ERROR;
        };
        (Command::Normalize, path)
    } else {
        if first.to_string_lossy().starts_with('-') {
            let _ = write!(stderr, "error: unknown option {first:?}\n{USAGE}");
            return INVOCATION_ERROR;
        }
        (Command::Check, first)
    };

    if arguments.next().is_some() {
        let _ = write!(
            stderr,
            "error: expected exactly one configuration path\n{USAGE}"
        );
        return INVOCATION_ERROR;
    }

    process_path(
        command,
        PathBuf::from(argument).as_path(),
        &mut stdout,
        &mut stderr,
    )
}

fn process_path(
    command: Command,
    path: &Path,
    mut stdout: impl Write,
    mut stderr: impl Write,
) -> u8 {
    let source = match FileConfigurationProvider::new(path).source() {
        Ok(source) => source,
        Err(error) => {
            let _ = writeln!(
                stderr,
                "error: cannot read path={:?}: {error}",
                path.as_os_str()
            );
            return INVOCATION_ERROR;
        }
    };

    if command == Command::Normalize {
        return match ModuleConfig::to_canonical_string(&source) {
            Ok(canonical) => {
                let _ = stdout.write_all(canonical.as_bytes());
                SUCCESS
            }
            Err(error) => {
                let _ = writeln!(
                    stderr,
                    "invalid configuration: path={:?}: {error}",
                    path.as_os_str(),
                );
                INVALID_CONFIGURATION
            }
        };
    }

    let parsed = if command == Command::CheckCanonical {
        ModuleConfig::check_canonical(&source).and_then(|()| ModuleConfig::parse(&source))
    } else {
        ModuleConfig::parse(&source)
    };
    match parsed {
        Ok(config) => {
            let summary = summary(&config);
            let _ = writeln!(
                stdout,
                "valid: path={:?} devices={} lines={} buttons={} soft_key_profiles={}",
                path.as_os_str(),
                summary.devices,
                summary.lines,
                summary.buttons,
                summary.soft_key_profiles,
            );
            SUCCESS
        }
        Err(error) => {
            let _ = writeln!(
                stderr,
                "invalid configuration: path={:?}: {error}",
                path.as_os_str(),
            );
            INVALID_CONFIGURATION
        }
    }
}

fn main() -> ExitCode {
    ExitCode::from(run(
        std::env::args_os().skip(1),
        std::io::stdout().lock(),
        std::io::stderr().lock(),
    ))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    const SAMPLE: &str = include_str!("../../../asterisk-module/sccp.conf.example");
    const DEPLOYMENT_SAMPLE: &str = include_str!("../../../docs/sccp-example-config.conf");
    const REFERENCE: &str = include_str!("../../../docs/CONFIGURATION.md");

    /// One example extracted from a fenced block in the configuration reference.
    ///
    /// The fence info string declares how the block must be validated, because a
    /// reference documents settings in isolation as well as whole files.
    struct Example {
        line: usize,
        kind: Kind,
        body: String,
    }

    enum Kind {
        /// A complete `sccp.conf` that must parse and already be canonical.
        Complete,
        /// A deliberate mistake whose rejection is the point of the example.
        Rejected,
        /// Settings belonging inside the named scope of an otherwise minimal file.
        Fragment(Scope),
    }

    enum Scope {
        General,
        Device,
        Line,
        /// Whole sections, completed with a general section and any missing
        /// device or line the parser requires.
        Sections,
        /// Preprocessor directives, which name files that do not exist here.
        Directives,
    }

    fn examples() -> Vec<Example> {
        let mut examples = Vec::new();
        let mut open: Option<(usize, &str)> = None;
        let mut foreign_fence = false;
        let mut body = String::new();
        for (number, text) in REFERENCE.lines().enumerate() {
            let fence = text.trim_end().strip_prefix("```");
            if foreign_fence {
                foreign_fence = !matches!(fence, Some(""));
                continue;
            }
            match open {
                None => {
                    match fence {
                        Some(info) if info.starts_with("ini") => {
                            open = Some((number + 1, info["ini".len()..].trim()));
                            body.clear();
                        }
                        // Another language's block must not be scanned for ini
                        // fences, and its closing fence must not close ours.
                        Some(info) if !info.is_empty() => foreign_fence = true,
                        _ => {}
                    }
                }
                Some((line, info)) if matches!(fence, Some("")) => {
                    let kind = match info {
                        "" => Kind::Complete,
                        "rejected" => Kind::Rejected,
                        "fragment=general" => Kind::Fragment(Scope::General),
                        "fragment=device" => Kind::Fragment(Scope::Device),
                        "fragment=line" => Kind::Fragment(Scope::Line),
                        "fragment=sections" => Kind::Fragment(Scope::Sections),
                        "fragment=directives" => Kind::Fragment(Scope::Directives),
                        other => panic!("line {line}: unknown ini fence info {other:?}"),
                    };
                    examples.push(Example {
                        line,
                        kind,
                        body: body.clone(),
                    });
                    open = None;
                }
                Some(_) => {
                    body.push_str(text);
                    body.push('\n');
                }
            }
        }
        assert!(open.is_none(), "unterminated ini block in the reference");
        examples
    }

    /// One example line with any trailing comment removed, as the parser reads
    /// it. Scanning the raw text instead would fold a comment into a section
    /// name or a button's line number, and a `;` inside a quoted value — which
    /// the reference does use — is not a comment.
    fn significant(text: &str) -> &str {
        let mut quoted = false;
        for (index, character) in text.char_indices() {
            match character {
                '"' => quoted = !quoted,
                ';' if !quoted => return text[..index].trim(),
                _ => {}
            }
        }
        text.trim()
    }

    /// Line names a fragment declares, so the completed file can assign them.
    ///
    /// A section is a line when it says so or when it inherits from a template
    /// that does, which is the shape the template examples use.
    fn declared_lines(body: &str) -> Vec<String> {
        struct Section {
            name: String,
            template: bool,
            parents: Vec<String>,
            line: bool,
        }

        let mut sections: Vec<Section> = Vec::new();
        for (number, text) in body.lines().enumerate() {
            let text = significant(text);
            if let Some(rest) = text.strip_prefix('[') {
                let (name, suffix) = rest.split_once(']').unwrap_or_else(|| {
                    panic!(
                        "example line {}: section header {text:?} does not close",
                        number + 1
                    )
                });
                let suffix = suffix.trim().trim_start_matches('(').trim_end_matches(')');
                let entries: Vec<_> = suffix
                    .split(',')
                    .map(str::trim)
                    .filter(|entry| !entry.is_empty())
                    .collect();
                sections.push(Section {
                    name: name.to_owned(),
                    template: entries.contains(&"!"),
                    parents: entries
                        .iter()
                        .filter(|entry| **entry != "!")
                        .map(|entry| (*entry).to_owned())
                        .collect(),
                    line: false,
                });
            } else if text
                .replace([' ', '\t'], "")
                .eq_ignore_ascii_case("type=line")
                && let Some(section) = sections.last_mut()
            {
                section.line = true;
            }
        }

        let templates: Vec<_> = sections
            .iter()
            .filter(|section| section.template && section.line)
            .map(|section| section.name.clone())
            .collect();
        sections
            .iter()
            .filter(|section| {
                !section.template
                    && (section.line
                        || section
                            .parents
                            .iter()
                            .any(|parent| templates.contains(parent)))
            })
            .map(|section| section.name.clone())
            .collect()
    }

    /// Line names a device fragment puts on a button, so they can be defined.
    fn referenced_lines(body: &str) -> Vec<String> {
        let mut names = Vec::new();
        for text in body.lines() {
            let text = significant(text);
            let Some((key, value)) = text.split_once('=') else {
                continue;
            };
            let value = value.trim();
            let name = match key.trim() {
                "line" => value.split(',').next().unwrap_or_default(),
                "button" => match value.split(',').map(str::trim).collect::<Vec<_>>()[..] {
                    ["line", name, ..] => name,
                    _ => continue,
                },
                _ => continue,
            };
            let name = name.trim();
            if !name.is_empty() && !names.iter().any(|known| known == name) {
                names.push(name.to_owned());
            }
        }
        names
    }

    /// The minimal parts a fragment has to be wrapped in to become a file the
    /// parser accepts. Naming them once keeps every scope's completion in step
    /// with the parser's requirements.
    const GENERAL: &str = "[general]\nbind = 0.0.0.0:2000\nadvertised_ipv4 = 192.0.2.10\n";
    const DEVICE: &str = "[SEPAAAABBBBCCCC]\ntype = device\n";
    const SPARE_LINE_NAME: &str = "9000";

    fn line_section(name: &str) -> String {
        format!("[{name}]\ntype = line\ncontext = from-sccp\n")
    }

    fn complete(example: &Example) -> String {
        let body = &example.body;
        let spare_button = format!("button = line, {SPARE_LINE_NAME}\n");
        let spare_line = line_section(SPARE_LINE_NAME);
        let scope = match &example.kind {
            Kind::Complete => return body.clone(),
            // A rejected example shows one mistake, so it is given the device
            // and line the parser also requires; otherwise missing scaffolding
            // would satisfy the rejection whatever the example says.
            Kind::Rejected => {
                return if declared_lines(body).is_empty() {
                    format!("{body}{DEVICE}{spare_button}{spare_line}")
                } else {
                    body.clone()
                };
            }
            Kind::Fragment(scope) => scope,
        };
        match scope {
            Scope::General => {
                format!("{GENERAL}{body}\n{DEVICE}{spare_button}{spare_line}")
            }
            Scope::Device => {
                // The fragment's own buttons name lines that must exist, and
                // the spare line is only added when the fragment did not
                // already reference it.
                let sections: String = referenced_lines(body)
                    .iter()
                    .filter(|name| *name != SPARE_LINE_NAME)
                    .map(|name| line_section(name))
                    .collect();
                format!("{GENERAL}{DEVICE}{spare_button}{body}\n{spare_line}{sections}")
            }
            Scope::Line => {
                format!("{GENERAL}{DEVICE}{spare_button}{spare_line}{body}\n")
            }
            Scope::Sections => {
                // A sections fragment supplies its own lines; only when it
                // declares none does the device need the spare one.
                let declared = declared_lines(body);
                let (assigned, spare) = if declared.is_empty() {
                    (vec![SPARE_LINE_NAME.to_owned()], spare_line.as_str())
                } else {
                    (declared, "")
                };
                let buttons: String = assigned
                    .iter()
                    .map(|name| format!("button = line, {name}\n"))
                    .collect();
                format!("{GENERAL}{body}\n{DEVICE}{buttons}{spare}")
            }
            Scope::Directives => {
                format!("{GENERAL}{DEVICE}{spare_button}{spare_line}{body}")
            }
        }
    }

    #[test]
    fn every_reference_example_matches_the_behavior_it_documents() {
        let examples = examples();
        // Every fenced ini block is an example, so a fence the scanner fails
        // to recognize shows up here rather than silently going unchecked.
        let fenced = REFERENCE
            .lines()
            .filter(|text| text.trim_end().starts_with("```ini"))
            .count();
        assert_eq!(
            examples.len(),
            fenced,
            "the reference has {fenced} ini blocks but {} were extracted",
            examples.len()
        );
        assert!(
            examples.len() >= 20,
            "the reference should keep its worked examples; found {}",
            examples.len()
        );

        for example in &examples {
            let line = example.line;
            if let Kind::Fragment(Scope::Directives) = example.kind {
                for directive in example.body.lines().filter(|text| !text.trim().is_empty()) {
                    let directive = directive.trim();
                    assert!(
                        directive.starts_with("#include") || directive.starts_with("#tryinclude"),
                        "line {line}: {directive:?} is not an include directive"
                    );
                }
                continue;
            }

            let source = complete(example);
            let outcome = ModuleConfig::parse(&source);
            match example.kind {
                Kind::Rejected => assert!(
                    outcome.is_err(),
                    "line {line}: example documents a rejection but parsed cleanly"
                ),
                _ => {
                    outcome.unwrap_or_else(|error| {
                        panic!("line {line}: example is not valid: {error}\n{source}")
                    });
                    ModuleConfig::check_canonical(&source).unwrap_or_else(|error| {
                        panic!("line {line}: example is not canonical: {error}\n{source}")
                    });
                }
            }
        }
    }

    fn invoke(arguments: &[&str]) -> (u8, String, String) {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let status = run(
            arguments.iter().map(OsString::from),
            &mut stdout,
            &mut stderr,
        );
        (
            status,
            String::from_utf8(stdout).unwrap(),
            String::from_utf8(stderr).unwrap(),
        )
    }

    #[test]
    fn distributed_configuration_is_valid_and_summarized() {
        let config = ModuleConfig::parse(SAMPLE).unwrap();
        ModuleConfig::check_canonical(SAMPLE).unwrap();
        ModuleConfig::check_canonical(DEPLOYMENT_SAMPLE).unwrap();
        let actual = summary(&config);

        assert_eq!(actual.devices, 1);
        assert_eq!(actual.lines, 2);
        assert!(actual.buttons > 0);
        assert!(actual.soft_key_profiles > 0);
    }

    #[test]
    fn canonical_check_and_normalize_have_stable_interfaces() {
        let directory = tempfile::tempdir().unwrap();
        let canonical = directory.path().join("canonical.conf");
        fs::write(&canonical, SAMPLE).unwrap();
        let (status, _, stderr) = invoke(&["--canonical", canonical.to_str().unwrap()]);
        assert_eq!(status, SUCCESS, "{stderr}");

        let mixed = directory.path().join("mixed.conf");
        fs::write(&mixed, SAMPLE.replace("server_name =", "SeRvEr_NaMe =")).unwrap();
        let (status, _, stderr) = invoke(&["--canonical", mixed.to_str().unwrap()]);
        assert_eq!(status, INVALID_CONFIGURATION);
        assert!(
            stderr.contains("canonical option name server_name"),
            "{stderr}"
        );

        let (status, stdout, stderr) = invoke(&["normalize", mixed.to_str().unwrap()]);
        assert_eq!(status, SUCCESS, "{stderr}");
        assert!(stdout.contains("server_name ="));
        assert!(!stdout.contains("SeRvEr_NaMe"));
    }

    #[test]
    fn help_and_usage_errors_have_stable_exit_codes() {
        let (status, stdout, stderr) = invoke(&["--help"]);
        assert_eq!(status, SUCCESS);
        assert_eq!(stdout, USAGE);
        assert!(stderr.is_empty());

        for arguments in [&[][..], &["one", "two"][..], &["--unknown"][..]] {
            let (status, stdout, stderr) = invoke(arguments);
            assert_eq!(status, INVOCATION_ERROR);
            assert!(stdout.is_empty());
            assert!(stderr.contains(USAGE.trim()));
        }
    }

    #[test]
    fn valid_and_invalid_files_are_reported_without_panics() {
        let directory = tempfile::tempdir().unwrap();
        let valid = directory.path().join("valid.conf");
        fs::write(&valid, SAMPLE).unwrap();
        let (status, stdout, stderr) = invoke(&[valid.to_str().unwrap()]);
        assert_eq!(status, SUCCESS);
        assert!(stdout.starts_with("valid: path="));
        assert!(stdout.contains("devices=1 lines=2"));
        assert!(stderr.is_empty());

        let invalid = directory.path().join("invalid.conf");
        fs::write(&invalid, "[general]\nport\n").unwrap();
        let (status, stdout, stderr) = invoke(&[invalid.to_str().unwrap()]);
        assert_eq!(status, INVALID_CONFIGURATION);
        assert!(stdout.is_empty());
        assert!(stderr.contains("invalid configuration:"));
        assert!(stderr.contains("line 2"));
    }

    #[test]
    fn read_errors_are_distinct_from_invalid_configuration() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing.conf");
        let (status, stdout, stderr) = invoke(&[missing.to_str().unwrap()]);

        assert_eq!(status, INVOCATION_ERROR);
        assert!(stdout.is_empty());
        assert!(stderr.contains("error: cannot read path="));
    }

    #[test]
    fn parser_diagnostics_do_not_disclose_sensitive_values() {
        let secret = "/do/not/expose/private-server-key.pem";
        let source = SAMPLE.replace(
            "advertised_ipv4 = 192.0.2.10",
            &format!("advertised_ipv4 = 192.0.2.10\ntls_private_key = {secret}"),
        );
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("secret.conf");
        fs::write(&path, source).unwrap();
        let (status, stdout, stderr) = invoke(&[path.to_str().unwrap()]);

        assert_eq!(status, INVALID_CONFIGURATION);
        assert!(stdout.is_empty());
        assert!(stderr.contains("<redacted>"), "{stderr}");
        assert!(!stderr.contains(secret), "{stderr}");
    }
}
