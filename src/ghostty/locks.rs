//! Shared policy for poisoned mutexes.

use std::sync::{Mutex, MutexGuard, PoisonError};

/// Poisoned auxiliary locks protect recoverable bookkeeping and keep using
/// the inner value. The terminal core has emulator invariants that a panic
/// may have interrupted, so its poisoned guard is never exposed.
pub(crate) fn lock_auxiliary<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => recover_auxiliary_poison(poisoned),
    }
}

/// Nonblocking form of [`lock_auxiliary`]. `None` means another thread owns
/// the lock; poison is recovered just like the blocking form.
#[cfg(test)]
pub(crate) fn try_lock_auxiliary<T>(mutex: &Mutex<T>) -> Option<MutexGuard<'_, T>> {
    match mutex.try_lock() {
        Ok(guard) => Some(guard),
        Err(std::sync::TryLockError::WouldBlock) => None,
        Err(std::sync::TryLockError::Poisoned(poisoned)) => {
            Some(recover_auxiliary_poison(poisoned))
        }
    }
}

pub(crate) fn recover_auxiliary_poison<T>(poisoned: PoisonError<T>) -> T {
    poisoned.into_inner()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TerminalCorePoisoned;

pub(crate) fn lock_terminal_core<T>(
    mutex: &Mutex<T>,
) -> Result<MutexGuard<'_, T>, TerminalCorePoisoned> {
    mutex.lock().map_err(|_| TerminalCorePoisoned)
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TerminalCoreTryLockError {
    WouldBlock,
    Poisoned,
}

#[cfg(test)]
pub(crate) fn try_lock_terminal_core<T>(
    mutex: &Mutex<T>,
) -> Result<MutexGuard<'_, T>, TerminalCoreTryLockError> {
    match mutex.try_lock() {
        Ok(guard) => Ok(guard),
        Err(std::sync::TryLockError::WouldBlock) => Err(TerminalCoreTryLockError::WouldBlock),
        Err(std::sync::TryLockError::Poisoned(_)) => Err(TerminalCoreTryLockError::Poisoned),
    }
}

pub(crate) fn terminal_core_is_poisoned<T>(mutex: &Mutex<T>) -> bool {
    mutex.is_poisoned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn poison<T>(mutex: &Mutex<T>) {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = mutex.lock().expect("test mutex starts unpoisoned");
            panic!("poison lock for policy test");
        }));
    }

    #[test]
    fn poisoned_auxiliary_lock_recovers_the_inner_guard() {
        let mutex = Mutex::new(7);
        poison(&mutex);

        assert_eq!(*lock_auxiliary(&mutex), 7);
        assert_eq!(*try_lock_auxiliary(&mutex).expect("lock is available"), 7);
    }

    #[test]
    fn poisoned_terminal_core_lock_never_returns_a_guard() {
        let mutex = Mutex::new(7);
        poison(&mutex);

        assert!(matches!(
            lock_terminal_core(&mutex),
            Err(TerminalCorePoisoned)
        ));
        assert!(matches!(
            try_lock_terminal_core(&mutex),
            Err(TerminalCoreTryLockError::Poisoned)
        ));
        assert!(terminal_core_is_poisoned(&mutex));
    }
}
