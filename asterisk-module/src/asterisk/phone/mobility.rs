//! SCCP extension-mobility login and appearance transactions.

use super::{
    Access, ApplicationId, ButtonDefinition, ButtonType, CallReference, DeviceId,
    HandsetStatusMessage, LineInstance, LogLevel, MOBILITY_APPLICATION_ID,
    MobilityAppearanceWriter, MobilityPreparation, MobilitySlot, ModuleConfig, MutexExt as _,
    Ordering, PhoneCommand, PhoneCommandAction, PhoneServiceEvent, PhoneServicePayload,
    PhoneServicePriority, PreparedMobilityTransaction, TransactionId, ast_log, authenticate_line,
    controller_step, execute_mobility_io, mobility_login_document, parse_mobility_login_submission,
    rollback_mobility_io,
};

pub fn configured_mobility_button(config: &ModuleConfig, slot: &MobilitySlot) -> bool {
    config.devices.get(&slot.device_id).is_some_and(|device| {
        device.buttons.iter().any(|button| {
            matches!(
                button,
                ButtonDefinition::Feature(feature)
                    if feature.instance == slot.button_instance
                        && feature.feature == ButtonType::Mobility
            )
        })
    })
}

pub(super) fn reserve_mobility_prompt(
    access: &Access,
    slot: MobilitySlot,
) -> Option<TransactionId> {
    let mut prompts = access.shared.pending_mobility_prompts.lock_unpoisoned();
    prompts.retain(|_, pending_slot| pending_slot != &slot);
    for _ in 0..=prompts.len() {
        let raw = access
            .shared
            .next_mobility_prompt_id
            .fetch_add(1, Ordering::Relaxed) as u32;
        if raw == 0 {
            continue;
        }
        let transaction_id = TransactionId::new(raw);
        let key = (slot.device_id.clone(), transaction_id);
        if let std::collections::hash_map::Entry::Vacant(entry) = prompts.entry(key) {
            entry.insert(slot);
            return Some(transaction_id);
        }
    }
    None
}

pub(super) async fn mobility_status(access: &Access, device_id: DeviceId, text: &'static str) {
    let _ = access
        .phone
        .send(PhoneCommand::new(
            device_id,
            PhoneCommandAction::SetStatusMessage {
                message: HandsetStatusMessage::Display {
                    text: text.into(),
                    timeout_seconds: 4,
                    priority: None,
                },
                beep: false,
            },
        ))
        .await;
}

pub(super) async fn handle_mobility_button(access: &Access, device_id: DeviceId, instance: u32) {
    let _mobility_guard = access.shared.mobility_mutations.lock().await;
    let Ok(slot) = MobilitySlot::new(device_id.clone(), instance) else {
        return;
    };
    if !configured_mobility_button(&access.config(), &slot) {
        return;
    }
    let logout = access
        .shared
        .mobility
        .lock_unpoisoned()
        .appearance_for_slot(&slot)
        .is_some();
    if logout {
        let prepared = access
            .shared
            .mobility
            .lock_unpoisoned()
            .prepare_logout(&slot);
        if let Ok(prepared) = prepared {
            if mobility_appearance_has_calls(access, prepared.previous()) {
                let _ = access.shared.mobility.lock_unpoisoned().abort(&prepared);
                mobility_status(access, device_id, "Mobility line is in use").await;
            } else if apply_mobility_transaction(access, &prepared).await {
                mobility_status(access, device_id, "Mobility logout complete").await;
            } else {
                mobility_status(access, device_id, "Mobility logout failed").await;
            }
        }
        return;
    }

    let Some(transaction_id) = reserve_mobility_prompt(access, slot.clone()) else {
        mobility_status(access, device_id, "Mobility unavailable").await;
        return;
    };
    let document = match mobility_login_document(slot.button_instance) {
        Ok(document) => document,
        Err(_) => {
            access
                .shared
                .pending_mobility_prompts
                .lock_unpoisoned()
                .remove(&(device_id.clone(), transaction_id));
            mobility_status(access, device_id, "Mobility unavailable").await;
            return;
        }
    };
    if access
        .phone
        .send_confirmed(PhoneCommand::new(
            device_id.clone(),
            PhoneCommandAction::ShowInputService {
                line_instance: LineInstance::new(0),
                call_reference: CallReference::new(0),
                application_id: ApplicationId::new(MOBILITY_APPLICATION_ID),
                transaction_id,
                priority: PhoneServicePriority::NORMAL,
                document,
            },
        ))
        .await
        .is_err()
    {
        access
            .shared
            .pending_mobility_prompts
            .lock_unpoisoned()
            .remove(&(device_id, transaction_id));
    }
}

pub(super) async fn handle_mobility_response(
    access: &Access,
    device_id: DeviceId,
    response: PhoneServiceEvent,
) {
    let _mobility_guard = access.shared.mobility_mutations.lock().await;
    if response.routing.application_id != ApplicationId::new(MOBILITY_APPLICATION_ID) {
        return;
    }
    let slot = access
        .shared
        .pending_mobility_prompts
        .lock_unpoisoned()
        .remove(&(device_id.clone(), response.routing.transaction_id));
    let Some(slot) = slot else {
        return;
    };
    if response.routing.line_instance != LineInstance::new(0)
        || response.routing.call_reference != CallReference::new(0)
    {
        mobility_status(access, device_id, "Mobility login rejected").await;
        return;
    }
    let PhoneServicePayload::Submission(submission) = response.payload else {
        mobility_status(access, device_id, "Mobility login rejected").await;
        return;
    };
    let Ok(request) = parse_mobility_login_submission(slot.button_instance, &submission) else {
        mobility_status(access, device_id, "Mobility login rejected").await;
        return;
    };
    let config = access.config();
    if !configured_mobility_button(&config, &slot)
        || config
            .appearances_for_device(&device_id)
            .any(|binding| binding.line.number == request.line_number())
    {
        mobility_status(access, device_id, "Mobility login rejected").await;
        return;
    }
    let Ok(line) = authenticate_line(&config, request.line_number(), request.credential()) else {
        mobility_status(access, device_id, "Mobility login rejected").await;
        return;
    };
    let configured_instances = config
        .appearances_for_device(&device_id)
        .map(|binding| binding.line_instance)
        .collect::<Vec<_>>();
    drop(config);
    let prepared =
        access
            .shared
            .mobility
            .lock_unpoisoned()
            .prepare_login(slot, line, configured_instances);
    match prepared {
        Ok(MobilityPreparation::Unchanged(_)) => {
            mobility_status(access, device_id, "Mobility already active").await;
        }
        Ok(MobilityPreparation::Transaction(prepared)) => {
            if mobility_appearance_has_calls(access, prepared.previous()) {
                let _ = access.shared.mobility.lock_unpoisoned().abort(&prepared);
                mobility_status(access, device_id, "Mobility line is in use").await;
            } else if apply_mobility_transaction(access, &prepared).await {
                mobility_status(access, device_id, "Mobility login complete").await;
            } else {
                mobility_status(access, device_id, "Mobility login failed").await;
            }
        }
        Err(_) => mobility_status(access, device_id, "Mobility login rejected").await,
    }
}

pub(super) fn mobility_appearance_has_calls(
    access: &Access,
    appearance: Option<&crate::call::mobility::RoamingAppearance>,
) -> bool {
    appearance.is_some_and(|appearance| {
        controller_step(&access.shared.controller, |controller| {
            controller.calls().any(|call| {
                call.device_id == appearance.slot.device_id
                    && call.line_instance == appearance.binding.line_instance
            })
        })
    })
}

pub fn mobility_device_registered(access: &Access, device_id: &DeviceId) -> bool {
    controller_step(&access.shared.controller, |controller| {
        controller.is_registered(device_id)
    })
}

pub(super) struct RuntimeMobilityWriter<'a> {
    access: &'a Access,
}

impl MobilityAppearanceWriter for RuntimeMobilityWriter<'_> {
    type Error = ();

    fn write<'a>(
        &'a mut self,
        appearance: &'a crate::call::mobility::RoamingAppearance,
        install: bool,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), Self::Error>> + Send + 'a>>
    {
        Box::pin(async move {
            if !mobility_device_registered(self.access, &appearance.slot.device_id) {
                return if install { Err(()) } else { Ok(()) };
            }
            self.access
                .phone
                .send_confirmed(PhoneCommand::new(
                    appearance.slot.device_id.clone(),
                    PhoneCommandAction::SetMobilityAppearance {
                        mobility_instance: LineInstance::new(appearance.slot.button_instance),
                        appearance: install.then(|| appearance.binding.appearance.clone()),
                    },
                ))
                .await
                .map_err(|_| ())
        })
    }
}

pub(super) async fn apply_mobility_transaction(
    access: &Access,
    transaction: &PreparedMobilityTransaction,
) -> bool {
    let mut writer = RuntimeMobilityWriter { access };
    if execute_mobility_io(&mut writer, transaction).await.is_err() {
        let _ = access.shared.mobility.lock_unpoisoned().abort(transaction);
        return false;
    }
    let committed = access
        .shared
        .mobility
        .lock_unpoisoned()
        .commit(transaction)
        .is_ok();
    if !committed {
        let _ = rollback_mobility_io(&mut writer, transaction).await;
        let _ = access.shared.mobility.lock_unpoisoned().abort(transaction);
    }
    committed
}

pub(super) async fn restore_mobility_appearances(access: &Access, device_id: &DeviceId) {
    let appearances = access
        .shared
        .mobility
        .lock_unpoisoned()
        .appearances_for_device(device_id)
        .cloned()
        .collect::<Vec<_>>();
    for appearance in appearances {
        let mut writer = RuntimeMobilityWriter { access };
        if writer.write(&appearance, true).await.is_err() {
            ast_log(
                LogLevel::Warning,
                "unable to restore a roaming mobility appearance after registration",
            );
        }
    }
}
