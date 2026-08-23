//! Typed composition adapter for Asterisk realtime configuration.

use crate::config::realtime::{
    RealtimeConfigurationSource, RealtimeError, RealtimeLoad, RealtimePredicate,
};

use super::super::raw;

#[derive(Clone, Copy, Debug, Default)]
pub struct AsteriskRealtime;

impl AsteriskRealtime {
    pub const fn new() -> Self {
        Self
    }
}

impl RealtimeConfigurationSource for AsteriskRealtime {
    fn load_many(
        &self,
        family: &str,
        predicates: &[RealtimePredicate],
    ) -> Result<RealtimeLoad, RealtimeError> {
        raw::realtime::load_realtime(family, predicates)
    }
}
