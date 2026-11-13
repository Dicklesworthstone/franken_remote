//! A supervised read-only native-session process. It cannot approve or inject anything.
//! Parent-created pipes and one immutable local selection bind each process epoch.
//! Native/IPC work stays off the authority thread; every status check independently
//! expires original evidence on `CLOCK_BOOTTIME`, including across suspend.
// Windows has no session-monitor process yet (frd refuses the option there).
#[cfg(unix)]
mod process;
pub mod protocol;
use crate::{input_agent::Seat, input_watchdog::StopReason};
pub use protocol::{Platform, Selection, State};
use std::{
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{
        Arc, Mutex, MutexGuard, OnceLock,
        atomic::{AtomicBool, AtomicU16, Ordering},
    },
    task::{Context, Poll, Waker},
    thread::{self, JoinHandle},
};

const OPEN_NS: u64 = 2_000_000_000;
/// Upper bound on one reply's evidence lifetime from the request. The helper's
/// own bound is shorter or equal (Linux logind 500 ms, macOS 2 s, see
/// fr-native `macos_session_policy`), and the smaller one applies.
const VALID_NS: u64 = 2_000_000_000;
static OCCUPIED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Selection,
    Image,
    Busy,
    Spawn,
    Clock,
    Protocol,
    Pipe,
    ProcessExited,
    OpeningExpired,
    EvidenceExpired,
    Stopped,
    Native(State),
    Cleanup,
}
impl Error {
    // Private, in-memory first-cause representation, never an IPC/wire code.
    // One compare-exchange records the complete cause before any mutex wait.
    const fn terminal_code(self) -> u16 {
        match self {
            Self::Selection => 1,
            Self::Image => 2,
            Self::Busy => 3,
            Self::Spawn => 4,
            Self::Clock => 5,
            Self::Protocol => 6,
            Self::Pipe => 7,
            Self::ProcessExited => 8,
            Self::OpeningExpired => 9,
            Self::EvidenceExpired => 10,
            Self::Stopped => 11,
            Self::Cleanup => 12,
            Self::Native(state) => 0x100 + state as u16,
        }
    }

    const fn from_terminal(code: u16) -> Option<Self> {
        Some(match code {
            0 => return None,
            1 => Self::Selection,
            2 => Self::Image,
            3 => Self::Busy,
            4 => Self::Spawn,
            5 => Self::Clock,
            7 => Self::Pipe,
            8 => Self::ProcessExited,
            9 => Self::OpeningExpired,
            10 => Self::EvidenceExpired,
            11 => Self::Stopped,
            12 => Self::Cleanup,
            0x100 => Self::Native(State::Opening),
            0x101 => Self::Native(State::Active),
            0x102 => Self::Native(State::Locked),
            0x103 => Self::Native(State::Inactive),
            0x104 => Self::Native(State::Suspending),
            0x105 => Self::Native(State::SessionUnavailable),
            0x106 => Self::Native(State::IdentityChanged),
            0x107 => Self::Native(State::ServiceUnavailable),
            0x108 => Self::Native(State::UnsupportedSession),
            0x109 => Self::Native(State::EvidenceExpired),
            0x10a => Self::Native(State::Failed),
            // Code 6 is Protocol; unknown private encodings fail the same way.
            _ => Self::Protocol,
        })
    }

    const fn input_stop_reason(self) -> StopReason {
        match self {
            Self::Native(State::Locked | State::Suspending) => StopReason::Suspended,
            _ => StopReason::AuthorityEnded,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Opening,
    Active,
    Stopped(Error),
}
#[derive(Debug, Clone)]
pub struct Configuration {
    pub image: PathBuf,
    pub selection: Selection,
}

/// Suspend-aware time shared by parent and child. No adjustable wall clock.
/// macOS's monotonic clock, unlike Linux's, keeps counting during sleep; so
/// does Windows' interrupt time.
#[cfg(windows)]
#[allow(clippy::unnecessary_wraps, reason = "the Unix clock read can fail")]
pub fn now_ns() -> Result<u64, Error> {
    Ok(fr_winsys::time::since_boot_ns())
}
/// Suspend-aware time shared by parent and child. No adjustable wall clock.
/// macOS's monotonic clock, unlike Linux's, keeps counting during sleep.
#[cfg(unix)]
pub fn now_ns() -> Result<u64, Error> {
    #[cfg(target_os = "linux")]
    let clock = rustix::time::ClockId::Boottime;
    #[cfg(not(target_os = "linux"))]
    let clock = rustix::time::ClockId::Monotonic;
    let t = rustix::time::clock_gettime(clock);
    u64::try_from(t.tv_sec)
        .ok()
        .and_then(|s| s.checked_mul(1_000_000_000))
        .and_then(|s| s.checked_add(u64::try_from(t.tv_nsec).ok()?))
        .ok_or(Error::Clock)
}
struct Shared {
    terminal: AtomicU16,
    // Exactly one local run's seat, bound before any grant. OnceLock::get never
    // waits for a registrar; a racing registrar rechecks terminal after set.
    input: OnceLock<Seat>,
    evidence: Mutex<Evidence>,
    waker: Mutex<Option<Waker>>,
    native_wake: crate::native_wait::Wake,
}
/// Positive evidence and its original time bounds have one serialization point.
/// Terminal causes have an earlier, independent atomic latch so a native stop
/// never waits for this mutex. No positive reply can replace that terminal latch
/// or refresh expiry/clock regression discovered under this evidence lock.
struct Evidence {
    status: Status,
    until_ns: u64,
    last_ns: u64,
}
impl Evidence {
    fn stop(&mut self, error: Error) {
        if !matches!(self.status, Status::Stopped(_)) {
            self.status = Status::Stopped(error);
        }
    }

    fn check(&mut self, sample: Result<u64, Error>) -> Result<u64, Error> {
        if let Status::Stopped(error) = self.status {
            return Err(error);
        }
        let result = sample.and_then(|now| {
            if now < self.last_ns {
                return Err(Error::Clock);
            }
            if now >= self.until_ns {
                return Err(if self.status == Status::Opening {
                    Error::OpeningExpired
                } else {
                    Error::EvidenceExpired
                });
            }
            self.last_ns = now;
            Ok(now)
        });
        if let Err(error) = result {
            self.stop(error);
        }
        result
    }

    fn accept(&mut self, reply: protocol::Reply, sent: u64, now: u64) -> Result<(), Error> {
        let result = match reply.state {
            State::Active => (|| {
                // A valid IPC response cannot make its source evidence younger.
                // The caller already checked the PREVIOUS bound under this lock.
                let until = reply
                    .until_ns
                    .min(sent.checked_add(VALID_NS).ok_or(Error::Clock)?);
                if now < sent || until <= now {
                    return Err(Error::EvidenceExpired);
                }
                self.until_ns = until;
                self.status = Status::Active;
                Ok(())
            })(),
            State::Opening if self.status == Status::Opening => Ok(()),
            State::Opening => Err(Error::Protocol),
            state => Err(Error::Native(state)),
        };
        if let Err(error) = result {
            // Publish the terminal cause before returning to the subprocess
            // supervisor. A preempted supervisor must not leave old Active
            // evidence usable after a native stop or invalid response.
            self.stop(error);
        }
        result
    }
}
impl Shared {
    fn terminal(&self) -> Option<Error> {
        Error::from_terminal(self.terminal.load(Ordering::Acquire))
    }

    fn latch(&self, error: Error) -> Error {
        let code = error.terminal_code();
        let first = self
            .terminal
            .compare_exchange(0, code, Ordering::AcqRel, Ordering::Acquire)
            .err()
            .unwrap_or(code);
        Error::from_terminal(first).unwrap_or(Error::Protocol)
    }

    fn evidence(&self) -> MutexGuard<'_, Evidence> {
        match self.evidence.lock() {
            Ok(evidence) => evidence,
            Err(poisoned) => {
                let mut evidence = poisoned.into_inner();
                evidence.status = Status::Stopped(self.latch(Error::Protocol));
                evidence
            }
        }
    }
    fn wake(&self) {
        let w = self
            .waker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(w) = w {
            w.wake();
        }
    }
    fn stop(&self, error: Error) -> Error {
        let error = self.latch(error);
        // Do not wait for evidence, admission, native work or a task wake before
        // reaching the actual input authority and its installed process fence.
        if let Some(seat) = self.input.get() {
            seat.retire(error.input_stop_reason());
        }
        self.native_wake.signal();
        self.wake();
        error
    }
    fn status(&self) -> Status {
        self.status_with(now_ns)
    }
    fn status_with(&self, clock: impl FnOnce() -> Result<u64, Error>) -> Status {
        if let Some(error) = self.terminal() {
            return Status::Stopped(self.stop(error));
        }
        let mut evidence = self.evidence();
        if let Some(error) = self.terminal() {
            evidence.status = Status::Stopped(error);
        } else if !matches!(evidence.status, Status::Stopped(_)) {
            let _ = evidence.check(clock());
        }
        let status = match evidence.status {
            Status::Stopped(error) => Status::Stopped(self.latch(error)),
            status => status,
        };
        drop(evidence);
        if let Some(error) = self.terminal() {
            return Status::Stopped(self.stop(error));
        }
        status
    }

    /// A wait may use the original bound only after terminal/poisoned evidence
    /// has reached the input fence. No native wait holds the evidence mutex.
    fn deadline(&self) -> Result<u64, Error> {
        if let Some(error) = self.terminal() {
            return Err(self.stop(error));
        }
        let until = {
            let evidence = self.evidence();
            if let Status::Stopped(error) = evidence.status {
                self.latch(error);
            }
            evidence.until_ns
        };
        match self.terminal() {
            Some(error) => Err(self.stop(error)),
            None => Ok(until),
        }
    }
    fn accept(&self, reply: protocol::Reply, sent: u64) -> Result<(), Error> {
        self.accept_with(reply, sent, now_ns)
    }
    fn accept_with(
        &self,
        reply: protocol::Reply,
        sent: u64,
        clock: impl FnOnce() -> Result<u64, Error>,
    ) -> Result<(), Error> {
        if let Some(error) = self.terminal() {
            return Err(self.stop(error));
        }
        // These replies can only end authority. They must not queue behind a
        // concurrent evidence reader, nor lose their cause to a later clock or
        // cleanup failure. Opening still needs the serialized phase check.
        if !matches!(reply.state, State::Opening | State::Active) {
            return Err(self.stop(Error::Native(reply.state)));
        }
        let mut evidence = self.evidence();
        let result = if let Some(error) = self.terminal() {
            Err(error)
        } else if let Status::Stopped(error) = evidence.status {
            Err(error)
        } else {
            // Clock sampling, previous-expiry validation and reply publication
            // are one operation. No status reader can publish a newer sample
            // between these checks or have it replaced by an older reply.
            evidence
                .check(clock())
                .and_then(|now| evidence.accept(reply, sent, now))
        };
        if let Err(error) = result {
            evidence.status = Status::Stopped(self.latch(error));
        }
        drop(evidence);
        if let Some(error) = self.terminal() {
            return Err(self.stop(error));
        }
        self.wake();
        result
    }
}
#[derive(Clone)]
pub struct Control(Arc<Shared>);
impl Control {
    pub fn status(&self) -> Status {
        self.0.status()
    }
    pub fn stop(&self) {
        self.0.stop(Error::Stopped);
    }

    /// Bind the one exact native-input seat before any grant. A terminal
    /// monitor immediately retires a late registration; replacing the seat is
    /// refused and retires the supplied replacement without touching custody.
    pub(crate) fn bind_input(&self, seat: &Seat) -> Result<(), Error> {
        if let Err(other) = self.0.input.set(seat.clone())
            && !self.0.input.get().is_some_and(|old| old.same_owner(&other))
        {
            other.retire(StopReason::AuthorityEnded);
            return Err(Error::Selection);
        }
        match self.status() {
            Status::Opening | Status::Active if seat.retirement_reason().is_some() => {
                Err(Error::Stopped)
            }
            Status::Opening | Status::Active => Ok(()),
            Status::Stopped(error) => Err(error),
        }
    }
    pub fn register(&self, task: &Context<'_>) {
        let mut w = self
            .0
            .waker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if w.as_ref().is_none_or(|w| !w.will_wake(task.waker())) {
            *w = Some(task.waker().clone());
        }
    }

    /// Poll disposable startup work while the original session remains live.
    /// A terminal event or expiry wins even when the work becomes ready in
    /// the same turn. The caller retains the future across pending turns.
    pub(crate) fn poll_setup<T>(
        &self,
        task: &mut Context<'_>,
        work: Pin<&mut impl Future<Output = T>>,
    ) -> Poll<Result<T, Error>> {
        self.register(task);
        if let Status::Stopped(error) = self.status() {
            return Poll::Ready(Err(error));
        }
        let result = work.poll(task);
        if let Status::Stopped(error) = self.status() {
            return Poll::Ready(Err(error));
        }
        result.map(Ok)
    }
}

/// Retain through cleanup. A stopped authority is not an exited child process.
#[must_use]
pub struct Monitor {
    control: Control,
    worker: Option<JoinHandle<Result<(), Error>>>,
}
impl Monitor {
    pub fn start(configuration: Configuration) -> Result<Self, Error> {
        configuration.selection.validate()?;
        // No Windows session monitor yet (fr-p2-host-windows-rt1).
        #[cfg(windows)]
        {
            drop(configuration);
            Err(Error::Selection)
        }
        #[cfg(unix)]
        Self::start_unix(configuration)
    }
    #[cfg(unix)]
    fn start_unix(configuration: Configuration) -> Result<Self, Error> {
        let expected = if cfg!(target_os = "macos") {
            Platform::Macos
        } else {
            Platform::Linux
        };
        if configuration.selection.platform != expected
            || (expected == Platform::Macos
                && configuration.selection.uid != rustix::process::geteuid().as_raw())
        {
            return Err(Error::Selection);
        }
        if !configuration.image.is_absolute() {
            return Err(Error::Image);
        }
        let now = now_ns()?;
        let until = now.checked_add(OPEN_NS).ok_or(Error::Clock)?;
        let native_wake = crate::native_wait::Wake::new().map_err(|_| Error::Spawn)?;
        OCCUPIED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| Error::Busy)?;
        let shared = Arc::new(Shared {
            terminal: AtomicU16::new(0),
            input: OnceLock::new(),
            evidence: Mutex::new(Evidence {
                status: Status::Opening,
                until_ns: until,
                last_ns: now,
            }),
            waker: Mutex::new(None),
            native_wake,
        });
        let inside = shared.clone();
        let worker = thread::Builder::new()
            .name("fr-session-monitor".into())
            .spawn(move || process::run(&configuration, &inside))
            .map_err(|_| {
                OCCUPIED.store(false, Ordering::Release);
                Error::Spawn
            })?;
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
    }
    pub fn try_finish(&mut self) -> Option<Result<(), Error>> {
        if self.worker.as_ref().is_some_and(|w| !w.is_finished()) {
            return None;
        }
        self.worker
            .take()
            .map(|w| w.join().unwrap_or(Err(Error::Cleanup)))
    }
}
impl Drop for Monitor {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod lifetime_tests;
