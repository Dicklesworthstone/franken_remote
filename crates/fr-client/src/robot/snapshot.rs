//! One explicit image request on an existing viewer, never a new observation grant.
//!
//! A single reservation covers queued/native work, the completed image, and its
//! local transfer. Reader cancellation cannot interrupt native IPC or free that
//! reservation while the worker still owns it. The original two-second deadline
//! never restarts. The owning runtime must drive `take` and bound its waiter by
//! `Request::deadline`; this mailbox creates neither a timer nor a worker thread.
use fr_media::worker::presentation_snapshot::{Snapshot, Stamp};
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard},
    task::{Context, Poll, Waker},
    time::{Duration, Instant},
};

pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Busy,
    Expired,
    Unavailable,
    NotCurrent,
    Closed,
}

enum Phase {
    Pending,
    Running {
        cancelled: bool,
        refusal: Option<Error>,
    },
    Ready(Result<Snapshot, Error>),
    Delivered {
        refusal: Option<Error>,
    },
}
struct Slot {
    id: u64,
    expected: Stamp,
    deadline: Instant,
    phase: Phase,
    waker: Option<Waker>,
}
impl Slot {
    fn refuse(&mut self, error: Error) -> Option<Waker> {
        match &mut self.phase {
            Phase::Pending | Phase::Ready(Ok(_)) => self.phase = Phase::Ready(Err(error)),
            Phase::Running { refusal, .. } => {
                refusal.get_or_insert(error);
            }
            Phase::Ready(Err(_)) => {}
            Phase::Delivered { .. } => return None,
        }
        self.waker.take()
    }
}
struct State {
    closed: bool,
    current: Option<Stamp>,
    next_id: u64,
    slot: Option<Slot>,
}
struct Shared(Mutex<State>, Box<dyn Fn() -> bool + Send + Sync>);
impl Shared {
    fn lock(&self) -> Result<MutexGuard<'_, State>, Error> {
        // Observe the original viewer's stop even while its owner is draining
        // native work. Never invoke an external predicate under this mutex.
        if !(self.1)() {
            self.close();
            return Err(Error::Closed);
        }
        match self.0.lock() {
            Ok(state) if !state.closed => Ok(state),
            Ok(_) => Err(Error::Closed),
            Err(poisoned) => {
                let mut state = poisoned.into_inner();
                state.closed = true;
                state.current = None;
                let retired = state.slot.take();
                drop(state);
                wake_retired(retired);
                Err(Error::Closed)
            }
        }
    }
    fn close(&self) {
        let retired = {
            let mut state = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.closed = true;
            state.current = None;
            state.slot.take()
        };
        wake_retired(retired);
    }
    fn cancel(&self, id: u64) {
        let Ok(mut state) = self.lock() else {
            return;
        };
        let Some(slot) = state.slot.as_mut().filter(|slot| slot.id == id) else {
            return;
        };
        let waker = slot.waker.take();
        let retired = if let Phase::Running { cancelled, .. } = &mut slot.phase {
            *cancelled = true;
            None
        } else {
            state.slot.take()
        };
        drop(state);
        drop(retired);
        drop(waker);
    }
    fn finish(&self, id: u64, result: Result<Snapshot, Error>, now: Instant) {
        let Ok(mut state) = self.lock() else {
            return;
        };
        let current = state.current;
        let Some(slot) = state.slot.as_mut().filter(|slot| slot.id == id) else {
            return;
        };
        let Phase::Running { cancelled, refusal } = slot.phase else {
            return;
        };
        if cancelled {
            drop(result);
            state.slot = None;
            return;
        }
        let error = refusal
            .or_else(|| (now >= slot.deadline).then_some(Error::Expired))
            .or_else(|| (current != Some(slot.expected)).then_some(Error::NotCurrent));
        let result = error.map_or_else(
            || {
                result.and_then(|image| {
                    if image.stamp() == slot.expected {
                        Ok(image)
                    } else {
                        Err(Error::NotCurrent)
                    }
                })
            },
            Err,
        );
        slot.phase = Phase::Ready(result);
        let waker = slot.waker.take();
        drop(state);
        wake(waker);
    }
}
fn wake(waker: Option<Waker>) {
    if let Some(waker) = waker {
        waker.wake();
    }
}
fn wake_retired(slot: Option<Slot>) {
    if let Some(mut slot) = slot {
        wake(slot.waker.take());
    }
}

/// The sole viewer-side endpoint. Dropping it retires the channel permanently.
/// Only actual compositor submissions may publish a stamp; recovery clears it.
pub struct Channel {
    shared: Arc<Shared>,
}
/// Cloneable local request endpoint. It carries no pixels or input authority.
#[derive(Clone)]
pub struct Handle {
    shared: Arc<Shared>,
}
impl Channel {
    #[must_use]
    pub fn new() -> (Self, Handle) {
        Self::with_liveness(|| true)
    }
    /// Also fence on the original viewer's cancellation, before its asynchronous
    /// cleanup reaches this owner. The predicate runs on every metadata access,
    /// before taking the mailbox mutex. It must be bounded, nonblocking, and
    /// must not re-enter this mailbox. It can only narrow existing permission:
    /// one false result closes the channel permanently, including delivered images.
    #[must_use]
    pub fn with_liveness(check: impl Fn() -> bool + Send + Sync + 'static) -> (Self, Handle) {
        let shared = Arc::new(Shared(
            Mutex::new(State {
                closed: false,
                current: None,
                next_id: 1,
                slot: None,
            }),
            Box::new(check),
        ));
        (
            Self {
                shared: shared.clone(),
            },
            Handle { shared },
        )
    }
    /// Updating metadata creates no image and does not refresh an accepted
    /// request. A changed presentation refuses old work without overlapping IPC.
    pub fn publish(&self, current: Option<Stamp>) {
        let Ok(mut state) = self.shared.lock() else {
            return;
        };
        state.current = current;
        let waker = state.slot.as_mut().and_then(|slot| {
            if let Phase::Delivered { refusal } = &mut slot.phase {
                // A delivered image is historical. Ordinary new frames do not
                // invalidate it, but recovery/retirement cannot later revive it.
                if current.is_none_or(|stamp| {
                    stamp.configuration != slot.expected.configuration
                        || stamp.recovery != slot.expected.recovery
                }) {
                    refusal.get_or_insert(Error::NotCurrent);
                }
                None
            } else if Some(slot.expected) != current {
                slot.refuse(Error::NotCurrent)
            } else {
                None
            }
        });
        drop(state);
        wake(waker);
    }
    /// Called by the existing viewer loop. Expiry also wakes an original waiter
    /// while native work drains; it never admits a second worker operation.
    pub fn take(&self) -> Option<Work> {
        self.take_at(Instant::now())
    }
    fn take_at(&self, now: Instant) -> Option<Work> {
        let mut state = self.shared.lock().ok()?;
        let slot = state.slot.as_mut()?;
        if now >= slot.deadline {
            let waker = slot.refuse(Error::Expired);
            drop(state);
            wake(waker);
            return None;
        }
        if !matches!(slot.phase, Phase::Pending) {
            return None;
        }
        slot.phase = Phase::Running {
            cancelled: false,
            refusal: None,
        };
        Some(Work {
            shared: self.shared.clone(),
            id: slot.id,
            expected: slot.expected,
            deadline: slot.deadline,
            complete: false,
        })
    }
    pub fn close(&self) {
        self.shared.close();
    }
}
impl Drop for Channel {
    fn drop(&mut self) {
        self.close();
    }
}
impl Handle {
    /// Copy metadata only. The caller still validates its original viewer and
    /// observation; this stamp grants neither freshness nor input permission.
    pub fn current_stamp(&self) -> Option<Stamp> {
        self.shared.lock().ok()?.current
    }
    pub fn request(&self, expected: Stamp) -> Result<Request, Error> {
        self.request_at(expected, Instant::now())
    }
    fn request_at(&self, expected: Stamp, now: Instant) -> Result<Request, Error> {
        let mut state = self.shared.lock()?;
        if state.slot.is_some() {
            return Err(Error::Busy);
        }
        match state.current {
            None => return Err(Error::Unavailable),
            Some(current) if current != expected => return Err(Error::NotCurrent),
            Some(_) => {}
        }
        let Some(next) = state.next_id.checked_add(1) else {
            state.closed = true;
            state.current = None;
            return Err(Error::Closed);
        };
        let deadline = now.checked_add(REQUEST_TIMEOUT).ok_or(Error::Expired)?;
        let id = state.next_id;
        state.next_id = next;
        state.slot = Some(Slot {
            id,
            expected,
            deadline,
            phase: Phase::Pending,
            waker: None,
        });
        Ok(Request {
            shared: self.shared.clone(),
            id,
            deadline,
            complete: false,
        })
    }
}

/// Owns one request from creation, including when its future is never polled.
/// Its runtime deadline must use this original instant, not a new timeout.
#[must_use]
pub struct Request {
    shared: Arc<Shared>,
    id: u64,
    deadline: Instant,
    complete: bool,
}
impl Request {
    pub const fn deadline(&self) -> Instant {
        self.deadline
    }
    fn poll_at(&mut self, task: &mut Context<'_>, now: Instant) -> Poll<Result<Image, Error>> {
        if self.complete {
            return Poll::Ready(Err(Error::Unavailable));
        }
        let shared = self.shared.clone();
        let mut state = match shared.lock() {
            Ok(state) => state,
            Err(error) => {
                self.complete = true;
                return Poll::Ready(Err(error));
            }
        };
        let Some(slot) = state.slot.as_mut().filter(|slot| slot.id == self.id) else {
            self.complete = true;
            return Poll::Ready(Err(Error::Unavailable));
        };
        let mut retired_waker = if now >= slot.deadline {
            slot.refuse(Error::Expired)
        } else {
            None
        };
        let result = match &mut slot.phase {
            Phase::Pending | Phase::Running { refusal: None, .. } => {
                if slot
                    .waker
                    .as_ref()
                    .is_none_or(|waker| !waker.will_wake(task.waker()))
                {
                    retired_waker = slot.waker.replace(task.waker().clone());
                }
                Poll::Pending
            }
            Phase::Running {
                cancelled,
                refusal: Some(error),
            } => {
                *cancelled = true;
                self.complete = true;
                retired_waker = slot.waker.take().or(retired_waker);
                Poll::Ready(Err(*error))
            }
            Phase::Ready(_) => {
                let Phase::Ready(result) =
                    std::mem::replace(&mut slot.phase, Phase::Delivered { refusal: None })
                else {
                    unreachable!()
                };
                self.complete = true;
                retired_waker = slot.waker.take().or(retired_waker);
                match result {
                    Ok(snapshot) => Poll::Ready(Ok(Image {
                        shared: self.shared.clone(),
                        id: self.id,
                        deadline: self.deadline,
                        snapshot: Some(snapshot),
                    })),
                    Err(error) => {
                        state.slot = None;
                        Poll::Ready(Err(error))
                    }
                }
            }
            Phase::Delivered { .. } => {
                self.complete = true;
                Poll::Ready(Err(Error::Unavailable))
            }
        };
        drop(state);
        drop(retired_waker);
        result
    }
}
impl Future for Request {
    type Output = Result<Image, Error>;
    fn poll(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<Self::Output> {
        self.get_mut().poll_at(task, Instant::now())
    }
}
impl Drop for Request {
    fn drop(&mut self) {
        if !self.complete {
            self.shared.cancel(self.id);
        }
    }
}

/// The original native operation's reservation. Dropping an unfinished work
/// item refuses its waiter; callers must drain already-started native IPC first.
#[must_use]
pub struct Work {
    shared: Arc<Shared>,
    id: u64,
    expected: Stamp,
    deadline: Instant,
    complete: bool,
}
impl Work {
    pub const fn stamp(&self) -> Stamp {
        self.expected
    }
    pub const fn deadline(&self) -> Instant {
        self.deadline
    }
    pub fn complete(self, result: Result<Snapshot, Error>) {
        self.complete_at(result, Instant::now());
    }
    fn complete_at(mut self, result: Result<Snapshot, Error>, now: Instant) {
        self.complete = true;
        self.shared.finish(self.id, result, now);
    }
}
impl Drop for Work {
    fn drop(&mut self) {
        if !self.complete {
            self.shared
                .finish(self.id, Err(Error::Unavailable), Instant::now());
        }
    }
}

/// Owns the only exported image and retains its global reservation through local
/// socket transfer. Neither the image nor its pixels can be cloned or formatted.
#[must_use]
pub struct Image {
    shared: Arc<Shared>,
    id: u64,
    deadline: Instant,
    snapshot: Option<Snapshot>,
}
impl Image {
    pub fn snapshot(&self) -> &Snapshot {
        self.snapshot.as_ref().expect("owned image until drop")
    }
    pub const fn deadline(&self) -> Instant {
        self.deadline
    }
    /// Check before each local transfer chunk and its final success record.
    /// New frames are allowed; expiry, recovery and owner closure are terminal.
    pub fn check(&self) -> Result<(), Error> {
        self.check_at(Instant::now())
    }
    fn check_at(&self, now: Instant) -> Result<(), Error> {
        let state = self.shared.lock()?;
        let slot = state
            .slot
            .as_ref()
            .filter(|slot| slot.id == self.id)
            .ok_or(Error::Unavailable)?;
        let Phase::Delivered { refusal } = slot.phase else {
            return Err(Error::Unavailable);
        };
        if let Some(error) = refusal {
            return Err(error);
        }
        if now >= self.deadline {
            return Err(Error::Expired);
        }
        Ok(())
    }
}
impl Drop for Image {
    fn drop(&mut self) {
        // Release the pixel allocation BEFORE reopening the global permit.
        drop(self.snapshot.take());
        self.shared.cancel(self.id);
    }
}

#[cfg(test)]
mod tests;
