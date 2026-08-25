//! Runtime control delivery and call-control services.

use super::{
    Access, ActiveSystemMessage, CallId, CallState, ControlOperation, ControlOutcome,
    ControlProviderError, DeviceId, Duration, Instant, LineInstance, LogLevel,
    MANAGER_CONTROL_DELIVERY_TIMEOUT, MessageTarget, MutexExt as _, PbxAudioFormat, PhoneCommand,
    PhoneCommandAction, ResetMode, ResetType, ast_log, cancel_no_answer_timer, controller_step,
    execute_call_transition_result, execute_control_cleanup, execute_control_effects,
    native_uniqueid_in_use, preferred_codec, registered_device_ids,
};

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
    if let Some(uniqueid) = assigned_channel_id.as_ref() {
        let in_use = native_uniqueid_in_use(uniqueid).map_err(|error| {
            ast_log(
                LogLevel::Warning,
                &format!("assigned channel identity contains invalid native text: {error}"),
            );
            ControlProviderError::Backend
        })?;
        if in_use {
            return Err(ControlProviderError::AssignedChannelIdConflict);
        }
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
            .and_then(|uniqueid| native_uniqueid_in_use(uniqueid).ok())
            .unwrap_or(false);
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
