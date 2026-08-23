//! Bounded phone-directory request, pagination, and HTTP response policy.
//!
//! `/sccp/directory` accepts only `GET` without a body. Its optional,
//! case-sensitive fields are `q`, one-based `page`, and `pageSize` in `1..=32`.
//! The aggregate query is limited to [`DIRECTORY_MAX_QUERY_BYTES`], searches to
//! [`DIRECTORY_MAX_SEARCH_CHARS`], and the source to
//! [`DIRECTORY_MAX_SOURCE_ENTRIES`]. Unknown/repeated fields, malformed form
//! encoding, page zero and out-of-range pages fail closed.
//!
//! Results contain one logical configured line, use configured public caller
//! identity with label/number fallback, and sort deterministically by name then
//! number. Typed `CiscoIPPhoneDirectory` serialization supplies XML escaping;
//! this module never builds XML manually. The phone schema limits each page to
//! 32 entries and the complete document to 8192 bytes.

use std::borrow::Cow;

use sccp_protocol::{
    CiscoIpPhoneDirectory, CiscoIpPhoneDirectoryEntry, PHONE_DIRECTORY_MAX_BYTES,
    PHONE_DIRECTORY_MAX_ENTRIES, PhoneXmlError,
};
use thiserror::Error;

use crate::config::ModuleConfig;
use crate::http::{
    HttpBackend, HttpError, HttpField, HttpLimits, HttpMethod, HttpMethodSet, HttpRequest,
    HttpResponse,
};

pub const DIRECTORY_HTTP_PATH: &str = "/sccp/directory";
pub const DIRECTORY_MAX_QUERY_BYTES: usize = 512;
pub const DIRECTORY_MAX_SEARCH_CHARS: usize = 64;
pub const DIRECTORY_MAX_SOURCE_ENTRIES: usize = 4_096;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectoryRecord {
    pub name: String,
    pub telephone: String,
}

impl DirectoryRecord {
    pub fn new(name: impl Into<String>, telephone: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            telephone: telephone.into(),
        }
    }

    /// Fits a configured identity to the phone directory schema without
    /// splitting a UTF-8 character.
    pub fn from_configured_identity(name: &str, telephone: &str) -> Self {
        Self::new(fit_phone_text(name), fit_phone_text(telephone))
    }
}

/// Builds one stable directory record per configured logical line. Caller-ID
/// overrides are the public identity when present; the line label and number
/// are the fallback.
pub fn records_from_config(config: &ModuleConfig) -> Vec<DirectoryRecord> {
    config
        .lines
        .values()
        .map(|line| {
            let caller_name = line.caller_name.trim();
            let name = if caller_name.is_empty() || caller_name == line.number {
                line.label.trim()
            } else {
                caller_name
            };
            let telephone = if line.caller_number.trim().is_empty() {
                line.number.trim()
            } else {
                line.caller_number.trim()
            };
            DirectoryRecord::from_configured_identity(name, telephone)
        })
        .collect()
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectoryRequest {
    pub search: Option<String>,
    /// One-based page number.
    pub page: usize,
    pub page_size: usize,
}

impl Default for DirectoryRequest {
    fn default() -> Self {
        Self {
            search: None,
            page: 1,
            page_size: PHONE_DIRECTORY_MAX_ENTRIES,
        }
    }
}

impl DirectoryRequest {
    /// Parses an application/x-www-form-urlencoded query using the standard
    /// form parser. The accepted keys are `q`, `page`, and `pageSize`.
    pub fn parse_query(query: &[u8]) -> Result<Self, DirectoryRequestError> {
        if query.len() > DIRECTORY_MAX_QUERY_BYTES {
            return Err(DirectoryRequestError::QueryExceedsLimit);
        }
        validate_percent_triplets(query)?;
        let fields = form_urlencoded::parse(query)
            .map(|(name, value)| {
                Ok(HttpField {
                    name: into_strict_text(name)?,
                    value: into_strict_text(value)?,
                })
            })
            .collect::<Result<Vec<_>, DirectoryRequestError>>()?;
        Self::from_fields(&fields)
    }

    /// Validates the already standards-decoded query fields exposed by the
    /// typed Asterisk HTTP boundary.
    pub fn from_fields(fields: &[HttpField]) -> Result<Self, DirectoryRequestError> {
        let mut request = Self::default();
        let mut search_seen = false;
        let mut page_seen = false;
        let mut page_size_seen = false;
        for field in fields {
            match field.name.as_str() {
                "q" => {
                    if search_seen {
                        return Err(DirectoryRequestError::DuplicateParameter("q"));
                    }
                    search_seen = true;
                    validate_search(&field.value)?;
                    request.search = (!field.value.is_empty()).then(|| field.value.clone());
                }
                "page" => {
                    if page_seen {
                        return Err(DirectoryRequestError::DuplicateParameter("page"));
                    }
                    page_seen = true;
                    request.page = parse_positive_number("page", &field.value)?;
                }
                "pageSize" => {
                    if page_size_seen {
                        return Err(DirectoryRequestError::DuplicateParameter("pageSize"));
                    }
                    page_size_seen = true;
                    request.page_size = parse_positive_number("pageSize", &field.value)?;
                    if request.page_size > PHONE_DIRECTORY_MAX_ENTRIES {
                        return Err(DirectoryRequestError::PageSizeExceedsLimit);
                    }
                }
                _ => return Err(DirectoryRequestError::UnknownParameter),
            }
        }
        request.validate()?;
        Ok(request)
    }

    pub fn validate(&self) -> Result<(), DirectoryRequestError> {
        if self.page == 0 {
            return Err(DirectoryRequestError::InvalidNumber("page"));
        }
        if self.page_size == 0 || self.page_size > PHONE_DIRECTORY_MAX_ENTRIES {
            return Err(DirectoryRequestError::PageSizeExceedsLimit);
        }
        if let Some(search) = &self.search {
            validate_search(search)?;
        }
        Ok(())
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum DirectoryRequestError {
    #[error("directory query exceeds the 512-byte limit")]
    QueryExceedsLimit,
    #[error("directory query contains invalid percent encoding")]
    InvalidPercentEncoding,
    #[error("directory query is not valid UTF-8")]
    InvalidUtf8,
    #[error("directory query contains an unknown parameter; expected q, page, or pageSize")]
    UnknownParameter,
    #[error("directory query repeats parameter {0}")]
    DuplicateParameter(&'static str),
    #[error("directory search must contain at most 64 characters and no control characters")]
    InvalidSearch,
    #[error("directory parameter {0} must be a positive decimal integer")]
    InvalidNumber(&'static str),
    #[error("directory pageSize must be between 1 and 32")]
    PageSizeExceedsLimit,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectoryPage {
    pub document: CiscoIpPhoneDirectory,
    pub page: usize,
    pub page_count: usize,
    pub result_count: usize,
}

impl DirectoryPage {
    pub fn build(
        records: &[DirectoryRecord],
        request: &DirectoryRequest,
    ) -> Result<Self, DirectoryPageError> {
        request.validate()?;
        if records.len() > DIRECTORY_MAX_SOURCE_ENTRIES {
            return Err(DirectoryPageError::SourceExceedsLimit);
        }
        let search = request.search.as_ref().map(|value| value.to_lowercase());
        let mut matches: Vec<_> = records
            .iter()
            .filter(|entry| {
                search.as_ref().is_none_or(|search| {
                    entry.name.to_lowercase().contains(search)
                        || entry.telephone.to_lowercase().contains(search)
                })
            })
            .collect();
        matches.sort_by(|left, right| {
            left.name
                .to_lowercase()
                .cmp(&right.name.to_lowercase())
                .then_with(|| left.telephone.cmp(&right.telephone))
                .then_with(|| left.name.cmp(&right.name))
        });

        let result_count = matches.len();
        let page_count = result_count.div_ceil(request.page_size).max(1);
        if request.page > page_count {
            return Err(DirectoryPageError::PageOutOfRange { page_count });
        }
        let start = (request.page - 1)
            .checked_mul(request.page_size)
            .ok_or(DirectoryPageError::PageOutOfRange { page_count })?;
        let end = start.saturating_add(request.page_size).min(result_count);
        let entries = matches[start..end]
            .iter()
            .map(|entry| CiscoIpPhoneDirectoryEntry {
                name: Some(entry.name.clone()),
                telephone: Some(entry.telephone.clone()),
            })
            .collect();
        let prompt = if result_count == 0 {
            if request.search.is_some() {
                "No matching entries".to_owned()
            } else {
                "Directory is empty".to_owned()
            }
        } else {
            format!("{}-{} of {result_count}", start + 1, end)
        };
        let document = CiscoIpPhoneDirectory::new("Directory", prompt, entries)?;
        Ok(Self {
            document,
            page: request.page,
            page_count,
            result_count,
        })
    }
}

#[derive(Debug, Error)]
pub enum DirectoryPageError {
    #[error(transparent)]
    Request(#[from] DirectoryRequestError),
    #[error("directory source exceeds the 4096-entry limit")]
    SourceExceedsLimit,
    #[error("directory page is out of range; maximum page is {page_count}")]
    PageOutOfRange { page_count: usize },
    #[error(transparent)]
    Xml(#[from] PhoneXmlError),
}

pub trait DirectoryProvider: Send + Sync + 'static {
    fn records(&self) -> Result<Vec<DirectoryRecord>, DirectoryProviderError>;
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum DirectoryProviderError {
    #[error("directory data is temporarily unavailable")]
    Unavailable,
}

pub struct DirectoryHttpService<P> {
    provider: P,
}

pub fn register_directory_http<P, B>(provider: P, backend: B) -> Result<B::Registration, HttpError>
where
    P: DirectoryProvider,
    B: HttpBackend,
{
    let service = DirectoryHttpService::new(provider);
    backend.register(
        DIRECTORY_HTTP_PATH,
        "SCCP phone directory",
        false,
        HttpMethodSet::GET,
        HttpLimits {
            max_body_bytes: 1,
            max_response_bytes: PHONE_DIRECTORY_MAX_BYTES,
            max_path_bytes: 256,
            max_fields: 64,
            max_field_name_bytes: 64,
            max_field_value_bytes: DIRECTORY_MAX_QUERY_BYTES,
        },
        move |request| service.handle(request),
    )
}

impl<P> DirectoryHttpService<P>
where
    P: DirectoryProvider,
{
    pub const fn new(provider: P) -> Self {
        Self { provider }
    }

    pub fn handle(&self, request: HttpRequest) -> HttpResponse {
        if request.method != HttpMethod::Get || !request.body.is_empty() {
            return text_response(405, "directory requests require GET without a body");
        }
        let request = match DirectoryRequest::from_fields(&request.query) {
            Ok(request) => request,
            Err(error) => return text_response(400, error.to_string()),
        };
        let records = match self.provider.records() {
            Ok(records) => records,
            Err(error) => return text_response(503, error.to_string()),
        };
        let page = match DirectoryPage::build(&records, &request) {
            Ok(page) => page,
            Err(DirectoryPageError::PageOutOfRange { .. }) => {
                return text_response(404, "directory page is out of range");
            }
            Err(_) => return text_response(500, "directory response could not be generated"),
        };
        match page.document.to_xml() {
            Ok(xml) => HttpResponse::new(200, "text/xml; charset=utf-8", xml.into_bytes())
                .expect("static directory response metadata is valid"),
            Err(_) => text_response(500, "directory response could not be generated"),
        }
    }
}

fn text_response(status: u16, message: impl Into<String>) -> HttpResponse {
    HttpResponse::text(status, message).expect("static directory response metadata is valid")
}

fn fit_phone_text(value: &str) -> String {
    value.chars().take(32).collect()
}

fn validate_search(value: &str) -> Result<(), DirectoryRequestError> {
    if value.chars().count() > DIRECTORY_MAX_SEARCH_CHARS || value.chars().any(char::is_control) {
        Err(DirectoryRequestError::InvalidSearch)
    } else {
        Ok(())
    }
}

fn parse_positive_number(field: &'static str, value: &str) -> Result<usize, DirectoryRequestError> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(DirectoryRequestError::InvalidNumber(field));
    }
    value
        .parse::<usize>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or(DirectoryRequestError::InvalidNumber(field))
}

fn validate_percent_triplets(query: &[u8]) -> Result<(), DirectoryRequestError> {
    let mut index = 0;
    while index < query.len() {
        if query[index] == b'%' {
            if query
                .get(index + 1)
                .is_none_or(|byte| !byte.is_ascii_hexdigit())
                || query
                    .get(index + 2)
                    .is_none_or(|byte| !byte.is_ascii_hexdigit())
            {
                return Err(DirectoryRequestError::InvalidPercentEncoding);
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    Ok(())
}

fn into_strict_text(value: Cow<'_, str>) -> Result<String, DirectoryRequestError> {
    if value.contains(char::REPLACEMENT_CHARACTER) {
        Err(DirectoryRequestError::InvalidUtf8)
    } else {
        Ok(value.into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    struct FakeProvider(Result<Vec<DirectoryRecord>, DirectoryProviderError>);

    impl DirectoryProvider for FakeProvider {
        fn records(&self) -> Result<Vec<DirectoryRecord>, DirectoryProviderError> {
            self.0.clone()
        }
    }

    fn request(query: Vec<HttpField>) -> HttpRequest {
        HttpRequest {
            method: HttpMethod::Get,
            route: DIRECTORY_HTTP_PATH.into(),
            path: DIRECTORY_HTTP_PATH.into(),
            remote_address: "192.0.2.10".into(),
            query,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    fn records(count: usize) -> Vec<DirectoryRecord> {
        (1..=count)
            .rev()
            .map(|index| DirectoryRecord::new(format!("Person {index:02}"), format!("{index:04}")))
            .collect()
    }

    #[test]
    fn form_query_decodes_search_and_applies_bounded_defaults() {
        let parsed = DirectoryRequest::parse_query(b"q=R%26D+West&page=2&pageSize=10").unwrap();
        assert_eq!(
            parsed,
            DirectoryRequest {
                search: Some("R&D West".into()),
                page: 2,
                page_size: 10,
            }
        );
        assert_eq!(
            DirectoryRequest::parse_query(b"").unwrap(),
            DirectoryRequest::default()
        );
    }

    #[test]
    fn request_rejects_encoding_duplicates_unknowns_controls_and_numeric_bounds() {
        for query in [
            b"q=%".as_slice(),
            b"q=%GG",
            b"q=%FF",
            b"q=one&q=two",
            b"offset=1",
            b"q=line%0Ainjection",
            b"page=0",
            b"page=-1",
            b"page=999999999999999999999999999999999999999999",
            b"pageSize=33",
        ] {
            assert!(DirectoryRequest::parse_query(query).is_err(), "{query:?}");
        }
        assert!(matches!(
            DirectoryRequest::parse_query(&vec![b'q'; DIRECTORY_MAX_QUERY_BYTES + 1]),
            Err(DirectoryRequestError::QueryExceedsLimit)
        ));
        assert!(matches!(
            DirectoryRequest::parse_query(format!("q={}", "x".repeat(65)).as_bytes()),
            Err(DirectoryRequestError::InvalidSearch)
        ));
    }

    #[test]
    fn pagination_is_one_based_sorted_bounded_and_reports_empty_results() {
        let page = DirectoryPage::build(
            &records(35),
            &DirectoryRequest {
                search: None,
                page: 2,
                page_size: 32,
            },
        )
        .unwrap();
        assert_eq!(page.page_count, 2);
        assert_eq!(page.result_count, 35);
        assert_eq!(page.document.prompt.as_deref(), Some("33-35 of 35"));
        assert_eq!(page.document.entries.len(), 3);
        assert_eq!(page.document.entries[0].name.as_deref(), Some("Person 33"));

        let empty = DirectoryPage::build(
            &records(3),
            &DirectoryRequest {
                search: Some("nobody".into()),
                page: 1,
                page_size: 32,
            },
        )
        .unwrap();
        assert_eq!(empty.page_count, 1);
        assert_eq!(empty.result_count, 0);
        assert!(empty.document.entries.is_empty());
        assert_eq!(
            empty.document.prompt.as_deref(),
            Some("No matching entries")
        );
    }

    #[test]
    fn pagination_rejects_out_of_range_pages_source_size_and_invalid_schema_entries() {
        assert!(matches!(
            DirectoryPage::build(
                &records(1),
                &DirectoryRequest {
                    search: None,
                    page: 1,
                    page_size: 0,
                },
            ),
            Err(DirectoryPageError::Request(
                DirectoryRequestError::PageSizeExceedsLimit
            ))
        ));
        assert!(matches!(
            DirectoryPage::build(
                &records(33),
                &DirectoryRequest {
                    search: None,
                    page: 3,
                    page_size: 32,
                },
            ),
            Err(DirectoryPageError::PageOutOfRange { page_count: 2 })
        ));
        assert!(matches!(
            DirectoryPage::build(
                &records(DIRECTORY_MAX_SOURCE_ENTRIES + 1),
                &DirectoryRequest::default(),
            ),
            Err(DirectoryPageError::SourceExceedsLimit)
        ));
        assert!(matches!(
            DirectoryPage::build(
                &[DirectoryRecord::new("x".repeat(33), "1000")],
                &DirectoryRequest::default(),
            ),
            Err(DirectoryPageError::Xml(PhoneXmlError::InvalidField { .. }))
        ));
    }

    #[test]
    fn http_service_serves_typed_escaped_pages_and_maps_failures_without_data_leaks() {
        let service = DirectoryHttpService::new(FakeProvider(Ok(vec![
            DirectoryRecord::new("R&D <West>", "10&01"),
            DirectoryRecord::new("Alice", "1002"),
        ])));
        let response = service.handle(request(vec![HttpField {
            name: "q".into(),
            value: "R&D".into(),
        }]));
        assert_eq!(response.status(), 200);
        assert_eq!(response.content_type(), "text/xml; charset=utf-8");
        let xml = std::str::from_utf8(response.body()).unwrap();
        assert!(xml.contains("R&amp;D &lt;West&gt;"));
        assert!(xml.contains("10&amp;01"));
        let parsed = CiscoIpPhoneDirectory::from_xml(response.body()).unwrap();
        assert_eq!(parsed.entries.len(), 1);

        let response = service.handle(request(vec![HttpField {
            name: "page".into(),
            value: "2".into(),
        }]));
        assert_eq!(response.status(), 404);

        let unavailable =
            DirectoryHttpService::new(FakeProvider(Err(DirectoryProviderError::Unavailable)));
        let response = unavailable.handle(request(Vec::new()));
        assert_eq!(response.status(), 503);
        assert!(
            !std::str::from_utf8(response.body())
                .unwrap()
                .contains("R&D")
        );
    }

    #[test]
    fn configured_identity_truncation_preserves_utf8_and_xml_byte_bound() {
        let record = DirectoryRecord::from_configured_identity(&"é".repeat(40), &"9".repeat(40));
        assert_eq!(record.name.chars().count(), 32);
        assert_eq!(record.telephone.chars().count(), 32);
        let page = DirectoryPage::build(&[record], &DirectoryRequest::default()).unwrap();
        let xml = page.document.to_xml().unwrap();
        assert!(xml.len() <= PHONE_DIRECTORY_MAX_BYTES);
    }

    #[test]
    fn configured_lines_become_unique_logical_directory_records_with_public_identity() {
        let config = ModuleConfig::parse(
            r#"
[general]
bind = 127.0.0.1:2000

[1001]
type = line
label = Front desk
callerid = "Reception" <91001>

[1002]
type = line
label = Warehouse

[SEP001122334455]
type = device
button = line, 1001
button = line, 1002

[SEP001122334466]
type = device
button = line, 1001
"#,
        )
        .unwrap();
        let mut records = records_from_config(&config);
        records.sort_by(|left, right| left.telephone.cmp(&right.telephone));
        assert_eq!(
            records,
            [
                DirectoryRecord::new("Warehouse", "1002"),
                DirectoryRecord::new("Reception", "91001"),
            ]
        );
    }
}
