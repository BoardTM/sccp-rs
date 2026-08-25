//! Rust-native MixMonitor ownership and Asterisk translation.
//!
//! One RAII session owns the channel reference, copied MixMonitor identifier,
//! typed state, and typed Rust event callback. Raw Asterisk statuses are mapped
//! at each syscall and no project-owned C record, userdata pointer, destroy
//! trampoline, or Rust-to-Rust foreign callback participates in this path.

use std::ffi::{CStr, CString, c_int};
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::asterisk::raw::handles::{Ao2Object, ChannelRef};
use crate::asterisk::sys;
use crate::media::recording::{
    RecordingCallback, RecordingDirection, RecordingError, RecordingEvent, RecordingState,
};
use crate::pbx::party::AsteriskChannel;

const MIXMONITOR_SOURCE: &CStr = c"MixMonitor";

static NEXT_RECORDING_TOKEN: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct MutedDirections {
    read: bool,
    write: bool,
}

impl MutedDirections {
    fn set(&mut self, direction: RecordingDirection, muted: bool) {
        match direction {
            RecordingDirection::Read => self.read = muted,
            RecordingDirection::Write => self.write = muted,
            RecordingDirection::Both => {
                self.read = muted;
                self.write = muted;
            }
        }
    }

    const fn any(self) -> bool {
        self.read || self.write
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RecordingStatus {
    state: RecordingState,
    muted_directions: MutedDirections,
}

impl RecordingStatus {
    const fn active() -> Self {
        Self {
            state: RecordingState::Active,
            muted_directions: MutedDirections {
                read: false,
                write: false,
            },
        }
    }

    fn mute_changed(&mut self, direction: RecordingDirection, muted: bool, affected: usize) {
        if affected == 0 {
            return;
        }
        self.muted_directions.set(direction, muted);
        self.state = if self.muted_directions.any() {
            RecordingState::Muted
        } else {
            RecordingState::Active
        };
    }

    fn stopped(&mut self) {
        self.state = RecordingState::Stopped;
        self.muted_directions = MutedDirections::default();
    }
}

struct RecordingId {
    text: String,
    native: CString,
}

struct CallbackOwner {
    callback: RecordingCallback,
}

impl CallbackOwner {
    fn new(callback: RecordingCallback) -> Self {
        Self { callback }
    }

    fn notify(&self, event: RecordingEvent) {
        (self.callback)(event);
    }
}

/// Typed owning handle used by the recording domain API.
pub struct NativeRecordingSession {
    // Preserve teardown ordering: destroy user callback state before releasing
    // the channel reference after `Drop::drop` has attempted the final stop.
    callback: CallbackOwner,
    channel: ChannelRef,
    id: RecordingId,
    status: RecordingStatus,
}

impl NativeRecordingSession {
    pub fn id(&self) -> &str {
        &self.id.text
    }

    pub const fn state(&self) -> RecordingState {
        self.status.state
    }

    pub fn stop(&mut self) -> Result<(), RecordingError> {
        if self.status.state == RecordingState::Stopped {
            return Ok(());
        }
        let result =
            unsafe { sys::ast_stop_mixmonitor(self.channel.as_ptr(), self.id.native.as_ptr()) };
        if result != 0 {
            return Err(RecordingError::StopFailed);
        }
        self.status.stopped();
        self.callback.notify(RecordingEvent::Stopped);
        Ok(())
    }

    pub fn set_muted(
        &mut self,
        direction: RecordingDirection,
        muted: bool,
    ) -> Result<usize, RecordingError> {
        if self.status.state == RecordingState::Stopped {
            return Err(RecordingError::MuteFailed);
        }
        let (flag, direction_name) = direction_parameters(direction);
        let affected = unsafe {
            sys::ast_audiohook_set_mute_all(
                self.channel.as_ptr(),
                MIXMONITOR_SOURCE.as_ptr(),
                flag,
                c_int::from(!muted),
            )
        };
        let affected = usize::try_from(affected).map_err(|_| RecordingError::MuteFailed)?;
        self.status.mute_changed(direction, muted, affected);

        unsafe { publish_mute_event(self, direction_name, muted, affected) };
        self.callback.notify(RecordingEvent::MuteChanged {
            direction,
            muted,
            affected,
        });
        Ok(affected)
    }
}

impl Drop for NativeRecordingSession {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

struct ProvisionalMixMonitor {
    session: Option<(ChannelRef, CString)>,
}

impl ProvisionalMixMonitor {
    fn new(channel: ChannelRef, id: CString) -> Self {
        Self {
            session: Some((channel, id)),
        }
    }

    fn id(&self) -> &CStr {
        self.session
            .as_ref()
            .map(|(_, id)| id.as_c_str())
            .expect("a provisional recorder owns its native session")
    }

    fn commit(mut self) -> (ChannelRef, CString) {
        self.session
            .take()
            .expect("a provisional recorder commits exactly once")
    }
}

impl Drop for ProvisionalMixMonitor {
    fn drop(&mut self) {
        let Some((channel, id)) = self.session.take() else {
            return;
        };
        unsafe { sys::ast_stop_mixmonitor(channel.as_ptr(), id.as_ptr()) };
    }
}

fn direction_parameters(direction: RecordingDirection) -> (u32, &'static CStr) {
    match direction {
        RecordingDirection::Read => (sys::AST_AUDIOHOOK_MUTE_READ, c"read"),
        RecordingDirection::Write => (sys::AST_AUDIOHOOK_MUTE_WRITE, c"write"),
        RecordingDirection::Both => (
            sys::AST_AUDIOHOOK_MUTE_READ | sys::AST_AUDIOHOOK_MUTE_WRITE,
            c"both",
        ),
    }
}

fn id_variable() -> Result<CString, RecordingError> {
    let token = NEXT_RECORDING_TOKEN.fetch_add(1, Ordering::Relaxed);
    CString::new(format!("SCCP_RECORDING_ID_{token:016x}")).map_err(|_| RecordingError::StartFailed)
}

fn combined_options(options: &str, id_variable: &CStr) -> Result<CString, RecordingError> {
    let id_variable = id_variable
        .to_str()
        .map_err(|_| RecordingError::StartFailed)?;
    CString::new(format!("{options}i({id_variable})")).map_err(|_| RecordingError::StartFailed)
}

fn native_text(value: &str) -> Result<CString, RecordingError> {
    CString::new(value).map_err(|_| RecordingError::StartFailed)
}

/// Start a MixMonitor session and retain all callback/channel ownership in Rust.
pub fn start_recording(
    channel: &AsteriskChannel<'_>,
    filename: &str,
    options: &str,
    callback: RecordingCallback,
) -> Result<NativeRecordingSession, RecordingError> {
    let callback = CallbackOwner::new(callback);
    let filename = native_text(filename)?;
    let id_variable = id_variable()?;
    let options = combined_options(options, &id_variable)?;
    let channel = unsafe { ChannelRef::acquire(channel.as_raw().cast()) }
        .ok_or(RecordingError::StartFailed)?;

    unsafe {
        sys::pbx_builtin_setvar_helper(channel.as_ptr(), id_variable.as_ptr(), ptr::null());
    }
    if unsafe { sys::ast_start_mixmonitor(channel.as_ptr(), filename.as_ptr(), options.as_ptr()) }
        != 0
    {
        unsafe {
            sys::pbx_builtin_setvar_helper(channel.as_ptr(), id_variable.as_ptr(), ptr::null());
        }
        return Err(RecordingError::StartFailed);
    }
    let id = unsafe { sys::pbx_builtin_getvar_helper(channel.as_ptr(), id_variable.as_ptr()) };
    let id = (!id.is_null())
        .then(|| unsafe { CStr::from_ptr(id) })
        .filter(|id| !id.is_empty())
        .and_then(|id| CString::new(id.to_bytes()).ok());
    unsafe {
        sys::pbx_builtin_setvar_helper(channel.as_ptr(), id_variable.as_ptr(), ptr::null());
    }
    let Some(native) = id else {
        return Err(RecordingError::StartFailed);
    };
    let provisional = ProvisionalMixMonitor::new(channel, native);
    let text = provisional
        .id()
        .to_str()
        .map_err(|_| RecordingError::StartFailed)?
        .to_owned();
    let (channel, native) = provisional.commit();

    let session = NativeRecordingSession {
        callback,
        channel,
        id: RecordingId { text, native },
        status: RecordingStatus::active(),
    };
    session.callback.notify(RecordingEvent::Started);
    Ok(session)
}

unsafe fn publish_mute_event(
    recording: &NativeRecordingSession,
    direction: &CStr,
    muted: bool,
    affected: usize,
) {
    let Ok(affected) = c_int::try_from(affected) else {
        return;
    };
    let blob = unsafe {
        sys::ast_json_pack(
            c"{s: s, s: b, s: s, s: i}".as_ptr(),
            c"direction".as_ptr(),
            direction.as_ptr(),
            c"state".as_ptr(),
            c_int::from(muted),
            c"mixmonitorid".as_ptr(),
            recording.id.native.as_ptr(),
            c"count".as_ptr(),
            affected,
        )
    };
    let message = if blob.is_null() {
        ptr::null_mut()
    } else {
        unsafe {
            sys::ast_channel_blob_create_from_cache(
                sys::ast_channel_uniqueid(recording.channel.as_ptr()),
                sys::ast_channel_mixmonitor_mute_type(),
                blob,
            )
        }
    };
    if let Some(message) = unsafe { Ao2Object::from_owned(message) } {
        unsafe {
            sys::stasis_publish(
                sys::ast_channel_topic(recording.channel.as_ptr()),
                message.as_ptr(),
            );
        }
    }
    if !blob.is_null() {
        unsafe { sys::ast_json_unref(blob) };
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;

    #[test]
    fn typed_status_tracks_directional_mute_transitions() {
        let mut status = RecordingStatus::active();
        status.mute_changed(RecordingDirection::Read, true, 1);
        assert_eq!(status.state, RecordingState::Muted);
        status.mute_changed(RecordingDirection::Write, true, 1);
        status.mute_changed(RecordingDirection::Read, false, 1);
        assert_eq!(status.state, RecordingState::Muted);
        status.mute_changed(RecordingDirection::Write, false, 1);
        assert_eq!(status.state, RecordingState::Active);
        status.mute_changed(RecordingDirection::Both, true, 0);
        assert_eq!(status.state, RecordingState::Active);
        status.stopped();
        assert_eq!(status.state, RecordingState::Stopped);
    }

    #[test]
    fn callback_receives_owned_typed_events() {
        let calls = Arc::new(AtomicUsize::new(0));
        let called = Arc::clone(&calls);
        let callback: RecordingCallback = Arc::new(move |_| {
            called.fetch_add(1, Ordering::SeqCst);
        });
        let owner = CallbackOwner::new(callback);
        owner.notify(RecordingEvent::Started);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn session_field_order_destroys_callback_before_channel_owner() {
        struct Probe {
            name: &'static str,
            order: Arc<Mutex<Vec<&'static str>>>,
        }
        impl Drop for Probe {
            fn drop(&mut self) {
                self.order.lock().unwrap().push(self.name);
            }
        }
        struct SessionOrder {
            callback: Probe,
            channel: Probe,
        }

        let order = Arc::new(Mutex::new(Vec::new()));
        let session = SessionOrder {
            callback: Probe {
                name: "callback",
                order: Arc::clone(&order),
            },
            channel: Probe {
                name: "channel",
                order: Arc::clone(&order),
            },
        };
        let _ = (&session.callback, &session.channel);
        drop(session);
        assert_eq!(*order.lock().unwrap(), ["callback", "channel"]);
    }

    #[test]
    fn generated_options_append_a_private_mixmonitor_identifier_variable() {
        let variable = c"SCCP_RECORDING_ID_0000000000000001";
        assert_eq!(
            combined_options("bW", variable).unwrap().to_str().unwrap(),
            "bWi(SCCP_RECORDING_ID_0000000000000001)"
        );
    }
}
