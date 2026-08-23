//! Asterisk's built-in database as a [`PersistentStore`].

use std::ffi::{CStr, CString, c_int};
use std::ptr;

use crate::asterisk::raw::handles::AsteriskAllocation;
use crate::asterisk::sys;
use crate::state::persistence::{PersistenceError, PersistentStore};

/// Persistent storage backed by Asterisk's built-in database.
#[derive(Clone, Copy, Debug, Default)]
pub struct AsteriskDatabase;

impl AsteriskDatabase {
    pub const fn new() -> Self {
        Self
    }
}

impl PersistentStore for AsteriskDatabase {
    fn get(&self, family: &str, key: &str) -> Result<Option<String>, PersistenceError> {
        let family = c_string("family", family)?;
        let key = c_string("key", key)?;
        let mut value = ptr::null_mut();
        let result =
            unsafe { sys::ast_db_get_allocated(family.as_ptr(), key.as_ptr(), &mut value) };
        if result != 0 {
            return Ok(None);
        }
        let value = unsafe { AsteriskAllocation::from_owned(value) }
            .ok_or(PersistenceError::Backend { operation: "get" })?;
        let bytes = unsafe { CStr::from_ptr(value.as_ptr()) }
            .to_bytes()
            .to_owned();
        String::from_utf8(bytes)
            .map(Some)
            .map_err(PersistenceError::InvalidUtf8)
    }

    fn put(&self, family: &str, key: &str, value: &str) -> Result<(), PersistenceError> {
        let family = c_string("family", family)?;
        let key = c_string("key", key)?;
        let value = c_string("value", value)?;
        let result = unsafe { sys::ast_db_put(family.as_ptr(), key.as_ptr(), value.as_ptr()) };
        operation_status("put", result)
    }

    fn delete(&self, family: &str, key: &str) -> Result<(), PersistenceError> {
        let family = c_string("family", family)?;
        let key = c_string("key", key)?;
        let result = unsafe { sys::ast_db_del(family.as_ptr(), key.as_ptr()) };
        operation_status("delete", result)
    }
}

fn c_string(field: &'static str, value: &str) -> Result<CString, PersistenceError> {
    CString::new(value).map_err(|source| PersistenceError::InvalidText { field, source })
}

fn operation_status(operation: &'static str, result: c_int) -> Result<(), PersistenceError> {
    if result == 0 {
        Ok(())
    } else {
        Err(PersistenceError::Backend { operation })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_nul_bytes_before_calling_asterisk() {
        let error = c_string("key", "device\0dnd").unwrap_err();

        assert!(matches!(
            error,
            PersistenceError::InvalidText { field: "key", .. }
        ));
    }
}
