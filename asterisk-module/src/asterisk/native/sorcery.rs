//! Asterisk Sorcery ownership for dynamic SCCP device and line objects.

use std::collections::BTreeSet;
use std::ffi::{CStr, CString, c_int, c_void};
use std::num::NonZeroUsize;
use std::ptr::{self, NonNull};
use std::sync::{Arc, Mutex, Once, OnceLock};

use thiserror::Error;

use crate::asterisk::sys;

use super::handles::Ao2Object;
use super::registry::{CallbackRegistration, contain_callback_panic};

mod object;

use object::{
    StoredObject, copy_object, field_apply, fields_export, object_alloc, object_copy,
    object_validate,
};

pub const DEVICE_TYPE: &CStr = c"device";
pub const LINE_TYPE: &CStr = c"line";

const MODULE_NAME: &CStr = c"chan_sccp2";
const ASTDB_PREFIX: &CStr = c"chan_sccp2";
const ASTDB_WIZARD: &CStr = c"astdb";
const FIELD_PATTERN: &CStr = c"^";
const SOURCE_FILE: &CStr = c"asterisk/native/sorcery.rs";
const SOURCE_FUNCTION: &CStr = c"sccp_sorcery";
const MAX_FIELD_NAME_BYTES: usize = 127;
const MAX_OBJECT_ID_BYTES: usize = 79;
const MAX_MAPPING_PREFIX_BYTES: usize = 255;
const MAX_ASTDB_PATH_BYTES: usize = 512;
const MAX_SNAPSHOT_OBJECTS: usize = 4096;

type MutationHook = dyn Fn(SorceryMutation) + Send + Sync + 'static;

static MUTATION_HOOK: OnceLock<Mutex<HookState>> = OnceLock::new();

enum HookState {
    Vacant,
    Registering,
    Active(Arc<CallbackRegistration<Arc<MutationHook>>>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SorceryObjectType {
    Device,
    Line,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SorceryMutationKind {
    Created,
    Updated,
    Deleted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SorceryMutation {
    pub kind: SorceryMutationKind,
    pub object_type: SorceryObjectType,
    pub id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawSorceryField {
    pub name: String,
    pub value: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawSorceryObject {
    pub id: String,
    pub fields: Vec<RawSorceryField>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SorcerySnapshot {
    pub devices: Vec<RawSorceryObject>,
    pub lines: Vec<RawSorceryObject>,
}

#[derive(Debug, Error)]
pub enum SorceryError {
    #[error("SCCP Sorcery is already registered")]
    AlreadyRegistered,
    #[error("Asterisk could not open the chan_sccp2 Sorcery instance")]
    OpenFailed,
    #[error("Asterisk could not map the {object_type} Sorcery object to AstDB")]
    MappingFailed { object_type: &'static str },
    #[error("Asterisk could not register the {object_type} Sorcery object type")]
    ObjectRegistrationFailed { object_type: &'static str },
    #[error("Asterisk could not register fields for the {object_type} Sorcery object type")]
    FieldRegistrationFailed { object_type: &'static str },
    #[error("Asterisk could not register the {object_type} Sorcery observer")]
    ObserverRegistrationFailed { object_type: &'static str },
    #[error("Asterisk could not retrieve {object_type} Sorcery objects")]
    RetrievalFailed { object_type: &'static str },
    #[error("Asterisk returned invalid UTF-8 in {location}")]
    InvalidNativeText { location: String },
    #[error("the {object_type} AstDB mapping contains an invalid key")]
    InvalidAstDbKey { object_type: &'static str },
    #[error("the {object_type} Sorcery snapshot exceeds its object limit")]
    SnapshotLimit { object_type: &'static str },
    #[error(
        "the {object_type} AstDB key set ({stored} stored) does not match the Sorcery object set ({loaded} loaded)"
    )]
    AstDbIntegrity {
        object_type: &'static str,
        stored: usize,
        loaded: usize,
    },
}

struct AstDbTree(Option<NonNull<sys::ast_db_entry>>);

impl Drop for AstDbTree {
    fn drop(&mut self) {
        if let Some(tree) = self.0 {
            unsafe { sys::ast_db_freetree(tree.as_ptr()) };
        }
    }
}

pub struct SorceryRegistration {
    sorcery: NonNull<sys::ast_sorcery>,
    hook: Arc<CallbackRegistration<Arc<MutationHook>>>,
    device_observer: bool,
    line_observer: bool,
    device_type: bool,
    line_type: bool,
    observer_shutdown: Once,
}

// SAFETY: Asterisk synchronizes ast_sorcery access and registration state is immutable once shared.
unsafe impl Send for SorceryRegistration {}
unsafe impl Sync for SorceryRegistration {}

impl SorceryRegistration {
    pub fn register(on_mutation: Arc<MutationHook>) -> Result<Self, SorceryError> {
        {
            let mut slot = mutation_slot()
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if !matches!(*slot, HookState::Vacant) {
                return Err(SorceryError::AlreadyRegistered);
            }
            *slot = HookState::Registering;
        }

        let result = Self::register_reserved(on_mutation);
        let mut slot = mutation_slot()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        match &result {
            Ok(registration) => *slot = HookState::Active(Arc::clone(&registration.hook)),
            Err(_) if matches!(*slot, HookState::Registering) => *slot = HookState::Vacant,
            Err(_) => {}
        }
        result
    }

    fn register_reserved(on_mutation: Arc<MutationHook>) -> Result<Self, SorceryError> {
        let hook = CallbackRegistration::new(NonZeroUsize::MAX, on_mutation);
        let sorcery = NonNull::new(unsafe {
            sys::__ast_sorcery_open(
                MODULE_NAME.as_ptr(),
                SOURCE_FILE.as_ptr(),
                line!() as c_int,
                SOURCE_FUNCTION.as_ptr(),
            )
        })
        .ok_or(SorceryError::OpenFailed)?;
        let mut registration = Self {
            sorcery,
            hook,
            device_observer: false,
            line_observer: false,
            device_type: false,
            line_type: false,
            observer_shutdown: Once::new(),
        };

        registration.register_type(DEVICE_TYPE, "device")?;
        registration.device_type = true;
        registration.register_type(LINE_TYPE, "line")?;
        registration.line_type = true;

        if unsafe {
            sys::ast_sorcery_observer_add(sorcery.as_ptr(), DEVICE_TYPE.as_ptr(), &DEVICE_OBSERVER)
        } != 0
        {
            return Err(SorceryError::ObserverRegistrationFailed {
                object_type: "device",
            });
        }
        registration.device_observer = true;
        if unsafe {
            sys::ast_sorcery_observer_add(sorcery.as_ptr(), LINE_TYPE.as_ptr(), &LINE_OBSERVER)
        } != 0
        {
            return Err(SorceryError::ObserverRegistrationFailed {
                object_type: "line",
            });
        }
        registration.line_observer = true;
        unsafe { sys::ast_sorcery_load(sorcery.as_ptr()) };
        Ok(registration)
    }

    pub fn snapshot(&self) -> Result<SorcerySnapshot, SorceryError> {
        Ok(SorcerySnapshot {
            devices: unsafe { self.retrieve_all(DEVICE_TYPE, "device")? },
            lines: unsafe { self.retrieve_all(LINE_TYPE, "line")? },
        })
    }

    pub fn shutdown_observers(&self) {
        self.observer_shutdown.call_once(|| {
            self.hook.close_admission();
            unsafe {
                if self.line_observer {
                    sys::ast_sorcery_observer_remove(
                        self.sorcery.as_ptr(),
                        LINE_TYPE.as_ptr(),
                        &LINE_OBSERVER,
                    );
                }
                if self.device_observer {
                    sys::ast_sorcery_observer_remove(
                        self.sorcery.as_ptr(),
                        DEVICE_TYPE.as_ptr(),
                        &DEVICE_OBSERVER,
                    );
                }
            }
            let mut slot = mutation_slot()
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            match &*slot {
                HookState::Active(registered) if Arc::ptr_eq(registered, &self.hook) => {
                    *slot = HookState::Vacant;
                }
                HookState::Registering => *slot = HookState::Vacant,
                _ => {}
            }
            drop(slot);
            self.hook.drain();
        });
    }

    fn register_type(
        &self,
        object_type: &'static CStr,
        label: &'static str,
    ) -> Result<(), SorceryError> {
        let mapping = unsafe {
            sys::__ast_sorcery_apply_default(
                self.sorcery.as_ptr(),
                object_type.as_ptr(),
                MODULE_NAME.as_ptr(),
                ASTDB_WIZARD.as_ptr(),
                ASTDB_PREFIX.as_ptr(),
            )
        };
        if mapping == sys::AST_SORCERY_APPLY_FAIL {
            return Err(SorceryError::MappingFailed { object_type: label });
        }
        if unsafe {
            sys::__ast_sorcery_object_register(
                self.sorcery.as_ptr(),
                object_type.as_ptr(),
                1,
                1,
                Some(object_alloc),
                None,
                Some(object_validate),
            )
        } != 0
        {
            return Err(SorceryError::ObjectRegistrationFailed { object_type: label });
        }
        unsafe {
            sys::ast_sorcery_object_set_copy_handler(
                self.sorcery.as_ptr(),
                object_type.as_ptr(),
                Some(object_copy),
            )
        };
        if unsafe {
            sys::ast_sorcery_object_fields_register(
                self.sorcery.as_ptr(),
                object_type.as_ptr(),
                FIELD_PATTERN.as_ptr(),
                Some(field_apply),
                Some(fields_export),
            )
        } != 0
        {
            unsafe {
                sys::ast_sorcery_object_unregister(self.sorcery.as_ptr(), object_type.as_ptr())
            };
            return Err(SorceryError::FieldRegistrationFailed { object_type: label });
        }
        Ok(())
    }

    unsafe fn retrieve_all(
        &self,
        object_type: &'static CStr,
        label: &'static str,
    ) -> Result<Vec<RawSorceryObject>, SorceryError> {
        let container = unsafe {
            sys::ast_sorcery_retrieve_by_fields(
                self.sorcery.as_ptr(),
                object_type.as_ptr(),
                sys::AST_RETRIEVE_FLAG_MULTIPLE | sys::AST_RETRIEVE_FLAG_ALL,
                ptr::null_mut(),
            )
        };
        let container = unsafe { Ao2Object::<sys::ao2_container>::from_owned(container.cast()) }
            .ok_or(SorceryError::RetrievalFailed { object_type: label })?;
        let mut iterator = unsafe { sys::ao2_iterator_init(container.as_ptr(), 0) };
        let result = (|| {
            let mut objects = Vec::new();
            loop {
                let object = unsafe {
                    sys::__ao2_iterator_next(
                        &mut iterator,
                        ptr::null(),
                        SOURCE_FILE.as_ptr(),
                        line!() as c_int,
                        SOURCE_FUNCTION.as_ptr(),
                    )
                }
                .cast::<StoredObject>();
                let Some(object) = (unsafe { Ao2Object::from_owned(object) }) else {
                    break;
                };
                if objects.len() >= MAX_SNAPSHOT_OBJECTS {
                    return Err(SorceryError::SnapshotLimit { object_type: label });
                }
                objects.push(unsafe { copy_object(object.as_ptr(), label) }?);
            }
            objects.sort_by(|left, right| left.id.cmp(&right.id));
            self.validate_astdb_ids(object_type, label, &objects)?;
            Ok(objects)
        })();
        unsafe { sys::ao2_iterator_destroy(&mut iterator) };
        result
    }

    fn validate_astdb_ids(
        &self,
        object_type: &'static CStr,
        label: &'static str,
        objects: &[RawSorceryObject],
    ) -> Result<(), SorceryError> {
        let Some(stored) = (unsafe { self.astdb_ids(object_type, label)? }) else {
            return Ok(());
        };
        let loaded = objects
            .iter()
            .map(|object| object.id.clone())
            .collect::<BTreeSet<_>>();
        if stored == loaded {
            Ok(())
        } else {
            Err(SorceryError::AstDbIntegrity {
                object_type: label,
                stored: stored.len(),
                loaded: loaded.len(),
            })
        }
    }

    unsafe fn astdb_ids(
        &self,
        object_type: &'static CStr,
        label: &'static str,
    ) -> Result<Option<BTreeSet<String>>, SorceryError> {
        let mut wizard = ptr::null_mut::<sys::ast_sorcery_wizard>();
        let mut data = ptr::null_mut::<c_void>();
        if unsafe {
            sys::ast_sorcery_get_wizard_mapping(
                self.sorcery.as_ptr(),
                object_type.as_ptr(),
                0,
                &mut wizard,
                &mut data,
            )
        } != 0
        {
            return Err(SorceryError::RetrievalFailed { object_type: label });
        }
        let wizard = unsafe { Ao2Object::<sys::ast_sorcery_wizard>::from_owned(wizard) }
            .ok_or(SorceryError::RetrievalFailed { object_type: label })?;
        let name = unsafe {
            copy_bounded_native_text(
                (*wizard.as_ptr()).name,
                MAX_FIELD_NAME_BYTES,
                format!("{label} wizard name"),
            )?
        };
        if name != "astdb" {
            return Ok(None);
        }
        let prefix = unsafe {
            copy_bounded_native_text(
                data.cast(),
                MAX_MAPPING_PREFIX_BYTES,
                format!("{label} AstDB prefix"),
            )?
        };
        if prefix.is_empty() {
            return Err(SorceryError::InvalidAstDbKey { object_type: label });
        }
        let family = format!("{prefix}/{label}");
        let family_c = CString::new(family.as_str())
            .map_err(|_| SorceryError::InvalidAstDbKey { object_type: label })?;
        let tree = AstDbTree(NonNull::new(unsafe {
            sys::ast_db_gettree(family_c.as_ptr(), ptr::null())
        }));
        let mut entry = tree.0.map_or(ptr::null_mut(), NonNull::as_ptr);
        let mut ids = BTreeSet::new();
        while let Some(current) = unsafe { entry.as_ref() } {
            if ids.len() >= MAX_SNAPSHOT_OBJECTS {
                return Err(SorceryError::SnapshotLimit { object_type: label });
            }
            let key = unsafe {
                copy_bounded_native_text(
                    current.key,
                    MAX_ASTDB_PATH_BYTES,
                    format!("{label} AstDB key"),
                )?
            };
            let id = astdb_key_id(&key, &family)
                .filter(|id| !id.is_empty() && id.len() <= MAX_OBJECT_ID_BYTES)
                .ok_or(SorceryError::InvalidAstDbKey { object_type: label })?;
            ids.insert(id.to_owned());
            entry = current.next;
        }
        Ok(Some(ids))
    }
}

impl Drop for SorceryRegistration {
    fn drop(&mut self) {
        self.shutdown_observers();
        unsafe {
            if self.line_type {
                sys::ast_sorcery_object_unregister(self.sorcery.as_ptr(), LINE_TYPE.as_ptr());
            }
            if self.device_type {
                sys::ast_sorcery_object_unregister(self.sorcery.as_ptr(), DEVICE_TYPE.as_ptr());
            }
            sys::__ao2_cleanup(self.sorcery.as_ptr().cast());
        }
    }
}

fn mutation_slot() -> &'static Mutex<HookState> {
    MUTATION_HOOK.get_or_init(|| Mutex::new(HookState::Vacant))
}

unsafe fn copy_bounded_native_text(
    pointer: *const std::ffi::c_char,
    maximum_bytes: usize,
    location: String,
) -> Result<String, SorceryError> {
    if pointer.is_null() {
        return Err(SorceryError::InvalidNativeText { location });
    }
    let value = unsafe { CStr::from_ptr(pointer) };
    if value.to_bytes().len() > maximum_bytes {
        return Err(SorceryError::InvalidNativeText { location });
    }
    value
        .to_str()
        .map(str::to_owned)
        .map_err(|_| SorceryError::InvalidNativeText { location })
}

fn astdb_key_id<'a>(key: &'a str, family: &str) -> Option<&'a str> {
    key.strip_prefix('/')?
        .strip_prefix(family)?
        .strip_prefix('/')
}

fn notify(kind: SorceryMutationKind, object_type: SorceryObjectType, object: *const c_void) {
    let registration = match &*mutation_slot()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
    {
        HookState::Active(registration) => Some(Arc::clone(registration)),
        HookState::Vacant | HookState::Registering => None,
    };
    let Some(registration) = registration else {
        return;
    };
    let Ok(lease) = registration.enter() else {
        return;
    };
    let id = unsafe { sys::ast_sorcery_object_get_id(object) };
    if id.is_null() {
        return;
    }
    let Ok(id) = (unsafe { CStr::from_ptr(id) }).to_str() else {
        return;
    };
    (lease.payload())(SorceryMutation {
        kind,
        object_type,
        id: id.to_owned(),
    });
}

macro_rules! observer_callback {
    ($name:ident, $kind:expr, $object_type:expr) => {
        unsafe extern "C" fn $name(object: *const c_void) {
            contain_callback_panic((), || notify($kind, $object_type, object));
        }
    };
}

observer_callback!(
    device_created,
    SorceryMutationKind::Created,
    SorceryObjectType::Device
);
observer_callback!(
    device_updated,
    SorceryMutationKind::Updated,
    SorceryObjectType::Device
);
observer_callback!(
    device_deleted,
    SorceryMutationKind::Deleted,
    SorceryObjectType::Device
);
observer_callback!(
    line_created,
    SorceryMutationKind::Created,
    SorceryObjectType::Line
);
observer_callback!(
    line_updated,
    SorceryMutationKind::Updated,
    SorceryObjectType::Line
);
observer_callback!(
    line_deleted,
    SorceryMutationKind::Deleted,
    SorceryObjectType::Line
);

static DEVICE_OBSERVER: sys::ast_sorcery_observer = sys::ast_sorcery_observer {
    created: Some(device_created),
    updated: Some(device_updated),
    deleted: Some(device_deleted),
    loaded: None,
};

static LINE_OBSERVER: sys::ast_sorcery_observer = sys::ast_sorcery_observer {
    created: Some(line_created),
    updated: Some(line_updated),
    deleted: Some(line_deleted),
    loaded: None,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn astdb_keys_require_the_exact_mapped_family() {
        assert_eq!(
            astdb_key_id("/chan_sccp2/device/SEP001122334455", "chan_sccp2/device"),
            Some("SEP001122334455")
        );
        assert_eq!(
            astdb_key_id("/chan_sccp2/line/1000", "chan_sccp2/device"),
            None
        );
        assert_eq!(
            astdb_key_id("chan_sccp2/device/SEP001122334455", "chan_sccp2/device"),
            None
        );
    }
}
