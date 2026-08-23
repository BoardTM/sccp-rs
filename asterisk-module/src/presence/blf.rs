//! Runtime ownership and race-safe delivery for monitored speed-dial hints.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use sccp_protocol::{BlfCallerInfo, BlfSpeedDialDefinition, BlfState, DeviceId};
use thiserror::Error;
use tokio::sync::mpsc;

use crate::presence::hints::{ExtensionState, Hint, HintUpdate, HintUpdateReason};

pub type HintCallback = Arc<dyn Fn(HintUpdate) + Send + Sync + 'static>;

/// Abstracts hint lookup/subscription so lifecycle and race handling can be
/// verified without loading the native module.
pub trait HintProvider {
    type Subscription: Send + 'static;
    type Error: fmt::Display;

    fn lookup(&self, context: &str, extension: &str) -> Result<Option<Hint>, Self::Error>;
    fn subscribe(
        &self,
        context: &str,
        extension: &str,
        callback: HintCallback,
    ) -> Result<Self::Subscription, Self::Error>;
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct BlfKey {
    device_id: DeviceId,
    instance: u32,
}

/// A normalized handset update tagged with its subscription generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlfEvent {
    pub device_id: DeviceId,
    pub instance: u32,
    pub number: String,
    pub label: String,
    pub state: BlfState,
    pub caller: Option<BlfCallerInfo>,
    generation: u64,
}

struct Entry<Subscription> {
    generation: u64,
    _subscription: Subscription,
}

#[derive(Default)]
struct InitialGate {
    initializing: bool,
    pending: Option<HintUpdate>,
}

/// Owns all active monitored-button subscriptions.
pub struct BlfSubscriptions<Provider: HintProvider> {
    provider: Provider,
    events: mpsc::UnboundedSender<BlfEvent>,
    entries: HashMap<BlfKey, Entry<Provider::Subscription>>,
    next_generation: u64,
}

impl<Provider: HintProvider> BlfSubscriptions<Provider> {
    pub fn new(provider: Provider, events: mpsc::UnboundedSender<BlfEvent>) -> Self {
        Self {
            provider,
            events,
            entries: HashMap::new(),
            next_generation: 1,
        }
    }

    /// Installs or replaces one monitored button and emits exactly one initial
    /// snapshot before live updates can pass its initialization gate.
    pub fn subscribe(
        &mut self,
        device_id: DeviceId,
        definition: &BlfSpeedDialDefinition,
    ) -> Result<(), BlfSubscriptionError> {
        let (extension, context) = parse_hint(&definition.hint)?;
        let key = BlfKey {
            device_id,
            instance: definition.instance,
        };
        self.entries.remove(&key);

        let generation = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        let gate = Arc::new(Mutex::new(InitialGate {
            initializing: true,
            pending: None,
        }));
        let events = self.events.clone();
        let callback_key = key.clone();
        let number = definition.number.clone();
        let label = definition.display_name.clone();
        let callback_gate = Arc::clone(&gate);
        let callback: HintCallback = Arc::new(move |update| {
            let mut gate = callback_gate
                .lock()
                .expect("BLF initialization lock poisoned");
            if gate.initializing {
                gate.pending = Some(update);
                return;
            }
            let _ = events.send(normalize_event(
                &callback_key,
                generation,
                &number,
                &label,
                update,
            ));
        });

        let subscription = self
            .provider
            .subscribe(context, extension, callback)
            .map_err(|error| BlfSubscriptionError::Provider(error.to_string()))?;
        self.entries.insert(
            key.clone(),
            Entry {
                generation,
                _subscription: subscription,
            },
        );

        let snapshot = match self.provider.lookup(context, extension) {
            Ok(Some(hint)) => HintUpdate {
                context: context.to_owned(),
                extension: extension.to_owned(),
                state: hint.state,
                reason: HintUpdateReason::Device,
                caller: None,
            },
            Ok(None) => unknown_update(context, extension),
            Err(error) => {
                self.entries.remove(&key);
                return Err(BlfSubscriptionError::Provider(error.to_string()));
            }
        };

        let mut gate = gate.lock().expect("BLF initialization lock poisoned");
        let initial = gate.pending.take().unwrap_or(snapshot);
        let _ = self.events.send(normalize_event(
            &key,
            generation,
            &definition.number,
            &definition.display_name,
            initial,
        ));
        gate.initializing = false;
        Ok(())
    }

    /// Returns whether an event belongs to the currently installed generation.
    pub fn is_current(&self, event: &BlfEvent) -> bool {
        self.entries
            .get(&BlfKey {
                device_id: event.device_id.clone(),
                instance: event.instance,
            })
            .is_some_and(|entry| entry.generation == event.generation)
    }

    /// Unsubscribes every monitored button owned by one device.
    pub fn remove_device(&mut self, device_id: &DeviceId) {
        self.entries.retain(|key, _| &key.device_id != device_id);
    }

    /// Unsubscribes every monitored button, including during reload/unload.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum BlfSubscriptionError {
    #[error("invalid BLF hint {0:?}; expected extension@context")]
    InvalidHint(String),

    #[error("hint service failed: {0}")]
    Provider(String),
}

pub fn map_extension_state(state: ExtensionState) -> BlfState {
    if state.raw() < 0 {
        BlfState::Unknown
    } else if state.contains(ExtensionState::UNAVAILABLE) {
        BlfState::Unavailable
    } else if state.contains(ExtensionState::RINGING) {
        BlfState::Ringing
    } else if state.contains(ExtensionState::ON_HOLD) {
        BlfState::Held
    } else if state.contains(ExtensionState::BUSY) || state.contains(ExtensionState::IN_USE) {
        BlfState::Busy
    } else if state.contains(ExtensionState::IDLE) {
        BlfState::Idle
    } else {
        BlfState::Unknown
    }
}

fn parse_hint(value: &str) -> Result<(&str, &str), BlfSubscriptionError> {
    let Some((extension, context)) = value.split_once('@') else {
        return Err(BlfSubscriptionError::InvalidHint(value.to_owned()));
    };
    if extension.is_empty() || context.is_empty() || context.contains('@') {
        return Err(BlfSubscriptionError::InvalidHint(value.to_owned()));
    }
    Ok((extension, context))
}

fn unknown_update(context: &str, extension: &str) -> HintUpdate {
    HintUpdate {
        context: context.to_owned(),
        extension: extension.to_owned(),
        state: ExtensionState::DEACTIVATED,
        reason: HintUpdateReason::Device,
        caller: None,
    }
}

fn normalize_event(
    key: &BlfKey,
    generation: u64,
    number: &str,
    label: &str,
    update: HintUpdate,
) -> BlfEvent {
    let state = map_extension_state(update.state);
    let caller = matches!(state, BlfState::Ringing | BlfState::Busy | BlfState::Held)
        .then(|| update.caller)
        .flatten()
        .filter(|caller| caller.presentation_allowed())
        .map(|caller| BlfCallerInfo {
            name: caller.name.unwrap_or_default(),
            number: caller.number.unwrap_or_default(),
        });
    BlfEvent {
        device_id: key.device_id.clone(),
        instance: key.instance,
        number: number.to_owned(),
        label: label.to_owned(),
        state,
        caller,
        generation,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::presence::hints::HintCaller;

    use super::*;

    #[derive(Clone)]
    struct FakeHints {
        state: Arc<Mutex<FakeState>>,
        drops: Arc<AtomicUsize>,
    }

    struct FakeState {
        snapshot: ExtensionState,
        update_during_lookup: Option<HintUpdate>,
        callback: Option<HintCallback>,
    }

    struct FakeSubscription(Arc<AtomicUsize>);

    impl Drop for FakeSubscription {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl HintProvider for FakeHints {
        type Subscription = FakeSubscription;
        type Error = &'static str;

        fn lookup(&self, _context: &str, _extension: &str) -> Result<Option<Hint>, Self::Error> {
            let (callback, update, snapshot) = {
                let mut state = self.state.lock().unwrap();
                (
                    state.callback.clone(),
                    state.update_during_lookup.take(),
                    state.snapshot,
                )
            };
            if let (Some(callback), Some(update)) = (callback, update) {
                callback(update);
            }
            Ok(Some(Hint {
                devices: "PJSIP/4000".into(),
                name: "Desk".into(),
                state: snapshot,
            }))
        }

        fn subscribe(
            &self,
            _context: &str,
            _extension: &str,
            callback: HintCallback,
        ) -> Result<Self::Subscription, Self::Error> {
            self.state.lock().unwrap().callback = Some(callback);
            Ok(FakeSubscription(Arc::clone(&self.drops)))
        }
    }

    fn update(state: ExtensionState) -> HintUpdate {
        HintUpdate {
            context: "internal".into(),
            extension: "4000".into(),
            state,
            reason: HintUpdateReason::Device,
            caller: None,
        }
    }

    fn setup(
        snapshot: ExtensionState,
        during_lookup: Option<HintUpdate>,
    ) -> (
        BlfSubscriptions<FakeHints>,
        mpsc::UnboundedReceiver<BlfEvent>,
        Arc<Mutex<FakeState>>,
        Arc<AtomicUsize>,
    ) {
        let state = Arc::new(Mutex::new(FakeState {
            snapshot,
            update_during_lookup: during_lookup,
            callback: None,
        }));
        let drops = Arc::new(AtomicUsize::new(0));
        let provider = FakeHints {
            state: Arc::clone(&state),
            drops: Arc::clone(&drops),
        };
        let (tx, rx) = mpsc::unbounded_channel();
        (BlfSubscriptions::new(provider, tx), rx, state, drops)
    }

    fn definition() -> BlfSpeedDialDefinition {
        BlfSpeedDialDefinition {
            instance: 2,
            number: "4000".into(),
            display_name: "Desk".into(),
            hint: "4000@internal".into(),
        }
    }

    #[test]
    fn maps_every_required_extension_state() {
        let cases = [
            (ExtensionState::IDLE, BlfState::Idle),
            (ExtensionState::RINGING, BlfState::Ringing),
            (ExtensionState::IN_USE, BlfState::Busy),
            (ExtensionState::BUSY, BlfState::Busy),
            (ExtensionState::ON_HOLD, BlfState::Held),
            (ExtensionState::UNAVAILABLE, BlfState::Unavailable),
            (ExtensionState::REMOVED, BlfState::Unknown),
        ];
        for (extension, blf) in cases {
            assert_eq!(map_extension_state(extension), blf);
        }
        assert_eq!(
            map_extension_state(ExtensionState::from_raw(
                ExtensionState::RINGING.raw() | ExtensionState::IN_USE.raw()
            )),
            BlfState::Ringing
        );
    }

    #[test]
    fn update_during_initial_lookup_wins_over_the_stale_snapshot() {
        let (mut subscriptions, mut events, _, _) =
            setup(ExtensionState::IDLE, Some(update(ExtensionState::ON_HOLD)));
        subscriptions
            .subscribe(DeviceId::new("SEP001122334455").unwrap(), &definition())
            .unwrap();

        let event = events.try_recv().unwrap();
        assert_eq!(event.state, BlfState::Held);
        assert!(events.try_recv().is_err());
        assert!(subscriptions.is_current(&event));
    }

    #[test]
    fn replacing_and_removing_a_device_invalidates_queued_generations() {
        let (mut subscriptions, mut events, _, drops) = setup(ExtensionState::IDLE, None);
        let device = DeviceId::new("SEP001122334455").unwrap();
        subscriptions
            .subscribe(device.clone(), &definition())
            .unwrap();
        let old = events.try_recv().unwrap();

        subscriptions
            .subscribe(device.clone(), &definition())
            .unwrap();
        let current = events.try_recv().unwrap();
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(!subscriptions.is_current(&old));
        assert!(subscriptions.is_current(&current));

        subscriptions.remove_device(&device);
        assert_eq!(drops.load(Ordering::SeqCst), 2);
        assert!(!subscriptions.is_current(&current));
        assert!(subscriptions.is_empty());
    }

    #[test]
    fn clear_unsubscribes_every_device_for_reload_or_unload() {
        let (mut subscriptions, mut events, _, drops) = setup(ExtensionState::IDLE, None);
        subscriptions
            .subscribe(DeviceId::new("SEP001122334455").unwrap(), &definition())
            .unwrap();
        let mut second = definition();
        second.instance = 3;
        subscriptions
            .subscribe(DeviceId::new("SEP00AABBCCDDEE").unwrap(), &second)
            .unwrap();
        let first_event = events.try_recv().unwrap();
        let second_event = events.try_recv().unwrap();

        subscriptions.clear();

        assert_eq!(drops.load(Ordering::SeqCst), 2);
        assert!(subscriptions.is_empty());
        assert!(!subscriptions.is_current(&first_event));
        assert!(!subscriptions.is_current(&second_event));
    }

    #[test]
    fn restricted_caller_information_is_removed_before_delivery() {
        let key = BlfKey {
            device_id: DeviceId::new("SEP001122334455").unwrap(),
            instance: 2,
        };
        let allowed = normalize_event(
            &key,
            1,
            "4000",
            "Desk",
            HintUpdate {
                caller: Some(HintCaller {
                    name: Some("Taylor".into()),
                    number: Some("5550100".into()),
                    name_presentation: 0,
                    number_presentation: 1,
                }),
                ..update(ExtensionState::RINGING)
            },
        );
        assert_eq!(
            allowed.caller,
            Some(BlfCallerInfo {
                name: "Taylor".into(),
                number: "5550100".into(),
            })
        );

        let restricted = normalize_event(
            &key,
            1,
            "4000",
            "Desk",
            HintUpdate {
                caller: Some(HintCaller {
                    name: Some("Taylor".into()),
                    number: Some("5550100".into()),
                    name_presentation: 0,
                    number_presentation: 0x20,
                }),
                ..update(ExtensionState::RINGING)
            },
        );
        assert_eq!(restricted.caller, None);
    }
}
