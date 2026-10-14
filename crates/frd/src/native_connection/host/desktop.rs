//! One protected listener and its independently owned native desktop service.
//! The original TLS/LocalAPI/consent/media owners are composed, never replaced.
use super::{LinuxError, LinuxServer, Request, serial};
mod drain;
use crate::{
    session_agent::{
        SessionAgent,
        source::{
            desktop::{self, LocalAction, dispatch},
            prepare::Setup,
        },
    },
    session_startup::{Approval, Host},
};
use asupersync::{cx::Cx, runtime::RuntimeHandle, types::CancelKind};
use drain::Budget as DrainBudget;
use fr_wire::{
    display::{Catalog, Select},
    negotiation::Role,
};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
};

/// An authentic per-peer outcome, not a presentation or OS-effect acknowledgement.
/// The outer error is native admission/lifetime; the inner is desktop service.
pub type PeerResult = Result<Result<(), dispatch::Error>, super::Error>;

/// Local request allocation, approval notification and terminal peer reporting.
/// Callbacks must not block. `approval` is cloned for each new peer; an approval
/// notification is NOT consent. `request` allocates fresh IDs as required by the
/// existing serial Application contract. All callbacks run outside owner locks.
///
/// `completed` receives each result the listener actually obtained, before its
/// transport-retirement wait. It cannot override fatal identity/ingress failures.
/// A terminal desktop lets an admitted peer finish its closure report and
/// completion within the fixed drain budget, with new requests fenced.
/// Abandonment never fabricates a completion result.
pub struct Connections<R, N, C> {
    pub request: R,
    pub approval: N,
    pub completed: C,
}

/// Which original service completed first. Cancellation/listener termination
/// fences both owners immediately, then lets the original desktop complete its
/// bounded cleanup/terminal reporting. Never proof of native worker exit.
/// Retain Driver and `LinuxServer` for explicit reap/stop after every result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum End {
    Desktop(Result<desktop::Report, dispatch::Error>),
    Listener(Result<serial::Statistics, LinuxError>),
    Cancelled,
    /// The original desktop did not complete its cooperative drain within 2s.
    /// Explicit reap is still required; never restart from an uncertain owner.
    DrainExpired,
}

// Track only the original admitted application's completion, not a new task or
// authority. A source can finish before its pending peer is polled one last time.
#[derive(Default)]
struct Completion {
    active: AtomicBool,
    ending: AtomicBool,
}

struct Application<R, N, C> {
    incoming: dispatch::Incoming,
    completion: Arc<Completion>,
    callbacks: Connections<R, N, C>,
}
impl<R, N, C> serial::Application for Application<R, N, C>
where
    R: FnMut(u64) -> Result<Request, serial::Error>,
    N: FnMut(Approval, Role) -> Result<(), ()> + Clone + Send + 'static,
    C: FnMut(serial::Statistics, PeerResult) -> Result<serial::Action, serial::Error>,
{
    type Output = Result<(), dispatch::Error>;
    fn request(&mut self, attempt: u64) -> Result<Request, serial::Error> {
        if self.completion.ending.load(Ordering::Acquire) {
            return Err(serial::Error::Cancelled);
        }
        (self.callbacks.request)(attempt)
    }
    fn serve(&mut self, host: Host) -> impl Future<Output = Self::Output> {
        self.completion.active.store(true, Ordering::Release);
        // Reserve the original dispatcher slot synchronously, before returning.
        // No unbounded channel, independent permission, transport or task.
        let peer = self
            .incoming
            .serve_host(host, self.callbacks.approval.clone());
        async move { peer?.await }
    }
    // A locally stopped share finishes its admitted peer on the way out, so the
    // viewer is told the host is stopping (see `Run::poll_drain`).
    fn finishes_peer_on_cancel(&self) -> bool {
        true
    }
    fn completed(
        &mut self,
        stats: serial::Statistics,
        result: PeerResult,
    ) -> Result<serial::Action, serial::Error> {
        self.completion.active.store(false, Ordering::Release);
        let action = (self.callbacks.completed)(stats, result)?;
        Ok(if self.completion.ending.load(Ordering::Acquire) {
            serial::Action::Stop
        } else {
            action
        })
    }
}

impl LinuxServer {
    /// Drive authenticated native arrivals all the way to the original desktop,
    /// including first-peer consent, async source preparation, media attachment,
    /// and continuous capture. No caller-managed handoff or spawned task is needed.
    /// Local events are polled BEFORE listener/network work on every turn.
    ///
    /// Supply an independent listener supervisor, not the broker, credential or
    /// Driver's source Cx. The original Driver creates its own peer service through
    /// its exact Incoming handle; callers cannot substitute a different desktop.
    /// Each application remains inside its original TLS/ingress/policy scope.
    ///
    /// This serves ONE selected OS-share lifetime with capacity-one networking.
    /// Idle/unauthorized native arrivals can be retried by the serial listener;
    /// source shutdown (including last-subscriber shutdown) ends this operation.
    /// No source is automatically recreated or old session resumed. A new share
    /// requires new explicit owners and observed cleanup of the previous source.
    ///
    /// Cancellation/listener termination fences BOTH original services before
    /// polling the existing desktop through a fixed, at-most-two-second drain.
    /// The listener stays owned but is not polled during that drain: no new
    /// admission, callback or ingress retirement can race terminal reporting.
    /// Panic or abandonment still drops both owners immediately after fencing.
    /// Retain Driver for reap, the factory's `Launch::retain_cleanup` for abandoned
    /// preparation, and this `LinuxServer` for stop. Completion never proves child
    /// exit or permits removing a rule while a transport holds its ingress lease.
    #[allow(clippy::too_many_arguments)]
    pub fn serve_desktop<'a, R, N, C, F, P, S, L>(
        &'a mut self,
        driver: &'a mut dispatch::Driver,
        supervisor: Cx,
        runtime: RuntimeHandle,
        policy: serial::Policy,
        connections: Connections<R, N, C>,
        factory: F,
        select: S,
        local: L,
    ) -> impl Future<Output = End> + 'a
    where
        R: FnMut(u64) -> Result<Request, serial::Error> + 'a,
        N: FnMut(Approval, Role) -> Result<(), ()> + Clone + Send + 'static,
        C: FnMut(serial::Statistics, PeerResult) -> Result<serial::Action, serial::Error> + 'a,
        F: FnOnce() -> P + Send + 'a,
        P: Future<Output = Result<Setup, ()>> + Send + 'a,
        S: FnOnce(&Catalog) -> Result<(Select, fr_media::worker::Configuration), ()> + Send + 'a,
        L: FnMut(&mut SessionAgent, &mut Context<'_>) -> Result<LocalAction, ()> + Send + 'a,
    {
        let incoming = driver.incoming();
        let fence = Fence {
            incoming: incoming.clone(),
            supervisor: supervisor.clone(),
        };
        let desktop = driver.serve_async(factory, select, local);
        let completion = Arc::new(Completion::default());
        let mut application = Application {
            incoming,
            completion: completion.clone(),
            callbacks: connections,
        };
        // Own the unused listener even before serial acceptance is polled.
        // Dropping this async body unpolled must close its protected socket too.
        let network_owner = NetworkOwner(self);
        let listener = async move {
            let network_owner = network_owner;
            network_owner
                .0
                .serve_serial(supervisor, runtime, policy, &mut application)
                .await
        };
        Run {
            fence,
            completion,
            desktop: Some(Box::pin(desktop)),
            listener: Some(Box::pin(listener)),
            result: None,
            ending: None,
            finishing: None,
        }
    }
}

struct NetworkOwner<'a>(&'a mut LinuxServer);
impl Drop for NetworkOwner<'_> {
    fn drop(&mut self) {
        self.0.close();
    }
}

struct Fence {
    incoming: dispatch::Incoming,
    supervisor: Cx,
}
impl Fence {
    fn stop(&self) {
        // Source/cohort authority ends before cancellation releases native work.
        self.incoming.close();
        self.supervisor.cancel_fast(CancelKind::User);
    }
}
impl Drop for Fence {
    fn drop(&mut self) {
        self.stop();
    }
}
struct Run<D, N> {
    fence: Fence,
    completion: Arc<Completion>,
    desktop: Option<Pin<Box<D>>>,
    listener: Option<Pin<Box<N>>>,
    result: Option<End>,
    ending: Option<(End, DrainBudget)>,
    /// The desktop ended while a peer was admitted: its listener is finishing
    /// that peer (closure report, `completed`, transport retirement).
    finishing: Option<(Result<desktop::Report, dispatch::Error>, DrainBudget)>,
}
impl<D, N> Run<D, N> {
    fn stop(&mut self) {
        self.completion.ending.store(true, Ordering::Release);
        self.fence.stop();
        self.ending = None;
        self.finishing = None;
        self.result.get_or_insert(End::Cancelled);
        // Release the desktop's hub-owned transport before listener cleanup.
        drop(self.desktop.take());
        drop(self.listener.take());
    }
}
impl<D, N> Future for Run<D, N>
where
    D: Future<Output = Result<desktop::Report, dispatch::Error>>,
    N: Future<Output = Result<serial::Statistics, LinuxError>>,
{
    type Output = End;
    fn poll(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<End> {
        let mut turn = Turn {
            owner: self.get_mut(),
            complete: false,
        };
        let this = &mut *turn.owner;
        let result = if let Some(result) = &this.result {
            Poll::Ready(result.clone())
        } else if this.ending.is_some() {
            this.poll_drain(task)
        } else if this.finishing.is_some() {
            this.poll_finishing(task)
        } else if this.fence.supervisor.is_cancel_requested() {
            match this.begin_drain(End::Cancelled) {
                Ok(()) => this.poll_drain(task),
                Err(error) => Poll::Ready(End::Desktop(Err(error))),
            }
        } else {
            match this
                .desktop
                .as_mut()
                .expect("active desktop")
                .as_mut()
                .poll(task)
            {
                Poll::Ready(result) => {
                    // The original Driver has already fenced the source/peer.
                    // Never poll idle acceptance, renew a deadline, or allow
                    // completed(Continue) to open another connection into a
                    // terminal desktop: `ending` refuses new requests and turns
                    // completion into Stop. An ADMITTED peer still finishes its
                    // one-record closure report and authentic completion, which
                    // may yield on real timers; dropping it after one poll lost
                    // both. Source authority ends first (as in `Fence::stop`),
                    // then the desktop's hub-owned transport is released so the
                    // listener's retirement wait can observe its destruction.
                    // The supervisor stays live until the peer finishes.
                    this.completion.ending.store(true, Ordering::Release);
                    this.fence.incoming.close();
                    drop(this.desktop.take());
                    if this.completion.active.load(Ordering::Acquire) {
                        match DrainBudget::new(&this.fence.supervisor) {
                            Ok(budget) => {
                                this.finishing = Some((result, budget));
                                this.poll_finishing(task)
                            }
                            Err(()) => Poll::Ready(End::Desktop(Err(dispatch::Error::Clock))),
                        }
                    } else {
                        Poll::Ready(End::Desktop(result))
                    }
                }
                Poll::Pending => {
                    let polled = this
                        .listener
                        .as_mut()
                        .expect("active listener")
                        .as_mut()
                        .poll(task);
                    match polled {
                        Poll::Ready(result) => {
                            // Completed: never poll it again, even while draining.
                            this.listener = None;
                            match this.begin_drain(End::Listener(result)) {
                                Ok(()) => this.poll_drain(task),
                                Err(error) => Poll::Ready(End::Desktop(Err(error))),
                            }
                        }
                        Poll::Pending => Poll::Pending,
                    }
                }
            }
        };
        if let Poll::Ready(value) = &result {
            this.result = Some(value.clone());
            this.stop();
        }
        turn.complete = true;
        result
    }
}
impl<D, N> Run<D, N>
where
    D: Future<Output = Result<desktop::Report, dispatch::Error>>,
{
    fn begin_drain(&mut self, end: End) -> Result<(), dispatch::Error> {
        self.completion.ending.store(true, Ordering::Release);
        self.fence.stop();
        let budget =
            DrainBudget::new(&self.fence.supervisor).map_err(|()| dispatch::Error::Clock)?;
        self.ending = Some((end, budget));
        Ok(())
    }
    /// Bounded by the same fixed drain budget. Past it, or on a requested stop,
    /// the peer is abandoned as before (no completion is fabricated) and its
    /// listener is never repolled. Either way the desktop's own result stands:
    /// a stop must not turn its uncertain input cleanup into `Cancelled`.
    fn poll_finishing(&mut self, task: &mut Context<'_>) -> Poll<End>
    where
        N: Future<Output = Result<serial::Statistics, LinuxError>>,
    {
        let (result, budget) = self.finishing.as_mut().expect("finishing peer");
        if self.fence.supervisor.is_cancel_requested() {
            return Poll::Ready(End::Desktop(result.clone()));
        }
        match budget.expired(task) {
            Ok(true) => return Poll::Ready(End::Desktop(result.clone())),
            Err(()) => return Poll::Ready(End::Desktop(Err(dispatch::Error::Clock))),
            Ok(false) => {}
        }
        let Poll::Ready(listener) = self
            .listener
            .as_mut()
            .expect("active listener")
            .as_mut()
            .poll(task)
        else {
            return Poll::Pending;
        };
        Poll::Ready(match listener {
            // The peer cancellation is induced by source teardown;
            // independent identity/ingress/retirement errors are not.
            Err(error)
                if !matches!(
                    error,
                    LinuxError::Serial(serial::Error::Host(super::Error::Cancelled))
                ) =>
            {
                End::Listener(Err(error))
            }
            _ => End::Desktop(result.clone()),
        })
    }
    fn poll_drain(&mut self, task: &mut Context<'_>) -> Poll<End>
    where
        N: Future<Output = Result<serial::Statistics, LinuxError>>,
    {
        let (end, budget) = self.ending.as_mut().expect("fixed drain budget");
        match budget.expired(task) {
            Ok(true) => return Poll::Ready(End::DrainExpired),
            Err(()) => return Poll::Ready(End::Desktop(Err(dispatch::Error::Clock))),
            Ok(false) => {}
        }
        let end = end.clone();
        if let Some(desktop) = &mut self.desktop {
            let Poll::Ready(result) = desktop.as_mut().poll(task) else {
                return Poll::Pending;
            };
            // A requested stop must not conceal the canonical native owner's
            // explicit uncertain-cleanup result. Other errors retain the first
            // listener/cancellation outcome; no second session was attempted.
            if let Err(error @ dispatch::Error::Desktop(_)) = result
                && matches!(&error, dispatch::Error::Desktop(cause)
                    if **cause == desktop::Error::InputCleanup)
            {
                return Poll::Ready(End::Desktop(Err(error)));
            }
            self.desktop = None;
        }
        // Then, as when the desktop ends first (`poll_finishing`), an admitted
        // peer finishes on its way out within the same budget: its closure
        // report tells the viewer the host is stopping. Dropping it with the
        // listener left the viewer to infer silence (`host_not_heard`). The
        // listener admits no new peer: requests fail once `ending` is set and
        // its own checks fail with the supervisor.
        if self.completion.active.load(Ordering::Acquire)
            && let Some(listener) = &mut self.listener
        {
            if listener.as_mut().poll(task).is_pending() {
                return Poll::Pending;
            }
            self.listener = None;
        }
        Poll::Ready(end)
    }
}
struct Turn<'a, D, N> {
    owner: &'a mut Run<D, N>,
    complete: bool,
}
impl<D, N> Drop for Turn<'_, D, N> {
    fn drop(&mut self) {
        if !self.complete {
            self.owner.stop();
        }
    }
}
impl<D, N> Drop for Run<D, N> {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests;
