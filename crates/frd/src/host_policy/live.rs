//! Opt-in live policy on the ORIGINAL Store, with no filesystem I/O in authority checks.
//!
//! Each observed revision fences every lease from the previous revision, even
//! when its values have changed back. New sessions use a new lease; existing
//! observation or input never inherits a broader scope or an approval bypass.
use super::{Policy, Store};
use asupersync::cx::Cx;
use std::{
    fmt,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

const OPEN_NS: u64 = 2_000_000_000;
const VALID_NS: u64 = 500_000_000;
const REFRESH: Duration = Duration::from_millis(100);
// A blocked filesystem call retains the permit until actual thread exit. Drop
// never enables an unlimited succession of replacement disk workers.
static WORKER: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Opening,
    Closed,
    Changed,
    Expired,
    Clock,
    Cancelled,
    Busy,
    Worker,
    Rollback,
    RevisionConflict,
    Store(super::Error),
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "live-host-policy: {self:?}")
    }
}
impl std::error::Error for Error {}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Opening,
    Active(Policy),
    Stopped(Error),
}
struct Epoch {
    policy: Policy,
    failure: Mutex<Option<Error>>,
}
impl Epoch {
    fn retire(&self, error: Error) {
        self.failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_or_insert(error);
    }
    fn check(&self) -> Result<(), Error> {
        match *self.failure.lock().map_err(|_| Error::Worker)? {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}
struct State {
    current: Option<Arc<Epoch>>,
    until: u64,
    previous: u64,
    failure: Option<Error>,
}
impl State {
    fn stop(&mut self, error: Error) -> Error {
        let error = *self.failure.get_or_insert(error);
        if let Some(epoch) = &self.current {
            epoch.retire(error);
        }
        error
    }
    fn check(&mut self, now: Result<u64, Error>) -> Result<u64, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let now = now.map_err(|e| self.stop(e))?;
        if now < self.previous {
            return Err(self.stop(Error::Clock));
        }
        if now >= self.until {
            return Err(self.stop(Error::Expired));
        }
        self.previous = now;
        Ok(now)
    }
    fn publish(&mut self, policy: Policy, started: u64, now: u64) -> Result<(), Error> {
        self.check(Ok(now))?;
        let until = started.checked_add(VALID_NS).ok_or(Error::Clock)?;
        if started > now || now >= until {
            return Err(self.stop(Error::Expired));
        }
        // Store supplies validated disk schema or its exact unsaved defaults.
        if policy != Policy::default() && policy.validate().is_err() {
            return Err(self.stop(Error::Store(super::Error::InvalidDocument)));
        }
        if let Some(old) = &self.current {
            if policy.revision < old.policy.revision {
                return Err(self.stop(Error::Rollback));
            }
            if policy.revision == old.policy.revision {
                if policy != old.policy {
                    return Err(self.stop(Error::RevisionConflict));
                }
                self.until = until;
                return Ok(());
            }
            // Do not compare values only: off -> on -> off between reads must
            // still end old unattended sessions. Revision gaps are conservative.
            old.retire(Error::Changed);
        }
        self.current = Some(Arc::new(Epoch {
            policy,
            failure: Mutex::new(None),
        }));
        self.until = until;
        Ok(())
    }
}
struct Shared {
    cx: Cx,
    state: Mutex<State>,
}
fn now(cx: &Cx) -> Result<u64, Error> {
    cx.checkpoint().map_err(|_| Error::Cancelled)?;
    Ok(cx.timer_driver().ok_or(Error::Clock)?.now().as_nanos())
}
impl Shared {
    fn with<T>(&self, f: impl FnOnce(&mut State, u64) -> Result<T, Error>) -> Result<T, Error> {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poison) => return Err(poison.into_inner().stop(Error::Worker)),
        };
        let current = state.check(now(&self.cx))?;
        f(&mut state, current)
    }
    fn stop(&self, error: Error) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .stop(error);
    }
}

/// Weak access to an explicitly watched local policy. It does not keep a Watch
/// alive, read a file, authenticate a peer, or approve an individual session.
#[derive(Clone)]
pub struct Handle {
    shared: Weak<Shared>,
    approval: Option<super::Approval>,
    sharing: Option<super::Sharing>,
}
impl fmt::Debug for Handle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LiveHostPolicy([local lifetime])")
    }
}
impl Handle {
    /// Local process overrides, never a saved-policy mutation. Every observed
    /// disk revision still retires old leases, even when these effective values
    /// remain unchanged. A connection keeps the overrides it captured at admission.
    #[must_use]
    pub fn with_overrides(
        mut self,
        approval: Option<super::Approval>,
        sharing: Option<super::Sharing>,
    ) -> Self {
        self.approval = approval;
        self.sharing = sharing;
        self
    }
    fn effective(&self, mut policy: Policy) -> Policy {
        policy.approval_mode = self.approval.unwrap_or(policy.approval_mode);
        policy.sharing_scope = self.sharing.unwrap_or(policy.sharing_scope);
        policy
    }
    pub fn status(&self) -> Status {
        match self.shared.upgrade().ok_or(Error::Closed).and_then(|s| {
            s.with(|state, _| {
                Ok(state.current.as_ref().map_or(Status::Opening, |e| {
                    Status::Active(self.effective(e.policy))
                }))
            })
        }) {
            Ok(status) => status,
            Err(error) => Status::Stopped(error),
        }
    }
    /// Snapshot policy and reserve its EXACT epoch atomically, before admission.
    /// Retain this lease through all connection handoffs and check before I/O.
    pub fn lease(&self) -> Result<Lease, Error> {
        let shared = self.shared.upgrade().ok_or(Error::Closed)?;
        shared.with(|state, _| {
            Ok(Lease {
                handle: self.clone(),
                epoch: state.current.clone().ok_or(Error::Opening)?,
            })
        })
    }
}
/// A connection's immutable policy selection. A new revision never refreshes or
/// reauthorizes this lease; its first terminal cause stays observable afterward.
#[derive(Clone)]
pub struct Lease {
    handle: Handle,
    epoch: Arc<Epoch>,
}
impl fmt::Debug for Lease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostPolicyLease")
            .field("revision", &self.epoch.policy.revision)
            .finish_non_exhaustive()
    }
}
impl Lease {
    pub fn check(&self) -> Result<Policy, Error> {
        self.epoch.check()?;
        let result = self
            .handle
            .shared
            .upgrade()
            .ok_or(Error::Closed)
            .and_then(|s| {
                s.with(|state, _| {
                    if !state
                        .current
                        .as_ref()
                        .is_some_and(|e| Arc::ptr_eq(e, &self.epoch))
                    {
                        return Err(Error::Changed);
                    }
                    self.epoch.check()?;
                    Ok(self.handle.effective(self.epoch.policy))
                })
            });
        if let Err(error) = result {
            self.epoch.retire(error);
        }
        result
    }
}

/// One read-only disk worker. Opting in is a LOCAL host decision; `Store::update`
/// remains a save operation, not evidence that a running host applied anything.
/// No file is created, written or watched from a reactor/authority poll.
///
/// First evidence expires after two seconds. Later reads refresh at 100ms and
/// expire 500ms from read START, never completion. Leases independently enforce
/// expiry while a read is stuck. These are bounds, not real-time scheduling or
/// suspend guarantees; the enclosing host must still fence on OS suspend.
/// Keep this owner after stop until `try_finish` observes actual disk-thread exit.
pub struct Watch {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}
impl fmt::Debug for Watch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HostPolicyWatch([original disk worker])")
    }
}
struct Permit;
impl Drop for Permit {
    fn drop(&mut self) {
        WORKER.store(false, Ordering::Release);
    }
}
impl Watch {
    pub fn start(cx: &Cx, store: Store) -> Result<Self, Error> {
        Self::start_reader(cx, move || store.load())
    }
    fn start_reader(
        cx: &Cx,
        mut read: impl FnMut() -> Result<Policy, super::Error> + Send + 'static,
    ) -> Result<Self, Error> {
        let start = now(cx)?;
        let until = start.checked_add(OPEN_NS).ok_or(Error::Clock)?;
        WORKER
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| Error::Busy)?;
        let permit = Permit;
        let shared = Arc::new(Shared {
            cx: cx.clone(),
            state: Mutex::new(State {
                current: None,
                until,
                previous: start,
                failure: None,
            }),
        });
        let work = shared.clone();
        let worker = thread::Builder::new()
            .name("fr-policy".into())
            .spawn(move || {
                let _permit = permit;
                let fence = WorkerFence(work);
                loop {
                    let result = (|| {
                        let start = fence.0.with(|_, now| Ok(now))?;
                        let policy = read().map_err(Error::Store)?;
                        fence.0.with(|state, now| state.publish(policy, start, now))
                    })();
                    if let Err(error) = result {
                        fence.0.stop(error);
                        break;
                    }
                    thread::park_timeout(REFRESH);
                }
            })
            .map_err(|_| Error::Worker)?;
        Ok(Self {
            shared,
            worker: Some(worker),
        })
    }
    pub fn handle(&self) -> Handle {
        Handle {
            shared: Arc::downgrade(&self.shared),
            approval: None,
            sharing: None,
        }
    }
    pub fn stop(&self) {
        self.shared.stop(Error::Closed);
        if let Some(worker) = &self.worker {
            worker.thread().unpark();
        }
    }
    /// None is still running native work, not completed cancellation. A read
    /// refusal can have clean thread exit; inspect `Handle::status` for that cause.
    pub fn try_finish(&mut self) -> Option<Result<(), Error>> {
        if self.worker.as_ref().is_some_and(|w| !w.is_finished()) {
            return None;
        }
        Some(
            self.worker
                .take()
                .map_or(Ok(()), |w| w.join().map_err(|_| Error::Worker)),
        )
    }
}
impl Drop for Watch {
    fn drop(&mut self) {
        self.stop();
    }
}
struct WorkerFence(Arc<Shared>);
impl Drop for WorkerFence {
    fn drop(&mut self) {
        // Includes unwinding while the enclosing caller retains its Watch.
        self.0.stop(Error::Worker);
    }
}

#[cfg(test)]
mod tests;
