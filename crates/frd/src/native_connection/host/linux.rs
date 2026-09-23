//! One protected Linux socket through the ORIGINAL native host/session service.
//! No raw socket or caller-supplied protection assertion leaves this owner.
use super::{Host, Request, Server};
use asupersync::{cx::Cx, types::CancelKind};
use fr_tailnet::ingress;
use fr_transport::native_accept::{self, Listener};
use std::{
    fmt,
    future::Future,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Ingress(ingress::Error),
    Host(super::Error),
    Serial(super::serial::Error),
    Spent,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "protected-native-host: {self:?}")
    }
}
impl std::error::Error for Error {}

/// An enforced listener and its separately observed firewall cleanup owner.
/// The broker/credential context must be distinct from the session context used
/// by `run` or `serve_serial`. Rebinding occurs only inside explicit serial service.
/// No scope widening or effect retry occurs. Retain the owner until stop completes.
pub struct LinuxServer {
    server: Server,
    boundary: ingress::Boundary,
    listener: Option<Listener>,
}
impl fmt::Debug for LinuxServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LinuxNativeHost([protected original socket])")
    }
}
impl Server {
    /// Opt-in privileged Linux kernel-TUN profile. Verify installed credentials
    /// and local address ownership, install/read back a drop-only rule, THEN bind
    /// the canonical native listener. The original Server is consumed so another
    /// identity adapter cannot be substituted between binding and admission.
    ///
    /// Failed or abandoned setup never removes a possibly installed restriction;
    /// it may leave a restrictive frd_* table. It never leaves a usable socket.
    /// Start run promptly: an unsupervised node snapshot expires rather than
    /// silently extending permission across idle time. run services renewal.
    pub async fn bind_linux(
        self,
        broker: &Cx,
        configuration: ingress::Configuration,
        transport: native_accept::Configuration,
    ) -> Result<LinuxServer, Error> {
        self.identity
            .status(broker)
            .map_err(|e| Error::Host(super::Error::Tailnet(e)))?;
        let node = self
            .api
            .node_identity(broker)
            .await
            .map_err(|e| Error::Host(super::Error::Tailnet(e)))?;
        let boundary = ingress::Boundary::install(broker, self.api.clone(), node, configuration)
            .await
            .map_err(Error::Ingress)?;
        let lease = boundary.lease().map_err(Error::Ingress)?;
        let listener = Listener::bind(broker, boundary.address(), transport)
            .await
            .map_err(|e| Error::Host(super::Error::Accept(e)))?;
        lease.check(listener.local_addr()).map_err(Error::Ingress)?;
        self.identity
            .status(broker)
            .map_err(|e| Error::Host(super::Error::Tailnet(e)))?;
        Ok(LinuxServer {
            server: self,
            boundary,
            listener: Some(listener),
        })
    }
}
impl LinuxServer {
    pub fn address(&self) -> SocketAddr {
        self.boundary.address()
    }
    /// Local operational identifier, never a peer-selected firewall target.
    pub fn cleanup_table(&self) -> &str {
        self.boundary.cleanup_table()
    }
    /// Fence before dropping an unused socket. An active service borrows this
    /// owner; cancel its dedicated session Cx to stop it independently.
    pub fn close(&mut self) {
        self.boundary.close();
        drop(self.listener.take());
    }
    /// The socket is consumed at CALL time and the existing acceptance deadline
    /// starts there. Startup/consent and input grants still belong to Host. Keep
    /// the callback alive for the entire admitted session (e.g. `serve_host`), not
    /// just until it enqueues a hub ticket. No second input grant is fabricated.
    ///
    /// The actual ingress lease is retained by the ORIGINAL transport's I/O
    /// guard across Host/HostSession/hub handoffs. Protection renewal runs beside
    /// the same application future. Any exit, expiry, panic or unpolled drop
    /// fences the session; the independent broker/credential context is untouched.
    pub fn run<'a, T: 'a, F, A>(
        &'a mut self,
        session: &'a Cx,
        request: Request,
        application: A,
    ) -> impl Future<Output = Result<T, Error>> + 'a
    where
        F: Future<Output = T> + 'a,
        A: FnOnce(Host) -> F + 'a,
    {
        let mut creation_fence = super::PanicFence(session, false);
        let listener = self.listener.take().ok_or(Error::Spent);
        let initial = listener.and_then(|listener| {
            let lease = self.boundary.lease().map_err(Error::Ingress)?;
            Ok(self.server.run_on_protected_listener(
                session,
                listener,
                request,
                Arc::new(move |address| lease.check(address).is_ok()),
                application,
            ))
        });
        let refusal = initial.as_ref().err().copied();
        if refusal.is_some() {
            self.boundary.close();
            session.cancel_fast(CancelKind::User);
        }
        let operation = self
            .boundary
            .supervise(async move { initial?.await.map_err(Error::Host) });
        let serving = Serving {
            session: session.clone(),
            inner: Box::pin(async move {
                if let Some(error) = refusal {
                    drop(operation);
                    return Err(error);
                }
                operation.await.map_err(Error::Ingress)?
            }),
        };
        creation_fence.1 = true;
        serving
    }
    /// Fence at CALL time. Refuse cleanup while a handed-off transport still
    /// retains its ingress lease, even after its session was cancelled. A failed
    /// cleanup is retryable on this owner; Drop never silently deletes the rule.
    pub fn stop<'a>(
        &'a mut self,
        cleanup: &'a Cx,
    ) -> impl Future<Output = Result<(), ingress::Error>> + 'a {
        self.close();
        self.boundary.stop(cleanup)
    }
}
impl Drop for LinuxServer {
    fn drop(&mut self) {
        self.close();
    }
}
// An ingress failure may finish before the nested native future is dropped.
// Fence immediately even when the caller retains the completed/failed future.
struct Serving<F> {
    session: Cx,
    inner: Pin<Box<F>>,
}
impl<T, F: Future<Output = Result<T, Error>>> Future for Serving<F> {
    type Output = Result<T, Error>;
    fn poll(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let mut panic_fence = super::PanicFence(&this.session, false);
        let result = this.inner.as_mut().poll(task);
        if result.is_ready() {
            this.session.cancel_fast(CancelKind::User);
        }
        panic_fence.1 = true;
        result
    }
}
impl<F> Drop for Serving<F> {
    fn drop(&mut self) {
        self.session.cancel_fast(CancelKind::User);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::{runtime::RuntimeBuilder, types::Budget};
    use std::future::poll_fn;

    fn run(test: impl FnOnce(Cx, Cx)) {
        let rt = RuntimeBuilder::current_thread()
            .enable_platform_reactor(true)
            .build()
            .unwrap();
        let broker = rt
            .handle()
            .try_request_cx_with_budget(Budget::INFINITE)
            .unwrap();
        let session = rt
            .handle()
            .try_request_cx_with_budget(Budget::INFINITE)
            .unwrap();
        rt.block_on(async {
            test(broker, session);
        });
    }
    #[test]
    fn unpolled_protected_service_drop_fences_only_its_session() {
        run(|broker, session| {
            drop(Serving {
                session: session.clone(),
                inner: Box::pin(std::future::pending::<Result<(), Error>>()),
            });
            assert!(session.is_cancel_requested());
            assert!(!broker.is_cancel_requested());
        });
    }
    #[test]
    fn ingress_failure_fences_while_completed_future_is_retained() {
        run(|broker, session| {
            let failure = Error::Ingress(ingress::Error::FirewallMismatch);
            let mut serving = Box::pin(Serving {
                session: session.clone(),
                inner: Box::pin(std::future::ready(Err::<(), _>(failure))),
            });
            let wake = std::task::Waker::noop();
            assert_eq!(
                serving.as_mut().poll(&mut Context::from_waker(wake)),
                Poll::Ready(Err(failure))
            );
            assert!(session.is_cancel_requested());
            assert!(!broker.is_cancel_requested());
            drop(serving);
        });
    }
    #[test]
    fn completion_keeps_the_actual_application_result_and_fences() {
        run(|broker, session| {
            let mut serving = Box::pin(Serving {
                session: session.clone(),
                inner: Box::pin(std::future::ready(Ok(17))),
            });
            assert_eq!(
                serving
                    .as_mut()
                    .poll(&mut Context::from_waker(std::task::Waker::noop())),
                Poll::Ready(Ok(17))
            );
            assert!(session.is_cancel_requested());
            assert!(!broker.is_cancel_requested());
        });
    }
    #[test]
    fn caught_protected_application_panic_does_not_retain_authority() {
        run(|broker, session| {
            let mut serving = Box::pin(Serving {
                session: session.clone(),
                inner: Box::pin(poll_fn(|_| -> Poll<Result<(), Error>> {
                    panic!("application fixture")
                })),
            });
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    serving
                        .as_mut()
                        .poll(&mut Context::from_waker(std::task::Waker::noop()))
                }))
                .is_err()
            );
            assert!(session.is_cancel_requested());
            assert!(!broker.is_cancel_requested());
            drop(serving);
        });
    }
}

mod persistent;
