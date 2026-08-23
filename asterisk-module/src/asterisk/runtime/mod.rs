//! Asterisk runtime composition support.

use super::*;

mod backend;
mod channel;
mod cli;
mod diagnostics;
mod lifecycle;
mod management;
mod media;
mod native_support;
mod presence;
mod services;
pub mod sync;

pub use backend::*;
pub use channel::*;
pub use cli::*;
pub use diagnostics::*;
pub use lifecycle::*;
pub use management::*;
pub use media::*;
pub use native_support::*;
pub use presence::*;
pub use services::*;

#[derive(Default)]
pub(super) struct RuntimeRecordings {
    sessions: OwnedRecordingSessions<backend::AnchoredRecordingSession>,
}

pub(super) fn retarget_station_to_anchor(access: &Access, call: &DirectMediaCall) -> bool {
    media::retarget_to_anchor(access, call)
}
