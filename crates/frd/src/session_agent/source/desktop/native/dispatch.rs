//! Cold/warm routing of authenticated Hosts to one independent OS-share owner.
//! One cold slot; warm peers use the existing bounded hub, never another queue.
use super::super::{LocalAction, Report, Wake};
use super::{NativeDesktop, SessionAgent, Setup};
use crate::session_startup::{
    Approval, Host,
    shared_viewers::{Admission, Entropy, HostService, Policy},
};
use asupersync::{
    channel::oneshot,
    cx::Cx,
    types::{CancelKind, Time},
};
use fr_media::worker::Configuration;
use fr_wire::{
    display::{Catalog, Select},
    negotiation::Role,
};
use std::{
    future::{Future, poll_fn},
    pin::{Pin, pin},
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Busy,
    Closed,
    Clock,
    WrongSession,
    Startup(crate::session_startup::Error),
    Desktop(Box<super::super::Error>),
    Viewers(crate::session_startup::shared_viewers::Error),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "native-dispatch: {self:?}")
    }
}
impl std::error::Error for Error {}
impl From<super::super::Error> for Error {
    fn from(error: super::super::Error) -> Self {
        Self::Desktop(Box::new(error))
    }
}

type Notify = Box<dyn FnMut(Approval, Role) -> Result<(), ()> + Send>;
struct First {
    host: Host,
    notify: Notify,
    reply: oneshot::Sender<HostService>,
}
enum Route {
    Cold(oneshot::Sender<First>),
    Starting(Cx),
    Warm(Admission, Arc<crate::media::ObservationControl>),
    Closed,
}
struct Shared {
    route: Mutex<Route>,
    os_session: u32,
}
impl Shared {
    fn close(&self) {
        let old = std::mem::replace(
            &mut *self
                .route
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            Route::Closed,
        );
        match old {
            Route::Starting(cx) => cx.cancel_fast(CancelKind::User),
            Route::Warm(admission, source) => {
                admission.fence();
                source.revoke();
            }
            Route::Cold(_) | Route::Closed => {}
        }
    }
}
/// Cloneable dispatch access, not authority or an owner of the source. A fresh
/// Host must already carry the original TLS, protected ingress and admission.
/// During cold startup, additional peers are refused Busy instead of queuing.
/// Once warm, existing hub limits include consent and attachment work as usual.
#[derive(Clone)]
pub struct Incoming(Arc<Shared>);
impl std::fmt::Debug for Incoming {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NativeIncoming([one OS share])")
    }
}
impl Incoming {
    /// Reserve at CALL time. Keep the returned future in the native connection's
    /// callback through termination. Dropping even an unpolled future cancels only
    /// that peer. No caller has to pass a Host through an unbounded task queue or
    /// keep the source in the first connection's cancellation scope.
    pub fn serve_host<N>(&self, mut host: Host, notify: N) -> Result<Peer, Error>
    where
        N: FnMut(Approval, Role) -> Result<(), ()> + Send + 'static,
    {
        let cx = host.cancellation_context();
        let mut fence = PeerFence(Some(cx.clone()));
        let (_, binding, until) = host.restrict_shared_observer().map_err(Error::Startup)?;
        if binding.os_session.as_raw() != u128::from(self.0.os_session) {
            return Err(Error::WrongSession);
        }
        let route = {
            let mut state = self.0.route.lock().map_err(|_| Error::Closed)?;
            match &*state {
                Route::Cold(_) => std::mem::replace(&mut *state, Route::Starting(cx.clone())),
                Route::Starting(_) => return Err(Error::Busy),
                Route::Warm(admission, source) => Route::Warm(admission.clone(), source.clone()),
                Route::Closed => return Err(Error::Closed),
            }
        };
        let work: Pin<Box<dyn Future<Output = Result<(), Error>> + Send>> = match route {
            Route::Cold(send) => {
                let (reply, mut receive) = oneshot::channel();
                if send
                    .send(
                        &cx,
                        First {
                            host,
                            notify: Box::new(notify),
                            reply,
                        },
                    )
                    .is_err()
                {
                    self.0.close();
                    return Err(Error::Closed);
                }
                let clock = cx.clone();
                let deadline = until.checked_mul(1000).ok_or(Error::Clock)?;
                Box::pin(async move {
                    // Original Host budget includes all queue/approval/native time.
                    let service = asupersync::time::timeout_at(
                        Time::from_nanos(deadline),
                        receive.recv(&clock),
                    )
                    .await
                    .map_err(|_| Error::Startup(crate::session_startup::Error::Expired))?
                    .map_err(|_| Error::Closed)?;
                    service.await.map_err(Error::Viewers)
                })
            }
            Route::Warm(admission, _) => {
                let service = admission.serve_host(host, notify).map_err(Error::Viewers)?;
                Box::pin(async move { service.await.map_err(Error::Viewers) })
            }
            Route::Starting(_) | Route::Closed => unreachable!(),
        };
        fence.0 = None;
        Ok(Peer { cx, work })
    }
    /// Stop this selected source and all its queued/active viewers. No new source
    /// is created automatically; another OS-share lifetime is an explicit choice.
    pub fn close(&self) {
        self.0.close();
    }
}

/// The independent OS-share task owns this driver, agent and eventual native
/// desktop. Cloned Incoming handles cannot keep any of them alive. Startup is
/// lazy: no factory/discovery/capture call before the first observer's consent.
pub struct Driver {
    shared: Arc<Shared>,
    first: Option<oneshot::Receiver<First>>,
    cx: Cx,
    agent: SessionAgent,
    desktop: Option<NativeDesktop>,
    policy: Policy,
    interval: Duration,
    entropy: Entropy,
}
impl std::fmt::Debug for Driver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NativeDesktopDriver([original agent and source])")
    }
}
impl SessionAgent {
    /// Pair the host dispatcher with an independent local OS-share driver. This
    /// does not grant OS permission or start a task. Supply a dedicated source Cx,
    /// never a viewer's Cx; run its driver alongside the native listener service.
    pub fn native_incoming(
        self,
        cx: Cx,
        policy: Policy,
        interval: Duration,
        entropy: Entropy,
    ) -> Result<(Incoming, Driver), Error> {
        policy.validate().map_err(Error::Viewers)?;
        if interval.is_zero() || interval > Duration::from_secs(1) {
            return Err(Error::from(super::super::Error::Capture(
                crate::media::shared_publisher::Error::InvalidBudget,
            )));
        }
        cx.checkpoint().map_err(|_| Error::Closed)?;
        cx.timer_driver().ok_or(Error::Clock)?;
        let (send, receive) = oneshot::channel();
        let shared = Arc::new(Shared {
            route: Mutex::new(Route::Cold(send)),
            os_session: self.permissions().os_session_id(),
        });
        Ok((
            Incoming(shared.clone()),
            Driver {
                shared,
                first: Some(receive),
                cx,
                agent: self,
                desktop: None,
                policy,
                interval,
                entropy,
            },
        ))
    }
}
impl Driver {
    /// One lifetime; terminal failure never retries source creation or replays a
    /// Host. Local events run before waiting/startup/service work and on the same
    /// bounded maintenance cadence. Capture permission and approval are separate.
    #[allow(clippy::too_many_lines)]
    pub fn serve<'a, F, S, L>(
        &'a mut self,
        factory: F,
        select: S,
        local: L,
    ) -> impl Future<Output = Result<Report, Error>> + Send + 'a
    where
        F: FnOnce() -> Result<Setup, ()> + Send + 'a,
        S: FnOnce(&Catalog) -> Result<(Select, Configuration), ()> + Send + 'a,
        L: FnMut(&mut SessionAgent, &mut Context<'_>) -> Result<LocalAction, ()> + Send + 'a,
    {
        self.serve_async(move || std::future::ready(factory()), select, local)
    }

    /// Async source factory variant of serve. Native initialization may wait
    /// without blocking the reactor or moving source ownership into the first
    /// peer's scope. Consent, deadlines, renewals and local events are unchanged.
    #[allow(clippy::too_many_lines)]
    pub fn serve_async<'a, F, P, S, L>(
        &'a mut self,
        factory: F,
        select: S,
        local: L,
    ) -> impl Future<Output = Result<Report, Error>> + Send + 'a
    where
        F: FnOnce() -> P + Send + 'a,
        P: Future<Output = Result<Setup, ()>> + Send + 'a,
        S: FnOnce(&Catalog) -> Result<(Select, Configuration), ()> + Send + 'a,
        L: FnMut(&mut SessionAgent, &mut Context<'_>) -> Result<LocalAction, ()> + Send + 'a,
    {
        let initial = self.first.take();
        let shared = self.shared.clone();
        let lifetime = self.cx.clone();
        let state = self.shared.clone();
        let mut events = local;
        let mut local = move |agent: &mut SessionAgent, task: &mut Context<'_>| {
            let check = || -> Result<(), ()> {
                lifetime.checkpoint().map_err(|_| ())?;
                if matches!(*state.route.lock().map_err(|_| ())?, Route::Closed) {
                    return Err(());
                }
                Ok(())
            };
            check()?;
            let result = events(agent, task)?;
            check()?;
            Ok(result)
        };
        let owner = Owner { driver: self };
        Guard {
            shared,
            work: Box::pin(async move {
                let mut initial = initial.ok_or(Error::Closed)?;
                let owner = owner;
                let this = &mut *owner.driver;
                let first = {
                    let mut timer = Wake {
                        driver: this.cx.timer_driver().ok_or(Error::Clock)?,
                        handle: None,
                    };
                    let mut receive = pin!(initial.recv(&this.cx));
                    let mut previous = timer.driver.now();
                    poll_fn(|task| {
                        this.cx.checkpoint().map_err(|_| Error::Closed)?;
                        if local(&mut this.agent, task)
                            .map_err(|()| Error::from(super::super::Error::LocalEvent))?
                            == LocalAction::Stop
                            || this.agent.is_revoked()
                        {
                            return Poll::Ready(Err(Error::Closed));
                        }
                        if matches!(
                            *this.shared.route.lock().map_err(|_| Error::Closed)?,
                            Route::Closed
                        ) {
                            return Poll::Ready(Err(Error::Closed));
                        }
                        let now = timer.driver.now();
                        if now < previous {
                            return Poll::Ready(Err(Error::Clock));
                        }
                        previous = now;
                        match receive.as_mut().poll(task) {
                            Poll::Ready(value) => Poll::Ready(value.map_err(|_| Error::Closed)),
                            Poll::Pending => {
                                timer.arm(
                                    Time::from_nanos(
                                        now.as_nanos()
                                            .checked_add(10_000_000)
                                            .ok_or(Error::Clock)?,
                                    ),
                                    task,
                                );
                                Poll::Pending
                            }
                        }
                    })
                    .await?
                };
                let First {
                    host,
                    notify,
                    reply,
                } = first;
                let desktop = this
                    .agent
                    .open_native_shared_desktop_async(
                        host,
                        factory,
                        select,
                        this.policy,
                        this.entropy.clone(),
                        notify,
                        &mut local,
                    )
                    .map_err(Error::from)?
                    .await
                    .map_err(Error::from)?;
                this.desktop = Some(desktop);
                let desktop = this.desktop.as_mut().ok_or(Error::Closed)?;
                let service = desktop.first().into_service().map_err(Error::Viewers)?;
                let admission = desktop.admissions();
                let source = desktop
                    .hub
                    .service_owner(&desktop.prepared.publisher)
                    .map_err(Error::Viewers)?;
                // Install the original continuous-service guard BEFORE publishing
                // warm routing; it fences reentrant joins on any later failure.
                let running = desktop
                    .serve(&mut this.agent, this.interval, this.entropy.clone(), local)
                    .map_err(Error::from)?;
                {
                    let mut route = this.shared.route.lock().map_err(|_| Error::Closed)?;
                    let Route::Starting(peer) = &*route else {
                        return Err(Error::Closed);
                    };
                    peer.checkpoint().map_err(|_| Error::Closed)?;
                    *route = Route::Warm(admission, Arc::new(source));
                }
                // Only the first peer's connection scope receives its own service.
                // A dropped first peer must not keep a newly created source alive.
                if reply.send(&this.cx, service).is_err() {
                    return Err(Error::Closed);
                }
                running.await.map_err(Error::from)
            }),
        }
    }
    /// Keep the original child cleanup owner after service ends. No second native
    /// worker or fresh deadline is created here. Caller supplies cleanup budget.
    /// None means opening never delivered a desktop, NOT proof that preparation
    /// spawned no child. The factory must retain `Launch::retain_cleanup` outside
    /// the operation for failed/abandoned native preparation, as with the opener.
    pub fn reap<'a>(
        &'a mut self,
        cleanup: &'a Cx,
        deadline: crate::worker::Deadline,
    ) -> impl Future<Output = Result<Option<asupersync::process::ExitStatus>, crate::worker::Error>> + 'a
    {
        self.close();
        async move {
            match &mut self.desktop {
                Some(d) => d.reap(cleanup, deadline).await.map(Some),
                None => Ok(None),
            }
        }
    }
    pub fn close(&mut self) {
        self.shared.close();
        if let Some(desktop) = &mut self.desktop {
            desktop.close();
        }
        drop(self.first.take());
    }
    pub fn worker_id(&self) -> Option<u32> {
        self.desktop.as_ref().and_then(NativeDesktop::worker_id)
    }
}
impl Drop for Driver {
    fn drop(&mut self) {
        self.close();
    }
}
struct Owner<'a> {
    driver: &'a mut Driver,
}
impl Drop for Owner<'_> {
    fn drop(&mut self) {
        self.driver.close();
    }
}
struct Guard<F> {
    shared: Arc<Shared>,
    work: Pin<Box<F>>,
}
impl<F: Future> Future for Guard<F> {
    type Output = F::Output;
    fn poll(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let mut guard = SharedFence(Some(&this.shared));
        let result = this.work.as_mut().poll(task);
        if result.is_pending() {
            guard.0 = None;
        }
        result
    }
}
impl<F> Drop for Guard<F> {
    fn drop(&mut self) {
        self.shared.close();
    }
}
struct SharedFence<'a>(Option<&'a Shared>);
impl Drop for SharedFence<'_> {
    fn drop(&mut self) {
        if let Some(s) = self.0 {
            s.close();
        }
    }
}
struct PeerFence(Option<Cx>);
impl Drop for PeerFence {
    fn drop(&mut self) {
        if let Some(cx) = &self.0 {
            cx.cancel_fast(CancelKind::User);
        }
    }
}
/// Original connection scope: a completed result never reopens it. The native
/// listener callback retains this future; only the independent Driver owns media.
#[must_use = "dropping a peer service cancels that viewer"]
pub struct Peer {
    cx: Cx,
    work: Pin<Box<dyn Future<Output = Result<(), Error>> + Send>>,
}
impl Future for Peer {
    type Output = Result<(), Error>;
    fn poll(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let mut fence = PeerFence(Some(this.cx.clone()));
        let _current = Cx::set_current(Some(this.cx.clone()));
        let result = this.work.as_mut().poll(task);
        if result.is_pending() {
            fence.0 = None;
        }
        result
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.cx.cancel_fast(CancelKind::User);
    }
}
