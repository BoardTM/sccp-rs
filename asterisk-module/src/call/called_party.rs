//! Typed called-party overrides for the dialplan.
//!
//! `SCCPSetCalledParty(number)` or `SCCPSetCalledParty(name <number>)` updates
//! the called-party presentation of the current module-owned channel. A quoted
//! name supports only escaped quote and backslash. Names are bounded to 39
//! bytes and numbers to 23 bytes without control/NUL text. The provider updates
//! native connected-line state and the controller's typed call snapshot before
//! the next handset call-info publication.

use thiserror::Error;

use crate::pbx::dialplan::{
    DialplanApplicationResult, DialplanBackend, DialplanCallbackError, DialplanError,
    DialplanLimits,
};
use crate::pbx::party::AsteriskChannel;

pub const CALLED_PARTY_APPLICATION: &str = "SCCPSetCalledParty";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CalledPartyOverride {
    /// `None` means that no display name was supplied. `Some("")` preserves
    /// an explicitly empty quoted name.
    pub name: Option<String>,
    pub number: String,
}

impl CalledPartyOverride {
    pub fn parse(arguments: &str) -> Result<Self, CalledPartyError> {
        let arguments = arguments.trim();
        if arguments.is_empty() || arguments.len() > 256 || arguments.chars().any(char::is_control)
        {
            return Err(CalledPartyError::InvalidArguments);
        }
        if arguments.ends_with('>') {
            let opening = arguments
                .rfind('<')
                .ok_or(CalledPartyError::InvalidArguments)?;
            let name = arguments[..opening].trim();
            let number = &arguments[opening + 1..arguments.len() - 1];
            if name.contains(['<', '>']) {
                return Err(CalledPartyError::InvalidArguments);
            }
            return Ok(Self {
                name: parse_name(name)?,
                number: validated_number(number)?.to_owned(),
            });
        }
        if arguments.contains(['<', '>']) {
            return Err(CalledPartyError::InvalidArguments);
        }
        Ok(Self {
            name: None,
            number: validated_number(arguments)?.to_owned(),
        })
    }
}

fn parse_name(value: &str) -> Result<Option<String>, CalledPartyError> {
    if value.is_empty() {
        return Ok(None);
    }
    let parsed = if value.starts_with('"') || value.ends_with('"') {
        if value.len() < 2 || !value.starts_with('"') || !value.ends_with('"') {
            return Err(CalledPartyError::InvalidName);
        }
        let mut parsed = String::new();
        let mut escaped = false;
        for character in value[1..value.len() - 1].chars() {
            if escaped {
                if !matches!(character, '"' | '\\') {
                    return Err(CalledPartyError::InvalidName);
                }
                parsed.push(character);
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                return Err(CalledPartyError::InvalidName);
            } else {
                parsed.push(character);
            }
        }
        if escaped {
            return Err(CalledPartyError::InvalidName);
        }
        parsed
    } else {
        if value.contains(['"', '\\']) {
            return Err(CalledPartyError::InvalidName);
        }
        value.to_owned()
    };
    if parsed.len() > 39 || parsed.contains(['<', '>']) || parsed.chars().any(char::is_control) {
        return Err(CalledPartyError::InvalidName);
    }
    Ok(Some(parsed))
}

fn validated_number(value: &str) -> Result<&str, CalledPartyError> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 23
        || !value.is_ascii()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && !matches!(byte, b'<' | b'>' | b'"' | b'\\'))
    {
        return Err(CalledPartyError::InvalidNumber);
    }
    Ok(value)
}

pub trait CalledPartyProvider: Send + Sync + 'static {
    fn replace(
        &self,
        channel: &AsteriskChannel<'_>,
        called_party: &CalledPartyOverride,
    ) -> Result<(), CalledPartyProviderError>;
}

pub struct CalledPartyApplication<P> {
    provider: P,
}

impl<P: CalledPartyProvider> CalledPartyApplication<P> {
    pub const fn new(provider: P) -> Self {
        Self { provider }
    }

    pub fn execute(
        &self,
        arguments: &str,
        channel: &AsteriskChannel<'_>,
    ) -> Result<(), CalledPartyError> {
        let called_party = CalledPartyOverride::parse(arguments)?;
        self.provider
            .replace(channel, &called_party)
            .map_err(CalledPartyError::Provider)
    }
}

pub fn register_called_party_application<P: CalledPartyProvider, B: DialplanBackend>(
    provider: P,
    backend: B,
) -> Result<B::Registration, DialplanError> {
    let application = CalledPartyApplication::new(provider);
    backend.register_application(
        CALLED_PARTY_APPLICATION,
        "Set the current channel called party",
        "Replace the typed called-party name and number shown for the current channel",
        DialplanLimits {
            max_arguments_bytes: 256,
            max_value_bytes: 1,
            max_output_bytes: 1,
        },
        move |invocation| {
            application
                .execute(&invocation.arguments, &invocation.channel)
                .map(|()| DialplanApplicationResult::CONTINUE)
                .map_err(|_| DialplanCallbackError::Failed)
        },
    )
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum CalledPartyProviderError {
    #[error("the callback channel is not owned by this driver")]
    NotDriverChannel,
    #[error("the channel or handset appearance is unavailable")]
    Unavailable,
    #[error("called-party metadata was rejected by the channel backend")]
    NativeRejected,
    #[error("the called-party display could not be queued to the handset")]
    HandsetRejected,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum CalledPartyError {
    #[error("called party expects a number or a name followed by an angle-bracketed number")]
    InvalidArguments,
    #[error("called-party name is malformed or exceeds its bound")]
    InvalidName,
    #[error("called-party number is malformed or exceeds its bound")]
    InvalidNumber,
    #[error(transparent)]
    Provider(#[from] CalledPartyProviderError),
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    struct FakeProvider {
        replacements: Mutex<Vec<(usize, CalledPartyOverride)>>,
        failure: Option<CalledPartyProviderError>,
    }

    impl CalledPartyProvider for FakeProvider {
        fn replace(
            &self,
            channel: &AsteriskChannel<'_>,
            called_party: &CalledPartyOverride,
        ) -> Result<(), CalledPartyProviderError> {
            if let Some(error) = self.failure {
                return Err(error);
            }
            self.replacements
                .lock()
                .unwrap()
                .push((channel.as_raw() as usize, called_party.clone()));
            Ok(())
        }
    }

    fn channel() -> AsteriskChannel<'static> {
        let pointer = Box::leak(Box::new(1_u8));
        unsafe { AsteriskChannel::from_raw(std::ptr::from_mut(pointer).cast()).unwrap() }
    }

    fn application() -> CalledPartyApplication<FakeProvider> {
        CalledPartyApplication::new(FakeProvider {
            replacements: Mutex::new(Vec::new()),
            failure: None,
        })
    }

    #[test]
    fn parses_documented_name_and_number_and_bare_number() {
        assert_eq!(
            CalledPartyOverride::parse("\"Alice Example\" <12065550100>"),
            Ok(CalledPartyOverride {
                name: Some("Alice Example".into()),
                number: "12065550100".into(),
            })
        );
        assert_eq!(
            CalledPartyOverride::parse("Support Desk <*8123>"),
            Ok(CalledPartyOverride {
                name: Some("Support Desk".into()),
                number: "*8123".into(),
            })
        );
        assert_eq!(
            CalledPartyOverride::parse("+12065550100"),
            Ok(CalledPartyOverride {
                name: None,
                number: "+12065550100".into(),
            })
        );
    }

    #[test]
    fn quoted_names_preserve_explicit_empty_and_safe_escapes() {
        assert_eq!(
            CalledPartyOverride::parse("\"\" <1001>").unwrap().name,
            Some(String::new())
        );
        assert_eq!(
            CalledPartyOverride::parse("\"Alice \\\"A\\\" Example\" <1001>")
                .unwrap()
                .name,
            Some("Alice \"A\" Example".into())
        );
    }

    #[test]
    fn malformed_and_secret_like_inputs_fail_without_disclosure() {
        for arguments in [
            "",
            "<> latest-password",
            "Alice <>",
            "Alice <1001",
            "Alice 1001>",
            "\"Alice <1001>",
            "\"Alice\\q\" <1001>",
            "Alice <10 01>",
            "Alice <10\\01>",
            "token\n<1001>",
            "private-key\0<1001>",
        ] {
            let error = CalledPartyOverride::parse(arguments).unwrap_err();
            if !arguments.is_empty() {
                assert!(!error.to_string().contains(arguments));
            }
        }
    }

    #[test]
    fn name_and_number_bounds_are_checked_in_bytes() {
        assert_eq!(
            CalledPartyOverride::parse(&format!("{} <1001>", "x".repeat(40))),
            Err(CalledPartyError::InvalidName)
        );
        assert_eq!(
            CalledPartyOverride::parse(&"1".repeat(24)),
            Err(CalledPartyError::InvalidNumber)
        );
        assert_eq!(
            CalledPartyOverride::parse(&format!("{} <1001>", "å".repeat(20))),
            Err(CalledPartyError::InvalidName)
        );
    }

    #[test]
    fn fake_provider_preserves_exact_channel_and_typed_identity() {
        let channel = channel();
        let application = application();
        application.execute("\"Alice\" <1001>", &channel).unwrap();
        assert_eq!(
            application.provider.replacements.into_inner().unwrap(),
            [(
                channel.as_raw() as usize,
                CalledPartyOverride {
                    name: Some("Alice".into()),
                    number: "1001".into(),
                }
            )]
        );
    }

    #[test]
    fn provider_failures_remain_typed_and_secret_safe() {
        let application = CalledPartyApplication::new(FakeProvider {
            replacements: Mutex::new(Vec::new()),
            failure: Some(CalledPartyProviderError::NativeRejected),
        });
        assert_eq!(
            application.execute("private-destination", &channel()),
            Err(CalledPartyError::Provider(
                CalledPartyProviderError::NativeRejected
            ))
        );
        assert!(
            !application
                .execute("private-destination", &channel())
                .unwrap_err()
                .to_string()
                .contains("private-destination")
        );
    }

    #[test]
    fn registration_is_unavailable_without_native_linkage() {
        let result = register_called_party_application(
            FakeProvider {
                replacements: Mutex::new(Vec::new()),
                failure: None,
            },
            crate::pbx::dialplan::UnavailableDialplan,
        );
        assert!(matches!(result, Err(DialplanError::Unavailable)));
    }
}
