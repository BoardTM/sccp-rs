//! Asterisk realtime adapter for the owned configuration port.
//!
//! All Asterisk variables and configuration objects are scoped RAII values.
//! Rows are copied into domain-owned values before those allocations are
//! released; no raw pointer, project ABI record, or integer status crosses the
//! adapter boundary.

use std::ffi::{CStr, CString, c_char, c_int};
use std::ptr::{self, NonNull};

use crate::asterisk::sys;
use crate::config::realtime::{
    RealtimeError, RealtimeField, RealtimeLoad, RealtimePredicate, RealtimeRow,
    decode_snapshot_rows, validate_query,
};

use super::ownership::AstConfigOwner;

const SOURCE_FILE: &CStr = c"asterisk/native/realtime.rs";
const SOURCE_FUNCTION: &CStr = c"sccp_realtime";
const EMPTY: &CStr = c"";

type VariablesDestroy = unsafe fn(*mut sys::ast_variable);

unsafe fn destroy_variables(variables: *mut sys::ast_variable) {
    unsafe { sys::ast_variables_destroy(variables) };
}

struct AstVariables {
    pointer: Option<NonNull<sys::ast_variable>>,
    destroy: VariablesDestroy,
}

impl AstVariables {
    fn empty() -> Self {
        Self {
            pointer: None,
            destroy: destroy_variables,
        }
    }

    #[cfg(test)]
    fn with_destroy(
        pointer: Option<NonNull<sys::ast_variable>>,
        destroy: VariablesDestroy,
    ) -> Self {
        Self { pointer, destroy }
    }
}

impl Drop for AstVariables {
    fn drop(&mut self) {
        if let Some(variables) = self.pointer {
            unsafe { (self.destroy)(variables.as_ptr()) };
        }
    }
}

fn owned_c_string(field: String, value: &str) -> Result<CString, RealtimeError> {
    CString::new(value).map_err(|source| RealtimeError::InvalidText { field, source })
}

unsafe fn copy_text(value: *const c_char, location: String) -> Result<String, RealtimeError> {
    unsafe { CStr::from_ptr(value) }
        .to_str()
        .map(str::to_owned)
        .map_err(|source| RealtimeError::InvalidNativeText { location, source })
}

unsafe fn copy_value(
    value: *const c_char,
    location: String,
) -> Result<Option<String>, RealtimeError> {
    if value.is_null() {
        return Ok(None);
    }
    let value = unsafe { copy_text(value, location) }?;
    // Asterisk realtime uses one ASCII space to carry an explicit empty value
    // through backends which otherwise collapse it with SQL NULL.
    Ok(Some(if value == " " { String::new() } else { value }))
}

unsafe fn copy_row(
    mut variable: *const sys::ast_variable,
    row_index: usize,
) -> Result<RealtimeRow, RealtimeError> {
    let mut fields = Vec::new();
    let mut field_index = 0;
    while let Some(current) = unsafe { variable.as_ref() } {
        if current.name.is_null() {
            return Err(RealtimeError::MissingFieldName {
                row: row_index,
                field: field_index,
            });
        }
        let name = unsafe {
            copy_text(
                current.name,
                format!("row {row_index}, field {field_index} name"),
            )
        }?;
        let value = unsafe {
            copy_value(
                current.value,
                format!("row {row_index}, field {field_index} value"),
            )
        }?;
        fields.push(RealtimeField { name, value });
        variable = current.next;
        field_index += 1;
    }
    Ok(RealtimeRow { fields })
}

unsafe fn build_predicates(
    predicates: &[RealtimePredicate],
) -> Result<AstVariables, RealtimeError> {
    let mut variables = AstVariables::empty();
    let mut tail = ptr::null_mut::<sys::ast_variable>();
    for (index, predicate) in predicates.iter().enumerate() {
        let name = owned_c_string(format!("predicate[{index}].name"), &predicate.name)?;
        let value = owned_c_string(format!("predicate[{index}].value"), &predicate.value)?;
        let variable = unsafe {
            sys::_ast_variable_new(
                name.as_ptr(),
                value.as_ptr(),
                EMPTY.as_ptr(),
                SOURCE_FILE.as_ptr(),
                SOURCE_FUNCTION.as_ptr(),
                line!() as c_int,
            )
        };
        if variable.is_null() {
            return Err(RealtimeError::BackendFailure);
        }
        if tail.is_null() {
            variables.pointer = NonNull::new(variable);
        } else {
            unsafe { (*tail).next = variable };
        }
        tail = variable;
    }
    Ok(variables)
}

unsafe fn load_multiple(
    family: &CStr,
    filters: *const sys::ast_variable,
) -> Result<RealtimeLoad, RealtimeError> {
    let Some(config) =
        NonNull::new(unsafe { sys::ast_load_realtime_multientry_fields(family.as_ptr(), filters) })
    else {
        return Ok(RealtimeLoad::default());
    };
    let config = AstConfigOwner::new(config);

    let mut rows = Vec::new();
    let mut category = ptr::null_mut::<sys::ast_category>();
    loop {
        category = unsafe {
            sys::ast_category_browse_filtered(config.as_ptr(), ptr::null(), category, ptr::null())
        };
        if category.is_null() {
            break;
        }
        let variables = unsafe { sys::ast_category_first(category) };
        rows.push(unsafe { copy_row(variables, rows.len()) }?);
    }
    decode_snapshot_rows(rows)
}

pub fn load_realtime(
    family: &str,
    predicates: &[RealtimePredicate],
) -> Result<RealtimeLoad, RealtimeError> {
    validate_query(family, predicates)?;
    let family = owned_c_string("family".to_owned(), family)?;
    let filters = unsafe { build_predicates(predicates) }?;

    if unsafe { sys::ast_check_realtime(family.as_ptr()) } == 0 {
        return Err(RealtimeError::BackendUnavailable);
    }
    unsafe {
        load_multiple(
            &family,
            filters
                .pointer
                .map_or(ptr::null(), |variables| variables.as_ptr()),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    static VARIABLE_DESTROYS: AtomicUsize = AtomicUsize::new(0);
    static VARIABLE_TEST_LOCK: Mutex<()> = Mutex::new(());

    unsafe fn count_variable_destroy(_: *mut sys::ast_variable) {
        VARIABLE_DESTROYS.fetch_add(1, Ordering::SeqCst);
    }

    #[test]
    fn native_values_preserve_null_empty_and_asterisk_empty_sentinel() {
        assert_eq!(
            unsafe { copy_value(ptr::null(), "null".to_owned()) }.unwrap(),
            None
        );
        assert_eq!(
            unsafe { copy_value(c"".as_ptr(), "empty".to_owned()) }.unwrap(),
            Some(String::new())
        );
        assert_eq!(
            unsafe { copy_value(c" ".as_ptr(), "sentinel".to_owned()) }.unwrap(),
            Some(String::new())
        );
        assert_eq!(
            unsafe { copy_value(c"value".as_ptr(), "value".to_owned()) }.unwrap(),
            Some("value".to_owned())
        );
    }

    #[test]
    fn invalid_native_utf8_is_a_typed_domain_failure() {
        let invalid = [0xff_u8, 0];
        let error = unsafe {
            copy_value(
                invalid.as_ptr().cast::<c_char>(),
                "row 0, field 0 value".to_owned(),
            )
        }
        .unwrap_err();
        assert!(matches!(error, RealtimeError::InvalidNativeText { .. }));
    }

    #[test]
    fn empty_realtime_variable_owner_destroys_nothing() {
        let _guard = VARIABLE_TEST_LOCK.lock().unwrap();
        VARIABLE_DESTROYS.store(0, Ordering::SeqCst);
        drop(AstVariables::with_destroy(None, count_variable_destroy));
        assert_eq!(VARIABLE_DESTROYS.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn nonempty_realtime_variable_owner_destroys_once() {
        let _guard = VARIABLE_TEST_LOCK.lock().unwrap();
        VARIABLE_DESTROYS.store(0, Ordering::SeqCst);
        let variables = NonNull::<sys::ast_variable>::dangling();
        drop(AstVariables::with_destroy(
            Some(variables),
            count_variable_destroy,
        ));
        assert_eq!(VARIABLE_DESTROYS.load(Ordering::SeqCst), 1);
    }
}
