//! One observation-only OS-share lifetime: local consent, capture, and viewers.
//! Existing owners keep their clocks, transport, source identity and budgets.
use super::{Error as ConsentError, Renewal, SessionAgent};
use crate::{
    media::{
        ObservationControl,
        shared_publisher::{self, Publisher},
    },
    session_startup::shared_viewers::{self, Admission, Entropy, Hub},
};
use asupersync::{
    cx::Cx,
    time::{TimerDriverHandle, TimerHandle},
    types::Time,
};
use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Consent(ConsentError),
    Startup(crate::session_startup::Error),
    Preparation(super::prepare::Error),
    SourceSetup,
    Publication(crate::session_startup::PublisherError),
    Viewers(shared_viewers::Error),
    Capture(shared_publisher::Error),
    LocalEvent,
    Closed,
    Clock,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalAction {
    Continue,
    Stop,
}
/// Permission and driver milestones only, never frame freshness or input grants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    pub source_renewals: u64,
    pub viewers: shared_viewers::Statistics,
}

impl SessionAgent {
    /// Service an already-approved source and its exact original viewer hub as
    /// one lifetime. Source identity and this local agent's unique registration
    /// are checked synchronously. A foreign source/agent refuses, never takes over.
    /// Dropping even the unpolled returned future fences every viewer (including
    /// pending joins) and source before cancelling either sibling future.
    ///
    /// `local` drains bounded, nonblocking actual platform events/probes into this
    /// agent and may register `task.waker()` for immediate event delivery. It runs
    /// BEFORE capture or network on every poll, plus a source-clock maintenance
    /// wake within 10 ms or an earlier consent deadline. It must not fabricate OS
    /// permission, block on native work, or discard input cleanup returned by any
    /// agent lifecycle method. This owner serves observation only, not input.
    /// Returning Stop closes this source/cohort, not other sources of the agent.
    ///
    /// The single capture future and each original network future are retained
    /// across waits, not cancelled/recreated on timer or peer progress. Only fresh
    /// LOCAL checks renew source consent. Viewer renewal remains independent.
    /// Keep Publisher afterward to confirm child exit through its existing reap.
    pub fn serve_shared_desktop<'a, L>(
        &'a mut self,
        publisher: &'a mut Publisher,
        hub: &'a mut Hub,
        capture_interval: Duration,
        entropy: Entropy,
        local: L,
    ) -> Result<impl Future<Output = Result<Report, Error>> + Send + use<'a, L>, Error>
    where
        L: FnMut(&mut SessionAgent, &mut Context<'_>) -> Result<LocalAction, ()> + Send + 'a,
    {
        let source = hub.service_owner(publisher).map_err(Error::Viewers)?;
        let driver = source.context().timer_driver().ok_or(Error::Clock)?;
        let registration = self
            .register_original_source(publisher)
            .map_err(Error::Consent)?;
        let admission = hub.admissions();
        Ok(Running {
            agent: self,
            source,
            registration,
            admission,
            entropy,
            local: Box::new(local),
            network: Some(Box::pin(hub.serve())),
            capture: Some(Box::pin(publisher.serve(capture_interval, |_| {}))),
            timer: Wake {
                driver,
                handle: None,
            },
            source_renewals: 0,
            finished: false,
        })
    }
}

type Service<'a, T, E> = Pin<Box<dyn Future<Output = Result<T, E>> + Send + 'a>>;
struct Running<'a, L> {
    agent: &'a mut SessionAgent,
    source: ObservationControl,
    registration: Arc<Renewal>,
    admission: Admission,
    entropy: Entropy,
    local: Box<L>,
    network: Option<Service<'a, shared_viewers::Statistics, shared_viewers::Error>>,
    capture: Option<Service<'a, (), shared_publisher::Error>>,
    timer: Wake,
    source_renewals: u64,
    finished: bool,
}
impl<L> Running<'_, L> {
    fn finish(&mut self) {
        if !self.finished {
            self.finished = true;
            // Fence ALL original viewers, including joins not yet in Publisher,
            // before releasing any media or cancelling any network/native await.
            self.admission.fence();
            self.registration.close();
            drop(self.network.take());
            drop(self.capture.take());
            self.timer.cancel();
        }
    }
    fn report(&self, viewers: shared_viewers::Statistics) -> Report {
        Report {
            source_renewals: self.source_renewals,
            viewers,
        }
    }
}
impl<L> Running<'_, L>
where
    L: FnMut(&mut SessionAgent, &mut Context<'_>) -> Result<LocalAction, ()>,
{
    fn turn(&mut self, task: &mut Context<'_>) -> Poll<Result<Report, Error>> {
        if self.finished {
            return Poll::Ready(Err(Error::Closed));
        }
        self.source
            .check()
            .map_err(|e| Error::Capture(shared_publisher::Error::Media(e)))?;
        if (self.local)(self.agent, task).map_err(|()| Error::LocalEvent)? == LocalAction::Stop {
            return Poll::Ready(Ok(
                self.report(self.admission.statistics().map_err(Error::Viewers)?)
            ));
        }
        // Service only THIS source's original registration, not unrelated sources
        // retained by the local agent. Caller entropy executes outside all locks.
        let status = self
            .registration
            .service(self.agent, &mut || (self.entropy)())
            .map_err(Error::Consent)?;
        self.source_renewals = self
            .source_renewals
            .saturating_add(u64::from(status.renewed));
        if let Poll::Ready(result) = self
            .network
            .as_mut()
            .ok_or(Error::Closed)?
            .as_mut()
            .poll(task)
        {
            return Poll::Ready(result.map(|s| self.report(s)).map_err(Error::Viewers));
        }
        self.registration
            .recheck(self.agent)
            .map_err(Error::Consent)?;
        // Publisher's existing Sleep binds at first poll. Bind its original source
        // Cx only around synchronous poll, never hold thread-local context across
        // an await or use an unrelated ambient clock/fallback timer thread.
        {
            let _current = Cx::set_current(Some(self.source.context()));
            if let Poll::Ready(result) = self
                .capture
                .as_mut()
                .ok_or(Error::Closed)?
                .as_mut()
                .poll(task)
            {
                return Poll::Ready(match result {
                    Err(e) => Err(Error::Capture(e)),
                    Ok(()) => Ok(self.report(self.admission.statistics().map_err(Error::Viewers)?)),
                });
            }
        }
        self.registration
            .recheck(self.agent)
            .map_err(Error::Consent)?;
        let now = self
            .source
            .check()
            .map_err(|e| Error::Capture(shared_publisher::Error::Media(e)))?;
        let wake = now
            .as_micros()
            .checked_add(10_000)
            .ok_or(Error::Clock)?
            .min(status.next_check.as_micros());
        self.timer.arm(
            Time::from_nanos(wake.checked_mul(1000).ok_or(Error::Clock)?),
            task,
        );
        Poll::Pending
    }
}
impl<L> Future for Running<'_, L>
where
    L: FnMut(&mut SessionAgent, &mut Context<'_>) -> Result<LocalAction, ()>,
{
    type Output = Result<Report, Error>;
    fn poll(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<Self::Output> {
        let running = self.get_mut();
        let mut guard = PollGuard {
            running,
            completed: false,
        };
        let result = guard.running.turn(task);
        if result.is_ready() {
            // A caller may retain a completed future. Terminal result itself
            // fences/cancels owners; safety does not depend on caller Drop.
            guard.running.finish();
        }
        guard.completed = true;
        result
    }
}
struct PollGuard<'r, 'a, L> {
    running: &'r mut Running<'a, L>,
    completed: bool,
}
impl<L> Drop for PollGuard<'_, '_, L> {
    fn drop(&mut self) {
        if !self.completed {
            self.running.finish();
        }
    }
}
impl<L> Drop for Running<'_, L> {
    fn drop(&mut self) {
        self.finish();
    }
}
// One timer registered with the EXISTING source runtime, no worker/thread/ticker.
// Keep one registration under network/event floods; spent handles cannot rearm.
pub(super) struct Wake {
    pub(super) driver: TimerDriverHandle,
    pub(super) handle: Option<TimerHandle>,
}
impl Wake {
    pub(super) fn arm(&mut self, deadline: Time, task: &Context<'_>) {
        self.cancel();
        self.handle = Some(self.driver.register(deadline, task.waker().clone()));
        if self.driver.now() >= deadline {
            task.waker().wake_by_ref();
        }
    }
    pub(super) fn cancel(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = self.driver.cancel(&handle);
        }
    }
}
impl Drop for Wake {
    fn drop(&mut self) {
        self.cancel();
    }
}

mod startup;

mod launch;

mod native;
pub use native::{NativeDesktop, dispatch};
