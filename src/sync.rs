//! The locks of the threads.

use std::sync::{Mutex, MutexGuard, PoisonError};

/// Lock `mutex`. If a thread panicked with the lock, keep its data: the
/// other threads go on.
pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
