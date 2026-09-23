//! One explicitly scoped Linux observation, from protected accept to capture.
//! The same source driver and connection retain separate authority and cleanup.
use super::{Driver, Error, Incoming, LocalAction, Report, Setup};
use crate::native_connection::host::{LinuxError, LinuxServer, Request};
use crate::session_startup::Approval;
use asupersync::{cx::Cx, types::CancelKind};
use fr_media::worker::Configuration;
use fr_wire::{
    display::{Catalog, Select},
    negotiation::Role,
};
use std::{
    future::{Future, poll_fn},
    pin::{Pin, pin},
    task::{Context, Poll},
};

/// Which original service ended first, not a statement about remote effects or
/// physical presentation. The other service is then deliberately cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishedFirst {
    Desktop,
    Connection,
}

/// Both original results, including the cancellation caused by the first ending.
/// A completed join is not proof of successful viewing: inspect BOTH results.
/// Native child, indicator and ingress cleanup remain separately observable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    pub first: FinishedFirst,
    pub desktop: Result<Report, Error>,
    pub connection: Result<Result<(), Error>, LinuxError>,
}

impl Driver {
    /// Run one observation through the already protected Linux listener AND its
    /// exact native desktop driver. The cold dispatch handle is derived here,
    /// never supplied by a caller who could accidentally pair a foreign source.
    /// Native TLS, membership, live policy, original approval and positive media
    /// negotiation all precede the source factory, just as in the existing paths.
    ///
    /// This is an explicit one-observation service, not a persistent daemon or
    /// simultaneous-client listener. Completion of EITHER service ends the other;
    /// no new source, connection, grant or retry is created. The independent
    /// broker/credential contexts are untouched. Supply a dedicated connection
    /// Cx, distinct from this driver's original source and cleanup contexts.
    ///
    /// Local source events are polled before network work. Keep callbacks bounded
    /// and nonblocking. Source and connection futures are retained across turns,
    /// with their own original clocks/deadlines; no healthy drive is restarted.
    /// Every exit, panic and even unpolled abandonment fences both scopes before
    /// releasing pending work. Retain this `Driver` and `LinuxServer` for reap/stop,
    /// and retain factory-created Retirement/native owners through failed startup.
    #[allow(clippy::too_many_arguments)]
    pub fn serve_on_linux<'a, F, P, S, L, N>(
        &'a mut self,
        listener: &'a mut LinuxServer,
        connection: &'a Cx,
        request: Request,
        factory: F,
        select: S,
        local: L,
        notify: N,
    ) -> impl Future<Output = Result<Completion, Error>> + 'a
    where
        F: FnOnce() -> P + Send + 'a,
        P: Future<Output = Result<Setup, ()>> + Send + 'a,
        S: FnOnce(&Catalog) -> Result<(Select, Configuration), ()> + Send + 'a,
        L: FnMut(&mut super::SessionAgent, &mut Context<'_>) -> Result<LocalAction, ()> + Send + 'a,
        N: FnMut(Approval, Role) -> Result<(), ()> + Send + 'static,
    {
        let incoming = Incoming(self.shared.clone());
        let routed = incoming.clone();
        let source = self.serve_async(factory, select, local);
        let peer = listener.run(connection, request, move |host| async move {
            routed.serve_host(host, notify)?.await
        });
        let stop = incoming.clone();
        Joined {
            incoming,
            connection: connection.clone(),
            work: Some(Box::pin(async move {
                let mut source = pin!(source);
                let mut peer = pin!(peer);
                let mut desktop = None;
                let mut network = None;
                let mut first = None;
                poll_fn(|task| {
                    if desktop.is_none()
                        && let Poll::Ready(result) = source.as_mut().poll(task)
                    {
                        desktop = Some(result);
                        first.get_or_insert(FinishedFirst::Desktop);
                        stop.close();
                        connection.cancel_fast(CancelKind::User);
                    }
                    if network.is_none()
                        && let Poll::Ready(result) = peer.as_mut().poll(task)
                    {
                        network = Some(result);
                        first.get_or_insert(FinishedFirst::Connection);
                        // Do not call the factory or keep a cohort alive after
                        // its only connection ended. Original cleanup is retained.
                        stop.close();
                        connection.cancel_fast(CancelKind::User);
                        task.waker().wake_by_ref();
                    }
                    if desktop.is_some() && network.is_some() {
                        return Poll::Ready(Ok(Completion {
                            first: first.expect("a completed service established the cause"),
                            desktop: desktop.take().expect("checked above"),
                            connection: network.take().expect("checked above"),
                        }));
                    }
                    Poll::Pending
                })
                .await
            })),
        }
    }
}

// Fence before dropping either future, including an unpolled one or a caught
// panic. The completed future cannot retain/re-poll a native operation afterward.
struct Joined<F> {
    incoming: Incoming,
    connection: Cx,
    work: Option<Pin<Box<F>>>,
}
impl<F> Joined<F> {
    fn finish(&mut self) {
        self.incoming.close();
        self.connection.cancel_fast(CancelKind::User);
        drop(self.work.take());
    }
}
impl<F: Future<Output = Result<Completion, Error>>> Future for Joined<F> {
    type Output = F::Output;
    fn poll(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<Self::Output> {
        let mut guard = Turn {
            joined: self.get_mut(),
            complete: false,
        };
        let result = match &mut guard.joined.work {
            Some(work) => work.as_mut().poll(task),
            None => Poll::Ready(Err(Error::Closed)),
        };
        if result.is_ready() {
            guard.joined.finish();
        }
        guard.complete = true;
        result
    }
}
struct Turn<'a, F> {
    joined: &'a mut Joined<F>,
    complete: bool,
}
impl<F> Drop for Turn<'_, F> {
    fn drop(&mut self) {
        if !self.complete {
            self.joined.finish();
        }
    }
}
impl<F> Drop for Joined<F> {
    fn drop(&mut self) {
        self.finish();
    }
}
