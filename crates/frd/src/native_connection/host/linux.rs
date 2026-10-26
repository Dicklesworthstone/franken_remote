//! One bounded protected Linux bind set through the ORIGINAL host/session service.
//! No raw socket or caller-supplied protection assertion leaves this owner.
use super::{Host, Request, Server};
use asupersync::{cx::Cx, types::CancelKind};
use fr_tailnet::ingress;
use fr_transport::native_accept::{self, Listener};
use std::{fmt, future::Future, net::SocketAddr};

mod boundaries;
use boundaries::Boundaries;

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
    boundary: Boundaries,
    listener: Option<Listener>,
}
impl fmt::Debug for LinuxServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LinuxNativeHost([protected original sockets])")
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
        self.bind_linux_addresses(broker, vec![configuration], transport)
            .await
    }

    /// Bind all current exact node addresses as ONE serial listener. Each
    /// address has its own independently installed/read-back ingress boundary;
    /// no socket is opened until the complete set is protected. The first valid
    /// Initial chooses one endpoint for the existing TLS/admission path. The
    /// unused sockets close before TLS, and the next serial attempt binds the
    /// same protected set again. This does not admit concurrent peers.
    pub async fn bind_linux_addresses(
        mut self,
        broker: &Cx,
        configurations: Vec<ingress::Configuration>,
        transport: native_accept::Configuration,
    ) -> Result<LinuxServer, Error> {
        if configurations.is_empty()
            || configurations.len() > native_accept::MAX_ADDRESSES
            || configurations
                .iter()
                .enumerate()
                .any(|(index, configuration)| {
                    configurations[..index]
                        .iter()
                        .any(|old| old.address() == configuration.address())
                })
        {
            return Err(Error::Ingress(ingress::Error::InvalidConfiguration));
        }
        self.identity
            .status(broker)
            .map_err(|e| Error::Host(super::Error::Tailnet(e)))?;
        let addresses: Vec<_> = configurations
            .iter()
            .map(ingress::Configuration::address)
            .collect();
        let mut boundary = Boundaries(Vec::with_capacity(configurations.len()));
        let installed = async {
            let node = self
                .api
                .node_identity(broker)
                .await
                .map_err(|e| Error::Host(super::Error::Tailnet(e)))?;
            if !addresses
                .iter()
                .all(|address| node.addresses().contains(&address.ip()))
            {
                return Err(Error::Ingress(ingress::Error::InvalidConfiguration));
            }
            // Matching addresses alone cannot join snapshots from different
            // node keys, daemons or tailnets. Keep the first snapshot's original
            // validity while acquiring one non-cloneable, same-node snapshot
            // per sibling. No rule or socket opens before this set agrees.
            let mut siblings = Vec::with_capacity(configurations.len() - 1);
            for _ in 1..configurations.len() {
                siblings.push(
                    self.api
                        .revalidate_node(broker, &node)
                        .await
                        .map_err(|e| Error::Host(super::Error::Tailnet(e)))?,
                );
            }
            for (configuration, node) in configurations
                .into_iter()
                .zip(std::iter::once(node).chain(siblings))
            {
                boundary.0.push(
                    ingress::Boundary::install(broker, self.api.clone(), node, configuration)
                        .await
                        .map_err(Error::Ingress)?,
                );
            }
            let check = boundary.check().map_err(Error::Ingress)?;
            let listener = Listener::bind_addresses(broker, &addresses, transport)
                .await
                .map_err(|e| Error::Host(super::Error::Accept(e)))?;
            if !listener
                .local_addresses()
                .iter()
                .all(|&address| check(address))
            {
                return Err(Error::Ingress(ingress::Error::Closed));
            }
            self.identity
                .status(broker)
                .map_err(|e| Error::Host(super::Error::Tailnet(e)))?;
            Ok(listener)
        }
        .await;
        let listener = match installed {
            Ok(listener) => listener,
            Err(error) => {
                // There is no admitted transport. Retire every successfully
                // installed rule even when a later address/bind was refused.
                // Cancellation may leave restrictive residue as before. Report
                // failed cleanup instead of hiding it behind the setup error.
                boundary.stop(broker).await.map_err(Error::Ingress)?;
                return Err(error);
            }
        };
        // This context predates every listener/session cancellation and already
        // owns the enforced boundary and the credential checks. Retaining it is
        // not a new permission or an un-cancelled copy of a peer's context.
        self.credential_clock = Some(broker.clone());
        Ok(LinuxServer {
            server: self,
            boundary,
            listener: Some(listener),
        })
    }
}
impl LinuxServer {
    /// Last completed observation-terminal attempt on the original protected
    /// listener. This is transport delivery, never native cleanup confirmation.
    pub fn observation_closure(&self) -> Option<super::ObservationClosure> {
        self.server.observation_closure()
    }
    pub fn address(&self) -> SocketAddr {
        self.boundary.0[0].address()
    }
    /// The complete original protected bind set.
    pub fn addresses(&self) -> Vec<SocketAddr> {
        self.boundary.addresses()
    }
    /// Local operational identifier, never a peer-selected firewall target.
    pub fn cleanup_table(&self) -> &str {
        self.boundary.0[0].cleanup_table()
    }
    /// Every independently owned rule; useful for local cleanup diagnostics.
    pub fn cleanup_tables(&self) -> Vec<&str> {
        self.boundary
            .0
            .iter()
            .map(ingress::Boundary::cleanup_table)
            .collect()
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
            let check = self.boundary.check().map_err(Error::Ingress)?;
            Ok(self.server.run_on_protected_listener(
                session,
                listener,
                request,
                check,
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
            inner: Some(Box::pin(async move {
                if let Some(error) = refusal {
                    drop(operation);
                    return Err(error);
                }
                operation.await.map_err(Error::Ingress)?
            })),
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
mod service;
use service::Serving;

mod persistent;
