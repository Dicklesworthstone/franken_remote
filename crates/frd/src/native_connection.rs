//! Installed-tailnet connection kept under renewal through native viewer service.
//! Reuses fr-tailnet's canonical TLS dialer, not another connection implementation.
//! A client without an installed `LocalAPI` (iOS) dials one exact `MagicDNS`
//! name with fr-tailnet's named dialer on the same TLS/QUIC profile instead.
#[cfg(host_stack)]
pub mod host;
pub mod reconnect;

use crate::session_startup::Viewer;
use asupersync::{
    cx::Cx, net::quic_native::NativeQuicUdpConnection, tls::Certificate, types::CancelKind,
};
use fr_tailnet::{DialRoute, NamedClient, NativeClient};
pub use fr_tailnet::{Discovery, Error as TailnetError, HostName, HostPin, LocalApi, PeerSelector};
use fr_transport::quic::Policy;
use fr_wire::negotiation::Offer;
use std::{
    fmt,
    future::{Future, poll_fn},
    net::{IpAddr, SocketAddr},
    pin::{Pin, pin},
    sync::{Arc, Mutex},
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
    /// Prefer a verified IPv6 pair, with one verified IPv4 alternative before
    /// application startup. Explicit family choices never fall back. Both
    /// families cross the same tailnet tunnel; IPv6 goes first because a macOS
    /// host with IPv4 forwarding on can enforce tunnel-only ingress for IPv6
    /// alone, and the preferred attempt holds half the startup timeout.
    Automatic,
    Ipv4,
    Ipv6,
}
#[derive(Debug, Clone, Copy)]
pub struct Configuration {
    pub port: u16,
    pub family: AddressFamily,
    pub startup_timeout: Duration,
    pub transport: Policy,
    /// The largest picture (pixel width, height) this viewer can show 1:1,
    /// usually its screen's. A host that can scale sends pictures that fit
    /// inside instead of the display's native pixels. `None` keeps native.
    pub picture: Option<(u32, u32)>,
}
impl Default for Configuration {
    fn default() -> Self {
        Self {
            port: 8443,
            family: AddressFamily::Automatic,
            startup_timeout: Duration::from_secs(30),
            transport: Policy::default(),
            picture: None,
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
            || self.picture.is_some_and(|(width, height)| {
                width.min(height) < fr_wire::display::picture::MIN_BOX_PIXELS
            })
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
        AddressFamily::Automatic => false,
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
fn select_routes(
    local: &[IpAddr],
    peer: &[IpAddr],
    cfg: Configuration,
) -> Result<(DialRoute, Option<DialRoute>), Error> {
    cfg.validate()?;
    if cfg.family != AddressFamily::Automatic {
        return select_route(local, peer, cfg).map(|route| (route, None));
    }
    let ipv4 = select_route(
        local,
        peer,
        Configuration {
            family: AddressFamily::Ipv4,
            ..cfg
        },
    )
    .ok();
    let ipv6 = select_route(
        local,
        peer,
        Configuration {
            family: AddressFamily::Ipv6,
            ..cfg
        },
    )
    .ok();
    match (ipv6, ipv4) {
        (Some(primary), alternate) => Ok((primary, alternate)),
        (None, Some(primary)) => Ok((primary, None)),
        (None, None) => Err(Error::AddressFamilyUnavailable),
    }
}
/// Owns a locally provisioned canonical dialer and one scoped viewer attempt.
/// Roots come from local platform/package trust, never the contacted peer. The
/// exclusive borrow bounds concurrent application sessions through this owner.
pub struct Client {
    route: Route,
}
/// How the destination is chosen and authenticated.
enum Route {
    /// The installed daemon's `LocalAPI` selects, pins and renews the exact
    /// destination node (Linux, macOS).
    Installed { api: LocalApi, native: NativeClient },
    /// No installed daemon (iOS): one exact `MagicDNS` name under strict
    /// `WebPKI`, plus the address pin (`HostPin`) that every later attempt of
    /// this client must match. The pin is the caller's, or on first connect
    /// the one observed; it is readable through [`Client::pin`].
    Named {
        client: NamedClient,
        pin: Arc<Mutex<Option<HostPin>>>,
        /// Dial the pin's addresses instead of resolving the name
        /// ([`Client::named_at`]).
        direct: bool,
    },
}
/// Read-only access to a named client's pin (see [`Client::pin_watch`]).
#[derive(Clone)]
pub struct PinWatch(Arc<Mutex<Option<HostPin>>>);
impl fmt::Debug for PinWatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PinWatch([redacted])")
    }
}
impl PinWatch {
    pub fn get(&self) -> Option<HostPin> {
        self.0.lock().ok().and_then(|pin| pin.clone())
    }
}
/// What a run asks for: an installed-daemon selector, or the named route's
/// one fixed destination.
#[derive(Clone, Copy)]
enum Destination<'a> {
    Selected(PeerSelector<'a>),
    Named,
}
impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.route {
            Route::Installed { .. } => {
                f.write_str("NativeViewerClient([installed tailnet and local trust])")
            }
            Route::Named { .. } => {
                f.write_str("NativeViewerClient([named destination and local trust])")
            }
        }
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
        Ok(Self {
            route: Route::Installed { api, native },
        })
    }
    /// A client with no installed `LocalAPI`: `destination` is one exact
    /// `MagicDNS` name, `pin` the addresses it was first trusted at (None on a
    /// first connect, which then pins what it observes). A pin for another
    /// name is refused. There is no destination lease to renew: the host's
    /// own admission and the session's transport checks bound the session.
    pub fn named(
        destination: HostName,
        pin: Option<HostPin>,
        roots: Vec<Certificate>,
        handshake_timeout: Duration,
    ) -> Result<Self, Error> {
        if pin.as_ref().is_some_and(|pin| pin.name() != &destination) {
            return Err(Error::InvalidConfiguration);
        }
        let client =
            NamedClient::new(destination, roots, handshake_timeout).map_err(Error::Tailnet)?;
        Ok(Self {
            route: Route::Named {
                client,
                pin: Arc::new(Mutex::new(pin)),
                direct: false,
            },
        })
    }
    /// A named client that dials `pin`'s own tailnet addresses instead of
    /// resolving the name: for a machine listed by an authenticated host
    /// directory. The certificate must still be valid for exactly the pin's
    /// name, and every attempt dials only the pinned addresses.
    pub fn named_at(
        pin: HostPin,
        roots: Vec<Certificate>,
        handshake_timeout: Duration,
    ) -> Result<Self, Error> {
        let client = NamedClient::new(pin.name().clone(), roots, handshake_timeout)
            .map_err(Error::Tailnet)?;
        Ok(Self {
            route: Route::Named {
                client,
                pin: Arc::new(Mutex::new(Some(pin))),
                direct: true,
            },
        })
    }
    /// The named route's current pin: the caller's, or the one the first
    /// successful handshake observed. None for an installed-daemon client or
    /// before any named handshake succeeded without a pin.
    pub fn pin(&self) -> Option<HostPin> {
        self.pin_watch().and_then(|watch| watch.get())
    }
    /// A read-only view of the named route's pin that stays readable while a
    /// run holds this client (an app persists the first observed pin as soon
    /// as the attempt is authenticated). None for an installed-daemon client.
    pub fn pin_watch(&self) -> Option<PinWatch> {
        match &self.route {
            Route::Installed { .. } => None,
            Route::Named { pin, .. } => Some(PinWatch(pin.clone())),
        }
    }
    /// [`Self::run`] for a [`Client::named`] or [`Client::named_at`] client:
    /// one attempt to its one destination, under its pin. An
    /// installed-daemon client refuses.
    pub fn run_named<'a, T: 'a, F, A>(
        &'a mut self,
        cx: Cx,
        cfg: Configuration,
        offer: Offer,
        application: A,
    ) -> impl Future<Output = Result<T, Error>> + 'a
    where
        F: Future<Output = T> + 'a,
        A: FnOnce(Viewer) -> F + 'a,
    {
        self.run_checked(
            cx.clone(),
            cx,
            Destination::Named,
            cfg,
            offer,
            None,
            application,
        )
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
        self.run_checked(
            cx.clone(),
            cx,
            Destination::Selected(selector),
            cfg,
            offer,
            None,
            application,
        )
    }
    // A reconnect pin is comparison-only metadata. Every attempt still performs
    // fresh LocalAPI, TLS and post-handshake checks before exposing a Viewer.
    #[allow(clippy::too_many_arguments)]
    //
    // `cx` owns the connection: destination lease, renewal and this scope.
    // `application` is the Viewer's own context. Fencing a granted controller
    // cancels the application context; the connection stays up so its
    // closing exchange (on an independent cleanup context) can still run.
    // Both are cancelled when the scope ends. `run` passes one context.
    fn run_checked<'a, T: 'a, F, A>(
        &'a mut self,
        cx: Cx,
        application_cx: Cx,
        destination: Destination<'a>,
        cfg: Configuration,
        offer: Offer,
        expected: Option<&'a fr_tailnet::PeerTarget>,
        application: A,
    ) -> impl Future<Output = Result<T, Error>> + 'a
    where
        F: Future<Output = T> + 'a,
        A: FnOnce(Viewer) -> F + 'a,
    {
        Scoped {
            cx: cx.clone(),
            application: application_cx.clone(),
            inner: Box::pin(async move {
                cfg.validate()?;
                offer.validate().map_err(|_| Error::InvalidConfiguration)?;
                match (&self.route, destination) {
                    (Route::Installed { api, native }, Destination::Selected(selector)) => {
                        let target = api
                            .peer_target(&cx, selector)
                            .await
                            .map_err(Error::Tailnet)?;
                        if expected.is_some_and(|old| !old.same_identity(&target)) {
                            return Err(Error::Tailnet(fr_tailnet::Error::IdentityChanged));
                        }
                        let (route, alternate) =
                            select_routes(target.local_addresses(), target.addresses(), cfg)?;
                        let connected = native
                            .dial_with_fallback(&cx, target, route, alternate)
                            .map_err(Error::Tailnet)?
                            .await
                            .map_err(Error::Tailnet)?;
                        let (native, mut owner) = connected
                            .into_owned_connection(cx.clone())
                            .map_err(Error::Tailnet)?;
                        let lease = owner.lease();
                        let (viewer, abort) = start_viewer(
                            &cx,
                            &application_cx,
                            native,
                            Arc::new(move || lease.check().is_ok()),
                            offer,
                            cfg,
                        )?;
                        // Finish INSIDE the supervised application future: the
                        // ready boundary below cancels the connection and its
                        // destination lease. Renewal keeps progressing during
                        // this bounded attempt. The original application
                        // result is never replaced by the best-effort socket
                        // write or mistaken for remote cleanup.
                        run_application(
                            &cx,
                            owner.serve(),
                            finish_application(application(viewer), abort),
                        )
                        .await
                    }
                    (
                        Route::Named {
                            client,
                            pin,
                            direct,
                        },
                        Destination::Named,
                    ) => {
                        // Named destinations prefer IPv6 then IPv4 like the
                        // installed route's Automatic; no explicit family.
                        if cfg.family != AddressFamily::Automatic {
                            return Err(Error::InvalidConfiguration);
                        }
                        let current = pin.lock().map_err(|_| Error::InvalidConfiguration)?.clone();
                        let connected = if *direct {
                            let at = current.as_ref().ok_or(Error::InvalidConfiguration)?;
                            client
                                .dial_pinned(&cx, cfg.port, at)
                                .map_err(Error::Tailnet)?
                                .await
                        } else {
                            client
                                .dial(&cx, cfg.port, current.as_ref())
                                .map_err(Error::Tailnet)?
                                .await
                        }
                        .map_err(Error::Tailnet)?;
                        let (observed, native) = connected.into_parts();
                        // Every later attempt of this client must match it.
                        *pin.lock().map_err(|_| Error::InvalidConfiguration)? = Some(observed);
                        // No installed metadata exists to lease or renew; the
                        // TLS identity is fixed for this connection's life.
                        let (viewer, abort) = start_viewer(
                            &cx,
                            &application_cx,
                            native,
                            Arc::new(|| true),
                            offer,
                            cfg,
                        )?;
                        run_application(
                            &cx,
                            std::future::pending(),
                            finish_application(application(viewer), abort),
                        )
                        .await
                    }
                    _ => Err(Error::InvalidConfiguration),
                }
            }),
        }
    }
}
/// The authenticated connection becomes a `Viewer` with its connection check
/// and an armed client-abort report.
fn start_viewer(
    cx: &Cx,
    application_cx: &Cx,
    native: NativeQuicUdpConnection,
    check: Arc<dyn Fn() -> bool + Send + Sync>,
    offer: Offer,
    cfg: Configuration,
) -> Result<(Viewer, fr_transport::quic::ClientAbortReport), Error> {
    let mut viewer = join_viewer(application_cx, native, check, offer, cfg)?;
    let abort = fr_transport::quic::ClientAbortReport::default();
    viewer
        .arm_client_abort(cx, abort.registration())
        .map_err(Error::Session)?;
    Ok((viewer, abort))
}
async fn finish_application<T>(
    application: impl Future<Output = T>,
    abort: fr_transport::quic::ClientAbortReport,
) -> T {
    let result = application.await;
    let _ = abort.finish().await;
    result
}
fn join_viewer(
    cx: &Cx,
    native: NativeQuicUdpConnection,
    check: Arc<dyn Fn() -> bool + Send + Sync>,
    offer: Offer,
    cfg: Configuration,
) -> Result<Viewer, Error> {
    cfg.validate()?;
    // Scaled pictures are optional: a full offer keeps native pictures.
    let offer = match cfg.picture {
        Some(_) => fr_wire::display::picture::with_capability(offer)
            .map_err(|_| Error::InvalidConfiguration)?,
        None => offer,
    };
    let picture = cfg
        .picture
        .filter(|_| fr_wire::display::picture::selected(&offer.capabilities));
    let mut viewer = Viewer::new(
        cx.clone(),
        native,
        offer,
        cfg.transport,
        cfg.startup_timeout,
    )
    .map_err(Error::Session)?;
    if let Some(within) = picture {
        viewer.state_picture(within).map_err(Error::Session)?;
    }
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
    let lost = |result: Result<(), fr_tailnet::Error>| {
        Err(Error::Tailnet(
            result.err().unwrap_or(fr_tailnet::Error::Revoked),
        ))
    };
    poll_fn(|task| {
        let result = match service.as_mut().poll(task) {
            Poll::Ready(result) => Poll::Ready(lost(result)),
            Poll::Pending => match application.as_mut().poll(task) {
                // The application meets a stopped destination lease first, as
                // a refused transport authorization (an unnamed
                // Transport(Unauthorized)). The lease's own cause is the
                // reason: report it. A live lease leaves the result as is, and
                // so does a lease that only saw the ending session cancel it:
                // that is not a cause, and it hid every other end as
                // `cancelled`.
                Poll::Ready(value) => Poll::Ready(match service.as_mut().poll(task) {
                    Poll::Ready(Err(cause)) if cause != fr_tailnet::Error::Cancelled => {
                        lost(Err(cause))
                    }
                    _ => Ok(value),
                }),
                Poll::Pending => Poll::Pending,
            },
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
    application: Cx,
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
            this.application.cancel_fast(CancelKind::User);
        }
        result
    }
}
impl<F> Drop for Scoped<F> {
    fn drop(&mut self) {
        self.cx.cancel_fast(CancelKind::User);
        self.application.cancel_fast(CancelKind::User);
    }
}
#[cfg(test)]
mod tests;
