//! Typed composition adapter for Asterisk extension hints.

use std::sync::Arc;

use crate::config::HintTarget;
use crate::presence::blf::{HintCallback, HintProvider, HintSnapshot};
use crate::presence::hints::{Hint, HintError, HintUpdate, dispatch_lookup, dispatch_subscribe};

use crate::asterisk::raw::presence::{NativeHintAdapter, NativeHintSubscription};

#[derive(Clone, Copy, Debug, Default)]
pub struct AsteriskHints;

impl AsteriskHints {
    pub const fn new() -> Self {
        Self
    }

    pub fn lookup(&self, target: &HintTarget) -> Result<Option<HintSnapshot>, HintError> {
        dispatch_lookup(&NativeHintAdapter, target.context(), target.extension())
            .map(|hint| hint.map(|hint| snapshot_from_hint(target, hint)))
    }

    pub fn subscribe<F>(
        &self,
        target: &HintTarget,
        callback: F,
    ) -> Result<HintSubscription, HintError>
    where
        F: Fn(HintSnapshot) + Send + Sync + 'static,
    {
        let callback_target = target.clone();
        let native = dispatch_subscribe(
            &NativeHintAdapter,
            target.context(),
            target.extension(),
            Arc::new(move |update| callback(snapshot_from_update(&callback_target, update))),
        )?;
        Ok(HintSubscription { _native: native })
    }
}

fn snapshot_from_hint(target: &HintTarget, hint: Hint) -> HintSnapshot {
    // `devices` is intentionally diagnostic only. Asterisk owns aggregation
    // and may compose any registered device-state technology in this string.
    let Hint { state, .. } = hint;
    HintSnapshot {
        target: target.clone(),
        state,
        reason: crate::presence::hints::HintUpdateReason::Device,
        caller: None,
    }
}

fn snapshot_from_update(target: &HintTarget, update: HintUpdate) -> HintSnapshot {
    HintSnapshot {
        target: target.clone(),
        state: update.state,
        reason: update.reason,
        caller: update.caller,
    }
}

/// An active subscription whose typed native owner drains Asterisk's watcher
/// reference before releasing callback storage.
pub struct HintSubscription {
    _native: NativeHintSubscription,
}

impl HintProvider for AsteriskHints {
    type Subscription = HintSubscription;
    type Error = HintError;

    fn lookup(&self, target: &HintTarget) -> Result<Option<HintSnapshot>, Self::Error> {
        Self::lookup(self, target)
    }

    fn subscribe(
        &self,
        target: &HintTarget,
        callback: HintCallback,
    ) -> Result<Self::Subscription, Self::Error> {
        Self::subscribe(self, target, move |update| callback(update))
    }
}

#[cfg(test)]
mod tests {
    use crate::presence::hints::ExtensionState;

    use super::*;

    #[test]
    fn semantic_snapshot_is_technology_opaque() {
        let target = HintTarget::parse("4000@internal").unwrap();
        for devices in [
            "PJSIP/alice",
            "SIP/bob",
            "SCCP/4000",
            "Custom:queue-ready",
            "PJSIP/alice&SIP/bob&SCCP/4000&Custom:queue-ready",
        ] {
            let snapshot = snapshot_from_hint(
                &target,
                Hint {
                    devices: devices.into(),
                    name: "Desk".into(),
                    state: ExtensionState::RINGING,
                },
            );
            assert_eq!(snapshot.target, target);
            assert_eq!(snapshot.state, ExtensionState::RINGING);
            assert_eq!(
                snapshot.reason,
                crate::presence::hints::HintUpdateReason::Device
            );
            assert_eq!(snapshot.caller, None);
        }
    }
}
