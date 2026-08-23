//! Typed status-line messages for the dialplan.
//!
//! `SCCPSetMessage(message[,timeout[,priority]])` routes to the exact handset
//! appearance owned by the current channel. Text is at most 31 UTF-8 bytes,
//! timeout is `0..=255` seconds, and priority is a known phone notification
//! priority (`0..=6`). Empty text clears that priority. The field parser uses
//! quoted comma/quote/backslash escaping; it is not an XML surface.

use sccp_protocol::{HandsetStatusMessage, NotificationPriority};
use thiserror::Error;

use crate::pbx::dialplan::{
    DialplanApplicationResult, DialplanBackend, DialplanCallbackError, DialplanError,
    DialplanLimits,
};
use crate::pbx::party::AsteriskChannel;

pub const HANDSET_MESSAGE_APPLICATION: &str = "SCCPSetMessage";
const MAX_MESSAGE_BYTES: usize = 31;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HandsetMessageOperation(pub HandsetStatusMessage);

impl HandsetMessageOperation {
    pub fn parse(arguments: &str) -> Result<Self, HandsetMessageError> {
        if arguments.len() > 128 || arguments.chars().any(char::is_control) {
            return Err(HandsetMessageError::InvalidArguments);
        }
        let fields = parse_fields(arguments)?;
        if fields.is_empty() || fields.len() > 3 {
            return Err(HandsetMessageError::InvalidArguments);
        }
        let text = &fields[0];
        if text.len() > MAX_MESSAGE_BYTES || text.chars().any(char::is_control) {
            return Err(HandsetMessageError::InvalidText);
        }
        let timeout_seconds = match fields.get(1).map(String::as_str) {
            None | Some("") => 0,
            Some(value) => parse_decimal(value).ok_or(HandsetMessageError::InvalidTimeout)?,
        };
        let priority = match fields.get(2).map(String::as_str) {
            None | Some("") => None,
            Some(value) => {
                let value =
                    u32::from(parse_decimal(value).ok_or(HandsetMessageError::InvalidPriority)?);
                let priority = NotificationPriority::from(value);
                if !priority.is_known() {
                    return Err(HandsetMessageError::InvalidPriority);
                }
                Some(priority)
            }
        };
        Ok(Self(if text.is_empty() {
            HandsetStatusMessage::Clear { priority }
        } else {
            HandsetStatusMessage::Display {
                text: text.clone(),
                timeout_seconds,
                priority,
            }
        }))
    }
}

fn parse_decimal(value: &str) -> Option<u8> {
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        value.parse().ok()
    } else {
        None
    }
}

fn parse_fields(arguments: &str) -> Result<Vec<String>, HandsetMessageError> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut quote_closed = false;
    let mut escaped = false;
    let mut was_quoted = false;
    for character in arguments.chars() {
        if escaped {
            if !matches!(character, ',' | '"' | '\\') {
                return Err(HandsetMessageError::InvalidArguments);
            }
            field.push(character);
            escaped = false;
            continue;
        }
        if quoted {
            match character {
                '\\' => escaped = true,
                '"' => {
                    quoted = false;
                    quote_closed = true;
                }
                character => field.push(character),
            }
            continue;
        }
        match character {
            '\\' if !quote_closed => escaped = true,
            '"' if field.trim().is_empty() && !quote_closed => {
                field.clear();
                quoted = true;
                was_quoted = true;
            }
            ',' => {
                fields.push(if was_quoted {
                    std::mem::take(&mut field)
                } else {
                    std::mem::take(&mut field).trim().to_owned()
                });
                quoted = false;
                quote_closed = false;
                was_quoted = false;
            }
            character if quote_closed && character.is_whitespace() => {}
            _ if quote_closed => return Err(HandsetMessageError::InvalidArguments),
            character => field.push(character),
        }
    }
    if escaped || quoted {
        return Err(HandsetMessageError::InvalidArguments);
    }
    fields.push(if was_quoted {
        field
    } else {
        field.trim().to_owned()
    });
    Ok(fields)
}

pub trait HandsetMessageProvider: Send + Sync + 'static {
    fn apply(
        &self,
        channel: &AsteriskChannel<'_>,
        operation: &HandsetMessageOperation,
    ) -> Result<(), HandsetMessageProviderError>;
}

pub struct HandsetMessageApplication<P> {
    provider: P,
}

impl<P: HandsetMessageProvider> HandsetMessageApplication<P> {
    pub const fn new(provider: P) -> Self {
        Self { provider }
    }

    pub fn execute(
        &self,
        arguments: &str,
        channel: &AsteriskChannel<'_>,
    ) -> Result<(), HandsetMessageError> {
        let operation = HandsetMessageOperation::parse(arguments)?;
        self.provider
            .apply(channel, &operation)
            .map_err(HandsetMessageError::Provider)
    }
}

pub fn register_handset_message_application<P: HandsetMessageProvider, B: DialplanBackend>(
    provider: P,
    backend: B,
) -> Result<B::Registration, DialplanError> {
    let application = HandsetMessageApplication::new(provider);
    backend.register_application(
        HANDSET_MESSAGE_APPLICATION,
        "Set a handset status-line message",
        "Display or clear a bounded status-line message for the current channel's handset",
        DialplanLimits {
            max_arguments_bytes: 128,
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
pub enum HandsetMessageProviderError {
    #[error("the callback channel is not owned by this driver")]
    NotDriverChannel,
    #[error("the channel or handset appearance is unavailable")]
    Unavailable,
    #[error("the handset is not registered")]
    NotRegistered,
    #[error("the status-line command could not be queued to the handset")]
    HandsetRejected,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum HandsetMessageError {
    #[error("status message expects message, optional timeout, and optional priority fields")]
    InvalidArguments,
    #[error("status message text contains unsupported data or exceeds its bound")]
    InvalidText,
    #[error("status message timeout must be an integer from 0 through 255")]
    InvalidTimeout,
    #[error("status message priority must be an integer from 0 through 6")]
    InvalidPriority,
    #[error(transparent)]
    Provider(#[from] HandsetMessageProviderError),
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;

    struct FakeProvider {
        operations: Mutex<Vec<(usize, HandsetMessageOperation)>>,
        failure: Option<HandsetMessageProviderError>,
        drops: Option<Arc<AtomicUsize>>,
    }

    impl Drop for FakeProvider {
        fn drop(&mut self) {
            if let Some(drops) = &self.drops {
                drops.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    impl HandsetMessageProvider for FakeProvider {
        fn apply(
            &self,
            channel: &AsteriskChannel<'_>,
            operation: &HandsetMessageOperation,
        ) -> Result<(), HandsetMessageProviderError> {
            if let Some(error) = self.failure {
                return Err(error);
            }
            self.operations
                .lock()
                .unwrap()
                .push((channel.as_raw() as usize, operation.clone()));
            Ok(())
        }
    }

    fn channel() -> AsteriskChannel<'static> {
        let pointer = Box::leak(Box::new(1_u8));
        unsafe { AsteriskChannel::from_raw(std::ptr::from_mut(pointer).cast()).unwrap() }
    }

    fn display(
        text: &str,
        timeout_seconds: u8,
        priority: Option<NotificationPriority>,
    ) -> HandsetMessageOperation {
        HandsetMessageOperation(HandsetStatusMessage::Display {
            text: text.into(),
            timeout_seconds,
            priority,
        })
    }

    #[test]
    fn parses_documented_message_timeout_priority_and_defaults() {
        assert_eq!(
            HandsetMessageOperation::parse("\"Test Test\", 10"),
            Ok(display("Test Test", 10, None))
        );
        assert_eq!(
            HandsetMessageOperation::parse("Ready"),
            Ok(display("Ready", 0, None))
        );
        assert_eq!(
            HandsetMessageOperation::parse("Alert,,6"),
            Ok(display("Alert", 0, Some(NotificationPriority::Timed)))
        );
    }

    #[test]
    fn quoted_and_escaped_message_fields_preserve_text() {
        assert_eq!(
            HandsetMessageOperation::parse("\"Sales, \\\"West\\\"\\\\Desk\",5,3"),
            Ok(display(
                "Sales, \"West\"\\Desk",
                5,
                Some(NotificationPriority::Privacy)
            ))
        );
        assert_eq!(
            HandsetMessageOperation::parse("Sales\\, West,5"),
            Ok(display("Sales, West", 5, None))
        );
    }

    #[test]
    fn empty_text_clears_the_selected_or_default_slot() {
        assert_eq!(
            HandsetMessageOperation::parse(""),
            Ok(HandsetMessageOperation(HandsetStatusMessage::Clear {
                priority: None
            }))
        );
        assert_eq!(
            HandsetMessageOperation::parse(",,4"),
            Ok(HandsetMessageOperation(HandsetStatusMessage::Clear {
                priority: Some(NotificationPriority::DoNotDisturb)
            }))
        );
    }

    #[test]
    fn timeout_and_every_documented_priority_are_bounded() {
        assert_eq!(
            HandsetMessageOperation::parse("Ready,255,0"),
            Ok(display("Ready", 255, Some(NotificationPriority::Idle)))
        );
        for priority in NotificationPriority::ALL_KNOWN {
            let parsed =
                HandsetMessageOperation::parse(&format!("Ready,1,{}", priority.wire_value()))
                    .unwrap();
            assert_eq!(parsed, display("Ready", 1, Some(*priority)));
        }
        for arguments in ["Ready,-1", "Ready,256", "Ready,1,-1", "Ready,1,7"] {
            assert!(HandsetMessageOperation::parse(arguments).is_err());
        }
    }

    #[test]
    fn malformed_and_secret_like_arguments_fail_without_disclosure() {
        for arguments in [
            "private-key,1,2,3",
            "\"private-key,1,2",
            "private-key\\q,1",
            "\"private-key\"suffix,1",
            "private-key\n,1",
        ] {
            let error = HandsetMessageOperation::parse(arguments).unwrap_err();
            assert!(!error.to_string().contains("private-key"));
        }
        assert_eq!(
            HandsetMessageOperation::parse(&"x".repeat(32)),
            Err(HandsetMessageError::InvalidText)
        );
        assert_eq!(
            HandsetMessageOperation::parse(&"å".repeat(16)),
            Err(HandsetMessageError::InvalidText)
        );
    }

    #[test]
    fn provider_receives_exact_channel_and_operation_and_owns_its_lifecycle() {
        let drops = Arc::new(AtomicUsize::new(0));
        let application = HandsetMessageApplication::new(FakeProvider {
            operations: Mutex::new(Vec::new()),
            failure: None,
            drops: Some(Arc::clone(&drops)),
        });
        let channel = channel();
        application.execute("Ready,5,2", &channel).unwrap();
        assert_eq!(
            application.provider.operations.lock().unwrap().as_slice(),
            [(
                channel.as_raw() as usize,
                display("Ready", 5, Some(NotificationPriority::Monitor))
            )]
        );
        drop(application);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn provider_failures_remain_typed_and_secret_safe() {
        let application = HandsetMessageApplication::new(FakeProvider {
            operations: Mutex::new(Vec::new()),
            failure: Some(HandsetMessageProviderError::HandsetRejected),
            drops: None,
        });
        let error = application.execute("private-key", &channel()).unwrap_err();
        assert_eq!(
            error,
            HandsetMessageError::Provider(HandsetMessageProviderError::HandsetRejected)
        );
        assert!(!error.to_string().contains("private-key"));
    }

    #[test]
    fn registration_is_unavailable_without_native_linkage() {
        let result = register_handset_message_application(
            FakeProvider {
                operations: Mutex::new(Vec::new()),
                failure: None,
                drops: None,
            },
            crate::pbx::dialplan::UnavailableDialplan,
        );
        assert!(matches!(result, Err(DialplanError::Unavailable)));
    }
}
