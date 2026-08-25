//! Ordered effect-execution failures, separate from effect models and ports.

use std::error::Error;
use std::fmt;
use std::future::Future;

use super::{DriverEffect, HandsetEffect, PbxBackend, PbxEffect};

#[derive(Debug)]
pub enum EffectExecutionError<BackendError, HandsetError> {
    Backend {
        index: usize,
        effect: Box<PbxEffect>,
        error: BackendError,
    },
    Handset {
        index: usize,
        effect: Box<HandsetEffect>,
        error: HandsetError,
    },
}

impl<BackendError: fmt::Display, HandsetError: fmt::Display> fmt::Display
    for EffectExecutionError<BackendError, HandsetError>
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Backend {
                index,
                effect,
                error,
            } => write!(
                formatter,
                "backend effect {index} ({effect:?}) failed: {error}"
            ),
            Self::Handset {
                index,
                effect,
                error,
            } => write!(
                formatter,
                "handset effect {index} ({effect:?}) failed: {error}"
            ),
        }
    }
}

impl<BackendError, HandsetError> Error for EffectExecutionError<BackendError, HandsetError>
where
    BackendError: Error + 'static,
    HandsetError: Error + 'static,
{
}

pub(super) async fn execute_effects<Backend, SendHandset, SendFuture, HandsetError>(
    backend: &Backend,
    effects: Vec<DriverEffect>,
    mut send_handset: SendHandset,
) -> Result<(), EffectExecutionError<Backend::Error, HandsetError>>
where
    Backend: PbxBackend,
    SendHandset: FnMut(HandsetEffect) -> SendFuture,
    SendFuture: Future<Output = Result<(), HandsetError>>,
{
    for (index, effect) in effects.into_iter().enumerate() {
        match effect {
            DriverEffect::Backend(effect) => {
                let followup =
                    backend
                        .execute(&effect)
                        .map_err(|error| EffectExecutionError::Backend {
                            index,
                            effect: Box::new(effect.clone()),
                            error,
                        })?;
                if let Some(effect) = followup {
                    send_handset(effect.clone()).await.map_err(|error| {
                        EffectExecutionError::Handset {
                            index,
                            effect: Box::new(effect),
                            error,
                        }
                    })?;
                }
            }
            DriverEffect::Handset(effect) => {
                send_handset(effect.clone()).await.map_err(|error| {
                    EffectExecutionError::Handset {
                        index,
                        effect: Box::new(effect),
                        error,
                    }
                })?;
            }
        }
    }
    Ok(())
}

pub(super) async fn execute_cleanup_effects<Backend, SendHandset, SendFuture, HandsetError>(
    backend: &Backend,
    effects: Vec<DriverEffect>,
    mut send_handset: SendHandset,
) -> Vec<EffectExecutionError<Backend::Error, HandsetError>>
where
    Backend: PbxBackend,
    SendHandset: FnMut(HandsetEffect) -> SendFuture,
    SendFuture: Future<Output = Result<(), HandsetError>>,
{
    let mut errors = Vec::new();
    for (index, effect) in effects.into_iter().enumerate() {
        match effect {
            DriverEffect::Backend(effect) => match backend.execute(&effect) {
                Ok(Some(followup)) => {
                    if let Err(error) = send_handset(followup.clone()).await {
                        errors.push(EffectExecutionError::Handset {
                            index,
                            effect: Box::new(followup),
                            error,
                        });
                    }
                }
                Ok(None) => {}
                Err(error) => errors.push(EffectExecutionError::Backend {
                    index,
                    effect: Box::new(effect),
                    error,
                }),
            },
            DriverEffect::Handset(effect) => {
                if let Err(error) = send_handset(effect.clone()).await {
                    errors.push(EffectExecutionError::Handset {
                        index,
                        effect: Box::new(effect),
                        error,
                    });
                }
            }
        }
    }
    errors
}
