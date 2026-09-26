//! Cold native TLS acceptance joined to installed-tailnet admission and Host.
//! This consumes one ALREADY protected listener, not a multi-client socket loop.
//! The kernel/interface restriction must be established by the local broker;
//! neither source prefixes nor the callback below establish that restriction.
mod shared;
pub use crate::session_startup::closure_reporting::ObservationClosure;
pub use shared::SharedObserver;

use crate::{
    host_policy::{self, live},
    session_startup::{Configuration, Host},
};
use asupersync::{cx::Cx, net::quic_core::ConnectionId, types::CancelKind};
use fr_tailnet::{
    Admission, ConnectionAddresses, GrantPolicy, LocalApi, NativeServerIdentity, NodeIdentity,
};
use fr_transport::native_accept::{self, Listener};
use std::{
    fmt,
    future::Future,
    net::SocketAddr,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};

/// Content-free failures. Certificate bytes, private names, addresses and local
/// metadata never appear in errors or diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    IngressUnavailable,
    Policy(live::Error),
    Cancelled,
    Clock,
    Tailnet(fr_tailnet::Error),
    Accept(native_accept::Error),
    Session(crate::session_startup::Error),
    Shared(crate::session_startup::shared_viewers::Error),
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "native-host: {self:?}")
    }
}
impl std::error::Error for Error {}

/// Local generation allocation and admission policy; never supplied by a peer.
/// Membership is the only profile here. Capability-policy fallback is not used.
pub struct Request {
    pub connection_id: ConnectionId,
    pub admission: GrantPolicy,
    pub session: Configuration,
}

/// Check the continuing lifetime of the independently enforced ingress boundary
/// on THIS socket. It must be local, nonblocking and fail closed. Returning true
/// is not a way to qualify an unprotected listener; no default check is supplied.
pub type IngressCheck = Arc<dyn Fn(SocketAddr) -> bool + Send + Sync>;
type Check = Arc<dyn Fn() -> Result<(), Error> + Send + Sync>;

/// One pending/active native application per owner. Provision credentials through
/// `LocalApi::native_server_identity` with locally trusted roots BEFORE listening.
/// The broker retains and drives that identity's existing renewal service; this
/// owner neither provisions certificates for peer names nor spawns another task.
pub struct Server {
    api: LocalApi,
    identity: NativeServerIdentity,
    live_policy: Option<live::Handle>,
    // Set only by the locally enforced bind path, before accepting a peer.
    // It observes the original credential lifetime after a session is cancelled.
    credential_clock: Option<Cx>,
    last_observation_closure: Option<ObservationClosure>,
}
impl fmt::Debug for Server {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NativeHost([installed authority and local credentials])")
    }
}
#[derive(Default)]
struct Factory {
    node: Option<NodeIdentity>,
    failure: Option<fr_tailnet::Error>,
}
impl Server {
    pub fn new(api: LocalApi, identity: NativeServerIdentity) -> Self {
        Self {
            api,
            identity,
            live_policy: None,
            credential_clock: None,
            last_observation_closure: None,
        }
    }

    /// Last completed observation report attempt, separately from the original
    /// application's result. None includes no observation grant or abandonment.
    pub const fn observation_closure(&self) -> Option<ObservationClosure> {
        self.last_observation_closure
    }

    /// Opt in to the original local file monitor. Its observed policy overrides
    /// each Request's sharing scope and approval flag; other request budgets and
    /// identities stay unchanged. The caller retains Watch and observes its disk
    /// cleanup independently. No fallback to request flags is allowed if policy
    /// evidence is opening, stale, malformed, rolled back, stopped or dropped.
    ///
    /// Each acceptance snapshots one epoch at CALL time. Any observed newer
    /// revision ends pending and active connections; only a NEW acceptance can
    /// use new values. This does not approve a viewer, change tailnet grants or
    /// replace the independent credentials/OS-source owners. Existing callers
    /// without this explicit opt-in retain their startup-selected policy.
    #[must_use]
    pub fn with_live_policy(mut self, policy: live::Handle) -> Self {
        self.live_policy = Some(policy);
        self
    }

    /// Accept a cold native client, perform fresh exact-endpoint membership
    /// checks AFTER TLS, and hand the original transport/admission to Host.
    /// Application startup/consent, media readiness and control remain separate.
    /// Nothing reaches the application callback before all identity checks pass.
    ///
    /// Supply a dedicated session Cx, NEVER the broker/credential-service Cx.
    /// Every exit or abandonment fences it before dropping pending work. A
    /// retained transport check survives `Host` -> `HostSession` -> publishing and
    /// control handoffs; stopping credentials or ingress fences application I/O.
    /// Even a parked application is checked on a bounded 10ms timer pulse.
    ///
    /// One socket is consumed. The native acquisition deadline starts at CALL
    /// time. Fresh host metadata is obtained only after an Initial; TLS and local
    /// lookup share the native handshake budget. Its three-second snapshot must
    /// still be revalidatable after TLS: a slow handshake is refused, not granted
    /// a replacement identity. This is not enabling the unfinished frd CLI loop.
    #[allow(clippy::too_many_lines)]
    pub fn run_on_protected_listener<'a, T: 'a, F, A>(
        &'a mut self,
        cx: &'a Cx,
        listener: Listener,
        request: Request,
        ingress: IngressCheck,
        application: A,
    ) -> impl Future<Output = Result<T, Error>> + 'a
    where
        F: Future<Output = T> + 'a,
        A: FnOnce(Host) -> F + 'a,
    {
        self.run_on_protected_listener_gated(
            cx,
            listener,
            request,
            ingress.clone(),
            ingress,
            application,
        )
    }

    // Separate ordinary cancellation from the independently enforced transport
    // boundary at construction, never by swapping checks after revocation.
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn run_on_protected_listener_gated<'a, T: 'a, F, A>(
        &'a mut self,
        cx: &'a Cx,
        listener: Listener,
        mut request: Request,
        ingress: IngressCheck,
        terminal_ingress: IngressCheck,
        application: A,
    ) -> impl Future<Output = Result<T, Error>> + 'a
    where
        F: Future<Output = T> + 'a,
        A: FnOnce(Host) -> F + 'a,
    {
        let mut creation_fence = PanicFence(cx, false);
        self.last_observation_closure = None;
        // Only the already-provisioned independent broker clock can outlive
        // session cancellation. A raw listener without that clock stays silent.
        let (reporting, registration) = self
            .credential_clock
            .as_ref()
            .map(crate::session_startup::closure_reporting::Reporting::new)
            .unzip();
        let run_api = self.api.clone();
        let address = listener.local_addr();
        let identity = self.identity.clone();
        let clock = cx.clone();
        // Snapshot policy before returning the acceptance future. A delayed
        // first poll must not acquire a replacement epoch after local revocation.
        let policy = self
            .live_policy
            .as_ref()
            .map(live::Handle::lease)
            .transpose()
            .map_err(Error::Policy)
            .and_then(|lease| {
                if let Some(lease) = &lease {
                    let value = lease.check().map_err(Error::Policy)?;
                    request.admission.scope = match value.sharing_scope {
                        host_policy::Sharing::OwnUser => fr_tailnet::Scope::OwnUser,
                        host_policy::Sharing::Tailnet => fr_tailnet::Scope::Tailnet,
                    };
                    request.session.require_approval =
                        value.approval_mode == host_policy::Approval::Local;
                }
                Ok(lease)
            });
        let terminal_clock = self.credential_clock.clone().unwrap_or_else(|| cx.clone());
        let terminal_identity = identity.clone();
        let terminal_policy = policy.clone();
        let terminal_check: Check = Arc::new(move || {
            // Policy and credentials are the ORIGINAL owners/epoch. Their
            // revocation, expiry or broker cancellation still refuses the report.
            if let Some(lease) = terminal_policy.as_ref().map_err(|error| *error)? {
                lease.check().map_err(Error::Policy)?;
            }
            if !terminal_ingress(address) {
                return Err(Error::IngressUnavailable);
            }
            terminal_identity
                .status(&terminal_clock)
                .map_err(Error::Tailnet)?;
            Ok(())
        });
        let check: Check = Arc::new(move || {
            clock.checkpoint().map_err(|_| Error::Cancelled)?;
            if let Some(lease) = policy.as_ref().map_err(|error| *error)? {
                lease.check().map_err(Error::Policy)?;
            }
            if !ingress(address) {
                return Err(Error::IngressUnavailable);
            }
            identity.status(&clock).map_err(Error::Tailnet)?;
            Ok(())
        });
        let state = Arc::new(Mutex::new(Factory::default()));
        let factory = state.clone();
        let api = self.api.clone();
        let identity = self.identity.clone();
        let initial = request
            .session
            .check_incoming()
            .map_err(Error::Session)
            .and_then(|()| check())
            .and_then(|()| {
                listener
                    .accept_with_identity(
                        cx,
                        request.connection_id,
                        move |_, parameters| async move {
                            let result = async {
                                let node = api.node_identity(cx).await?;
                                if !node.addresses().contains(&address.ip()) {
                                    return Err(fr_tailnet::Error::AddressMismatch);
                                }
                                let driver = identity.quic_handshake(cx, &node, parameters)?;
                                factory.lock().map_err(|_| fr_tailnet::Error::Revoked)?.node =
                                    Some(node);
                                Ok(driver)
                            }
                            .await;
                            result.map_err(|error| {
                                if let Ok(mut state) = factory.lock() {
                                    state.failure = Some(error);
                                }
                                native_accept::Error::IdentityUnavailable
                            })
                        },
                    )
                    .map_err(Error::Accept)
            });
        let inside = check.clone();
        let operation = Scoped {
            cx: cx.clone(),
            check,
            pulse: Box::pin(asupersync::time::sleep(cx.now(), Duration::from_millis(10))),
            inner: Box::pin(async move {
                let native = initial?.await.map_err(|error| {
                    state
                        .lock()
                        .ok()
                        .and_then(|state| state.failure)
                        .map_or(Error::Accept(error), Error::Tailnet)
                })?;
                inside()?;
                let original = state
                    .lock()
                    .map_err(|_| Error::Tailnet(fr_tailnet::Error::Revoked))?
                    .node
                    .take()
                    .ok_or(Error::Tailnet(fr_tailnet::Error::IdentityMismatch))?;
                let current = run_api
                    .revalidate_node(cx, &original)
                    .await
                    .map_err(Error::Tailnet)?;
                let addresses = ConnectionAddresses {
                    local: native.local_addr(),
                    peer: native.peer_addr(),
                };
                if addresses.local != address || !current.addresses().contains(&address.ip()) {
                    return Err(Error::Tailnet(fr_tailnet::Error::AddressMismatch));
                }
                // No source-prefix, name-based, cached or capability fallback.
                let proof = run_api
                    .authorize_membership(cx, addresses, request.admission)
                    .await
                    .map_err(Error::Tailnet)?;
                run_api
                    .revalidate_node(cx, &current)
                    .await
                    .map_err(Error::Tailnet)?;
                inside()?;
                let admission =
                    Admission::new(run_api.clone(), cx.clone(), proof).map_err(Error::Tailnet)?;
                let mut host = Host::from_admitted(native, admission, request.session)
                    .map_err(Error::Session)?;
                let gate = inside.clone();
                let stop = cx.clone();
                host.retain_connection_checks(
                    Arc::new(move || {
                        if gate().is_err() {
                            stop.cancel_fast(CancelKind::User);
                            return false;
                        }
                        true
                    }),
                    Arc::new(move || terminal_check().is_ok()),
                )
                .map_err(Error::Session)?;
                if let Some(registration) = registration {
                    host.retain_observation_reporting(registration)
                        .map_err(Error::Session)?;
                }
                inside()?;
                Ok(application(host).await)
            }),
        };
        creation_fence.1 = true;
        async move {
            // Drop the ordinary Scoped owner before finishing. Its cancellation
            // and the original session Drop/fence remain terminal. No callback
            // or application traffic is serviced by the one-record drain.
            let result = operation.await;
            if let Some(reporting) = reporting {
                let reason = match &result {
                    Ok(_) | Err(Error::Cancelled) => fr_wire::closure::ClosedReason::HostStopping,
                    Err(Error::IngressUnavailable | Error::Policy(_) | Error::Tailnet(_)) => {
                        fr_wire::closure::ClosedReason::PermissionLost
                    }
                    Err(_) => fr_wire::closure::ClosedReason::HostFailure,
                };
                self.last_observation_closure = Box::pin(reporting.finish(reason)).await;
            }
            result
        }
    }
}

// Guard outside the async body: unpolled abandonment and callback unwind fence
// the session before dropping Host, lookup/socket or caller-owned native work.
struct Scoped<F> {
    cx: Cx,
    check: Check,
    inner: Pin<Box<F>>,
    pulse: Pin<Box<asupersync::time::Sleep>>,
}
impl<T, F: Future<Output = Result<T, Error>>> Future for Scoped<F> {
    type Output = Result<T, Error>;
    fn poll(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let mut panic_fence = PanicFence(&this.cx, false);
        if this.pulse.as_mut().poll(task).is_ready() {
            let Some(clock) = this.cx.timer_driver() else {
                this.cx.cancel_fast(CancelKind::User);
                return Poll::Ready(Err(Error::Clock));
            };
            this.pulse = Box::pin(asupersync::time::sleep(
                clock.now(),
                Duration::from_millis(10),
            ));
            let _ = this.pulse.as_mut().poll(task);
        }
        let result = match (this.check)() {
            Err(error) => Poll::Ready(Err(error)),
            Ok(()) => this.inner.as_mut().poll(task),
        };
        // Pending work cannot retain a newly revoked lifetime. A completed
        // application may already have closed its HostSession normally; preserve
        // its result rather than misreporting that expected cleanup as failure.
        let result = if result.is_pending() {
            match (this.check)() {
                Err(error) => Poll::Ready(Err(error)),
                Ok(()) => result,
            }
        } else {
            result
        };
        if result.is_ready() {
            this.cx.cancel_fast(CancelKind::User);
        }
        panic_fence.1 = true;
        result
    }
}
struct PanicFence<'a>(&'a Cx, bool);
impl Drop for PanicFence<'_> {
    fn drop(&mut self) {
        if !self.1 {
            self.0.cancel_fast(CancelKind::User);
        }
    }
}
impl<F> Drop for Scoped<F> {
    fn drop(&mut self) {
        self.cx.cancel_fast(CancelKind::User);
    }
}

mod linux;
pub use linux::{Error as LinuxError, LinuxServer};

pub mod serial;

// Persistent protected acceptance through the original native desktop driver.
pub mod desktop;
