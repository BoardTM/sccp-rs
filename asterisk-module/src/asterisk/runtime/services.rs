use super::super::*;
use super::backend::{
    AnchoredRecordingSession, ConfirmedRecordingAnchor, MediaAnchorMutation, PendingRecordingAnchor,
};
use super::media::{prepare_anchor_retarget, prepare_direct_retarget};

pub async fn run_events(
    access: Access,
    mut events: mpsc::Receiver<PhoneEvent>,
    mut blf_events: mpsc::UnboundedReceiver<BlfEvent>,
    mut parking_events: mpsc::UnboundedReceiver<ParkingEvent>,
    mut control_requests: mpsc::UnboundedReceiver<RuntimeControlRequest>,
    mut service_requests: mpsc::UnboundedReceiver<RuntimeServiceRequest>,
) {
    let mut recording_sessions = RuntimeRecordings::default();
    let mut deadlines = tokio::time::interval(Duration::from_millis(100));
    deadlines.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            event = events.recv() => {
                let Some(event) = event else { return; };
                handle_phone_event(&access, &mut recording_sessions, event).await;
                prune_recording_sessions(&access, &mut recording_sessions).await;
            }
            event = blf_events.recv() => {
                let Some(event) = event else { return; };
                handle_blf_event(&access, event);
            }
            event = parking_events.recv() => {
                let Some(event) = event else { return; };
                handle_parking_event(&access, event).await;
            }
            request = control_requests.recv() => {
                let Some(request) = request else { return; };
                let result = handle_control_operation(&access, request.operation).await;
                let _ = request.response.send(result);
            }
            request = service_requests.recv() => {
                let Some(request) = request else { return; };
                let result = handle_service_operation(
                    &access,
                    &mut recording_sessions,
                    request.operation,
                ).await;
                let _ = request.response.send(result);
            }
            _ = deadlines.tick() => {
                let (actions, auto_answers) = controller_step(&access.shared.controller, |controller| {
                    let now = Instant::now();
                    let mut effects = controller.expire_digits(now);
                    effects.extend(controller.expire_call_waiting_tones(now));
                    (effects, controller.expire_auto_answers(now))
                });
                execute_effects(&access, actions).await;
                for transition in auto_answers {
                    execute_answer_call_transition(&access, transition).await;
                }
                let remote_hangups = controller_step(&access.shared.controller, |controller| {
                    controller.expire_remote_hangups(Instant::now())
                });
                execute_cleanup_effects(&access, remote_hangups).await;
                expire_forwarding_entries(&access, Instant::now()).await;
                expire_no_answer_routes(&access, Instant::now()).await;
                expire_parking_attempts(&access, Instant::now()).await;
                prune_recording_sessions(&access, &mut recording_sessions).await;
            }
        }
    }
}

pub async fn run_call_signals(
    access: Access,
    mut signals: mpsc::UnboundedReceiver<RuntimeCallSignal>,
) {
    let mut last_sequence = 0;
    let mut lanes = HashMap::<PbxCallId, mpsc::UnboundedSender<RuntimeCallSignal>>::new();
    while let Some(signal) = signals.recv().await {
        if signal.sequence <= last_sequence {
            ast_log(
                LogLevel::Error,
                "discarding an out-of-order SCCP call signal",
            );
            continue;
        }
        last_sequence = signal.sequence;
        lanes.retain(|_, sender| !sender.is_closed());
        let pbx_id = signal.pbx_id;
        let sender = lanes.entry(pbx_id).or_insert_with(|| {
            let (sender, mut receiver) = mpsc::unbounded_channel::<RuntimeCallSignal>();
            let lane_access = access.clone();
            access.handle.spawn(async move {
                while let Some(signal) = receiver.recv().await {
                    let terminal = matches!(signal.kind, RuntimeCallSignalKind::Hangup { .. });
                    handle_runtime_call_signal(&lane_access, signal).await;
                    if terminal {
                        break;
                    }
                }
            });
            sender
        });
        if sender.send(signal).is_err() {
            lanes.remove(&pbx_id);
        }
    }
}

pub async fn handle_runtime_call_signal(access: &Access, signal: RuntimeCallSignal) {
    let line = controller_step(&access.shared.controller, |controller| {
        controller
            .call_by_pbx(signal.pbx_id)
            .map(|call| call.line.clone())
    });
    match signal.kind {
        RuntimeCallSignalKind::StopTone => {
            let effects = controller_step(&access.shared.controller, |controller| {
                controller
                    .call_by_pbx(signal.pbx_id)
                    .map(|call| {
                        HandsetEffect::StartTone {
                            device_id: call.device_id,
                            call_id: call.sccp_id,
                            tone: Tone::Silence,
                        }
                        .into()
                    })
                    .into_iter()
                    .collect()
            });
            execute_effects(access, effects).await;
        }
        RuntimeCallSignalKind::Answer { completion } => {
            let actions = controller_step(&access.shared.controller, |controller| {
                controller.pbx_answer(signal.pbx_id)
            });
            if !actions.is_empty() {
                cancel_no_answer_timer(access, signal.pbx_id);
            }
            let delivered = execute_effects_confirmed(access, actions).await;
            let _ = completion.send(delivered);
        }
        RuntimeCallSignalKind::Hangup { handset_call_id } => {
            handle_runtime_hangup_signal(access, signal.pbx_id, handset_call_id).await;
        }
        RuntimeCallSignalKind::Proceeding => {
            let actions = controller_step(&access.shared.controller, |controller| {
                controller.pbx_proceeding(signal.pbx_id)
            });
            execute_effects(access, actions).await;
        }
        RuntimeCallSignalKind::Ringing => {
            let actions = controller_step(&access.shared.controller, |controller| {
                controller.pbx_ringing(signal.pbx_id)
            });
            execute_effects(access, actions).await;
        }
        RuntimeCallSignalKind::Progress => {
            let Some(call) = controller_step(&access.shared.controller, |controller| {
                controller.call_by_pbx(signal.pbx_id)
            }) else {
                return;
            };
            let early_media = configured_early_media(access, &call.device_id, call.sccp_id);
            let media_mode = outbound_media_mode(access, &call.device_id);
            let actions = controller_step(&access.shared.controller, |controller| {
                controller.pbx_progress_with_media_mode(signal.pbx_id, early_media, media_mode)
            });
            execute_effects(access, actions).await;
        }
        RuntimeCallSignalKind::Busy | RuntimeCallSignalKind::Congestion => {
            let Some(call) = controller_step(&access.shared.controller, |controller| {
                controller
                    .call_by_pbx(signal.pbx_id)
                    .filter(|call| call.state == CallState::Calling)
            }) else {
                return;
            };
            let state = if matches!(signal.kind, RuntimeCallSignalKind::Busy) {
                PhoneCallState::Busy
            } else {
                PhoneCallState::Congestion
            };
            if let Err(error) =
                send_handset_call_state(access, call.device_id, call.sccp_id, state).await
            {
                ast_log(
                    LogLevel::Warning,
                    &format!("unable to publish terminal handset state: {error}"),
                );
            }
        }
        RuntimeCallSignalKind::Hold | RuntimeCallSignalKind::Unhold => {
            let Some(call_id) = controller_step(&access.shared.controller, |controller| {
                controller.active_call_id(signal.pbx_id).or_else(|| {
                    controller
                        .call_by_pbx(signal.pbx_id)
                        .map(|call| call.sccp_id)
                })
            }) else {
                return;
            };
            let hold = matches!(signal.kind, RuntimeCallSignalKind::Hold);
            handle_hold_or_resume(access, call_id, hold, true).await;
        }
        RuntimeCallSignalKind::VideoUpdate => {
            let actions = controller_step(&access.shared.controller, |controller| {
                controller.refresh_video_for_pbx(signal.pbx_id)
            });
            execute_effects(access, actions).await;
        }
        RuntimeCallSignalKind::PartyUpdate(snapshot) => {
            let actions = controller_step(&access.shared.controller, |controller| {
                let mut effects = controller.update_call_info_by_pbx(signal.pbx_id, |current| {
                    snapshot.apply_to_call_info(current)
                });
                effects.extend(controller.pbx_remote_identity_ready(signal.pbx_id));
                effects
            });
            execute_effects(access, actions).await;
        }
    }
    if let Some(line) = line {
        publish_line(access, &line);
    }
}

pub async fn handle_runtime_hangup_signal(
    access: &Access,
    pbx_id: PbxCallId,
    handset_call_id: CallId,
) {
    if let Some(pending) = take_pending_retrieval_by_pbx(access, pbx_id) {
        access
            .shared
            .parking_registry
            .lock_unpoisoned()
            .release_claim(&pending.lot, pending.slot, handset_call_id);
    }
    let remote_hangup_tone = access.config().general.remote_hangup_tone;
    let (conference_id, plan, surviving_conference) =
        controller_step(&access.shared.controller, |controller| {
            let conference_id = controller
                .conference_session_by_pbx(pbx_id)
                .map(|session| session.id);
            let plan = controller.begin_remote_hangup(
                pbx_id,
                remote_hangup_tone,
                REMOTE_HANGUP_PRESENTATION_TIME,
                Instant::now(),
            );
            let surviving = conference_id
                .and_then(|conference_id| controller.conference_session_by_id(conference_id))
                .cloned();
            (conference_id, plan, surviving)
        });
    remove_channel(access, pbx_id);
    if let Some(plan) = plan {
        if let Some(call) = plan.outcome.primary.as_ref() {
            publish_line(access, &call.line);
        }
        if let Some(session) = surviving_conference {
            execute_cleanup_effects(access, plan.outcome.effects).await;
            let show_list = access
                .config()
                .conference_for_device(&session.device_id)
                .is_some_and(|conference| conference.show_conference_list);
            if show_list {
                show_conference_list(access, session.device_id, session.original_handset_call_id)
                    .await;
            }
        } else if let Some(conference_id) = conference_id {
            execute_cleanup_effects(access, plan.outcome.effects).await;
            cancel_conference_announcement(access, conference_id);
        } else if plan.pending.is_some() {
            execute_remote_hangup_plan(access, plan).await;
        } else {
            execute_effects(access, plan.outcome.effects).await;
        }
    } else if let Some(conference_id) = conference_id {
        cancel_conference_announcement(access, conference_id);
    }
}

pub async fn prune_recording_sessions(access: &Access, recordings: &mut RuntimeRecordings) {
    let recordings = &mut recordings.sessions;
    let live = controller_step(&access.shared.controller, |controller| {
        controller
            .calls()
            .map(|call| call.pbx_id)
            .collect::<HashSet<_>>()
    });
    let finished = recordings.extract_if(|call_id, session| {
        !live.contains(&call_id) || matches!(session.state(), Ok(RecordingState::Stopped))
    });
    for (call_id, mut session) in finished {
        if !live.contains(&call_id) {
            let _ = session.stop_native();
            session.release_anchor();
            continue;
        }
        let mutation = MediaAnchorMutation::acquire(access).await;
        if let Err((_, session)) = restore_recording_session(access, session, &mutation).await {
            let _ = recordings.insert(call_id, session);
        }
    }
}

pub async fn handle_control_operation(
    access: &Access,
    operation: ControlOperation,
) -> Result<ControlOutcome, ControlProviderError> {
    match operation {
        ControlOperation::Message {
            target,
            text,
            beep,
            timeout_seconds,
        } => {
            let devices = match &target {
                MessageTarget::Device(device_id) => {
                    if !access.config().devices.contains_key(device_id) {
                        return Err(ControlProviderError::DeviceNotFound);
                    }
                    let registered = controller_step(&access.shared.controller, |controller| {
                        controller.is_registered(device_id)
                    });
                    if !registered {
                        return Err(ControlProviderError::DeviceNotRegistered);
                    }
                    vec![device_id.clone()]
                }
                MessageTarget::RegisteredDevices | MessageTarget::System => {
                    let mut devices = registered_device_ids(&access.shared);
                    devices.sort();
                    devices
                }
            };
            let persistent = target == MessageTarget::System;
            if persistent {
                let expires_at = (timeout_seconds != 0)
                    .then(|| Instant::now() + Duration::from_secs(u64::from(timeout_seconds)));
                *access
                    .shared
                    .system_message
                    .lock()
                    .map_err(|_| ControlProviderError::Unavailable)? = Some(ActiveSystemMessage {
                    text: text.clone(),
                    beep,
                    expires_at,
                });
            }
            let attempted = devices.len();
            let mut delivered = 0;
            let deliveries = async {
                for device_id in devices {
                    if deliver_status_message(
                        access,
                        device_id,
                        text.clone(),
                        beep,
                        timeout_seconds,
                    )
                    .await
                    .is_ok()
                    {
                        delivered += 1;
                    }
                }
            };
            let _ = tokio::time::timeout(MANAGER_CONTROL_DELIVERY_TIMEOUT, deliveries).await;
            if matches!(target, MessageTarget::Device(_)) && delivered == 0 {
                return Err(ControlProviderError::HandsetDelivery);
            }
            Ok(ControlOutcome::Message {
                target,
                attempted,
                delivered,
                persistent,
            })
        }
        ControlOperation::Reset { device_id, mode } => {
            if !access.config().devices.contains_key(&device_id) {
                return Err(ControlProviderError::DeviceNotFound);
            }
            let registered = controller_step(&access.shared.controller, |controller| {
                controller.is_registered(&device_id)
            });
            if !registered {
                return Err(ControlProviderError::DeviceNotRegistered);
            }
            let reset_type = match mode {
                ResetMode::Reset => ResetType::Reset,
                ResetMode::Restart => ResetType::Restart,
                ResetMode::ApplyConfiguration => ResetType::ApplyConfiguration,
            };
            send_confirmed_control(
                access,
                PhoneCommand::new(
                    device_id.clone(),
                    PhoneCommandAction::ResetDevice { reset_type },
                ),
            )
            .await?;
            Ok(ControlOutcome::Reset { device_id, mode })
        }
        ControlOperation::Answer { call_id, device_id } => {
            answer_control_call(access, call_id, device_id).await
        }
        ControlOperation::End { call_id } => end_control_call(access, call_id).await,
        ControlOperation::Originate {
            device_id,
            line,
            destination,
            assigned_channel_id,
        } => {
            originate_control_call(access, device_id, line, destination, assigned_channel_id).await
        }
    }
}

pub async fn deliver_status_message(
    access: &Access,
    device_id: DeviceId,
    text: String,
    beep: bool,
    timeout_seconds: u8,
) -> Result<(), ControlProviderError> {
    send_confirmed_control(
        access,
        PhoneCommand::new(
            device_id,
            PhoneCommandAction::SetStatusMessage {
                message: sccp_protocol::HandsetStatusMessage::Display {
                    text,
                    timeout_seconds,
                    priority: None,
                },
                beep,
            },
        ),
    )
    .await
}

pub async fn send_confirmed_control(
    access: &Access,
    command: PhoneCommand,
) -> Result<(), ControlProviderError> {
    tokio::time::timeout(
        MANAGER_CONTROL_DELIVERY_TIMEOUT,
        access.phone.send_confirmed(command),
    )
    .await
    .map_err(|_| ControlProviderError::HandsetDelivery)?
    .map_err(|_| ControlProviderError::HandsetDelivery)
}

pub async fn restore_system_message(access: &Access, device_id: &DeviceId) {
    let now = Instant::now();
    let message = {
        let mut active = access.shared.system_message.lock_unpoisoned();
        let remaining = active.as_ref().and_then(|message| {
            message
                .expires_at
                .map(|expiry| expiry.saturating_duration_since(now))
        });
        if remaining.is_some_and(|remaining| remaining.is_zero()) {
            *active = None;
            None
        } else {
            active.clone().map(|message| {
                let timeout_seconds = remaining.map_or(0, |remaining| {
                    remaining.as_secs().clamp(1, u64::from(u8::MAX)) as u8
                });
                (message.text, message.beep, timeout_seconds)
            })
        }
    };
    let Some((text, beep, timeout_seconds)) = message else {
        return;
    };
    if deliver_status_message(access, device_id.clone(), text, beep, timeout_seconds)
        .await
        .is_err()
    {
        ast_log(
            LogLevel::Warning,
            "unable to restore the active system message on a registered device",
        );
    }
}

pub async fn answer_control_call(
    access: &Access,
    call_id: CallId,
    requested_device: Option<DeviceId>,
) -> Result<ControlOutcome, ControlProviderError> {
    let call = controller_step(&access.shared.controller, |controller| {
        controller.call(call_id)
    })
    .ok_or(ControlProviderError::CallNotFound)?;
    if requested_device
        .as_ref()
        .is_some_and(|device| device != &call.device_id)
    {
        return Err(ControlProviderError::CallOwnership);
    }
    if call.state != CallState::Ringing {
        return Err(ControlProviderError::CallNotRinging);
    }
    let transition = controller_step(&access.shared.controller, |controller| {
        controller.begin_active_call_switch_transaction(&call.device_id, call_id)
    })
    .map_err(|_| ControlProviderError::CallNotRinging)?;
    if !execute_call_transition_result(access, transition).await? {
        return Err(ControlProviderError::CallNotRinging);
    }
    cancel_no_answer_timer(access, call.pbx_id);
    Ok(ControlOutcome::Answer {
        device_id: call.device_id,
        call_id,
    })
}

pub async fn end_control_call(
    access: &Access,
    call_id: CallId,
) -> Result<ControlOutcome, ControlProviderError> {
    let call = controller_step(&access.shared.controller, |controller| {
        controller.call(call_id)
    })
    .ok_or(ControlProviderError::CallNotFound)?;
    let effects = controller_step(&access.shared.controller, |controller| {
        controller.hangup(call_id)
    });
    if effects.is_empty() {
        return Err(ControlProviderError::CallNotFound);
    }
    execute_control_cleanup(access, effects).await?;
    Ok(ControlOutcome::End {
        device_id: call.device_id,
        call_id,
    })
}

pub async fn originate_control_call(
    access: &Access,
    device_id: DeviceId,
    requested_line: Option<String>,
    destination: String,
    assigned_channel_id: Option<String>,
) -> Result<ControlOutcome, ControlProviderError> {
    let config = access.config();
    if !config.devices.contains_key(&device_id) {
        return Err(ControlProviderError::DeviceNotFound);
    }
    let selected_line = controller_step(&access.shared.controller, |controller| {
        controller
            .registered_device(&device_id)
            .map(|registered| registered.selected_line)
    })
    .ok_or(ControlProviderError::DeviceNotRegistered)?;
    let mut bindings = config
        .appearances_for_device(&device_id)
        .cloned()
        .collect::<Vec<_>>();
    bindings.sort_by_key(|binding| binding.line_instance);
    let binding = if let Some(line) = requested_line.as_deref() {
        bindings
            .into_iter()
            .find(|binding| binding.line.number == line)
    } else if let Some(instance) = selected_line {
        bindings
            .iter()
            .find(|binding| binding.line_instance == instance)
            .cloned()
            .or_else(|| bindings.into_iter().next())
    } else {
        bindings.into_iter().next()
    }
    .ok_or(ControlProviderError::LineNotFound)?;
    drop(config);
    let codec = preferred_codec(
        access,
        &device_id,
        binding.line_instance,
        &PbxAudioFormat::ALL,
    )
    .ok_or(ControlProviderError::NoCompatibleCodec)?;
    if assigned_channel_id
        .as_ref()
        .is_some_and(|uniqueid| native_uniqueid_in_use(uniqueid))
    {
        return Err(ControlProviderError::AssignedChannelIdConflict);
    }
    let call_id = access.phone.reserve_call_id();
    send_confirmed_control(
        access,
        PhoneCommand::new(
            device_id.clone(),
            PhoneCommandAction::BeginCall {
                line_instance: LineInstance::new(binding.line_instance),
                call_id,
                codec,
            },
        ),
    )
    .await?;
    let (pbx_id, mut effects) = controller_step(&access.shared.controller, |controller| {
        let effects = controller.begin_phone_call(call_id, binding.clone(), codec, Instant::now());
        let pbx_id = controller.call_pbx_id(call_id);
        (pbx_id, effects)
    });
    let Some(pbx_id) = pbx_id else {
        let _ = access
            .phone
            .send(PhoneCommand::new(
                device_id,
                PhoneCommandAction::CloseCall { call_id },
            ))
            .await;
        return Err(ControlProviderError::Backend);
    };
    if let Some(uniqueid) = &assigned_channel_id {
        access
            .shared
            .assigned_channel_ids
            .lock()
            .map_err(|_| ControlProviderError::Unavailable)?
            .insert(pbx_id, uniqueid.clone());
    }
    effects.extend(controller_step(&access.shared.controller, |controller| {
        controller.enbloc(call_id, destination)
    }));
    let result = execute_control_effects(access, effects).await;
    access
        .shared
        .assigned_channel_ids
        .lock()
        .map_err(|_| ControlProviderError::Unavailable)?
        .remove(&pbx_id);
    if let Err(error) = result {
        let conflict = assigned_channel_id
            .as_ref()
            .is_some_and(|uniqueid| native_uniqueid_in_use(uniqueid));
        let cleanup = controller_step(&access.shared.controller, |controller| {
            controller.hangup(call_id)
        });
        let _ = execute_control_cleanup(access, cleanup).await;
        let _ = access
            .phone
            .send(PhoneCommand::new(
                device_id,
                PhoneCommandAction::CloseCall { call_id },
            ))
            .await;
        return Err(if conflict {
            ControlProviderError::AssignedChannelIdConflict
        } else {
            error
        });
    }
    Ok(ControlOutcome::Originate {
        device_id,
        line: binding.line.number,
        call_id,
    })
}

pub async fn handle_service_operation(
    access: &Access,
    recordings: &mut RuntimeRecordings,
    operation: ServiceOperation,
) -> Result<ServiceOutcome, ServiceProviderError> {
    match operation {
        ServiceOperation::Microphone { device_id, enabled } => {
            microphone_service_operation(access, device_id, enabled).await
        }
        ServiceOperation::Recording {
            command,
            call_id,
            filename,
            append,
            bridged_only,
            direction,
        } => {
            recording_service_operation(
                access,
                recordings,
                command,
                call_id,
                filename,
                append,
                bridged_only,
                direction,
            )
            .await
        }
        ServiceOperation::Parking {
            command,
            device_id,
            call_id,
            line_instance,
            lot,
            slot,
        } => {
            parking_service_operation(
                access,
                command,
                device_id,
                call_id,
                line_instance,
                lot,
                slot,
            )
            .await
        }
        ServiceOperation::Conference {
            command,
            conference_id,
            participant_id,
        } => conference_service_operation(access, command, conference_id, participant_id).await,
    }
}

pub async fn microphone_service_operation(
    access: &Access,
    device_id: DeviceId,
    enabled: bool,
) -> Result<ServiceOutcome, ServiceProviderError> {
    if !access.config().devices.contains_key(&device_id) {
        return Err(ServiceProviderError::DeviceNotFound);
    }
    let call_id = controller_step(&access.shared.controller, |controller| {
        let registered = controller.registered_device(&device_id)?;
        let mut selected = registered
            .selected_calls()
            .filter(|call_id| {
                controller
                    .call(*call_id)
                    .is_some_and(|call| call.device_id == device_id)
            })
            .collect::<Vec<_>>();
        selected.sort_by_key(|call_id| call_id.0);
        if selected.len() == 1 {
            return selected.first().copied();
        }
        let mut active = controller
            .calls()
            .filter(|call| {
                call.device_id == device_id
                    && matches!(
                        call.state,
                        CallState::Connected
                            | CallState::Held
                            | CallState::SharedHeld
                            | CallState::Barged
                    )
            })
            .map(|call| call.sccp_id)
            .collect::<Vec<_>>();
        active.sort_by_key(|call_id| call_id.0);
        (active.len() == 1).then(|| active[0])
    })
    .ok_or_else(|| {
        if controller_step(&access.shared.controller, |controller| {
            controller.is_registered(&device_id)
        }) {
            ServiceProviderError::CallState
        } else {
            ServiceProviderError::DeviceNotRegistered
        }
    })?;
    send_confirmed_service(
        access,
        PhoneCommand::new(
            device_id.clone(),
            PhoneCommandAction::SetMicrophoneMode { enabled },
        ),
    )
    .await?;
    Ok(ServiceOutcome::Microphone {
        device_id,
        call_id,
        enabled,
    })
}

#[allow(clippy::too_many_arguments)]
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

async fn restore_recording_session(
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

#[allow(clippy::too_many_arguments)]
pub async fn parking_service_operation(
    access: &Access,
    command: AmiParkingCommand,
    device_id: DeviceId,
    call_id: Option<CallId>,
    line_instance: Option<u32>,
    requested_lot: Option<String>,
    slot: Option<u32>,
) -> Result<ServiceOutcome, ServiceProviderError> {
    if !access.config().devices.contains_key(&device_id) {
        return Err(ServiceProviderError::DeviceNotFound);
    }
    if !controller_step(&access.shared.controller, |controller| {
        controller.is_registered(&device_id)
    }) {
        return Err(ServiceProviderError::DeviceNotRegistered);
    }
    match command {
        AmiParkingCommand::Park => {
            let call_id = call_id.ok_or(ServiceProviderError::CallNotFound)?;
            let call = controller_step(&access.shared.controller, |controller| {
                controller.call(call_id)
            })
            .ok_or(ServiceProviderError::CallNotFound)?;
            if call.device_id != device_id {
                return Err(ServiceProviderError::CallOwnership);
            }
            let config = access.config();
            let enabled = config
                .parking_for_device(&device_id)
                .is_some_and(|parking| parking.enabled);
            let line_lot = access
                .line_binding(&device_id, call.line_instance)
                .and_then(|binding| {
                    config
                        .parking_for_line(&binding.line.number)
                        .and_then(|parking| parking.lot.clone())
                });
            let lot = requested_lot.or(line_lot);
            drop(config);
            let result = controller_step(&access.shared.controller, |controller| {
                let pbx_id = controller.call_pbx_id(call_id);
                (pbx_id, controller.park(call_id, enabled, lot.clone()))
            });
            let pbx_id = result.0.ok_or(ServiceProviderError::CallNotFound)?;
            let effects = result.1.map_err(parking_service_error)?;
            access
                .shared
                .pending_parks
                .lock()
                .map_err(|_| ServiceProviderError::Unavailable)?
                .insert(
                    call_id,
                    PendingPark {
                        pbx_id,
                        device_id: device_id.clone(),
                        requested_lot: lot.clone(),
                        parkee_unique_id: None,
                        deadline: Instant::now() + PARKING_CONFIRM_TIMEOUT,
                    },
                );
            execute_service_effects(access, effects).await?;
            Ok(ServiceOutcome::Parking {
                command,
                device_id,
                call_id,
                lot,
                slot: None,
            })
        }
        AmiParkingCommand::Retrieve => {
            let slot = slot.ok_or(ServiceProviderError::ParkingNotFound)?;
            let config = access.config();
            let selected_line = line_instance.or_else(|| {
                controller_step(&access.shared.controller, |controller| {
                    controller
                        .registered_device(&device_id)
                        .and_then(|device| device.selected_line)
                })
            });
            let binding = selected_line
                .and_then(|line| access.line_binding(&device_id, line))
                .or_else(|| config.appearances_for_device(&device_id).next().cloned())
                .ok_or(ServiceProviderError::CallState)?;
            let lot = requested_lot
                .or_else(|| {
                    config
                        .parking_for_line(&binding.line.number)
                        .and_then(|parking| parking.lot.clone())
                })
                .unwrap_or_else(|| "default".to_owned());
            drop(config);
            let call_id = begin_parking_retrieval(
                access,
                device_id.clone(),
                binding.line_instance,
                lot.clone(),
                slot,
            )
            .await?;
            Ok(ServiceOutcome::Parking {
                command,
                device_id,
                call_id,
                lot: Some(lot),
                slot: Some(slot),
            })
        }
    }
}

pub async fn conference_service_operation(
    access: &Access,
    command: AmiConferenceCommand,
    conference_id: ConferenceId,
    participant_id: Option<ParticipantId>,
) -> Result<ServiceOutcome, ServiceProviderError> {
    let session = controller_step(&access.shared.controller, |controller| {
        controller.conference_session_by_id(conference_id).cloned()
    })
    .filter(|session| session.phase == ConferencePhase::Active)
    .ok_or(ServiceProviderError::ConferenceNotFound)?;
    match command {
        AmiConferenceCommand::End => {
            let effects = controller_step(&access.shared.controller, |controller| {
                controller.end_conference_by_moderator(&session.device_id, conference_id)
            })
            .map_err(conference_end_service_error)?;
            cancel_conference_announcement(access, conference_id);
            execute_service_cleanup(access, effects).await?;
        }
        AmiConferenceCommand::Kick => {
            remove_conference_participant(
                access,
                session,
                participant_id.ok_or(ServiceProviderError::ParticipantNotFound)?,
            )
            .await?;
        }
        AmiConferenceCommand::Mute => {
            let participant_id = participant_id.ok_or(ServiceProviderError::ParticipantNotFound)?;
            let muted = session
                .participants
                .get(participant_id)
                .map(|participant| !participant.muted)
                .ok_or(ServiceProviderError::ParticipantNotFound)?;
            set_conference_participant_muted(access, session, participant_id, muted).await?;
        }
        AmiConferenceCommand::Moderate => {
            let participant_id = participant_id.ok_or(ServiceProviderError::ParticipantNotFound)?;
            let moderator = session
                .participants
                .get(participant_id)
                .map(|participant| !participant.moderator)
                .ok_or(ServiceProviderError::ParticipantNotFound)?;
            set_conference_participant_moderator(access, session, participant_id, moderator)
                .await?;
        }
        AmiConferenceCommand::Invite => return Err(ServiceProviderError::Unsupported),
    }
    Ok(ServiceOutcome::Conference {
        command,
        conference_id,
        participant_id,
    })
}

pub fn parking_service_error(error: ParkingRejection) -> ServiceProviderError {
    match error {
        ParkingRejection::Disabled => ServiceProviderError::ParkingDisabled,
        ParkingRejection::Conflict => ServiceProviderError::ParkingConflict,
        ParkingRejection::Unavailable => ServiceProviderError::CallNotFound,
    }
}

pub fn recording_registry_service_error<E>(
    error: RecordingRegistryError<E>,
) -> ServiceProviderError {
    match error {
        RecordingRegistryError::Exists => ServiceProviderError::RecordingExists,
        RecordingRegistryError::NotFound => ServiceProviderError::RecordingNotFound,
        RecordingRegistryError::Session(_) => ServiceProviderError::RecordingFailed,
    }
}

pub fn conference_participant_service_error(
    error: ConferenceParticipantRejection,
) -> ServiceProviderError {
    match error {
        ConferenceParticipantRejection::Unavailable => ServiceProviderError::ConferenceNotFound,
        ConferenceParticipantRejection::NotModerator => {
            ServiceProviderError::ConferenceAuthorization
        }
        ConferenceParticipantRejection::InvalidParticipant => {
            ServiceProviderError::ParticipantNotFound
        }
        ConferenceParticipantRejection::Moderator
        | ConferenceParticipantRejection::LastModerator
        | ConferenceParticipantRejection::Conflict => ServiceProviderError::ConferenceConflict,
    }
}

pub fn conference_end_service_error(error: ConferenceEndRejection) -> ServiceProviderError {
    match error {
        ConferenceEndRejection::Unavailable => ServiceProviderError::ConferenceNotFound,
        ConferenceEndRejection::NotModerator => ServiceProviderError::ConferenceAuthorization,
        ConferenceEndRejection::Conflict => ServiceProviderError::ConferenceConflict,
    }
}

pub async fn send_confirmed_service(
    access: &Access,
    command: PhoneCommand,
) -> Result<(), ServiceProviderError> {
    tokio::time::timeout(
        MANAGER_CONTROL_DELIVERY_TIMEOUT,
        access.phone.send_confirmed(command),
    )
    .await
    .map_err(|_| ServiceProviderError::Delivery)?
    .map_err(|_| ServiceProviderError::Delivery)
}

pub async fn execute_service_effects(
    access: &Access,
    effects: Vec<DriverEffect>,
) -> Result<(), ServiceProviderError> {
    let backend = AsteriskBackend::new(access);
    for (index, effect) in effects.into_iter().enumerate() {
        if let Err(error) = execute_one_effect(access, &backend, index, effect).await {
            handle_effect_error(access, &backend, error).await;
            return Err(ServiceProviderError::Delivery);
        }
    }
    Ok(())
}

pub async fn execute_service_cleanup(
    access: &Access,
    effects: Vec<DriverEffect>,
) -> Result<(), ServiceProviderError> {
    let backend = AsteriskBackend::new(access);
    let errors = execute_backend_cleanup_effects(&backend, effects, |effect| {
        execute_handset_effect(access, effect)
    })
    .await;
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ServiceProviderError::Delivery)
    }
}

pub fn native_uniqueid_in_use(uniqueid: &str) -> bool {
    let uniqueid = c_string(uniqueid);
    unsafe { native_channel::uniqueid_in_use(&uniqueid) }
}

pub async fn execute_control_effects(
    access: &Access,
    effects: Vec<DriverEffect>,
) -> Result<(), ControlProviderError> {
    let backend = AsteriskBackend::new(access);
    for (index, effect) in effects.into_iter().enumerate() {
        if let Err(error) = execute_one_effect(access, &backend, index, effect).await {
            let provider_error = match &error {
                EffectExecutionError::Backend { .. } => ControlProviderError::Backend,
                EffectExecutionError::Handset { .. } => ControlProviderError::HandsetDelivery,
            };
            handle_effect_error(access, &backend, error).await;
            return Err(provider_error);
        }
    }
    Ok(())
}

pub async fn execute_control_cleanup(
    access: &Access,
    effects: Vec<DriverEffect>,
) -> Result<(), ControlProviderError> {
    let backend = AsteriskBackend::new(access);
    let errors = execute_backend_cleanup_effects(&backend, effects, |effect| {
        execute_handset_effect(access, effect)
    })
    .await;
    if errors.is_empty() {
        return Ok(());
    }
    let handset_failure = errors
        .iter()
        .any(|error| matches!(error, EffectExecutionError::Handset { .. }));
    for error in errors {
        ast_log(
            LogLevel::Warning,
            &format!("SCCP management-control cleanup failed: {error}"),
        );
    }
    Err(if handset_failure {
        ControlProviderError::HandsetDelivery
    } else {
        ControlProviderError::Backend
    })
}
