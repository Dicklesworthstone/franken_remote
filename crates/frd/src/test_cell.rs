//! `Cell` for test state that a host session's closures capture. Host session
//! futures are Send, so those closures cannot hold a `&Cell`; this has the
//! same `get`, `set` and `replace` over a mutex.
use std::sync::{Mutex, MutexGuard, PoisonError};

pub(crate) struct SendCell<T>(Mutex<T>);

impl<T: Copy> SendCell<T> {
    pub(crate) const fn new(value: T) -> Self {
        Self(Mutex::new(value))
    }
    fn lock(&self) -> MutexGuard<'_, T> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
    pub(crate) fn get(&self) -> T {
        *self.lock()
    }
    pub(crate) fn set(&self, value: T) {
        *self.lock() = value;
    }
    pub(crate) fn replace(&self, value: T) -> T {
        std::mem::replace(&mut *self.lock(), value)
    }
}
