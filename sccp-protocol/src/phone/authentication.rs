//! Typed, secret-safe values for the phone HTTP authentication exchange.
//!
//! This exchange is form-encoded HTTP with a plain-text decision token. It is
//! intentionally separate from the phone XML models because neither the
//! request nor the response is an XML document.

use std::fmt;
use std::io::Write;

use percent_encoding::percent_decode_str;
use thiserror::Error;

use crate::types::DeviceId;

/// Maximum encoded size accepted by [`PhoneAuthenticationRequest::parse_query`].
pub const PHONE_AUTHENTICATION_MAX_QUERY_BYTES: usize = 1_024;
/// Maximum UTF-8 byte length of an authentication user identifier.
pub const PHONE_AUTHENTICATION_MAX_USER_ID_BYTES: usize = 128;
/// Maximum UTF-8 byte length of an authentication password.
pub const PHONE_AUTHENTICATION_MAX_PASSWORD_BYTES: usize = 256;
/// Maximum response size retained or emitted by this module.
pub const PHONE_AUTHENTICATION_MAX_RESPONSE_BYTES: usize = 256;

const AUTHORIZED: &[u8] = b"AUTHORIZED";
const UNAUTHORIZED: &[u8] = b"UN-AUTHORIZED";

/// User identifier forwarded by the phone to its authentication service.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct PhoneAuthenticationUserId(String);

impl PhoneAuthenticationUserId {
    /// Validates and wraps an identifier without exposing it through diagnostics.
    pub fn new(value: impl Into<String>) -> Result<Self, PhoneAuthenticationError> {
        let value = value.into();
        validate_credential(
            "authentication user identifier",
            &value,
            PHONE_AUTHENTICATION_MAX_USER_ID_BYTES,
        )?;
        Ok(Self(value))
    }

    /// Exposes the credential only to an authentication policy implementation.
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for PhoneAuthenticationUserId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PhoneAuthenticationUserId(<redacted>)")
    }
}

impl TryFrom<String> for PhoneAuthenticationUserId {
    type Error = PhoneAuthenticationError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

/// Password forwarded by the phone to its authentication service.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct PhoneAuthenticationPassword(String);

impl PhoneAuthenticationPassword {
    /// Validates and wraps a password without exposing it through diagnostics.
    pub fn new(value: impl Into<String>) -> Result<Self, PhoneAuthenticationError> {
        let value = value.into();
        validate_credential(
            "authentication password",
            &value,
            PHONE_AUTHENTICATION_MAX_PASSWORD_BYTES,
        )?;
        Ok(Self(value))
    }

    /// Exposes the credential only to an authentication policy implementation.
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for PhoneAuthenticationPassword {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PhoneAuthenticationPassword(<redacted>)")
    }
}

impl TryFrom<String> for PhoneAuthenticationPassword {
    type Error = PhoneAuthenticationError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

/// The three fields supplied to the configured phone authentication URL.
///
/// Debug output redacts the user ID and password while retaining the device ID
/// for session diagnostics.
#[derive(Clone, Eq, PartialEq)]
pub struct PhoneAuthenticationRequest {
    pub user_id: PhoneAuthenticationUserId,
    pub password: PhoneAuthenticationPassword,
    pub device_id: DeviceId,
}

impl PhoneAuthenticationRequest {
    /// Parses an `application/x-www-form-urlencoded` query with exact field
    /// names `UserID`, `Password`, and `devicename`.
    pub fn parse_query(query: &[u8]) -> Result<Self, PhoneAuthenticationError> {
        if query.len() > PHONE_AUTHENTICATION_MAX_QUERY_BYTES {
            return Err(PhoneAuthenticationError::QueryExceedsLimit);
        }
        let query =
            std::str::from_utf8(query).map_err(|_| PhoneAuthenticationError::InvalidEncoding)?;
        validate_encoded_form(query)?;
        Self::from_fields(
            form_urlencoded::parse(query.as_bytes())
                .map(|(name, value)| (name.into_owned(), value.into_owned())),
        )
    }

    /// Validates fields already decoded by a standards-based HTTP boundary.
    pub fn from_fields<I, N, V>(fields: I) -> Result<Self, PhoneAuthenticationError>
    where
        I: IntoIterator<Item = (N, V)>,
        N: AsRef<str>,
        V: AsRef<str>,
    {
        let mut user_id = None;
        let mut password = None;
        let mut device_id = None;
        for (name, value) in fields {
            let name = name.as_ref();
            let value = value.as_ref();
            match name {
                "UserID" => set_once(
                    &mut user_id,
                    "UserID",
                    PhoneAuthenticationUserId::new(value)?,
                )?,
                "Password" => set_once(
                    &mut password,
                    "Password",
                    PhoneAuthenticationPassword::new(value)?,
                )?,
                "devicename" => {
                    if value.trim() != value || value.chars().any(char::is_control) {
                        return Err(PhoneAuthenticationError::InvalidDeviceName);
                    }
                    let parsed = DeviceId::new(value)
                        .map_err(|_| PhoneAuthenticationError::InvalidDeviceName)?;
                    set_once(&mut device_id, "devicename", parsed)?;
                }
                _ => return Err(PhoneAuthenticationError::UnknownField),
            }
        }
        Ok(Self {
            user_id: user_id.ok_or(PhoneAuthenticationError::MissingField("UserID"))?,
            password: password.ok_or(PhoneAuthenticationError::MissingField("Password"))?,
            device_id: device_id.ok_or(PhoneAuthenticationError::MissingField("devicename"))?,
        })
    }
}

impl fmt::Debug for PhoneAuthenticationRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PhoneAuthenticationRequest")
            .field("user_id", &"<redacted>")
            .field("password", &"<redacted>")
            .field("device_id", &self.device_id)
            .finish()
    }
}

/// A bounded unsupported authentication response retained without inspection.
#[derive(Clone, Eq, PartialEq)]
pub struct OpaquePhoneAuthenticationResponse(Vec<u8>);

impl OpaquePhoneAuthenticationResponse {
    /// Retains an unrecognized response after enforcing the response byte limit.
    pub fn new(value: Vec<u8>) -> Result<Self, PhoneAuthenticationError> {
        validate_response_size(value.len())?;
        Ok(Self(value))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for OpaquePhoneAuthenticationResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpaquePhoneAuthenticationResponse")
            .field("bytes", &self.0.len())
            .finish()
    }
}

/// Plain-text decision returned by a phone authentication endpoint.
#[derive(Clone, Eq, PartialEq)]
pub enum PhoneAuthenticationResponse {
    Authorized,
    Unauthorized,
    /// A bounded response token not recognized by this version of the crate.
    Opaque(OpaquePhoneAuthenticationResponse),
}

impl PhoneAuthenticationResponse {
    /// Parses a bounded decision token, preserving unrecognized bytes exactly.
    pub fn from_bytes(value: &[u8]) -> Result<Self, PhoneAuthenticationError> {
        validate_response_size(value.len())?;
        let trimmed = value.trim_ascii();
        Ok(match trimmed {
            AUTHORIZED => Self::Authorized,
            UNAUTHORIZED => Self::Unauthorized,
            _ => Self::Opaque(OpaquePhoneAuthenticationResponse(value.to_vec())),
        })
    }

    /// Borrows the canonical decision token or the preserved opaque response.
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Authorized => AUTHORIZED,
            Self::Unauthorized => UNAUTHORIZED,
            Self::Opaque(value) => value.as_bytes(),
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        self.as_bytes().to_vec()
    }

    /// Writes the serialized response without logging or formatting credentials.
    pub fn write_to(&self, mut writer: impl Write) -> Result<(), PhoneAuthenticationError> {
        writer
            .write_all(self.as_bytes())
            .map_err(|_| PhoneAuthenticationError::Write)
    }
}

impl fmt::Debug for PhoneAuthenticationResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Authorized => formatter.write_str("Authorized"),
            Self::Unauthorized => formatter.write_str("Unauthorized"),
            Self::Opaque(value) => formatter.debug_tuple("Opaque").field(value).finish(),
        }
    }
}

/// Validation and I/O failures at the authentication HTTP boundary.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum PhoneAuthenticationError {
    /// The encoded query is larger than [`PHONE_AUTHENTICATION_MAX_QUERY_BYTES`].
    #[error("phone authentication query exceeds its byte limit")]
    QueryExceedsLimit,
    /// The query is not a canonical, control-free encoded form.
    #[error("phone authentication form is not valid UTF-8 or percent encoding")]
    InvalidEncoding,
    #[error("phone authentication form contains an unknown field")]
    UnknownField,
    #[error("phone authentication form repeats field {0}")]
    DuplicateField(&'static str),
    #[error("phone authentication form is missing field {0}")]
    MissingField(&'static str),
    /// A credential violates its byte bound or contains a control character.
    #[error("phone authentication credential {field} exceeds its bound or contains controls")]
    InvalidCredential { field: &'static str },
    /// The device name is not a valid [`DeviceId`].
    #[error("phone authentication device name is invalid")]
    InvalidDeviceName,
    /// The response exceeds [`PHONE_AUTHENTICATION_MAX_RESPONSE_BYTES`].
    #[error("phone authentication response exceeds its byte limit")]
    ResponseExceedsLimit,
    #[error("unable to write phone authentication response")]
    Write,
}

fn validate_credential(
    field: &'static str,
    value: &str,
    maximum: usize,
) -> Result<(), PhoneAuthenticationError> {
    if value.len() > maximum || value.chars().any(char::is_control) {
        return Err(PhoneAuthenticationError::InvalidCredential { field });
    }
    Ok(())
}

fn validate_response_size(actual: usize) -> Result<(), PhoneAuthenticationError> {
    if actual > PHONE_AUTHENTICATION_MAX_RESPONSE_BYTES {
        return Err(PhoneAuthenticationError::ResponseExceedsLimit);
    }
    Ok(())
}

fn set_once<T>(
    target: &mut Option<T>,
    field: &'static str,
    value: T,
) -> Result<(), PhoneAuthenticationError> {
    if target.replace(value).is_some() {
        return Err(PhoneAuthenticationError::DuplicateField(field));
    }
    Ok(())
}

fn validate_encoded_form(query: &str) -> Result<(), PhoneAuthenticationError> {
    if query.is_empty() {
        return Ok(());
    }
    for field in query.split('&') {
        if field.is_empty() {
            return Err(PhoneAuthenticationError::InvalidEncoding);
        }
        let (name, value) = field.split_once('=').unwrap_or((field, ""));
        validate_percent_triplets(name)?;
        validate_percent_triplets(value)?;
        for component in [name, value] {
            let decoded = percent_decode_str(component)
                .decode_utf8()
                .map_err(|_| PhoneAuthenticationError::InvalidEncoding)?;
            if decoded.chars().any(char::is_control) {
                return Err(PhoneAuthenticationError::InvalidEncoding);
            }
        }
    }
    Ok(())
}

fn validate_percent_triplets(value: &str) -> Result<(), PhoneAuthenticationError> {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let Some(pair) = bytes.get(index + 1..index + 3) else {
                return Err(PhoneAuthenticationError::InvalidEncoding);
            };
            if !pair.iter().all(u8::is_ascii_hexdigit) {
                return Err(PhoneAuthenticationError::InvalidEncoding);
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    #[test]
    fn request_decodes_exact_form_fields_and_redacts_credentials() {
        let request = PhoneAuthenticationRequest::parse_query(
            b"UserID=alex%40example.test&Password=p%40ss+word%26more&devicename=sep001122334455",
        )
        .unwrap();
        assert_eq!(request.user_id.expose_secret(), "alex@example.test");
        assert_eq!(request.password.expose_secret(), "p@ss word&more");
        assert_eq!(request.device_id.as_str(), "SEP001122334455");

        let debug = format!("{request:?}");
        assert!(!debug.contains("alex"));
        assert!(!debug.contains("p@ss"));
        assert!(debug.contains("<redacted>"));
        assert_eq!(
            format!("{:?}", request.user_id),
            "PhoneAuthenticationUserId(<redacted>)"
        );
        assert_eq!(
            format!("{:?}", request.password),
            "PhoneAuthenticationPassword(<redacted>)"
        );
    }

    #[test]
    fn request_requires_exact_unique_fields_and_secret_safe_bounds() {
        for query in [
            "UserId=private-user&Password=private-pass&devicename=SEP001122334455",
            "UserID=private-user&password=private-pass&devicename=SEP001122334455",
            "UserID=private-user&Password=private-pass&DeviceName=SEP001122334455",
            "UserID=private-user&Password=private-pass",
            "UserID=private-user&UserID=other&Password=private-pass&devicename=SEP001122334455",
            "UserID=private%Q0user&Password=private-pass&devicename=SEP001122334455",
            "UserID=private%0Auser&Password=private-pass&devicename=SEP001122334455",
            "UserID=private-user&Password=private-pass&devicename=../../secret",
        ] {
            let error = PhoneAuthenticationRequest::parse_query(query.as_bytes()).unwrap_err();
            let text = error.to_string();
            assert!(!text.contains("private-user"), "{text}");
            assert!(!text.contains("private-pass"), "{text}");
        }

        let oversized = format!(
            "UserID={}&Password=secret&devicename=SEP001122334455",
            "u".repeat(PHONE_AUTHENTICATION_MAX_USER_ID_BYTES + 1)
        );
        let error = PhoneAuthenticationRequest::parse_query(oversized.as_bytes()).unwrap_err();
        assert!(!error.to_string().contains(&"u".repeat(32)));
        let oversized = format!(
            "UserID=user&Password={}&devicename=SEP001122334455",
            "p".repeat(PHONE_AUTHENTICATION_MAX_PASSWORD_BYTES + 1)
        );
        let error = PhoneAuthenticationRequest::parse_query(oversized.as_bytes()).unwrap_err();
        assert!(!error.to_string().contains(&"p".repeat(32)));
        assert!(matches!(
            PhoneAuthenticationRequest::parse_query(&vec![
                b'x';
                PHONE_AUTHENTICATION_MAX_QUERY_BYTES + 1
            ]),
            Err(PhoneAuthenticationError::QueryExceedsLimit)
        ));
        assert!(matches!(
            PhoneAuthenticationRequest::parse_query(&[0xff]),
            Err(PhoneAuthenticationError::InvalidEncoding)
        ));
    }

    #[test]
    fn empty_credentials_are_typed_for_policy_driven_denial() {
        let request = PhoneAuthenticationRequest::parse_query(
            b"UserID=&Password=&devicename=SEP001122334455",
        )
        .unwrap();
        assert!(request.user_id.expose_secret().is_empty());
        assert!(request.password.expose_secret().is_empty());
    }

    #[test]
    fn response_round_trips_exact_tokens_and_preserves_unknown_bodies_opaquely() {
        for expected in [
            PhoneAuthenticationResponse::Authorized,
            PhoneAuthenticationResponse::Unauthorized,
        ] {
            assert_eq!(
                PhoneAuthenticationResponse::from_bytes(expected.as_bytes()).unwrap(),
                expected
            );
        }
        assert_eq!(
            PhoneAuthenticationResponse::from_bytes(b"AUTHORIZED\r\n").unwrap(),
            PhoneAuthenticationResponse::Authorized
        );

        for unknown in [
            b"MAYBE".as_slice(),
            b"<!DOCTYPE auth [<!ENTITY secret 'private'>]><auth>&secret;</auth>".as_slice(),
            b"<auth><nested><result>AUTHORIZED</result></nested></auth>".as_slice(),
            b"<auth><".as_slice(),
            &[0xff],
        ] {
            let response = PhoneAuthenticationResponse::from_bytes(unknown).unwrap();
            let PhoneAuthenticationResponse::Opaque(value) = response else {
                panic!("unknown authentication body must remain opaque");
            };
            assert_eq!(value.as_bytes(), unknown);
            let debug = format!("{value:?}");
            assert!(debug.contains(&unknown.len().to_string()));
            assert!(!debug.contains("private"));
            assert!(!debug.contains("AUTHORIZED"));
        }
        let nested = format!("<auth>{}{}</auth>", "<n>".repeat(33), "</n>".repeat(33));
        assert!(nested.len() <= PHONE_AUTHENTICATION_MAX_RESPONSE_BYTES);
        assert!(matches!(
            PhoneAuthenticationResponse::from_bytes(nested.as_bytes()).unwrap(),
            PhoneAuthenticationResponse::Opaque(_)
        ));
        assert!(matches!(
            PhoneAuthenticationResponse::from_bytes(&vec![
                b'x';
                PHONE_AUTHENTICATION_MAX_RESPONSE_BYTES
                    + 1
            ]),
            Err(PhoneAuthenticationError::ResponseExceedsLimit)
        ));
    }

    #[test]
    fn response_writer_propagates_failures_without_body_data() {
        #[derive(Debug)]
        struct FailingWriter;
        impl Write for FailingWriter {
            fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("sensitive downstream context"))
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let mut body = Vec::new();
        PhoneAuthenticationResponse::Authorized
            .write_to(&mut body)
            .unwrap();
        assert_eq!(body, AUTHORIZED);
        let error = PhoneAuthenticationResponse::Unauthorized
            .write_to(FailingWriter)
            .unwrap_err();
        assert_eq!(error, PhoneAuthenticationError::Write);
        assert!(!error.to_string().contains("sensitive"));
    }
}
