//! Read-only Linux/logind session lifetime evidence, never a capture/input grant.
//!
//! One native worker owns a fixed system-bus connection. All foreign calls stay
//! off the authority thread. Missing, stale or negative evidence is terminal;
//! unlocking or reconnecting never restores the original lifetime.
use std::{
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
    task::{Context, Waker},
    thread::{self, JoinHandle},
    time::Duration,
};
#[cfg(feature = "linux-session-events")]
pub mod agent;
#[cfg(feature = "linux-local-approval")]
pub mod approval;
mod bus;
#[cfg(any(
    feature = "linux-input-agent",
    all(test, feature = "linux-session-events")
))]
pub(crate) mod input;

const START_NS: u64 = 2_000_000_000;
const VALID_NS: u64 = 500_000_000;
const REFRESH_NS: u64 = 100_000_000;
// At most one selected local session per process, including retiring/stuck work.
static WORKER: AtomicBool = AtomicBool::new(false);

/// Explicit LOCAL selection. No environment-derived session/display, automatic
/// user switching, D-Bus address override or peer-supplied selector is accepted.
#[derive(Clone)]
pub struct Selection {
    pub session: String,
    pub uid: u32,
    pub seat: String,
    pub display: String,
}
impl fmt::Debug for Selection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LogindSelection([local session])")
    }
}
impl Selection {
    fn validate(&self) -> Result<(), Error> {
        fn name(s: &str) -> bool {
            !s.is_empty()
                && s.len() <= 64
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        }
        if !name(&self.session)
            || !name(&self.seat)
            || self.display.len() > 64
            || !self.display.starts_with(':')
            || self.display.len() < 2
            || !self.display[1..]
                .bytes()
                .all(|b| b.is_ascii_digit() || b == b'.')
        {
            return Err(Error::Selection);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Selection,
    Busy,
    Thread,
    Clock,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "logind: {self:?}")
    }
}
impl std::error::Error for Error {}

/// Content-free terminal cause. A hint is evidence from the selected trusted
/// desktop/logind, NOT independent proof that the physical screen is unlocked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum StopReason {
    OwnerStopped = 2,
    BusUnavailable,
    UntrustedService,
    SessionUnavailable,
    IdentityChanged,
    Locked,
    Inactive,
    Suspending,
    UnsupportedSession,
    Malformed,
    EventFlood,
    EvidenceExpired,
    Clock,
    NativeFailure,
}
impl StopReason {
    fn from_state(n: u8) -> Self {
        match n {
            2 => Self::OwnerStopped,
            3 => Self::BusUnavailable,
            4 => Self::UntrustedService,
            5 => Self::SessionUnavailable,
            6 => Self::IdentityChanged,
            7 => Self::Locked,
            8 => Self::Inactive,
            9 => Self::Suspending,
            10 => Self::UnsupportedSession,
            11 => Self::Malformed,
            12 => Self::EventFlood,
            13 => Self::EvidenceExpired,
            14 => Self::Clock,
            _ => Self::NativeFailure,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Opening,
    /// Fresh positive logind evidence only. OS capture permission, local sharing
    /// approval, source freshness and input authority remain separate checks.
    Active,
    Stopped(StopReason),
}
struct Shared {
    selection: Selection,
    state: AtomicU8,
    deadline: AtomicU64,
    waker: Mutex<Option<Waker>>,
}
impl Shared {
    fn stop(&self, reason: StopReason) {
        let _ = self
            .state
            .try_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                (state < 2).then_some(reason as u8)
            });
    }
    fn wake(&self) {
        // Never invoke an arbitrary executor waker while holding our mutex.
        let wake = self.waker.lock().ok().and_then(|mut w| w.take());
        if let Some(wake) = wake {
            wake.wake();
        }
    }
    fn status(&self, now: Result<u64, StopReason>) -> Status {
        let state = self.state.load(Ordering::Acquire);
        if state < 2 {
            match now {
                Err(e) => self.stop(e),
                Ok(now) if now >= self.deadline.load(Ordering::Acquire) => {
                    self.stop(StopReason::EvidenceExpired);
                }
                Ok(_) => {}
            }
        }
        match self.state.load(Ordering::Acquire) {
            0 => Status::Opening,
            1 => Status::Active,
            state => Status::Stopped(StopReason::from_state(state)),
        }
    }
    fn publish(&self, started: u64) -> Result<(), StopReason> {
        // Anchor freshness BEFORE the query. A slow successful reply cannot
        // become new evidence on arrival. CLOCK_BOOTTIME includes suspension.
        let until = started.checked_add(VALID_NS).ok_or(StopReason::Clock)?;
        if bus::boottime()? >= until {
            return Err(StopReason::EvidenceExpired);
        }
        if let Status::Stopped(reason) = self.status(bus::boottime()) {
            return Err(reason);
        }
        self.deadline.store(until, Ordering::Release);
        self.state
            .try_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                (state < 2).then_some(1)
            })
            .map_err(StopReason::from_state)?;
        Ok(())
    }
}

/// Cloneable, nonblocking evidence handle. It has no native pointer and cannot
/// restart a worker. Consumers MUST check it before renewing local authority.
#[derive(Clone)]
pub struct Control(Arc<Shared>);
impl fmt::Debug for Control {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LogindControl([lifetime])")
    }
}
impl Control {
    /// Exact selected local input process/display association, not a permission.
    /// The X11 native factory must not accidentally gate one desktop with another
    /// session's evidence. No display address or UID comes from a remote peer.
    /// Match the explicit local display and the input process effective UID.
    /// Association alone is not fresh evidence; callers must also check status.
    pub fn matches_local_x11(&self, display: &str) -> bool {
        self.0.selection.display == display && self.0.selection.uid == bus::effective_uid()
    }
    pub fn status(&self) -> Status {
        self.0.status(bus::boottime())
    }
    /// Original positive-read deadline on `CLOCK_BOOTTIME`. Intended for the
    /// same-kernel inherited-pipe monitor; IPC must never start a new lifetime.
    /// This is lifecycle evidence, not consent, capture permission or input authority.
    pub fn evidence_deadline_ns(&self) -> Option<u64> {
        let until = self.0.deadline.load(Ordering::Acquire);
        (self.status() == Status::Active).then_some(until)
    }
    pub fn stop(&self) {
        self.0.stop(StopReason::OwnerStopped);
        self.0.wake();
    }
    /// Subscribe to native progress/termination. Also check status on a bounded
    /// local maintenance cadence: this waker cannot wake a stalled native thread.
    /// Status checks independently expire evidence using the suspend-aware clock.
    pub fn register(&self, task: &Context<'_>) {
        match self.0.waker.lock() {
            Ok(mut w) => {
                if w.as_ref().is_none_or(|w| !w.will_wake(task.waker())) {
                    *w = Some(task.waker().clone());
                }
            }
            Err(_) => self.0.stop(StopReason::NativeFailure),
        }
    }
}

/// Native cleanup owner. Stop is immediate; `try_finish` observes actual thread
/// exit without joining a running foreign call. Drop requests stop, never waits
/// on native code. A stuck retired worker retains the process-wide permit, so
/// repeated attempts cannot accumulate unbounded worker threads.
pub struct Watch {
    control: Control,
    worker: Option<JoinHandle<()>>,
}
impl fmt::Debug for Watch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LogindWatch([native owner])")
    }
}
struct Permit;
impl Drop for Permit {
    fn drop(&mut self) {
        WORKER.store(false, Ordering::Release);
    }
}
impl Watch {
    pub fn start(selection: Selection) -> Result<Self, Error> {
        Self::spawn(selection, bus::SYSTEM_ADDRESS.to_owned(), 0)
    }
    fn spawn(selection: Selection, address: String, trusted_uid: u32) -> Result<Self, Error> {
        selection.validate()?;
        let until = bus::boottime()
            .map_err(|_| Error::Clock)?
            .checked_add(START_NS)
            .ok_or(Error::Clock)?;
        WORKER
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| Error::Busy)?;
        let permit = Permit;
        let shared = Arc::new(Shared {
            selection: selection.clone(),
            state: AtomicU8::new(0),
            deadline: AtomicU64::new(until),
            waker: Mutex::new(None),
        });
        let inside = shared.clone();
        let worker = thread::Builder::new()
            .name("fr-logind".into())
            .spawn(move || {
                let _permit = permit;
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run(&inside, &selection, &address, trusted_uid)
                }));
                inside.stop(
                    result
                        .unwrap_or(Err(StopReason::NativeFailure))
                        .err()
                        .unwrap_or(StopReason::OwnerStopped),
                );
                inside.wake();
            })
            .map_err(|_| Error::Thread)?;
        Ok(Self {
            control: Control(shared),
            worker: Some(worker),
        })
    }
    pub fn control(&self) -> Control {
        self.control.clone()
    }
    pub fn stop(&self) {
        self.control.stop();
        if let Some(worker) = &self.worker {
            worker.thread().unpark();
        }
    }
    pub fn try_finish(&mut self) -> Result<bool, StopReason> {
        if self
            .worker
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
        {
            return Ok(false);
        }
        if let Some(worker) = self.worker.take() {
            worker.join().map_err(|_| StopReason::NativeFailure)?;
        }
        Ok(true)
    }
}
impl Drop for Watch {
    fn drop(&mut self) {
        self.stop();
    }
}
fn run(
    shared: &Arc<Shared>,
    selected: &Selection,
    address: &str,
    trusted_uid: u32,
) -> Result<(), StopReason> {
    let mut connection = bus::Connection::open(address, selected, shared.clone(), trusted_uid)?;
    let mut original = None;
    let mut next = 0;
    loop {
        if let Status::Stopped(reason) = shared.status(bus::boottime()) {
            return Err(reason);
        }
        connection.drain()?;
        let start = bus::boottime()?;
        if start >= next {
            let snapshot = connection.snapshot()?;
            snapshot.validate(selected)?;
            if original.as_ref().is_some_and(|old| old != &snapshot) {
                return Err(StopReason::IdentityChanged);
            }
            original = Some(snapshot);
            connection.drain()?;
            shared.publish(start)?;
            shared.wake();
            next = start.checked_add(REFRESH_NS).ok_or(StopReason::Clock)?;
        }
        thread::park_timeout(Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests;

#[cfg(all(test, feature = "linux-session-monitor"))]
#[path = "logind/tests/process.rs"]
mod process_tests;
