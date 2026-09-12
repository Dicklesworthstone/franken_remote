//! Installed-tailnet connection kept under renewal through native viewer service.
//! Reuses fr-tailnet's canonical TLS dialer, not another connection implementation.
use crate::session_startup::Viewer;
use asupersync::{
    cx::Cx, net::quic_native::NativeQuicUdpConnection, tls::Certificate, types::CancelKind,
};
use fr_tailnet::{DialRoute, LocalApi, NativeClient, PeerSelector};
use fr_transport::quic::Policy;
use fr_wire::negotiation::Offer;
use std::{
    fmt,
    future::{Future, poll_fn},
    net::{IpAddr, SocketAddr},
    pin::{Pin, pin},
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

/// Content-free failures; no destination names, certificate/key bytes, response
/// bodies, paths or entered text are included in errors or debug output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidConfiguration,
    AddressFamilyUnavailable,
    Tailnet(fr_tailnet::Error),
    Cancelled,
    Session(crate::session_startup::Error),
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressFamily {
    Ipv4,
    Ipv6,
}
#[derive(Debug, Clone, Copy)]
pub struct Configuration {
    pub port: u16,
    pub family: AddressFamily,
    pub startup_timeout: Duration,
    pub transport: Policy,
}
impl Default for Configuration {
    fn default() -> Self {
        Self {
            port: 8443,
            family: AddressFamily::Ipv4,
            startup_timeout: Duration::from_secs(30),
            transport: Policy::default(),
        }
    }
}
impl Configuration {
    fn validate(self) -> Result<(), Error> {
        // The canonical native dial profile advertises these fixed receive
        // windows. Refuse incompatible policy before allocating a socket; do
        // not claim that a later adapter can retract already-advertised credit.
        if self.port == 0
            || self.startup_timeout < Duration::from_millis(1)
            || self.startup_timeout > Duration::from_secs(60)
            || self.transport.stream_window != 65_536
            || self.transport.connection_window != 524_288
        {
            return Err(Error::InvalidConfiguration);
        }
        self.transport
            .validate()
            .map_err(|_| Error::InvalidConfiguration)
    }
}
fn select_route(local: &[IpAddr], peer: &[IpAddr], cfg: Configuration) -> Result<DialRoute, Error> {
    cfg.validate()?;
    let matches = |ip: &&IpAddr| match cfg.family {
        AddressFamily::Ipv4 => ip.is_ipv4(),
        AddressFamily::Ipv6 => ip.is_ipv6(),
    };
    let local = *local
        .iter()
        .find(matches)
        .ok_or(Error::AddressFamilyUnavailable)?;
    let remote = *peer
        .iter()
        .find(matches)
        .ok_or(Error::AddressFamilyUnavailable)?;
    // Inputs are exact authenticated PeerTarget sets. NativeClient independently
    // rechecks the chosen route and identity before binding/handshake and handoff.
    Ok(DialRoute {
        local,
        remote: SocketAddr::new(remote, cfg.port),
    })
}
/// Owns a locally provisioned canonical dialer and one scoped viewer attempt.
/// Roots come from local platform/package trust, never the contacted peer. The
/// exclusive borrow bounds concurrent application sessions through this owner.
pub struct Client {
    api: LocalApi,
    native: NativeClient,
}
impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NativeViewerClient([installed tailnet and local trust])")
    }
}
impl Client {
    pub fn new(
        api: LocalApi,
        roots: Vec<Certificate>,
        handshake_timeout: Duration,
    ) -> Result<Self, Error> {
        let native =
            NativeClient::new(api.clone(), roots, handshake_timeout).map_err(Error::Tailnet)?;
        Ok(Self { api, native })
    }
    /// Discover, authenticate and run ONE viewer in a dedicated session Cx.
    /// The application receives the existing Viewer only after the canonical
    /// native dialer's hostname/chain/ALPN checks and post-TLS revalidation.
    /// Negotiation, local approval, media readiness and control remain separate.
    ///
    /// Target renewal runs alongside the ENTIRE application future. Its transport
    /// guard survives normal `Viewer` -> `ViewerSession` -> controlled/streaming
    /// handoffs. Every exit, including unpolled abandonment, cancels this Cx
    /// before dropping application work. Never pass a daemon-wide context.
    /// Reconnection is a new run; no uncertain action or credential is replayed.
    pub fn run<'a, T: 'a, F, A>(
        &'a mut self,
        cx: Cx,
        selector: PeerSelector<'a>,
        cfg: Configuration,
        offer: Offer,
        application: A,
    ) -> impl Future<Output = Result<T, Error>> + 'a
    where
        F: Future<Output = T> + 'a,
        A: FnOnce(Viewer) -> F + 'a,
    {
        Scoped {
            cx: cx.clone(),
            inner: Box::pin(async move {
                cfg.validate()?;
                offer.validate().map_err(|_| Error::InvalidConfiguration)?;
                let target = self
                    .api
                    .peer_target(&cx, selector)
                    .await
                    .map_err(Error::Tailnet)?;
                let route = select_route(target.local_addresses(), target.addresses(), cfg)?;
                let connected = self
                    .native
                    .dial(&cx, target, route)
                    .map_err(Error::Tailnet)?
                    .await
                    .map_err(Error::Tailnet)?;
                let (native, mut owner) = connected
                    .into_owned_connection(cx.clone())
                    .map_err(Error::Tailnet)?;
                let lease = owner.lease();
                let viewer = join_viewer(
                    &cx,
                    native,
                    Arc::new(move || lease.check().is_ok()),
                    offer,
                    cfg,
                )?;
                run_application(&cx, owner.serve(), application(viewer)).await
            }),
        }
    }
}
fn join_viewer(
    cx: &Cx,
    native: NativeQuicUdpConnection,
    check: Arc<dyn Fn() -> bool + Send + Sync>,
    offer: Offer,
    cfg: Configuration,
) -> Result<Viewer, Error> {
    cfg.validate()?;
    let mut viewer = Viewer::new(
        cx.clone(),
        native,
        offer,
        cfg.transport,
        cfg.startup_timeout,
    )
    .map_err(Error::Session)?;
    viewer
        .retain_connection_check(check)
        .map_err(Error::Session)?;
    Ok(viewer)
}
// Renewal is polled first and runs continuously. A completed refresh never
// wins a race that cancels a healthy network turn or restarts application work.
async fn run_application<T>(
    cx: &Cx,
    service: impl Future<Output = Result<(), fr_tailnet::Error>>,
    application: impl Future<Output = T>,
) -> Result<T, Error> {
    let mut service = pin!(service);
    let mut application = pin!(application);
    poll_fn(|task| {
        let result = match service.as_mut().poll(task) {
            Poll::Ready(result) => Poll::Ready(Err(Error::Tailnet(
                result.err().unwrap_or(fr_tailnet::Error::Revoked),
            ))),
            Poll::Pending => application.as_mut().poll(task).map(Ok),
        };
        if result.is_ready() {
            cx.cancel_fast(CancelKind::User);
        }
        result
    })
    .await
}
struct Scoped<F> {
    cx: Cx,
    inner: Pin<Box<F>>,
}
impl<T, F: Future<Output = Result<T, Error>>> Future for Scoped<F> {
    type Output = Result<T, Error>;
    fn poll(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if this.cx.checkpoint().is_err() {
            return Poll::Ready(Err(Error::Cancelled));
        }
        let result = this.inner.as_mut().poll(task);
        if result.is_ready() {
            this.cx.cancel_fast(CancelKind::User);
        }
        result
    }
}
impl<F> Drop for Scoped<F> {
    fn drop(&mut self) {
        self.cx.cancel_fast(CancelKind::User);
    }
}
#[cfg(test)]
mod tests;
