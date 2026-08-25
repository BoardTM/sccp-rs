//! Typed composition adapter for Asterisk extension hints.

use crate::config::HintTarget;
use crate::presence::blf::{HintCallback, HintProvider, HintSnapshot};
use crate::presence::hints::{HintError, dispatch_lookup, dispatch_subscribe};

use crate::asterisk::raw::presence::{NativeHintAdapter, NativeHintSubscription};

#[derive(Clone, Copy, Debug, Default)]
pub struct AsteriskHints;

impl AsteriskHints {
    pub const fn new() -> Self {
        Self
    }

    pub fn lookup(&self, target: &HintTarget) -> Result<Option<HintSnapshot>, HintError> {
        dispatch_lookup(&NativeHintAdapter, target)
    }

    pub fn subscribe<F>(
        &self,
        target: &HintTarget,
        callback: F,
    ) -> Result<HintSubscription, HintError>
    where
        F: Fn(HintSnapshot) + Send + Sync + 'static,
    {
        let native = dispatch_subscribe(&NativeHintAdapter, target, std::sync::Arc::new(callback))?;
        Ok(HintSubscription { _native: native })
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
