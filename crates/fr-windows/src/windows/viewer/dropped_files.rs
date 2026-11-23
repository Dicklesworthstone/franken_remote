//! Shell drop-owned storage. Unlike `CF_HDROP` borrowed from the clipboard, a
//! `WM_DROPFILES` recipient owns one `DragFinish` obligation, even on refusal.

use crate::file_choice::{DropMemory, Error};
use windows::Win32::{
    Foundation::{HGLOBAL, HWND, WPARAM},
    UI::{
        Shell::{DragAcceptFiles, DragFinish, HDROP},
        WindowsAndMessaging::{GetForegroundWindow, IsIconic},
    },
};

pub(super) struct Dropped(DropMemory<HDROP>);

impl Dropped {
    /// # Safety
    /// `value` is the original `WM_DROPFILES` wParam delivered to this viewer.
    /// Adopt it exactly once, before any eligibility/state branch. This type
    /// stays on the receiving UI thread; a worker receives only copied bytes.
    pub(super) unsafe fn from_message(value: WPARAM) -> Result<Self, Error> {
        if value.0 == 0 {
            return Err(Error::Native);
        }
        Ok(Self(DropMemory::new(HDROP(value.0 as *mut _), finish)))
    }

    /// Unlock and finish the original native drop before delivering any bytes.
    pub(super) fn copy(self) -> Result<Vec<u8>, Error> {
        self.0.copy(|drop| {
            // SAFETY: this unique drop recipient owns the unchanged allocation
            // until DropMemory invokes DragFinish after this closure returns.
            let bytes = unsafe { super::file_memory::copy(HGLOBAL(drop.0)) }?;
            // DROPFILES::fNC: accept the viewer's client area, not its title bar
            // or another nonclient target. The bounded copy includes the header.
            if bytes[12..16] != [0, 0, 0, 0] {
                return Err(Error::InvalidPath);
            }
            Ok(bytes)
        })
    }
}

fn finish(drop: HDROP) {
    // SAFETY: DropMemory owns the original WM_DROPFILES allocation and invokes
    // this once, after all reads and locks. It is never a clipboard-owned HDROP.
    unsafe { DragFinish(drop) };
}

/// Called only for the original live viewer HWND on its UI thread. The
/// registration describes readiness; every delivered drop is rechecked too.
pub(super) fn accept(window: HWND, enabled: bool) {
    // SAFETY: the caller owns this live viewer and has published/retired its
    // original armed choice before changing the native registration.
    unsafe { DragAcceptFiles(window, enabled) };
}

pub(super) fn foreground(window: HWND) -> bool {
    // SAFETY: scalar queries of the caller's original live viewer, with no
    // event pumping or changes to activation/control ownership.
    unsafe { GetForegroundWindow() == window && !IsIconic(window).as_bool() }
}
