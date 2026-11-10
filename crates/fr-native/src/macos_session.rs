//! Read-only macOS console/sleep evidence for the supervised session child.
//!
//! `AppKit` and Security/CoreGraphics calls belong to the child's main thread,
//! never the broker. The pipe responder independently expires the last original
//! read while a native call or run-loop turn is stalled. No screen-lock guarantee
//! is inferred from on-console, login-done or protected-data availability.
use crate::{
    clock,
    macos_session_policy::{Evidence, Identity, Observation, Status, Stop},
};
use core::{ffi::c_void, marker::PhantomData, ptr::NonNull};
use frd::session_monitor::protocol::{Reply, State};
use std::{
    rc::Rc,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

#[repr(C)]
#[derive(Default)]
struct NativeSnapshot {
    uid: u32,
    security_session: u32,
    console_set: u32,
    flags: u32,
}

unsafe extern "C" {
    fn fr_ms_open(uid: u32, out: *mut *mut c_void) -> i32;
    fn fr_ms_snapshot(owner: *mut c_void, out: *mut NativeSnapshot) -> i32;
    fn fr_ms_pump(owner: *mut c_void, timeout_ms: u32) -> i32;
    fn fr_ms_close(owner: *mut c_void);
}

fn native_stop(code: i32) -> Stop {
    match code {
        3 => Stop::Inactive,
        4 => Stop::Suspending,
        5 => Stop::SessionUnavailable,
        6 => Stop::IdentityChanged,
        7 => Stop::ServiceUnavailable,
        8 => Stop::UnsupportedSession,
        _ => Stop::Failed,
    }
}

fn reply(status: Status) -> Reply {
    let state = match status {
        Status::Opening => State::Opening,
        Status::Active { until_ns } => {
            return Reply {
                state: State::Active,
                until_ns,
            };
        }
        Status::Stopped(why) => match why {
            Stop::Inactive => State::Inactive,
            Stop::Suspending => State::Suspending,
            Stop::SessionUnavailable => State::SessionUnavailable,
            Stop::IdentityChanged => State::IdentityChanged,
            Stop::ServiceUnavailable => State::ServiceUnavailable,
            Stop::UnsupportedSession => State::UnsupportedSession,
            Stop::EvidenceExpired => State::EvidenceExpired,
            Stop::Failed => State::Failed,
        },
    };
    Reply { state, until_ns: 0 }
}

/// The read-only pipe responder's view. It cannot refresh native evidence.
#[derive(Clone)]
pub struct Control(Arc<Mutex<Evidence>>);

impl Control {
    /// Start the one opening budget before `AppKit` initialization, allowing
    /// Opening replies without blocking the parent's shorter IPC deadline.
    pub fn opening(uid: u32) -> Self {
        Self(Arc::new(Mutex::new(Evidence::new(
            uid,
            clock::monotonic_ns(),
        ))))
    }

    fn lock(&self) -> MutexGuard<'_, Evidence> {
        self.0.lock().unwrap_or_else(|poisoned| {
            let mut evidence = poisoned.into_inner();
            evidence.stop(Stop::Failed);
            evidence
        })
    }

    /// Re-check original native evidence on Darwin's sleep-inclusive clock.
    /// Sampling under this mutex prevents a preempted reader from incorrectly
    /// reporting clock regression behind a newer successful read.
    pub fn status(&self) -> Reply {
        let mut evidence = self.lock();
        reply(evidence.status(clock::monotonic_ns()))
    }

    fn stop(&self, why: Stop) -> Reply {
        reply(self.lock().stop(why))
    }

    /// Unexpected IPC/owner loss is terminal and cannot become fresh evidence.
    pub fn fail(&self) {
        self.stop(Stop::Failed);
    }
}

/// Native subscriptions and snapshots stay on the creating main thread.
/// A positive reply means only the documented console/sleep scope is current.
pub struct Monitor {
    owner: NonNull<c_void>,
    control: Control,
    _main_thread: PhantomData<Rc<()>>,
}

impl Monitor {
    /// Attach public negative notifications, then obtain the first independent
    /// native snapshot. A second owner cannot reset the original Control.
    pub fn open(control: Control) -> Result<Self, State> {
        let status = control.status();
        if status.state != State::Opening {
            return Err(if status.state == State::Active {
                control.stop(Stop::Failed).state
            } else {
                status.state
            });
        }
        let uid = control.lock().uid();
        let mut pointer = core::ptr::null_mut();
        // SAFETY: writable pointer storage, no retained Rust pointer. The glue
        // verifies the main thread and effective/real UID before touching AppKit.
        let code = unsafe { fr_ms_open(uid, &raw mut pointer) };
        if code != 0 {
            return Err(control.stop(native_stop(code)).state);
        }
        let Some(owner) = NonNull::new(pointer) else {
            return Err(control.stop(Stop::Failed).state);
        };
        let mut monitor = Self {
            owner,
            control,
            _main_thread: PhantomData,
        };
        let first = monitor.poll();
        if first.state != State::Active {
            return Err(first.state);
        }
        Ok(monitor)
    }

    /// One genuine bounded-age read. No event or IPC receipt refreshes this.
    fn poll(&mut self) -> Reply {
        let status = self.control.status();
        if !matches!(status.state, State::Opening | State::Active) {
            return status;
        }
        let Some(started) = clock::monotonic_ns() else {
            return self.control.stop(Stop::Failed);
        };
        let mut snapshot = NativeSnapshot::default();
        // SAFETY: exclusively owned main-thread native object and writable
        // scalar snapshot. No callback retains the Rust snapshot pointer.
        let code = unsafe { fr_ms_snapshot(self.owner.as_ptr(), &raw mut snapshot) };
        if code != 0 {
            return self.control.stop(native_stop(code));
        }
        if snapshot.flags & !15 != 0 {
            return self.control.stop(Stop::Failed);
        }
        let observation = Observation {
            identity: Identity {
                uid: snapshot.uid,
                security_session: snapshot.security_session,
                console_set: snapshot.console_set,
            },
            on_console: snapshot.flags & 1 != 0,
            login_done: snapshot.flags & 2 != 0,
            graphic_access: snapshot.flags & 4 != 0,
            protected_data_available: snapshot.flags & 8 != 0,
        };
        let mut evidence = self.control.lock();
        reply(evidence.observe(started, clock::monotonic_ns(), observation))
    }

    /// Service the original main-thread subscriptions, then read native state.
    /// At most 50 ms and 64 events are requested from `AppKit`. A native stall is
    /// still a trust boundary: it expires evidence and is reaped by the parent.
    pub fn turn(&mut self, wait: Duration) -> Reply {
        let status = self.control.status();
        if status.state != State::Active {
            return status;
        }
        let timeout = u32::try_from(wait.as_millis().min(50)).unwrap_or(50);
        // SAFETY: this !Send owner and its AppKit subscriptions stay on the
        // original main thread; native code retains no borrowed Rust storage.
        let code = unsafe { fr_ms_pump(self.owner.as_ptr(), timeout) };
        if code != 0 {
            return self.control.stop(native_stop(code));
        }
        self.poll()
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        // Fence first. Subscription removal never re-enables the original epoch.
        self.control.fail();
        // SAFETY: the one retained Objective-C owner, on its creating main
        // thread. It removes its own subscriptions and releases exactly once.
        unsafe { fr_ms_close(self.owner.as_ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_non_main_thread_is_refused_before_appkit_is_opened() {
        // The ordinary Rust harness runs this off the process main thread.
        let control = Control::opening(rustix::process::geteuid().as_raw());
        let result = Monitor::open(control.clone());
        assert!(matches!(result, Err(State::UnsupportedSession)));
        assert_eq!(control.status().state, State::UnsupportedSession);
    }

    #[test]
    fn only_known_negative_native_states_have_a_specific_reason() {
        for code in [0, 1, 2, 9, -1, i32::MAX] {
            assert_eq!(native_stop(code), Stop::Failed);
        }
        for code in [3, 4, 5, 6, 7, 8] {
            assert_ne!(native_stop(code), Stop::Failed);
        }
    }
}
