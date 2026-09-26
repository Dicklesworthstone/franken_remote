//! Same-user, cross-process exclusion against native positive-consent windows.
//! No server grab, authority lock, timer, queue, retry or peer-selectable path.
use super::PlatformError;
use std::{
    ffi::{CStr, c_char, c_int},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
};
unsafe extern "C" {
    fn fr_input_gate_open(display: *const c_char) -> c_int;
    fn fr_input_gate_lock(fd: c_int, exclusive: c_int) -> c_int;
    fn fr_input_gate_unlock(fd: c_int) -> c_int;
}

pub(super) struct Gate {
    fd: Option<OwnedFd>,
    held: bool,
    revoked: bool,
}
impl Gate {
    pub(super) fn open(display: &CStr) -> Result<Self, PlatformError> {
        // SAFETY: live NUL string; returns a new CLOEXEC descriptor or -1.
        let fd = unsafe { fr_input_gate_open(display.as_ptr()) };
        if fd < 0 {
            return Err(PlatformError::Unavailable);
        }
        Ok(Self {
            // SAFETY: unique descriptor returned by the checked native opener.
            fd: Some(unsafe { OwnedFd::from_raw_fd(fd) }),
            held: false,
            revoked: false,
        })
    }
    pub(super) fn enter(&mut self) -> Result<(), PlatformError> {
        if self.revoked {
            return Err(PlatformError::Permission);
        }
        let fd = self.fd.as_ref().ok_or(PlatformError::Unavailable)?;
        if self.held {
            return Ok(());
        }
        // SAFETY: borrowed live descriptor. The kernel request never waits.
        match unsafe { fr_input_gate_lock(fd.as_raw_fd(), 0) } {
            1 => {
                self.held = true;
                Ok(())
            }
            0 => {
                self.revoked = true;
                Err(PlatformError::Permission)
            }
            _ => {
                self.fd = None;
                Err(PlatformError::Unavailable)
            }
        }
    }
    pub(super) fn leave(&mut self) {
        if self.held {
            self.held = false;
            if let Some(fd) = &self.fd {
                // SAFETY: this owner holds the lock on this live descriptor.
                if unsafe { fr_input_gate_unlock(fd.as_raw_fd()) } != 1 {
                    // Closing releases the lock, but the owner stays unusable.
                    self.fd = None;
                }
            }
        }
    }
    pub(super) fn is_held(&self) -> bool {
        self.held
    }
    pub(super) fn failed(&self) -> bool {
        self.fd.is_none()
    }
    pub(super) fn revoked(&self) -> bool {
        self.revoked
    }
}
