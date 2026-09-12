//! One bounded outbound QUIC attempt to an installed-daemon-selected node.
//! TLS authenticates the destination; this is neither a host listener nor a
//! desktop grant. There is no DNS, redirect, alternate transport, or retry here.
use super::{LocalApi, Lookup, PeerTarget, bounded, now};
use crate::Error;
use asupersync::{
    cx::Cx,
    net::{
        quic_core::{ConnectionId, TransportParameters},
        quic_native::{
            NativeQuicConnectionConfig, NativeQuicUdpConnection, QuicUdpEndpoint,
            QuicUdpEndpointConfig, handshake_driver::QuicHandshakeDriver,
        },
    },
    time::sleep,
    tls::{Certificate, TlsConnector},
};
use std::{
    fmt,
    future::{Future, poll_fn},
    io::Read,
    net::{IpAddr, SocketAddr},
    pin::pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::Poll,
    time::Duration,
};

const ALPN: &[u8] = b"fr-remote/0";
const REFRESH_INTERVAL: Duration = Duration::from_millis(500);
const MAX_ROOTS: usize = 256;
const MAX_ROOT_BYTES: usize = 1024 * 1024;

/// Exactly one address pair chosen locally from the current node-owned sets.
/// No automatic family fallback can retarget an attempt or reset its deadline.
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
        if timeout < Duration::from_millis(100) || timeout > Duration::from_secs(30) {
            return Err(Error::InvalidPolicy);
        }
        if roots.is_empty() || roots.len() > MAX_ROOTS {
            return Err(Error::InvalidTrustStore);
        }
        let bytes = roots.iter().try_fold(0usize, |total, cert| {
            total
                .checked_add(cert.as_der().len())
                .filter(|n| *n <= MAX_ROOT_BYTES)
        });
        if bytes.is_none() {
            return Err(Error::InvalidTrustStore);
        }
        // Do not use the upstream convenience client_config: its exact-leaf
        // fallback is intentionally unnecessary for this WebPKI-only path.
        let tls = TlsConnector::builder()
            .with_strict_ca_validation()
            .add_root_certificates(roots)
            .alpn_protocols_required(vec![ALPN.to_vec()])
            .min_protocol_version(0x0304u16.into())
            .max_protocol_version(0x0304u16.into())
            .enable_early_data(false)
            .build()
            .map_err(|_| Error::InvalidTrustStore)?;
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
        self.api.check_peer_target(cx, &target)?;
        route.check(&target)?;
        let started = now(cx)?;
        let end = started
            .checked_add(u64::try_from(self.timeout.as_micros()).map_err(|_| Error::InvalidPolicy)?)
            .ok_or(Error::Clock)?;
        self.busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| Error::Busy)?;
        let slot = Lookup(self.busy.clone());
        Ok(async move {
            let _slot = slot;
            let remaining = end.checked_sub(now(cx)?).ok_or(Error::Timeout)?;
            Box::pin(bounded(
                cx,
                Duration::from_micros(remaining),
                self.connect(cx, target, route),
            ))
            .await
        })
    }

    async fn connect(
        &self,
        cx: &Cx,
        target: PeerTarget,
        route: DialRoute,
    ) -> Result<ConnectedPeer, Error> {
        let target = self.api.revalidate_peer_target(cx, &target).await?;
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
        let current = Mutex::new(Arc::new(target));
        let native = {
            let mut refresh = pin!(self.refresh_while_connecting(cx, &current));
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
                    .map_err(|_| Error::NativeHandshake)
            })
            .await?
        };
        // A completed TLS exchange alone cannot freeze metadata from before the
        // handshake. Finish a fresh, same-identity read before exposing ANY app I/O.
        let old = current.into_inner().map_err(|_| Error::Revoked)?;
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

fn connection_ids() -> Result<(ConnectionId, ConnectionId), Error> {
    let mut bytes = [0u8; 32];
    // Kernel entropy, no deterministic runtime seed and no peer-controlled path.
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|_| Error::EntropyUnavailable)?;
    let a = ConnectionId::new(&bytes[..16]).map_err(|_| Error::EntropyUnavailable)?;
    let b = ConnectionId::new(&bytes[16..]).map_err(|_| Error::EntropyUnavailable)?;
    if a == b {
        return Err(Error::EntropyUnavailable);
    }
    Ok((a, b))
}
pub(super) fn endpoint_config() -> QuicUdpEndpointConfig {
    QuicUdpEndpointConfig {
        max_packet_size: 1200,
        max_batch_size: 16,
        ..Default::default()
    }
}
pub(super) fn connection_config() -> NativeQuicConnectionConfig {
    NativeQuicConnectionConfig {
        max_local_bidi: 0,
        max_local_uni: 8,
        send_window: 65_536,
        recv_window: 65_536,
        connection_send_limit: 524_288,
        connection_recv_limit: 524_288,
        max_datagram_frame_size: 1200,
        ..Default::default()
    }
}
pub(super) fn transport_parameters() -> Result<Vec<u8>, Error> {
    let cfg = connection_config();
    let mut bytes = Vec::new();
    TransportParameters {
        initial_max_data: Some(cfg.connection_recv_limit),
        initial_max_stream_data_uni: Some(cfg.recv_window),
        initial_max_streams_bidi: Some(0),
        initial_max_streams_uni: Some(8),
        max_datagram_frame_size: Some(1200),
        ..Default::default()
    }
    .encode(&mut bytes)
    .map_err(|_| Error::InvalidPolicy)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests;
