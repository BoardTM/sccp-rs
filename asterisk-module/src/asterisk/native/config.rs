//! Native Asterisk static configuration source.
//!
//! Runtime configuration is parsed by Asterisk first so includes, templates,
//! category additions, and its exact lexical rules match the host process.
//! The effective ordered categories are copied into owned text before the
//! native configuration object is destroyed, then the shared Serde schema
//! performs SCCP-specific typing and validation.

use std::ffi::{CStr, CString};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::{Arc, Mutex};

use crate::asterisk::sys;
use crate::config::ModuleConfig;
use crate::config::provider::{
    ConfigurationOrigin, ConfigurationProvider, ConfigurationProviderError,
    StaticConfigurationSource,
};

use super::ownership::ConfigLoad;

const REQUESTOR: &CStr = c"chan_sccp2";

#[derive(Clone, Debug)]
pub struct AsteriskConfigurationSource {
    path: PathBuf,
    last_source: Arc<Mutex<Option<String>>>,
}

impl AsteriskConfigurationSource {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            last_source: Arc::new(Mutex::new(None)),
        }
    }
}

impl StaticConfigurationSource for AsteriskConfigurationSource {
    fn origin(&self) -> ConfigurationOrigin {
        ConfigurationOrigin::File(self.path.clone())
    }

    fn read_source(&self) -> Result<String, ConfigurationProviderError> {
        let mut last_source = self
            .last_source
            .lock()
            .map_err(|_| native_failure(&self.path, "configuration source cache is poisoned"))?;
        match load_effective_source(&self.path, last_source.is_some())? {
            Some(source) => {
                *last_source = Some(source.clone());
                Ok(source)
            }
            None => last_source.clone().ok_or_else(|| {
                native_failure(&self.path, "unchanged status has no cached configuration")
            }),
        }
    }
}

impl ConfigurationProvider for AsteriskConfigurationSource {
    fn load(&self) -> Result<ModuleConfig, ConfigurationProviderError> {
        let source = self.read_source()?;
        ModuleConfig::parse(&source)
            .map_err(|error| ConfigurationProviderError::invalid(self.origin(), error))
    }
}

fn native_failure(path: &Path, message: impl Into<String>) -> ConfigurationProviderError {
    ConfigurationProviderError::unavailable(
        "Asterisk configuration parser",
        format!("{}: {}", path.display(), message.into()),
    )
}

unsafe fn copy_text(pointer: *const std::ffi::c_char) -> Result<String, String> {
    if pointer.is_null() {
        return Err("native parser returned a null string".into());
    }
    unsafe { CStr::from_ptr(pointer) }
        .to_str()
        .map(str::to_owned)
        .map_err(|error| format!("native parser returned invalid UTF-8: {error}"))
}

fn quote_value(value: &str) -> String {
    if value.trim() != value || value.contains(';') || value.starts_with('#') {
        format!("\"{}\"", value.replace('"', "\\\""))
    } else {
        value.to_owned()
    }
}

fn load_effective_source(
    path: &Path,
    check_unchanged: bool,
) -> Result<Option<String>, ConfigurationProviderError> {
    let path_text = path
        .to_str()
        .ok_or_else(|| native_failure(path, "path is not valid UTF-8"))?;
    let path_c = CString::new(path_text)
        .map_err(|_| native_failure(path, "path contains an interior NUL byte"))?;
    let flags = sys::ast_flags {
        flags: if check_unchanged { 1 << 1 } else { 0 },
    };
    let config = match ConfigLoad::decode(unsafe {
        sys::ast_config_load2(path_c.as_ptr(), REQUESTOR.as_ptr(), flags)
    }) {
        ConfigLoad::Missing => {
            return Err(native_failure(
                path,
                "file is missing or could not be loaded",
            ));
        }
        ConfigLoad::Unchanged => return Ok(None),
        ConfigLoad::Invalid => {
            return Err(native_failure(path, "file has invalid Asterisk syntax"));
        }
        ConfigLoad::Loaded(config) => config,
    };

    let mut output = String::new();
    let mut category = ptr::null_mut::<sys::ast_category>();
    loop {
        category = unsafe {
            sys::ast_category_browse_filtered(config.as_ptr(), ptr::null(), category, ptr::null())
        };
        if category.is_null() {
            break;
        }
        let name = unsafe { copy_text(sys::ast_category_get_name(category)) }
            .map_err(|error| native_failure(path, error))?;
        if !output.is_empty() {
            output.push('\n');
        }
        writeln!(output, "[{name}]").map_err(|error| native_failure(path, error.to_string()))?;

        let mut variable = unsafe { sys::ast_category_first(category) };
        while let Some(current) = unsafe { variable.as_ref() } {
            let name =
                unsafe { copy_text(current.name) }.map_err(|error| native_failure(path, error))?;
            let value =
                unsafe { copy_text(current.value) }.map_err(|error| native_failure(path, error))?;
            writeln!(output, "{name} = {}", quote_value(&value))
                .map_err(|error| native_failure(path, error.to_string()))?;
            variable = current.next;
        }
    }
    Ok(Some(output))
}
