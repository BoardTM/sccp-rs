//! Recording service transactions and anchor restoration.

use super::{
    Access, AmiRecordingCommand, AnchoredRecordingSession, Arc, AsteriskBackend, CallId, CallState,
    ConfirmedRecordingAnchor, DeviceId, LogLevel, MediaAnchorMutation, PbxCallId,
    PbxServiceCapabilities as _, PendingRecordingAnchor, PhoneCommand, PhoneCommandAction,
    RecordingCallback, RecordingDirection, RecordingEvent, RecordingProvider as _,
    RecordingRegistryError, RecordingSessionControl as _, RecordingState, RecordingTogglePlan,
    RecordingToggleRejection, RuntimeRecordings, ServiceOutcome, ServiceProviderError, ast_log,
    controller_step, ordered_recording_start, ordered_recording_stop, plan_recording_toggle,
    prepare_anchor_retarget, prepare_direct_retarget, send_confirmed_service,
};

pub fn recording_registry_service_error<E>(
    error: RecordingRegistryError<E>,
) -> ServiceProviderError {
    match error {
        RecordingRegistryError::Exists => ServiceProviderError::RecordingExists,
        RecordingRegistryError::NotFound => ServiceProviderError::RecordingNotFound,
        RecordingRegistryError::Session(_) => ServiceProviderError::RecordingFailed,
    }
}

pub async fn recording_service_operation(
    access: &Access,
    recordings: &mut RuntimeRecordings,
    command: AmiRecordingCommand,
    call_id: PbxCallId,
    filename: Option<String>,
    append: bool,
    bridged_only: bool,
    direction: Option<RecordingDirection>,
) -> Result<ServiceOutcome, ServiceProviderError> {
    let recordings = &mut recordings.sessions;
    let (device_id, handset_call_id) = controller_step(&access.shared.controller, |controller| {
        controller
            .call_by_pbx(call_id)
            .map(|call| (call.device_id.clone(), call.sccp_id))
    })
    .ok_or(ServiceProviderError::CallNotFound)?;
    match command {
        AmiRecordingCommand::Start => {
            if recordings.contains(call_id) {
                return Err(ServiceProviderError::RecordingExists);
            }
            let mut options = String::new();
            if append {
                options.push('a');
            }
            if bridged_only {
                options.push('b');
            }
            let filename = filename.ok_or(ServiceProviderError::RecordingFailed)?;
            let callback_access = access.clone();
            let callback_device = device_id.clone();
            let callback: RecordingCallback = Arc::new(move |event| {
                if event == RecordingEvent::Stopped {
                    callback_access.spawn_phone(PhoneCommand::new(
                        callback_device.clone(),
                        PhoneCommandAction::SetRecordingStatus {
                            call_id: handset_call_id,
                            active: false,
                        },
                    ));
                }
            });
            let mutation = MediaAnchorMutation::acquire(access).await;
            let pending = PendingRecordingAnchor::acquire(access, call_id, &mutation)
                .map_err(|_| ServiceProviderError::RecordingFailed)?;
            let mutation_ref = &mutation;
            let (inner, anchor) = ordered_recording_start(
                confirm_recording_anchor(access, pending, mutation_ref),
                || {
                    AsteriskBackend::new(access)
                        .recordings()
                        .start_recording(call_id, &filename, &options, callback)
                        .map_err(|_| ServiceProviderError::RecordingFailed)
                },
                |mut anchor| async move {
                    let result = restore_recording_anchor(access, &mut anchor, mutation_ref).await;
                    if result.is_err() {
                        ast_log(
                            LogLevel::Warning,
                            &format!(
                                "unable to restore direct media after recording start failed for PBX call {}",
                                call_id.0
                            ),
                        );
                    }
                },
            )
            .await?;
            let session = AnchoredRecordingSession::new(inner, anchor);
            if let Err((error, mut session)) = recordings.insert_owned(call_id, session) {
                let _ = session.stop_native();
                let _ = restore_recording_session(access, session, &mutation).await;
                return Err(recording_registry_service_error(error));
            }
            drop(mutation);
            if let Err(error) = send_confirmed_service(
                access,
                PhoneCommand::new(
                    device_id,
                    PhoneCommandAction::SetRecordingStatus {
                        call_id: handset_call_id,
                        active: true,
                    },
                ),
            )
            .await
            {
                if let Ok(session) = recordings.take(call_id) {
                    let mutation = MediaAnchorMutation::acquire(access).await;
                    if let Err((_, session)) =
                        stop_and_restore_recording(access, session, &mutation).await
                    {
                        let _ = recordings.insert(call_id, session);
                    }
                }
                return Err(error);
            }
            Ok(ServiceOutcome::Recording {
                command,
                call_id,
                active: true,
                muted: false,
                affected: 0,
            })
        }
        AmiRecordingCommand::Stop => {
            let session = recordings
                .take(call_id)
                .map_err(recording_registry_service_error)?;
            if let Err(error) = send_confirmed_service(
                access,
                PhoneCommand::new(
                    device_id.clone(),
                    PhoneCommandAction::SetRecordingStatus {
                        call_id: handset_call_id,
                        active: false,
                    },
                ),
            )
            .await
            {
                let _ = recordings.insert(call_id, session);
                return Err(error);
            }
            let mutation = MediaAnchorMutation::acquire(access).await;
            if let Err((error, session)) =
                stop_and_restore_recording(access, session, &mutation).await
            {
                let recording_still_active =
                    !matches!(session.state(), Ok(RecordingState::Stopped));
                let _ = recordings.insert(call_id, session);
                if recording_still_active {
                    let _ = send_confirmed_service(
                        access,
                        PhoneCommand::new(
                            device_id,
                            PhoneCommandAction::SetRecordingStatus {
                                call_id: handset_call_id,
                                active: true,
                            },
                        ),
                    )
                    .await;
                }
                return Err(error);
            }
            Ok(ServiceOutcome::Recording {
                command,
                call_id,
                active: false,
                muted: false,
                affected: 0,
            })
        }
        AmiRecordingCommand::Mute | AmiRecordingCommand::Unmute => {
            let muted = command == AmiRecordingCommand::Mute;
            let affected = recordings
                .set_muted(
                    call_id,
                    direction.ok_or(ServiceProviderError::RecordingFailed)?,
                    muted,
                )
                .map_err(recording_registry_service_error)?;
            Ok(ServiceOutcome::Recording {
                command,
                call_id,
                active: true,
                muted,
                affected,
            })
        }
    }
}

async fn confirm_recording_anchor(
    access: &Access,
    pending: PendingRecordingAnchor,
    _mutation: &MediaAnchorMutation<'_>,
) -> Result<ConfirmedRecordingAnchor, ServiceProviderError> {
    let Some(call) = pending.direct_call() else {
        return Ok(pending.confirm());
    };
    let retarget =
        prepare_anchor_retarget(access, call).ok_or(ServiceProviderError::RecordingFailed)?;
    if let Err(error) = send_confirmed_service(access, retarget.command()).await {
        retarget.rollback(access);
        return Err(error);
    }
    retarget.confirm();
    Ok(pending.confirm())
}

async fn restore_recording_anchor(
    access: &Access,
    anchor: &mut ConfirmedRecordingAnchor,
    _mutation: &MediaAnchorMutation<'_>,
) -> Result<(), ServiceProviderError> {
    if let Some(call) = anchor.restore_call() {
        let retarget =
            prepare_direct_retarget(access, &call).ok_or(ServiceProviderError::RecordingFailed)?;
        if let Err(error) = send_confirmed_service(access, retarget.command()).await {
            retarget.rollback(access);
            return Err(error);
        }
        retarget.confirm();
    }
    anchor.release();
    Ok(())
}

pub(super) async fn restore_recording_session(
    access: &Access,
    mut session: AnchoredRecordingSession,
    mutation: &MediaAnchorMutation<'_>,
) -> Result<(), (ServiceProviderError, AnchoredRecordingSession)> {
    if let Err(error) = restore_recording_anchor(access, session.anchor_mut(), mutation).await {
        return Err((error, session));
    }
    Ok(())
}

async fn stop_and_restore_recording(
    access: &Access,
    session: AnchoredRecordingSession,
    mutation: &MediaAnchorMutation<'_>,
) -> Result<(), (ServiceProviderError, AnchoredRecordingSession)> {
    ordered_recording_stop(
        session,
        |session| {
            session
                .stop_native()
                .map_err(|_| ServiceProviderError::RecordingFailed)
        },
        |session| restore_recording_session(access, session, mutation),
    )
    .await
}

pub async fn toggle_monitor_recording(
    access: &Access,
    recordings: &mut RuntimeRecordings,
    device_id: &DeviceId,
    handset_call_id: CallId,
) -> Result<ServiceOutcome, ServiceProviderError> {
    let call = controller_step(&access.shared.controller, |controller| {
        controller.call(handset_call_id)
    })
    .ok_or(ServiceProviderError::CallNotFound)?;
    let active = recordings.sessions.contains(call.pbx_id);
    let plan = plan_recording_toggle(
        device_id,
        &call.device_id,
        matches!(call.state, CallState::Connected | CallState::Barged),
        active,
    )
    .map_err(|error| match error {
        RecordingToggleRejection::Ownership => ServiceProviderError::CallOwnership,
        RecordingToggleRejection::CallState => ServiceProviderError::CallState,
    })?;
    let (command, filename) = match plan {
        RecordingTogglePlan::Start => (
            AmiRecordingCommand::Start,
            Some(format!("sccp-monitor-{}.wav", call.pbx_id.0)),
        ),
        RecordingTogglePlan::Stop => (AmiRecordingCommand::Stop, None),
    };
    recording_service_operation(
        access,
        recordings,
        command,
        call.pbx_id,
        filename,
        false,
        false,
        None,
    )
    .await
}
