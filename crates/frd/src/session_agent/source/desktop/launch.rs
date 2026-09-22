//! One guarded lifetime from the first admitted Host to continuous shared service.
use super::{Error, LocalAction, Renewal, Report, SessionAgent};
use crate::{
    media::{SharedCaptureUpdate, shared_publisher::Publisher},
    session_startup::{
        Approval, Host,
        shared_viewers::{Admission, Entropy, Policy, Ticket},
    },
};
use asupersync::{cx::Cx, types::CancelKind};
use fr_wire::negotiation::Role;
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};

impl SessionAgent {
    /// Bootstrap the first protected/admitted observer and immediately serve the
    /// same shared desktop, without a caller-managed ownership handoff. The source
    /// must already be independently authorized and locally selected. No listener
    /// or OS permission is created; native events/probes are still required.
    ///
    /// `announce` receives weak admission access and the first viewer's cancellation
    /// ticket exactly once, after media attachment and BEFORE decoder completion.
    /// It is a bounded nonblocking LOCAL callback, NOT a readiness notification.
    /// It may admit other protected Hosts immediately. Neither it nor approval or
    /// local-event callbacks run under source or registry locks. A callback error,
    /// panic or revocation cannot release a new capture or an unfenced pending join.
    ///
    /// The original source, Host, initial picture and startup deadlines remain in
    /// force. The SAME local callback and entropy supplier span both phases. The
    /// report counts consent renewals during continuous service, not bootstrap.
    /// Dropping even an unpolled future or retaining one after a caught panic fences
    /// all owned authority before dropping network/native work. Retain Publisher
    /// to reap its original child; source failure is terminal, never an auto-retry.
    #[allow(clippy::too_many_arguments)]
    pub fn run_shared_desktop<'a, N, L, A>(
        &'a mut self,
        mut first: Host,
        publisher: &'a mut Publisher,
        initial: &'a SharedCaptureUpdate,
        policy: Policy,
        capture_interval: Duration,
        entropy: Entropy,
        notify: N,
        mut local: L,
        announce: A,
    ) -> Result<impl Future<Output = Result<Report, Error>> + Send + use<'a, N, L, A>, Error>
    where
        N: FnMut(Approval, Role) -> Result<(), ()> + Send + 'a,
        L: FnMut(&mut SessionAgent, &mut Context<'_>) -> Result<LocalAction, ()> + Send + 'a,
        A: FnOnce(Admission, Ticket) -> Result<(), ()> + Send + 'a,
    {
        // Validate at CALL time, before borrowing the agent/publisher inside the
        // two-phase future. The second validation never resets any original clock.
        policy.validate().map_err(Error::Viewers)?;
        let source = publisher.opening_control(initial).map_err(Error::Capture)?;
        source.context().timer_driver().ok_or(Error::Clock)?;
        let (peer, binding, _) = first.shared_open_context().map_err(Error::Startup)?;
        if binding.os_session.as_raw() != u128::from(self.permissions().os_session_id()) {
            return Err(Error::Consent(super::ConsentError::SessionChanged));
        }
        let registration = self.register_original_source(publisher).map_err(|error| {
            peer.cancel_fast(CancelKind::User);
            Error::Consent(error)
        })?;
        // One fixed metadata cell only, not a queue or a new authority. The outer
        // unwind guard must also fence joins created reentrantly by announce.
        let resources = Arc::new(Mutex::new(Resources {
            registration: Some(registration),
            cohort: None,
        }));
        let retained = resources.clone();
        let inner = async move {
            let mut hub = self
                .open_shared_desktop(
                    first,
                    publisher,
                    initial,
                    policy,
                    entropy.clone(),
                    notify,
                    &mut local,
                )?
                .await?;
            let admission = hub.admissions();
            let ticket = hub.initial();
            retained.lock().map_err(|_| Error::Closed)?.cohort = Some(admission.clone());
            // Create the continuous service's guard BEFORE calling application
            // code. Its first poll rechecks actual local consent before any I/O.
            let running =
                self.serve_shared_desktop(publisher, &mut hub, capture_interval, entropy, local)?;
            announce(admission, ticket).map_err(|()| Error::LocalEvent)?;
            running.await
        };
        Ok(Launch::new(peer, resources, inner))
    }
}

type Work<'a> = Pin<Box<dyn Future<Output = Result<Report, Error>> + Send + 'a>>;
// One fixed ownership cell also supports source creation after first consent.
// Only local source owners can install these already-existing registrations.
#[derive(Default)]
pub(super) struct Resources {
    pub(super) registration: Option<Arc<Renewal>>,
    pub(super) cohort: Option<Admission>,
}
pub(super) struct Launch<'a> {
    peer: Cx,
    resources: Arc<Mutex<Resources>>,
    inner: Option<Work<'a>>,
    finished: bool,
}
impl<'a> Launch<'a> {
    pub(super) fn new(
        peer: Cx,
        resources: Arc<Mutex<Resources>>,
        inner: impl Future<Output = Result<Report, Error>> + Send + 'a,
    ) -> Self {
        Self {
            peer,
            resources,
            inner: Some(Box::pin(inner)),
            finished: false,
        }
    }
    fn finish(&mut self) {
        if !self.finished {
            self.finished = true;
            let Resources {
                registration,
                cohort,
            } = std::mem::take(
                &mut *self
                    .resources
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
            if let Some(admission) = cohort {
                admission.fence();
            }
            self.peer.cancel_fast(CancelKind::User);
            if let Some(registration) = registration {
                registration.close();
            }
            drop(self.inner.take());
        }
    }
}
impl Future for Launch<'_> {
    type Output = Result<Report, Error>;
    fn poll(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<Self::Output> {
        let mut guard = Turn {
            launch: self.get_mut(),
            complete: false,
        };
        if guard.launch.finished {
            guard.complete = true;
            return Poll::Ready(Err(Error::Closed));
        }
        let result = match guard.launch.inner.as_mut() {
            Some(inner) => inner.as_mut().poll(task),
            None => Poll::Ready(Err(Error::Closed)),
        };
        if result.is_ready() {
            guard.launch.finish();
        }
        guard.complete = true;
        result
    }
}
struct Turn<'r, 'a> {
    launch: &'r mut Launch<'a>,
    complete: bool,
}
impl Drop for Turn<'_, '_> {
    fn drop(&mut self) {
        if !self.complete {
            self.launch.finish();
        }
    }
}
impl Drop for Launch<'_> {
    fn drop(&mut self) {
        self.finish();
    }
}
