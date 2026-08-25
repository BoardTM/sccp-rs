//! supplementary backend-effect translation.

use super::{
    Access, Arc, AsteriskBackend, AsteriskBackendError, CallFeatureError,
    ConferenceDestinationOperation, ConferenceTaskStartError, ForwardingOperation,
    ForwardingRouteReason, LogLevel, MutexExt as _, RedirectReasonCode, SupplementaryBackend,
    VoicemailOperation, ast_log, controller_step, execute_cleanup_effects, native_bridging,
};

impl SupplementaryBackend for AsteriskBackend<'_> {
    fn forward(&self, operation: &ForwardingOperation) -> Result<(), Self::Error> {
        let reason = match operation.reason {
            ForwardingRouteReason::Unconditional => RedirectReasonCode::UNCONDITIONAL,
            ForwardingRouteReason::Busy => RedirectReasonCode::USER_BUSY,
            ForwardingRouteReason::NoAnswer => RedirectReasonCode::NO_ANSWER,
        };
        self.redirect_and_route(
            operation.call_id,
            operation.context.as_str(),
            operation.destination.as_str(),
            reason,
        )
    }

    fn voicemail(&self, operation: &VoicemailOperation) -> Result<(), Self::Error> {
        self.redirect_and_route(
            operation.pbx_call_id,
            operation.target.context(),
            operation.target.destination(),
            RedirectReasonCode::SEND_TO_VOICEMAIL,
        )
    }

    fn start_conference_destination(
        &self,
        operation: &ConferenceDestinationOperation,
    ) -> Result<(), Self::Error> {
        let arguments = if operation.application_options.is_empty() {
            operation.destination.clone()
        } else {
            format!(
                "{},{}",
                operation.destination, operation.application_options
            )
        };
        let (application, cancellation) = self.with_call_feature_channel(
            "start conference destination",
            operation.call_id,
            |channel| {
                native_bridging::prepare_conference_destination(channel, &arguments)
                    .map_err(AsteriskBackendError::CallFeature)
            },
        )?;
        let runtime = self.access.handle.clone();
        let blocking_runtime = runtime.clone();
        let cleanup_runtime = runtime.clone();
        let phone = self.access.phone.clone();
        let shared = Arc::downgrade(&self.access.shared);
        let call_id = operation.call_id;
        let handset_call_id = operation.handset_call_id;
        let held_calls = operation.held_calls.clone();
        let mutation = operation.mutation;
        self.access
            .shared
            .conference_destination_tasks
            .lock_unpoisoned()
            .start(&runtime, call_id, cancellation, move |token| async move {
                let result = blocking_runtime
                    .spawn_blocking(move || application.run())
                    .await;
                let failed = match result {
                    Ok(Ok(())) => false,
                    Ok(Err(error)) => {
                        ast_log(
                            LogLevel::Warning,
                            &format!("conference destination ended with an error: {error}"),
                        );
                        true
                    }
                    Err(error) => {
                        ast_log(
                            LogLevel::Warning,
                            &format!("conference destination task failed: {error}"),
                        );
                        true
                    }
                };
                if let Some(shared) = shared.upgrade() {
                    let completed = shared
                        .conference_destination_tasks
                        .lock_unpoisoned()
                        .complete(token);
                    if completed {
                        if failed {
                            let cleanup = controller_step(&shared.controller, |controller| {
                                controller.conference_destination_failed(
                                    mutation,
                                    handset_call_id,
                                    &held_calls,
                                    &held_calls,
                                )
                            });
                            let access = Access {
                                handle: cleanup_runtime,
                                phone,
                                shared,
                            };
                            execute_cleanup_effects(&access, cleanup).await;
                        } else {
                            controller_step(&shared.controller, |controller| {
                                controller.complete_conference_mutation(mutation)
                            });
                        }
                    }
                }
            })
            .map(|_| ())
            .map_err(|error| match error {
                ConferenceTaskStartError::AlreadyRunning => {
                    AsteriskBackendError::CallFeature(CallFeatureError::Conflict {
                        operation: "start conference destination",
                    })
                }
                ConferenceTaskStartError::ShuttingDown => {
                    AsteriskBackendError::CallFeature(CallFeatureError::Unavailable {
                        operation: "start conference destination",
                    })
                }
                ConferenceTaskStartError::GenerationExhausted => {
                    AsteriskBackendError::CallFeature(CallFeatureError::NativeFailure {
                        operation: "start conference destination",
                    })
                }
            })
    }
}
