//! The Windows Stop sharing window around a host run, shown by frd itself
//! (`fr_winsys::indicator`): no helper process. Same contract as the process
//! owner: Opening until the window is on screen, Ready while its UI thread
//! answers within the check budget, `LocalRevoke` when the person presses Stop
//! sharing or closes it, Expired when it stops answering, Unavailable when it
//! cannot open. Every terminal state requests the host's stop.
use super::process::{Error, Status};
use fr_winsys::indicator::Indicator;
use std::{
    sync::{
        Arc, Condvar, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// How long the UI thread may take to answer one check.
const CHECK_BUDGET: Duration = Duration::from_millis(250);
const CHECK_INTERVAL: Duration = Duration::from_millis(50);
/// Closing the window and joining its threads.
const CLOSE_BUDGET: Duration = Duration::from_millis(1500);

struct Shared {
    on_stop: Box<dyn Fn() + Send + Sync>,
    status: Mutex<Status>,
    changed: Condvar,
}
impl Shared {
    fn state(&self) -> MutexGuard<'_, Status> {
        self.status.lock().unwrap_or_else(|poisoned| {
            let mut status = poisoned.into_inner();
            *status = Status::Stopped(Error::Panicked);
            status
        })
    }
    fn status(&self) -> Status {
        *self.state()
    }
    /// The first terminal cause stands. It is stored before the host's stop
    /// is requested, so the host never reads a stop without its cause.
    fn stop(&self, error: Error) {
        {
            let mut status = self.state();
            if !matches!(*status, Status::Stopped(_)) {
                *status = Status::Stopped(error);
            }
        }
        self.changed.notify_all();
        (self.on_stop)();
    }
    fn ready(&self) {
        let mut status = self.state();
        if *status == Status::Opening {
            *status = Status::Ready;
            drop(status);
            self.changed.notify_all();
        }
    }
    /// Sleep up to `period`, waking early on any change.
    fn pause(&self, period: Duration) {
        let status = self.state();
        if matches!(*status, Status::Stopped(_)) {
            return;
        }
        let _ = self.changed.wait_timeout(status, period);
    }
}

#[derive(Clone)]
pub struct Control(Arc<Shared>);
impl Control {
    pub fn status(&self) -> Status {
        self.0.status()
    }
    /// Park until the window reaches a terminal state (its cause) or the
    /// host run finishes (`None`).
    pub fn wait_for_stop(&self, finished: &AtomicBool) -> Option<Error> {
        let mut status = self.0.state();
        loop {
            if finished.load(Ordering::Acquire) {
                return None;
            }
            if let Status::Stopped(error) = *status {
                return Some(error);
            }
            status = match self.0.changed.wait_timeout(status, CHECK_INTERVAL) {
                Ok((status, _)) => status,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
    }
    pub fn wake_watcher(&self) {
        let status = self.0.state();
        self.0.changed.notify_all();
        drop(status);
    }
}

/// The window's supervising thread. Dropping the owner requests a stop; only
/// [`Owner::finish`] proves the window closed.
pub struct Owner {
    control: Control,
    worker: Option<JoinHandle<Result<(), Error>>>,
}
impl Owner {
    pub fn start(on_stop: impl Fn() + Send + Sync + 'static) -> Result<Self, Error> {
        let shared = Arc::new(Shared {
            on_stop: Box::new(on_stop),
            status: Mutex::new(Status::Opening),
            changed: Condvar::new(),
        });
        let running = shared.clone();
        let Ok(worker) = thread::Builder::new()
            .name("fr-observation-ui".into())
            .spawn(move || supervise(&running))
        else {
            shared.stop(Error::Unavailable);
            return Err(Error::Unavailable);
        };
        Ok(Self {
            control: Control(shared),
            worker: Some(worker),
        })
    }
    pub fn control(&self) -> Control {
        self.control.clone()
    }
    pub fn stop(&self) {
        self.control.0.stop(Error::Stopped);
    }
    /// Stop, close the window and join its supervisor, within a bound.
    pub fn finish(&mut self) -> Result<(), Error> {
        self.stop();
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        let until = Instant::now() + CLOSE_BUDGET;
        while !worker.is_finished() && Instant::now() < until {
            thread::sleep(Duration::from_millis(5));
        }
        if !worker.is_finished() {
            return Err(Error::Cleanup);
        }
        worker.join().map_err(|_| Error::Panicked)?
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.stop();
    }
}

fn supervise(shared: &Arc<Shared>) -> Result<(), Error> {
    let revoked = shared.clone();
    let Ok(indicator) = Indicator::open(move || revoked.stop(Error::LocalRevoke)) else {
        shared.stop(Error::Unavailable);
        return Ok(());
    };
    loop {
        if matches!(shared.status(), Status::Stopped(_)) {
            break;
        }
        if !indicator.responsive(CHECK_BUDGET) {
            shared.stop(Error::Expired);
            break;
        }
        shared.ready();
        shared.pause(CHECK_INTERVAL);
    }
    if indicator.close() {
        Ok(())
    } else {
        Err(Error::Cleanup)
    }
}
