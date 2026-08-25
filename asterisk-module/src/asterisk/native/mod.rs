//! Raw Asterisk edge grouped by the native resource each module owns.

pub mod bridge;
pub mod channel;
pub mod config;
pub mod dialplan;
pub mod handles;
pub mod http;
#[cfg(feature = "live-asterisk-tests")]
#[path = "../../../live-tests/bridge.rs"]
mod live_bridge_tests;
pub mod manager;
mod ownership;
pub mod presence;
pub mod realtime;
pub mod recording;
pub mod registry;
pub mod system;

#[cfg(feature = "live-asterisk-tests")]
pub(super) fn live_bridge_cli_entry() -> crate::asterisk::sys::ast_cli_entry {
    live_bridge_tests::cli_entry()
}
