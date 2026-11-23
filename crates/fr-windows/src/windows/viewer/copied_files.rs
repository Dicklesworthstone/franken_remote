//! A read-only clipboard borrow for one explicit Send copied files gesture.
//! Copy bounded `CF_HDROP` bytes, unlock and close before opening any source.
//! The clipboard owns its HGLOBAL throughout; it is never freed here.

use crate::file_choice::{Error, MAX_COPIED_BYTES};
use std::{thread, time::Duration};
use windows::Win32::{
    Foundation::{ERROR_SUCCESS, GetLastError, HGLOBAL, SetLastError},
    System::{
        DataExchange::{
            CloseClipboard, GetClipboardData, GetClipboardSequenceNumber,
            IsClipboardFormatAvailable, OpenClipboard,
        },
        Memory::{GlobalLock, GlobalSize, GlobalUnlock},
    },
};

const FILES: u32 = 15; // CF_HDROP, a clipboard-owned DROPFILES allocation.
const OPEN_ATTEMPTS: usize = 20;
const OPEN_RETRY: Duration = Duration::from_millis(5);

pub(super) fn sequence() -> u32 {
    // SAFETY: reads only the window station's scalar clipboard sequence.
    unsafe { GetClipboardSequenceNumber() }
}

/// `check` only reads cancellation and the original gesture clock. It must not
/// acquire authority locks while the clipboard is borrowed. Delayed rendering
/// may block `GetClipboardData`, so this entire function runs on the existing
/// single, retained preparation worker, never on the UI or authority thread.
pub(super) fn read(
    expected: u32,
    mut check: impl FnMut() -> Result<(), Error>,
) -> Result<Vec<u8>, Error> {
    if expected == 0 {
        return Err(Error::Native);
    }
    let mut open = None;
    for attempt in 0..OPEN_ATTEMPTS {
        check()?;
        if sequence() != expected {
            return Err(Error::ClipboardChanged);
        }
        // SAFETY: this worker borrows the clipboard read-only. It does not
        // empty it, change its owner or register a native window callback.
        if unsafe { OpenClipboard(None) }.is_ok() {
            open = Some(Open(true));
            break;
        }
        if attempt + 1 < OPEN_ATTEMPTS {
            thread::sleep(OPEN_RETRY);
        }
    }
    let open = open.ok_or(Error::ClipboardBusy)?;
    let result = (|| {
        check()?;
        // OpenClipboard prevents another program replacing the selection.
        // Check the originally captured sequence after obtaining that borrow.
        if sequence() != expected {
            return Err(Error::ClipboardChanged);
        }
        // SAFETY: a format query under this thread's clipboard borrow.
        if unsafe { IsClipboardFormatAvailable(FILES) }.is_err() {
            return Err(Error::NoCopiedFiles);
        }
        // SAFETY: the clipboard is open. The result remains clipboard-owned
        // and is used only within this borrow; no DragFinish/GlobalFree occurs.
        let handle = unsafe { GetClipboardData(FILES) }.map_err(|_| Error::Native)?;
        check()?;
        // Rendering a delayed format may increment the sequence itself. The
        // exclusive borrow still protects this original selection, so do not
        // mistake its rendering for another copy or retry against a new value.
        let global = HGLOBAL(handle.0);
        // SAFETY: CF_HDROP is a global allocation; GlobalSize validates its
        // handle. Bound the full allocation before any slice or allocation.
        let bytes = unsafe { GlobalSize(global) };
        if !(20..=MAX_COPIED_BYTES).contains(&bytes) {
            return Err(Error::InvalidPath);
        }
        // SAFETY: this live clipboard-owned allocation stays valid until the
        // clipboard closes. The successful lock is released below before that.
        let pointer = unsafe { GlobalLock(global) };
        if pointer.is_null() {
            return Err(Error::Native);
        }
        let locked = Locked(Some(global));
        // SAFETY: GlobalSize established the bounded allocation length and
        // GlobalLock pins its start. The clipboard remains exclusively open;
        // only an owned byte copy escapes, never this pointer or its handle.
        let copied = unsafe { std::slice::from_raw_parts(pointer.cast::<u8>(), bytes) }.to_vec();
        locked.release()?;
        Ok(copied)
    })();
    // A cleanup failure overrides any apparent selection success. The caller
    // already treats Error::Cleanup as terminal for the original native owner.
    open.close()?;
    let copied = result?;
    check()?;
    Ok(copied)
}

struct Open(bool);
impl Open {
    fn close(mut self) -> Result<(), Error> {
        // SAFETY: this exact worker opened the clipboard. No borrowed handle
        // or locked pointer remains when the guarded operation has returned.
        unsafe { CloseClipboard() }.map_err(|_| Error::Cleanup)?;
        self.0 = false;
        Ok(())
    }
}
impl Drop for Open {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: best-effort cleanup of this thread's original borrow,
            // including a failed explicit close. Failure is never success.
            let _ = unsafe { CloseClipboard() };
        }
    }
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
        // SAFETY: releases exactly the successful GlobalLock in read, before
        // CloseClipboard. FALSE with ERROR_SUCCESS means the last lock was
        // released, so clear the last-error slot before checking both results.
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
