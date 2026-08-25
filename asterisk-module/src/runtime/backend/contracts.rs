//! Backend service ownership and shared error contracts.

use crate::media::recording::RecordingProvider;
use crate::presence::blf::HintProvider;
use crate::state::persistence::PersistentStore;

/// Direct backend services whose return values, callbacks, or owned handles do
/// not fit the queued effect executor.
pub trait PbxServiceCapabilities {
    type Persistence: PersistentStore;
    type Hints: HintProvider;
    type Recordings: RecordingProvider;

    fn persistence(&self) -> &Self::Persistence;
    fn hints(&self) -> &Self::Hints;
    fn recordings(&self) -> &Self::Recordings;
}

/// One error domain shared by the PBX capabilities implemented by a backend.
pub trait PbxBackendError {
    type Error;
}
