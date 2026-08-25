use super::{
    Access, BTreeSet, BlfEvent, ButtonDefinition, CallState, DeviceId, DeviceState, DndMode,
    HashMap, Instant, LineInstance, LogLevel, MutexExt as _, MwiSubscriptionChange, PhoneCommand,
    PhoneCommandAction, ast_log, controller_step,
};

use crate::asterisk::raw::presence::{NativeMwiSubscription, publish_device_state, subscribe_mwi};

pub fn publish_device_lines(access: &Access, device: &DeviceId) {
    let config = access.config();
    let mut lines = BTreeSet::new();
    for definition in config.device_definitions() {
        if &definition.id == device {
            for line in definition.lines() {
                lines.insert(line.number.clone());
            }
        }
    }
    lines.extend(
        access
            .shared
            .mobility
            .lock_unpoisoned()
            .appearances_for_device(device)
            .map(|appearance| appearance.binding.line.number.clone()),
    );
    for line in lines {
        publish_line(access, &line);
    }
}

pub fn publish_line(access: &Access, line: &str) {
    let state = device_state(access, line);
    let mut published = access.shared.published_line_states.lock_unpoisoned();
    let changed = match published.get_mut(line) {
        Some(previous) if *previous == state => false,
        Some(previous) => {
            *previous = state;
            true
        }
        None => {
            published.insert(line.to_owned(), state);
            true
        }
    };
    drop(published);
    if !changed {
        return;
    }
    publish_device_state(line, state);
}

pub fn install_blf(access: &Access, device_id: &DeviceId) {
    let config = access.config();
    let Some(device) = config.devices.get(device_id) else {
        access
            .shared
            .blf_subscriptions
            .lock_unpoisoned()
            .remove_device(device_id);
        return;
    };
    let mut subscriptions = access.shared.blf_subscriptions.lock_unpoisoned();
    subscriptions.remove_device(device_id);
    for definition in &device.buttons {
        let ButtonDefinition::BlfSpeedDial(definition) = definition else {
            continue;
        };
        let Some(target) = device.blf_targets.get(&definition.instance) else {
            ast_log(
                LogLevel::Warning,
                &format!(
                    "unable to subscribe BLF button {} for {device_id}: no normalized hint target",
                    definition.instance
                ),
            );
            continue;
        };
        if let Err(error) = subscriptions.subscribe(device_id.clone(), definition, target) {
            ast_log(
                LogLevel::Warning,
                &format!(
                    "unable to subscribe BLF button {} for {device_id}: {error}",
                    definition.instance
                ),
            );
        }
    }
}

pub fn handle_blf_event(access: &Access, event: BlfEvent) {
    let mut subscriptions = access.shared.blf_subscriptions.lock_unpoisoned();
    if !subscriptions.is_current(&event) {
        return;
    }
    subscriptions.retry_terminal(&event);
    drop(subscriptions);
    access.spawn_phone(PhoneCommand::new(
        event.device_id,
        PhoneCommandAction::SetBlfStatus {
            instance: LineInstance::new(event.instance),
            state: event.state,
            caller: event.caller,
        },
    ));
}

/// Retries only subscriptions whose previous installation failed or whose
/// Asterisk hint was removed/deactivated. Backoff ownership lives in the
/// subscription registry, so this inexpensive scan is safe on the runtime
/// deadline tick.
pub fn retry_blf(access: &Access, now: Instant) {
    let config = access.config();
    let registered = controller_step(&access.shared.controller, |controller| {
        config
            .devices
            .keys()
            .filter(|device| controller.is_registered(device))
            .cloned()
            .collect::<Vec<_>>()
    });
    let mut subscriptions = access.shared.blf_subscriptions.lock_unpoisoned();
    for device_id in registered {
        let Some(device) = config.devices.get(&device_id) else {
            continue;
        };
        for button in &device.buttons {
            let ButtonDefinition::BlfSpeedDial(definition) = button else {
                continue;
            };
            if !subscriptions.retry_due(&device_id, definition.instance, now) {
                continue;
            }
            let Some(target) = device.blf_targets.get(&definition.instance) else {
                continue;
            };
            if let Err(error) = subscriptions.subscribe(device_id.clone(), definition, target) {
                ast_log(
                    LogLevel::Warning,
                    &format!(
                        "unable to retry BLF button {} for {device_id}: {error}",
                        definition.instance
                    ),
                );
            }
        }
    }
}

pub fn uninstall_device_blf(access: &Access, device_id: &DeviceId) {
    access
        .shared
        .blf_subscriptions
        .lock_unpoisoned()
        .remove_device(device_id);
}

pub fn uninstall_blf(access: &Access) {
    access.shared.blf_subscriptions.lock_unpoisoned().clear();
}

pub struct StagedMwiSubscriptions {
    pub subscriptions: HashMap<String, NativeMwiSubscription>,
}

impl StagedMwiSubscriptions {
    pub fn new(changes: &[MwiSubscriptionChange]) -> Result<Self, String> {
        let mut staged = Self {
            subscriptions: HashMap::new(),
        };
        for change in changes {
            let subscription =
                subscribe_mwi(change.line.clone(), change.mailbox.clone()).map_err(|error| {
                    format!(
                        "unable to stage MWI subscription for line {}: {error}",
                        change.line
                    )
                })?;
            staged
                .subscriptions
                .insert(change.line.clone(), subscription);
        }
        Ok(staged)
    }

    pub fn commit(mut self, access: &Access, removed: &[MwiSubscriptionChange]) {
        let old = {
            let mut live = access.shared.mwi_subscriptions.lock_unpoisoned();
            let old = removed
                .iter()
                .filter_map(|change| live.remove(&change.line))
                .collect::<Vec<_>>();
            live.extend(self.subscriptions.drain());
            old
        };
        drop(old);
    }
}

pub fn install_mwi(access: &Access) {
    let config = access.config();
    let subscriptions: Vec<_> = config
        .lines
        .values()
        .filter_map(|line| {
            line.mailbox
                .as_ref()
                .map(|mailbox| (line.number.clone(), mailbox.clone()))
        })
        .collect();
    let mut installed = HashMap::new();
    for (line, mailbox) in subscriptions {
        match subscribe_mwi(line.clone(), mailbox.clone()) {
            Ok(subscription) => {
                installed.insert(line, subscription);
            }
            Err(error) => {
                ast_log(
                    LogLevel::Warning,
                    &format!("unable to subscribe to mailbox {mailbox} for SCCP/{line}: {error}"),
                );
            }
        }
    }
    *access.shared.mwi_subscriptions.lock_unpoisoned() = installed;
}

pub fn uninstall_mwi(access: &Access) {
    drop(std::mem::take(
        &mut *access.shared.mwi_subscriptions.lock_unpoisoned(),
    ));
}

pub fn device_state(access: &Access, line: &str) -> DeviceState {
    let config = access.config();
    let mut appearances = config
        .appearances_for_line(line)
        .map(|binding| binding.device_id.clone())
        .collect::<BTreeSet<_>>();
    appearances.extend(
        access
            .shared
            .mobility
            .lock_unpoisoned()
            .appearances_for_line(line)
            .map(|appearance| appearance.binding.device_id.clone()),
    );
    let (registered_dnd, states) = controller_step(&access.shared.controller, |controller| {
        let registered_dnd = appearances
            .iter()
            .filter(|device| controller.is_registered(device))
            .map(|device| {
                controller
                    .feature_state(device)
                    .map_or(DndMode::Off, |features| features.dnd)
            })
            .collect::<Vec<_>>();
        let states = controller
            .calls()
            .filter(|call| call.line == line)
            .map(|call| call.state)
            .collect::<Vec<_>>();
        (registered_dnd, states)
    });
    aggregate_device_state(!appearances.is_empty(), &registered_dnd, &states)
}

fn aggregate_device_state(
    has_appearance: bool,
    registered_dnd: &[DndMode],
    states: &[CallState],
) -> DeviceState {
    if !has_appearance {
        return DeviceState::Removed;
    }
    if registered_dnd.is_empty() {
        return DeviceState::Unavailable;
    }

    let ringing = states.contains(&CallState::Ringing);
    let on_hold = states
        .iter()
        .any(|state| matches!(state, CallState::Held | CallState::SharedHeld));
    let in_use = states.iter().any(|state| {
        matches!(
            state,
            CallState::Collecting
                | CallState::PickupCollecting
                | CallState::Calling
                | CallState::Connected
                | CallState::Parking
                | CallState::Retrieving
                | CallState::RemoteInUse
                | CallState::Barged
                | CallState::TransferCollecting
        )
    });

    if ringing && (in_use || on_hold) {
        DeviceState::RingInUse
    } else if ringing {
        DeviceState::Ringing
    } else if in_use {
        DeviceState::InUse
    } else if on_hold {
        DeviceState::OnHold
    } else if registered_dnd.iter().all(|mode| *mode == DndMode::Reject) {
        DeviceState::Busy
    } else {
        DeviceState::NotInUse
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(registered_dnd: &[DndMode], calls: &[CallState]) -> DeviceState {
        aggregate_device_state(true, registered_dnd, calls)
    }

    #[test]
    fn absent_and_unregistered_lines_have_distinct_states() {
        assert_eq!(
            aggregate_device_state(false, &[], &[]),
            DeviceState::Removed
        );
        assert_eq!(state(&[], &[]), DeviceState::Unavailable);
    }

    #[test]
    fn idle_dnd_is_busy_only_when_every_registered_appearance_rejects() {
        assert_eq!(state(&[DndMode::Reject], &[]), DeviceState::Busy);
        assert_eq!(
            state(&[DndMode::Reject, DndMode::Reject], &[]),
            DeviceState::Busy
        );
        assert_eq!(state(&[DndMode::Silent], &[]), DeviceState::NotInUse);
        assert_eq!(
            state(&[DndMode::Reject, DndMode::Off], &[]),
            DeviceState::NotInUse
        );
        assert_eq!(
            state(&[DndMode::Reject, DndMode::Silent], &[]),
            DeviceState::NotInUse
        );
    }

    #[test]
    fn call_activity_takes_precedence_over_idle_dnd() {
        assert_eq!(
            state(&[DndMode::Reject], &[CallState::Connected]),
            DeviceState::InUse
        );
        assert_eq!(
            state(&[DndMode::Reject], &[CallState::Held]),
            DeviceState::OnHold
        );
    }

    #[test]
    fn active_held_and_ringing_calls_publish_rich_states() {
        assert_eq!(
            state(&[DndMode::Off], &[CallState::Ringing]),
            DeviceState::Ringing
        );
        assert_eq!(
            state(&[DndMode::Off], &[CallState::SharedHeld]),
            DeviceState::OnHold
        );
        assert_eq!(
            state(&[DndMode::Off], &[CallState::Held, CallState::Connected]),
            DeviceState::InUse
        );
        assert_eq!(
            state(&[DndMode::Off], &[CallState::Ringing, CallState::Connected]),
            DeviceState::RingInUse
        );
        assert_eq!(
            state(
                &[DndMode::Off],
                &[CallState::Ringing, CallState::SharedHeld]
            ),
            DeviceState::RingInUse
        );
    }

    #[test]
    fn every_live_nonterminal_call_state_is_accounted_for() {
        for call in [
            CallState::Collecting,
            CallState::PickupCollecting,
            CallState::Calling,
            CallState::Connected,
            CallState::Parking,
            CallState::Retrieving,
            CallState::RemoteInUse,
            CallState::Barged,
            CallState::TransferCollecting,
        ] {
            assert_eq!(state(&[DndMode::Off], &[call]), DeviceState::InUse);
        }
        assert_eq!(
            state(&[DndMode::Off], &[CallState::Ended]),
            DeviceState::NotInUse
        );
    }
}
