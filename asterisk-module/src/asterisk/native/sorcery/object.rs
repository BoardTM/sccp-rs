use std::ffi::{CStr, CString, c_int, c_void};
use std::mem;
use std::ptr::{self, NonNull};

use crate::asterisk::sys;

use super::super::registry::contain_callback_panic;
use super::{
    MAX_FIELD_NAME_BYTES, MAX_OBJECT_ID_BYTES, RawSorceryField, RawSorceryObject, SorceryError,
    copy_bounded_native_text,
};

const EMPTY: &CStr = c"";
const FIELD_SOURCE: &CStr = c"asterisk/native/sorcery/object.rs";
const SOURCE_FUNCTION: &CStr = c"sccp_sorcery_object";
const MAX_OBJECT_FIELDS: usize = 256;
const MAX_FIELD_VALUE_BYTES: usize = 4096;
const MAX_OBJECT_BYTES: usize = 64 * 1024;

#[derive(Clone)]
struct StoredField {
    name: CString,
    value: CString,
}

#[repr(C)]
pub(super) struct StoredObject {
    details: sys::ast_sorcery_object_details,
    fields: Vec<StoredField>,
}

pub(super) unsafe extern "C" fn object_alloc(_: *const std::ffi::c_char) -> *mut c_void {
    contain_callback_panic(ptr::null_mut(), || {
        let object = unsafe {
            sys::ast_sorcery_generic_alloc(mem::size_of::<StoredObject>(), Some(object_destroy))
        }
        .cast::<StoredObject>();
        let Some(mut object) = NonNull::new(object) else {
            return ptr::null_mut();
        };
        unsafe { ptr::write(ptr::addr_of_mut!(object.as_mut().fields), Vec::new()) };
        object.as_ptr().cast()
    })
}

pub(super) unsafe extern "C" fn object_destroy(object: *mut c_void) {
    contain_callback_panic((), || {
        let Some(object) = NonNull::new(object.cast::<StoredObject>()) else {
            return;
        };
        unsafe { ptr::drop_in_place(ptr::addr_of_mut!((*object.as_ptr()).fields)) };
    });
}

pub(super) unsafe extern "C" fn object_copy(
    source: *const c_void,
    destination: *mut c_void,
) -> c_int {
    contain_callback_panic(-1, || {
        let (Some(source), Some(mut destination)) = (
            NonNull::new(source.cast_mut().cast::<StoredObject>()),
            NonNull::new(destination.cast::<StoredObject>()),
        ) else {
            return -1;
        };
        unsafe {
            destination
                .as_mut()
                .fields
                .clone_from(&source.as_ref().fields)
        };
        0
    })
}

pub(super) unsafe extern "C" fn object_validate(
    _: *const sys::ast_sorcery,
    object: *mut c_void,
) -> c_int {
    contain_callback_panic(-1, || {
        let Some(object) = NonNull::new(object.cast::<StoredObject>()) else {
            return -1;
        };
        let id = unsafe { sys::ast_sorcery_object_get_id(object.as_ptr().cast()) };
        if id.is_null() {
            return -1;
        }
        let id = unsafe { CStr::from_ptr(id) };
        if id.is_empty() || id.to_str().is_err() || id.to_bytes().len() > MAX_OBJECT_ID_BYTES {
            return -1;
        }
        if validate_fields(unsafe { &object.as_ref().fields }) {
            0
        } else {
            -1
        }
    })
}

pub(super) unsafe extern "C" fn field_apply(
    _: *const sys::aco_option,
    variable: *mut sys::ast_variable,
    object: *mut c_void,
) -> c_int {
    contain_callback_panic(-1, || {
        let (Some(variable), Some(mut object)) = (
            NonNull::new(variable),
            NonNull::new(object.cast::<StoredObject>()),
        ) else {
            return -1;
        };
        let variable = unsafe { variable.as_ref() };
        if variable.name.is_null() || variable.value.is_null() {
            return -1;
        }
        let name = unsafe { CStr::from_ptr(variable.name) };
        let value = unsafe { CStr::from_ptr(variable.value) };
        if !valid_field_name(name)
            || value.to_str().is_err()
            || value.to_bytes().len() > MAX_FIELD_VALUE_BYTES
        {
            return -1;
        }
        let fields = unsafe { &mut object.as_mut().fields };
        if !upsert_field(fields, name, value) || !validate_fields(fields) {
            return -1;
        }
        0
    })
}

pub(super) unsafe extern "C" fn fields_export(
    object: *const c_void,
    output: *mut *mut sys::ast_variable,
) -> c_int {
    contain_callback_panic(-1, || {
        let (Some(object), Some(mut output)) = (
            NonNull::new(object.cast_mut().cast::<StoredObject>()),
            NonNull::new(output),
        ) else {
            return -1;
        };
        unsafe { *output.as_mut() = ptr::null_mut() };
        let mut tail = ptr::null_mut::<sys::ast_variable>();
        for field in unsafe { &object.as_ref().fields } {
            let variable = unsafe {
                sys::_ast_variable_new(
                    field.name.as_ptr(),
                    field.value.as_ptr(),
                    EMPTY.as_ptr(),
                    FIELD_SOURCE.as_ptr(),
                    SOURCE_FUNCTION.as_ptr(),
                    line!() as c_int,
                )
            };
            if variable.is_null() {
                unsafe { sys::ast_variables_destroy(*output.as_ref()) };
                unsafe { *output.as_mut() = ptr::null_mut() };
                return -1;
            }
            if tail.is_null() {
                unsafe { *output.as_mut() = variable };
            } else {
                unsafe { (*tail).next = variable };
            }
            tail = variable;
        }
        0
    })
}

pub(super) unsafe fn copy_object(
    object: *const StoredObject,
    object_type: &'static str,
) -> Result<RawSorceryObject, SorceryError> {
    let id = unsafe { sys::ast_sorcery_object_get_id(object.cast()) };
    let id = unsafe {
        copy_bounded_native_text(id, MAX_OBJECT_ID_BYTES, format!("{object_type} object id"))?
    };
    let fields = unsafe { &(*object).fields }
        .iter()
        .map(|field| {
            Ok(RawSorceryField {
                name: field.name.to_str().map(str::to_owned).map_err(|_| {
                    SorceryError::InvalidNativeText {
                        location: format!("{object_type} {id} field name"),
                    }
                })?,
                value: field.value.to_str().map(str::to_owned).map_err(|_| {
                    SorceryError::InvalidNativeText {
                        location: format!("{object_type} {id} field value"),
                    }
                })?,
            })
        })
        .collect::<Result<Vec<_>, SorceryError>>()?;
    Ok(RawSorceryObject { id, fields })
}

fn valid_field_name(name: &CStr) -> bool {
    let bytes = name.to_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_FIELD_NAME_BYTES
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(*byte, b'_' | b'-' | b'.'))
}

fn validate_fields(fields: &[StoredField]) -> bool {
    fields.len() <= MAX_OBJECT_FIELDS
        && fields.iter().all(|field| {
            valid_field_name(field.name.as_c_str())
                && field.value.to_str().is_ok()
                && field.value.as_bytes().len() <= MAX_FIELD_VALUE_BYTES
        })
        && fields.iter().fold(0_usize, |total, field| {
            total
                .saturating_add(field.name.as_bytes().len())
                .saturating_add(field.value.as_bytes().len())
        }) <= MAX_OBJECT_BYTES
}

fn upsert_field(fields: &mut Vec<StoredField>, name: &CStr, value: &CStr) -> bool {
    let existing = fields
        .iter()
        .position(|field| field.name.as_bytes().eq_ignore_ascii_case(name.to_bytes()));
    if value.is_empty() && is_indexed_repeatable(name.to_bytes()) {
        if let Some(index) = existing {
            fields.remove(index);
        }
        return true;
    }
    let value = value.to_owned();
    if let Some(index) = existing {
        fields[index].name = name.to_owned();
        fields[index].value = value;
    } else {
        if fields.len() >= MAX_OBJECT_FIELDS {
            return false;
        }
        fields.push(StoredField {
            name: name.to_owned(),
            value,
        });
    }
    true
}

fn is_indexed_repeatable(name: &[u8]) -> bool {
    let Some(separator) = name.iter().position(|byte| *byte == b'.') else {
        return false;
    };
    let normalized = name[..separator]
        .iter()
        .filter(|byte| byte.is_ascii_alphanumeric())
        .map(u8::to_ascii_lowercase)
        .collect::<Vec<_>>();
    matches!(
        normalized.as_slice(),
        b"button"
            | b"line"
            | b"allow"
            | b"disallow"
            | b"setvar"
            | b"featuredefault"
            | b"permit"
            | b"deny"
            | b"permithost"
            | b"template"
    ) && name[separator + 1..].iter().all(u8::is_ascii_digit)
        && separator + 1 < name.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(values: &[(&str, &str)]) -> Vec<StoredField> {
        values
            .iter()
            .map(|(name, value)| StoredField {
                name: CString::new(*name).unwrap(),
                value: CString::new(*value).unwrap(),
            })
            .collect()
    }

    fn values(fields: &[StoredField]) -> Vec<(&str, &str)> {
        fields
            .iter()
            .map(|field| (field.name.to_str().unwrap(), field.value.to_str().unwrap()))
            .collect()
    }

    #[test]
    fn scalar_updates_replace_in_place_and_preserve_empty_values() {
        let mut stored = fields(&[("description", "old"), ("softkeyprofile", "default")]);
        assert!(upsert_field(&mut stored, c"description", c"new"));
        assert!(upsert_field(&mut stored, c"SoftKeyProfile", c""));
        assert_eq!(
            values(&stored),
            vec![("description", "new"), ("SoftKeyProfile", "")]
        );
    }

    #[test]
    fn indexed_repeatable_fields_are_unique_ordered_and_tombstoned_by_empty_values() {
        let mut stored = fields(&[("button.0001", "line,1000"), ("button.0002", "hold")]);
        assert!(upsert_field(&mut stored, c"button.0001", c"line,2000"));
        assert!(upsert_field(&mut stored, c"button.0002", c""));
        assert!(upsert_field(&mut stored, c"button.0003", c"transfer"));
        assert_eq!(
            values(&stored),
            vec![("button.0001", "line,2000"), ("button.0003", "transfer")]
        );
    }

    #[test]
    fn only_supported_numeric_suffixes_have_tombstone_semantics() {
        assert!(is_indexed_repeatable(b"setvar.12"));
        assert!(is_indexed_repeatable(b"Feature_Default.2"));
        assert!(is_indexed_repeatable(b"template.0001"));
        assert!(!is_indexed_repeatable(b"description.12"));
        assert!(!is_indexed_repeatable(b"button.primary"));
        assert!(!is_indexed_repeatable(b"button."));
    }

    #[test]
    fn raw_field_limits_reject_unbounded_or_non_text_values() {
        assert!(valid_field_name(c"button.0001"));
        assert!(!valid_field_name(c"button/0001"));
        assert!(!valid_field_name(
            CString::new("x".repeat(MAX_FIELD_NAME_BYTES + 1))
                .unwrap()
                .as_c_str()
        ));

        let mut stored = fields(&[("description", "phone")]);
        stored[0].value = CString::new("x".repeat(MAX_FIELD_VALUE_BYTES + 1)).unwrap();
        assert!(!validate_fields(&stored));
    }
}
