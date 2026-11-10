//! The named-destination dialer for a client with no installed `LocalAPI`
//! (iOS: the Tailscale app exposes no local API to other apps).
//!
//! Destination identity is the host's certificate for ONE exact `MagicDNS`
//! name (`<machine>.<tailnet>.ts.net`), verified under strict `WebPKI`
//! against roots the caller provisions locally (never the contacted peer).
//! Only Tailscale's control plane can obtain a publicly trusted certificate
//! for a `ts.net` name, and only for the node that owns it, so a completed
//! handshake proves "the node that currently holds this name". Resolution
//! uses the system resolver (`MagicDNS` while the Tailscale VPN is up), and
//! every resolved address must lie inside Tailscale's own ranges, so a name
//! that resolves off-tailnet is refused before any packet is sent.
//!
//! [`HostPin`] adds continuity across sessions: the first successful connect
//! reports the exact tailnet addresses the name resolved to; later connects
//! that present that pin refuse (`IdentityChanged`) when the name resolves to
//! any address outside it, so a renamed or replaced machine that inherits the
//! name is not silently trusted. Tailnet addresses are stable per node.
//!
//! What this does NOT prove: the client's own identity (the host admits the
//! connection independently from its `LocalAPI` `WhoIs` of the source
//! address), or that the node is still a tailnet member after the handshake
//! (no client-side metadata lease exists without `LocalAPI`). The TLS/QUIC
//! profile is the installed dialer's (`crate::profile`).
use crate::{
    Error,
    profile::{
        ALPN, bounded, connection_config, connection_ids, connector, endpoint_config,
        handshake_error, may_try_alternate, now, transport_parameters,
    },
};
use asupersync::{
    cx::Cx,
    net::quic_native::{
        NativeQuicUdpConnection, QuicUdpEndpoint, handshake_driver::QuicHandshakeDriver,
    },
    tls::{Certificate, TlsConnector},
};
use std::{
    fmt,
    future::Future,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs},
    pin::Pin,
    str::FromStr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll, Waker},
    time::Duration,
};

const MAX_NAME: usize = 253;
const MAX_LABEL: usize = 63;
/// A Tailscale node has one IPv4 and one IPv6 address; allow slack, not lists.
const MAX_ADDRESSES: usize = 4;
/// The system resolver call cannot be cancelled; bound how many may be
/// outstanding at once so repeated timeouts cannot accumulate threads.
const MAX_RESOLVERS: usize = 2;
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);
const PIN_PREFIX: &str = "fr-pin-v1";
static RESOLVERS: AtomicUsize = AtomicUsize::new(0);

/// One exact `MagicDNS` machine name, `<machine>.<tailnet>.ts.net`, stored in
/// lowercase ASCII. No IP literal, wildcard, trailing dot, Unicode or other
/// suffix is accepted.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct HostName(String);
impl fmt::Debug for HostName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HostName([redacted])")
    }
}
impl HostName {
    pub fn parse(text: &str) -> Result<Self, Error> {
        if text.is_empty() || text.len() > MAX_NAME || !text.is_ascii() {
            return Err(Error::InvalidEndpoint);
        }
        let name = text.to_ascii_lowercase();
        let labels: Vec<&str> = name.split('.').collect();
        // Exactly machine + tailnet + "ts" + "net".
        if labels.len() != 4 || labels[2] != "ts" || labels[3] != "net" {
            return Err(Error::InvalidEndpoint);
        }
        for label in &labels[..2] {
            let bytes = label.as_bytes();
            if bytes.is_empty()
                || bytes.len() > MAX_LABEL
                || bytes[0] == b'-'
                || bytes[bytes.len() - 1] == b'-'
                || !bytes
                    .iter()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
            {
                return Err(Error::InvalidEndpoint);
            }
        }
        Ok(Self(name))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Tailscale assigns node addresses only from `100.64.0.0/10` (shared address
/// space) and `fd7a:115c:a1e0::/48`. This is a routing sanity check, not
/// identity: identity is the certificate and the pin.
pub fn is_tailnet_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            a == 100 && (64..128).contains(&b)
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
        }
    }
}

/// The exact tailnet addresses one name resolved to when it was first
/// trusted. Serialized as `fr-pin-v1 <name> <address>...` (sorted, unique).
#[derive(Clone, PartialEq, Eq)]
pub struct HostPin {
    name: HostName,
    addresses: Vec<IpAddr>,
}
impl fmt::Debug for HostPin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HostPin([redacted])")
    }
}
impl fmt::Display for HostPin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{PIN_PREFIX} {}", self.name.as_str())?;
        for address in &self.addresses {
            write!(f, " {address}")?;
        }
        Ok(())
    }
}
impl FromStr for HostPin {
    type Err = Error;
    fn from_str(text: &str) -> Result<Self, Error> {
        let mut parts = text.split(' ');
        if parts.next() != Some(PIN_PREFIX) {
            return Err(Error::InvalidPolicy);
        }
        let name = HostName::parse(parts.next().ok_or(Error::InvalidPolicy)?)
            .map_err(|_| Error::InvalidPolicy)?;
        let mut addresses = Vec::new();
        for part in parts {
            let ip: IpAddr = part.parse().map_err(|_| Error::InvalidPolicy)?;
            if addresses.len() == MAX_ADDRESSES {
                return Err(Error::InvalidPolicy);
            }
            addresses.push(ip);
        }
        let pin = Self::new(name, addresses).map_err(|_| Error::InvalidPolicy)?;
        // Only the canonical spelling round-trips; no alternate forms.
        if pin.to_string() != text {
            return Err(Error::InvalidPolicy);
        }
        Ok(pin)
    }
}
impl HostPin {
    /// A pin for addresses learned from an authenticated source (a host's
    /// tailnet directory) rather than the resolver: tailnet addresses only,
    /// at most four, sorted and unique.
    pub fn from_addresses(name: HostName, addresses: Vec<IpAddr>) -> Result<Self, Error> {
        Self::new(name, addresses)
    }
    fn new(name: HostName, mut addresses: Vec<IpAddr>) -> Result<Self, Error> {
        addresses.sort_unstable();
        addresses.dedup();
        if addresses.is_empty()
            || addresses.len() > MAX_ADDRESSES
            || !addresses.iter().all(|ip| is_tailnet_address(*ip))
        {
            return Err(Error::OutsideTailnet);
        }
        Ok(Self { name, addresses })
    }
    pub fn name(&self) -> &HostName {
        &self.name
    }
    pub fn addresses(&self) -> &[IpAddr] {
        &self.addresses
    }
    /// A resolution is trusted under this pin only when it is the same name
    /// and every resolved address was already pinned. Fewer addresses (one
    /// family withdrawn) still match; any new address does not.
    pub fn admits(&self, name: &HostName, resolved: &[IpAddr]) -> Result<(), Error> {
        if &self.name != name {
            return Err(Error::InvalidPolicy);
        }
        if resolved.is_empty() || !resolved.iter().all(|ip| self.addresses.contains(ip)) {
            return Err(Error::IdentityChanged);
        }
        Ok(())
    }
}

/// Check resolver output: only tailnet addresses, at least one, at most
/// [`MAX_ADDRESSES`] distinct, IPv6 first (as the installed dialer prefers).
fn checked_addresses(resolved: impl IntoIterator<Item = IpAddr>) -> Result<Vec<IpAddr>, Error> {
    let mut addresses: Vec<IpAddr> = Vec::new();
    for ip in resolved {
        // A v4-mapped v6 answer is a v4 address in disguise; refuse it.
        if !is_tailnet_address(ip) {
            return Err(Error::OutsideTailnet);
        }
        if !addresses.contains(&ip) {
            if addresses.len() == MAX_ADDRESSES {
                return Err(Error::OutsideTailnet);
            }
            addresses.push(ip);
        }
    }
    if addresses.is_empty() {
        return Err(Error::NameUnresolved);
    }
    addresses.sort_unstable_by_key(|ip| (ip.is_ipv4(), *ip));
    Ok(addresses)
}

/// Locally provisioned trust for ONE named destination with one shared dial
/// slot. Clones share the slot.
#[derive(Clone)]
pub struct NamedClient {
    destination: HostName,
    tls: TlsConnector,
    busy: Arc<AtomicBool>,
    timeout: Duration,
}
impl fmt::Debug for NamedClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NamedClient([named destination, local trust])")
    }
}
struct Slot(Arc<AtomicBool>);
impl Drop for Slot {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}
impl NamedClient {
    pub fn new(
        destination: HostName,
        roots: Vec<Certificate>,
        timeout: Duration,
    ) -> Result<Self, Error> {
        Ok(Self {
            destination,
            tls: connector(roots, timeout)?,
            busy: Arc::new(AtomicBool::new(false)),
            timeout,
        })
    }
    pub fn destination(&self) -> &HostName {
        &self.destination
    }

    /// Resolve the name, apply `pin` (when present), then dial the preferred
    /// family with one fallback to the other family after a bind failure,
    /// local UDP I/O failure or handshake timeout, all inside the original
    /// timeout. The slot is claimed at CALL time; dropping the future (even
    /// unpolled) releases it and drops any socket it owns.
    pub fn dial<'a>(
        &'a self,
        cx: &'a Cx,
        port: u16,
        pin: Option<&'a HostPin>,
    ) -> Result<impl Future<Output = Result<NamedConnection, Error>> + 'a, Error> {
        if port == 0 {
            return Err(Error::InvalidEndpoint);
        }
        if pin.is_some_and(|pin| pin.name != self.destination) {
            return Err(Error::InvalidPolicy);
        }
        let started = now(cx)?;
        let duration = u64::try_from(self.timeout.as_micros()).map_err(|_| Error::InvalidPolicy)?;
        let end = started.checked_add(duration).ok_or(Error::Clock)?;
        self.busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| Error::Busy)?;
        let slot = Slot(self.busy.clone());
        Ok(async move {
            let _slot = slot;
            let resolved = Box::pin(bounded(
                cx,
                RESOLVE_TIMEOUT.min(self.timeout),
                resolve(&self.destination, port)?,
            ))
            .await
            .map_err(|error| match error {
                Error::Timeout => Error::NameUnresolved,
                other => other,
            })?;
            let addresses = checked_addresses(resolved)?;
            if let Some(pin) = pin {
                pin.admits(&self.destination, &addresses)?;
            }
            self.dial_resolved(cx, &addresses, port, started, end)
                .await
                .map(|native| NamedConnection {
                    pin: pin.cloned().unwrap_or_else(|| HostPin {
                        name: self.destination.clone(),
                        addresses: {
                            let mut sorted = addresses.clone();
                            sorted.sort_unstable();
                            sorted
                        },
                    }),
                    native,
                })
        })
    }

    /// Dial `pin`'s own addresses without the resolver: for a destination
    /// whose exact tailnet addresses came from an authenticated directory
    /// (`MagicDNS` may answer only one family, and a host may serve only the
    /// other). TLS still verifies the certificate for exactly this name; only
    /// tailnet addresses (all a pin can hold) are dialed, IPv6 first with
    /// one IPv4 alternative. The slot is claimed at CALL time.
    pub fn dial_pinned<'a>(
        &'a self,
        cx: &'a Cx,
        port: u16,
        pin: &'a HostPin,
    ) -> Result<impl Future<Output = Result<NamedConnection, Error>> + 'a, Error> {
        if port == 0 {
            return Err(Error::InvalidEndpoint);
        }
        if pin.name != self.destination {
            return Err(Error::InvalidPolicy);
        }
        let started = now(cx)?;
        let duration = u64::try_from(self.timeout.as_micros()).map_err(|_| Error::InvalidPolicy)?;
        let end = started.checked_add(duration).ok_or(Error::Clock)?;
        self.busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| Error::Busy)?;
        let slot = Slot(self.busy.clone());
        Ok(async move {
            let _slot = slot;
            let addresses = checked_addresses(pin.addresses.iter().copied())?;
            self.dial_resolved(cx, &addresses, port, started, end)
                .await
                .map(|native| NamedConnection {
                    pin: pin.clone(),
                    native,
                })
        })
    }

    /// One preferred address and at most one other-family alternative.
    async fn dial_resolved(
        &self,
        cx: &Cx,
        addresses: &[IpAddr],
        port: u16,
        started: u64,
        end: u64,
    ) -> Result<NativeQuicUdpConnection, Error> {
        let preferred = *addresses.first().ok_or(Error::NameUnresolved)?;
        let alternate = addresses
            .iter()
            .copied()
            .find(|ip| ip.is_ipv4() != preferred.is_ipv4());
        let preferred_end = if alternate.is_some() {
            started + (end - started) / 2
        } else {
            end
        };
        let first = self
            .connect_until(cx, SocketAddr::new(preferred, port), preferred_end)
            .await;
        match (first, alternate) {
            (Err(error), Some(alternate)) if may_try_alternate(error) => {
                self.connect_until(cx, SocketAddr::new(alternate, port), end)
                    .await
            }
            (result, _) => result,
        }
    }

    async fn connect_until(
        &self,
        cx: &Cx,
        remote: SocketAddr,
        end: u64,
    ) -> Result<NativeQuicUdpConnection, Error> {
        let remaining = end
            .checked_sub(now(cx)?)
            .filter(|left| *left != 0)
            .ok_or(Error::HandshakeTimeout)?;
        let result = Box::pin(bounded(
            cx,
            Duration::from_micros(remaining),
            self.connect(cx, remote),
        ))
        .await;
        let finished = now(cx)?;
        match result {
            Err(Error::Timeout) | Ok(_) if finished >= end => Err(Error::HandshakeTimeout),
            other => other,
        }
    }

    async fn connect(&self, cx: &Cx, remote: SocketAddr) -> Result<NativeQuicUdpConnection, Error> {
        let (initial, local) = connection_ids()?;
        let driver = QuicHandshakeDriver::client(
            self.tls.config().clone(),
            self.destination
                .as_str()
                .to_owned()
                .try_into()
                .map_err(|_| Error::InvalidEndpoint)?,
            transport_parameters()?,
        )
        .map_err(|_| Error::NativeHandshake)?;
        // No installed daemon reports this client's own tailnet address, so
        // the system routes the unspecified bind (Tailscale's VPN route on
        // iOS). The host admits the source address it sees, not this bind.
        let unspecified = if remote.is_ipv4() {
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        } else {
            IpAddr::V6(Ipv6Addr::UNSPECIFIED)
        };
        let endpoint =
            QuicUdpEndpoint::bind(cx, SocketAddr::new(unspecified, 0), endpoint_config())
                .await
                .map_err(|_| Error::NativeBind)?;
        let native = NativeQuicUdpConnection::connect(
            cx,
            endpoint,
            remote,
            driver,
            initial,
            local,
            connection_config(),
            ALPN,
        )
        .await
        .map_err(|error| handshake_error(&error))?;
        if native.peer_addr() != remote {
            return Err(Error::AddressMismatch);
        }
        Ok(native)
    }
}

/// A completed strict-WebPKI connection to the named destination and the pin
/// that admitted it (the presented pin, or on first connect the observed
/// one). No application bytes have been sent.
pub struct NamedConnection {
    pin: HostPin,
    native: NativeQuicUdpConnection,
}
impl fmt::Debug for NamedConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NamedConnection([verified destination])")
    }
}
impl NamedConnection {
    pub fn pin(&self) -> &HostPin {
        &self.pin
    }
    pub fn into_parts(self) -> (HostPin, NativeQuicUdpConnection) {
        (self.pin, self.native)
    }
}

/// The system resolver on a bounded helper thread. The returned future never
/// blocks the runtime; dropping it abandons (but cannot cancel) the lookup.
fn resolve(
    name: &HostName,
    port: u16,
) -> Result<impl Future<Output = Result<Vec<IpAddr>, Error>> + use<>, Error> {
    if RESOLVERS.fetch_add(1, Ordering::AcqRel) >= MAX_RESOLVERS {
        RESOLVERS.fetch_sub(1, Ordering::AcqRel);
        return Err(Error::Busy);
    }
    let shared: Arc<Mutex<Resolution>> = Arc::new(Mutex::new(Resolution::default()));
    let worker = shared.clone();
    let query = name.as_str().to_owned();
    let release = Release;
    std::thread::Builder::new()
        .name("fr-resolve".into())
        .spawn(move || {
            let _release = release;
            // Content-free result: the OS error text is never retained.
            let result = (query.as_str(), port)
                .to_socket_addrs()
                .map(|found| found.map(|a| a.ip()).take(16).collect::<Vec<_>>())
                .map_err(|_| Error::NameUnresolved);
            if let Ok(mut state) = worker.lock() {
                state.result = Some(result);
                if let Some(waker) = state.waker.take() {
                    waker.wake();
                }
            }
        })
        .map_err(|_| Error::Busy)?;
    Ok(Resolving(shared))
}
/// One claimed resolver thread; released when that thread ends (or fails to
/// start), however its lookup finished.
struct Release;
impl Drop for Release {
    fn drop(&mut self) {
        RESOLVERS.fetch_sub(1, Ordering::AcqRel);
    }
}
#[derive(Default)]
struct Resolution {
    result: Option<Result<Vec<IpAddr>, Error>>,
    waker: Option<Waker>,
}
struct Resolving(Arc<Mutex<Resolution>>);
impl Future for Resolving {
    type Output = Result<Vec<IpAddr>, Error>;
    fn poll(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<Self::Output> {
        let Ok(mut state) = self.0.lock() else {
            return Poll::Ready(Err(Error::NameUnresolved));
        };
        if let Some(result) = state.result.take() {
            return Poll::Ready(result);
        }
        state.waker = Some(task.waker().clone());
        Poll::Pending
    }
}

#[cfg(test)]
mod tests;
