//! Mutex handling for PTY bookkeeping.

use std::sync::{Mutex, MutexGuard};

pub(crate) fn lock_auxiliary<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}
