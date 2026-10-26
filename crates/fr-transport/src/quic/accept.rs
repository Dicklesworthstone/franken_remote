//! Cold, single-connection server admission to the canonical native UDP owner.
//!
//! No client address or Initial CID is supplied out of band. This layer learns
//! routing metadata from a bounded receive, then delegates ALL TLS, packet
//! protection and connection construction to Asupersync. Routing is not identity:
//! the host must independently enforce protected tailnet ingress and `LocalAPI`
//! admission before exposing any application operation.
//!
//! The pinned upstream single-connection API cannot take a consumed Initial.
//! Consequently the discovery datagram is discarded and the canonical handshake
//! processes its retransmission. This costs an Initial PTO; it is not a new QUIC
//! implementation, Retry protocol, lossless demultiplexer or multi-client server.
//! A bounded set of exact local addresses may compete for the same ONE incoming
//! connection. Discovery polls them fairly under one aggregate budget; only the
//! selected socket enters TLS. Success transfers it to the authenticated owner;
//! refusal, timeout and abandonment close every socket. No task is started.

use crate::quic::{ALPN, Policy};
use asupersync::{
    cx::Cx,
    net::{
        quic_core::{ConnectionId, LongPacketType, ProtectedHeaderPrefix, TransportParameters},
        quic_native::{
            NativeQuicConnectionConfig, NativeQuicUdpConnection, QuicUdpEndpoint,
            QuicUdpEndpointConfig, handshake_driver::QuicHandshakeDriver,
        },
    },
    time::timeout_at,
    types::Time,
};
use std::{
    fmt,
    future::{Future, poll_fn},
    net::SocketAddr,
    pin::pin,
    time::Duration,
};

/// Largest UDP payload this listener sends, accepts or advertises. Tailscale's
/// TUN MTU is 1280, so an IPv4 payload above 1252 bytes (IPv6: 1232) is never
/// delivered, and the native dialer's endpoint receives at most 1200 bytes. A
/// 1500-byte limit split the real Tailscale certificate chain into 1438-byte
/// handshake datagrams that never arrived (live run, 2026-10-12). 1200 is
/// also the QUIC minimum, so a compliant client Initial is exactly this size.
pub(crate) const MAX_DATAGRAM: usize = 1200;
// IPv6 over the 1280-byte tailnet MTU carries at most 1280 - 40 - 8 bytes.
const _: () = assert!(MAX_DATAGRAM <= 1280 - 40 - 8);
// Upper bound on datagrams one receive call may take from this socket. Discovery
// still reads exactly one at a time (explicit `receive_batch(cx, 1)`), but the
// SAME socket then carries the established connection. With a cap of one, each
// bounded I/O turn could drain only a single datagram while a streaming viewer
// sends more than one per host turn. The kernel backlog then grows without
// limit, inflating ACK latency until a reliable record misses its deadline and
// the session closes. Sixteen matches the upstream handshake batch and the
// dialer's endpoint; buffers stay bounded (16 x `MAX_DATAGRAM + 1` bytes).
const RECEIVE_BATCH: usize = 16;
const MAX_SCAN: u16 = 256;
const MAX_TIMEOUT: Duration = Duration::from_secs(30);
/// Matches the bounded address list accepted from installed node metadata. This
/// is address selection for one peer, not concurrent connection demultiplexing.
pub const MAX_ADDRESSES: usize = 8;

/// No packet bytes, addresses, TLS diagnostics or certificate material in errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Configuration,
    Clock,
    Cancelled,
    Endpoint,
    InitialTimeout,
    InitialBudget,
    HandshakeTimeout,
    Handshake,
    PeerChanged,
    IdentityUnavailable,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "native-accept: {self:?}")
    }
}
impl std::error::Error for Error {}

/// Independent fixed acquisition and handshake budgets. Noise does not slide
/// either deadline. Allocation/read bounds also apply before TLS starts.
#[derive(Debug, Clone, Copy)]
pub struct Configuration {
    pub initial_timeout: Duration,
    pub handshake_timeout: Duration,
    pub max_initial_datagrams: u16,
    pub transport: Policy,
}
impl Default for Configuration {
    fn default() -> Self {
        Self {
            initial_timeout: Duration::from_secs(10),
            handshake_timeout: Duration::from_secs(10),
            max_initial_datagrams: 64,
            transport: Policy::default(),
        }
    }
}
impl Configuration {
    fn validate(self) -> Result<(), Error> {
        if self.initial_timeout.is_zero()
            || self.initial_timeout > MAX_TIMEOUT
            || self.handshake_timeout.is_zero()
            || self.handshake_timeout > MAX_TIMEOUT
            || !(1..=MAX_SCAN).contains(&self.max_initial_datagrams)
            || self.transport.stream_window != 65_536
            || self.transport.connection_window != 524_288
        {
            return Err(Error::Configuration);
        }
        self.transport.validate().map_err(|_| Error::Configuration)
    }
}

/// The same bounded, unidirectional stream profile as the canonical native
/// viewer dialer. Feed these bytes to the locally provisioned server identity;
/// never obtain a server certificate or trust roots from a connecting client.
pub fn transport_parameters() -> Vec<u8> {
    let mut bytes = Vec::new();
    // These fixed values are all in the QUIC varint domain.
    TransportParameters {
        max_udp_payload_size: Some(MAX_DATAGRAM as u64),
        initial_max_data: Some(524_288),
        initial_max_stream_data_bidi_local: Some(65_536),
        initial_max_stream_data_bidi_remote: Some(65_536),
        initial_max_stream_data_uni: Some(65_536),
        initial_max_streams_bidi: Some(0),
        // Half of MAX_STREAMS (quic.rs): lifetime unidirectional stream credit.
        initial_max_streams_uni: Some(32),
        max_datagram_frame_size: Some(1200),
        disable_active_migration: true,
        ..Default::default()
    }
    .encode(&mut bytes)
    .expect("fixed native transport parameters fit QUIC varints");
    bytes
}
fn native_configuration() -> NativeQuicConnectionConfig {
    NativeQuicConnectionConfig {
        max_local_bidi: 0,
        max_local_uni: 32,
        send_window: 65_536,
        recv_window: 65_536,
        connection_send_limit: 524_288,
        connection_recv_limit: 524_288,
        max_datagram_frame_size: 1200,
        ..Default::default()
    }
}

// Only invariant, unprotected header fields are inspected here. The upstream
// decoder validates the fixed bit and CID lengths; packet_len checks Length
// before any TLS work. A header-protection sample requires PN offset + 4 + 16.
// Nonempty tokens are not accepted: this listener issues no Retry or NEW_TOKEN.
fn initial(bytes: &[u8]) -> Option<(ConnectionId, ConnectionId)> {
    if !(1200..=MAX_DATAGRAM).contains(&bytes.len()) || bytes.first()? & 0xf0 != 0xc0 {
        return None;
    }
    let ProtectedHeaderPrefix::Long(header) = ProtectedHeaderPrefix::decode(bytes, 0).ok()? else {
        return None;
    };
    if header.version != 1
        || header.packet_type != LongPacketType::Initial
        || !(8..=20).contains(&header.dst_cid.len())
        || header.src_cid.is_empty()
        || !header.token.is_empty()
        || header.packet_len(bytes.len()).ok()? < header.packet_number_offset.checked_add(20)?
    {
        return None;
    }
    Some((header.dst_cid, header.src_cid))
}
fn now(cx: &Cx) -> Result<u64, Error> {
    cx.checkpoint().map_err(|_| Error::Cancelled)?;
    Ok(cx.timer_driver().ok_or(Error::Clock)?.now().as_nanos())
}
fn deadline(cx: &Cx, duration: Duration) -> Result<u64, Error> {
    now(cx)?
        .checked_add(u64::try_from(duration.as_nanos()).map_err(|_| Error::Configuration)?)
        .ok_or(Error::Clock)
}
fn remaining(cx: &Cx, until: u64, expired: Error) -> Result<Duration, Error> {
    until
        .checked_sub(now(cx)?)
        .filter(|&n| n != 0)
        .map(Duration::from_nanos)
        .ok_or(expired)
}

/// A bounded UDP socket, not a tailnet ingress proof or observation permission.
/// Bind only after the host's qualified interface restriction is in force. A
/// tailnet-looking IP address alone does NOT satisfy that requirement.
pub struct Listener {
    endpoints: Vec<QuicUdpEndpoint>,
    config: Configuration,
}
impl fmt::Debug for Listener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NativeListener([bounded exact-address sockets])")
    }
}
impl Listener {
    pub async fn bind(cx: &Cx, address: SocketAddr, config: Configuration) -> Result<Self, Error> {
        Self::bind_addresses(cx, &[address], config).await
    }
    /// Bind each exact local address before accepting any peer. No wildcard
    /// fallback is added. A failed bind drops every earlier socket, so a partial
    /// set is never silently published. The host must protect EVERY address
    /// independently before calling this method.
    pub async fn bind_addresses(
        cx: &Cx,
        addresses: &[SocketAddr],
        config: Configuration,
    ) -> Result<Self, Error> {
        config.validate()?;
        if addresses.is_empty()
            || addresses.len() > MAX_ADDRESSES
            || addresses
                .iter()
                .enumerate()
                .any(|(index, address)| addresses[..index].contains(address))
        {
            return Err(Error::Configuration);
        }
        now(cx)?;
        let mut endpoints = Vec::with_capacity(addresses.len());
        for &address in addresses {
            let endpoint = QuicUdpEndpoint::bind(
                cx,
                address,
                QuicUdpEndpointConfig {
                    // Sentinel byte detects truncation of oversized discovery
                    // datagrams. The later owner retains this same bounded socket.
                    max_packet_size: MAX_DATAGRAM + 1,
                    max_batch_size: RECEIVE_BATCH,
                    socket_recv_buffer_size: Some(65_536),
                    socket_send_buffer_size: Some(65_536),
                    enable_timestamping: false,
                },
            )
            .await
            .map_err(|_| {
                if cx.checkpoint().is_err() {
                    Error::Cancelled
                } else {
                    Error::Endpoint
                }
            })?;
            now(cx)?;
            endpoints.push(endpoint);
        }
        Ok(Self { endpoints, config })
    }
    /// First bound address, retained for single-address callers. Admission for
    /// a set must use `local_addresses` and the actual selected TLS endpoint.
    pub fn local_addr(&self) -> SocketAddr {
        self.endpoints[0].local_addr()
    }
    /// The complete fixed bind set, including the kernel-selected ports.
    pub fn local_addresses(&self) -> Vec<SocketAddr> {
        self.endpoints
            .iter()
            .map(QuicUdpEndpoint::local_addr)
            .collect()
    }
    /// Original local bounds, retained unchanged by a serial replacement.
    pub const fn configuration(&self) -> Configuration {
        self.config
    }
    /// Discover one candidate and establish TLS with that exact source/CID.
    /// The acquisition deadline starts NOW, not when the future is first polled.
    /// `identity` is called at most once, only after bounded header inspection.
    /// It must be a nonblocking local identity check/factory. It receives the
    /// fixed server transport parameters, never untrusted certificate material.
    /// Every I/O stage checks cancellation; the caller supplies a session Cx.
    pub fn accept<'a, F>(
        self,
        cx: &'a Cx,
        local_cid: ConnectionId,
        identity: F,
    ) -> Result<impl Future<Output = Result<NativeQuicUdpConnection, Error>> + 'a, Error>
    where
        F: FnOnce(Vec<u8>) -> Result<QuicHandshakeDriver, Error> + 'a,
    {
        self.accept_with_identity(cx, local_cid, move |_, parameters| {
            std::future::ready(identity(parameters))
        })
    }
    /// Refresh installed host identity AFTER a candidate arrives, without blocking
    /// the reactor or pinning a three-second metadata snapshot across idle time.
    /// The factory and TLS share ONE absolute handshake deadline. Expired or
    /// cancelled identity work is dropped; it cannot launch a late handshake.
    ///
    /// The peer address is untrusted UDP routing metadata until TLS completes.
    /// It is not an admission proof. Reauthorize the actual established endpoints
    /// before any application handoff. The factory is invoked at most once and
    /// must return a bounded, cancellation-aware future; no task is spawned here.
    pub fn accept_with_identity<'a, F, I>(
        self,
        cx: &'a Cx,
        local_cid: ConnectionId,
        identity: F,
    ) -> Result<impl Future<Output = Result<NativeQuicUdpConnection, Error>> + 'a, Error>
    where
        F: FnOnce(SocketAddr, Vec<u8>) -> I + 'a,
        I: Future<Output = Result<QuicHandshakeDriver, Error>> + 'a,
    {
        self.accept_with_local_identity(cx, local_cid, move |_, peer, parameters| {
            identity(peer, parameters)
        })
    }
    /// The selected exact local endpoint accompanies the candidate's untrusted
    /// peer address. A multi-address host uses it to refresh its own installed
    /// identity before TLS. Neither address is an application admission proof.
    pub fn accept_with_local_identity<'a, F, I>(
        self,
        cx: &'a Cx,
        local_cid: ConnectionId,
        identity: F,
    ) -> Result<impl Future<Output = Result<NativeQuicUdpConnection, Error>> + 'a, Error>
    where
        F: FnOnce(SocketAddr, SocketAddr, Vec<u8>) -> I + 'a,
        I: Future<Output = Result<QuicHandshakeDriver, Error>> + 'a,
    {
        if !(8..=20).contains(&local_cid.len()) {
            return Err(Error::Configuration);
        }
        let until = deadline(cx, self.config.initial_timeout)?;
        Ok(async move {
            let Self {
                mut endpoints,
                config,
            } = self;
            let mut candidate = None;
            let mut next = 0;
            for _ in 0..config.max_initial_datagrams {
                remaining(cx, until, Error::InitialTimeout)?;
                // Each pending receive borrows one original socket and keeps at
                // most one datagram. Rotating after EVERY packet prevents a noisy
                // IPv4 endpoint from starving an already-arrived IPv6 Initial.
                let mut reads: Vec<_> = endpoints
                    .iter_mut()
                    .map(|endpoint| Box::pin(endpoint.receive_batch(cx, 1)))
                    .collect();
                let received = poll_fn(|task| {
                    for offset in 0..reads.len() {
                        let index = (next + offset) % reads.len();
                        if let std::task::Poll::Ready(packets) = reads[index].as_mut().poll(task) {
                            return std::task::Poll::Ready(packets.map(|packets| (index, packets)));
                        }
                    }
                    std::task::Poll::Pending
                });
                let (index, packets) = timeout_at(Time::from_nanos(until), received)
                    .await
                    .map_err(|_| Error::InitialTimeout)?
                    .map_err(|_| {
                        if cx.checkpoint().is_err() {
                            Error::Cancelled
                        } else {
                            Error::Endpoint
                        }
                    })?;
                drop(reads);
                next = (index + 1) % endpoints.len();
                // Recheck AFTER the receive: a ready packet is not permission
                // to accept after the original acquisition budget expired.
                remaining(cx, until, Error::InitialTimeout)?;
                if let Some(packet) = packets.first()
                    && let Some((dcid, scid)) = initial(&packet.data)
                    && dcid != local_cid
                    && scid != local_cid
                {
                    candidate = Some((index, packet.src_addr, dcid, scid));
                    break;
                }
            }
            let (index, peer, dcid, scid) = candidate.ok_or(Error::InitialBudget)?;
            let endpoint = endpoints.swap_remove(index);
            let local = endpoint.local_addr();
            // Capacity remains ONE. No unused socket can acquire another peer
            // or hold a queued Initial while this connection is authenticated.
            drop(endpoints);
            // Factory invocation, all identity I/O and TLS consume this deadline.
            let until = deadline(cx, config.handshake_timeout)?;
            let driver =
                bounded_identity(cx, until, identity(local, peer, transport_parameters())).await?;
            remaining(cx, until, Error::HandshakeTimeout)?;
            let connection = timeout_at(
                Time::from_nanos(until),
                NativeQuicUdpConnection::accept(
                    cx,
                    endpoint,
                    driver,
                    dcid,
                    local_cid,
                    native_configuration(),
                    ALPN,
                ),
            )
            .await
            .map_err(|_| Error::HandshakeTimeout)?
            .map_err(|_| {
                if cx.checkpoint().is_err() {
                    Error::Cancelled
                } else {
                    Error::Handshake
                }
            })?;
            remaining(cx, until, Error::HandshakeTimeout)?;
            // A second source reusing routing metadata must NOT inherit the
            // selected candidate, even after completing TLS.
            if connection.peer_addr() != peer || connection.peer_connection_id() != scid {
                return Err(Error::PeerChanged);
            }
            Ok(connection)
        })
    }
}

async fn bounded_identity(
    cx: &Cx,
    until: u64,
    identity: impl Future<Output = Result<QuicHandshakeDriver, Error>>,
) -> Result<QuicHandshakeDriver, Error> {
    let mut identity = pin!(identity);
    let mut previous = now(cx)?;
    let checked = poll_fn(|task| {
        remaining(cx, until, Error::HandshakeTimeout)?;
        let current = now(cx)?;
        if current < previous {
            return std::task::Poll::Ready(Err(Error::Clock));
        }
        previous = current;
        let result = identity.as_mut().poll(task);
        // A ready factory may itself revoke its context or consume the budget.
        remaining(cx, until, Error::HandshakeTimeout)?;
        result
    });
    timeout_at(Time::from_nanos(until), checked)
        .await
        .map_err(|_| Error::HandshakeTimeout)?
}

#[cfg(test)]
#[path = "accept/tests.rs"]
mod tests;
