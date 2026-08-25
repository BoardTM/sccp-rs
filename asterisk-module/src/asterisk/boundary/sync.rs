// Shared poison-recovery policy for callback and runtime boundary state.
//
// Panics are contained before crossing Asterisk callbacks. Recovering the
// protected value here prevents one poisoned standard-library lock from
// turning every later callback into another panic.

use std::sync::{Condvar, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

pub(in super::super) trait MutexExt<T> {
    fn lock_unpoisoned(&self) -> MutexGuard<'_, T>;
}

impl<T> MutexExt<T> for Mutex<T> {
    fn lock_unpoisoned(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

pub(in super::super) trait RwLockExt<T> {
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

pub(in super::super) trait CondvarExt {
    fn wait_unpoisoned<'a, T>(&self, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T>;
}

impl CondvarExt for Condvar {
    fn wait_unpoisoned<'a, T>(&self, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
        self.wait(guard)
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use super::*;

    #[test]
    fn mutex_recovers_after_a_contained_callback_panic() {
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
    fn rwlock_uses_the_same_recovery_policy() {
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
