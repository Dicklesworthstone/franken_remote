//! Fixed installed-image boundary. Every owner lives and retires on the same
//! drained blocking operation; no subprocess or cleanup enters the reactor.
use crate::{Command, Error, Read, SourceIdentity};
use std::{ffi::c_void, ptr::NonNull};

unsafe extern "C" {
    fn fr_tailnet_macos_start(
        image: *const std::ffi::c_char,
        operation: u8,
        address: *const u8,
        out: *mut *mut c_void,
        started: *mut i32,
        identity: *mut u8,
        identity_length: *mut usize,
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

/// Sole owner of the verified, initially suspended installed-app child.
/// Create, use and drop this value on one drained blocking operation. Drop
/// kills the original process group before reaping its retained leader.
pub struct Snapshot {
    owner: NonNull<c_void>,
    source: SourceIdentity,
}
impl Snapshot {
    pub fn start(command: Command) -> Result<Self, Error> {
        let (operation, address) = command.wire()?;
        let mut owner = std::ptr::null_mut();
        let mut digest = [0; 64];
        let mut length = 0;
        // SAFETY: fixed terminated path and valid output slot. C returns sole
        // ownership only after authenticating the still-suspended original child.
        let result = unsafe {
            fr_tailnet_macos_start(
                c"/Applications/Tailscale.app/Contents/MacOS/Tailscale".as_ptr(),
                operation,
                address.as_ptr(),
                &raw mut owner,
                std::ptr::null_mut(),
                digest.as_mut_ptr(),
                &raw mut length,
            )
        };
        match result {
            0 => {
                let owner = NonNull::new(owner).ok_or(Error::Unavailable)?;
                if length == 0 || length > digest.len() {
                    // SAFETY: sole returned owner must retire even if native
                    // identity reporting violated its bounded-output contract.
                    unsafe { fr_tailnet_macos_destroy(owner.as_ptr()) };
                    return Err(Error::Untrusted);
                }
                Ok(Self {
                    owner,
                    source: SourceIdentity {
                        digest,
                        length: u8::try_from(length).map_err(|_| Error::Untrusted)?,
                    },
                })
            }
            2 => Err(Error::Untrusted),
            _ => Err(Error::Unavailable),
        }
    }
    pub const fn source_identity(&self) -> SourceIdentity {
        self.source
    }
    /// Recheck the original cancellation/deadline immediately before this call.
    pub fn resume(&mut self) -> Result<(), Error> {
        // SAFETY: exclusive live ownership; C resumes only its unreaped child.
        if unsafe { fr_tailnet_macos_resume(self.owner.as_ptr()) } == 0 {
            Ok(())
        } else {
            Err(Error::Unavailable)
        }
    }
    pub fn read(&mut self, buffer: &mut [u8]) -> Result<Read, Error> {
        let mut length = 0;
        // SAFETY: uniquely borrowed writable slice; C writes at most its length
        // and retains no pointer. It performs only one nonblocking read.
        let result = unsafe {
            fr_tailnet_macos_read(
                self.owner.as_ptr(),
                buffer.as_mut_ptr(),
                buffer.len(),
                &raw mut length,
            )
        };
        match result {
            0 => Ok(Read::Pending),
            1 => Ok(Read::Eof),
            2 if length <= buffer.len() => Ok(Read::Bytes(length)),
            _ => Err(Error::Unavailable),
        }
    }
    pub fn try_wait(&mut self) -> Result<Option<bool>, Error> {
        // SAFETY: C observes this live owner's child without reaping or releasing
        // its PID; later Drop still owns exact process-group cleanup.
        match unsafe { fr_tailnet_macos_try_wait(self.owner.as_ptr()) } {
            0 => Ok(None),
            1 => Ok(Some(true)),
            2 => Ok(Some(false)),
            _ => Err(Error::Unavailable),
        }
    }
}
impl Drop for Snapshot {
    fn drop(&mut self) {
        // SAFETY: sole allocation and child owner, destroyed once on the blocking
        // worker. C kills the original group before reaping its retained leader.
        unsafe { fr_tailnet_macos_destroy(self.owner.as_ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    unsafe extern "C" {
        fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
    }
    fn assert_untrusted_image_refused_and_reaped(command: Command) {
        let (operation, address) = command.wire().unwrap();
        let mut owner = std::ptr::null_mut();
        let mut pid = 0;
        let mut identity = [0xa5; 64];
        let mut identity_length = 64;
        // SAFETY: fixed actual system image, validated numeric command and valid
        // outputs. The production signature gate runs while suspended, without
        // a test bypass. Even a successful negative retains its original owner.
        let result = unsafe {
            fr_tailnet_macos_start(
                c"/bin/echo".as_ptr(),
                operation,
                address.as_ptr(),
                &raw mut owner,
                &raw mut pid,
                identity.as_mut_ptr(),
                &raw mut identity_length,
            )
        };
        if !owner.is_null() {
            // SAFETY: even an unexpected successful negative owns a real child.
            unsafe { fr_tailnet_macos_destroy(owner) };
        }
        assert_eq!(result, 2);
        assert!(owner.is_null());
        assert_eq!(identity_length, 0);
        assert_eq!(identity, [0; 64], "refusal published source identity");
        assert!(pid > 0);
        let mut status = 0;
        // SAFETY: positive original child PID, valid status and Darwin WNOHANG.
        let waited = unsafe { waitpid(pid, &raw mut status, 1) };
        let error = std::io::Error::last_os_error();
        assert_eq!(waited, -1);
        assert_eq!(error.raw_os_error(), Some(10)); // Darwin ECHILD
    }

    #[test]
    fn an_apple_signed_image_without_tailscales_identity_is_refused_and_reaped() {
        assert_untrusted_image_refused_and_reaped(Command::Status);
    }

    #[test]
    fn whois_cannot_bypass_installed_image_validation_or_leave_a_child() {
        for address in ["100.64.0.2", "fd7a:115c:a1e0::2"] {
            assert_untrusted_image_refused_and_reaped(Command::WhoIs(address.parse().unwrap()));
        }
    }

    #[test]
    fn invalid_native_operations_fail_before_spawn_and_publish_no_identity() {
        for operation in [1, 2, 3, 5, 7, u8::MAX] {
            let mut owner = std::ptr::null_mut();
            let mut pid = -1;
            let mut identity = [0xa5; 64];
            let mut identity_length = 64;
            let address = [0; 16];
            // SAFETY: all buffers and outputs have the production sizes. The
            // invalid tag must refuse before process creation; if it fails to
            // do so, the actual signed system image still has no trust bypass.
            let result = unsafe {
                fr_tailnet_macos_start(
                    c"/bin/echo".as_ptr(),
                    operation,
                    address.as_ptr(),
                    &raw mut owner,
                    &raw mut pid,
                    identity.as_mut_ptr(),
                    &raw mut identity_length,
                )
            };
            if !owner.is_null() {
                // SAFETY: retire any unexpectedly returned original allocation.
                unsafe { fr_tailnet_macos_destroy(owner) };
            }
            assert_eq!(result, 1);
            assert!(owner.is_null());
            assert_eq!(pid, 0, "invalid command started a process");
            assert_eq!(identity_length, 0);
            assert_eq!(identity, [0; 64]);
        }
    }

    #[test]
    fn invalid_addresses_refuse_without_needing_an_installed_application() {
        for address in [
            "0.0.0.0",
            "127.0.0.1",
            "::1",
            "ff02::1",
            "::ffff:100.64.0.2",
        ] {
            assert!(matches!(
                Snapshot::start(Command::WhoIs(address.parse().unwrap())),
                Err(Error::InvalidEndpoint)
            ));
        }
    }
}
