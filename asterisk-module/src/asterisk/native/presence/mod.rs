//! Raw Asterisk presence resources.

mod hints;
mod mwi;

pub use hints::{NativeHintAdapter, NativeHintSubscription};
pub use mwi::{NativeMwiSubscription, subscribe_mwi};

// Transitional composition route while the remaining presence fragment is
// split; the implementation and ownership live in the system edge.
pub use super::system::publish_device_state;
