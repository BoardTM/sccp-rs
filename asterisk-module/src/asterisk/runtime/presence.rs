use super::super::*;

use crate::asterisk::raw::presence::NativeMwiSubscription;

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
    raw::presence::publish_device_state(line, state);
}

pub fn install_blf(access: &Access, device_id: &DeviceId) {
    let definitions = access
        .config()
        .devices
        .get(device_id)
        .map(|device| device.buttons.clone())
        .unwrap_or_default();
    let mut subscriptions = access.shared.blf_subscriptions.lock_unpoisoned();
    subscriptions.remove_device(device_id);
    for definition in definitions {
        let ButtonDefinition::BlfSpeedDial(definition) = definition else {
            continue;
        };
        if let Err(error) = subscriptions.subscribe(device_id.clone(), &definition) {
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
    if !access
        .shared
        .blf_subscriptions
        .lock_unpoisoned()
        .is_current(&event)
    {
        return;
    }
    access.spawn_phone(PhoneCommand::new(
        event.device_id,
        PhoneCommandAction::SetBlfStatus {
            instance: LineInstance::new(event.instance),
            number: event.number,
            label: event.label,
            state: event.state,
            caller: event.caller,
        },
    ));
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
                raw::presence::subscribe_mwi(change.line.clone(), change.mailbox.clone()).map_err(
                    |error| {
                        format!(
                            "unable to stage MWI subscription for line {}: {error}",
                            change.line
                        )
                    },
                )?;
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
        match raw::presence::subscribe_mwi(line.clone(), mailbox.clone()) {
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
    let Some(binding) = access.config().line(line).cloned() else {
        return DeviceState::Unknown;
    };
    let (registered, states) = controller_step(&access.shared.controller, |controller| {
        let registered = controller.is_registered(&binding.device_id);
        let states = controller
            .calls()
            .filter(|call| call.line == line)
            .map(|call| call.state)
            .collect::<Vec<_>>();
        (registered, states)
    });
    if !registered {
        return DeviceState::Unavailable;
    }
    if states.iter().any(|state| *state == CallState::Ringing) {
        DeviceState::Ringing
    } else if states.iter().any(|state| {
        matches!(
            state,
            CallState::Collecting
                | CallState::Calling
                | CallState::Connected
                | CallState::Held
                | CallState::TransferCollecting
        )
    }) {
        DeviceState::InUse
    } else {
        DeviceState::NotInUse
    }
}
