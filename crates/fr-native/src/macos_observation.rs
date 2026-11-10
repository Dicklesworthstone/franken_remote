//! Main-thread macOS observation panels for the read-only host helper.
//!
//! Only completed `AppKit` turns publish positive visibility evidence. A
//! separate IPC/watchdog thread can expire that original evidence while the
//! native loop is blocked. No capture, input, clipboard or approval API is used.
pub use crate::macos_observation_policy::{Status, Stop};
use crate::{clock, macos_observation_policy::Evidence};
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
    fn fr_oi_bind_parent(parent: u32) -> i32;
    fn fr_oi_open(max_displays: u32, out: *mut *mut c_void) -> i32;
    fn fr_oi_pump(owner: *mut c_void, timeout_ms: u32) -> i32;
    fn fr_oi_close(owner: *mut c_void) -> i32;
}

/// Bind the real inherited parent before opening UI. A kernel process-exit
/// watch ends this read-only child even if its main thread later stalls.
pub fn bind_parent(parent: u32) -> Result<(), Stop> {
    // SAFETY: scalar PID only; the native boundary checks real parent/UIDs,
    // retains its process-lifetime dispatch source, and holds no Rust pointer.
    if unsafe { fr_oi_bind_parent(parent) } == 0 {
        Ok(())
    } else {
        Err(Stop::Failed)
    }
}

struct Shared {
    evidence: Mutex<Evidence>,
    closed: AtomicBool,
}

/// Read-only liveness and negative stop control. This type cannot stamp new
/// native evidence or recreate a stopped panel lifetime.
#[derive(Clone)]
pub struct Control(Arc<Shared>);

impl Control {
    /// Start the original two-second opening budget before calling `AppKit`.
    pub fn opening() -> Self {
        Self(Arc::new(Shared {
            evidence: Mutex::new(Evidence::new(clock::monotonic_ns())),
            closed: AtomicBool::new(false),
        }))
    }

    fn lock(&self) -> MutexGuard<'_, Evidence> {
        self.0.evidence.lock().unwrap_or_else(|poisoned| {
            let mut evidence = poisoned.into_inner();
            evidence.stop(Stop::Failed);
            evidence
        })
    }

    /// Recheck the same native evidence on the sleep-inclusive Darwin clock.
    /// Clock reads occur under its mutex so preempted readers cannot invent a
    /// rollback after a newer event-loop turn has completed.
    pub fn status(&self) -> Status {
        self.lock().status(clock::monotonic_ns())
    }

    /// Require a completed native turn that STARTED at or after this request's
    /// receipt timestamp. This query cannot renew evidence. The original
    /// parent deadline then cannot outlive the original native observation.
    pub fn ready_since(&self, floor_ns: u64) -> bool {
        self.lock().ready_since(floor_ns, clock::monotonic_ns())
    }

    /// End the original owner before native cleanup; this cannot grant a peer
    /// observation or substitute for the broker's own stop/teardown ordering.
    pub fn stop(&self) {
        self.lock().stop(Stop::AuthorityEnded);
    }

    /// A failed channel, worker or native call is terminal.
    pub fn fail(&self) {
        self.lock().stop(Stop::Failed);
    }

    /// Native resources closed on their creating thread. A terminal status
    /// alone is not proof of this cleanup.
    pub fn is_closed(&self) -> bool {
        self.0.closed.load(Ordering::Acquire)
    }

    fn closed(&self) {
        self.0.closed.store(true, Ordering::Release);
    }

    fn begin(&self) -> Option<u64> {
        let mut evidence = self.lock();
        let now = clock::monotonic_ns();
        match evidence.status(now) {
            Status::Opening | Status::Shown => now,
            Status::Stopped(_) => None,
        }
    }

    fn complete(&self, started: u64, status: Status) -> Status {
        self.lock().complete(started, clock::monotonic_ns(), status)
    }
}

/// Original bounded set of native panels, owned and pumped on the main thread.
/// The non-Send marker prevents safe code from moving `AppKit` ownership.
pub struct Panels {
    owner: NonNull<c_void>,
    control: Control,
    _main_thread: PhantomData<Rc<()>>,
}

impl Panels {
    /// Create one fixed panel per attached display, bounded by the admitted
    /// display catalog. No positive evidence exists until a real `turn` ends.
    pub fn open(control: Control) -> Result<Self, Stop> {
        if control.status() != Status::Opening {
            control.fail();
            return Err(Stop::Failed);
        }
        let Ok(max_displays) = u32::try_from(fr_wire::display::MAX_DISPLAYS) else {
            control.fail();
            control.closed();
            return Err(Stop::Failed);
        };
        let mut pointer = core::ptr::null_mut();
        // SAFETY: writable pointer storage. The glue verifies main-thread and
        // parent binding, retains its original panels, and stores no Rust data.
        let code = unsafe { fr_oi_open(max_displays, &raw mut pointer) };
        if code != 0 {
            control.fail();
            return Err(Stop::Failed);
        }
        let Some(owner) = NonNull::new(pointer) else {
            control.fail();
            return Err(Stop::Failed);
        };
        let panels = Self {
            owner,
            control,
            _main_thread: PhantomData,
        };
        if let Status::Stopped(why) = panels.control.status() {
            return Err(why);
        }
        Ok(panels)
    }

    /// Complete bounded event work and verify every original panel's native
    /// visibility. A call that returns late cannot publish fresh readiness.
    pub fn turn(&mut self, wait: Duration) -> Status {
        let Some(started) = self.control.begin() else {
            return self.control.status();
        };
        let timeout = u32::try_from(wait.as_millis().min(20)).unwrap_or(20);
        // SAFETY: exclusively owned, non-Send main-thread native handle;
        // bounded scalar wait. No Rust pointer is retained by native callbacks.
        let native = unsafe { fr_oi_pump(self.owner.as_ptr(), timeout) };
        let status = match native {
            0 => Status::Opening,
            1 => Status::Shown,
            2 => Status::Stopped(Stop::Local),
            _ => Status::Stopped(Stop::Failed),
        };
        self.control.complete(started, status)
    }
}

impl Drop for Panels {
    fn drop(&mut self) {
        self.control.stop();
        // SAFETY: exactly one main-thread owner consumes this retained native
        // reference. Native cleanup contains Objective-C exceptions, and only
        // reports success after its original WindowServer windows are absent.
        if unsafe { fr_oi_close(self.owner.as_ptr()) } == 0 {
            self.control.closed();
        } else {
            self.control.fail();
        }
    }
}
