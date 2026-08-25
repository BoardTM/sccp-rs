//! Shared ownership and shutdown rules for Rust-native Asterisk callbacks.
//!
//! Asterisk's manager, HTTP, and dialplan registries all have the same hard
//! lifetime boundary: each API publishes a stable descriptor and userdata
//! pointer, callbacks may already be running when unregister begins, and a
//! callback is allowed to unregister itself. Waiting from that callback would
//! deadlock; freeing its userdata would be a use-after-free.
//!
//! [`CallbackRegistration`] makes those rules independent of any one Asterisk
//! API. The allocation is reference counted, callback admission is bounded, and every
//! admitted callback owns a [`CallbackLease`]. Unregister first calls
//! [`CallbackRegistration::close_admission`], removes the Asterisk-visible
//! registration, and then calls [`CallbackRegistration::drain`]. Draining is
//! synchronous for an external caller and reports
//! [`ShutdownDisposition::DeferredToCallback`] to a callback unregistering
//! itself. That callback's strong reference and payload reference keep both
//! allocations alive until the lease is released. The one-step
//! [`CallbackRegistration::shutdown`] is provided for registries whose removal
//! is already complete.
//!
//! Native callback adapters must wrap the complete callback body (including lease
//! destruction) with [`contain_callback_panic`].  No Rust unwind may cross an
//! Asterisk ABI boundary.

use crate::asterisk::boundary::{CondvarExt as _, MutexExt as _};
use std::cell::RefCell;
use std::ffi::c_void;
use std::marker::PhantomData;
use std::num::NonZeroUsize;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

pub use crate::asterisk::boundary::contain_panic as contain_callback_panic;

thread_local! {
    /// A stack, rather than one current pointer, because one Asterisk callback
    /// can synchronously invoke another registration (or recurse).
    static ACTIVE_CALLBACKS: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

#[derive(Debug)]
struct Gate<T> {
    accepting: bool,
    active_callbacks: usize,
    deferred_drop: bool,
    payload: Option<Arc<T>>,
}

/// Read-only lifecycle information for diagnostics and tests.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CallbackSnapshot {
    accepting: bool,
    active_callbacks: usize,
    deferred_drop: bool,
    payload_retained: bool,
}

/// Admission failure at the Rust callback boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CallbackAdmissionError {
    ShuttingDown,
    Saturated,
}

/// Whether unregister completed synchronously or must finish on callback exit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShutdownDisposition {
    Drained,
    DeferredToCallback,
}

/// Reference-counted callback state shared by manager, HTTP, and dialplan.
///
/// Construct this only through [`CallbackRegistration::new`].  A native
/// registry retains one strong reference with [`retain_for_native`], and each
/// native callback acquires a temporary strong reference with
/// [`acquire_from_native`] before entering the gate.
pub struct CallbackRegistration<T> {
    maximum_active_callbacks: NonZeroUsize,
    gate: Mutex<Gate<T>>,
    drained: Condvar,
}

impl<T> CallbackRegistration<T> {
    pub fn new(maximum_active_callbacks: NonZeroUsize, payload: T) -> Arc<Self> {
        Arc::new(Self {
            maximum_active_callbacks,
            gate: Mutex::new(Gate {
                accepting: true,
                active_callbacks: 0,
                deferred_drop: false,
                payload: Some(Arc::new(payload)),
            }),
            drained: Condvar::new(),
        })
    }

    /// Retain the API-specific native descriptor while the registration owner
    /// unlinks it. This is intentionally separate from callback admission:
    /// unregister must be able to close admission before waiting for leases.
    pub fn payload_for_owner(&self) -> Option<Arc<T>> {
        self.lock_gate().payload.clone()
    }

    /// Admit one callback and retain its state and handler payload.
    pub fn enter(self: &Arc<Self>) -> Result<CallbackLease<T>, CallbackAdmissionError> {
        let mut gate = self.lock_gate();
        if !gate.accepting {
            return Err(CallbackAdmissionError::ShuttingDown);
        }
        if gate.active_callbacks >= self.maximum_active_callbacks.get() {
            return Err(CallbackAdmissionError::Saturated);
        }
        let payload = gate
            .payload
            .clone()
            .ok_or(CallbackAdmissionError::ShuttingDown)?;
        gate.active_callbacks += 1;
        drop(gate);

        let identity = self.identity();
        ACTIVE_CALLBACKS.with(|active| active.borrow_mut().push(identity));
        Ok(CallbackLease {
            registration: self.clone(),
            payload,
            identity,
            _not_send: PhantomData,
        })
    }

    /// Prevent a callback looked up after this point from entering user code.
    ///
    /// A native port calls this while it still owns the API-specific registry
    /// lock, then unlinks the Asterisk-visible registration before calling
    /// [`Self::drain`]. This preserves the established callback ordering.
    pub fn close_admission(&self) {
        self.lock_gate().accepting = false;
    }

    /// Drain callbacks when doing so cannot deadlock the calling thread.
    ///
    /// This operation is idempotent.  A callback unregistering its own
    /// registration gets `DeferredToCallback`; its lease performs the final
    /// notification and retains the payload and allocation until exit.
    pub fn drain(&self) -> ShutdownDisposition {
        let current_callback = self.is_active_on_current_thread();
        let mut gate = self.lock_gate();
        // Defensive as well as convenient: drain may safely be used without a
        // preceding API-specific unlink when a registry has no external entry.
        gate.accepting = false;
        let retired_payload = gate.payload.take();
        if gate.active_callbacks == 0 {
            gate.deferred_drop = false;
            drop(gate);
            drop(retired_payload);
            return ShutdownDisposition::Drained;
        }
        if current_callback {
            gate.deferred_drop = true;
            drop(gate);
            drop(retired_payload);
            return ShutdownDisposition::DeferredToCallback;
        }
        while gate.active_callbacks != 0 {
            gate = self.drained.wait_unpoisoned(gate);
        }
        gate.deferred_drop = false;
        drop(gate);
        drop(retired_payload);
        ShutdownDisposition::Drained
    }

    /// Close and drain in one step when no Asterisk-visible unlink must occur
    /// between those phases.
    pub fn shutdown(&self) -> ShutdownDisposition {
        self.close_admission();
        self.drain()
    }

    #[cfg(test)]
    fn snapshot(&self) -> CallbackSnapshot {
        let gate = self.lock_gate();
        CallbackSnapshot {
            accepting: gate.accepting,
            active_callbacks: gate.active_callbacks,
            deferred_drop: gate.deferred_drop,
            payload_retained: gate.payload.is_some(),
        }
    }

    fn identity(&self) -> usize {
        std::ptr::from_ref(self).addr()
    }

    fn is_active_on_current_thread(&self) -> bool {
        let identity = self.identity();
        ACTIVE_CALLBACKS.with(|active| active.borrow().contains(&identity))
    }

    fn lock_gate(&self) -> MutexGuard<'_, Gate<T>> {
        self.gate.lock_unpoisoned()
    }
}

/// An admitted callback. Dropping it releases one active-callback count.
///
/// The lease is deliberately `!Send`: self-unregister detection is thread
/// local and Asterisk callbacks are synchronous on the invoking thread.
pub struct CallbackLease<T> {
    registration: Arc<CallbackRegistration<T>>,
    payload: Arc<T>,
    identity: usize,
    _not_send: PhantomData<Rc<()>>,
}

impl<T> CallbackLease<T> {
    pub fn payload(&self) -> &T {
        &self.payload
    }

    #[cfg(test)]
    fn registration(&self) -> &Arc<CallbackRegistration<T>> {
        &self.registration
    }
}

impl<T> Drop for CallbackLease<T> {
    fn drop(&mut self) {
        ACTIVE_CALLBACKS.with(|active| {
            let mut active = active.borrow_mut();
            if let Some(index) = active
                .iter()
                .rposition(|identity| *identity == self.identity)
            {
                active.remove(index);
            } else {
                debug_assert!(false, "callback lease left a different thread");
            }
        });

        let mut gate = self.registration.lock_gate();
        debug_assert!(gate.active_callbacks != 0);
        gate.active_callbacks = gate.active_callbacks.saturating_sub(1);
        if gate.active_callbacks == 0 {
            gate.deferred_drop = false;
            self.registration.drained.notify_all();
        }
    }
}

/// Transfer one strong registration reference to a native registry.
///
/// The returned pointer must eventually be passed exactly once to
/// [`release_from_native`].  Until then, callbacks may use
/// [`acquire_from_native`] while the native registry guarantees that the
/// userdata is still registered or is an already-admitted callback.
pub fn retain_for_native<T>(registration: &Arc<CallbackRegistration<T>>) -> NonNull<c_void> {
    let retained = Arc::into_raw(registration.clone())
        .cast_mut()
        .cast::<c_void>();
    // SAFETY: `Arc::into_raw` returns the address of a live allocation and can
    // never produce a null pointer.
    unsafe { NonNull::new_unchecked(retained) }
}

/// Acquire a callback-owned strong reference from native userdata.
///
/// # Safety
///
/// `userdata` must be a non-null pointer returned by [`retain_for_native`],
/// and its retained native strong reference must not yet have been released.
/// Acquisition must be serialized with native unregister so those conditions
/// remain true through the strong-count increment.
pub unsafe fn acquire_from_native<T>(
    userdata: *mut c_void,
) -> Option<Arc<CallbackRegistration<T>>> {
    let pointer = NonNull::new(userdata)?.cast::<CallbackRegistration<T>>();
    // SAFETY: required by this function's contract; the native retained Arc
    // keeps the allocation and Arc metadata alive through both operations.
    unsafe { Arc::increment_strong_count(pointer.as_ptr()) };
    Some(unsafe { Arc::from_raw(pointer.as_ptr()) })
}

/// Release the strong reference previously transferred to native code.
///
/// # Safety
///
/// `userdata` must be either null or an unreleased pointer returned by
/// [`retain_for_native`] for the same `T`. It must be released exactly once,
/// after native lookup can no longer begin a new callback.
pub unsafe fn release_from_native<T>(userdata: *mut c_void) {
    let Some(pointer) = NonNull::new(userdata) else {
        return;
    };
    // SAFETY: the caller transfers back exactly the strong reference created
    // by retain_for_native and satisfies the type/exactly-once contract.
    drop(unsafe { Arc::from_raw(pointer.cast::<CallbackRegistration<T>>().as_ptr()) });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    #[derive(Clone)]
    struct DropCounter(Arc<AtomicUsize>);

    impl Drop for DropCounter {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn admission_is_bounded_and_shutdown_is_idempotent() {
        let registration = CallbackRegistration::new(NonZeroUsize::MIN, 7usize);
        let lease = registration.enter().unwrap();
        assert_eq!(*lease.payload(), 7);
        assert!(matches!(
            registration.enter(),
            Err(CallbackAdmissionError::Saturated)
        ));
        drop(lease);
        assert_eq!(registration.shutdown(), ShutdownDisposition::Drained);
        assert_eq!(registration.shutdown(), ShutdownDisposition::Drained);
        assert!(matches!(
            registration.enter(),
            Err(CallbackAdmissionError::ShuttingDown)
        ));
        assert_eq!(
            registration.snapshot(),
            CallbackSnapshot {
                accepting: false,
                active_callbacks: 0,
                deferred_drop: false,
                payload_retained: false,
            }
        );
    }

    #[test]
    fn external_shutdown_waits_for_every_inflight_callback() {
        let registration = CallbackRegistration::new(NonZeroUsize::MAX, ());
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let callback_registration = registration.clone();
        let callback = thread::spawn(move || {
            let _lease = callback_registration.enter().unwrap();
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        entered_rx.recv().unwrap();
        registration.close_admission();
        assert!(matches!(
            registration.enter(),
            Err(CallbackAdmissionError::ShuttingDown)
        ));

        let (drained_tx, drained_rx) = mpsc::channel();
        let shutdown_registration = registration.clone();
        let shutdown = thread::spawn(move || {
            drained_tx.send(shutdown_registration.drain()).unwrap();
        });
        assert!(drained_rx.recv_timeout(Duration::from_millis(50)).is_err());
        assert!(!registration.snapshot().accepting);
        release_tx.send(()).unwrap();
        assert_eq!(
            drained_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            ShutdownDisposition::Drained
        );
        callback.join().unwrap();
        shutdown.join().unwrap();
    }

    #[test]
    fn callback_time_shutdown_defers_payload_and_allocation_drop() {
        let drops = Arc::new(AtomicUsize::new(0));
        let registration = CallbackRegistration::new(NonZeroUsize::MAX, DropCounter(drops.clone()));
        let lease = registration.enter().unwrap();
        assert_eq!(
            lease.registration().shutdown(),
            ShutdownDisposition::DeferredToCallback
        );
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        assert_eq!(
            registration.snapshot(),
            CallbackSnapshot {
                accepting: false,
                active_callbacks: 1,
                deferred_drop: true,
                payload_retained: false,
            }
        );
        drop(registration);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(lease);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn nested_callbacks_are_recognized_as_self_unregister() {
        let registration = CallbackRegistration::new(NonZeroUsize::MAX, ());
        let outer = registration.enter().unwrap();
        let inner = registration.enter().unwrap();
        assert_eq!(
            inner.registration().shutdown(),
            ShutdownDisposition::DeferredToCallback
        );
        drop(inner);
        assert_eq!(registration.snapshot().active_callbacks, 1);
        drop(outer);
        assert_eq!(registration.snapshot().active_callbacks, 0);
        assert!(!registration.snapshot().deferred_drop);
    }

    #[test]
    fn native_reference_survives_self_unregister_until_callback_exit() {
        let drops = Arc::new(AtomicUsize::new(0));
        let registration = CallbackRegistration::new(NonZeroUsize::MIN, DropCounter(drops.clone()));
        let userdata = retain_for_native(&registration);
        let callback = unsafe { acquire_from_native::<DropCounter>(userdata.as_ptr()) }.unwrap();
        let lease = callback.enter().unwrap();
        assert_eq!(callback.shutdown(), ShutdownDisposition::DeferredToCallback);
        unsafe { release_from_native::<DropCounter>(userdata.as_ptr()) };
        drop(registration);
        drop(callback);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(lease);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn panic_is_contained_after_the_callback_lease_is_released() {
        let registration = CallbackRegistration::new(NonZeroUsize::MIN, ());
        let callback_registration = registration.clone();
        let result = contain_callback_panic(-1, move || {
            let _lease = callback_registration.enter().unwrap();
            panic!("callback panic")
        });
        assert_eq!(result, -1);
        assert_eq!(registration.snapshot().active_callbacks, 0);
        assert_eq!(registration.shutdown(), ShutdownDisposition::Drained);
    }
}
