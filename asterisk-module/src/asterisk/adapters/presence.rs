//! Typed composition adapter for Asterisk extension hints.

use std::sync::Arc;

use crate::presence::blf::{HintCallback, HintProvider};
use crate::presence::hints::{Hint, HintError, HintUpdate, dispatch_lookup, dispatch_subscribe};

use crate::asterisk::raw::presence::{NativeHintAdapter, NativeHintSubscription};

#[derive(Clone, Copy, Debug, Default)]
pub struct AsteriskHints;

impl AsteriskHints {
    pub const fn new() -> Self {
        Self
    }

    pub fn lookup(&self, context: &str, extension: &str) -> Result<Option<Hint>, HintError> {
        dispatch_lookup(&NativeHintAdapter, context, extension)
    }

    pub fn subscribe<F>(
        &self,
        context: &str,
        extension: &str,
        callback: F,
    ) -> Result<HintSubscription, HintError>
    where
        F: Fn(HintUpdate) + Send + Sync + 'static,
    {
        let native =
            dispatch_subscribe(&NativeHintAdapter, context, extension, Arc::new(callback))?;
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

    fn lookup(&self, context: &str, extension: &str) -> Result<Option<Hint>, Self::Error> {
        Self::lookup(self, context, extension)
    }

    fn subscribe(
        &self,
        context: &str,
        extension: &str,
        callback: HintCallback,
    ) -> Result<Self::Subscription, Self::Error> {
        Self::subscribe(self, context, extension, move |update| callback(update))
    }
}
