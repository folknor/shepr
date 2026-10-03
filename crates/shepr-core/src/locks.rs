//! Poison recovery for auxiliary bookkeeping mutexes.

use std::sync::{Mutex, MutexGuard, PoisonError};

/// Poisoned auxiliary locks protect recoverable bookkeeping and keep using
/// the inner value. Use this only for state whose invariants survive a panic.
pub fn lock_auxiliary<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => recover_auxiliary_poison(poisoned),
    }
}

/// Nonblocking form of [`lock_auxiliary`]. `None` means another thread owns
/// the lock; poison is recovered just like the blocking form.
pub fn try_lock_auxiliary<T>(mutex: &Mutex<T>) -> Option<MutexGuard<'_, T>> {
    match mutex.try_lock() {
        Ok(guard) => Some(guard),
        Err(std::sync::TryLockError::WouldBlock) => None,
        Err(std::sync::TryLockError::Poisoned(poisoned)) => {
            Some(recover_auxiliary_poison(poisoned))
        }
    }
}

pub fn recover_auxiliary_poison<T>(poisoned: PoisonError<T>) -> T {
    poisoned.into_inner()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poisoned_auxiliary_lock_recovers_the_inner_guard() {
        let mutex = std::sync::Arc::new(Mutex::new(7));
        let poisoner = std::sync::Arc::clone(&mutex);
        let outcome = std::thread::spawn(move || {
            let _guard = poisoner.lock().expect("initially unpoisoned");
            panic!("poison auxiliary lock");
        })
        .join();
        assert!(outcome.is_err());
        assert_eq!(*lock_auxiliary(&mutex), 7);
        assert_eq!(*try_lock_auxiliary(&mutex).expect("available"), 7);
    }
}
