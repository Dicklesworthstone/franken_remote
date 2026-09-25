//! A supervised read-only logind process. It cannot approve or inject anything.
//! Parent-created pipes and one immutable local selection bind each process epoch.
//! Native/IPC work stays off the authority thread; every status check independently
//! expires original evidence on `CLOCK_BOOTTIME`, including across suspend.
mod process;
pub mod protocol;
pub use protocol::{Selection, State};
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    task::{Context, Waker},
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

/// Linux suspend-aware time shared by parent and child. No adjustable wall clock.
pub fn now_ns() -> Result<u64, Error> {
    let t = rustix::time::clock_gettime(rustix::time::ClockId::Boottime);
    u64::try_from(t.tv_sec)
        .ok()
        .and_then(|s| s.checked_mul(1_000_000_000))
        .and_then(|s| s.checked_add(u64::try_from(t.tv_nsec).ok()?))
        .ok_or(Error::Clock)
}
struct Shared {
    state: Mutex<Status>,
    until: AtomicU64,
    last: AtomicU64,
    waker: Mutex<Option<Waker>>,
}
impl Shared {
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
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !matches!(*state, Status::Stopped(_)) {
                *state = Status::Stopped(error);
            }
        }
        self.wake();
    }
    fn status(&self) -> Status {
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if matches!(*state, Status::Stopped(_)) {
                return *state;
            }
            match now_ns() {
                Err(error) => *state = Status::Stopped(error),
                Ok(at) if at < self.last.fetch_max(at, Ordering::AcqRel) => {
                    *state = Status::Stopped(Error::Clock);
                }
                Ok(at) if at >= self.until.load(Ordering::Acquire) => {
                    *state = Status::Stopped(if *state == Status::Opening {
                        Error::OpeningExpired
                    } else {
                        Error::EvidenceExpired
                    });
                }
                Ok(_) => return *state,
            }
        }
        self.wake();
        *self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn accept(&self, reply: protocol::Reply, sent: u64) -> Result<(), Error> {
        if let Status::Stopped(error) = self.status() {
            return Err(error);
        }
        match reply.state {
            State::Active => {
                let mut state = self.state.lock().map_err(|_| Error::Protocol)?;
                if let Status::Stopped(error) = *state {
                    return Err(error);
                }
                let now = now_ns()?;
                // IPC never refreshes old evidence. Sample inside the original
                // state lock, so a suspended publisher cannot revive an expired owner.
                let until = reply
                    .until_ns
                    .min(sent.checked_add(VALID_NS).ok_or(Error::Clock)?);
                if now < sent || until <= now || now >= self.until.load(Ordering::Acquire) {
                    return Err(Error::EvidenceExpired);
                }
                self.last.store(now, Ordering::Release);
                self.until.store(until, Ordering::Release);
                *state = Status::Active;
                drop(state);
                self.wake();
                Ok(())
            }
            State::Opening if self.status() == Status::Opening => Ok(()),
            State::Opening => Err(Error::Protocol),
            state => Err(Error::Native(state)),
        }
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
        if !configuration.image.is_absolute() {
            return Err(Error::Image);
        }
        let now = now_ns()?;
        let until = now.checked_add(OPEN_NS).ok_or(Error::Clock)?;
        OCCUPIED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| Error::Busy)?;
        let shared = Arc::new(Shared {
            state: Mutex::new(Status::Opening),
            until: AtomicU64::new(until),
            last: AtomicU64::new(now),
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
