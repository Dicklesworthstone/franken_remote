//! One-shot custody of a native child lost during asynchronous startup.
//!
//! The original launch/worker registers here BEFORE native work. Dropping an
//! in-flight startup fences and transfers its actual child, not a PID, to this
//! bounded closing slot. A missing handle is never evidence of process exit.
use super::{Child, Cx, Deadline, Error, ExitStatus, Time, now, sleep_until};
use std::{
    fmt,
    sync::{Arc, Mutex, MutexGuard},
};

/// Retain this with the attempt BEFORE polling startup. This is only cleanup
/// ownership: it cannot submit media, input, or create another process. There is
/// exactly one launch, one possible child and one retained exit receipt.
#[must_use]
pub struct Retirement {
    shared: Arc<Mutex<Custody>>,
}

pub(super) struct Registration {
    shared: Arc<Mutex<Custody>>,
}

// Allocate the complete closing slot before spawn, not a Box in Worker::drop.
struct Custody {
    phase: Phase,
    child: Option<Child>,
}
enum Phase {
    Prepared,
    NotStarted,
    Running,
    Retired,
    Collecting,
    Reaped(ExitStatus),
}
fn lock(shared: &Mutex<Custody>) -> MutexGuard<'_, Custody> {
    // No foreign calls or callbacks run under this private state lock. Poison
    // must not discard the only child owner during unwinding.
    shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
impl fmt::Debug for Retirement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MediaRetirement([original child custody])")
    }
}
impl Registration {
    pub(super) fn new() -> (Self, Retirement) {
        let shared = Arc::new(Mutex::new(Custody {
            phase: Phase::Prepared,
            child: None,
        }));
        (
            Self {
                shared: Arc::clone(&shared),
            },
            Retirement { shared },
        )
    }
    pub(super) fn started(&self) {
        lock(&self.shared).phase = Phase::Running;
    }
    pub(super) fn retire(&self, child: Child, exit: Option<ExitStatus>) {
        let mut state = lock(&self.shared);
        state.phase = match exit {
            Some(status) => Phase::Reaped(status),
            None => Phase::Retired,
        };
        if exit.is_none() {
            state.child = Some(child);
        }
    }
    pub(super) fn reaped(&self, status: ExitStatus) {
        lock(&self.shared).phase = Phase::Reaped(status);
    }
}
impl Drop for Registration {
    fn drop(&mut self) {
        let mut state = lock(&self.shared);
        if matches!(state.phase, Phase::Prepared) {
            // The launch was consumed/dropped before a child existed. Running
            // without a transferred child would remain unconfirmed, not None.
            state.phase = Phase::NotStarted;
        }
    }
}
impl Retirement {
    /// Collect only a retired original child. `None` means the one-shot launch
    /// positively ended before spawn; `Some` is the exact OS exit receipt.
    /// A still-owned launch/worker returns `ReapPending` and is NOT killed here.
    /// Cancellation, timeout and dropping this future retain custody for a
    /// subsequent drain with an independent cleanup context. The deadline is
    /// absolute and never refreshed by polling or child progress.
    pub async fn reap(&mut self, cx: &Cx, deadline: Deadline) -> Result<Option<ExitStatus>, Error> {
        let mut previous = check(cx, deadline, None)?;
        let mut collecting = {
            let mut state = lock(&self.shared);
            match &state.phase {
                Phase::NotStarted => return Ok(None),
                Phase::Reaped(status) => return Ok(Some(*status)),
                Phase::Retired => {}
                _ => return Err(Error::ReapPending),
            }
            let child = state.child.take().ok_or(Error::Unavailable)?;
            state.phase = Phase::Collecting;
            Collecting {
                shared: Arc::clone(&self.shared),
                child: Some(child),
            }
        };
        loop {
            previous = check(cx, deadline, Some(previous))?;
            if let Some(status) = collecting
                .child
                .as_mut()
                .ok_or(Error::Unavailable)?
                .try_wait()
                .map_err(|_| Error::PipeFailed)?
            {
                lock(&self.shared).phase = Phase::Reaped(status);
                // try_wait has actually reaped this child. Drop it outside the
                // lock; the guard must no longer restore the retired state.
                collecting.child = None;
                check(cx, deadline, Some(previous))?;
                return Ok(Some(status));
            }
            sleep_until(
                Time::from_nanos(previous.as_nanos().saturating_add(5_000_000))
                    .min(deadline.time()),
            )
            .await;
        }
    }
}
fn check(cx: &Cx, deadline: Deadline, previous: Option<Time>) -> Result<Time, Error> {
    cx.checkpoint().map_err(|_| Error::Cancelled)?;
    let current = now(cx)?;
    if previous.is_some_and(|before| current < before) {
        return Err(Error::ClockRegression);
    }
    if current >= deadline.time() {
        return Err(Error::ReapPending);
    }
    Ok(current)
}
struct Collecting {
    shared: Arc<Mutex<Custody>>,
    child: Option<Child>,
}
impl Drop for Collecting {
    fn drop(&mut self) {
        if let Some(child) = self.child.take() {
            let mut state = lock(&self.shared);
            state.child = Some(child);
            state.phase = Phase::Retired;
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use asupersync::{process::Command, runtime::RuntimeBuilder, types::CancelKind};
    use std::{
        future::{Future, poll_fn},
        task::Poll,
        time::Duration,
    };

    #[test]
    fn interrupted_reap_restores_the_exact_child_without_a_new_deadline() {
        let runtime = RuntimeBuilder::new()
            .worker_threads(1)
            .blocking_threads(1, 2)
            .enable_platform_reactor(true)
            .build()
            .unwrap();
        runtime.block_on(async {
            let cx = Cx::current().unwrap();
            let until = Deadline::after(&cx, Duration::from_secs(1)).unwrap();
            // A real still-running child forces the collector to suspend. This
            // deliberately bypasses Worker's preceding kill, so the RAII case
            // is deterministic rather than depending on the OS kill latency.
            let child = Command::new("/usr/bin/sleep")
                .arg("60")
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let pid = child.id();
            let (registration, mut owner) = Registration::new();
            registration.started();
            registration.retire(child, None);
            drop(registration);
            {
                let mut future = Box::pin(owner.reap(&cx, until));
                poll_fn(|task| {
                    assert!(future.as_mut().poll(task).is_pending());
                    Poll::Ready(())
                })
                .await;
            }
            {
                let mut custody = lock(&owner.shared);
                assert!(matches!(custody.phase, Phase::Retired));
                assert_eq!(custody.child.as_ref().unwrap().id(), pid);
                custody.child.as_mut().unwrap().start_kill().unwrap();
            }
            let status = owner.reap(&cx, until).await.unwrap().unwrap();
            assert!(!status.success());
        });
    }

    #[test]
    fn cancelled_suspended_reap_restores_custody_before_returning() {
        let runtime = RuntimeBuilder::new()
            .worker_threads(1)
            .blocking_threads(1, 2)
            .enable_platform_reactor(true)
            .build()
            .unwrap();
        let cancel = runtime.request_cx_with_budget(asupersync::types::Budget::INFINITE);
        runtime.block_on(async {
            let cleanup = Cx::current().unwrap();
            let until = Deadline::after(&cleanup, Duration::from_secs(1)).unwrap();
            let child = Command::new("/usr/bin/sleep")
                .arg("60")
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let pid = child.id();
            let (registration, mut owner) = Registration::new();
            registration.started();
            registration.retire(child, None);
            drop(registration);
            {
                let mut future = Box::pin(owner.reap(&cancel, until));
                poll_fn(|task| {
                    assert!(future.as_mut().poll(task).is_pending());
                    cancel.cancel_fast(CancelKind::User);
                    Poll::Ready(())
                })
                .await;
                assert_eq!(future.await, Err(Error::Cancelled));
            }
            {
                let mut custody = lock(&owner.shared);
                assert!(matches!(custody.phase, Phase::Retired));
                assert_eq!(custody.child.as_ref().unwrap().id(), pid);
                custody.child.as_mut().unwrap().start_kill().unwrap();
            }
            assert!(owner.reap(&cleanup, until).await.unwrap().is_some());
        });
    }
}
