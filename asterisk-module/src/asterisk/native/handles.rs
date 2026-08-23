//! Shared RAII ownership for Asterisk-managed allocations and channels.

use std::ffi::{CStr, c_char, c_int, c_void};
use std::mem::{self, ManuallyDrop};
use std::ptr::{self, NonNull};
use std::rc::Rc;

use crate::asterisk::sys;

const SOURCE_FILE: &CStr = c"asterisk/native/handles.rs";
const SOURCE_FUNCTION: &CStr = c"sccp_native_handle";
const SOURCE_VARIABLE: &CStr = c"channel";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ChannelLockError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct NativeStatus(c_int);

impl NativeStatus {
    pub(super) const fn new(value: c_int) -> Self {
        Self(value)
    }

    pub(super) const fn is_success(self) -> bool {
        self.0 == 0
    }

    pub(super) fn result<E>(self, error: E) -> Result<(), E> {
        self.is_success().then_some(()).ok_or(error)
    }
}

/// One running reference that prevents the owning module from unloading while
/// a native resource or detached operation is live.
pub(super) struct ModuleReference {
    module: NonNull<sys::ast_module>,
}

impl ModuleReference {
    pub(super) unsafe fn acquire(module: *mut sys::ast_module) -> Option<Self> {
        NonNull::new(unsafe {
            sys::__ast_module_running_ref(
                module,
                SOURCE_FILE.as_ptr(),
                line!() as c_int,
                SOURCE_FUNCTION.as_ptr(),
            )
        })
        .map(|module| Self { module })
    }
}

// Asterisk module references are explicitly retained for detached native
// worker threads and released by whichever thread drops the owner.
unsafe impl Send for ModuleReference {}

impl Drop for ModuleReference {
    fn drop(&mut self) {
        unsafe {
            sys::__ast_module_unref(
                self.module.as_ptr(),
                SOURCE_FILE.as_ptr(),
                line!() as c_int,
                SOURCE_FUNCTION.as_ptr(),
            );
        }
    }
}

/// One owned AO2 reference to an Asterisk channel.
#[derive(Clone)]
pub struct ChannelRef(Ao2Object<sys::ast_channel>);

impl ChannelRef {
    /// Acquire one reference from a live borrowed channel.
    pub unsafe fn acquire(channel: *mut sys::ast_channel) -> Option<Self> {
        unsafe { Ao2Object::acquire(channel) }.map(Self)
    }

    /// Take ownership of one reference already transferred by Asterisk.
    pub unsafe fn from_owned(channel: *mut sys::ast_channel) -> Option<Self> {
        unsafe { Ao2Object::from_owned(channel) }.map(Self)
    }

    pub const fn as_ptr(&self) -> *mut sys::ast_channel {
        self.0.as_ptr()
    }

    pub const fn as_non_null(&self) -> NonNull<sys::ast_channel> {
        self.0.as_non_null()
    }

    /// Transfers this owned reference into a native API that consumes it.
    pub(super) fn into_raw(self) -> *mut sys::ast_channel {
        self.0.into_raw()
    }
}

// Asterisk explicitly permits channel references to cross its worker threads.
// Mutating access to channel state still requires Asterisk's channel lock.
unsafe impl Send for ChannelRef {}

/// A same-thread Asterisk channel lock retaining its channel for the complete
/// critical section.
pub(super) struct ChannelLock {
    channel: ChannelRef,
    _same_thread: std::marker::PhantomData<Rc<()>>,
}

unsafe fn lock_channel(channel: NonNull<sys::ast_channel>, try_only: bool) -> NativeStatus {
    let result = if try_only {
        unsafe {
            sys::__ao2_trylock(
                channel.as_ptr().cast(),
                sys::AO2_LOCK_REQ_MUTEX,
                SOURCE_FILE.as_ptr(),
                SOURCE_FUNCTION.as_ptr(),
                line!() as c_int,
                SOURCE_VARIABLE.as_ptr(),
            )
        }
    } else {
        unsafe {
            sys::__ao2_lock(
                channel.as_ptr().cast(),
                sys::AO2_LOCK_REQ_MUTEX,
                SOURCE_FILE.as_ptr(),
                SOURCE_FUNCTION.as_ptr(),
                line!() as c_int,
                SOURCE_VARIABLE.as_ptr(),
            )
        }
    };
    NativeStatus::new(result)
}

unsafe fn unlock_channel(channel: NonNull<sys::ast_channel>) {
    unsafe {
        sys::__ao2_unlock(
            channel.as_ptr().cast(),
            SOURCE_FILE.as_ptr(),
            SOURCE_FUNCTION.as_ptr(),
            line!() as c_int,
            SOURCE_VARIABLE.as_ptr(),
        );
    }
}

impl ChannelLock {
    /// Take ownership of a channel reference returned already locked by an
    /// Asterisk API such as the group-pickup lookup.
    pub(super) unsafe fn from_locked(channel: ChannelRef) -> Self {
        Self {
            channel,
            _same_thread: std::marker::PhantomData,
        }
    }

    pub(super) fn acquire(channel: ChannelRef) -> Result<Self, ChannelRef> {
        if unsafe { lock_channel(channel.as_non_null(), false) }.is_success() {
            Ok(Self {
                channel,
                _same_thread: std::marker::PhantomData,
            })
        } else {
            Err(channel)
        }
    }

    pub(super) fn try_acquire(channel: ChannelRef) -> Result<Self, ChannelRef> {
        if unsafe { lock_channel(channel.as_non_null(), true) }.is_success() {
            Ok(Self {
                channel,
                _same_thread: std::marker::PhantomData,
            })
        } else {
            Err(channel)
        }
    }

    pub(super) const fn as_ptr(&self) -> *mut sys::ast_channel {
        self.channel.as_ptr()
    }

    pub(super) fn clone_channel(&self) -> ChannelRef {
        self.channel.clone()
    }

    /// Unlock explicitly and return the still-owned channel reference.
    pub(super) fn unlock(self) -> ChannelRef {
        let this = ManuallyDrop::new(self);
        unsafe { unlock_channel(this.channel.as_non_null()) };
        unsafe { std::ptr::read(&this.channel) }
    }
}

impl Drop for ChannelLock {
    fn drop(&mut self) {
        unsafe { unlock_channel(self.channel.as_non_null()) };
    }
}

/// A same-thread lock over a channel whose lifetime and AO2 reference are
/// retained by the caller or enclosing Asterisk callback.
pub(super) struct BorrowedChannelLock {
    channel: NonNull<sys::ast_channel>,
    _same_thread: std::marker::PhantomData<Rc<()>>,
}

impl BorrowedChannelLock {
    pub(super) unsafe fn from_locked(channel: NonNull<sys::ast_channel>) -> Self {
        Self {
            channel,
            _same_thread: std::marker::PhantomData,
        }
    }

    pub(super) unsafe fn acquire(
        channel: NonNull<sys::ast_channel>,
    ) -> Result<Self, ChannelLockError> {
        unsafe { lock_channel(channel, false) }.result(ChannelLockError)?;
        Ok(unsafe { Self::from_locked(channel) })
    }

    pub(super) unsafe fn try_acquire(
        channel: NonNull<sys::ast_channel>,
    ) -> Result<Self, ChannelLockError> {
        unsafe { lock_channel(channel, true) }.result(ChannelLockError)?;
        Ok(unsafe { Self::from_locked(channel) })
    }
}

impl Drop for BorrowedChannelLock {
    fn drop(&mut self) {
        unsafe { unlock_channel(self.channel) };
    }
}

/// One owned AO2 reference returned by an Asterisk API.
pub(super) struct Ao2Object<T>(NonNull<T>);

impl<T> Ao2Object<T> {
    pub(super) unsafe fn acquire(pointer: *mut T) -> Option<Self> {
        let object = NonNull::new(pointer)?;
        Some(unsafe { Self::from_borrowed(object) })
    }

    pub(super) unsafe fn from_borrowed(object: NonNull<T>) -> Self {
        unsafe {
            sys::__ao2_ref(
                object.as_ptr().cast(),
                1,
                ptr::null(),
                SOURCE_FILE.as_ptr(),
                line!() as c_int,
                SOURCE_FUNCTION.as_ptr(),
            );
        }
        Self(object)
    }

    pub(super) unsafe fn from_owned(pointer: *mut T) -> Option<Self> {
        NonNull::new(pointer).map(Self)
    }

    pub(super) const fn as_ptr(&self) -> *mut T {
        self.0.as_ptr()
    }

    pub(super) fn into_raw(self) -> *mut T {
        ManuallyDrop::new(self).as_ptr()
    }

    const fn as_non_null(&self) -> NonNull<T> {
        self.0
    }
}

impl<T> Clone for Ao2Object<T> {
    fn clone(&self) -> Self {
        unsafe { Self::from_borrowed(self.as_non_null()) }
    }
}

impl<T> Drop for Ao2Object<T> {
    fn drop(&mut self) {
        unsafe { sys::__ao2_cleanup(self.as_ptr().cast()) };
    }
}

/// An optional `ast_strdup` allocation that can transfer ownership into an
/// Asterisk party structure.
pub(super) struct AsteriskString(*mut c_char);

impl AsteriskString {
    pub(super) const fn absent() -> Self {
        Self(ptr::null_mut())
    }

    pub(super) unsafe fn duplicate(value: &CStr) -> Option<Self> {
        let value = unsafe {
            sys::__ast_strdup(
                value.as_ptr(),
                SOURCE_FILE.as_ptr(),
                line!() as c_int,
                SOURCE_FUNCTION.as_ptr(),
            )
        };
        (!value.is_null()).then_some(Self(value))
    }

    pub(super) fn take(mut self) -> *mut c_char {
        mem::replace(&mut self.0, ptr::null_mut())
    }
}

impl Drop for AsteriskString {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                sys::__ast_free(
                    self.0.cast(),
                    SOURCE_FILE.as_ptr(),
                    line!() as c_int,
                    SOURCE_FUNCTION.as_ptr(),
                );
            }
        }
    }
}

/// One allocation returned by an Asterisk API and released with `ast_free`.
pub struct AsteriskAllocation<T>(NonNull<T>);

impl<T> AsteriskAllocation<T> {
    /// Take ownership of an allocation returned by Asterisk.
    pub unsafe fn from_owned(pointer: *mut T) -> Option<Self> {
        NonNull::new(pointer).map(Self)
    }

    pub const fn as_ptr(&self) -> *mut T {
        self.0.as_ptr()
    }

    /// Transfer ownership back to an Asterisk API that consumes the
    /// allocation.
    pub fn into_raw(self) -> *mut T {
        let this = ManuallyDrop::new(self);
        this.0.as_ptr()
    }
}

impl<T> Drop for AsteriskAllocation<T> {
    fn drop(&mut self) {
        unsafe {
            sys::__ast_free(
                self.as_ptr().cast::<c_void>(),
                SOURCE_FILE.as_ptr(),
                line!() as c_int,
                SOURCE_FUNCTION.as_ptr(),
            );
        }
    }
}
