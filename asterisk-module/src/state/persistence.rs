//! Policy-free persistent storage contracts.

use std::ffi::NulError;

use thiserror::Error;

/// A synchronous key-value store organized into families.
pub trait PersistentStore: Send + Sync {
    /// Returns the value for `family` and `key`, or `None` when it is absent.
    fn get(&self, family: &str, key: &str) -> Result<Option<String>, PersistenceError>;

    /// Creates or replaces the value for `family` and `key`.
    fn put(&self, family: &str, key: &str, value: &str) -> Result<(), PersistenceError>;

    /// Removes `family` and `key`. Removing an absent key succeeds.
    fn delete(&self, family: &str, key: &str) -> Result<(), PersistenceError>;
}

#[derive(Debug, Error)]
pub enum PersistenceError {
    #[error("{field} contains a NUL byte")]
    InvalidText {
        field: &'static str,
        #[source]
        source: NulError,
    },

    #[error("persistence backend {operation} failed")]
    Backend { operation: &'static str },

    #[error("persistence backend returned a non-UTF-8 value")]
    InvalidUtf8(#[source] std::string::FromUtf8Error),
}
