//! Read-only observation-indicator child supervision. Native UI, process
//! spawning, socket I/O and wait stay on ONE retained native-I/O worker. The
//! caller checks bounded shared state and supplies its original stop handle.
mod io;
#[cfg(test)]
mod tests;
#[cfg(target_os = "linux")]
mod watcher;
use crate::input_process::ProcessLaunch;
use crate::sys::rustix;
use std::{
    fmt,
    sync::{
        Arc, Condvar, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

const OPEN_BUDGET: Duration = Duration::from_secs(3);
const CHECK_BUDGET: Duration = Duration::from_millis(250);
const CHECK_INTERVAL: Duration = Duration::from_millis(50);
#[cfg(test)]
const TURN: Duration = Duration::from_millis(5);

/// The original observation-indicator budget includes suspend. In particular,
/// macOS `Instant` is uptime, so a sleeping host must not preserve Ready UI
/// evidence merely because neither its worker nor its watcher was scheduled.
/// This is the same kernel-clock convention as the native session monitor.
fn now_ns() -> Result<u64, Error> {
    #[cfg(target_os = "linux")]
    let clock = rustix::time::ClockId::Boottime;
    #[cfg(not(target_os = "linux"))]
    let clock = rustix::time::ClockId::Monotonic;
    let now = rustix::time::clock_gettime(clock);
    u64::try_from(now.tv_sec)
        .ok()
        .and_then(|seconds| seconds.checked_mul(1_000_000_000))
        .and_then(|seconds| seconds.checked_add(u64::try_from(now.tv_nsec).ok()?))
        .ok_or(Error::Clock)
}

fn deadline(issued: u64, budget: Duration) -> Result<u64, Error> {
    issued
        .checked_add(u64::try_from(budget.as_nanos()).map_err(|_| Error::Clock)?)
        .ok_or(Error::Clock)
}
// Each locally selected role has one bounded native worker. A microphone-only
// stop surface may coexist with observation indication, but neither role can
// replace an uncollected original child.
static OBSERVATION: Capacity = Capacity::new();
static MICROPHONE: Capacity = Capacity::new();
struct Capacity {
    occupied: AtomicBool,
    retired: Mutex<Option<JoinHandle<Result<(), Error>>>>,
}
impl Capacity {
    const fn new() -> Self {
        Self {
            occupied: AtomicBool::new(false),
            retired: Mutex::new(None),
        }
    }
}
#[derive(Clone, Copy)]
pub(super) enum Role {
    Observation,
    Microphone,
}
impl Role {
    fn capacity(self) -> &'static Capacity {
        match self {
            Self::Observation => &OBSERVATION,
            Self::Microphone => &MICROPHONE,
        }
    }
}

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
    Clock,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Opening,
    Ready,
    Stopped(Error),
}
struct State {
    status: Status,
    until: u64,
    last: u64,
}
impl State {
    fn stop(&mut self, error: Error) -> Status {
        if !matches!(self.status, Status::Stopped(_)) {
            self.status = Status::Stopped(error);
        }
        self.status
    }

    fn check_at(&mut self, sample: Result<u64, Error>) -> Result<u64, Error> {
        if let Status::Stopped(error) = self.status {
            return Err(error);
        }
        let checked = sample.and_then(|now| {
            if now < self.last {
                return Err(Error::Clock);
            }
            if now >= self.until {
                return Err(Error::Expired);
            }
            self.last = now;
            Ok(now)
        });
        if let Err(error) = checked {
            self.stop(error);
        }
        checked
    }

    fn ready_at(&mut self, issued: u64, sample: Result<u64, Error>) -> Result<(), Error> {
        // Previous validity, current clock and publication share this one
        // state lock. A reply cannot revive an expired or stopped owner.
        let now = self.check_at(sample)?;
        let renewed = deadline(issued, CHECK_BUDGET).and_then(|until| {
            if issued > now {
                Err(Error::Clock)
            } else if now >= until {
                Err(Error::Expired)
            } else {
                Ok(until)
            }
        });
        let until = match renewed {
            Ok(until) => until,
            Err(error) => {
                self.stop(error);
                return Err(error);
            }
        };
        self.status = Status::Ready;
        self.until = until;
        Ok(())
    }
}
struct Shared {
    // The original host's nonblocking, idempotent StopHandle::request, never
    // a peer callback, another authority, or native work under this mutex.
    on_stop: Box<dyn Fn() + Send + Sync>,
    state: Mutex<State>,
    changed: Condvar,
    #[cfg(target_os = "linux")]
    watcher_wake: watcher::Wake,
    native_wake: crate::native_wait::Wake,
}
impl Shared {
    fn notify_watcher(&self) -> bool {
        self.changed.notify_all();
        #[cfg(target_os = "linux")]
        {
            self.watcher_wake.signal()
        }
        #[cfg(not(target_os = "linux"))]
        {
            true
        }
    }
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| {
            let mut state = poisoned.into_inner();
            state.stop(Error::Panicked);
            state
        })
    }
    fn stop(&self, error: Error) {
        {
            self.state().stop(error);
        }
        let _ = self.notify_watcher();
        self.native_wake.signal();
        // Store the cause BEFORE waking the original host. Otherwise startup
        // could mistake this native refusal for an unrelated normal stop.
        // No mutex is held during the callback or any ensuing native cleanup.
        (self.on_stop)();
    }
    fn status(&self) -> Status {
        let status = {
            let mut state = self.state();
            if !matches!(state.status, Status::Stopped(_)) {
                let _ = state.check_at(now_ns());
            }
            state.status
        };
        if matches!(status, Status::Stopped(_)) {
            let _ = self.notify_watcher();
            self.native_wake.signal();
            // The original stop has been requested before callers observe a
            // terminal return, even when THEY detect a worker's missed deadline.
            (self.on_stop)();
        }
        status
    }
    fn check(&self) -> Result<(), Error> {
        match self.status() {
            Status::Opening | Status::Ready => Ok(()),
            Status::Stopped(error) => Err(error),
        }
    }
    fn ready(&self, issued: u64) -> Result<(), Error> {
        let (result, shortened) = {
            let mut state = self.state();
            let previous_until = state.until;
            if let Status::Stopped(error) = state.status {
                (Err(error), false)
            } else {
                let result = state.ready_at(issued, now_ns());
                // A first Check shortens the opening budget. Later successful
                // replies normally extend it: the watcher may keep sleeping
                // until its old deadline and then re-read the current one.
                (result, state.until < previous_until)
            }
        };
        if (shortened || result.is_err()) && !self.notify_watcher() && result.is_ok() {
            self.stop(Error::Unavailable);
            return Err(Error::Unavailable);
        }
        if result.is_err() {
            self.native_wake.signal();
            (self.on_stop)();
        }
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
    /// Park the independent watcher until the original UI evidence expires,
    /// its owner publishes a terminal event, or the containing host finishes.
    /// Renewals never move a wait's own clock backwards or revive terminal UI.
    #[cfg(any(host_stack, test))]
    pub(super) fn wait_for_stop(&self, finished: &AtomicBool) -> Option<Error> {
        if finished.load(Ordering::Acquire) {
            return None;
        }
        #[cfg(target_os = "linux")]
        let Ok(deadline) = watcher::Deadline::new() else {
            self.0.stop(Error::Unavailable);
            return match self.0.status() {
                Status::Stopped(error) => Some(error),
                Status::Opening | Status::Ready => Some(Error::Unavailable),
            };
        };
        let mut state = self.0.state();
        loop {
            if finished.load(Ordering::Acquire) {
                return None;
            }
            match state.check_at(now_ns()) {
                Ok(_) => {}
                Err(error) => {
                    drop(state);
                    // Publish the original cause and stop the host without
                    // holding this lock, exactly as a polling check does.
                    self.0.stop(error);
                    return Some(error);
                }
            }
            #[cfg(target_os = "linux")]
            {
                let until = state.until;
                drop(state);
                let result = deadline.wait(&self.0.watcher_wake, until);
                state = self.0.state();
                if result.is_err() {
                    state.stop(Error::Unavailable);
                }
            }
            #[cfg(not(target_os = "linux"))]
            {
                // A relative condvar clock can stop during system sleep.
                // Preserve the existing 5 ms detection bound until this
                // platform has a qualified suspend-inclusive kernel wait.
                let remaining =
                    Duration::from_nanos(state.until - state.last).min(Duration::from_millis(5));
                state = match self.0.changed.wait_timeout(state, remaining) {
                    Ok((state, _)) => state,
                    Err(poisoned) => {
                        let (mut state, _) = poisoned.into_inner();
                        state.stop(Error::Panicked);
                        state
                    }
                };
            }
        }
    }
    /// Pair the external completion flag with this same mutex for the condvar
    /// fallback. Linux retains the notification counter across that race.
    #[cfg(any(host_stack, test))]
    pub(super) fn wake_watcher(&self) {
        let state = self.0.state();
        let notified = self.0.notify_watcher();
        drop(state);
        if !notified {
            self.0.stop(Error::Unavailable);
        }
    }
}
impl fmt::Debug for Control {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ObservationIndicatorControl")
            .field(&self.status())
            .finish()
    }
}

/// One process-wide worker permit. A stopped flag does NOT release it; only
/// joining the original thread, after its child was reaped, does that.
pub struct Owner {
    control: Control,
    worker: Option<JoinHandle<Result<(), Error>>>,
    finished: Option<Result<(), Error>>,
    capacity: &'static Capacity,
}
impl Owner {
    pub fn start(
        launch: ProcessLaunch,
        on_stop: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self, Error> {
        Self::start_role(launch, on_stop, Role::Observation)
    }
    pub(super) fn start_role(
        launch: ProcessLaunch,
        on_stop: impl Fn() + Send + Sync + 'static,
        role: Role,
    ) -> Result<Self, Error> {
        let capacity = role.capacity();
        let timing = now_ns().and_then(|now| Ok((now, deadline(now, OPEN_BUDGET)?)));
        let (now, until) = timing.inspect_err(|_| {
            on_stop();
        })?;
        let native_wake = crate::native_wait::Wake::new().map_err(|_| {
            on_stop();
            Error::Unavailable
        })?;
        #[cfg(target_os = "linux")]
        let watcher_wake = watcher::Wake::new().map_err(|_| {
            on_stop();
            Error::Unavailable
        })?;
        if let Err(error) = claim(capacity) {
            on_stop();
            return Err(error);
        }
        let shared = Arc::new(Shared {
            on_stop: Box::new(on_stop),
            state: Mutex::new(State {
                status: Status::Opening,
                until,
                last: now,
            }),
            changed: Condvar::new(),
            #[cfg(target_os = "linux")]
            watcher_wake,
            native_wake,
        });
        let running = shared.clone();
        let worker = thread::Builder::new()
            .name("fr-observation-ui".into())
            .spawn(move || {
                let _guard = Guard(running.clone());
                io::run(&launch, &running, role)
            });
        if let Ok(worker) = worker {
            Ok(Self {
                control: Control(shared),
                worker: Some(worker),
                finished: None,
                capacity,
            })
        } else {
            shared.stop(Error::Unavailable);
            capacity.occupied.store(false, Ordering::Release);
            Err(Error::Unavailable)
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
        if self
            .worker
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
        {
            return None;
        }
        let result = self.worker.take().map_or(Ok(()), |worker| {
            worker
                .join()
                .map_err(|_| Error::Panicked)
                .and_then(|result| result)
        });
        // Never release capacity on uncertain cleanup, including a panic.
        if result.is_ok() {
            self.capacity.occupied.store(false, Ordering::Release);
        }
        self.finished = Some(result);
        Some(result)
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.stop();
        if let Some(worker) = self.worker.take() {
            let mut retired = self
                .capacity
                .retired
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            debug_assert!(retired.is_none());
            *retired = Some(worker);
        }
    }
}
fn claim(capacity: &Capacity) -> Result<(), Error> {
    let mut retired = capacity.retired.lock().map_err(|_| Error::Panicked)?;
    if retired.as_ref().is_some_and(|worker| !worker.is_finished()) {
        return Err(Error::Busy);
    }
    if let Some(worker) = retired.take() {
        worker.join().map_err(|_| Error::Panicked)??;
        capacity.occupied.store(false, Ordering::Release);
    }
    capacity
        .occupied
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map(|_| ())
        .map_err(|_| Error::Busy)
}
struct Guard(Arc<Shared>);
impl Drop for Guard {
    fn drop(&mut self) {
        self.0.stop(if thread::panicking() {
            Error::Panicked
        } else {
            Error::Stopped
        });
    }
}
