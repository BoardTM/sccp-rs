//! Shared primitives used only at actual Asterisk callback boundaries.

mod callback;
mod output;
mod status;
mod sync;
mod text;
mod value;

pub use callback::contain_panic;
pub use output::write_c_text;
pub use status::{CallbackStatus, DeviceState, LogLevel};
pub(super) use sync::{CondvarExt, MutexExt, RwLockExt};
pub use text::{
    NativeTextError, native_c_string, nullable_lossy_c_text, optional_c_text, required_c_text,
};
pub use value::{read_c_int, write_c_int};
