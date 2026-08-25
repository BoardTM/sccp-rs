//! Asterisk Manager Interface contracts, registration, and runtime services.
//!
//! # Registered actions
//!
//! - [`inventory`] owns `SCCPShowDevices`, `SCCPShowDevice`, `SCCPShowLines`,
//!   `SCCPShowLine`, `SCCPShowAppearances`, `SCCPShowAppearance`,
//!   `SCCPShowButtons`, and `SCCPShowButton`.
//! - [`runtime`] owns `SCCPShowChannels`, `SCCPShowChannel`,
//!   `SCCPShowMediaStreams`, `SCCPShowMediaStream`,
//!   `SCCPShowMediaStatistics`, `SCCPShowMediaStatistic`,
//!   `SCCPShowConferences`, `SCCPShowConference`,
//!   `SCCPShowConferenceParticipants`, and `SCCPShowConferenceParticipant`.
//! - [`features`] owns `SCCPDeviceSetDND` and `SCCPLineForwardUpdate`.
//! - [`controls`] owns bounded message, restart, answer, hangup, and originate
//!   actions; [`services`] owns microphone, recording, parking, and conference
//!   controls.
//!
//! Requests reject unknown, duplicate, control-text, NUL, and credential-like
//! fields. Inventory lists are capped at 40 items. Live lists cap calls at 40,
//! media/statistics at 24, conferences at 36, and participants at 16. Both
//! surfaces cap a response at 512 fields, 4096 bytes per value, and 64 KiB
//! aggregate. Selectors and output order use stable typed identifiers.
//!
//! Restricted party identity, credentials, opaque alarm/XML payloads, RTP
//! addresses, and raw channel variables never enter a management snapshot or
//! `Debug` output. Events are the bounded `SCCPRegistration`, `SCCPAlarm`,
//! `SCCPFeature`, `SCCPMedia`, and `SCCPCall` schemas: at most 16 fields, 1024
//! bytes per value, and 8 KiB aggregate. Publication happens after accepted
//! controller state, without holding the controller lock.
//!
//! Every registration group is RAII-owned. Partial registration rolls back,
//! duplicate action names fail deterministically, callback panics are contained,
//! and unregister/unload invalidates new work and waits for in-flight callbacks.

pub mod cli;
mod cli_support;
pub mod controls;
pub mod diagnostics;
pub mod events;
pub mod features;
pub mod inventory;
pub mod manager;
pub mod runtime;
pub mod services;
