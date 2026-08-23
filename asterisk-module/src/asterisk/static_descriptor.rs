//! Address-stable storage for descriptors registered with Asterisk.

use std::cell::UnsafeCell;
use std::mem::MaybeUninit;

/// Writable storage whose address remains stable for the loaded module's
/// lifetime.
#[repr(transparent)]
pub(super) struct StaticDescriptor<T>(UnsafeCell<MaybeUninit<T>>);

impl<T> StaticDescriptor<T> {
    pub(super) const fn uninit() -> Self {
        Self(UnsafeCell::new(MaybeUninit::uninit()))
    }

    /// Initializes the descriptor during a serialized registration attempt.
    pub(super) unsafe fn write(&self, value: T) -> *mut T {
        // SAFETY: the caller owns the one-time module registration transition.
        unsafe { (*self.0.get()).write(value) }
    }

    /// Returns the stable pointer after successful initialization.
    pub(super) unsafe fn as_ptr(&self) -> *mut T {
        // SAFETY: the caller guarantees `write` completed first.
        unsafe { (*self.0.get()).as_mut_ptr() }
    }
}

// The descriptor is mutated only during the loader's serialized constructor
// transition and by Asterisk according to the descriptor ABI.
unsafe impl<T> Sync for StaticDescriptor<T> {}
