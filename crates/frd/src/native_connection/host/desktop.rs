//! One protected listener and its independently owned native desktop service.
//! The original TLS/LocalAPI/consent/media owners are composed, never replaced.
use super::{LinuxError, LinuxServer, Request, serial};
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
use fr_wire::{
    display::{Catalog, Select},
    negotiation::Role,
};
use std::{
    future::Future,
    pin::Pin,
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
/// It is not invoked with a fabricated result when the whole share is abandoned.
pub struct Connections<R, N, C> {
    pub request: R,
    pub approval: N,
    pub completed: C,
}

/// Which original service completed first. The other service is fenced and its
/// pending future eagerly dropped, NOT reported as successful or OS-cleaned-up.
/// Retain Driver and `LinuxServer` for explicit reap/stop after this result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum End {
    Desktop(Result<desktop::Report, dispatch::Error>),
    Listener(Result<serial::Statistics, LinuxError>),
    Cancelled,
}

struct Application<R, N, C> {
    incoming: dispatch::Incoming,
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
        (self.callbacks.request)(attempt)
    }
    fn serve(&mut self, host: Host) -> impl Future<Output = Self::Output> {
        // Reserve the original dispatcher slot synchronously, before returning.
        // No unbounded channel, independent permission, transport or task.
        let peer = self
            .incoming
            .serve_host(host, self.callbacks.approval.clone());
        async move { peer?.await }
    }
    fn completed(
        &mut self,
        stats: serial::Statistics,
        result: PeerResult,
    ) -> Result<serial::Action, serial::Error> {
        (self.callbacks.completed)(stats, result)
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
    /// Completion, cancellation, panic and even unpolled abandonment fence BOTH
    /// original services before eagerly dropping their work. Retain Driver for
    /// reap, the factory's `Launch::retain_cleanup` for abandoned preparation, and
    /// this `LinuxServer` for stop. A terminal future is not proof of child exit or
    /// permission to remove a rule while a transport still holds its ingress lease.
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
        let mut application = Application {
            incoming,
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
            desktop: Some(Box::pin(desktop)),
            listener: Some(Box::pin(listener)),
            result: None,
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
    desktop: Option<Pin<Box<D>>>,
    listener: Option<Pin<Box<N>>>,
    result: Option<End>,
}
impl<D, N> Run<D, N> {
    fn stop(&mut self) {
        self.fence.stop();
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
        } else if this.fence.supervisor.is_cancel_requested() {
            Poll::Ready(End::Cancelled)
        } else {
            match this
                .desktop
                .as_mut()
                .expect("active desktop")
                .as_mut()
                .poll(task)
            {
                Poll::Ready(result) => Poll::Ready(End::Desktop(result)),
                Poll::Pending => this
                    .listener
                    .as_mut()
                    .expect("active listener")
                    .as_mut()
                    .poll(task)
                    .map(End::Listener),
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
