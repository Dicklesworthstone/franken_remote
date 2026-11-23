//! A read-only clipboard borrow for one explicit Send copied files gesture.
//! Copy bounded `CF_HDROP` bytes, unlock and close before opening any source.
//! The clipboard owns its HGLOBAL throughout; it is never freed here.

use crate::{file_choice::Error, windows::viewer::file_memory};
use std::{thread, time::Duration};
use windows::Win32::{
    Foundation::HGLOBAL,
    System::DataExchange::{
        CloseClipboard, GetClipboardData, GetClipboardSequenceNumber, IsClipboardFormatAvailable,
        OpenClipboard,
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
        // SAFETY: the exclusive clipboard borrow keeps this original CF_HDROP
        // allocation stable and live through the bounded copy and its unlock.
        unsafe { file_memory::copy(HGLOBAL(handle.0)) }
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
