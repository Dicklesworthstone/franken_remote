//! Cold authenticated observers retain the native guard while a shared hub serves them.
use super::{Cx, Error, IngressCheck, Listener, Request, Server};
use crate::session_startup::{Approval, shared_viewers};
use fr_wire::negotiation::Role;
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};
type Outcome = Result<Result<(), shared_viewers::Error>, Error>;
type Receipt = Arc<Mutex<Option<shared_viewers::Ticket>>>;

impl Server {
    /// Join one cold native observer to an ALREADY running shared desktop.
    /// The original hub, capture publisher and local permission/lock service
    /// remain independently serviced by the OS-session owner. This does not
    /// create a second source, grant control or enable the unfinished CLI loop.
    ///
    /// Ingress must already be enforced on this exact socket. All canonical
    /// identity, TLS, post-TLS membership and continuing credential/ingress checks
    /// run before and throughout shared service. Negotiation and approval consume
    /// the original Host budget, including time parked in the bounded hub slot.
    /// `notify` reports local approval requests; success is NOT an approval.
    ///
    /// Keep this future for the ENTIRE observer lifetime, not just admission.
    /// Completion, abandonment and external cancellation fence this dedicated Cx
    /// and its original slot, never another viewer or the source/credential Cx.
    pub fn serve_shared_observer<'a, N>(
        &'a mut self,
        cx: &'a Cx,
        listener: Listener,
        request: Request,
        ingress: IngressCheck,
        admission: shared_viewers::Admission,
        notify: N,
    ) -> SharedObserver<'a>
    where
        N: FnMut(Approval, Role) -> Result<(), ()> + Send + 'static,
    {
        let receipt = Arc::new(Mutex::new(None));
        let handed = receipt.clone();
        // Construct at call time so parked/unpolled native attempts keep their
        // immutable acquisition deadline and existing abandonment fence.
        let operation = self.run_on_protected_listener(
            cx,
            listener,
            request,
            ingress,
            move |host| async move {
                let service = admission.serve_host(host, notify)?;
                *handed.lock().map_err(|_| shared_viewers::Error::Poisoned)? =
                    Some(service.ticket());
                service.await
            },
        );
        SharedObserver {
            operation: Some(Box::pin(operation)),
            receipt,
            result: None,
        }
    }
}

/// One guarded connection and its original hub receipt. A ticket becomes
/// available ONLY after TLS and installed-tailnet admission, not as a consent grant.
/// Its state distinguishes Opening/Starting/Serving from terminal completion;
/// neither ticket presence nor Serving means media readiness or input authority.
#[must_use = "dropping a shared observer cancels its original connection"]
pub struct SharedObserver<'a> {
    operation: Option<Pin<Box<dyn Future<Output = Outcome> + Send + 'a>>>,
    receipt: Receipt,
    result: Option<Result<(), Error>>,
}
impl SharedObserver<'_> {
    /// Local UI cancellation/status for the exact original observer, never a
    /// numeric lookup that could target a replacement occupying a reused slot.
    pub fn ticket(&self) -> Result<Option<shared_viewers::Ticket>, Error> {
        self.receipt
            .lock()
            .map(|receipt| receipt.clone())
            .map_err(|_| Error::Shared(shared_viewers::Error::Poisoned))
    }
    fn outcome(&self, result: Outcome) -> Result<(), Error> {
        // Inspect BEFORE dropping the completed operation. Otherwise its Drop
        // could turn external cancellation into a fabricated prior hub result.
        // Credential/ingress failures retain their own error; only expected Cx
        // teardown joins an already terminal receipt from this exact observer.
        if matches!(result, Err(Error::Cancelled))
            && let Some(shared_viewers::State::Finished(outcome)) =
                self.ticket()?.as_ref().map(shared_viewers::Ticket::state)
        {
            return outcome.map_err(Error::Shared);
        }
        result?.map_err(Error::Shared)
    }
}
impl Future for SharedObserver<'_> {
    type Output = Result<(), Error>;
    fn poll(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if let Some(result) = this.result {
            return Poll::Ready(result);
        }
        let Some(operation) = this.operation.as_mut() else {
            return Poll::Ready(Err(Error::Cancelled));
        };
        let Poll::Ready(result) = operation.as_mut().poll(task) else {
            return Poll::Pending;
        };
        let result = this.outcome(result);
        this.result = Some(result);
        // Eagerly release pending native work and fence the receipt even if the
        // caller retains this completed future. Scoped fences before inner Drop.
        this.operation = None;
        Poll::Ready(result)
    }
}
