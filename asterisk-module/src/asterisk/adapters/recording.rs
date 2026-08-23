//! Asterisk-backed recording adapter and owning session handle.

use std::sync::Arc;

use crate::asterisk::raw::recording::{NativeRecordingSession, start_recording};
use crate::media::recording::{
    RecordingDirection, RecordingError, RecordingEvent, RecordingSessionControl, RecordingState,
    validate_filename, validate_text,
};
use crate::pbx::party::AsteriskChannel;

#[derive(Clone, Copy, Debug, Default)]
pub struct AsteriskRecording;

impl AsteriskRecording {
    pub const fn new() -> Self {
        Self
    }

    pub fn start<F>(
        &self,
        channel: &AsteriskChannel<'_>,
        filename: &str,
        options: &str,
        callback: F,
    ) -> Result<RecordingSession, RecordingError>
    where
        F: Fn(RecordingEvent) + Send + Sync + 'static,
    {
        validate_filename(filename)?;
        validate_text("options", options)?;
        let inner = start_recording(channel, filename, options, Arc::new(callback))?;
        Ok(RecordingSession { inner })
    }
}

/// Owns one native recording and its event callback. Drop performs the native
/// session's best-effort stop before releasing its channel/callback references.
pub struct RecordingSession {
    inner: NativeRecordingSession,
}

impl RecordingSession {
    pub fn id(&self) -> Result<String, RecordingError> {
        Ok(self.inner.id().to_owned())
    }

    pub fn state(&self) -> Result<RecordingState, RecordingError> {
        Ok(self.inner.state())
    }

    pub fn stop(&mut self) -> Result<(), RecordingError> {
        self.inner.stop()
    }

    pub fn set_muted(
        &mut self,
        direction: RecordingDirection,
        muted: bool,
    ) -> Result<usize, RecordingError> {
        self.inner.set_muted(direction, muted)
    }
}

impl RecordingSessionControl for RecordingSession {
    type Error = RecordingError;

    fn id(&self) -> Result<String, Self::Error> {
        self.id()
    }

    fn state(&self) -> Result<RecordingState, Self::Error> {
        self.state()
    }

    fn stop(&mut self) -> Result<(), Self::Error> {
        self.stop()
    }

    fn set_muted(
        &mut self,
        direction: RecordingDirection,
        muted: bool,
    ) -> Result<usize, Self::Error> {
        self.set_muted(direction, muted)
    }
}
