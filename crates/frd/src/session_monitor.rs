//! A supervised read-only native-session process. It cannot approve or inject anything.
//! Parent-created pipes and one immutable local selection bind each process epoch.
//! Native/IPC work stays off the authority thread; every status check independently
//! expires original evidence on `CLOCK_BOOTTIME`, including across suspend.
mod process;
pub mod protocol;
pub use protocol::{Platform, Selection, State};
use std::{
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, Waker},
    thread::{self, JoinHandle},
};

const OPEN_NS: u64 = 2_000_000_000;
const VALID_NS: u64 = 500_000_000;
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
/// macOS's monotonic clock, unlike Linux's, keeps counting during sleep.
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
    evidence: Mutex<Evidence>,
    waker: Mutex<Option<Waker>>,
}
/// Status and its original time bounds have one serialization point. In
/// particular, accepting a reply cannot refresh an expiry or overwrite a clock
/// regression discovered by another reader between separate checks.
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
    fn evidence(&self) -> MutexGuard<'_, Evidence> {
        match self.evidence.lock() {
            Ok(evidence) => evidence,
            Err(poisoned) => {
                let mut evidence = poisoned.into_inner();
                evidence.stop(Error::Protocol);
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
    fn stop(&self, error: Error) {
        self.evidence().stop(error);
        self.wake();
    }
    fn status(&self) -> Status {
        self.status_with(now_ns)
    }
    fn status_with(&self, clock: impl FnOnce() -> Result<u64, Error>) -> Status {
        let mut evidence = self.evidence();
        if !matches!(evidence.status, Status::Stopped(_)) {
            let _ = evidence.check(clock());
        }
        let status = evidence.status;
        drop(evidence);
        if matches!(status, Status::Stopped(_)) {
            self.wake();
        }
        status
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
        let mut evidence = self.evidence();
        let result = if let Status::Stopped(error) = evidence.status {
            Err(error)
        } else {
            // Clock sampling, previous-expiry validation and reply publication
            // are one operation. No status reader can publish a newer sample
            // between these checks or have it replaced by an older reply.
            evidence
                .check(clock())
                .and_then(|now| evidence.accept(reply, sent, now))
        };
        drop(evidence);
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
        OCCUPIED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| Error::Busy)?;
        let shared = Arc::new(Shared {
            evidence: Mutex::new(Evidence {
                status: Status::Opening,
                until_ns: until,
                last_ns: now,
            }),
            waker: Mutex::new(None),
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
