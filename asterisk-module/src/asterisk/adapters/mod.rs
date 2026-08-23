//! Rust-native implementations of domain ports backed by Asterisk.

pub mod bridging;
mod channel_metadata;
mod completion;
mod dialplan;
mod http;
mod manager;
pub mod parking;
mod party;
mod persistence;
mod presence;
mod realtime;
mod recording;
mod registration;

pub use bridging::AsteriskCallFeatures;
pub use channel_metadata::AsteriskChannelMetadata;
pub use completion::AsteriskCallCompletion;
pub use dialplan::{AsteriskDialplan, DialplanRegistration};
pub use http::{AsteriskHttp, HttpRegistration};
pub use manager::{AsteriskManager, ManagerActionRegistration};
pub use parking::AsteriskParking;
pub use party::AsteriskPartyUpdates;
pub use persistence::AsteriskDatabase;
pub use presence::AsteriskHints;
pub use realtime::AsteriskRealtime;
pub use recording::{AsteriskRecording, RecordingSession};
pub use registration::{AsteriskRegistrationExtensions, config_directory};
