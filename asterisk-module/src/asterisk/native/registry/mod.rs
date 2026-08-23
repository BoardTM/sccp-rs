//! Reusable ownership primitives for Asterisk callback registries.

mod callback;

pub use callback::{
    CallbackAdmissionError, CallbackRegistration, ShutdownDisposition, acquire_from_native,
    contain_callback_panic, release_from_native, retain_for_native,
};
