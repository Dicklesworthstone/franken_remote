//! First-viewer bootstrap under the same original local source-consent owner.
use super::{Error, LocalAction, Renewal, SessionAgent, Wake};
use crate::{
    media::{ObservationControl, SharedCaptureUpdate, shared_publisher::Publisher},
    session_startup::{
        Approval, Host,
        shared_viewers::{Entropy, Hub, Policy},
    },
};
use asupersync::{
    cx::Cx,
    types::{CancelKind, Time},
};
use fr_wire::negotiation::Role;
use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

impl SessionAgent {
    /// Open the FIRST observer of an independently authorized, locally selected
    /// source, with local events and consent renewal serviced during negotiation,
    /// approval and media attachment. The returned Hub retains the SAME Host and
    /// pending decoder handshake; hand it to `serve_shared_desktop` to continue.
    /// Success means service ownership, NOT first decode, visibility or control.
    ///
    /// The original Host and unused Publisher deadlines both keep running. A
    /// slow approval cannot extend the source's two-second unused-startup budget.
    /// No new capture is admitted before a subscriber exists, and the supplied
    /// initial IDR retains its actual capture timestamp and reference deadline.
    /// Sources should therefore be prepared near admission, not kept warm idle.
    ///
    /// `first` must come from the protected TLS/tailnet admission boundary.
    /// `notify` is one bounded nonblocking LOCAL notification, not permission.
    /// `local` must feed real platform events/probes and retain held-input cleanup.
    /// Neither callback runs under policy locks. Dropping even the unpolled future,
    /// local Stop, failure or unwinding fences the original source and peer before
    /// releasing startup work. Publisher remains available for supervised reaping.
    #[allow(clippy::too_many_arguments)]
    pub fn open_shared_desktop<'a, N, L>(
        &'a mut self,
        mut first: Host,
        publisher: &'a mut Publisher,
        initial: &'a SharedCaptureUpdate,
        policy: Policy,
        entropy: Entropy,
        mut notify: N,
        local: L,
    ) -> Result<impl Future<Output = Result<Hub, Error>> + Send + use<'a, N, L>, Error>
    where
        N: FnMut(Approval, Role) -> Result<(), ()> + Send + 'a,
        L: FnMut(&mut SessionAgent, &mut Context<'_>) -> Result<LocalAction, ()> + Send + 'a,
    {
        policy.validate().map_err(Error::Viewers)?;
        let source = publisher.opening_control(initial).map_err(Error::Capture)?;
        let driver = source.context().timer_driver().ok_or(Error::Clock)?;
        let queue = publisher.join_queue();
        let (peer, binding, until) = first
            .bind_shared_source(queue.clone())
            .map_err(Error::Startup)?;
        if binding.os_session.as_raw() != u128::from(self.permissions().os_session_id()) {
            return Err(Error::Consent(super::ConsentError::SessionChanged));
        }
        let registration = self.register_original_source(publisher).map_err(|error| {
            peer.cancel_fast(CancelKind::User);
            Error::Consent(error)
        })?;
        let tickets = entropy.clone();
        let renewal_entropy = entropy.clone();
        let inner = Box::pin(async move {
            let session = first
                .open(Duration::from_millis(5), move |approval, role| {
                    queue.check_source().map_err(|_| ())?;
                    notify(approval, role)?;
                    queue.check_source().map_err(|_| ())
                })
                .await
                .map_err(Error::Startup)?;
            let host = session
                .start_shared_display_capped(
                    publisher,
                    initial,
                    policy.join_timeout,
                    until,
                    policy.send,
                    move || tickets(),
                )
                .await
                .map_err(Error::Publication)?;
            Hub::new(host, policy, entropy).map_err(Error::Viewers)
        });
        // The guard exists at CALL time; none of its authority depends on a first poll.
        Ok(Opening {
            agent: self,
            source,
            registration,
            peer,
            until,
            local: Box::new(local),
            entropy: renewal_entropy,
            inner: Some(inner),
            timer: Wake {
                driver,
                handle: None,
            },
            finished: false,
        })
    }
}

type Work<'a> = Pin<Box<dyn Future<Output = Result<Hub, Error>> + Send + 'a>>;
struct Opening<'a, L> {
    agent: &'a mut SessionAgent,
    source: ObservationControl,
    registration: Arc<Renewal>,
    peer: Cx,
    until: u64,
    entropy: Entropy,
    local: Box<L>,
    inner: Option<Work<'a>>,
    timer: Wake,
    finished: bool,
}
impl<L> Opening<'_, L> {
    fn finish(&mut self) {
        if !self.finished {
            self.finished = true;
            self.peer.cancel_fast(CancelKind::User);
            self.registration.close();
            drop(self.inner.take());
            self.timer.cancel();
        }
    }
}
impl<L> Opening<'_, L>
where
    L: FnMut(&mut SessionAgent, &mut Context<'_>) -> Result<LocalAction, ()>,
{
    fn turn(&mut self, task: &mut Context<'_>) -> Poll<Result<Hub, Error>> {
        if self.finished {
            return Poll::Ready(Err(Error::Closed));
        }
        self.source
            .check()
            .map_err(|e| Error::Capture(crate::media::shared_publisher::Error::Media(e)))?;
        if peer_now(&self.peer)? >= self.until {
            return Poll::Ready(Err(Error::Startup(crate::session_startup::Error::Expired)));
        }
        if (self.local)(self.agent, task).map_err(|()| Error::LocalEvent)? == LocalAction::Stop {
            return Poll::Ready(Err(Error::Closed));
        }
        let status = self
            .registration
            .service(self.agent, &mut || (self.entropy)())
            .map_err(Error::Consent)?;
        match self
            .inner
            .as_mut()
            .ok_or(Error::Closed)?
            .as_mut()
            .poll(task)
        {
            Poll::Ready(result) => {
                let mut hub = result?;
                if let Err(error) = self.registration.recheck(self.agent) {
                    hub.close();
                    return Poll::Ready(Err(Error::Consent(error)));
                }
                // Transfer the original source registration and network ownership,
                // not a refreshed permission, budget, readiness or capture proof.
                self.finished = true;
                self.timer.cancel();
                drop(self.inner.take());
                Poll::Ready(Ok(hub))
            }
            Poll::Pending => {
                self.registration
                    .recheck(self.agent)
                    .map_err(Error::Consent)?;
                let now = peer_now(&self.peer)?;
                let wake = now
                    .checked_add(10_000)
                    .ok_or(Error::Clock)?
                    .min(status.next_check.as_micros())
                    .min(self.until);
                self.timer.arm(
                    Time::from_nanos(wake.checked_mul(1000).ok_or(Error::Clock)?),
                    task,
                );
                Poll::Pending
            }
        }
    }
}
impl<L> Future for Opening<'_, L>
where
    L: FnMut(&mut SessionAgent, &mut Context<'_>) -> Result<LocalAction, ()>,
{
    type Output = Result<Hub, Error>;
    fn poll(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<Self::Output> {
        let mut guard = PollGuard {
            opening: self.get_mut(),
            completed: false,
        };
        let result = guard.opening.turn(task);
        if result.is_ready() {
            guard.opening.finish();
        }
        guard.completed = true;
        result
    }
}
struct PollGuard<'r, 'a, L> {
    opening: &'r mut Opening<'a, L>,
    completed: bool,
}
impl<L> Drop for PollGuard<'_, '_, L> {
    fn drop(&mut self) {
        if !self.completed {
            self.opening.finish();
        }
    }
}
impl<L> Drop for Opening<'_, L> {
    fn drop(&mut self) {
        self.finish();
    }
}

fn peer_now(cx: &Cx) -> Result<u64, Error> {
    cx.checkpoint()
        .map_err(|_| Error::Startup(crate::session_startup::Error::Cancelled))?;
    Ok(crate::media::host_now(cx)
        .map_err(|_| Error::Clock)?
        .as_micros())
}
