//! Read-only observation-indicator child supervision. Native UI, process
//! spawning, socket I/O and wait stay on ONE retained native-I/O worker. The
//! caller checks bounded shared state and supplies its original stop handle.
mod io;
#[cfg(test)]
mod tests;
use crate::input_process::ProcessLaunch;
use std::{
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const OPEN_BUDGET: Duration = Duration::from_secs(3);
const CHECK_BUDGET: Duration = Duration::from_millis(250);
const CHECK_INTERVAL: Duration = Duration::from_millis(50);
const TURN: Duration = Duration::from_millis(5);
static OCCUPIED: AtomicBool = AtomicBool::new(false);
static RETIRED: Mutex<Option<JoinHandle<Result<(), Error>>>> = Mutex::new(None);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Busy,
    Unavailable,
    Protocol,
    Expired,
    LocalRevoke,
    Stopped,
    Cleanup,
    Panicked,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Opening,
    Ready,
    Stopped(Error),
}
struct State {
    status: Status,
    until: Instant,
}
impl State {
    fn check_at(&mut self, now: Instant) -> Status {
        if !matches!(self.status, Status::Stopped(_)) && now >= self.until {
            self.status = Status::Stopped(Error::Expired);
        }
        self.status
    }
    fn ready_at(&mut self, issued: Instant, now: Instant) -> bool {
        if matches!(self.check_at(now), Status::Stopped(_)) {
            return false;
        }
        let Some(until) = issued.checked_add(CHECK_BUDGET) else {
            self.status = Status::Stopped(Error::Expired);
            return false;
        };
        if issued > now || now >= until {
            self.status = Status::Stopped(Error::Expired);
            return false;
        }
        self.status = Status::Ready;
        self.until = until;
        true
    }
}
struct Shared {
    // The original host's nonblocking, idempotent StopHandle::request, never
    // a peer callback, another authority, or native work under this mutex.
    on_stop: Box<dyn Fn() + Send + Sync>,
    state: Mutex<State>,
}
impl Shared {
    fn stop(&self, error: Error) {
        {
            let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if !matches!(state.status, Status::Stopped(_)) {
                state.status = Status::Stopped(error);
            }
        }
        // Store the cause BEFORE waking the original host. Otherwise startup
        // could mistake this native refusal for an unrelated normal stop.
        // No mutex is held during the callback or any ensuing native cleanup.
        (self.on_stop)();
    }
    fn status(&self) -> Status {
        let status = self.state.lock().map_or(Status::Stopped(Error::Panicked), |mut state| {
            state.check_at(Instant::now())
        });
        if let Status::Stopped(error) = status {
            // The original stop has been requested before callers observe a
            // terminal return, even when THEY detect a worker's missed deadline.
            self.stop(error);
        }
        status
    }
    fn check(&self) -> Result<(), Error> {
        match self.status() {
            Status::Opening | Status::Ready => Ok(()),
            Status::Stopped(error) => Err(error),
        }
    }
    fn ready(&self, issued: Instant) -> Result<(), Error> {
        let ready = self.state.lock().is_ok_and(|mut state| {
            state.ready_at(issued, Instant::now())
        });
        if ready {
            Ok(())
        } else {
            self.stop(Error::Expired);
            Err(Error::Expired)
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
}
impl fmt::Debug for Control {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ObservationIndicatorControl").field(&self.status()).finish()
    }
}

/// One process-wide worker permit. A stopped flag does NOT release it; only
/// joining the original thread, after its child was reaped, does that.
pub struct Owner {
    control: Control,
    worker: Option<JoinHandle<Result<(), Error>>>,
    finished: Option<Result<(), Error>>,
}
impl Owner {
    pub fn start(
        launch: ProcessLaunch,
        on_stop: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self, Error> {
        if let Err(error) = claim() {
            on_stop();
            return Err(error);
        }
        let shared = Arc::new(Shared {
            on_stop: Box::new(on_stop),
            state: Mutex::new(State {
                status: Status::Opening,
                until: Instant::now() + OPEN_BUDGET,
            }),
        });
        let running = shared.clone();
        let worker = thread::Builder::new().name("fr-observation-ui".into()).spawn(move || {
            let _guard = Guard(running.clone());
            io::run(&launch, &running)
        });
        match worker {
            Ok(worker) => Ok(Self {
                control: Control(shared),
                worker: Some(worker),
                finished: None,
            }),
            Err(_) => {
                shared.stop(Error::Unavailable);
                OCCUPIED.store(false, Ordering::Release);
                Err(Error::Unavailable)
            }
        }
    }
    pub fn control(&self) -> Control {
        self.control.clone()
    }
    pub fn stop(&self) {
        self.control.stop();
    }
    /// Nonblocking collection. Pending native work remains owned and prevents
    /// replacement. A native refusal may still have clean process teardown.
    pub fn try_finish(&mut self) -> Option<Result<(), Error>> {
        if let Some(finished) = self.finished {
            return Some(finished);
        }
        if self.worker.as_ref().is_some_and(|worker| !worker.is_finished()) {
            return None;
        }
        let result = self.worker.take().map_or(Ok(()), |worker| {
            worker.join().map_err(|_| Error::Panicked).and_then(|result| result)
        });
        // Never release capacity on uncertain cleanup, including a panic.
        if result.is_ok() {
            OCCUPIED.store(false, Ordering::Release);
        }
        self.finished = Some(result);
        Some(result)
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.stop();
        if let Some(worker) = self.worker.take() {
            let mut retired = RETIRED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            debug_assert!(retired.is_none());
            *retired = Some(worker);
        }
    }
}
fn claim() -> Result<(), Error> {
    let mut retired = RETIRED.lock().map_err(|_| Error::Panicked)?;
    if retired.as_ref().is_some_and(|worker| !worker.is_finished()) {
        return Err(Error::Busy);
    }
    if let Some(worker) = retired.take() {
        worker.join().map_err(|_| Error::Panicked)??;
        OCCUPIED.store(false, Ordering::Release);
    }
    OCCUPIED.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map(|_| ()).map_err(|_| Error::Busy)
}
struct Guard(Arc<Shared>);
impl Drop for Guard {
    fn drop(&mut self) {
        self.0.stop(if thread::panicking() { Error::Panicked } else { Error::Stopped });
    }
}
