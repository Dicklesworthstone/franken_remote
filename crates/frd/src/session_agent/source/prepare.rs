//! Native source preparation under one call-time local consent reservation.
use super::desktop::{LocalAction, Wake};
use super::{Entry, Error as ConsentError, Renewal, SessionAgent, Status};
use crate::{
    media::{
        self, ObservationControl, SharedCaptureUpdate,
        discovery::DiscoveredSource,
        shared_publisher::{self, Publisher},
    },
    worker::Launch,
};
use asupersync::{cx::Cx, types::Time};
use fr_core::time::HostInstant;
use fr_media::{delivery::SharedFramePool, worker::Configuration};
use fr_wire::display::{Catalog, Select};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

/// Local package launch, independent ALREADY authorized source, and bounded pool.
/// Construct only from local sharing policy, never a peer path or media request.
/// Retain `Launch::retain_cleanup`'s `Retirement` before passing the launch here.
/// A factory can construct this after remote observation approval, without keeping
/// a capture process or source authority alive throughout the approval prompt.
pub struct Setup {
    pub control: ObservationControl,
    pub launch: Launch,
    pub pool: SharedFramePool,
}
impl std::fmt::Debug for Setup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NativeSourceSetup([local authority and package])")
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Consent(ConsentError),
    Media(media::Error),
    Publication(shared_publisher::Error),
    Selection,
    LocalEvent,
    Expired,
    Closed,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}

/// The same selected child, physical frame pool and source-bound bootstrap IDR.
/// Registration already belongs to the original local agent. This is not visible
/// presentation or a viewer grant. Closing/dropping Publisher fences the source;
/// its original Retirement remains usable after an interrupted preparation.
pub struct Prepared {
    pub(crate) publisher: Publisher,
    pub(crate) initial: SharedCaptureUpdate,
}
impl Prepared {
    pub fn parts(&mut self) -> (&mut Publisher, &SharedCaptureUpdate) {
        (&mut self.publisher, &self.initial)
    }
    pub fn worker_id(&self) -> Option<u32> {
        self.publisher.worker_id()
    }
}
impl std::fmt::Debug for Prepared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PreparedNativeSource([selected source and initial picture])")
    }
}

/// Shares one of the agent's eight original source slots. No native object lives
/// here; local indicator revoke can fence the child while its pipe is stalled.
pub(super) struct Reservation {
    control: ObservationControl,
    os_session: u32,
    until: HostInstant,
    active: AtomicBool,
}
impl Reservation {
    pub(super) fn close(&self) {
        if self.active.swap(false, Ordering::AcqRel) {
            self.control.revoke();
        }
    }
    pub(super) fn is_closed(&self) -> bool {
        !self.active.load(Ordering::Acquire) || self.control.check().is_err()
    }
    pub(super) fn check(&self, agent: &SessionAgent) -> Result<Status, ConsentError> {
        let result = (|| {
            if !self.active.load(Ordering::Acquire) {
                return Err(ConsentError::Closed);
            }
            Renewal::permission(agent, self.os_session)?;
            let now = self.control.check().map_err(ConsentError::Media)?;
            if now >= self.until {
                return Err(ConsentError::Closed);
            }
            // Preparation never renews consent or native work. One fixed deadline
            // includes discovery, local choice, configure, capture and unpolled time.
            Ok(Status {
                renewed: false,
                authorized_until: self.until,
                next_check: self.until,
            })
        })();
        if result.is_err() {
            self.close();
        }
        result
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        self.close();
    }
}

impl SessionAgent {
    /// Discover actual displays, make one LOCAL selection, configure the same
    /// supervised child and capture its first shared IDR. No caller-built frame,
    /// substitute codec or viewer-owned source is accepted. A single two-second
    /// budget (or earlier source expiry) starts NOW, not at first polling.
    ///
    /// Local events run before each native poll, independently of stalled IPC.
    /// Callbacks are bounded/nonblocking and outside all registry/policy locks;
    /// they must feed actual permissions/events and retain held-input cleanup.
    /// Any failure, panic, local revoke or unpolled abandonment fences authority
    /// before dropping native work. Success transfers the reserved slot directly
    /// to the existing selected-source renewer, without clearing its sticky claim.
    pub fn prepare_native_shared_source<'a, S, L>(
        &'a mut self,
        setup: Setup,
        select: S,
        local: L,
    ) -> Result<impl Future<Output = Result<Prepared, Error>> + Send + use<'a, S, L>, Error>
    where
        S: FnOnce(&Catalog) -> Result<(Select, Configuration), ()> + Send + 'a,
        L: FnMut(&mut SessionAgent, &mut Context<'_>) -> Result<LocalAction, ()> + Send + 'a,
    {
        let os_session = self.permissions().os_session_id();
        Renewal::permission(self, os_session).map_err(Error::Consent)?;
        let control = &setup.control;
        control.check().map_err(Error::Media)?;
        let until = HostInstant::from_micros(
            control
                .deadline(Duration::from_secs(2))
                .map_err(Error::Media)?
                .time()
                .as_nanos()
                / 1000,
        );
        let driver = control.context().timer_driver().ok_or(Error::Closed)?;
        let mut sources = self
            .sources
            .lock()
            .map_err(|_| Error::Consent(ConsentError::Poisoned))?;
        let slot = sources
            .entries
            .iter()
            .position(|e| e.as_ref().is_none_or(Entry::is_closed))
            .ok_or(Error::Consent(ConsentError::Full))?;
        control
            .reserve_local_preparation()
            .map_err(Error::Consent)?;
        let reservation = Arc::new(Reservation {
            control: control.clone(),
            os_session,
            until,
            active: AtomicBool::new(true),
        });
        sources.entries[slot] = Some(Entry::Preparing(reservation.clone()));
        drop(sources);
        let future = async move {
            let discovery = DiscoveredSource::start(&setup.control, setup.launch)
                .await
                .map_err(Error::Media)?;
            let (choice, config) = select(&discovery.catalog().map_err(Error::Media)?)
                .map_err(|()| Error::Selection)?;
            // A callback may revoke the original source reentrantly.
            setup.control.check().map_err(Error::Media)?;
            let mut source = discovery
                .configure_prepared(&setup.control, choice, config)
                .map_err(Error::Media)?
                .await
                .map_err(Error::Media)?;
            let initial = source
                .prepare_shared_capture(&setup.control, &setup.pool)
                .map_err(Error::Media)?
                .capture_if_changed(true)
                .await
                .map_err(Error::Media)?;
            let publisher = Publisher::new(source, setup.control, setup.pool, &initial)
                .map_err(Error::Publication)?;
            Ok(Prepared { publisher, initial })
        };
        Ok(Preparing {
            agent: self,
            reservation,
            slot,
            local: Box::new(local),
            inner: Some(Box::pin(future)),
            timer: Wake {
                driver,
                handle: None,
            },
            finished: false,
        })
    }
}
type Work<'a> = Pin<Box<dyn Future<Output = Result<Prepared, Error>> + Send + 'a>>;
struct Preparing<'a, L> {
    agent: &'a mut SessionAgent,
    reservation: Arc<Reservation>,
    slot: usize,
    local: Box<L>,
    inner: Option<Work<'a>>,
    timer: Wake,
    finished: bool,
}
impl<L> Preparing<'_, L> {
    fn finish(&mut self) {
        if !self.finished {
            self.finished = true;
            self.reservation.close();
            drop(self.inner.take());
            self.timer.cancel();
        }
    }
}
impl<L> Preparing<'_, L>
where
    L: FnMut(&mut SessionAgent, &mut Context<'_>) -> Result<LocalAction, ()>,
{
    fn turn(&mut self, task: &mut Context<'_>) -> Poll<Result<Prepared, Error>> {
        if self.finished {
            return Poll::Ready(Err(Error::Closed));
        }
        if self.reservation.control.check().map_err(Error::Media)? >= self.reservation.until {
            return Poll::Ready(Err(Error::Expired));
        }
        self.reservation.check(self.agent).map_err(Error::Consent)?;
        if (self.local)(self.agent, task).map_err(|()| Error::LocalEvent)? == LocalAction::Stop {
            return Poll::Ready(Err(Error::Closed));
        }
        self.reservation.check(self.agent).map_err(Error::Consent)?;
        let polled = {
            let _current = Cx::set_current(Some(self.reservation.control.context()));
            self.inner
                .as_mut()
                .ok_or(Error::Closed)?
                .as_mut()
                .poll(task)
        };
        // Preserve the original native/selection refusal rather than replacing it
        // with the cancellation caused by its own cleanup. The outer guard still
        // fences authority before releasing the failed native future.
        if let Poll::Ready(Err(error)) = polled {
            return Poll::Ready(Err(error));
        }
        self.reservation.check(self.agent).map_err(Error::Consent)?;
        if let Poll::Ready(result) = polled {
            let prepared = result?;
            let mut sources = self
                .agent
                .sources
                .lock()
                .map_err(|_| Error::Consent(ConsentError::Poisoned))?;
            if !matches!(&sources.entries[self.slot], Some(Entry::Preparing(p)) if Arc::ptr_eq(p, &self.reservation))
            {
                return Poll::Ready(Err(Error::Closed));
            }
            let renewal = Renewal::attach_prepared(
                &prepared.publisher,
                self.agent,
                &self.reservation.control,
            )
            .map_err(Error::Consent)?;
            sources.entries[self.slot] = Some(Entry::Published(Arc::new(renewal)));
            self.reservation.active.store(false, Ordering::Release);
            self.finished = true;
            self.timer.cancel();
            drop(self.inner.take());
            return Poll::Ready(Ok(prepared));
        }
        let now = self
            .reservation
            .control
            .check()
            .map_err(Error::Media)?
            .as_micros();
        self.timer.arm(
            Time::from_nanos(
                now.saturating_add(10_000)
                    .min(self.reservation.until.as_micros())
                    .saturating_mul(1000),
            ),
            task,
        );
        Poll::Pending
    }
}
impl<L> Future for Preparing<'_, L>
where
    L: FnMut(&mut SessionAgent, &mut Context<'_>) -> Result<LocalAction, ()>,
{
    type Output = Result<Prepared, Error>;
    fn poll(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<Self::Output> {
        let mut guard = Turn {
            value: self.get_mut(),
            completed: false,
        };
        let result = guard.value.turn(task);
        if result.is_ready() {
            guard.value.finish();
        }
        guard.completed = true;
        result
    }
}
struct Turn<'r, 'a, L> {
    value: &'r mut Preparing<'a, L>,
    completed: bool,
}
impl<L> Drop for Turn<'_, '_, L> {
    fn drop(&mut self) {
        if !self.completed {
            self.value.finish();
        }
    }
}
impl<L> Drop for Preparing<'_, L> {
    fn drop(&mut self) {
        self.finish();
    }
}
