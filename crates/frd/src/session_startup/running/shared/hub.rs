//! Bounded service of original shared-viewer sessions; capture stays source-owned.
//! A slot covers admission, attachment and service, not just a completed viewer.
use super::{HostSession, SharedHost};
use crate::media::{
    ObservationControl,
    shared_publisher::{JoinQueue, MAX_SUBSCRIBERS},
};
use fr_media::delivery::SendPolicy;
use fr_transport::quic::Disposition;
use fr_wire::negotiation::{ControlBinding, Role};
use std::{
    future::{Future, poll_fn},
    pin::Pin,
    sync::{Arc, Mutex, Weak},
    task::{Context, Poll, Waker},
    time::Duration,
};

/// A locally provisioned unpredictable nonce source, shared by the original
/// session owners. Called outside registry/source locks, never across an await.
pub type Entropy = Arc<dyn Fn() -> Result<u128, ()> + Send + Sync>;

#[derive(Debug, Clone, Copy)]
pub struct Policy {
    /// Includes the initial viewer and every in-progress late join.
    pub viewers: usize,
    pub join_timeout: Duration,
    pub send: SendPolicy,
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            viewers: 3,
            join_timeout: Duration::from_secs(2),
            send: SendPolicy::default(),
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidPolicy,
    Full,
    DuplicateSession,
    ForeignScope,
    WrongRole,
    Closed,
    Poisoned,
    Source(crate::media::shared_publisher::Error),
    Publication(crate::session_startup::PublisherError),
    Session(crate::session_startup::Error),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}

/// Serving means the canonical host driver owns the session, NOT that the
/// decoder is ready, pixels are visible, or input authority has been granted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Starting,
    Serving,
    Finished(Result<(), Error>),
}
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Statistics {
    pub admitted: u64,
    pub finished: u64,
    pub failed: u64,
}

struct Receipt {
    parent: ControlBinding,
    control: ObservationControl,
    state: Mutex<State>,
}
impl Receipt {
    fn finish(&self, result: Result<(), Error>) {
        self.control.revoke();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !matches!(*state, State::Finished(_)) {
            *state = State::Finished(result);
        }
    }
    fn serving(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *state == State::Starting {
            *state = State::Serving;
        }
    }
}
/// Local, generation-safe cancellation/receipt handle for ONE original viewer.
/// Dropping a receipt does not disconnect; `close` revokes immediately. Old
/// receipts never address a replacement by its reused numeric slot or session ID.
#[derive(Clone)]
pub struct Ticket {
    receipt: Arc<Receipt>,
    registry: Weak<Mutex<Slots>>,
}
impl Ticket {
    pub fn state(&self) -> State {
        *self
            .receipt
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    pub fn close(&self) {
        self.receipt.finish(Err(Error::Closed));
        wake(&self.registry);
    }
}
impl std::fmt::Debug for Ticket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SharedViewerTicket")
            .field(&self.state())
            .finish()
    }
}

type Task = Pin<Box<dyn Future<Output = Result<(), Error>> + Send>>;
struct Entry {
    receipt: Arc<Receipt>,
    task: Option<Task>,
}
struct Slots {
    entries: [Option<Entry>; MAX_SUBSCRIBERS],
    policy: Policy,
    closed: bool,
    waker: Option<Waker>,
    statistics: Statistics,
}
fn wake(registry: &Weak<Mutex<Slots>>) {
    let waker = registry.upgrade().and_then(|slots| {
        slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .waker
            .take()
    });
    if let Some(waker) = waker {
        waker.wake();
    }
}

/// Weak admission access to a running hub. It grants no consent and cannot keep
/// the source, hub or any connection alive. Only already-approved `HostSession`s
/// from the same boot/OS scope are accepted. Refusal closes the moved session.
#[derive(Clone)]
pub struct Admission {
    registry: Weak<Mutex<Slots>>,
    source: JoinQueue,
    parent: ControlBinding,
    entropy: Entropy,
}
impl Admission {
    pub fn admit(&self, mut session: HostSession) -> Result<Ticket, Error> {
        session.check().map_err(Error::Session)?;
        if session.selection().role != Role::Observe {
            return Err(Error::WrongRole);
        }
        let parent = session.binding();
        if parent.host_boot != self.parent.host_boot || parent.os_session != self.parent.os_session
        {
            return Err(Error::ForeignScope);
        }
        self.source.check_source().map_err(Error::Source)?;
        let registry = self.registry.upgrade().ok_or(Error::Closed)?;
        let receipt = Arc::new(Receipt {
            parent,
            control: session.original_observation(),
            state: Mutex::new(State::Starting),
        });
        let (slot, policy) = {
            let mut slots = registry.lock().map_err(|_| Error::Poisoned)?;
            if slots.closed {
                return Err(Error::Closed);
            }
            if slots
                .entries
                .iter()
                .flatten()
                .any(|e| e.receipt.parent.remote_session == parent.remote_session)
            {
                return Err(Error::DuplicateSession);
            }
            let slot = slots.entries[..slots.policy.viewers]
                .iter()
                .position(Option::is_none)
                .ok_or(Error::Full)?;
            slots.entries[slot] = Some(Entry {
                receipt: receipt.clone(),
                task: None,
            });
            (slot, slots.policy)
        };
        let reservation = Reservation {
            registry,
            slot,
            receipt: receipt.clone(),
            installed: false,
        };
        let entropy = self.entropy.clone();
        // Construct NOW: its immutable budget includes time queued in this hub.
        // No registry lock crosses source checks, caller code, or native work.
        let opening = session.join_shared_display(
            self.source.clone(),
            policy.join_timeout,
            policy.send,
            move || entropy(),
        );
        let entropy = self.entropy.clone();
        let running_receipt = receipt.clone();
        let task = Box::pin(async move {
            let mut host = opening.await.map_err(Error::Publication)?;
            running_receipt.serving();
            host.serve(move || entropy(), |_, _| Ok(Disposition::Blocked))
                .await
                .map_err(Error::Session)
        });
        reservation.install(task)?;
        Ok(Ticket {
            receipt,
            registry: self.registry.clone(),
        })
    }
    pub fn statistics(&self) -> Result<Statistics, Error> {
        let registry = self.registry.upgrade().ok_or(Error::Closed)?;
        let slots = registry.lock().map_err(|_| Error::Poisoned)?;
        Ok(slots.statistics)
    }
}
// A concurrently closed hub or an unwind between reserve/install cannot leave a
// hidden occupied slot or a live joining session behind.
struct Reservation {
    registry: Arc<Mutex<Slots>>,
    slot: usize,
    receipt: Arc<Receipt>,
    installed: bool,
}
impl Reservation {
    fn install(mut self, task: Task) -> Result<(), Error> {
        {
            let mut slots = self.registry.lock().map_err(|_| Error::Poisoned)?;
            if slots.closed {
                return Err(Error::Closed);
            }
            let entry = slots.entries[self.slot].as_mut().ok_or(Error::Closed)?;
            if !Arc::ptr_eq(&entry.receipt, &self.receipt) {
                return Err(Error::Closed);
            }
            entry.task = Some(task);
            slots.statistics.admitted = slots.statistics.admitted.saturating_add(1);
            self.installed = true;
        }
        wake(&Arc::downgrade(&self.registry));
        Ok(())
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        if !self.installed {
            self.receipt.finish(Err(Error::Closed));
            let retired = {
                let mut slots = self
                    .registry
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if slots.entries[self.slot]
                    .as_ref()
                    .is_some_and(|e| Arc::ptr_eq(&e.receipt, &self.receipt))
                {
                    slots.entries[self.slot].take()
                } else {
                    None
                }
            };
            drop(retired);
            wake(&Arc::downgrade(&self.registry));
        }
    }
}

/// One bounded service owner for original observation connections. A slow or
/// failed join never prevents another slot's renewal, UDP service or recovery.
/// Capture and LOCAL source renewal must run independently; `Hub` cannot renew
/// source consent. No task is cancelled and restarted merely to poll another.
pub struct Hub {
    registry: Arc<Mutex<Slots>>,
    admission: Admission,
    initial: Ticket,
    next: usize,
}
impl Hub {
    pub fn new(mut first: SharedHost, policy: Policy, entropy: Entropy) -> Result<Self, Error> {
        if policy.viewers == 0
            || policy.viewers > MAX_SUBSCRIBERS
            || policy.join_timeout < Duration::from_micros(1)
            || policy.join_timeout > Duration::from_secs(2)
        {
            return Err(Error::InvalidPolicy);
        }
        first.session.check().map_err(Error::Session)?;
        let subscriber = first.subscriber.as_ref().ok_or(Error::Closed)?;
        subscriber.session_live().map_err(Error::Source)?;
        let source = subscriber.original_source();
        let parent = first.session.binding();
        let receipt = Arc::new(Receipt {
            parent,
            control: first.session.original_observation(),
            state: Mutex::new(State::Serving),
        });
        let first_entropy = entropy.clone();
        let task = Box::pin(async move {
            first
                .serve(move || first_entropy(), |_, _| Ok(Disposition::Blocked))
                .await
                .map_err(Error::Session)
        });
        let mut entries = core::array::from_fn(|_| None);
        entries[0] = Some(Entry {
            receipt: receipt.clone(),
            task: Some(task),
        });
        let registry = Arc::new(Mutex::new(Slots {
            entries,
            policy,
            closed: false,
            waker: None,
            statistics: Statistics {
                admitted: 1,
                ..Statistics::default()
            },
        }));
        let weak = Arc::downgrade(&registry);
        Ok(Self {
            admission: Admission {
                registry: weak.clone(),
                source,
                parent,
                entropy,
            },
            initial: Ticket {
                receipt,
                registry: weak,
            },
            registry,
            next: 0,
        })
    }
    pub fn admissions(&self) -> Admission {
        self.admission.clone()
    }
    pub fn initial(&self) -> Ticket {
        self.initial.clone()
    }
    pub fn close(&mut self) {
        let (retired, waker) = {
            let mut slots = self
                .registry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            slots.closed = true;
            // Fence every connection BEFORE dropping any network future/native UI.
            for entry in slots.entries.iter().flatten() {
                entry.receipt.finish(Err(Error::Closed));
            }
            let retired: [Option<Entry>; MAX_SUBSCRIBERS] =
                core::array::from_fn(|i| slots.entries[i].take());
            (retired, slots.waker.take())
        };
        drop(retired);
        if let Some(waker) = waker {
            waker.wake();
        }
    }
    /// Each poll visits each bounded slot at most once, rotating its first slot.
    /// Futures and native/network callbacks are always polled OUTSIDE the mutex.
    pub fn serve(&mut self) -> impl Future<Output = Result<Statistics, Error>> + '_ {
        let guard = Run { hub: self };
        async move { poll_fn(|cx| guard.hub.poll(cx)).await }
    }
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<Result<Statistics, Error>> {
        {
            let mut slots = self.registry.lock().map_err(|_| Error::Poisoned)?;
            if slots.closed {
                return Poll::Ready(Err(Error::Closed));
            }
            slots.waker = Some(cx.waker().clone());
        }
        if let Err(error) = self.admission.source.check_source() {
            return Poll::Ready(Err(Error::Source(error)));
        }
        for offset in 0..MAX_SUBSCRIBERS {
            let slot = (self.next + offset) % MAX_SUBSCRIBERS;
            let next = {
                let mut slots = self.registry.lock().map_err(|_| Error::Poisoned)?;
                slots.entries[slot]
                    .as_mut()
                    .and_then(|entry| entry.task.take().map(|task| (entry.receipt.clone(), task)))
            };
            let Some((receipt, mut task)) = next else {
                continue;
            };
            let result = if receipt.control.check().is_err() {
                Poll::Ready(Err(Error::Closed))
            } else {
                task.as_mut().poll(cx)
            };
            match result {
                Poll::Pending => {
                    let mut slots = self.registry.lock().map_err(|_| Error::Poisoned)?;
                    if let Some(entry) = &mut slots.entries[slot] {
                        entry.task = Some(task);
                    }
                }
                Poll::Ready(result) => {
                    receipt.finish(result);
                    drop(task);
                    let mut slots = self.registry.lock().map_err(|_| Error::Poisoned)?;
                    slots.entries[slot] = None;
                    slots.statistics.finished = slots.statistics.finished.saturating_add(1);
                    if result.is_err() {
                        slots.statistics.failed = slots.statistics.failed.saturating_add(1);
                    }
                }
            }
        }
        self.next = (self.next + 1) % MAX_SUBSCRIBERS;
        let mut slots = self.registry.lock().map_err(|_| Error::Poisoned)?;
        if slots.entries.iter().all(Option::is_none) {
            slots.closed = true;
            Poll::Ready(Ok(slots.statistics))
        } else {
            Poll::Pending
        }
    }
}
struct Run<'a> {
    hub: &'a mut Hub,
}
impl Drop for Run<'_> {
    fn drop(&mut self) {
        self.hub.close();
    }
}
impl Drop for Hub {
    fn drop(&mut self) {
        self.close();
    }
}

impl Hub {
    pub(crate) fn service_owner(
        &self,
        publisher: &crate::media::shared_publisher::Publisher,
    ) -> Result<ObservationControl, Error> {
        if self.registry.lock().map_err(|_| Error::Poisoned)?.closed {
            return Err(Error::Closed);
        }
        self.admission
            .source
            .service_owner(publisher)
            .map_err(Error::Source)
    }
}
impl Admission {
    // Fence even pending, not-yet-subscribed joins before any sibling service
    // future is dropped. Does not call user code or drop network/native owners.
    pub(crate) fn fence(&self) {
        if let Some(registry) = self.registry.upgrade() {
            let waker = {
                let mut slots = registry
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                slots.closed = true;
                for entry in slots.entries.iter().flatten() {
                    entry.receipt.finish(Err(Error::Closed));
                }
                slots.waker.take()
            };
            if let Some(waker) = waker {
                waker.wake();
            }
        }
    }
}
