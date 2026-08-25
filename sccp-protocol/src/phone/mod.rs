//! Phone-facing HTTP and XML protocol models.
//!
//! Use [`authentication`] to validate credentials submitted to an HTTP
//! authentication endpoint, [`provisioning`] to read and write boot
//! configuration documents, and [`service`] to decode application-envelope
//! payloads. The [`xml`] module contains the interactive display documents and
//! the shared [`xml::PhoneXmlDocument`] parsing contract.

/// Form-encoded authentication requests and bounded decision responses.
pub mod authentication;
/// Typed boot provisioning documents and their validation rules.
pub mod provisioning;
/// Application-envelope routing, submissions, and execute responses.
pub mod service;
/// Bounded XML documents for interactive phone displays and telemetry.
pub mod xml;

mod validation;
