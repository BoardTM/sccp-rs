//! Typed parsing for the existing SCCP channel-request auto-answer surface.

use std::fmt;
use std::time::Duration;

use thiserror::Error;

use sccp_protocol::Tone;

const MAX_REQUEST_ADDRESS_BYTES: usize = 255;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AutoAnswerMode {
    OneWay,
    TwoWay,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AutoAnswerCause {
    Busy,
    Unavailable,
    Congestion,
}

impl AutoAnswerCause {
    pub const fn asterisk_code(self) -> i32 {
        match self {
            Self::Busy => 17,
            Self::Unavailable => 44,
            Self::Congestion => 34,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AutoAnswerRequest {
    pub mode: AutoAnswerMode,
    pub unavailable_cause: Option<AutoAnswerCause>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AutoAnswerPolicy {
    pub delay: Duration,
    pub tone: Tone,
}

#[derive(Clone, Eq, PartialEq)]
pub struct InboundDialRequest {
    target: String,
    auto_answer: Option<AutoAnswerRequest>,
}

impl InboundDialRequest {
    pub fn parse(raw: &str) -> Result<Self, AutoAnswerParseError> {
        if raw.is_empty()
            || raw.len() > MAX_REQUEST_ADDRESS_BYTES
            || raw.chars().any(char::is_control)
        {
            return Err(AutoAnswerParseError::InvalidAddress);
        }
        let parts = raw.split('/').collect::<Vec<_>>();
        if parts
            .iter()
            .any(|part| part.is_empty() || *part != part.trim())
        {
            return Err(AutoAnswerParseError::InvalidAddress);
        }
        let (target_parts, option) = match parts.as_slice() {
            [_] => (&parts[..1], None),
            [target, candidate] if parse_dial_option(candidate).is_ok() => {
                let _ = target;
                (&parts[..1], Some(*candidate))
            }
            [_, _] => (&parts[..2], None),
            [_, second, _] if parse_dial_option(second).is_ok() => {
                return Err(AutoAnswerParseError::DuplicateOption);
            }
            [_, _, candidate] => (&parts[..2], Some(*candidate)),
            _ => return Err(AutoAnswerParseError::DuplicateOption),
        };
        let auto_answer = option.map(parse_dial_option).transpose()?;
        Ok(Self {
            target: target_parts.join("/"),
            auto_answer,
        })
    }

    pub fn target(&self) -> &str {
        &self.target
    }

    pub fn auto_answer(&self) -> Option<AutoAnswerRequest> {
        self.auto_answer
    }

    pub fn apply_requestor_mode(&mut self, mode: Option<AutoAnswerMode>) {
        let Some(mode) = mode else {
            return;
        };
        self.auto_answer = Some(AutoAnswerRequest {
            mode,
            unavailable_cause: self
                .auto_answer
                .and_then(|request| request.unavailable_cause),
        });
    }
}

impl fmt::Debug for InboundDialRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InboundDialRequest")
            .field("target", &"<redacted>")
            .field("auto_answer", &self.auto_answer)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum AutoAnswerParseError {
    #[error("invalid SCCP channel-request address")]
    InvalidAddress,
    #[error("invalid SCCP auto-answer option")]
    InvalidOption,
    #[error("duplicate SCCP auto-answer option")]
    DuplicateOption,
    #[error("invalid SCCP requestor auto-answer mode")]
    InvalidRequestorMode,
}

pub fn parse_requestor_mode(
    raw: Option<&str>,
) -> Result<Option<AutoAnswerMode>, AutoAnswerParseError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    if raw.is_empty() {
        return Ok(None);
    }
    let mode = match raw.to_ascii_lowercase().as_str() {
        "1way" | "1w" => AutoAnswerMode::OneWay,
        "2way" | "2w" => AutoAnswerMode::TwoWay,
        _ => return Err(AutoAnswerParseError::InvalidRequestorMode),
    };
    Ok(Some(mode))
}

fn parse_dial_option(raw: &str) -> Result<AutoAnswerRequest, AutoAnswerParseError> {
    let normalized = raw.to_ascii_lowercase();
    let (mode, suffix) = if let Some(suffix) = normalized.strip_prefix("aa1w") {
        (AutoAnswerMode::OneWay, suffix)
    } else if let Some(suffix) = normalized.strip_prefix("aa2w") {
        (AutoAnswerMode::TwoWay, suffix)
    } else if let Some(suffix) = normalized.strip_prefix("aa=1w") {
        (AutoAnswerMode::OneWay, suffix)
    } else if let Some(suffix) = normalized.strip_prefix("aa=2w") {
        (AutoAnswerMode::TwoWay, suffix)
    } else {
        return Err(AutoAnswerParseError::InvalidOption);
    };
    let unavailable_cause = match suffix {
        "" => None,
        "b" => Some(AutoAnswerCause::Busy),
        "u" => Some(AutoAnswerCause::Unavailable),
        "c" => Some(AutoAnswerCause::Congestion),
        _ => return Err(AutoAnswerParseError::InvalidOption),
    };
    Ok(AutoAnswerRequest {
        mode,
        unavailable_cause,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_exact_address_grammars_and_causes() {
        for (raw, target, mode, cause) in [
            ("1001/aa1w", "1001", AutoAnswerMode::OneWay, None),
            (
                "1001/aa2wb",
                "1001",
                AutoAnswerMode::TwoWay,
                Some(AutoAnswerCause::Busy),
            ),
            (
                "1001/aa=1wu",
                "1001",
                AutoAnswerMode::OneWay,
                Some(AutoAnswerCause::Unavailable),
            ),
            (
                "SEP001122334455/1001/aa=2wc",
                "SEP001122334455/1001",
                AutoAnswerMode::TwoWay,
                Some(AutoAnswerCause::Congestion),
            ),
        ] {
            let request = InboundDialRequest::parse(raw).unwrap();
            assert_eq!(request.target(), target);
            assert_eq!(
                request.auto_answer(),
                Some(AutoAnswerRequest {
                    mode,
                    unavailable_cause: cause,
                })
            );
        }
    }

    #[test]
    fn requestor_aliases_override_mode_but_preserve_address_cause() {
        let mut request = InboundDialRequest::parse("1001/aa1wb").unwrap();
        for alias in ["2way", "2W"] {
            request.apply_requestor_mode(parse_requestor_mode(Some(alias)).unwrap());
            assert_eq!(
                request.auto_answer(),
                Some(AutoAnswerRequest {
                    mode: AutoAnswerMode::TwoWay,
                    unavailable_cause: Some(AutoAnswerCause::Busy),
                })
            );
        }
        assert_eq!(
            parse_requestor_mode(Some("1way")),
            Ok(Some(AutoAnswerMode::OneWay))
        );
        assert_eq!(
            parse_requestor_mode(Some("1W")),
            Ok(Some(AutoAnswerMode::OneWay))
        );
        assert_eq!(parse_requestor_mode(None), Ok(None));
        assert_eq!(parse_requestor_mode(Some("")), Ok(None));
        assert_eq!(
            parse_requestor_mode(Some(" 1w")),
            Err(AutoAnswerParseError::InvalidRequestorMode)
        );
    }

    #[test]
    fn causes_map_to_the_exact_asterisk_request_codes() {
        assert_eq!(AutoAnswerCause::Busy.asterisk_code(), 17);
        assert_eq!(AutoAnswerCause::Unavailable.asterisk_code(), 44);
        assert_eq!(AutoAnswerCause::Congestion.asterisk_code(), 34);
    }

    #[test]
    fn rejects_malformed_duplicate_and_conflicting_options_without_disclosure() {
        for raw in [
            "",
            "device/line/aa",
            "device/line/aa3w",
            "device/line/aa1wx",
            "1001/aa1w/aa2w",
            "device/line/unknown",
            "device/line/aa1w/aa1w",
            " 1001/aa1w",
            "1001 /aa1w",
        ] {
            let error = InboundDialRequest::parse(raw).unwrap_err();
            if !raw.is_empty() {
                assert!(!error.to_string().contains(raw));
            }
        }
        let error = parse_requestor_mode(Some("2way-secret")).unwrap_err();
        assert!(!error.to_string().contains("secret"));
        let debug = format!("{:?}", InboundDialRequest::parse("secret/aa2w").unwrap());
        assert!(!debug.contains("secret"));
        assert_eq!(
            InboundDialRequest::parse("aa100").unwrap().target(),
            "aa100"
        );
        assert_eq!(parse_requestor_mode(Some("")), Ok(None));
    }
}
