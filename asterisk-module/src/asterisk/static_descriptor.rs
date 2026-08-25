//! Address-stable storage for descriptors registered with Asterisk.

use std::cell::UnsafeCell;
use std::mem::MaybeUninit;

/// Writable storage whose address remains stable for the loaded module's
/// lifetime.
#[repr(transparent)]
pub(super) struct StaticDescriptor<T: Copy>(UnsafeCell<MaybeUninit<T>>);

impl<T: Copy> StaticDescriptor<T> {
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

// Only descriptor POD may use this retry-overwritable storage. The descriptor
// is mutated during the loader's serialized constructor transition and by
// Asterisk according to the descriptor ABI.
unsafe impl<T: Copy> Sync for StaticDescriptor<T> {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_descriptors_may_be_rewritten_for_registration_retry() {
        let descriptor = StaticDescriptor::uninit();
        unsafe {
            descriptor.write([1_u32, 2]);
            assert_eq!(*descriptor.as_ptr(), [1, 2]);
            descriptor.write([3_u32, 4]);
            assert_eq!(*descriptor.as_ptr(), [3, 4]);
        }
    }
}
