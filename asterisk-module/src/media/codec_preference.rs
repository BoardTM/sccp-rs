//! Typed per-channel audio preference changes for the dialplan.
//!
//! `SCCPSetCodec(codec[,codec...])` replaces the current module-owned channel's
//! ordered audio preferences. Accepted names normalize to an Asterisk audio
//! format represented by the SCCP adapter; duplicates collapse in
//! first-occurrence order. Every
//! requested format must be representable by Asterisk and present in the
//! normalized configured pre-dial policy. Operation words such as `read`,
//! `replace`, `append`, `remove`, and `reset` are deliberately unsupported.

use thiserror::Error;

use crate::media::formats::PbxAudioFormat;
use crate::pbx::dialplan::{
    DialplanApplicationResult, DialplanBackend, DialplanCallbackError, DialplanError,
    DialplanLimits,
};
use crate::pbx::party::AsteriskChannel;

pub const CODEC_PREFERENCE_APPLICATION: &str = "SCCPSetCodec";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CodecPreferenceOperation {
    Replace(Vec<PbxAudioFormat>),
}

impl CodecPreferenceOperation {
    pub fn parse(arguments: &str) -> Result<Self, CodecPreferenceError> {
        let parts = arguments.split(',').map(str::trim).collect::<Vec<_>>();
        if parts.is_empty() || parts.iter().any(|part| part.is_empty()) {
            return Err(CodecPreferenceError::InvalidArguments);
        }
        if matches!(
            parts[0].to_ascii_lowercase().as_str(),
            "read" | "replace" | "append" | "remove" | "reset"
        ) {
            return Err(CodecPreferenceError::UnsupportedOperation);
        }
        Ok(Self::Replace(parse_formats(&parts)?))
    }
}

fn parse_formats(values: &[&str]) -> Result<Vec<PbxAudioFormat>, CodecPreferenceError> {
    let mut formats = Vec::new();
    for value in values {
        if value.len() > 32
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(CodecPreferenceError::UnsupportedFormat);
        }
        let normalized = value
            .chars()
            .filter(|character| character.is_ascii_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect::<String>();
        let format = match normalized.as_str() {
            "ulaw" | "pcmu" | "g711ulaw" => PbxAudioFormat::G711Ulaw,
            "alaw" | "pcma" | "g711alaw" => PbxAudioFormat::G711Alaw,
            "g722" => PbxAudioFormat::G722,
            "g723" | "g7231" => PbxAudioFormat::G723,
            "g729" | "g729a" | "g729b" | "g729ab" => PbxAudioFormat::G729,
            "g726" | "g726aal2" | "g72632" => PbxAudioFormat::G726Aal2,
            "gsm" => PbxAudioFormat::Gsm,
            "slin16" | "wideband256k" => PbxAudioFormat::Slin16,
            "ilbc" => PbxAudioFormat::Ilbc,
            "siren7" | "g722132" => PbxAudioFormat::Siren7,
            "opus" => PbxAudioFormat::Opus,
            _ => return Err(CodecPreferenceError::UnsupportedFormat),
        };
        if !formats.contains(&format) {
            formats.push(format);
        }
    }
    if formats.is_empty() {
        return Err(CodecPreferenceError::InvalidArguments);
    }
    Ok(formats)
}

pub const fn audio_format_name(format: PbxAudioFormat) -> &'static str {
    match format {
        PbxAudioFormat::G711Ulaw => "ulaw",
        PbxAudioFormat::G711Alaw => "alaw",
        PbxAudioFormat::G722 => "g722",
        PbxAudioFormat::G723 => "g723",
        PbxAudioFormat::G729 => "g729",
        PbxAudioFormat::G726Aal2 => "g726aal2",
        PbxAudioFormat::Gsm => "gsm",
        PbxAudioFormat::Slin16 => "slin16",
        PbxAudioFormat::Ilbc => "ilbc",
        PbxAudioFormat::Siren7 => "siren7",
        PbxAudioFormat::Opus => "opus",
    }
}

pub fn render_audio_preferences(preferences: &[PbxAudioFormat]) -> String {
    preferences
        .iter()
        .map(|format| audio_format_name(*format))
        .collect::<Vec<_>>()
        .join(",")
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodecPreferenceContext {
    /// Normalized configured formats supported by both the channel backend and
    /// the registered station, in configured preference order.
    pub configured: Vec<PbxAudioFormat>,
    /// Current per-channel preferences, or the configured list when no
    /// override is active.
    pub effective: Vec<PbxAudioFormat>,
}

pub trait CodecPreferenceProvider: Send + Sync + 'static {
    fn context(
        &self,
        channel: &AsteriskChannel<'_>,
    ) -> Result<CodecPreferenceContext, CodecPreferenceProviderError>;

    fn replace(
        &self,
        channel: &AsteriskChannel<'_>,
        preferences: &[PbxAudioFormat],
    ) -> Result<(), CodecPreferenceProviderError>;
}

pub struct CodecPreferenceApplication<P> {
    provider: P,
}

impl<P: CodecPreferenceProvider> CodecPreferenceApplication<P> {
    pub const fn new(provider: P) -> Self {
        Self { provider }
    }

    pub fn preferences(
        &self,
        channel: &AsteriskChannel<'_>,
    ) -> Result<Vec<PbxAudioFormat>, CodecPreferenceError> {
        let context = self
            .provider
            .context(channel)
            .map_err(CodecPreferenceError::Provider)?;
        normalized_context(context)
    }

    pub fn execute(
        &self,
        arguments: &str,
        channel: &AsteriskChannel<'_>,
    ) -> Result<(), CodecPreferenceError> {
        let CodecPreferenceOperation::Replace(requested) =
            CodecPreferenceOperation::parse(arguments)?;
        let context = self
            .provider
            .context(channel)
            .map_err(CodecPreferenceError::Provider)?;
        let configured = normalize(context.configured);
        if configured.is_empty() {
            return Err(CodecPreferenceError::NoConfiguredFormat);
        }
        ensure_configured(&configured, &requested)?;
        self.provider
            .replace(channel, &requested)
            .map_err(CodecPreferenceError::Provider)
    }
}

fn normalized_context(
    context: CodecPreferenceContext,
) -> Result<Vec<PbxAudioFormat>, CodecPreferenceError> {
    let configured = normalize(context.configured);
    if configured.is_empty() {
        return Err(CodecPreferenceError::NoConfiguredFormat);
    }
    let effective = normalize(context.effective);
    Ok(if effective.is_empty() {
        configured
    } else {
        effective
    })
}

fn normalize(preferences: Vec<PbxAudioFormat>) -> Vec<PbxAudioFormat> {
    let mut normalized = Vec::new();
    for format in preferences {
        if !normalized.contains(&format) {
            normalized.push(format);
        }
    }
    normalized
}

fn ensure_configured(
    configured: &[PbxAudioFormat],
    requested: &[PbxAudioFormat],
) -> Result<(), CodecPreferenceError> {
    requested
        .iter()
        .all(|format| configured.contains(format))
        .then_some(())
        .ok_or(CodecPreferenceError::FormatNotConfigured)
}

pub fn register_codec_preference_application<P: CodecPreferenceProvider, B: DialplanBackend>(
    provider: P,
    backend: B,
) -> Result<B::Registration, DialplanError> {
    let application = CodecPreferenceApplication::new(provider);
    backend.register_application(
        CODEC_PREFERENCE_APPLICATION,
        "Set channel codec preference",
        "Replace representable pre-dial audio preferences",
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
pub enum CodecPreferenceProviderError {
    #[error("the callback channel is not owned by this driver")]
    NotDriverChannel,
    #[error("the channel is no longer in a pre-dial state")]
    NotPreDial,
    #[error("the channel or registered station is unavailable")]
    Unavailable,
    #[error("the channel does not have one unambiguous handset appearance")]
    AmbiguousChannel,
    #[error("the requested format is no longer available under current media policy")]
    FormatUnavailable,
    #[error("the native channel rejected the audio format")]
    NativeRejected,
    #[error("the native update failed and the controller codec could not be restored")]
    RollbackFailed,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum CodecPreferenceError {
    #[error("codec preference expects a codec list or a supported operation and codec list")]
    InvalidArguments,
    #[error("the requested audio format is not representable by this channel backend")]
    UnsupportedFormat,
    #[error("the requested audio format is outside the configured channel policy")]
    FormatNotConfigured,
    #[error("the channel has no configured representable audio format")]
    NoConfiguredFormat,
    #[error("this codec-preference operation is not supported by the dialplan application")]
    UnsupportedOperation,
    #[error(transparent)]
    Provider(#[from] CodecPreferenceProviderError),
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    struct FakeNative {
        context: CodecPreferenceContext,
        replacements: Mutex<Vec<(usize, Vec<PbxAudioFormat>)>>,
        failure: Option<CodecPreferenceProviderError>,
    }

    impl CodecPreferenceProvider for FakeNative {
        fn context(
            &self,
            _channel: &AsteriskChannel<'_>,
        ) -> Result<CodecPreferenceContext, CodecPreferenceProviderError> {
            if let Some(error) = self.failure {
                return Err(error);
            }
            Ok(self.context.clone())
        }

        fn replace(
            &self,
            channel: &AsteriskChannel<'_>,
            preferences: &[PbxAudioFormat],
        ) -> Result<(), CodecPreferenceProviderError> {
            if let Some(error) = self.failure {
                return Err(error);
            }
            self.replacements
                .lock()
                .unwrap()
                .push((channel.as_raw() as usize, preferences.to_vec()));
            Ok(())
        }
    }

    fn channel() -> AsteriskChannel<'static> {
        let pointer = Box::leak(Box::new(1_u8));
        unsafe { AsteriskChannel::from_raw(std::ptr::from_mut(pointer).cast()).unwrap() }
    }

    fn fake(
        configured: Vec<PbxAudioFormat>,
        effective: Vec<PbxAudioFormat>,
    ) -> CodecPreferenceApplication<FakeNative> {
        CodecPreferenceApplication::new(FakeNative {
            context: CodecPreferenceContext {
                configured,
                effective,
            },
            replacements: Mutex::new(Vec::new()),
            failure: None,
        })
    }

    fn all() -> Vec<PbxAudioFormat> {
        PbxAudioFormat::ALL.to_vec()
    }

    #[test]
    fn parser_preserves_documented_bare_replacement_and_codec_order() {
        assert_eq!(
            CodecPreferenceOperation::parse("alaw").unwrap(),
            CodecPreferenceOperation::Replace(vec![PbxAudioFormat::G711Alaw])
        );
        assert_eq!(
            CodecPreferenceOperation::parse("pcmu,g722").unwrap(),
            CodecPreferenceOperation::Replace(
                vec![PbxAudioFormat::G711Ulaw, PbxAudioFormat::G722,]
            )
        );
    }

    #[test]
    fn unevidenced_mutation_modes_are_rejected_explicitly() {
        for arguments in [
            "read",
            "replace,ulaw",
            "append,ulaw",
            "remove,ulaw",
            "reset",
        ] {
            assert_eq!(
                CodecPreferenceOperation::parse(arguments),
                Err(CodecPreferenceError::UnsupportedOperation)
            );
        }
    }

    #[test]
    fn malformed_unsupported_and_secret_like_values_fail_without_echoing_input() {
        for arguments in [
            "",
            "replace",
            "append",
            "remove",
            "replace,,ulaw",
            "isac",
            "password",
            "private-key",
            "token",
            "u/law",
            "g722\0hidden",
        ] {
            let error = CodecPreferenceOperation::parse(arguments).unwrap_err();
            if !arguments.is_empty() {
                assert!(!error.to_string().contains(arguments));
            }
        }
    }

    #[test]
    fn replacement_is_normalized_and_restricted_to_configured_formats() {
        let channel = channel();
        let application = fake(all(), all());
        application.execute("g722,ulaw,g722", &channel).unwrap();
        assert_eq!(
            application.provider.replacements.into_inner().unwrap(),
            [(
                channel.as_raw() as usize,
                vec![PbxAudioFormat::G722, PbxAudioFormat::G711Ulaw]
            )]
        );

        assert_eq!(
            fake(vec![PbxAudioFormat::G711Ulaw], vec![]).execute("alaw", &channel),
            Err(CodecPreferenceError::FormatNotConfigured)
        );
    }

    #[test]
    fn typed_read_uses_the_normalized_effective_preferences() {
        let channel = channel();
        let application = fake(
            vec![
                PbxAudioFormat::G722,
                PbxAudioFormat::G711Ulaw,
                PbxAudioFormat::G722,
            ],
            vec![PbxAudioFormat::G711Alaw],
        );
        assert_eq!(
            application.preferences(&channel).unwrap(),
            [PbxAudioFormat::G711Alaw]
        );
    }

    #[test]
    fn fake_native_preserves_exact_channel_format_and_failure() {
        let channel = channel();
        let application = fake(all(), all());
        application.execute("g722", &channel).unwrap();
        assert_eq!(
            application.provider.replacements.into_inner().unwrap(),
            [(channel.as_raw() as usize, vec![PbxAudioFormat::G722])]
        );

        let failed = CodecPreferenceApplication::new(FakeNative {
            context: CodecPreferenceContext {
                configured: all(),
                effective: all(),
            },
            replacements: Mutex::new(Vec::new()),
            failure: Some(CodecPreferenceProviderError::NativeRejected),
        });
        assert_eq!(
            failed.execute("ulaw", &channel),
            Err(CodecPreferenceError::Provider(
                CodecPreferenceProviderError::NativeRejected
            ))
        );
    }

    #[test]
    fn rendering_uses_canonical_asterisk_audio_names() {
        assert_eq!(
            render_audio_preferences(&all()),
            "ulaw,alaw,g722,g723,g729,g726aal2,gsm,slin16,ilbc,siren7,opus"
        );
    }

    #[cfg(feature = "development")]
    #[test]
    fn registration_is_unavailable_without_native_linkage() {
        let result = register_codec_preference_application(
            FakeNative {
                context: CodecPreferenceContext {
                    configured: all(),
                    effective: all(),
                },
                replacements: Mutex::new(Vec::new()),
                failure: None,
            },
            crate::pbx::dialplan::UnavailableDialplan,
        );
        assert!(matches!(result, Err(DialplanError::Unavailable)));
    }
}
