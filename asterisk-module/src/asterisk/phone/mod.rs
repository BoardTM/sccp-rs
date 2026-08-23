//! Handset-facing SCCP event handling and feature orchestration.
//!
//! The child modules separate core call signaling, parking and pickup,
//! per-device feature state, and conference behavior while sharing only the
//! private Asterisk composition context from the parent module.

use super::*;

mod calls;
mod conference;
mod features;
mod parking;

pub use calls::*;
pub use conference::*;
pub use features::*;
pub use parking::*;
