//! Cold/warm routing of authenticated Hosts to one independent OS-share owner.
//! One cold slot; warm peers use the existing bounded hub, never another queue.
use super::super::{LocalAction, Report, Wake};
use super::{Opened, SessionAgent, Setup};
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
impl Error {
    /// True when this is one observer's own outcome rather than a host fault:
    /// the observer left, timed out, broke protocol or was refused. A running
    /// share records each viewer's end in `Report` and ends `Ok` when its last
    /// viewer leaves. A cold share instead fails with the FIRST observer's own
    /// session, display-choice, attachment or deadline error, because that
    /// observer's startup is fused with the source's. Source setup, preparation,
    /// capture, consent, local, clock, entropy and hub failures are host faults.
    /// Classification uses typed stages only. A new variant must be placed here.
    #[must_use]
    pub fn is_peer_outcome(&self) -> bool {
        match self {
            Self::Busy | Self::WrongSession => true,
            Self::Startup(error) => peer_session(error),
            Self::Viewers(error) => peer_viewers(error),
            Self::Desktop(error) => match &**error {
                super::super::Error::Startup(error) => peer_session(error),
                super::super::Error::Publication(error) => peer_publication(error),
                super::super::Error::Viewers(error) => peer_viewers(error),
                super::super::Error::Consent(_)
                | super::super::Error::Preparation(_)
                | super::super::Error::SourceSetup
                | super::super::Error::Capture(_)
                | super::super::Error::LocalEvent
                | super::super::Error::Closed
                | super::super::Error::Clock
                | super::super::Error::NoInputPermission
                | super::super::Error::InputCleanup => false,
            },
            Self::Closed | Self::Clock => false,
        }
    }
}
// One observer's Host session: its transport, negotiation, local approval,
// renewal, admission refresh, deadline or cancellation.
fn peer_session(error: &crate::session_startup::Error) -> bool {
    use crate::session_startup::Error as E;
    match error {
        E::PublicationRegistry(_)
        | E::SharedPublication(_)
        | E::InvalidConfiguration
        | E::Clock => false,
        E::ReferenceRecovery(_)
        | E::DecoderStartup(_)
        | E::Clipboard(_)
        | E::ReceiverFeedback(_)
        | E::PresentedState(_)
        | E::Media(_)
        | E::MediaTransport(_)
        | E::ClientStartup(_)
        | E::Input(_)
        | E::ControlRenewal(_)
        | E::ControlGrant(_)
        | E::ClientRenewal(_)
        | E::Renewal(_)
        | E::Admission(_)
        | E::Protocol(_)
        | E::Transport(_)
        | E::Authority
        | E::Order
        | E::Denied
        | E::Expired
        | E::ClockSynchronization
        | E::Cancelled
        | E::RemoteClosed(_)
        | E::Closed => true,
    }
}
// The first observer's display choice, attachments and decoder fit on its own
// session. `Media` is that observer's authority; `Shared` is the source.
fn peer_publication(error: &crate::session_startup::PublisherError) -> bool {
    use crate::session_startup::PublisherError as E;
    match error {
        E::Session(error) => peer_session(error),
        E::Expired
        | E::Display(_)
        | E::Media(_)
        | E::Startup(_)
        | E::Transport(_)
        | E::Routes(_)
        | E::Input(_)
        | E::Wire(_) => true,
        E::InvalidConfiguration | E::Identity | E::Clock(_) | E::Shared(_) => false,
    }
}
fn peer_viewers(error: &crate::session_startup::shared_viewers::Error) -> bool {
    use crate::session_startup::shared_viewers::Error as E;
    match error {
        E::Session(error) => peer_session(error),
        E::Publication(error) => peer_publication(error),
        E::Full | E::DuplicateSession | E::ForeignScope | E::WrongRole => true,
        E::InvalidPolicy | E::Closed | E::Poisoned | E::Source(_) => false,
    }
}

type Notify = Box<dyn FnMut(Approval, Role) -> Result<(), ()> + Send>;
struct First {
    host: Host,
    notify: Notify,
    reply: oneshot::Sender<FirstService>,
}
/// What the first viewer's own connection scope awaits: its shared-viewer
/// service, or the end of the exclusive controlled share (sender dropped),
/// which the independent Driver serves on this same Host session.
enum FirstService {
    Shared(HostService),
    Controlled(oneshot::Receiver<()>),
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
    // The agent carries a ControlProfile: only its cold first viewer may
    // negotiate control. Warm joins always go through the observer hub.
    control: bool,
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
        // Without control every Host is restricted to observation before its
        // ClientHello. With control only a cold first viewer stays unrestricted
        // (the opener decides by its negotiated role); a warm join is restricted
        // by the hub, and a Starting route refuses Busy before any negotiation.
        let (_, binding, until) = if self.0.control {
            host.shared_open_context()
        } else {
            host.restrict_shared_observer()
        }
        .map_err(Error::Startup)?;
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
                    match service {
                        FirstService::Shared(service) => service.await.map_err(Error::Viewers),
                        // The Driver owns and reports the controlled share; this
                        // scope only keeps the connection until it has ended.
                        FirstService::Controlled(mut ended) => {
                            let _ = ended.recv(&clock).await;
                            Ok(())
                        }
                    }
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
    desktop: Option<Opened>,
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
            control: self.control_profile().is_some(),
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
    /// Access this exact driver's authenticated-Host dispatcher. The handle is
    /// not permission and cannot keep the source alive after Driver stops.
    pub fn incoming(&self) -> Incoming {
        Incoming(self.shared.clone())
    }

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
                let opened = this
                    .agent
                    .open_native_desktop_async(
                        host,
                        factory,
                        select,
                        this.policy,
                        this.entropy.clone(),
                        notify,
                        &mut local,
                        this.interval,
                    )
                    .map_err(Error::from)?
                    .await
                    .map_err(Error::from)?;
                this.desktop = Some(opened);
                let desktop = match this.desktop.as_mut().ok_or(Error::Closed)? {
                    Opened::Shared(desktop) => desktop,
                    Opened::Controlled(desktop) => {
                        // Exclusive: the route stays Starting, so every later
                        // viewer is refused Busy until this share has ended.
                        let (ended, waiting) = oneshot::channel();
                        if reply
                            .send(&this.cx, FirstService::Controlled(waiting))
                            .is_err()
                        {
                            return Err(Error::Closed);
                        }
                        let result = desktop
                            .serve(
                                &mut this.agent,
                                this.cx.clone(),
                                this.entropy.clone(),
                                local,
                            )
                            .await;
                        drop(ended);
                        return controlled_outcome(result);
                    }
                };
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
                if reply.send(&this.cx, FirstService::Shared(service)).is_err() {
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
        self.desktop.as_ref().and_then(Opened::worker_id)
    }
}
/// A running controlled share ends like a shared one: its viewer's own ending
/// (departure, protocol, deadline, its lease ending) is the served viewer's
/// recorded outcome, while capture, consent, local and input-cleanup failures
/// remain host faults. Opening failures never reach here.
fn controlled_outcome(result: Result<Report, super::super::Error>) -> Result<Report, Error> {
    match result.map_err(Error::from) {
        Ok(report) => Ok(report),
        Err(error) if error.is_peer_outcome() => Ok(super::controlled::served(true)),
        Err(error) => Err(error),
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

mod linux;
pub use linux::{Completion, FinishedFirst};

#[cfg(test)]
mod tests {
    use super::{super::super::Error as Desktop, Error};
    use crate::{
        display_selection::Error as Display,
        media::{self, shared_publisher},
        session_agent::source::{Error as Consent, prepare},
        session_startup::{Error as Session, PublisherError as Publication, shared_viewers},
        worker,
    };
    use fr_transport::quic;

    fn desktop(error: Desktop) -> Error {
        Error::Desktop(Box::new(error))
    }

    #[test]
    fn a_departing_or_refused_first_observer_is_a_peer_outcome() {
        let cancelled = media::Error::Worker(worker::Error::Cancelled);
        for error in [
            // Observed when `fr displays` leaves after reading the catalog.
            desktop(Desktop::Publication(Publication::Display(
                Display::Transport(quic::Error::Expired),
            ))),
            desktop(Desktop::Publication(Publication::Media(cancelled))),
            desktop(Desktop::Publication(Publication::Expired)),
            desktop(Desktop::Startup(Session::Transport(quic::Error::Expired))),
            desktop(Desktop::Startup(Session::Denied)),
            desktop(Desktop::Startup(Session::Expired)),
            desktop(Desktop::Startup(Session::Cancelled)),
            desktop(Desktop::Viewers(shared_viewers::Error::Session(
                Session::Expired,
            ))),
            Error::Busy,
            Error::Startup(Session::Protocol(fr_wire::negotiation::Error::Version)),
        ] {
            assert!(error.is_peer_outcome(), "{error:?}");
        }
    }

    #[test]
    fn a_running_controlled_share_records_its_viewers_ending_but_not_host_faults() {
        let peer = super::controlled_outcome(Err(Desktop::Publication(Publication::Session(
            Session::Transport(quic::Error::Expired),
        ))))
        .unwrap();
        assert_eq!((peer.viewers.admitted, peer.viewers.failed), (1, 1));
        for fault in [
            Desktop::InputCleanup,
            Desktop::NoInputPermission,
            Desktop::LocalEvent,
            Desktop::Publication(Publication::Shared(shared_publisher::Error::Closed)),
        ] {
            let error = super::controlled_outcome(Err(fault)).unwrap_err();
            assert!(!error.is_peer_outcome(), "{error:?}");
        }
    }

    #[test]
    fn source_capture_consent_local_and_clock_failures_are_host_faults() {
        let exited = media::Error::Worker(worker::Error::PeerClosed);
        for error in [
            desktop(Desktop::SourceSetup),
            desktop(Desktop::Preparation(prepare::Error::Media(exited))),
            desktop(Desktop::Preparation(prepare::Error::Expired)),
            desktop(Desktop::Capture(shared_publisher::Error::Closed)),
            desktop(Desktop::Capture(shared_publisher::Error::Media(exited))),
            desktop(Desktop::Consent(Consent::NoCapturePermission)),
            desktop(Desktop::Publication(Publication::Shared(
                shared_publisher::Error::Closed,
            ))),
            desktop(Desktop::Publication(Publication::Session(
                Session::SharedPublication(shared_publisher::Error::Closed),
            ))),
            desktop(Desktop::Publication(Publication::Identity)),
            desktop(Desktop::Startup(Session::Clock)),
            desktop(Desktop::Viewers(shared_viewers::Error::Source(
                shared_publisher::Error::Closed,
            ))),
            desktop(Desktop::LocalEvent),
            desktop(Desktop::Closed),
            desktop(Desktop::Clock),
            Error::Viewers(shared_viewers::Error::Closed),
            Error::Closed,
            Error::Clock,
        ] {
            assert!(!error.is_peer_outcome(), "{error:?}");
        }
    }
}
