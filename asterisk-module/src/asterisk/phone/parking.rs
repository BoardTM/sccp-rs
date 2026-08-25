//! Handset pickup, parking, and retrieval interactions.

use super::{
    Access, CallDirection, CallId, CallInfo, CallState, DeviceId, Instant, LineInstance,
    MutexExt as _, PARKING_CONFIRM_TIMEOUT, PARKING_MENU_MAX_ITEMS, PARKING_NOTIFICATION_TIME,
    ParkedCall, ParkingEvent, ParkingEventKind, ParkingMenuEntry, ParkingRejection,
    ParkingRetrievalBehavior, PbxAudioFormat, PendingPark, PendingParkingNotification,
    PendingRetrieval, PhoneCommand, PhoneCommandAction, PickupRejection, ServiceProviderError,
    TransactionId, controller_step, execute_cleanup_effects, execute_effects,
    execute_service_effects, handset_call_id_from_channel, parking_service_error, preferred_codec,
    send_confirmed_service,
};

pub(super) async fn handle_pickup_soft_key(
    access: &Access,
    device_id: DeviceId,
    call_id: CallId,
    line_instance: u32,
    directed: bool,
) {
    let config = access.config();
    let binding = access.line_binding(&device_id, line_instance);
    let pickup = binding
        .as_ref()
        .and_then(|binding| config.features_for_line(&binding.line.number))
        .map(|features| features.pickup.clone());
    drop(config);
    let (Some(binding), Some(pickup)) = (binding, pickup) else {
        reject_pickup(access, device_id, call_id, PickupRejection::Unavailable).await;
        return;
    };
    let permitted = !pickup.pickup_groups.is_empty() || !pickup.named_pickup_groups.is_empty();
    if directed {
        let context = pickup
            .directed_context
            .unwrap_or_else(|| binding.line.context.clone());
        let result = controller_step(&access.shared.controller, |controller| {
            controller.begin_directed_pickup(
                call_id,
                permitted,
                pickup.directed,
                context,
                pickup.answer_directed,
            )
        });
        match result {
            Ok(()) => {
                let _ = access
                    .phone
                    .send(PhoneCommand::new(
                        device_id,
                        PhoneCommandAction::DisplayPrompt {
                            call_id,
                            timeout_seconds: 0,
                            text: "Enter pickup extension".into(),
                        },
                    ))
                    .await;
            }
            Err(rejection) => reject_pickup(access, device_id, call_id, rejection).await,
        }
    } else {
        let result = controller_step(&access.shared.controller, |controller| {
            controller.group_pickup(call_id, permitted, pickup.answer_directed)
        });
        match result {
            Ok(effects) => execute_effects(access, effects).await,
            Err(rejection) => reject_pickup(access, device_id, call_id, rejection).await,
        }
    }
}

pub(super) async fn reject_pickup(
    access: &Access,
    device_id: DeviceId,
    call_id: CallId,
    rejection: PickupRejection,
) {
    let text = match rejection {
        PickupRejection::Permission => "Pickup not permitted",
        PickupRejection::Disabled => "Directed pickup disabled",
        PickupRejection::Conflict => "Another pickup attempt won",
        PickupRejection::Unavailable => "Pickup unavailable",
    };
    let _ = access
        .phone
        .send(PhoneCommand::new(
            device_id.clone(),
            PhoneCommandAction::DisplayPrompt {
                call_id,
                timeout_seconds: 4,
                text: text.into(),
            },
        ))
        .await;
    let collecting = controller_step(&access.shared.controller, |controller| {
        controller
            .call(call_id)
            .is_some_and(|call| call.state == CallState::Collecting)
    });
    if collecting {
        let cleanup = controller_step(&access.shared.controller, |controller| {
            controller.hangup(call_id)
        });
        execute_effects(access, cleanup).await;
        let _ = access
            .phone
            .send(PhoneCommand::new(
                device_id,
                PhoneCommandAction::CloseCall { call_id },
            ))
            .await;
    }
}

pub(super) async fn handle_park_request(
    access: &Access,
    device_id: DeviceId,
    call_id: CallId,
    line_instance: u32,
    button_lot: Option<String>,
) {
    let config = access.config();
    let enabled = config
        .parking_for_device(&device_id)
        .is_some_and(|parking| parking.enabled);
    let binding = access.line_binding(&device_id, line_instance);
    let line_lot = binding.as_ref().and_then(|binding| {
        config
            .parking_for_line(&binding.line.number)
            .and_then(|parking| parking.lot.clone())
    });
    let lot = button_lot.or(line_lot);
    drop(config);
    if binding.is_none() {
        reject_parking(access, device_id, call_id, ParkingRejection::Unavailable).await;
        return;
    }
    let result = controller_step(&access.shared.controller, |controller| {
        let pbx_id = controller.call_pbx_id(call_id);
        (pbx_id, controller.park(call_id, enabled, lot.clone()))
    });
    let (Some(pbx_id), Ok(effects)) = result else {
        reject_parking(
            access,
            device_id,
            call_id,
            result.1.err().unwrap_or(ParkingRejection::Unavailable),
        )
        .await;
        return;
    };
    access.shared.pending_parks.lock_unpoisoned().insert(
        call_id,
        PendingPark {
            pbx_id,
            device_id: device_id.clone(),
            requested_lot: lot,
            parkee_unique_id: None,
            deadline: Instant::now() + PARKING_CONFIRM_TIMEOUT,
        },
    );
    let _ = access
        .phone
        .send(PhoneCommand::new(
            device_id,
            PhoneCommandAction::DisplayPrompt {
                call_id,
                timeout_seconds: PARKING_CONFIRM_TIMEOUT.as_secs() as u32,
                text: "Parking call".into(),
            },
        ))
        .await;
    execute_effects(access, effects).await;
}

pub(super) async fn reject_parking(
    access: &Access,
    device_id: DeviceId,
    call_id: CallId,
    rejection: ParkingRejection,
) {
    let text = match rejection {
        ParkingRejection::Disabled => "Parking disabled",
        ParkingRejection::Conflict => "Call cannot be parked",
        ParkingRejection::Unavailable => "Parking unavailable",
    };
    let _ = access
        .phone
        .send(PhoneCommand::new(
            device_id,
            PhoneCommandAction::DisplayPrompt {
                call_id,
                timeout_seconds: 4,
                text: text.into(),
            },
        ))
        .await;
}

pub(super) async fn handle_parking_lot_button(
    access: &Access,
    device_id: DeviceId,
    instance: u32,
    call_id: Option<CallId>,
    line_instance: u32,
) {
    let button = access
        .config()
        .parking_lot_for_button(&device_id, instance)
        .cloned();
    let Some(button) = button else {
        return;
    };
    let connected_call = call_id.filter(|call_id| {
        controller_step(&access.shared.controller, |controller| {
            controller
                .call(*call_id)
                .is_some_and(|call| call.state == CallState::Connected)
        })
    });
    if let Some(call_id) = connected_call {
        handle_park_request(access, device_id, call_id, line_instance, Some(button.lot)).await;
        return;
    }

    let parked = access
        .shared
        .parking_registry
        .lock_unpoisoned()
        .calls_in_lot(&button.lot);
    if parked.len() == 1 && button.retrieval == ParkingRetrievalBehavior::RetrieveSingle {
        let _ =
            begin_parking_retrieval(access, device_id, line_instance, button.lot, parked[0].slot)
                .await;
    } else {
        show_parking_menu(access, device_id, instance, &button.lot, &parked).await;
    }
}

pub(super) async fn show_parking_menu(
    access: &Access,
    device_id: DeviceId,
    instance: u32,
    lot: &str,
    calls: &[ParkedCall],
) {
    let calls = parking_menu_entries(calls);
    let _ = access
        .phone
        .send(PhoneCommand::new(
            device_id,
            PhoneCommandAction::ShowParkingMenu {
                instance: LineInstance::new(instance),
                transaction_id: TransactionId::new(instance),
                lot: lot.to_owned(),
                calls,
            },
        ))
        .await;
}

fn parking_menu_entries(calls: &[ParkedCall]) -> Vec<ParkingMenuEntry> {
    calls
        .iter()
        .take(PARKING_MENU_MAX_ITEMS)
        .map(|call| ParkingMenuEntry {
            slot: call.slot,
            caller_name: call.caller_name.clone(),
            caller_number: call.caller_number.clone(),
            connected_name: call.connected_name.clone(),
            connected_number: call.connected_number.clone(),
        })
        .collect()
}

fn parking_retrieval_call_info(call: ParkedCall) -> CallInfo {
    CallInfo {
        direction: CallDirection::Inbound,
        calling_name: call.caller_name,
        calling_number: call.caller_number,
        called_name: if call.connected_name.is_empty() {
            format!("Parked call {}", call.slot)
        } else {
            call.connected_name
        },
        called_number: call.slot.to_string(),
        ..CallInfo::default()
    }
}

pub async fn begin_parking_retrieval(
    access: &Access,
    device_id: DeviceId,
    requested_line_instance: u32,
    lot: String,
    slot: u32,
) -> Result<CallId, ServiceProviderError> {
    let config = access.config();
    let binding = if requested_line_instance == 0 {
        config.appearances_for_device(&device_id).next().cloned()
    } else {
        access.line_binding(&device_id, requested_line_instance)
    };
    let Some(binding) = binding else {
        return Err(ServiceProviderError::CallState);
    };
    let Some(codec) = preferred_codec(
        access,
        &device_id,
        binding.line_instance,
        &PbxAudioFormat::ALL,
    ) else {
        return Err(ServiceProviderError::CallState);
    };
    let call = access
        .shared
        .parking_registry
        .lock_unpoisoned()
        .call(&lot, slot)
        .cloned();
    let Some(call) = call else {
        publish_parking_lot(access, &lot);
        return Err(ServiceProviderError::ParkingNotFound);
    };
    let call_id = access.phone.reserve_call_id();
    let claimed = access.shared.parking_registry.lock_unpoisoned().claim(
        &lot,
        slot,
        device_id.clone(),
        call_id,
    );
    if !claimed {
        return Err(ServiceProviderError::ParkingConflict);
    }
    if send_confirmed_service(
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
    .await
    .is_err()
    {
        access
            .shared
            .parking_registry
            .lock_unpoisoned()
            .release_claim(&lot, slot, call_id);
        return Err(ServiceProviderError::Delivery);
    }
    let info = parking_retrieval_call_info(call);
    let result = controller_step(&access.shared.controller, |controller| {
        let effects = controller.begin_parking_retrieval(
            call_id,
            binding,
            codec,
            Some(lot.clone()),
            slot,
            info,
        );
        let pbx_id = controller.call_pbx_id(call_id);
        (pbx_id, effects)
    });
    let (Some(pbx_id), effects) = result else {
        access
            .shared
            .parking_registry
            .lock_unpoisoned()
            .release_claim(&lot, slot, call_id);
        let _ = access
            .phone
            .send(PhoneCommand::new(
                device_id,
                PhoneCommandAction::CloseCall { call_id },
            ))
            .await;
        return Err(ServiceProviderError::ParkingConflict);
    };
    let effects = match effects {
        Ok(effects) => effects,
        Err(error) => {
            access
                .shared
                .parking_registry
                .lock_unpoisoned()
                .release_claim(&lot, slot, call_id);
            let _ = access
                .phone
                .send(PhoneCommand::new(
                    device_id,
                    PhoneCommandAction::CloseCall { call_id },
                ))
                .await;
            return Err(parking_service_error(error));
        }
    };
    access.shared.pending_retrievals.lock_unpoisoned().insert(
        call_id,
        PendingRetrieval {
            pbx_id,
            device_id,
            lot,
            slot,
            deadline: Instant::now() + PARKING_CONFIRM_TIMEOUT,
        },
    );
    execute_service_effects(access, effects).await?;
    Ok(call_id)
}

pub async fn handle_parking_event(access: &Access, event: ParkingEvent) {
    let kind = event.kind;
    let lot = event.lot.clone();
    let change = access
        .shared
        .parking_registry
        .lock_unpoisoned()
        .apply(&event);
    match kind {
        ParkingEventKind::Parked | ParkingEventKind::Swap => {
            if let Some((call_id, pending)) = take_pending_park(access, &event) {
                let effects = controller_step(&access.shared.controller, |controller| {
                    controller.parking_confirmed(call_id, event.slot)
                });
                execute_effects(access, effects).await;
                access.shared.parking_notifications.lock_unpoisoned().push(
                    PendingParkingNotification {
                        device_id: pending.device_id,
                        call_id,
                        deadline: Instant::now() + PARKING_NOTIFICATION_TIME,
                    },
                );
            }
        }
        ParkingEventKind::Retrieved => {
            let call_id = handset_call_id_from_channel(&event.retriever_channel)
                .or_else(|| change.claim.map(|claim| claim.call_id));
            if let Some(call_id) = call_id {
                access
                    .shared
                    .pending_retrievals
                    .lock_unpoisoned()
                    .remove(&call_id);
                let effects = controller_step(&access.shared.controller, |controller| {
                    controller.parking_retrieved(call_id)
                });
                execute_effects(access, effects).await;
            }
        }
        ParkingEventKind::Failed => {
            if let Some((call_id, pending)) = take_pending_park(access, &event) {
                let effects = controller_step(&access.shared.controller, |controller| {
                    controller.parking_failed(call_id)
                });
                execute_effects(access, effects).await;
                let _ = access
                    .phone
                    .send(PhoneCommand::new(
                        pending.device_id,
                        PhoneCommandAction::DisplayPrompt {
                            call_id,
                            timeout_seconds: 4,
                            text: "Unable to park call".into(),
                        },
                    ))
                    .await;
            }
        }
        ParkingEventKind::Timeout | ParkingEventKind::GiveUp => {}
    }
    publish_parking_lot(access, &lot);
}

pub(super) fn take_pending_park(
    access: &Access,
    event: &ParkingEvent,
) -> Option<(CallId, PendingPark)> {
    let mut pending = access.shared.pending_parks.lock_unpoisoned();
    let selected = pending
        .iter()
        .filter(|(_, attempt)| {
            attempt
                .parkee_unique_id
                .as_deref()
                .is_some_and(|unique_id| unique_id == event.parkee_unique_id)
                || (attempt.parkee_unique_id.is_none()
                    && attempt
                        .requested_lot
                        .as_deref()
                        .is_none_or(|lot| lot == event.lot))
        })
        .min_by_key(|(call_id, attempt)| (attempt.deadline, call_id.0))
        .map(|(call_id, _)| *call_id)?;
    pending.remove(&selected).map(|attempt| (selected, attempt))
}

pub(super) fn publish_parking_lot(access: &Access, lot: &str) {
    let enabled = access
        .shared
        .parking_registry
        .lock_unpoisoned()
        .lot_has_calls(lot);
    let config = access.config();
    for (device_id, device) in &config.devices {
        for (&instance, button) in &device.parking.feature_buttons {
            if button.lot == lot {
                access.spawn_phone(PhoneCommand::new(
                    device_id.clone(),
                    PhoneCommandAction::SetFeatureStatus {
                        instance: LineInstance::new(instance),
                        enabled,
                    },
                ));
            }
        }
    }
}

pub async fn expire_parking_attempts(access: &Access, now: Instant) {
    let expired_parks = {
        let mut pending = access.shared.pending_parks.lock_unpoisoned();
        let expired: Vec<_> = pending
            .iter()
            .filter(|(_, attempt)| attempt.deadline <= now)
            .map(|(call_id, _)| *call_id)
            .collect();
        expired
            .into_iter()
            .filter_map(|call_id| pending.remove(&call_id).map(|attempt| (call_id, attempt)))
            .collect::<Vec<_>>()
    };
    for (call_id, pending) in expired_parks {
        let effects = controller_step(&access.shared.controller, |controller| {
            controller.parking_failed(call_id)
        });
        execute_cleanup_effects(access, effects).await;
        let _ = access
            .phone
            .send(PhoneCommand::new(
                pending.device_id,
                PhoneCommandAction::DisplayPrompt {
                    call_id,
                    timeout_seconds: 4,
                    text: "Parking timed out".into(),
                },
            ))
            .await;
    }

    let expired_retrievals = {
        let mut pending = access.shared.pending_retrievals.lock_unpoisoned();
        let expired: Vec<_> = pending
            .iter()
            .filter(|(_, attempt)| attempt.deadline <= now)
            .map(|(call_id, _)| *call_id)
            .collect();
        expired
            .into_iter()
            .filter_map(|call_id| pending.remove(&call_id).map(|attempt| (call_id, attempt)))
            .collect::<Vec<_>>()
    };
    for (call_id, pending) in expired_retrievals {
        access
            .shared
            .parking_registry
            .lock_unpoisoned()
            .release_claim(&pending.lot, pending.slot, call_id);
        let effects = controller_step(&access.shared.controller, |controller| {
            controller.parking_retrieval_failed(call_id)
        });
        execute_cleanup_effects(access, effects).await;
        let _ = access
            .phone
            .send(PhoneCommand::new(
                pending.device_id.clone(),
                PhoneCommandAction::DisplayPrompt {
                    call_id,
                    timeout_seconds: 3,
                    text: "Parked call unavailable".into(),
                },
            ))
            .await;
        access
            .shared
            .parking_notifications
            .lock_unpoisoned()
            .push(PendingParkingNotification {
                device_id: pending.device_id,
                call_id,
                deadline: now + PARKING_NOTIFICATION_TIME,
            });
    }

    let notifications = {
        let mut pending = access.shared.parking_notifications.lock_unpoisoned();
        let mut expired = Vec::new();
        pending.retain(|notification| {
            if notification.deadline <= now {
                expired.push(notification.clone());
                false
            } else {
                true
            }
        });
        expired
    };
    for notification in notifications {
        let _ = access
            .phone
            .send(PhoneCommand::new(
                notification.device_id,
                PhoneCommandAction::CloseCall {
                    call_id: notification.call_id,
                },
            ))
            .await;
    }
}

#[cfg(test)]
mod parking_projection_tests {
    use super::*;

    fn parked(caller_name: &str, caller_number: &str, connected_name: &str) -> ParkedCall {
        ParkedCall {
            lot: "default".into(),
            slot: 701,
            timeout_seconds: 30,
            duration_seconds: 2,
            parker_dial_string: String::new(),
            parkee_channel: "SCCP/1001".into(),
            parkee_unique_id: "id".into(),
            caller_name: caller_name.into(),
            caller_number: caller_number.into(),
            connected_name: connected_name.into(),
            connected_number: String::new(),
        }
    }

    #[test]
    fn parking_ui_projection_preserves_redaction() {
        let entries = parking_menu_entries(&[parked("", "", "")]);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].caller_name.is_empty());
        assert!(entries[0].caller_number.is_empty());
        assert!(entries[0].connected_name.is_empty());
        assert!(entries[0].connected_number.is_empty());
    }

    #[test]
    fn retrieval_projection_never_reconstructs_redacted_identity() {
        let info = parking_retrieval_call_info(parked("", "", ""));
        assert!(info.calling_name.is_empty());
        assert!(info.calling_number.is_empty());
        assert_eq!(info.called_name, "Parked call 701");
    }
}
