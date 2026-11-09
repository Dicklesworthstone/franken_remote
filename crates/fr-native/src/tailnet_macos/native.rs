//! Fixed installed-image boundary. Every owner lives and retires on the same
//! drained blocking operation; no subprocess or cleanup enters the reactor.
use fr_tailnet::Error;
use std::{ffi::c_void, ptr::NonNull};

unsafe extern "C" {
    fn fr_tailnet_macos_start(
        image: *const std::ffi::c_char,
        peers: i32,
        out: *mut *mut c_void,
        started: *mut i32,
    ) -> i32;
    fn fr_tailnet_macos_resume(snapshot: *mut c_void) -> i32;
    fn fr_tailnet_macos_read(
        snapshot: *mut c_void,
        buffer: *mut u8,
        capacity: usize,
        length: *mut usize,
    ) -> i32;
    fn fr_tailnet_macos_try_wait(snapshot: *mut c_void) -> i32;
    fn fr_tailnet_macos_destroy(snapshot: *mut c_void);
}

pub(super) enum Read {
    Pending,
    Eof,
    Bytes(usize),
}
pub(super) struct Snapshot(NonNull<c_void>);
impl Snapshot {
    /// `peers` selects the full snapshot or the one without its peer map.
    pub(super) fn start(peers: bool) -> Result<Self, Error> {
        let mut owner = std::ptr::null_mut();
        // SAFETY: fixed terminated path and valid output slot. C returns sole
        // ownership only after authenticating the still-suspended original child.
        let result = unsafe {
            fr_tailnet_macos_start(
                c"/Applications/Tailscale.app/Contents/MacOS/Tailscale".as_ptr(),
                i32::from(peers),
                &raw mut owner,
                std::ptr::null_mut(),
            )
        };
        match result {
            0 => NonNull::new(owner)
                .map(Self)
                .ok_or(Error::LocalApiUnavailable),
            2 => Err(Error::UntrustedLocalApi),
            _ => Err(Error::LocalApiUnavailable),
        }
    }
    /// Recheck the original cancellation/deadline immediately before this call.
    pub(super) fn resume(&mut self) -> Result<(), Error> {
        // SAFETY: exclusive live ownership; C resumes only its unreaped child.
        if unsafe { fr_tailnet_macos_resume(self.0.as_ptr()) } == 0 {
            Ok(())
        } else {
            Err(Error::LocalApiUnavailable)
        }
    }
    pub(super) fn read(&mut self, buffer: &mut [u8]) -> Result<Read, Error> {
        let mut length = 0;
        // SAFETY: uniquely borrowed writable slice; C writes at most its length
        // and retains no pointer. It performs only one nonblocking read.
        let result = unsafe {
            fr_tailnet_macos_read(
                self.0.as_ptr(),
                buffer.as_mut_ptr(),
                buffer.len(),
                &raw mut length,
            )
        };
        match result {
            0 => Ok(Read::Pending),
            1 => Ok(Read::Eof),
            2 if length <= buffer.len() => Ok(Read::Bytes(length)),
            _ => Err(Error::LocalApiUnavailable),
        }
    }
    pub(super) fn try_wait(&mut self) -> Result<Option<bool>, Error> {
        // SAFETY: C observes this live owner's child without reaping or releasing
        // its PID; later Drop still owns exact process-group cleanup.
        match unsafe { fr_tailnet_macos_try_wait(self.0.as_ptr()) } {
            0 => Ok(None),
            1 => Ok(Some(true)),
            2 => Ok(Some(false)),
            _ => Err(Error::LocalApiUnavailable),
        }
    }
}
impl Drop for Snapshot {
    fn drop(&mut self) {
        // SAFETY: sole allocation and child owner, destroyed once on the blocking
        // worker. C kills the original group before reaping its retained leader.
        unsafe { fr_tailnet_macos_destroy(self.0.as_ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    unsafe extern "C" {
        fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
    }
    #[test]
    fn an_apple_signed_image_without_tailscales_identity_is_refused_and_reaped() {
        let mut owner = std::ptr::null_mut();
        let mut pid = 0;
        // SAFETY: fixed actual system image and valid outputs. The same production
        // signature gate runs while suspended; there is no test bypass.
        let result = unsafe {
            fr_tailnet_macos_start(c"/bin/echo".as_ptr(), 1, &raw mut owner, &raw mut pid)
        };
        if !owner.is_null() {
            // SAFETY: even an unexpected successful negative owns a real child.
            unsafe { fr_tailnet_macos_destroy(owner) };
        }
        assert_eq!(result, 2);
        assert!(owner.is_null());
        assert!(pid > 0);
        let mut status = 0;
        // SAFETY: positive original child PID, valid status and Darwin WNOHANG.
        let waited = unsafe { waitpid(pid, &raw mut status, 1) };
        let error = std::io::Error::last_os_error();
        assert_eq!(waited, -1);
        assert_eq!(error.raw_os_error(), Some(10)); // Darwin ECHILD
    }
}
