//! Shared primitives used only at actual Asterisk callback boundaries.

mod callback;
mod output;
mod status;
mod text;
mod value;

pub use callback::contain_panic;
pub use output::write_c_text;
pub use status::{CallbackStatus, DeviceState, LogLevel};
pub use text::{optional_c_text, required_c_text};
pub use value::{read_c_int, write_c_int};
