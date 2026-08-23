//! Asterisk dialplan publication for registered SCCP appearances.

use std::ffi::{CStr, CString, c_int};
use std::path::PathBuf;
use std::ptr;

use crate::asterisk::sys;
use crate::config::RegistrationTarget;
use crate::pbx::registration::{
    RegistrationContextPolicy, RegistrationExtensionBackend, RegistrationExtensionBackendError,
    RegistrationExtensionSpec,
};

const MAX_IDENTIFIER_BYTES: usize = 79;
const SOURCE_FILE: &CStr = c"asterisk/adapters/registration.rs";
const SOURCE_FUNCTION: &CStr = c"AsteriskRegistrationExtensions";
const REGISTRAR: &CStr = c"RustSCCPRegistration";
const EEXIST: c_int = 17;
const ENOENT: c_int = 2;

#[derive(Clone, Copy, Debug, Default)]
pub struct AsteriskRegistrationExtensions;

impl AsteriskRegistrationExtensions {
    pub const fn new() -> Self {
        Self
    }
}

impl RegistrationExtensionBackend for AsteriskRegistrationExtensions {
    fn publish(
        &self,
        extension: &RegistrationExtensionSpec,
    ) -> Result<(), RegistrationExtensionBackendError> {
        publish(extension, false)
    }

    fn replace(
        &self,
        extension: &RegistrationExtensionSpec,
    ) -> Result<(), RegistrationExtensionBackendError> {
        publish(extension, true)
    }

    fn unpublish(
        &self,
        target: &RegistrationTarget,
    ) -> Result<(), RegistrationExtensionBackendError> {
        let context = identifier(&target.context)?;
        let extension = identifier(&target.extension)?;
        let result = unsafe {
            sys::ast_context_remove_extension(
                context.as_ptr(),
                extension.as_ptr(),
                1,
                REGISTRAR.as_ptr(),
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(RegistrationExtensionBackendError::NotFound)
        }
    }
}

fn publish(
    extension: &RegistrationExtensionSpec,
    replace: bool,
) -> Result<(), RegistrationExtensionBackendError> {
    let context = identifier(&extension.target.context)?;
    let target = identifier(&extension.target.extension)?;
    let line = line_text(&extension.line)?;

    unsafe {
        if sys::ast_context_find(context.as_ptr()).is_null() {
            if extension.context_policy == RegistrationContextPolicy::RequireExisting {
                return Err(RegistrationExtensionBackendError::NotFound);
            }
            if sys::ast_context_find_or_create(
                ptr::null_mut(),
                ptr::null_mut(),
                context.as_ptr(),
                REGISTRAR.as_ptr(),
            )
            .is_null()
            {
                return Err(RegistrationExtensionBackendError::Failed);
            }
        }

        let application_data = sys::__ast_strdup(
            line.as_ptr(),
            SOURCE_FILE.as_ptr(),
            line!() as c_int,
            SOURCE_FUNCTION.as_ptr(),
        );
        if application_data.is_null() {
            return Err(RegistrationExtensionBackendError::Failed);
        }

        *errno_location() = 0;
        let result = sys::ast_add_extension(
            context.as_ptr(),
            c_int::from(replace),
            target.as_ptr(),
            1,
            ptr::null(),
            ptr::null(),
            c"NoOp".as_ptr(),
            application_data.cast(),
            Some(sys::ast_free_ptr),
            REGISTRAR.as_ptr(),
        );
        if result == 0 {
            return Ok(());
        }
        match *errno_location() {
            EEXIST => Err(RegistrationExtensionBackendError::Conflict),
            ENOENT => Err(RegistrationExtensionBackendError::NotFound),
            _ => Err(RegistrationExtensionBackendError::Failed),
        }
    }
}

fn identifier(value: &str) -> Result<CString, RegistrationExtensionBackendError> {
    if value.is_empty()
        || value.len() > MAX_IDENTIFIER_BYTES
        || value
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
        || value.contains(['&', '@'])
    {
        return Err(RegistrationExtensionBackendError::Invalid);
    }
    CString::new(value).map_err(|_| RegistrationExtensionBackendError::Invalid)
}

fn line_text(value: &str) -> Result<CString, RegistrationExtensionBackendError> {
    if value.is_empty() || value.len() > MAX_IDENTIFIER_BYTES || value.chars().any(char::is_control)
    {
        return Err(RegistrationExtensionBackendError::Invalid);
    }
    CString::new(value).map_err(|_| RegistrationExtensionBackendError::Invalid)
}

/// Returns Asterisk's configured directory as an owned path.
pub fn config_directory() -> Option<PathBuf> {
    let directory = unsafe { sys::ast_config_AST_CONFIG_DIR };
    if directory.is_null() {
        return None;
    }
    Some(PathBuf::from(
        unsafe { CStr::from_ptr(directory) }
            .to_string_lossy()
            .as_ref(),
    ))
}

unsafe extern "C" {
    fn __errno_location() -> *mut c_int;
}

unsafe fn errno_location() -> *mut c_int {
    unsafe { __errno_location() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unsafe_identifiers_before_calling_asterisk() {
        assert!(matches!(
            identifier("bad context"),
            Err(RegistrationExtensionBackendError::Invalid)
        ));
        assert!(matches!(
            identifier("bad\0extension"),
            Err(RegistrationExtensionBackendError::Invalid)
        ));
        assert!(matches!(
            line_text("bad\0line"),
            Err(RegistrationExtensionBackendError::Invalid)
        ));
    }
}
