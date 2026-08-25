//! Invariant validation grouped by each controller-owned registry.

mod call_registry;
mod conference;
mod device_registry;
mod ownership;
mod shared_control;
mod transfer;

use super::Controller;

pub(super) fn validate(controller: &Controller) -> Option<String> {
    ownership::error(controller)
        .or_else(|| device_registry::error(controller))
        .or_else(|| call_registry::error(controller))
        .or_else(|| shared_control::error(controller))
        .or_else(|| conference::error(controller))
        .or_else(|| transfer::error(controller))
}
