//! Explicit non-poisoning policy for the module's transactional runtime state.
//!
//! A panic is contained before it can cross an Asterisk callback. Runtime
//! registries are updated through prepare/commit or generation-checked state
//! transitions, so a poisoned standard-library lock does not make every later
//! callback panic again. We recover the protected value centrally and let the
//! owning transition's invariants decide whether the operation can proceed.

use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

pub trait MutexExt<T> {
    fn lock_unpoisoned(&self) -> MutexGuard<'_, T>;
}

impl<T> MutexExt<T> for Mutex<T> {
    fn lock_unpoisoned(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

pub trait RwLockExt<T> {
    fn read_unpoisoned(&self) -> RwLockReadGuard<'_, T>;
    fn write_unpoisoned(&self) -> RwLockWriteGuard<'_, T>;
}

impl<T> RwLockExt<T> for RwLock<T> {
    fn read_unpoisoned(&self) -> RwLockReadGuard<'_, T> {
        self.read().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write_unpoisoned(&self) -> RwLockWriteGuard<'_, T> {
        self.write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use super::*;

    #[test]
    fn a_contained_panic_does_not_turn_a_runtime_mutex_into_a_panic_loop() {
        let state = Mutex::new(Vec::new());
        let _ = catch_unwind(AssertUnwindSafe(|| {
            let mut state = state.lock().unwrap();
            state.push("committed-before-panic");
            panic!("simulated callback panic");
        }));

        state.lock_unpoisoned().push("next-callback");
        assert_eq!(
            *state.lock_unpoisoned(),
            ["committed-before-panic", "next-callback"]
        );
    }

    #[test]
    fn runtime_rwlock_uses_the_same_recovery_policy() {
        let state = RwLock::new(1);
        let _ = catch_unwind(AssertUnwindSafe(|| {
            let mut state = state.write().unwrap();
            *state = 2;
            panic!("simulated reload panic");
        }));

        *state.write_unpoisoned() = 3;
        assert_eq!(*state.read_unpoisoned(), 3);
    }
}
