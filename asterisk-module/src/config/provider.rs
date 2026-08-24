//! Validated configuration sources for the channel module.
//!
//! Providers return the same normalized [`ModuleConfig`] regardless of where
//! their source data lives. A refresh only builds and validates a candidate;
//! applying that candidate to a running module is deliberately a caller
//! responsibility.
//!
//! [`FileConfigurationProvider`] reads the configured file on every load or
//! refresh. [`RealtimeConfigurationProvider`] builds a normalized snapshot from
//! ordered Asterisk realtime rows. [`HybridConfigurationProvider`] parses the
//! file first, overlays realtime device rows and then line rows, and performs
//! one final normalization/validation pass. Provider failures never mutate or
//! discard the caller's current snapshot.
//!
//! Realtime overlay rows require a `name` field. Repeated fields retain backend
//! order, database `NULL` removes a file override, an explicit empty string
//! clears an inherited value, and `_delete = yes` removes the named section and
//! cannot be mixed with ordinary fields. Neither SQL schemas nor backend table
//! policy are supplied here; configured table names are Asterisk realtime
//! families owned by the deployment.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use thiserror::Error;

use crate::config::realtime::{
    RealtimeConfigurationSource, RealtimeError, RealtimeField, RealtimePredicate, RealtimeRow,
};
use crate::config::{
    ConfigError, ConfigOverlayKind, ConfigOverlaySection, ConfigOverlayValue, ModuleConfig,
    RealtimeTableConfig,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfigurationOrigin {
    File(PathBuf),
    Named(String),
}

impl fmt::Display for ConfigurationOrigin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File(path) => path.display().fmt(formatter),
            Self::Named(name) => formatter.write_str(name),
        }
    }
}

#[derive(Debug, Error)]
pub enum ConfigurationProviderError {
    #[error("unable to read configuration {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("invalid configuration {origin}: {source}")]
    Invalid {
        origin: ConfigurationOrigin,
        #[source]
        source: Box<ConfigError>,
    },
    #[error("configuration provider {provider} is unavailable: {message}")]
    Unavailable { provider: String, message: String },
    #[error("realtime query for family {family} failed: {source}")]
    Realtime {
        family: String,
        #[source]
        source: Box<RealtimeError>,
    },
    #[error("invalid realtime row {row} from family {family}: {source}")]
    RealtimeRow {
        family: String,
        row: usize,
        #[source]
        source: RealtimeRowError,
    },
    #[error("realtime family {family} does not belong to the candidate revision")]
    RealtimeRevision {
        family: String,
        expected: Option<String>,
        actual: Option<String>,
    },
}

impl ConfigurationProviderError {
    pub fn invalid(origin: ConfigurationOrigin, source: ConfigError) -> Self {
        Self::Invalid {
            origin,
            source: Box::new(source),
        }
    }

    pub fn unavailable(provider: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Unavailable {
            provider: provider.into(),
            message: message.into(),
        }
    }

    pub fn config_error(&self) -> Option<&ConfigError> {
        match self {
            Self::Invalid { source, .. } => Some(source.as_ref()),
            Self::Read { .. }
            | Self::Unavailable { .. }
            | Self::Realtime { .. }
            | Self::RealtimeRow { .. }
            | Self::RealtimeRevision { .. } => None,
        }
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum RealtimeRowError {
    #[error("section-name field {field:?} is missing")]
    MissingSectionName { field: String },
    #[error("section-name field {field:?} is database NULL")]
    NullSectionName { field: String },
    #[error("section-name field {field:?} is empty")]
    EmptySectionName { field: String },
    #[error("section-name field {field:?} occurs more than once")]
    DuplicateSectionName { field: String },
    #[error("reserved field _delete occurs more than once")]
    DuplicateDelete,
    #[error("reserved field _delete is database NULL")]
    NullDelete,
    #[error(
        "reserved field _delete has value {value:?}; expected yes/no, true/false, on/off, or 1/0"
    )]
    InvalidDelete { value: String },
    #[error("section deletion conflicts with ordinary field {field:?}")]
    DeleteWithField { field: String },
    #[error("configuration field name is empty")]
    EmptyFieldName,
    #[error("row type {actual:?} conflicts with the {expected} query")]
    ConflictingSectionType {
        expected: &'static str,
        actual: String,
    },
}

/// Produces complete, normalized configuration snapshots.
///
/// `load` obtains the provider's initial candidate. `refresh` obtains a new
/// candidate from the same source and defaults to a fresh `load`. Neither
/// method mutates a running module, retains the prior snapshot, or combines
/// providers. On failure the caller's current configuration remains usable.
pub trait ConfigurationProvider: Send + Sync {
    fn load(&self) -> Result<ModuleConfig, ConfigurationProviderError>;

    fn refresh(&self) -> Result<ModuleConfig, ConfigurationProviderError> {
        self.load()
    }
}

/// Ordered static configuration input. Production uses Asterisk's native
/// parser; standalone tools and tests use the filesystem implementation.
pub trait StaticConfigurationSource: Send + Sync {
    fn origin(&self) -> ConfigurationOrigin;
    fn read_source(&self) -> Result<String, ConfigurationProviderError>;

    fn realtime_tables(&self) -> Result<Option<RealtimeTableConfig>, ConfigurationProviderError> {
        let contents = self.read_source()?;
        ModuleConfig::realtime_tables_from_source(&contents)
            .map_err(|source| ConfigurationProviderError::invalid(self.origin(), source))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileConfigurationProvider {
    path: PathBuf,
}

impl FileConfigurationProvider {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn read(&self) -> Result<String, ConfigurationProviderError> {
        read_with_includes(&self.path, &mut Vec::new())
    }

    pub fn source(&self) -> Result<String, ConfigurationProviderError> {
        self.read()
    }

    /// Reads only the general provider-selection surface. Full file or hybrid
    /// normalization remains the selected provider's responsibility.
    pub fn realtime_tables(
        &self,
    ) -> Result<Option<RealtimeTableConfig>, ConfigurationProviderError> {
        StaticConfigurationSource::realtime_tables(self)
    }
}

fn read_with_includes(
    path: &Path,
    stack: &mut Vec<PathBuf>,
) -> Result<String, ConfigurationProviderError> {
    if stack.len() >= 32 {
        return Err(ConfigurationProviderError::unavailable(
            "file",
            format!("include nesting exceeds 32 files at {}", path.display()),
        ));
    }
    let identity = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if let Some(position) = stack.iter().position(|candidate| candidate == &identity) {
        let mut cycle: Vec<_> = stack[position..]
            .iter()
            .map(|entry| entry.display().to_string())
            .collect();
        cycle.push(identity.display().to_string());
        return Err(ConfigurationProviderError::unavailable(
            "file",
            format!("configuration include cycle: {}", cycle.join(" -> ")),
        ));
    }
    let source = fs::read_to_string(path).map_err(|source| ConfigurationProviderError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    stack.push(identity);
    let mut expanded = String::new();
    for line in source.lines() {
        let trimmed = line.trim();
        let directive = if let Some(argument) = trimmed.strip_prefix("#include") {
            Some((false, argument))
        } else {
            trimmed
                .strip_prefix("#tryinclude")
                .map(|argument| (true, argument))
        };
        let Some((optional, argument)) = directive else {
            expanded.push_str(line);
            expanded.push('\n');
            continue;
        };
        let argument = argument.trim();
        let include = argument
            .strip_prefix('"')
            .and_then(|argument| argument.strip_suffix('"'))
            .or_else(|| {
                argument
                    .strip_prefix('<')
                    .and_then(|argument| argument.strip_suffix('>'))
            })
            .unwrap_or(argument);
        if include.is_empty() {
            stack.pop();
            return Err(ConfigurationProviderError::unavailable(
                "file",
                format!("{} contains an empty include directive", path.display()),
            ));
        }
        let include_path = Path::new(include);
        let include_path = if include_path.is_absolute() {
            include_path.to_path_buf()
        } else {
            path.parent()
                .unwrap_or_else(|| Path::new("."))
                .join(include_path)
        };
        match read_with_includes(&include_path, stack) {
            Ok(source) => expanded.push_str(&source),
            Err(ConfigurationProviderError::Read { source, .. })
                if optional && source.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                stack.pop();
                return Err(error);
            }
        }
    }
    stack.pop();
    Ok(expanded)
}

impl StaticConfigurationSource for FileConfigurationProvider {
    fn origin(&self) -> ConfigurationOrigin {
        ConfigurationOrigin::File(self.path.clone())
    }

    fn read_source(&self) -> Result<String, ConfigurationProviderError> {
        self.read()
    }
}

impl ConfigurationProvider for FileConfigurationProvider {
    fn load(&self) -> Result<ModuleConfig, ConfigurationProviderError> {
        let contents = self.read()?;
        ModuleConfig::parse(&contents).map_err(|source| ConfigurationProviderError::Invalid {
            origin: ConfigurationOrigin::File(self.path.clone()),
            source: Box::new(source),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RealtimeSectionKind {
    Device,
    Line,
}

impl RealtimeSectionKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Device => "device",
            Self::Line => "line",
        }
    }

    fn overlay_kind(self) -> ConfigOverlayKind {
        match self {
            Self::Device => ConfigOverlayKind::Device,
            Self::Line => ConfigOverlayKind::Line,
        }
    }
}

/// One ordered query whose rows become configuration sections.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RealtimeConfigurationQuery {
    family: String,
    predicates: Vec<RealtimePredicate>,
    section_name_field: String,
    section_kind: Option<RealtimeSectionKind>,
}

impl RealtimeConfigurationQuery {
    pub fn new(
        family: impl Into<String>,
        predicates: Vec<RealtimePredicate>,
        section_name_field: impl Into<String>,
    ) -> Self {
        Self {
            family: family.into(),
            predicates,
            section_name_field: section_name_field.into(),
            section_kind: None,
        }
    }

    pub fn devices(family: impl Into<String>) -> Self {
        Self::all_named(family, RealtimeSectionKind::Device)
    }

    pub fn lines(family: impl Into<String>) -> Self {
        Self::all_named(family, RealtimeSectionKind::Line)
    }

    fn all_named(family: impl Into<String>, kind: RealtimeSectionKind) -> Self {
        Self {
            family: family.into(),
            predicates: vec![RealtimePredicate::new("_revision LIKE", "%")],
            section_name_field: "name".into(),
            section_kind: Some(kind),
        }
    }

    pub fn with_section_kind(mut self, kind: RealtimeSectionKind) -> Self {
        self.section_kind = Some(kind);
        self
    }

    pub fn family(&self) -> &str {
        &self.family
    }

    pub fn predicates(&self) -> &[RealtimePredicate] {
        &self.predicates
    }

    pub fn section_name_field(&self) -> &str {
        &self.section_name_field
    }

    pub fn section_kind(&self) -> Option<RealtimeSectionKind> {
        self.section_kind
    }
}

#[derive(Debug)]
enum CandidateRevision {
    Unknown,
    Unversioned,
    Versioned(String),
}

impl CandidateRevision {
    fn include(
        &mut self,
        family: &str,
        actual: Option<String>,
    ) -> Result<(), ConfigurationProviderError> {
        match (&*self, actual) {
            (Self::Unknown, None) => *self = Self::Unversioned,
            (Self::Unknown, Some(revision)) => *self = Self::Versioned(revision),
            (Self::Unversioned, None) => {}
            (Self::Versioned(expected), Some(actual)) if *expected == actual => {}
            (Self::Unversioned, Some(actual)) => {
                return Err(ConfigurationProviderError::RealtimeRevision {
                    family: family.to_owned(),
                    expected: None,
                    actual: Some(actual),
                });
            }
            (Self::Versioned(expected), actual) => {
                return Err(ConfigurationProviderError::RealtimeRevision {
                    family: family.to_owned(),
                    expected: Some(expected.clone()),
                    actual,
                });
            }
        }
        Ok(())
    }
}

pub struct RealtimeConfigurationProvider {
    source: Arc<dyn RealtimeConfigurationSource>,
    queries: Vec<RealtimeConfigurationQuery>,
}

impl RealtimeConfigurationProvider {
    pub fn new(
        source: Arc<dyn RealtimeConfigurationSource>,
        queries: Vec<RealtimeConfigurationQuery>,
    ) -> Self {
        Self { source, queries }
    }

    pub fn queries(&self) -> &[RealtimeConfigurationQuery] {
        &self.queries
    }

    fn overlays(&self) -> Result<Vec<ConfigOverlaySection>, ConfigurationProviderError> {
        load_realtime_overlays(self.source.as_ref(), &self.queries)
    }
}

impl ConfigurationProvider for RealtimeConfigurationProvider {
    fn load(&self) -> Result<ModuleConfig, ConfigurationProviderError> {
        let overlays = self.overlays()?;
        ModuleConfig::parse_with_overlays("", &overlays).map_err(|source| {
            ConfigurationProviderError::invalid(
                ConfigurationOrigin::Named("realtime configuration".into()),
                source,
            )
        })
    }
}

pub struct HybridConfigurationProvider<F = FileConfigurationProvider> {
    file: F,
    source: Arc<dyn RealtimeConfigurationSource>,
    queries: Vec<RealtimeConfigurationQuery>,
    expected_tables: Option<RealtimeTableConfig>,
}

impl<F> HybridConfigurationProvider<F>
where
    F: StaticConfigurationSource,
{
    pub fn new(
        file: F,
        source: Arc<dyn RealtimeConfigurationSource>,
        queries: Vec<RealtimeConfigurationQuery>,
    ) -> Self {
        Self {
            file,
            source,
            queries,
            expected_tables: None,
        }
    }

    pub fn from_tables(
        file: F,
        source: Arc<dyn RealtimeConfigurationSource>,
        tables: &RealtimeTableConfig,
    ) -> Self {
        let mut provider = Self::new(
            file,
            source,
            vec![
                RealtimeConfigurationQuery::devices(&tables.device_family),
                RealtimeConfigurationQuery::lines(&tables.line_family),
            ],
        );
        provider.expected_tables = Some(tables.clone());
        provider
    }

    pub fn file(&self) -> &F {
        &self.file
    }

    pub fn queries(&self) -> &[RealtimeConfigurationQuery] {
        &self.queries
    }
}

impl<F> ConfigurationProvider for HybridConfigurationProvider<F>
where
    F: StaticConfigurationSource,
{
    fn load(&self) -> Result<ModuleConfig, ConfigurationProviderError> {
        // Every source is read into local values before normalization. No
        // partial row set or invalid candidate is retained by the provider.
        let contents = self.file.read_source()?;
        if let Some(expected) = &self.expected_tables {
            let selected =
                ModuleConfig::realtime_tables_from_source(&contents).map_err(|source| {
                    ConfigurationProviderError::invalid(self.file.origin(), source)
                })?;
            if selected.as_ref() != Some(expected) {
                return Err(ConfigurationProviderError::unavailable(
                    "hybrid",
                    "devicetable or linetable changed; restart the module to select new families",
                ));
            }
        }
        let overlays = load_realtime_overlays(self.source.as_ref(), &self.queries)?;
        ModuleConfig::parse_with_overlays(&contents, &overlays).map_err(|source| {
            ConfigurationProviderError::invalid(
                ConfigurationOrigin::Named(format!("{} plus realtime", self.file.origin())),
                source,
            )
        })
    }
}

fn load_realtime_overlays(
    source: &dyn RealtimeConfigurationSource,
    queries: &[RealtimeConfigurationQuery],
) -> Result<Vec<ConfigOverlaySection>, ConfigurationProviderError> {
    if queries.is_empty() {
        return Err(ConfigurationProviderError::unavailable(
            "realtime",
            "at least one ordered query is required",
        ));
    }
    let mut overlays = Vec::new();
    let mut revision = CandidateRevision::Unknown;
    for query in queries {
        if query.section_name_field.trim().is_empty()
            || query.section_name_field.eq_ignore_ascii_case("_delete")
        {
            return Err(ConfigurationProviderError::unavailable(
                "realtime",
                "section-name field must be non-empty and cannot be _delete",
            ));
        }
        let load = source
            .load_many(&query.family, &query.predicates)
            .map_err(|source| ConfigurationProviderError::Realtime {
                family: query.family.clone(),
                source: Box::new(source),
            })?;
        revision.include(&query.family, load.revision)?;
        for (index, row) in load.rows.into_iter().enumerate() {
            overlays.push(decode_realtime_row(query, index + 1, row)?);
        }
    }
    Ok(overlays)
}

fn decode_realtime_row(
    query: &RealtimeConfigurationQuery,
    row_number: usize,
    row: RealtimeRow,
) -> Result<ConfigOverlaySection, ConfigurationProviderError> {
    let fail = |source| ConfigurationProviderError::RealtimeRow {
        family: query.family.clone(),
        row: row_number,
        source,
    };
    let mut section_name = None;
    let mut delete = None;
    let mut ordinary = Vec::new();
    let mut delete_conflict = None;
    for RealtimeField { name, value } in row.fields {
        let field = name.trim();
        if field.is_empty() {
            return Err(fail(RealtimeRowError::EmptyFieldName));
        }
        if field.eq_ignore_ascii_case(&query.section_name_field) {
            if section_name.is_some() {
                return Err(fail(RealtimeRowError::DuplicateSectionName {
                    field: query.section_name_field.clone(),
                }));
            }
            let value = value.ok_or_else(|| {
                fail(RealtimeRowError::NullSectionName {
                    field: query.section_name_field.clone(),
                })
            })?;
            if value.trim().is_empty() {
                return Err(fail(RealtimeRowError::EmptySectionName {
                    field: query.section_name_field.clone(),
                }));
            }
            section_name = Some(value.trim().to_owned());
            continue;
        }
        if field.eq_ignore_ascii_case("_delete") {
            if delete.is_some() {
                return Err(fail(RealtimeRowError::DuplicateDelete));
            }
            let value = value.ok_or_else(|| fail(RealtimeRowError::NullDelete))?;
            delete = Some(parse_delete(&value).ok_or_else(|| {
                fail(RealtimeRowError::InvalidDelete {
                    value: value.clone(),
                })
            })?);
            continue;
        }
        if field.eq_ignore_ascii_case("type")
            && let (Some(expected), Some(actual)) = (query.section_kind, value.as_deref())
        {
            delete_conflict.get_or_insert_with(|| field.to_owned());
            if !actual.trim().eq_ignore_ascii_case(expected.as_str()) {
                return Err(fail(RealtimeRowError::ConflictingSectionType {
                    expected: expected.as_str(),
                    actual: actual.into(),
                }));
            }
            continue;
        }
        delete_conflict.get_or_insert_with(|| field.to_owned());
        ordinary.push(ConfigOverlayValue {
            key: field.to_owned(),
            value,
        });
    }
    let section_name = section_name.ok_or_else(|| {
        fail(RealtimeRowError::MissingSectionName {
            field: query.section_name_field.clone(),
        })
    })?;
    let delete = delete.unwrap_or(false);
    if delete && let Some(field) = delete_conflict {
        return Err(fail(RealtimeRowError::DeleteWithField { field }));
    }
    Ok(ConfigOverlaySection {
        name: section_name,
        source: format!("realtime {} row {row_number}", query.family),
        line: row_number,
        kind: query.section_kind.map(RealtimeSectionKind::overlay_kind),
        delete,
        values: ordinary,
    })
}

fn parse_delete(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    const CONFIG: &str = r#"
        [general]
        bind = 0.0.0.0:2000
        advertised_address = 192.0.2.10

        [SEP001122334455]
        type = device
        line = 1001

        [1001]
        type = line
        label = Reception
        context = from-sccp
    "#;

    static NEXT_TEMP_FILE: AtomicUsize = AtomicUsize::new(1);

    struct TempConfig {
        path: PathBuf,
    }

    impl TempConfig {
        fn new(contents: &str) -> Self {
            let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "sccp-config-provider-{}-{sequence}.conf",
                std::process::id()
            ));
            fs::write(&path, contents).unwrap();
            Self { path }
        }

        fn write(&self, contents: &str) {
            fs::write(&self.path, contents).unwrap();
        }
    }

    impl Drop for TempConfig {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
        }
    }

    struct FakeProvider {
        contents: Mutex<String>,
        loads: AtomicUsize,
    }

    enum FakeRealtimeResponse {
        Rows(Vec<RealtimeRow>),
        Revision(Option<&'static str>, Vec<RealtimeRow>),
        Failure,
    }

    #[derive(Default)]
    struct FakeRealtime {
        responses: Mutex<HashMap<String, VecDeque<FakeRealtimeResponse>>>,
        calls: Mutex<Vec<(String, Vec<RealtimePredicate>)>>,
    }

    impl FakeRealtime {
        fn push(&self, family: &str, response: FakeRealtimeResponse) {
            self.responses
                .lock()
                .unwrap()
                .entry(family.into())
                .or_default()
                .push_back(response);
        }
    }

    impl RealtimeConfigurationSource for FakeRealtime {
        fn load_many(
            &self,
            family: &str,
            predicates: &[RealtimePredicate],
        ) -> Result<crate::config::realtime::RealtimeLoad, RealtimeError> {
            self.calls
                .lock()
                .unwrap()
                .push((family.into(), predicates.to_vec()));
            match self
                .responses
                .lock()
                .unwrap()
                .get_mut(family)
                .and_then(VecDeque::pop_front)
            {
                Some(FakeRealtimeResponse::Rows(rows)) => {
                    Ok(crate::config::realtime::RealtimeLoad {
                        revision: Some("1".to_owned()),
                        rows,
                    })
                }
                Some(FakeRealtimeResponse::Revision(revision, rows)) => {
                    Ok(crate::config::realtime::RealtimeLoad {
                        revision: revision.map(str::to_owned),
                        rows,
                    })
                }
                Some(FakeRealtimeResponse::Failure) | None => Err(RealtimeError::BackendFailure),
            }
        }
    }

    fn realtime_row(fields: &[(&str, Option<&str>)]) -> RealtimeRow {
        RealtimeRow {
            fields: fields
                .iter()
                .map(|(name, value)| RealtimeField {
                    name: (*name).into(),
                    value: value.map(str::to_owned),
                })
                .collect(),
        }
    }

    fn hybrid_provider(
        file: &TempConfig,
        source: Arc<FakeRealtime>,
    ) -> HybridConfigurationProvider {
        HybridConfigurationProvider::new(
            FileConfigurationProvider::new(&file.path),
            source,
            vec![
                RealtimeConfigurationQuery::devices("devices"),
                RealtimeConfigurationQuery::lines("lines"),
            ],
        )
    }

    impl FakeProvider {
        fn new(contents: &str) -> Self {
            Self {
                contents: Mutex::new(contents.into()),
                loads: AtomicUsize::new(0),
            }
        }

        fn replace(&self, contents: &str) {
            *self.contents.lock().unwrap() = contents.into();
        }
    }

    impl ConfigurationProvider for FakeProvider {
        fn load(&self) -> Result<ModuleConfig, ConfigurationProviderError> {
            self.loads.fetch_add(1, Ordering::Relaxed);
            ModuleConfig::parse(&self.contents.lock().unwrap()).map_err(|source| {
                ConfigurationProviderError::invalid(
                    ConfigurationOrigin::Named("fake".into()),
                    source,
                )
            })
        }
    }

    #[test]
    fn file_provider_matches_direct_normalization_exactly() {
        let file = TempConfig::new(CONFIG);
        let provider = FileConfigurationProvider::new(&file.path);

        assert_eq!(provider.path(), file.path);
        assert_eq!(
            provider.load().unwrap(),
            ModuleConfig::parse(CONFIG).unwrap()
        );
    }

    #[test]
    fn file_provider_parses_the_distributed_sample() {
        let file = TempConfig::new(include_str!("../../sccp.conf.example"));
        let config = FileConfigurationProvider::new(&file.path).load().unwrap();

        assert_eq!(config.devices.len(), 1);
        assert_eq!(config.lines.len(), 2);
    }

    #[test]
    fn file_provider_preserves_typed_parse_errors_and_locations() {
        let file = TempConfig::new(&CONFIG.replace(
            "advertised_address = 192.0.2.10",
            "advertised_address = 192.0.2.10\n        audio_dscp = 64",
        ));
        let provider = FileConfigurationProvider::new(&file.path);
        let error = provider.load().unwrap_err();

        assert!(matches!(
            &error,
            ConfigurationProviderError::Invalid {
                origin: ConfigurationOrigin::File(path),
                source,
            } if path == &file.path
                && matches!(source.as_ref(), ConfigError::InvalidValue { key, value }
                    if key.contains("line 5 [general].audio_dscp")
                        && value.contains("expected DSCP 0..63"))
        ));
        assert!(error.config_error().is_some());
    }

    #[test]
    fn file_provider_reports_read_failures_with_the_exact_path() {
        let path = std::env::temp_dir().join(format!(
            "missing-sccp-config-{}-{}.conf",
            std::process::id(),
            NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        let error = FileConfigurationProvider::new(&path).load().unwrap_err();

        assert!(matches!(
            error,
            ConfigurationProviderError::Read { path: actual, source }
                if actual == path && source.kind() == io::ErrorKind::NotFound
        ));
    }

    #[test]
    fn refresh_reloads_a_complete_candidate_without_mutating_the_prior_snapshot() {
        let file = TempConfig::new(CONFIG);
        let provider = FileConfigurationProvider::new(&file.path);
        let original = provider.load().unwrap();
        file.write(&CONFIG.replace("label = Reception", "label = Updated"));

        let refreshed = provider.refresh().unwrap();

        assert_eq!(original.lines["1001"].label, "Reception");
        assert_eq!(refreshed.lines["1001"].label, "Updated");
    }

    #[test]
    fn fake_provider_obeys_load_refresh_and_failure_semantics() {
        let provider = FakeProvider::new(CONFIG);
        let first = provider.load().unwrap();
        provider.replace(&CONFIG.replace("label = Reception", "label = Fake updated"));
        let refreshed = provider.refresh().unwrap();
        provider.replace("[general]\naudio_cos = 9");
        let error = provider.refresh().unwrap_err();

        assert_eq!(provider.loads.load(Ordering::Relaxed), 3);
        assert_eq!(first.lines["1001"].label, "Reception");
        assert_eq!(refreshed.lines["1001"].label, "Fake updated");
        assert!(matches!(
            error,
            ConfigurationProviderError::Invalid {
                origin: ConfigurationOrigin::Named(name),
                source,
            } if name == "fake" && matches!(source.as_ref(), ConfigError::InvalidValue { .. })
        ));
    }

    #[test]
    fn pure_realtime_provider_builds_the_same_normalized_model() {
        let source = Arc::new(FakeRealtime::default());
        source.push(
            "globals",
            FakeRealtimeResponse::Rows(vec![realtime_row(&[
                ("category", Some("general")),
                ("server_name", Some("Realtime PBX")),
            ])]),
        );
        source.push(
            "devices",
            FakeRealtimeResponse::Rows(vec![realtime_row(&[
                ("name", Some("SEP001122334455")),
                ("description", Some("Realtime desk")),
                ("line", Some("1001")),
            ])]),
        );
        source.push(
            "lines",
            FakeRealtimeResponse::Rows(vec![realtime_row(&[
                ("name", Some("1001")),
                ("label", Some("Realtime line")),
                ("context", Some("from-realtime")),
            ])]),
        );
        let provider = RealtimeConfigurationProvider::new(
            Arc::<FakeRealtime>::clone(&source),
            vec![
                RealtimeConfigurationQuery::new(
                    "globals",
                    vec![RealtimePredicate::new("category LIKE", "%")],
                    "category",
                ),
                RealtimeConfigurationQuery::devices("devices"),
                RealtimeConfigurationQuery::lines("lines"),
            ],
        );

        let config = provider.load().unwrap();

        assert_eq!(config.lines["1001"].label, "Realtime line");
        assert_eq!(config.lines["1001"].context, "from-realtime");
        assert_eq!(config.devices.len(), 1);
        assert_eq!(config.general.server_name, "Realtime PBX");
        let calls = source.calls.lock().unwrap();
        assert_eq!(
            calls.iter().map(|call| call.0.as_str()).collect::<Vec<_>>(),
            ["globals", "devices", "lines"]
        );
        assert_eq!(calls[0].1, [RealtimePredicate::new("category LIKE", "%")]);
        assert_eq!(calls[1].1, [RealtimePredicate::new("_revision LIKE", "%")]);
    }

    #[test]
    fn realtime_candidate_rejects_mixed_family_revisions() {
        let file = TempConfig::new(CONFIG);
        let source = Arc::new(FakeRealtime::default());
        source.push(
            "devices",
            FakeRealtimeResponse::Revision(Some("41"), Vec::new()),
        );
        source.push(
            "lines",
            FakeRealtimeResponse::Revision(Some("42"), Vec::new()),
        );

        let error = hybrid_provider(&file, source).load().unwrap_err();

        assert!(matches!(
            error,
            ConfigurationProviderError::RealtimeRevision {
                family,
                expected: Some(expected),
                actual: Some(actual),
            } if family == "lines" && expected == "41" && actual == "42"
        ));
    }

    #[test]
    fn hybrid_selection_does_not_require_static_device_or_line_sections() {
        let file = TempConfig::new(
            "[general]\nadvertised_address=192.0.2.10\ndevicetable=devices\nlinetable=lines\n",
        );
        let tables = FileConfigurationProvider::new(&file.path)
            .realtime_tables()
            .unwrap()
            .unwrap();
        let source = Arc::new(FakeRealtime::default());
        source.push(
            "devices",
            FakeRealtimeResponse::Rows(vec![realtime_row(&[
                ("name", Some("SEP001122334455")),
                ("line", Some("1001")),
            ])]),
        );
        source.push(
            "lines",
            FakeRealtimeResponse::Rows(vec![realtime_row(&[
                ("name", Some("1001")),
                ("label", Some("Database only")),
            ])]),
        );
        let provider = HybridConfigurationProvider::from_tables(
            FileConfigurationProvider::new(&file.path),
            source,
            &tables,
        );

        let config = provider.load().unwrap();

        assert_eq!(config.lines["1001"].label, "Database only");
    }

    #[test]
    fn hybrid_rows_override_in_query_order_and_preserve_repeatable_field_order() {
        let file = TempConfig::new(&CONFIG.replace(
            "description = Reception",
            "description = Reception\n        sccp_tos = 0x20\n        button = speed_dial, Old, 2000",
        ));
        let source = Arc::new(FakeRealtime::default());
        source.push(
            "devices",
            FakeRealtimeResponse::Rows(vec![
                realtime_row(&[
                    ("name", Some("SEP001122334455")),
                    ("description", Some("First row")),
                    ("signaling_dscp", Some("AF31")),
                    ("button", Some("speed_dial, First, 2100")),
                    ("button", Some("speed_dial, Second, 2200")),
                ]),
                realtime_row(&[
                    ("name", Some("SEP001122334455")),
                    ("description", Some("Later row")),
                ]),
            ]),
        );
        source.push("lines", FakeRealtimeResponse::Rows(Vec::new()));

        let config = hybrid_provider(&file, source).load().unwrap();
        let device = &config.devices[&sccp_protocol::DeviceId::new("SEP001122334455").unwrap()];

        assert_eq!(device.description, "Later row");
        assert_eq!(device.network.qos.signaling.dscp.0, 26);
        assert!(matches!(
            &device.buttons[1],
            sccp_protocol::ButtonDefinition::SpeedDial(speed)
                if speed.display_name == "First" && speed.number == "2100"
        ));
        assert!(matches!(
            &device.buttons[2],
            sccp_protocol::ButtonDefinition::SpeedDial(speed)
                if speed.display_name == "Second" && speed.number == "2200"
        ));
        assert_eq!(device.buttons.len(), 3);
    }

    #[test]
    fn database_null_reveals_inheritance_while_empty_explicitly_clears_it() {
        let input = CONFIG.replace(
            "[1001]",
            "[line-defaults](!)\n        type = line\n        mailbox = inherited@default\n\n        [1001](line-defaults)",
        );
        let file = TempConfig::new(&input);
        let source = Arc::new(FakeRealtime::default());
        for mailbox in [None, Some("")] {
            source.push("devices", FakeRealtimeResponse::Rows(Vec::new()));
            source.push(
                "lines",
                FakeRealtimeResponse::Rows(vec![realtime_row(&[
                    ("name", Some("1001")),
                    ("mailbox", mailbox),
                ])]),
            );
        }
        let provider = hybrid_provider(&file, source);

        let null_candidate = provider.load().unwrap();
        let empty_candidate = provider.refresh().unwrap();

        assert_eq!(
            null_candidate.lines["1001"].mailbox.as_deref(),
            Some("inherited@default")
        );
        assert_eq!(empty_candidate.lines["1001"].mailbox, None);
    }

    #[test]
    fn alias_equivalent_line_value_replaces_the_file_spelling() {
        let file = TempConfig::new(&CONFIG.replace(
            "label = Reception",
            "label = Reception\n        vmnum = 1000",
        ));
        let source = Arc::new(FakeRealtime::default());
        source.push("devices", FakeRealtimeResponse::Rows(Vec::new()));
        source.push(
            "lines",
            FakeRealtimeResponse::Rows(vec![realtime_row(&[
                ("name", Some("1001")),
                ("voicemail_number", Some("2000")),
            ])]),
        );

        let config = hybrid_provider(&file, source).load().unwrap();

        assert_eq!(
            config
                .features_for_line("1001")
                .unwrap()
                .voicemail
                .number
                .as_ref()
                .map(|value| value.as_str()),
            Some("2000")
        );
    }

    #[test]
    fn delete_row_removes_the_named_file_section() {
        let file = TempConfig::new(&format!(
            "{CONFIG}\n[SEP112233445566]\ntype=device\nline=1001\n"
        ));
        let source = Arc::new(FakeRealtime::default());
        source.push(
            "devices",
            FakeRealtimeResponse::Rows(vec![realtime_row(&[
                ("name", Some("SEP001122334455")),
                ("_delete", Some("yes")),
            ])]),
        );
        source.push("lines", FakeRealtimeResponse::Rows(Vec::new()));

        let config = hybrid_provider(&file, source).load().unwrap();

        assert_eq!(config.devices.len(), 1);
        assert!(
            config
                .devices
                .contains_key(&sccp_protocol::DeviceId::new("SEP112233445566").unwrap())
        );
    }

    #[test]
    fn strict_row_controls_reject_ambiguous_or_conflicting_rows() {
        let cases = [
            (
                realtime_row(&[("description", Some("missing name"))]),
                "section-name field \"name\" is missing",
            ),
            (
                realtime_row(&[("name", None)]),
                "section-name field \"name\" is database NULL",
            ),
            (
                realtime_row(&[("name", Some(""))]),
                "section-name field \"name\" is empty",
            ),
            (
                realtime_row(&[("name", Some("1001")), ("name", Some("1002"))]),
                "section-name field \"name\" occurs more than once",
            ),
            (
                realtime_row(&[("name", Some("1001")), ("_delete", None)]),
                "reserved field _delete is database NULL",
            ),
            (
                realtime_row(&[
                    ("name", Some("1001")),
                    ("_delete", Some("no")),
                    ("_delete", Some("yes")),
                ]),
                "reserved field _delete occurs more than once",
            ),
            (
                realtime_row(&[("name", Some("1001")), ("_delete", Some("maybe"))]),
                "expected yes/no",
            ),
            (
                realtime_row(&[
                    ("name", Some("1001")),
                    ("_delete", Some("yes")),
                    ("label", Some("conflict")),
                ]),
                "deletion conflicts with ordinary field \"label\"",
            ),
            (
                realtime_row(&[
                    ("name", Some("1001")),
                    ("_delete", Some("yes")),
                    ("type", Some("line")),
                ]),
                "deletion conflicts with ordinary field \"type\"",
            ),
            (
                realtime_row(&[("name", Some("1001")), ("type", Some("device"))]),
                "conflicts with the line query",
            ),
        ];
        for (row, expected) in cases {
            let source = Arc::new(FakeRealtime::default());
            source.push("lines", FakeRealtimeResponse::Rows(vec![row]));
            let provider = RealtimeConfigurationProvider::new(
                source,
                vec![RealtimeConfigurationQuery::lines("lines")],
            );

            let error = provider.load().unwrap_err().to_string();

            assert!(error.contains("family lines"), "{error}");
            assert!(error.contains("row 1"), "{error}");
            assert!(
                error.contains(expected),
                "{error} did not contain {expected}"
            );
        }
    }

    #[test]
    fn failed_partial_query_or_validation_cannot_replace_the_live_snapshot() {
        let file = TempConfig::new(CONFIG);
        let source = Arc::new(FakeRealtime::default());
        source.push("devices", FakeRealtimeResponse::Rows(Vec::new()));
        source.push("lines", FakeRealtimeResponse::Rows(Vec::new()));
        source.push(
            "devices",
            FakeRealtimeResponse::Rows(vec![realtime_row(&[
                ("name", Some("SEP001122334455")),
                ("description", Some("must not publish")),
            ])]),
        );
        source.push("lines", FakeRealtimeResponse::Failure);
        source.push(
            "devices",
            FakeRealtimeResponse::Rows(vec![realtime_row(&[
                ("name", Some("SEP001122334455")),
                ("unknown_setting", Some("invalid")),
            ])]),
        );
        source.push("lines", FakeRealtimeResponse::Rows(Vec::new()));
        let provider = hybrid_provider(&file, source);
        let live = provider.load().unwrap();
        let live_description = live.devices.values().next().unwrap().description.clone();

        let query_error = provider.refresh().unwrap_err();
        assert!(matches!(
            query_error,
            ConfigurationProviderError::Realtime { .. }
        ));
        assert_eq!(
            live.devices.values().next().unwrap().description,
            live_description
        );

        let validation_error = provider.refresh().unwrap_err();
        assert!(matches!(
            validation_error,
            ConfigurationProviderError::Invalid { source, .. }
                if matches!(source.as_ref(), ConfigError::InvalidValue { key, .. }
                    if key.contains("[realtime devices row 1].unknown_setting"))
        ));
        assert_eq!(
            live.devices.values().next().unwrap().description,
            live_description
        );
    }

    #[test]
    fn runtime_selected_table_families_are_stable_until_restart() {
        let input = CONFIG.replace(
            "advertised_address = 192.0.2.10",
            "advertised_address = 192.0.2.10\n        devicetable = devices\n        linetable = lines",
        );
        let file = TempConfig::new(&input);
        let file_provider = FileConfigurationProvider::new(&file.path);
        let tables = file_provider.realtime_tables().unwrap().unwrap();
        let source = Arc::new(FakeRealtime::default());
        source.push("devices", FakeRealtimeResponse::Rows(Vec::new()));
        source.push("lines", FakeRealtimeResponse::Rows(Vec::new()));
        let provider = HybridConfigurationProvider::from_tables(file_provider, source, &tables);
        let live = provider.load().unwrap();
        file.write(&input.replace("linetable = lines", "linetable = replacement_lines"));

        let error = provider.refresh().unwrap_err();

        assert!(matches!(
            error,
            ConfigurationProviderError::Unavailable { provider, message }
                if provider == "hybrid" && message.contains("restart")
        ));
        assert_eq!(live.lines["1001"].label, "Reception");
    }

    #[test]
    fn standalone_file_source_expands_includes_and_rejects_cycles() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("sccp.conf");
        let stations = directory.path().join("stations.conf");
        fs::write(
            &root,
            "[general]\nadvertised_address = 192.0.2.10\n#include \"stations.conf\"\n#tryinclude \"optional.conf\"\n",
        )
        .unwrap();
        fs::write(
            &stations,
            "[SEP001122334455]\ntype = device\nline = 1001\n\n[1001]\ntype = line\nlabel = Included\ncontext = internal\n",
        )
        .unwrap();

        let provider = FileConfigurationProvider::new(&root);
        let config = provider.load().unwrap();
        assert_eq!(config.lines["1001"].label, "Included");

        fs::write(&stations, "#include \"sccp.conf\"\n").unwrap();
        let error = provider.load().unwrap_err().to_string();
        assert!(error.contains("include cycle"), "{error}");
    }
}
