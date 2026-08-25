#![allow(dead_code)]

use std::fmt;
use std::fs;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use proc_macro2::{Span, TokenStream, TokenTree};
use syn::spanned::Spanned;
use syn::visit::Visit;

pub fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

pub fn workspace_root() -> PathBuf {
    crate_root()
        .parent()
        .expect("asterisk-module must be in the workspace")
        .to_path_buf()
}

pub struct SourceContract {
    text: String,
    tokens: Option<Vec<String>>,
    string_literals: Option<Vec<String>>,
}

impl SourceContract {
    fn loaded(path: &Path, label: &str) -> Self {
        let text = fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("unable to read {label}: {error}"));
        let (tokens, string_literals) =
            if path.extension().is_some_and(|extension| extension == "rs") {
                syn::parse_file(&text).unwrap_or_else(|error| {
                    panic!("architecture source {label} must parse: {error}")
                });
                (Some(rust_tokens(&text)), Some(rust_string_literals(&text)))
            } else {
                (None, None)
            };
        Self {
            text,
            tokens,
            string_literals,
        }
    }

    fn rust_fragment(text: &str) -> Self {
        Self {
            text: text.to_owned(),
            tokens: Some(rust_tokens(text)),
            string_literals: syn::parse_file(text)
                .ok()
                .map(|_| rust_string_literals(text)),
        }
    }

    fn text_fragment(text: &str) -> Self {
        Self {
            text: text.to_owned(),
            tokens: None,
            string_literals: None,
        }
    }

    /// Match one formatting-independent token sequence.
    ///
    /// Complete Rust files are parsed with `syn` when loaded. Contract
    /// fragments are then compared as lexical token sequences, so whitespace,
    /// comments, wrapping, and rustfmt changes cannot satisfy or break a
    /// structural assertion. Non-Rust build assets use whitespace-normalized
    /// text because their external tools remain their authoritative parsers.
    pub fn contains(&self, pattern: impl AsRef<str>) -> bool {
        self.find(pattern).is_some()
    }

    pub fn find(&self, pattern: impl AsRef<str>) -> Option<usize> {
        let pattern = pattern.as_ref();
        if let Some(tokens) = &self.tokens {
            let needle = rust_tokens(pattern);
            if needle.is_empty() {
                return Some(0);
            }
            tokens
                .windows(needle.len())
                .position(|candidate| candidate == needle)
        } else {
            normalize_text(&self.text).find(&normalize_text(pattern))
        }
    }

    /// Match source-level string-literal content explicitly.
    ///
    /// Keeping this separate prevents an identifier mentioned in a diagnostic
    /// or comment from satisfying a structural Rust contract.
    pub fn contains_literal(&self, pattern: impl AsRef<str>) -> bool {
        let pattern = pattern.as_ref();
        self.string_literals.as_ref().map_or_else(
            || normalize_text(&self.text).contains(&normalize_text(pattern)),
            |literals| literals.iter().any(|literal| literal.contains(pattern)),
        )
    }

    pub fn matches(&self, pattern: impl AsRef<str>) -> impl Iterator<Item = ()> {
        let pattern = pattern.as_ref();
        let count = if let Some(tokens) = &self.tokens {
            let needle = rust_tokens(pattern);
            if needle.is_empty() {
                0
            } else {
                tokens
                    .windows(needle.len())
                    .filter(|candidate| *candidate == needle)
                    .count()
            }
        } else {
            normalize_text(&self.text)
                .match_indices(&normalize_text(pattern))
                .count()
        };
        std::iter::repeat_n((), count)
    }

    pub fn contains_between(
        &self,
        start: impl AsRef<str>,
        end: impl AsRef<str>,
        pattern: impl AsRef<str>,
    ) -> bool {
        let Some(tokens) = &self.tokens else {
            let normalized = normalize_text(&self.text);
            let Some(start) = normalized.find(&normalize_text(start.as_ref())) else {
                return false;
            };
            let Some(end) = normalized[start..].find(&normalize_text(end.as_ref())) else {
                return false;
            };
            return normalized[start..start + end].contains(&normalize_text(pattern.as_ref()));
        };
        let start_pattern = rust_tokens(start.as_ref());
        let end_pattern = rust_tokens(end.as_ref());
        let needle = rust_tokens(pattern.as_ref());
        let Some(start) = token_position(tokens, &start_pattern, 0) else {
            return false;
        };
        let body_start = start + start_pattern.len();
        let Some(end) = token_position(tokens, &end_pattern, body_start) else {
            return false;
        };
        token_position(&tokens[body_start..end], &needle, 0).is_some()
    }

    pub fn contains_in_order(&self, patterns: &[&str]) -> bool {
        let Some(tokens) = &self.tokens else {
            let normalized = normalize_text(&self.text);
            let mut offset = 0;
            for pattern in patterns {
                let needle = normalize_text(pattern);
                let Some(relative) = normalized[offset..].find(&needle) else {
                    return false;
                };
                offset += relative + needle.len();
            }
            return true;
        };
        let mut offset = 0;
        for pattern in patterns {
            let needle = rust_tokens(pattern);
            let Some(position) = token_position(tokens, &needle, offset) else {
                return false;
            };
            offset = position + needle.len();
        }
        true
    }
}

impl Deref for SourceContract {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.text
    }
}

impl AsRef<str> for SourceContract {
    fn as_ref(&self) -> &str {
        &self.text
    }
}

impl fmt::Display for SourceContract {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.text)
    }
}

pub fn source(relative: &str) -> SourceContract {
    SourceContract::loaded(&crate_root().join(relative), relative)
}

pub fn path_source(path: &Path) -> SourceContract {
    SourceContract::loaded(path, &path.display().to_string())
}

pub fn workspace_source(relative: &str) -> SourceContract {
    SourceContract::loaded(
        &workspace_root().join(relative),
        &format!("workspace {relative}"),
    )
}

pub fn rust_sources(directory: &Path, output: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(directory).expect("source directory must be readable") {
        let path = entry.expect("source entry must be readable").path();
        if path.is_dir() {
            rust_sources(&path, output);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            output.push(path);
        }
    }
}

pub fn rust_token_count(source: &SourceContract) -> usize {
    source
        .tokens
        .as_ref()
        .expect("token counts require parsed Rust source")
        .len()
}

pub fn rust_modules(source: &str) -> Vec<String> {
    syn::parse_file(source)
        .expect("module contract source must parse")
        .items
        .into_iter()
        .filter_map(|item| match item {
            syn::Item::Mod(module) => Some(module.ident.to_string()),
            _ => None,
        })
        .collect()
}

pub fn rust_repr_c_types(source: &str) -> Vec<String> {
    let syntax = syn::parse_file(source).expect("representation contract source must parse");
    let mut collector = ReprCTypeCollector::default();
    collector.visit_file(&syntax);
    collector.names
}

fn has_repr_c(attributes: &[syn::Attribute]) -> bool {
    attributes.iter().any(|attribute| {
        let syn::Meta::List(meta) = &attribute.meta else {
            return false;
        };
        let tokens = rust_tokens(&meta.tokens.to_string());
        meta.path.is_ident("repr") && tokens.iter().any(|token| token == "C")
    })
}

#[derive(Default)]
struct ReprCTypeCollector {
    names: Vec<String>,
}

impl<'ast> Visit<'ast> for ReprCTypeCollector {
    fn visit_item_struct(&mut self, item: &'ast syn::ItemStruct) {
        if has_repr_c(&item.attrs) {
            self.names.push(item.ident.to_string());
        }
        syn::visit::visit_item_struct(self, item);
    }

    fn visit_item_enum(&mut self, item: &'ast syn::ItemEnum) {
        if has_repr_c(&item.attrs) {
            self.names.push(item.ident.to_string());
        }
        syn::visit::visit_item_enum(self, item);
    }

    fn visit_item_union(&mut self, item: &'ast syn::ItemUnion) {
        if has_repr_c(&item.attrs) {
            self.names.push(item.ident.to_string());
        }
        syn::visit::visit_item_union(self, item);
    }
}

pub fn rust_attribute_count(source: &str, attribute: &str) -> usize {
    let syntax = syn::parse_file(source).expect("attribute contract source must parse");
    let mut visitor = AttributeCounter {
        source,
        attribute: rust_tokens(attribute),
        count: 0,
    };
    visitor.visit_file(&syntax);
    visitor.count
}

pub fn rust_extern_c_functions(source: &str) -> Vec<String> {
    let syntax = syn::parse_file(source).expect("callback contract source must parse");
    let mut visitor = ExternCFunctionCollector::default();
    visitor.visit_file(&syntax);
    visitor.names
}

struct AttributeCounter<'a> {
    source: &'a str,
    attribute: Vec<String>,
    count: usize,
}

impl<'ast> Visit<'ast> for AttributeCounter<'_> {
    fn visit_attribute(&mut self, attribute: &'ast syn::Attribute) {
        let span = attribute.span();
        let text = &self.source
            [span_offset(self.source, span.start())..span_offset(self.source, span.end())];
        let tokens = rust_tokens(text);
        if token_position(&tokens, &self.attribute, 0).is_some() {
            self.count += 1;
        }
        syn::visit::visit_attribute(self, attribute);
    }
}

#[derive(Default)]
struct ExternCFunctionCollector {
    names: Vec<String>,
    callback_macros: std::collections::BTreeSet<String>,
}

impl<'ast> Visit<'ast> for ExternCFunctionCollector {
    fn visit_item_fn(&mut self, function: &'ast syn::ItemFn) {
        if function.sig.unsafety.is_some()
            && function
                .sig
                .abi
                .as_ref()
                .and_then(|abi| abi.name.as_ref())
                .is_some_and(|name| name.value() == "C")
        {
            self.names.push(function.sig.ident.to_string());
        }
        syn::visit::visit_item_fn(self, function);
    }

    fn visit_item_macro(&mut self, item: &'ast syn::ItemMacro) {
        let tokens = rust_tokens(&item.mac.tokens.to_string());
        let prefix = rust_tokens("unsafe extern \"C\" fn");
        if token_position(&tokens, &prefix, 0).is_some()
            && let Some(name) = &item.ident
        {
            self.callback_macros.insert(name.to_string());
        } else if item
            .mac
            .path
            .segments
            .last()
            .is_some_and(|segment| self.callback_macros.contains(&segment.ident.to_string()))
            && let Some(TokenTree::Ident(name)) = item.mac.tokens.clone().into_iter().next()
        {
            self.names.push(name.to_string());
        }
        syn::visit::visit_item_macro(self, item);
    }
}

/// Returns the parsed Rust item selected by a stable kind/name anchor.
///
/// `syn` supplies the source span, so formatting, intervening items, and braces
/// in literals cannot change the selected structural boundary.
pub fn rust_item(source: &str, anchor: &str) -> SourceContract {
    let syntax = syn::parse_file(source)
        .unwrap_or_else(|error| panic!("architecture source must parse as Rust: {error}"));
    let span = find_item_span(&syntax, anchor);
    SourceContract::rust_fragment(
        &source[span_offset(source, span.start())..span_offset(source, span.end())],
    )
}

pub fn rust_region(source: &str, start_anchor: &str, end_anchor: &str) -> SourceContract {
    let syntax = syn::parse_file(source)
        .unwrap_or_else(|error| panic!("architecture source must parse as Rust: {error}"));
    let start = find_item_span(&syntax, start_anchor).start();
    let end = find_item_span(&syntax, end_anchor).start();
    let start = span_offset(source, start);
    let end = span_offset(source, end);
    assert!(start < end, "Rust contract region anchors are reversed");
    SourceContract::rust_fragment(&source[start..end])
}

fn find_item_span(syntax: &syn::File, anchor: &str) -> Span {
    let target = RustItemTarget::from_anchor(anchor);
    let mut finder = RustItemFinder {
        target: &target,
        found: None,
    };
    finder.visit_file(syntax);
    finder
        .found
        .unwrap_or_else(|| panic!("Rust item anchor {anchor:?} must exist"))
}

enum RustItemTarget {
    Function(String),
    Impl {
        trait_name: Option<String>,
        type_name: String,
    },
    Struct(String),
}

impl RustItemTarget {
    fn from_anchor(anchor: &str) -> Self {
        if let Some(after) = anchor.split_once("fn ").map(|(_, after)| after) {
            return Self::Function(identifier_prefix(after));
        }
        if let Some(after) = anchor.strip_prefix("impl ") {
            let (trait_name, type_name) = after.split_once(" for ").map_or_else(
                || (None, identifier_prefix(after)),
                |(trait_name, type_name)| {
                    (
                        Some(identifier_prefix(trait_name)),
                        identifier_prefix(type_name),
                    )
                },
            );
            return Self::Impl {
                trait_name,
                type_name,
            };
        }
        if let Some(after) = anchor.split_once("struct ").map(|(_, after)| after) {
            return Self::Struct(identifier_prefix(after));
        }
        panic!("unsupported Rust item anchor {anchor:?}")
    }
}

struct RustItemFinder<'a> {
    target: &'a RustItemTarget,
    found: Option<Span>,
}

impl<'ast> Visit<'ast> for RustItemFinder<'_> {
    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        self.record_function(&item.sig.ident.to_string(), item.span());
        syn::visit::visit_item_fn(self, item);
    }

    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        self.record_function(&item.sig.ident.to_string(), item.span());
        syn::visit::visit_impl_item_fn(self, item);
    }

    fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
        if self.found.is_none()
            && let RustItemTarget::Impl {
                trait_name,
                type_name,
            } = self.target
            && type_path_name(&item.self_ty).is_some_and(|name| name == *type_name)
            && item.trait_.as_ref().and_then(|(_, path, _)| {
                path.segments
                    .last()
                    .map(|segment| segment.ident.to_string())
            }) == *trait_name
        {
            self.found = Some(item.span());
        }
        syn::visit::visit_item_impl(self, item);
    }

    fn visit_item_struct(&mut self, item: &'ast syn::ItemStruct) {
        if self.found.is_none()
            && matches!(self.target, RustItemTarget::Struct(name) if item.ident == name.as_str())
        {
            self.found = Some(item.span());
        }
        syn::visit::visit_item_struct(self, item);
    }
}

impl RustItemFinder<'_> {
    fn record_function(&mut self, name: &str, span: Span) {
        if self.found.is_none()
            && matches!(self.target, RustItemTarget::Function(target) if target == name)
        {
            self.found = Some(span);
        }
    }
}

fn rust_string_literals(source: &str) -> Vec<String> {
    let stream = TokenStream::from_str(source).expect("validated Rust source must tokenize");
    let mut values = Vec::new();
    collect_string_literals(stream, &mut values);
    values
}

fn collect_string_literals(stream: TokenStream, values: &mut Vec<String>) {
    for token in stream {
        match token {
            TokenTree::Group(group) => collect_string_literals(group.stream(), values),
            TokenTree::Literal(literal) => {
                if let Ok(syn::Lit::Str(literal)) = syn::parse_str(&literal.to_string()) {
                    values.push(literal.value());
                }
            }
            TokenTree::Ident(_) | TokenTree::Punct(_) => {}
        }
    }
}

fn type_path_name(ty: &syn::Type) -> Option<String> {
    let syn::Type::Path(path) = ty else {
        return None;
    };
    path.path
        .segments
        .last()
        .map(|segment| segment.ident.to_string())
}

fn identifier_prefix(text: &str) -> String {
    text.chars()
        .take_while(|character| character.is_ascii_alphanumeric() || *character == '_')
        .collect()
}

fn span_offset(source: &str, location: proc_macro2::LineColumn) -> usize {
    source
        .split_inclusive('\n')
        .take(location.line.saturating_sub(1))
        .map(str::len)
        .sum::<usize>()
        + location.column
}

/// Selects a match arm by tokens in its parsed pattern.
pub fn rust_match_arm(source: &str, anchor: &str) -> SourceContract {
    let syntax = syn::parse_file(source)
        .unwrap_or_else(|error| panic!("architecture source must parse as Rust: {error}"));
    let mut finder = RustMatchArmFinder {
        source,
        anchor: rust_tokens(anchor),
        found: None,
    };
    finder.visit_file(&syntax);
    let span = finder
        .found
        .unwrap_or_else(|| panic!("Rust match-arm pattern {anchor:?} must exist"));
    SourceContract::rust_fragment(
        &source[span_offset(source, span.start())..span_offset(source, span.end())],
    )
}

struct RustMatchArmFinder<'a> {
    source: &'a str,
    anchor: Vec<String>,
    found: Option<Span>,
}

impl<'ast> Visit<'ast> for RustMatchArmFinder<'_> {
    fn visit_arm(&mut self, arm: &'ast syn::Arm) {
        if self.found.is_none() {
            let span = arm.pat.span();
            let pattern = &self.source
                [span_offset(self.source, span.start())..span_offset(self.source, span.end())];
            let tokens = rust_tokens(pattern);
            if token_position(&tokens, &self.anchor, 0).is_some() {
                self.found = Some(arm.span());
                return;
            }
        }
        syn::visit::visit_arm(self, arm);
    }
}

pub fn docker_stage(source: &str, stage: &str) -> SourceContract {
    let mut offset = 0;
    let mut start = None;
    let mut end = None;
    for line in source.split_inclusive('\n') {
        let words = line.split_whitespace().collect::<Vec<_>>();
        let is_from = words
            .first()
            .is_some_and(|word| word.eq_ignore_ascii_case("FROM"));
        if start.is_some() && is_from {
            end = Some(offset);
            break;
        }
        if is_from
            && words.windows(2).any(|pair| {
                pair[0].eq_ignore_ascii_case("AS") && pair[1].eq_ignore_ascii_case(stage)
            })
        {
            start = Some(offset);
        }
        offset += line.len();
    }
    let start = start.unwrap_or_else(|| panic!("Docker stage {stage:?} must exist"));
    SourceContract::text_fragment(&source[start..end.unwrap_or(source.len())])
}

fn normalize_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn token_position(tokens: &[String], needle: &[String], offset: usize) -> Option<usize> {
    (!needle.is_empty() && offset <= tokens.len())
        .then(|| {
            tokens[offset..]
                .windows(needle.len())
                .position(|candidate| candidate == needle)
                .map(|relative| offset + relative)
        })
        .flatten()
}

/// A small lexer for already-`syn`-validated Rust sources and deliberately
/// incomplete Rust fragments used as contract needles. Keeping punctuation as
/// individual tokens also permits anchors such as an unmatched call `(`.
fn rust_tokens(source: &str) -> Vec<String> {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index].is_ascii_whitespace() {
            index += 1;
            continue;
        }
        if bytes[index..].starts_with(b"//") {
            index += 2;
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
            continue;
        }
        if bytes[index..].starts_with(b"/*") {
            index += 2;
            let mut depth = 1usize;
            while index < bytes.len() && depth != 0 {
                if bytes[index..].starts_with(b"/*") {
                    depth += 1;
                    index += 2;
                } else if bytes[index..].starts_with(b"*/") {
                    depth -= 1;
                    index += 2;
                } else {
                    index += 1;
                }
            }
            continue;
        }

        let start = index;
        if is_identifier_start(bytes[index]) {
            index += 1;
            while index < bytes.len() && is_identifier_continue(bytes[index]) {
                index += 1;
            }
            if matches!(bytes.get(index), Some(b'\"' | b'\''))
                && matches!(&bytes[start..index], b"b" | b"c")
            {
                index = quoted_literal_end(bytes, index, bytes[index]);
            } else if bytes[start] == b'r' && matches!(bytes.get(index), Some(b'#' | b'\"')) {
                index = raw_literal_end(bytes, start);
            }
        } else if bytes[index].is_ascii_digit() {
            index += 1;
            while index < bytes.len()
                && (bytes[index].is_ascii_alphanumeric() || matches!(bytes[index], b'_' | b'.'))
            {
                index += 1;
            }
        } else if bytes[index] == b'\''
            && bytes
                .get(index + 1)
                .is_some_and(|byte| is_identifier_start(*byte))
        {
            index += 2;
            while index < bytes.len() && is_identifier_continue(bytes[index]) {
                index += 1;
            }
            if bytes.get(index) == Some(&b'\'') {
                index += 1;
            }
        } else if matches!(bytes[index], b'\"' | b'\'') {
            index = quoted_literal_end(bytes, index, bytes[index]);
        } else {
            index += 1;
        }
        tokens.push(source[start..index].to_owned());
    }
    let mut canonical = Vec::with_capacity(tokens.len());
    for token in tokens {
        if matches!(token.as_str(), ")" | "]" | "}")
            && canonical.last().is_some_and(|previous| previous == ",")
        {
            canonical.pop();
        }
        canonical.push(token);
    }
    canonical
}

fn is_identifier_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

fn is_identifier_continue(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn quoted_literal_end(bytes: &[u8], mut index: usize, quote: u8) -> usize {
    index += 1;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            index = (index + 2).min(bytes.len());
        } else if bytes[index] == quote {
            return index + 1;
        } else {
            index += 1;
        }
    }
    index
}

fn raw_literal_end(bytes: &[u8], start: usize) -> usize {
    let mut quote = start + 1;
    while quote < bytes.len() && bytes[quote] == b'#' {
        quote += 1;
    }
    if bytes.get(quote) != Some(&b'\"') {
        return quote;
    }
    let hashes = quote - start - 1;
    let mut index = quote + 1;
    while index < bytes.len() {
        if bytes[index] == b'\"'
            && bytes
                .get(index + 1..index + 1 + hashes)
                .is_some_and(|suffix| suffix.iter().all(|byte| *byte == b'#'))
        {
            return index + 1 + hashes;
        }
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_contracts_ignore_layout_and_comments_but_not_semantic_tokens() {
        let compact = "fn operation(){owner.commit(value);}";
        let formatted = r#"
            fn operation() {
                // A harmless explanation must not affect the contract.
                owner
                    .commit(
                        value,
                    );
            }
        "#;
        for source in [compact, formatted] {
            let operation = rust_item(source, "fn operation");
            assert!(operation.contains("owner.commit(value)"));
            assert!(!operation.contains("owner.rollback(value)"));
        }

        let semantic_mutation =
            rust_item("fn operation() { owner.rollback(value); }", "fn operation");
        assert!(!semantic_mutation.contains("owner.commit(value)"));
        assert!(semantic_mutation.contains("owner.rollback(value)"));
    }

    #[test]
    fn match_arm_selection_uses_syn_spans_not_brace_text_scanning() {
        let source = r#"
            fn dispatch(value: Event) {
                match value {
                    Event::Target { payload } => {
                        let diagnostic = "braces in a literal: { ignored }";
                        handle(payload, diagnostic);
                    }
                    Event::Other => {}
                }
            }
        "#;
        let arm = rust_match_arm(source, "Event :: Target");
        assert!(arm.contains("handle(payload, diagnostic)"));
        assert!(!arm.contains("Event::Other"));
    }
}
