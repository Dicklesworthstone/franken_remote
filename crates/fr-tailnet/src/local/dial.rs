//! Bounded outbound QUIC dialing to an installed-daemon-selected node.
//! TLS authenticates the destination; this is neither a host listener nor a
//! desktop grant. An optional second address family uses the same identity,
//! shared dial slot and original timeout, before any application I/O exists.
//! There is no DNS, redirect or alternate transport. The TLS/QUIC profile is
//! shared with the named-destination dialer (`crate::profile`).
use super::{LocalApi, Lookup, PeerTarget, bounded, now};
use crate::Error;
// Used here, and re-exported for this module's tests and the certificate
// tests' fixtures.
use crate::profile::{ALPN, connection_ids, handshake_error, may_try_alternate};
pub(super) use crate::profile::{connection_config, endpoint_config, transport_parameters};
use asupersync::{
    cx::Cx,
    net::quic_native::{
        NativeQuicUdpConnection, QuicUdpEndpoint, handshake_driver::QuicHandshakeDriver,
    },
    time::sleep,
    tls::{Certificate, TlsConnector},
};
use std::{
    fmt,
    future::{Future, poll_fn},
    net::{IpAddr, SocketAddr},
    pin::pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::Poll,
    time::Duration,
};

const REFRESH_INTERVAL: Duration = Duration::from_millis(500);

/// Exactly one address pair chosen locally from the current node-owned sets.
/// Each attempt keeps this pair; a selected alternative is a separate handshake
/// under the original operation's deadline and the same verified identity.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct DialRoute {
    pub local: IpAddr,
    pub remote: SocketAddr,
}
impl fmt::Debug for DialRoute {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DialRoute([redacted])")
    }
}
impl DialRoute {
    fn check(self, target: &PeerTarget) -> Result<(), Error> {
        if self.remote.port() == 0
            || self.local.is_ipv4() != self.remote.is_ipv4()
            || !target.local_addresses().contains(&self.local)
            || !target.addresses().contains(&self.remote.ip())
            || self.local == self.remote.ip()
            || matches!(self.remote, SocketAddr::V6(v) if v.scope_id() != 0 || v.flowinfo() != 0)
        {
            return Err(Error::AddressMismatch);
        }
        Ok(())
    }
}

/// Reusable locally provisioned trust configuration with ONE shared dial slot.
/// Roots must come from the platform/package trust source, never the host being
/// contacted. Clones cannot create an unbounded family-racing connection storm.
#[derive(Clone)]
pub struct NativeClient {
    api: LocalApi,
    tls: TlsConnector,
    busy: Arc<AtomicBool>,
    timeout: Duration,
}
impl fmt::Debug for NativeClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NativeClient([local trust])")
    }
}
impl NativeClient {
    pub fn new(api: LocalApi, roots: Vec<Certificate>, timeout: Duration) -> Result<Self, Error> {
        let tls = crate::profile::connector(roots, timeout)?;
        Ok(Self {
            api,
            tls,
            busy: Arc::new(AtomicBool::new(false)),
            timeout,
        })
    }

    /// Claim at CALL time. The future owns the target and its sole socket;
    /// dropping it (even unpolled) releases the slot and cannot resume/replay it.
    /// `cx` and the target must use the original runtime clock domain.
    pub fn dial<'a>(
        &'a self,
        cx: &'a Cx,
        target: PeerTarget,
        route: DialRoute,
    ) -> Result<impl Future<Output = Result<ConnectedPeer, Error>> + 'a, Error> {
        self.dial_with_fallback(cx, target, route, None)
    }

    /// Dial one preferred pair, optionally followed by the other address family
    /// after a bind failure, local UDP I/O failure or handshake timeout. Both
    /// pairs must already belong to the SAME verified local/remote nodes and
    /// use the same remote port.
    ///
    /// The preferred family has half the original timeout when an alternative
    /// exists; the alternative receives only the remaining original time. There
    /// is one socket/handshake at a time, one shared dial slot, and continuous
    /// same-identity metadata renewal. Certificate, membership, cancellation
    /// and protocol refusals are terminal. No application operation is replayed.
    pub fn dial_with_fallback<'a>(
        &'a self,
        cx: &'a Cx,
        target: PeerTarget,
        route: DialRoute,
        alternate: Option<DialRoute>,
    ) -> Result<impl Future<Output = Result<ConnectedPeer, Error>> + 'a, Error> {
        self.api.check_peer_target(cx, &target)?;
        route.check(&target)?;
        if let Some(alternate) = alternate {
            alternate.check(&target)?;
            if alternate.local.is_ipv4() == route.local.is_ipv4()
                || alternate.remote.port() != route.remote.port()
            {
                return Err(Error::AddressMismatch);
            }
        }
        let started = now(cx)?;
        let duration = u64::try_from(self.timeout.as_micros()).map_err(|_| Error::InvalidPolicy)?;
        let end = started.checked_add(duration).ok_or(Error::Clock)?;
        self.busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| Error::Busy)?;
        let slot = Lookup(self.busy.clone());
        Ok(async move {
            let _slot = slot;
            if now(cx)? >= end {
                return Err(Error::Timeout);
            }
            let current = Mutex::new(Arc::new(target));
            let preferred_end = if alternate.is_some() {
                started + duration / 2
            } else {
                end
            };
            let result = self.connect_until(cx, &current, route, preferred_end).await;
            match (result, alternate) {
                (Err(error), Some(alternate)) if may_try_alternate(error) => {
                    // The first future (and its socket) has been dropped. Its
                    // latest same-identity snapshot remains checked by connect;
                    // a failed/expired authority can never authorize fallback.
                    self.connect_until(cx, &current, alternate, end).await
                }
                (result, _) => result,
            }
        })
    }

    async fn connect_until(
        &self,
        cx: &Cx,
        current: &Mutex<Arc<PeerTarget>>,
        route: DialRoute,
        end: u64,
    ) -> Result<ConnectedPeer, Error> {
        let remaining = end
            .checked_sub(now(cx)?)
            .filter(|left| *left != 0)
            .ok_or(Error::HandshakeTimeout)?;
        let result = Box::pin(bounded(
            cx,
            Duration::from_micros(remaining),
            self.connect(cx, current, route),
        ))
        .await;
        // A LocalAPI lookup timeout stays Timeout; only this attempt's own
        // fixed timeout can advance to the other already-verified family.
        let finished = now(cx)?;
        match result {
            Err(Error::Timeout) | Ok(_) if finished >= end => Err(Error::HandshakeTimeout),
            other => other,
        }
    }

    async fn connect(
        &self,
        cx: &Cx,
        current: &Mutex<Arc<PeerTarget>>,
        route: DialRoute,
    ) -> Result<ConnectedPeer, Error> {
        let old = current.lock().map_err(|_| Error::Revoked)?.clone();
        let target = self.api.revalidate_peer_target(cx, &old).await?;
        route.check(&target)?;
        let (initial, local) = connection_ids()?;
        let driver = QuicHandshakeDriver::client(
            self.tls.config().clone(),
            target
                .certificate_name()
                .to_owned()
                .try_into()
                .map_err(|_| Error::MalformedMetadata)?,
            transport_parameters()?,
        )
        .map_err(|_| Error::NativeHandshake)?;
        // Exact current local node address, never 0.0.0.0/:: or a DNS result.
        // This is outbound only; address binding is NOT proof of TUN ingress.
        let endpoint =
            QuicUdpEndpoint::bind(cx, SocketAddr::new(route.local, 0), endpoint_config())
                .await
                .map_err(|_| Error::NativeBind)?;
        let local_addr = endpoint.local_addr();
        self.api.check_peer_target(cx, &target)?;
        *current.lock().map_err(|_| Error::Revoked)? = Arc::new(target);
        let native = {
            let mut refresh = pin!(self.refresh_while_connecting(cx, current));
            let mut connecting = pin!(NativeQuicUdpConnection::connect(
                cx,
                endpoint,
                route.remote,
                driver,
                initial,
                local,
                connection_config(),
                ALPN,
            ));
            poll_fn(|task| {
                if let Poll::Ready(result) = refresh.as_mut().poll(task) {
                    return Poll::Ready(Err(result.err().unwrap_or(Error::Revoked)));
                }
                self.api
                    .check_peer_target(cx, current.lock().map_err(|_| Error::Revoked)?.as_ref())?;
                connecting
                    .as_mut()
                    .poll(task)
                    .map_err(|error| handshake_error(&error))
            })
            .await?
        };
        // A completed TLS exchange alone cannot freeze metadata from before the
        // handshake. Finish a fresh, same-identity read before exposing ANY app I/O.
        let old = current.lock().map_err(|_| Error::Revoked)?.clone();
        let target = self.api.revalidate_peer_target(cx, &old).await?;
        route.check(&target)?;
        if native.local_addr() != local_addr || native.peer_addr() != route.remote {
            return Err(Error::AddressMismatch);
        }
        self.api.check_peer_target(cx, &target)?;
        Ok(ConnectedPeer {
            api: self.api.clone(),
            target,
            native,
            local: local_addr,
            remote: route.remote,
        })
    }

    async fn refresh_while_connecting(
        &self,
        cx: &Cx,
        current: &Mutex<Arc<PeerTarget>>,
    ) -> Result<(), Error> {
        loop {
            sleep(cx.now(), REFRESH_INTERVAL).await;
            let old = current.lock().map_err(|_| Error::Revoked)?.clone();
            let next = self.api.revalidate_peer_target(cx, &old).await?;
            let mut slot = current.lock().map_err(|_| Error::Revoked)?;
            self.api.check_peer_target(cx, &old)?;
            *slot = Arc::new(next);
        }
    }
}

/// A completed real TLS/UDP connection still tied to its fresh target snapshot.
/// No application bytes are sent by dialing. The host must independently admit
/// this client and perform optional consent before observation or control.
pub struct ConnectedPeer {
    api: LocalApi,
    target: PeerTarget,
    native: NativeQuicUdpConnection,
    local: SocketAddr,
    remote: SocketAddr,
}
impl fmt::Debug for ConnectedPeer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ConnectedPeer([verified destination])")
    }
}
impl ConnectedPeer {
    /// Transfer the exact connection AND its authenticated destination into a
    /// renewable owner. This preserves provenance through long-lived viewer
    /// startup/service; dropping the returned owner invalidates every lease.
    /// Application I/O must retain its lease check and poll the owner's service.
    pub fn into_owned_connection(
        self,
        cx: Cx,
    ) -> Result<(NativeQuicUdpConnection, crate::TargetOwner), Error> {
        self.api.check_peer_target(&cx, &self.target)?;
        if self.native.local_addr() != self.local || self.native.peer_addr() != self.remote {
            return Err(Error::AddressMismatch);
        }
        let owner = crate::TargetOwner::new(self.api, cx, self.target)?;
        Ok((self.native, owner))
    }
    pub fn target(&self) -> &PeerTarget {
        &self.target
    }
    /// Immediate transfer into native viewer startup. A parked, expired result
    /// refuses and drops its socket. Ongoing session/path authority is separate.
    pub fn into_connection(self, cx: &Cx) -> Result<NativeQuicUdpConnection, Error> {
        self.api.check_peer_target(cx, &self.target)?;
        if self.native.local_addr() != self.local || self.native.peer_addr() != self.remote {
            return Err(Error::AddressMismatch);
        }
        Ok(self.native)
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests;
