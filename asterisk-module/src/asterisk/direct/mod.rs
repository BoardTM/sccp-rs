//! Direct Asterisk ABI adapter.
//!
//! The generated [`crate::asterisk::sys`] module is intentionally raw.
//! This module is the only place that turns those pointers and callbacks into
//! Rust ownership.  During the single-branch migration, individual domains are
//! moved here before the corresponding repository-owned C source is removed.

pub mod channel_driver;
pub mod handles;
pub mod module_info;
