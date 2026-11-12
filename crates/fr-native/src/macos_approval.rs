//! Main-thread `AppKit` consent surface. The helper has no admission, input,
//! capture or clipboard authority; the broker retains the original Approval.
pub use crate::macos_approval_policy::Status;
use crate::{
    clock,
    macos_approval_policy::{Evidence, Native},
};
use core::{ffi::c_void, marker::PhantomData, ptr::NonNull};
use std::{
    rc::Rc,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

unsafe extern "C" {
    fn fr_ap_open(
        control: u32,
        details: *const u8,
        details_len: usize,
        out: *mut *mut c_void,
    ) -> i32;
    fn fr_ap_pump(owner: *mut c_void, wait_ms: u32, observation: *mut Native) -> i32;
    fn fr_ap_close(owner: *mut c_void) -> i32;
}

const MAX_DETAILS_BYTES: usize = 1024;

fn valid_details(details: &str) -> bool {
    details.len() <= MAX_DETAILS_BYTES
        && details.bytes().any(|byte| byte.is_ascii_graphic())
        && details
            .bytes()
            .all(|byte| byte.is_ascii_graphic() || matches!(byte, b' ' | b'\n'))
}

struct Shared {
    evidence: Mutex<Evidence>,
    closed: AtomicBool,
}

/// Read-only native evidence and terminal cancellation for the independent
/// IPC worker. Only the actual main-thread owner can complete a native turn.
#[derive(Clone)]
pub struct Control(Arc<Shared>);
impl Control {
    pub fn opening() -> Self {
        Self(Arc::new(Shared {
            evidence: Mutex::new(Evidence::new(clock::monotonic_ns())),
            closed: AtomicBool::new(false),
        }))
    }

    fn lock(&self) -> MutexGuard<'_, Evidence> {
        self.0.evidence.lock().unwrap_or_else(|poisoned| {
            let mut evidence = poisoned.into_inner();
            evidence.refuse();
            evidence
        })
    }

    pub fn status(&self) -> Status {
        self.lock().status(clock::monotonic_ns())
    }

    pub fn ready_since(&self, floor: u64) -> bool {
        self.lock().ready_since(floor, clock::monotonic_ns())
    }

    pub fn cancel(&self) {
        self.lock().refuse();
    }

    pub fn is_closed(&self) -> bool {
        self.0.closed.load(Ordering::Acquire)
    }

    fn begin(&self) -> Option<u64> {
        let mut evidence = self.lock();
        let now = clock::monotonic_ns();
        if matches!(evidence.status(now), Status::Opening | Status::Shown) {
            now
        } else {
            None
        }
    }

    fn complete(&self, started: u64, native: Native) -> Status {
        self.lock().complete(started, clock::monotonic_ns(), native)
    }
}

/// The exact prompt, opened/pumped/destroyed on the same main thread. No
/// public method manufactures a yes; native scalar evidence passes safe policy.
pub struct Surface {
    owner: NonNull<c_void>,
    control: Control,
    _main_thread: PhantomData<Rc<()>>,
}
impl Surface {
    pub fn open(control: Control, request_control: bool, details: &str) -> Result<Self, Status> {
        if control.status() != Status::Opening || !valid_details(details) {
            control.cancel();
            return Err(Status::Refused);
        }
        let mut owner = core::ptr::null_mut();
        // SAFETY: bounded validated ASCII and writable output remain live
        // throughout this call. Native verifies the main thread, copies the
        // complete text before opening, and retains no Rust pointer. It refuses
        // a prompt whose full disclosure cannot fit in the visible surface.
        let result = unsafe {
            fr_ap_open(
                u32::from(request_control),
                details.as_ptr(),
                details.len(),
                &raw mut owner,
            )
        };
        let Some(owner) = NonNull::new(owner).filter(|_| result == 0) else {
            control.cancel();
            return Err(Status::Refused);
        };
        let surface = Self {
            owner,
            control,
            _main_thread: PhantomData,
        };
        if surface.control.status() != Status::Opening {
            return Err(Status::Refused);
        }
        Ok(surface)
    }

    pub fn turn(&mut self, wait: Duration) -> Status {
        let Some(started) = self.control.begin() else {
            return self.control.status();
        };
        let mut native = Native::default();
        let wait = u32::try_from(wait.as_millis().min(20)).unwrap_or(20);
        // SAFETY: the unique main-thread handle and writable fixed-size
        // snapshot live throughout the call. Native retains neither snapshot
        // nor callback, and every native event array has fixed capacity two.
        if unsafe { fr_ap_pump(self.owner.as_ptr(), wait, &raw mut native) } != 0 {
            self.control.cancel();
            return Status::Refused;
        }
        self.control.complete(started, native)
    }
}
impl Drop for Surface {
    fn drop(&mut self) {
        if matches!(self.control.status(), Status::Opening | Status::Shown) {
            self.control.cancel();
        }
        // SAFETY: exactly one non-Send owner consumes the retained native
        // graph. Positive evidence is not consumable until this call succeeds.
        if unsafe { fr_ap_close(self.owner.as_ptr()) } == 0 {
            self.control.0.closed.store(true, Ordering::Release);
        } else {
            self.control.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_approval_details_reject_hidden_controls_and_unbounded_text() {
        for text in [
            "",
            " \n ",
            "Device\rName",
            "Device\tName",
            "Device\0Name",
            "\u{1b}[2J",
            "\u{202e}Device",
        ] {
            assert!(!valid_details(text));
        }
        assert!(valid_details("Verified node: n123\nPlayback audio: off"));
        assert!(valid_details(&"a".repeat(MAX_DETAILS_BYTES)));
        assert!(!valid_details(&"a".repeat(MAX_DETAILS_BYTES + 1)));
    }
}
