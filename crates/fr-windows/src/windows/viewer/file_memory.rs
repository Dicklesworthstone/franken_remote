//! One bounded copy of a stable Win32 global allocation. The caller retains
//! its clipboard borrow or drop custody until this function has unlocked it.

use crate::file_choice::{Error, MAX_COPIED_BYTES};
use windows::Win32::{
    Foundation::{ERROR_SUCCESS, GetLastError, HGLOBAL, SetLastError},
    System::Memory::{GlobalLock, GlobalSize, GlobalUnlock},
};

/// # Safety
/// The caller owns or exclusively borrows this global allocation and prevents
/// it being freed, resized or mutated until this call returns. No native
/// pointer escapes; success includes release of this function's native lock.
pub(super) unsafe fn copy(global: HGLOBAL) -> Result<Vec<u8>, Error> {
    // SAFETY: the caller retains this stable original global allocation.
    let bytes = unsafe { GlobalSize(global) };
    if !(20..=MAX_COPIED_BYTES).contains(&bytes) {
        return Err(Error::InvalidPath);
    }
    // SAFETY: the same stable allocation remains owned by the caller. Bound
    // the full allocation before exposing a slice or allocating a byte copy.
    let pointer = unsafe { GlobalLock(global) };
    if pointer.is_null() {
        return Err(Error::Native);
    }
    let locked = Locked(Some(global));
    // SAFETY: GlobalSize established this bounded allocation's length, and
    // GlobalLock pins its start. The caller prevents concurrent mutation.
    let copied = unsafe { std::slice::from_raw_parts(pointer.cast::<u8>(), bytes) }.to_vec();
    locked.release()?;
    Ok(copied)
}

struct Locked(Option<HGLOBAL>);
impl Locked {
    fn release(mut self) -> Result<(), Error> {
        self.unlock()
    }

    fn unlock(&mut self) -> Result<(), Error> {
        let Some(global) = self.0 else {
            return Ok(());
        };
        // SAFETY: releases exactly the successful GlobalLock in copy before
        // the caller releases its original allocation. FALSE with zero last
        // error means the final lock was released successfully.
        let (unlocked, error) = unsafe {
            SetLastError(ERROR_SUCCESS);
            let unlocked = GlobalUnlock(global);
            (unlocked, GetLastError())
        };
        if unlocked.is_err() && error != ERROR_SUCCESS {
            return Err(Error::Cleanup);
        }
        self.0 = None;
        Ok(())
    }
}
impl Drop for Locked {
    fn drop(&mut self) {
        let _ = self.unlock();
    }
}
