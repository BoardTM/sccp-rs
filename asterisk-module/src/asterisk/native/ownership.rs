//! Shared RAII owners for native Asterisk configuration allocations.

use std::ptr::NonNull;

use crate::asterisk::sys;

type ConfigDestroy = unsafe fn(*mut sys::ast_config);

unsafe fn destroy_config(config: *mut sys::ast_config) {
    unsafe { sys::ast_config_destroy(config) };
}

pub(super) struct AstConfigOwner {
    pointer: NonNull<sys::ast_config>,
    destroy: ConfigDestroy,
}

impl AstConfigOwner {
    pub(super) fn new(pointer: NonNull<sys::ast_config>) -> Self {
        Self {
            pointer,
            destroy: destroy_config,
        }
    }

    pub(super) const fn as_ptr(&self) -> *mut sys::ast_config {
        self.pointer.as_ptr()
    }

    #[cfg(test)]
    fn with_destroy(pointer: NonNull<sys::ast_config>, destroy: ConfigDestroy) -> Self {
        Self { pointer, destroy }
    }
}

impl Drop for AstConfigOwner {
    fn drop(&mut self) {
        unsafe { (self.destroy)(self.pointer.as_ptr()) };
    }
}

pub(super) enum ConfigLoad {
    Missing,
    Unchanged,
    Invalid,
    Loaded(AstConfigOwner),
}

impl ConfigLoad {
    pub(super) fn decode(value: *mut sys::ast_config) -> Self {
        Self::decode_with(value, destroy_config)
    }

    fn decode_with(value: *mut sys::ast_config, destroy: ConfigDestroy) -> Self {
        match value as isize {
            0 => Self::Missing,
            -1 => Self::Unchanged,
            -2 => Self::Invalid,
            _ => NonNull::new(value).map_or(Self::Missing, |pointer| {
                Self::Loaded(AstConfigOwner::with_destroy_or_production(pointer, destroy))
            }),
        }
    }
}

impl AstConfigOwner {
    fn with_destroy_or_production(
        pointer: NonNull<sys::ast_config>,
        destroy: ConfigDestroy,
    ) -> Self {
        #[cfg(test)]
        {
            return Self::with_destroy(pointer, destroy);
        }
        #[cfg(not(test))]
        {
            let _ = destroy;
            Self::new(pointer)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    static DESTROYS: AtomicUsize = AtomicUsize::new(0);
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    unsafe fn count_destroy(_: *mut sys::ast_config) {
        DESTROYS.fetch_add(1, Ordering::SeqCst);
    }

    #[test]
    fn every_configuration_sentinel_is_decoded_before_ownership() {
        let _guard = TEST_LOCK.lock().unwrap();
        DESTROYS.store(0, Ordering::SeqCst);
        assert!(matches!(
            ConfigLoad::decode_with(std::ptr::null_mut(), count_destroy),
            ConfigLoad::Missing
        ));
        assert!(matches!(
            ConfigLoad::decode_with((-1_isize) as *mut sys::ast_config, count_destroy),
            ConfigLoad::Unchanged
        ));
        assert!(matches!(
            ConfigLoad::decode_with((-2_isize) as *mut sys::ast_config, count_destroy),
            ConfigLoad::Invalid
        ));
        assert_eq!(DESTROYS.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn loaded_configuration_is_destroyed_exactly_once() {
        let _guard = TEST_LOCK.lock().unwrap();
        DESTROYS.store(0, Ordering::SeqCst);
        let pointer = NonNull::<sys::ast_config>::dangling().as_ptr();
        let loaded = ConfigLoad::decode_with(pointer, count_destroy);
        assert!(matches!(loaded, ConfigLoad::Loaded(_)));
        drop(loaded);
        assert_eq!(DESTROYS.load(Ordering::SeqCst), 1);
    }
}
