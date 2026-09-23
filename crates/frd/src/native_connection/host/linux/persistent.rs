//! Keep one enforced ingress boundary across all capacity-one peer attempts.
use super::{Error, LinuxServer, Serving};
use crate::native_connection::host::{PanicFence, serial};
use asupersync::{cx::Cx, runtime::RuntimeHandle, types::CancelKind};
use std::{future::Future, sync::Arc};

impl LinuxServer {
    /// Serve successive peers without handing a raw listener or an asserted
    /// ingress predicate to the application. The boundary installed by
    /// `Server::bind_linux` is renewed throughout acceptance, application service,
    /// original-transport retirement, cooldown and the next bind.
    ///
    /// This is the canonical serial acceptor: capacity one, not simultaneous
    /// multi-client routing. Each peer gets new IDs and an independent context;
    /// local consent and any enabled live-policy monitor still govern admission.
    /// The application must keep its native desktop `Incoming`/`HostService` alive
    /// for the whole peer lifetime. The OS-source owner remains independent.
    ///
    /// Supply a dedicated supervisor distinct from the broker/credential/source
    /// contexts, with `RuntimeHandle` from the same runtime. No task is spawned.
    /// Constructing this operation consumes the listener; admission begins on its
    /// first poll. Dropping it unpolled is terminal, not permission to reuse the
    /// socket. Loss of protection fences the current peer before pending work is
    /// released, even when the failed future is retained by the caller.
    ///
    /// Returning a result does not remove firewall protection. Retain this owner
    /// and call `stop` with an independent cleanup context. Removal still refuses
    /// while any handed-off transport owns its original ingress lease.
    pub fn serve_serial<'a, A: serial::Application + 'a>(
        &'a mut self,
        supervisor: Cx,
        runtime: RuntimeHandle,
        policy: serial::Policy,
        application: &'a mut A,
    ) -> impl Future<Output = Result<serial::Statistics, Error>> + 'a {
        let mut fence = PanicFence(&supervisor, false);
        let initial = self
            .listener
            .take()
            .ok_or(Error::Spent)
            .and_then(|listener| {
                let lease = self.boundary.lease().map_err(Error::Ingress)?;
                Ok(self.server.serve_serial_on_protected_listener(
                    supervisor.clone(),
                    runtime,
                    listener,
                    policy,
                    Arc::new(move |address| lease.check(address).is_ok()),
                    application,
                ))
            });
        let refusal = initial.as_ref().err().copied();
        if refusal.is_some() {
            self.boundary.close();
            supervisor.cancel_fast(CancelKind::User);
        }
        // Supervise the WHOLE serial service, not each connection. A normal
        // peer's retirement must not retire the next peer's ingress lifetime.
        let operation = self
            .boundary
            .supervise(async move { initial?.await.map_err(Error::Serial) });
        let inner = Box::pin(async move {
            if let Some(error) = refusal {
                drop(operation);
                return Err(error);
            }
            operation.await.map_err(Error::Ingress)?
        });
        fence.1 = true;
        drop(fence);
        Serving {
            session: supervisor,
            inner: Some(inner),
        }
    }
}
