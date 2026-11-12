//! The one native client dial profile: strict-WebPKI TLS 1.3 with the
//! project ALPN, QUIC transport parameters, datagram bounds and failure
//! classification. Shared by the installed-daemon dialer (`local/dial.rs`,
//! Linux and macOS) and the named-destination dialer (`named.rs`, iOS), so a
//! client without `LocalAPI` cannot drift onto weaker TLS or transport limits.
use crate::Error;
use asupersync::{
    cx::Cx,
    net::{
        quic_core::{ConnectionId, TransportParameters},
        quic_native::{
            NativeQuicConnectionConfig, NativeQuicUdpConnectionError, QuicTlsError,
            QuicUdpEndpointConfig,
        },
    },
    time::sleep,
    tls::{Certificate, TlsConnector},
    types::Time,
};
use std::{
    future::{Future, poll_fn},
    io::Read,
    pin::pin,
    task::Poll,
    time::Duration,
};

pub(crate) const ALPN: &[u8] = b"fr-remote/0";
const MAX_ROOTS: usize = 256;
const MAX_ROOT_BYTES: usize = 1024 * 1024;
/// QUIC minimum datagram, below the tailnet MTU (1280) on IPv4 and IPv6.
const DATAGRAM_BYTES: usize = 1200;

/// Strict CA validation against locally provisioned roots only. Do not use the
/// upstream convenience `client_config`: its exact-leaf fallback is
/// intentionally unnecessary for this WebPKI-only path. No early data.
pub(crate) fn connector(roots: Vec<Certificate>, timeout: Duration) -> Result<TlsConnector, Error> {
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
    TlsConnector::builder()
        .with_strict_ca_validation()
        .add_root_certificates(roots)
        .alpn_protocols_required(vec![ALPN.to_vec()])
        .min_protocol_version(0x0304u16.into())
        .max_protocol_version(0x0304u16.into())
        .enable_early_data(false)
        .build()
        .map_err(|_| Error::InvalidTrustStore)
}

pub(crate) fn may_try_alternate(error: Error) -> bool {
    matches!(
        error,
        Error::NativeBind | Error::NativeRouteIo | Error::HandshakeTimeout
    )
}

/// Keep certificate rejection and the pinned stack's local UDP I/O failures
/// distinct. Only its typed, redaction-safe provider/code pair is read, never
/// an OS diagnostic or peer error string. All other TLS/protocol errors remain
/// terminal `NativeHandshake` failures.
pub(crate) fn handshake_error(error: &NativeQuicUdpConnectionError) -> Error {
    match error {
        NativeQuicUdpConnectionError::Handshake(
            QuicTlsError::ServerCertificateRejected { .. }
            | QuicTlsError::ServerCertificateChainEmpty
            | QuicTlsError::ServerCertificateUnverified,
        ) => Error::PeerCertificateRejected,
        NativeQuicUdpConnectionError::Handshake(QuicTlsError::CryptoProviderFailure {
            provider: "rustls-quic-handshake",
            code: "udp_send" | "udp_recv",
        }) => Error::NativeRouteIo,
        // The host's capacity-one listener is serving another viewer
        // (fr-0kyf). The other address family would not answer either.
        NativeQuicUdpConnectionError::Handshake(QuicTlsError::CryptoProviderFailure {
            provider: "rustls-quic-handshake",
            code: "peer_connection_refused",
        }) => Error::HostBusy,
        _ => Error::NativeHandshake,
    }
}

pub(crate) fn connection_ids() -> Result<(ConnectionId, ConnectionId), Error> {
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
pub(crate) fn endpoint_config() -> QuicUdpEndpointConfig {
    QuicUdpEndpointConfig {
        max_packet_size: DATAGRAM_BYTES,
        max_batch_size: 16,
        ..Default::default()
    }
}
pub(crate) fn connection_config() -> NativeQuicConnectionConfig {
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
pub(crate) fn transport_parameters() -> Result<Vec<u8>, Error> {
    let cfg = connection_config();
    let mut bytes = Vec::new();
    TransportParameters {
        initial_max_data: Some(cfg.connection_recv_limit),
        // The pinned native connection retains one conservative stream window,
        // taking the minimum of all three negotiated classes. Advertise equal
        // byte bounds; max_streams_bidi remains zero, so no bidi stream is admitted.
        initial_max_stream_data_bidi_local: Some(cfg.recv_window),
        initial_max_stream_data_bidi_remote: Some(cfg.recv_window),
        initial_max_stream_data_uni: Some(cfg.recv_window),
        initial_max_streams_bidi: Some(0),
        // fr-transport's MAX_STREAMS is twice this: lifetime unidirectional
        // credit, sized for repeated reference recoveries (each takes 3 pairs).
        initial_max_streams_uni: Some(32),
        max_datagram_frame_size: Some(1200),
        // The endpoint receives at most this many bytes per datagram
        // (`endpoint_config`); a peer must not send larger ones.
        max_udp_payload_size: Some(DATAGRAM_BYTES as u64),
        ..Default::default()
    }
    .encode(&mut bytes)
    .map_err(|_| Error::InvalidPolicy)?;
    Ok(bytes)
}

/// The runtime's monotonic clock in microseconds, after a cancellation check.
pub(crate) fn now(cx: &Cx) -> Result<u64, Error> {
    cx.checkpoint().map_err(|_| Error::Cancelled)?;
    Ok(cx
        .timer_driver()
        .ok_or(Error::MissingRuntime)?
        .now()
        .as_nanos()
        / 1000)
}

/// Pulse cancellation independently of a stalled read or handshake without
/// restarting that operation or accumulating timers. Dropping this future drops
/// the operation (and any socket it owns).
pub(crate) async fn bounded<T>(
    cx: &Cx,
    duration: Duration,
    operation: impl Future<Output = Result<T, Error>>,
) -> Result<T, Error> {
    let start = now(cx)?;
    let end = start
        .checked_add(u64::try_from(duration.as_micros()).map_err(|_| Error::InvalidPolicy)?)
        .ok_or(Error::Clock)?;
    let mut last = start;
    let mut pulse = Box::pin(sleep(
        Time::from_nanos(start.checked_mul(1000).ok_or(Error::Clock)?),
        Duration::from_millis(10).min(duration),
    ));
    let mut operation = pin!(operation);
    poll_fn(|task| {
        let current = match now(cx) {
            Ok(v) => v,
            Err(e) => return Poll::Ready(Err(e)),
        };
        if current < last {
            return Poll::Ready(Err(Error::Clock));
        }
        last = current;
        if current >= end {
            return Poll::Ready(Err(Error::Timeout));
        }
        let result = operation.as_mut().poll(task);
        // A poll can finish after the deadline or observe cancellation while
        // parsing a response. Check again before accepting output or waiting.
        let current = match now(cx) {
            Ok(v) => v,
            Err(e) => return Poll::Ready(Err(e)),
        };
        if current < last {
            return Poll::Ready(Err(Error::Clock));
        }
        last = current;
        if current >= end {
            return Poll::Ready(Err(Error::Timeout));
        }
        if let Poll::Ready(result) = result {
            return Poll::Ready(result);
        }
        if pulse.as_mut().poll(task).is_ready() {
            pulse = Box::pin(sleep(
                Time::from_nanos(current * 1000),
                Duration::from_micros((end - current).min(10_000)),
            ));
            // Time may have advanced during the operation poll, so this new
            // pulse can already be ready. Poll it on the next turn, where its
            // result is handled, rather than discarding Ready and repolling a
            // completed Sleep. The wake also ensures registration is not lost.
            task.waker().wake_by_ref();
        }
        Poll::Pending
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn certificate_rejection_is_distinct_from_other_tls_failures() {
        for rejected in [
            QuicTlsError::ServerCertificateRejected {
                code: "server_certificate_invalid",
            },
            QuicTlsError::ServerCertificateChainEmpty,
            QuicTlsError::ServerCertificateUnverified,
        ] {
            assert_eq!(
                handshake_error(&NativeQuicUdpConnectionError::Handshake(rejected)),
                Error::PeerCertificateRejected
            );
        }
        for other in [
            NativeQuicUdpConnectionError::Cancelled,
            NativeQuicUdpConnectionError::Handshake(QuicTlsError::InvalidServerName),
            NativeQuicUdpConnectionError::Handshake(QuicTlsError::ServerIdentityRootStoreEmpty),
            NativeQuicUdpConnectionError::HandshakeIncomplete("keys"),
            NativeQuicUdpConnectionError::AlpnMismatch {
                expected: b"fr-remote/0".to_vec(),
                negotiated: None,
            },
        ] {
            assert_eq!(handshake_error(&other), Error::NativeHandshake);
        }
    }

    #[test]
    fn a_refused_initial_is_a_busy_host_and_never_tries_the_other_family() {
        let refused = handshake_error(&NativeQuicUdpConnectionError::Handshake(
            QuicTlsError::CryptoProviderFailure {
                provider: "rustls-quic-handshake",
                code: "peer_connection_refused",
            },
        ));
        assert_eq!(refused, Error::HostBusy);
        assert!(!may_try_alternate(refused));
        // Any other close, or the code from another provider, stays a
        // terminal handshake failure.
        for (provider, code) in [
            ("rustls-quic-handshake", "peer_closed"),
            ("rustls", "peer_connection_refused"),
        ] {
            assert_eq!(
                handshake_error(&NativeQuicUdpConnectionError::Handshake(
                    QuicTlsError::CryptoProviderFailure { provider, code }
                )),
                Error::NativeHandshake
            );
        }
    }

    #[test]
    fn native_application_credit_is_nonzero_without_admitting_bidirectional_streams() {
        let parameters = TransportParameters::decode(&transport_parameters().unwrap()).unwrap();
        let bounds = [
            parameters.initial_max_stream_data_bidi_local.unwrap_or(0),
            parameters.initial_max_stream_data_bidi_remote.unwrap_or(0),
            parameters.initial_max_stream_data_uni.unwrap_or(0),
        ];
        assert_eq!(bounds, [connection_config().recv_window; 3]);
        assert_eq!(bounds.into_iter().min(), Some(65_536));
        assert_eq!(parameters.initial_max_streams_bidi, Some(0));
        assert_eq!(connection_config().max_local_bidi, 0);
        assert_eq!(parameters.initial_max_data, Some(524_288));
        // A peer is told the exact datagram size this endpoint can receive.
        assert_eq!(
            parameters.max_udp_payload_size,
            Some(endpoint_config().max_packet_size as u64)
        );
        assert!(endpoint_config().max_packet_size <= 1280 - 40 - 8);
    }

    #[test]
    fn the_connector_refuses_empty_oversized_and_out_of_range_policy() {
        assert_eq!(
            connector(Vec::new(), Duration::from_secs(5)).err(),
            Some(Error::InvalidTrustStore)
        );
        assert_eq!(
            connector(Vec::new(), Duration::from_millis(99)).err(),
            Some(Error::InvalidPolicy)
        );
        assert_eq!(
            connector(Vec::new(), Duration::from_secs(31)).err(),
            Some(Error::InvalidPolicy)
        );
    }
}
