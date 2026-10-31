//! Synthetic installed-daemon metadata and ephemeral CA. Real Unix HTTP and
//! (in the explicit namespace lane) real non-loopback-address UDP/TLS.
use super::*;
use crate::PeerSelector;
use asupersync::types::CancelKind;
use asupersync::{
    net::{
        quic_core::{ConnectionId, LongPacketType, ProtectedHeaderPrefix},
        quic_native::{
            NativeQuicUdpConnection, QuicUdpEndpoint,
            handshake_driver::{QuicHandshakeDriver, server_config},
        },
    },
    time::timeout,
    tls::Certificate,
};
use serde_json::{Value, json};
use std::{
    io::Write,
    os::unix::net::UnixListener,
    sync::atomic::AtomicUsize,
    thread::{self, JoinHandle},
};
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
};

fn metadata() -> Value {
    let key = format!("nodekey:{}", "2".repeat(64));
    let mut s = json!({"Version":"synthetic-dial-fixture-not-live-qualification", "BackendState":"Running",
        "TailscaleIPs":["100.64.0.1"], "CurrentTailnet":{"Name":"test.invalid","MagicDNSSuffix":"fixture.ts.net"},
        "Self":{"ID":"n-host","NodeID":1,"PublicKey":format!("nodekey:{}","1".repeat(64)),"UserID":7,
            "TailscaleIPs":["100.64.0.1"],"InNetworkMap":true},
        "Peer":{key.clone():{"ID":"n-peer","NodeID":2,"PublicKey":key,"UserID":7,
            "TailscaleIPs":["100.64.0.2"],"InNetworkMap":true}}});
    s["Self"]["DNSName"] = json!("local.fixture.ts.net.");
    peer_mut(&mut s)["DNSName"] = json!("remote.fixture.ts.net.");
    s
}
fn openssl(dir: &Path, args: &[&str]) {
    let out = Command::new("openssl")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "fixture OpenSSL: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
fn pki() -> &'static Path {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        use std::os::unix::fs::DirBuilderExt;
        let dir = std::env::temp_dir().join(format!("fr-dial-{}-{}", std::process::id(), super::super::wall_now().unwrap()));
        std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        openssl(&dir, &["req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes", "-keyout", "ca.key", "-out", "ca.pem", "-days", "2", "-subj", "/CN=Ephemeral dial test CA", "-addext", "basicConstraints=critical,CA:TRUE"]);
        for (label, name) in [("leaf", "remote.fixture.ts.net"), ("wrong", "wrong.fixture.ts.net")] {
            openssl(&dir, &["req", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes", "-keyout", &format!("{label}.key"), "-out", &format!("{label}.csr"), "-subj", "/CN=Ephemeral dial leaf"]);
            std::fs::write(dir.join("ext"), format!("subjectAltName=DNS:{name}\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\n")).unwrap();
            openssl(&dir, &["x509", "-req", "-in", &format!("{label}.csr"), "-CA", "ca.pem", "-CAkey", "ca.key", "-CAcreateserial", "-days", "2", "-extfile", "ext", "-out", &format!("{label}.pem")]);
            openssl(&dir, &["x509", "-in", &format!("{label}.pem"), "-outform", "DER", "-out", &format!("{label}.der")]);
            openssl(&dir, &["pkcs8", "-topk8", "-nocrypt", "-in", &format!("{label}.key"), "-outform", "DER", "-out", &format!("{label}.key.der")]);
        }
        dir
    })
}
fn roots() -> Vec<Certificate> {
    Certificate::from_pem(&std::fs::read(pki().join("ca.pem")).unwrap()).unwrap()
}
fn client(server: &Server, duration: Duration) -> NativeClient {
    NativeClient::new(server.client.clone(), roots(), duration).unwrap()
}
async fn target(server: &Server, cx: &Cx) -> crate::PeerTarget {
    server
        .client
        .peer_target(cx, PeerSelector::StableId("n-peer"))
        .await
        .unwrap()
}
fn route(port: u16) -> DialRoute {
    DialRoute {
        local: "100.64.0.1".parse().unwrap(),
        remote: format!("100.64.0.2:{port}").parse().unwrap(),
    }
}
fn dual_stack_metadata() -> Value {
    let mut value = metadata();
    value["TailscaleIPs"] = json!(["100.64.0.1", "fd7a:115c:a1e0::1"]);
    value["Self"]["TailscaleIPs"] = value["TailscaleIPs"].clone();
    peer_mut(&mut value)["TailscaleIPs"] = json!(["100.64.0.2", "fd7a:115c:a1e0::2"]);
    value
}
fn ipv6_route(port: u16) -> DialRoute {
    DialRoute {
        local: "fd7a:115c:a1e0::1".parse().unwrap(),
        remote: SocketAddr::new("fd7a:115c:a1e0::2".parse().unwrap(), port),
    }
}

#[test]
fn fallback_never_retries_identity_trust_protocol_or_authority_refusals() {
    assert!(may_try_alternate(Error::NativeBind));
    assert!(may_try_alternate(Error::NativeRouteIo));
    assert!(may_try_alternate(Error::HandshakeTimeout));
    for refusal in [
        Error::InvalidPolicy,
        Error::InvalidEndpoint,
        Error::MissingRuntime,
        Error::Busy,
        Error::Cancelled,
        Error::Timeout,
        Error::LocalApiUnavailable,
        Error::UntrustedLocalApi,
        Error::Http,
        Error::LocalApiDenied,
        Error::MalformedMetadata,
        Error::SnapshotChanged,
        Error::BackendNotRunning,
        Error::AddressMismatch,
        Error::SharedPeer,
        Error::IdentityMismatch,
        Error::ScopeDenied,
        Error::ExplicitScopeRequired,
        Error::MachineNotAuthorized,
        Error::TailnetMembershipUnverifiable,
        Error::CapabilityDenied,
        Error::InvalidCapability,
        Error::CertificateRejected,
        Error::CertificateNotDue,
        Error::InvalidTrustStore,
        Error::NativeHandshake,
        Error::PeerCertificateRejected,
        Error::EntropyUnavailable,
        Error::KeyExpired,
        Error::Clock,
        Error::Expired,
        Error::IdentityChanged,
        Error::Revoked,
    ] {
        assert!(
            !may_try_alternate(refusal),
            "{refusal:?} must remain terminal"
        );
    }
}

#[test]
fn only_the_pinned_handshake_udp_io_codes_allow_family_fallback() {
    for code in ["udp_send", "udp_recv"] {
        let failure = QuicTlsError::CryptoProviderFailure {
            provider: "rustls-quic-handshake",
            code,
        };
        let classified = handshake_error(&NativeQuicUdpConnectionError::Handshake(failure));
        assert_eq!(classified, Error::NativeRouteIo);
        assert!(may_try_alternate(classified));
        assert_eq!(classified.to_string(), "NativeRouteIo");
        assert_eq!(
            serde_json::to_string(&classified).unwrap(),
            "\"NativeRouteIo\""
        );
    }
    for (provider, code) in [
        ("rustls", "udp_send"),
        ("rustls", "udp_recv"),
        ("rustls-quic-handshake ", "udp_send"),
        ("", "udp_recv"),
        ("rustls-quic-handshake", "udp_send_timeout"),
        ("rustls-quic-handshake", "udp_receive"),
        ("rustls-quic-handshake", "peer_closed"),
        ("rustls-quic-handshake", "peer_closed_crypto_error"),
        ("rustls-quic-handshake", "client_handshake_incomplete"),
        ("rustls-quic-handshake", ""),
    ] {
        let failure = QuicTlsError::CryptoProviderFailure { provider, code };
        let classified = handshake_error(&NativeQuicUdpConnectionError::Handshake(failure));
        assert_eq!(classified, Error::NativeHandshake, "{provider}/{code}");
        assert!(!may_try_alternate(classified));
    }
}

#[test]
fn fallback_validates_both_pairs_before_claiming_the_shared_slot() {
    let server = Server::new(
        vec![response(&dual_stack_metadata(), false)],
        Duration::ZERO,
    );
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let dialer = client(&server, Duration::from_secs(5));
        for alternate in [
            route(8443),
            ipv6_route(9443),
            DialRoute {
                remote: "[fd7a:115c:a1e0::3]:8443".parse().unwrap(),
                ..ipv6_route(8443)
            },
            DialRoute {
                local: "fd7a:115c:a1e0::9".parse().unwrap(),
                ..ipv6_route(8443)
            },
        ] {
            let target = target(&server, &cx).await;
            let calls = server.calls.load(Ordering::SeqCst);
            assert!(matches!(
                dialer.dial_with_fallback(&cx, target, route(8443), Some(alternate)),
                Err(Error::AddressMismatch)
            ));
            assert_eq!(calls, server.calls.load(Ordering::SeqCst));
        }
        let first = dialer
            .dial_with_fallback(
                &cx,
                target(&server, &cx).await,
                route(8443),
                Some(ipv6_route(8443)),
            )
            .unwrap();
        assert!(matches!(
            dialer
                .clone()
                .dial(&cx, target(&server, &cx).await, ipv6_route(8443)),
            Err(Error::Busy)
        ));
        drop(first);
        drop(
            dialer
                .dial(&cx, target(&server, &cx).await, ipv6_route(8443))
                .unwrap(),
        );
    });
}

#[test]
fn fallback_does_not_restart_an_unpolled_deadline_or_ignore_cancellation() {
    let server = Server::new(
        vec![response(&dual_stack_metadata(), false)],
        Duration::ZERO,
    );
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let dialer = client(&server, Duration::from_millis(100));
        let pending = dialer
            .dial_with_fallback(
                &cx,
                target(&server, &cx).await,
                route(8443),
                Some(ipv6_route(8443)),
            )
            .unwrap();
        let calls = server.calls.load(Ordering::SeqCst);
        sleep(cx.now(), Duration::from_millis(150)).await;
        assert!(matches!(pending.await, Err(Error::Timeout)));
        assert_eq!(calls, server.calls.load(Ordering::SeqCst));
        let pending = dialer
            .dial_with_fallback(
                &cx,
                target(&server, &cx).await,
                route(8443),
                Some(ipv6_route(8443)),
            )
            .unwrap();
        let calls = server.calls.load(Ordering::SeqCst);
        cx.cancel_fast(CancelKind::User);
        assert!(matches!(pending.await, Err(Error::Cancelled)));
        assert_eq!(calls, server.calls.load(Ordering::SeqCst));
    });
}
async fn both<A: Future, B: Future>(a: A, b: B) -> (A::Output, B::Output) {
    let (mut a, mut b) = (pin!(a), pin!(b));
    let (mut ar, mut br) = (None, None);
    poll_fn(|cx| {
        if ar.is_none()
            && let Poll::Ready(v) = a.as_mut().poll(cx)
        {
            ar = Some(v);
        }
        if br.is_none()
            && let Poll::Ready(v) = b.as_mut().poll(cx)
        {
            br = Some(v);
        }
        if ar.is_some() && br.is_some() {
            Poll::Ready((ar.take().unwrap(), br.take().unwrap()))
        } else {
            Poll::Pending
        }
    })
    .await
}

#[test]
fn outbound_trust_requires_ca_roots_and_has_no_peer_leaf_pinning() {
    let api = LocalApi::installed();
    assert!(matches!(
        NativeClient::new(api.clone(), vec![], Duration::from_secs(5)),
        Err(Error::InvalidTrustStore)
    ));
    let leaf = Certificate::from_pem(&std::fs::read(pki().join("leaf.pem")).unwrap()).unwrap();
    assert!(matches!(
        NativeClient::new(api.clone(), leaf, Duration::from_secs(5)),
        Err(Error::InvalidTrustStore)
    ));
    assert!(matches!(
        NativeClient::new(api.clone(), roots(), Duration::ZERO),
        Err(Error::InvalidPolicy)
    ));
    assert!(matches!(
        NativeClient::new(api.clone(), roots(), Duration::from_secs(31)),
        Err(Error::InvalidPolicy)
    ));
    assert!(matches!(
        NativeClient::new(api, vec![roots().remove(0); 257], Duration::from_secs(5)),
        Err(Error::InvalidTrustStore)
    ));
}
#[test]
fn invalid_routes_cannot_allocate_a_socket_or_requery_metadata() {
    let server = Server::new(vec![response(&metadata(), false)], Duration::ZERO);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let dialer = client(&server, Duration::from_secs(3));
        for r in [
            route(0),
            DialRoute {
                local: "0.0.0.0".parse().unwrap(),
                ..route(8443)
            },
            DialRoute {
                remote: "100.64.0.3:8443".parse().unwrap(),
                ..route(8443)
            },
            DialRoute {
                remote: "[::ffff:100.64.0.2]:8443".parse().unwrap(),
                ..route(8443)
            },
        ] {
            let t = target(&server, &cx).await;
            let calls = server.calls.load(Ordering::SeqCst);
            assert!(matches!(
                dialer.dial(&cx, t, r),
                Err(Error::AddressMismatch)
            ));
            assert_eq!(calls, server.calls.load(Ordering::SeqCst));
        }
    });
}
#[test]
fn dial_slot_is_claimed_before_poll_and_shared_across_clones() {
    let server = Server::new(vec![response(&metadata(), false)], Duration::ZERO);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let dialer = client(&server, Duration::from_secs(3));
        let first = dialer
            .dial(&cx, target(&server, &cx).await, route(8443))
            .unwrap();
        let second = dialer.clone();
        assert!(matches!(
            second.dial(&cx, target(&server, &cx).await, route(8443)),
            Err(Error::Busy)
        ));
        drop(first);
        drop(
            second
                .dial(&cx, target(&server, &cx).await, route(8443))
                .unwrap(),
        );
    });
}
#[test]
fn unpolled_time_is_part_of_the_original_dial_deadline() {
    let server = Server::new(vec![response(&metadata(), false)], Duration::ZERO);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let dialer = client(&server, Duration::from_millis(100));
        let f = dialer
            .dial(&cx, target(&server, &cx).await, route(8443))
            .unwrap();
        let calls = server.calls.load(Ordering::SeqCst);
        sleep(cx.now(), Duration::from_millis(150)).await;
        assert!(matches!(f.await, Err(Error::Timeout)));
        assert_eq!(calls, server.calls.load(Ordering::SeqCst));
        drop(
            dialer
                .dial(&cx, target(&server, &cx).await, route(8443))
                .unwrap(),
        );
    });
}
#[test]
fn foreign_or_expired_target_and_bad_roots_never_start_a_handshake() {
    let server = Server::new(vec![response(&metadata(), false)], Duration::ZERO);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let foreign = NativeClient::new(
            LocalApi::new(&*server.client.path).unwrap(),
            roots(),
            Duration::from_secs(3),
        )
        .unwrap();
        assert!(matches!(
            foreign.dial(&cx, target(&server, &cx).await, route(8443)),
            Err(Error::IdentityMismatch)
        ));
        let dialer = client(&server, Duration::from_secs(3));
        let mut old = target(&server, &cx).await;
        old.expires_us = now(&cx).unwrap();
        assert!(matches!(
            dialer.dial(&cx, old, route(8443)),
            Err(Error::Expired)
        ));
        assert!(!format!("{dialer:?} {:?}", route(8443)).contains("100.64"));
    });
}

// Test-only listener: consume the first Initial to learn its random DCID, then
// let the real client retransmit it. This intentionally exercises a lost flight;
// it is NOT presented as the missing production multiplexing/ingress listener.
fn initial_destination(data: &[u8]) -> Result<ConnectionId, ()> {
    // The low first-byte bits and packet number are still protected. Reading
    // them with PacketHeader::decode rejects valid flights whenever the random
    // protection mask sets a reserved bit, before TLS can authenticate them.
    let ProtectedHeaderPrefix::Long(header) =
        ProtectedHeaderPrefix::decode(data, 0).map_err(|_| ())?
    else {
        return Err(());
    };
    if header.packet_type != LongPacketType::Initial
        || header.version != 1
        || header.packet_len(data.len()).is_err()
    {
        return Err(());
    }
    Ok(header.dst_cid)
}

#[test]
fn discovery_reads_only_invariant_fields_for_every_protected_header_mask() {
    use asupersync::net::quic_core::{LongHeader, PacketHeader};

    let destination = ConnectionId::new(b"initial-destination").unwrap();
    let mut data = Vec::new();
    PacketHeader::Long(LongHeader {
        packet_type: LongPacketType::Initial,
        version: 1,
        dst_cid: destination,
        src_cid: ConnectionId::new(b"initial-source").unwrap(),
        token: Vec::new(),
        payload_length: 32,
        packet_number: 0,
        packet_number_len: 1,
    })
    .encode(&mut data)
    .unwrap();
    data.extend_from_slice(&[0; 31]);
    for mask in 0..16 {
        data[0] = 0xc0 | mask;
        assert_eq!(initial_destination(&data), Ok(destination));
        if mask & 0x0c != 0 {
            assert!(PacketHeader::decode(&data, 0).is_err());
        }
    }
    assert!(initial_destination(&data[..data.len() - 1]).is_err());
    data[0] = 0xe0;
    assert!(initial_destination(&data).is_err());
}

async fn accept(
    cx: &Cx,
    mut endpoint: QuicUdpEndpoint,
    leaf: &str,
    delay: Duration,
) -> Result<NativeQuicUdpConnection, ()> {
    let packets = endpoint.receive_batch(cx, 1).await.map_err(|_| ())?;
    let destination = initial_destination(&packets[0].data)?;
    sleep(cx.now(), delay).await;
    let config = server_config(
        vec![
            std::fs::read(pki().join(format!("{leaf}.der")))
                .unwrap()
                .into(),
        ],
        std::fs::read(pki().join(format!("{leaf}.key.der")))
            .unwrap()
            .try_into()
            .unwrap(),
        vec![b"fr-remote/0".to_vec()],
    )
    .unwrap();
    let driver =
        QuicHandshakeDriver::server(config, super::super::dial::transport_parameters().unwrap())
            .unwrap();
    NativeQuicUdpConnection::accept(
        cx,
        endpoint,
        driver,
        destination,
        ConnectionId::new(b"dial-test-server").unwrap(),
        super::super::dial::connection_config(),
        b"fr-remote/0",
    )
    .await
    .map_err(|_| ())
}
async fn endpoint(cx: &Cx) -> QuicUdpEndpoint {
    QuicUdpEndpoint::bind(
        cx,
        "100.64.0.2:0".parse().unwrap(),
        super::super::dial::endpoint_config(),
    )
    .await
    .expect("explicit namespace lane needs 100.64.0.1/32 and 100.64.0.2/32 assigned")
}
#[test]
#[ignore = "explicit network namespace lane: scripts/verify-native-dial.sh"]
fn native_dial_revalidates_during_lost_flight_and_returns_the_exact_tls_connection() {
    let server = Server::new(vec![response(&metadata(), false)], Duration::ZERO);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let dialer = client(&server, Duration::from_secs(8));
        let endpoint = endpoint(&cx).await;
        let port = endpoint.local_addr().port();
        let started = now(&cx).unwrap();
        let (connected, remote) = Box::pin(timeout(
            cx.now(),
            Duration::from_secs(9),
            both(
                dialer
                    .dial(&cx, target(&server, &cx).await, route(port))
                    .unwrap(),
                accept(&cx, endpoint, "leaf", Duration::from_millis(1700)),
            ),
        ))
        .await
        .unwrap();
        let peer = connected.unwrap();
        let remote = remote.unwrap();
        assert!(
            server.calls.load(Ordering::SeqCst) >= 10,
            "metadata must renew during the real handshake"
        );
        assert!(peer.target().issued_us() > started + 1_000_000);
        assert_eq!(peer.target().certificate_name(), "remote.fixture.ts.net");
        let native = peer.into_connection(&cx).unwrap();
        assert_eq!(native.peer_addr(), remote.local_addr());
        assert_eq!(native.local_addr(), remote.peer_addr());
        assert_eq!(native.negotiated_alpn(), b"fr-remote/0");
        assert!(native.connection().can_send_app_data());
        assert!(
            native.connection().inner().streams().is_empty(),
            "dial must not send ClientHello or observations"
        );
    });
}
#[test]
#[ignore = "explicit network namespace lane: scripts/verify-native-dial.sh"]
fn native_dial_rejects_wrong_certificate_name_without_exposing_connection() {
    let server = Server::new(vec![response(&metadata(), false)], Duration::ZERO);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let dialer = client(&server, Duration::from_secs(4));
        let endpoint = endpoint(&cx).await;
        let port = endpoint.local_addr().port();
        let (result, _) = Box::pin(both(
            dialer
                .dial(&cx, target(&server, &cx).await, route(port))
                .unwrap(),
            timeout(
                cx.now(),
                Duration::from_secs(4),
                accept(&cx, endpoint, "wrong", Duration::ZERO),
            ),
        ))
        .await;
        // A valid protected Initial must reach the real TLS verifier. A timeout
        // is not certificate-rejection evidence: the old fixture's unprotected
        // header decoder could reject the flight before TLS even started.
        assert!(
            matches!(result, Err(Error::PeerCertificateRejected)),
            "{:?}",
            result.err()
        );
    });
}
#[test]
#[ignore = "explicit network namespace lane: scripts/verify-native-dial.sh"]
fn native_dial_cannot_survive_node_reassignment_during_tls() {
    let original = metadata();
    let mut changed = original.clone();
    peer_mut(&mut changed)["PublicKey"] = json!(format!("nodekey:{}", "3".repeat(64)));
    let mut rows = vec![response(&original, false); 4];
    rows.push(response(&changed, false));
    let server = Server::new(rows, Duration::ZERO);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let dialer = client(&server, Duration::from_secs(3));
        let target = target(&server, &cx).await;
        let result = dialer.dial(&cx, target, route(8443)).unwrap().await;
        assert!(matches!(
            result,
            Err(Error::IdentityMismatch | Error::IdentityChanged)
        ));
    });
}
#[test]
#[ignore = "explicit network namespace lane: scripts/verify-native-dial.sh"]
fn native_dial_timeout_and_abandonment_release_socket_and_slot() {
    let server = Server::new(vec![response(&metadata(), false)], Duration::ZERO);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let dialer = client(&server, Duration::from_millis(150));
        let mut waiting = Box::pin(
            dialer
                .dial(&cx, target(&server, &cx).await, route(8443))
                .unwrap(),
        );
        poll_fn(|task| {
            assert!(waiting.as_mut().poll(task).is_pending());
            Poll::Ready(())
        })
        .await;
        drop(waiting);
        let r = dialer
            .dial(&cx, target(&server, &cx).await, route(8443))
            .unwrap()
            .await;
        assert!(matches!(
            r,
            Err(Error::HandshakeTimeout | Error::NativeRouteIo)
        ));
        drop(
            dialer
                .dial(&cx, target(&server, &cx).await, route(8443))
                .unwrap(),
        );
    });
}

#[test]
#[ignore = "explicit network namespace lane: scripts/verify-native-dial.sh"]
fn native_dial_ipv6_uses_the_selected_node_addresses_and_expired_handoff_refuses() {
    let mut data = metadata();
    data["Self"]["TailscaleIPs"] = json!(["fd7a:115c:a1e0::1"]);
    data["TailscaleIPs"] = json!(["fd7a:115c:a1e0::1"]);
    peer_mut(&mut data)["TailscaleIPs"] = json!(["fd7a:115c:a1e0::2"]);
    let server = Server::new(vec![response(&data, false)], Duration::ZERO);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let dialer = client(&server, Duration::from_secs(5));
        let endpoint = QuicUdpEndpoint::bind(
            &cx,
            "[fd7a:115c:a1e0::2]:0".parse().unwrap(),
            super::super::dial::endpoint_config(),
        )
        .await
        .unwrap();
        let r = DialRoute {
            local: "fd7a:115c:a1e0::1".parse().unwrap(),
            remote: endpoint.local_addr(),
        };
        let (connection, remote) = Box::pin(both(
            dialer.dial(&cx, target(&server, &cx).await, r).unwrap(),
            accept(&cx, endpoint, "leaf", Duration::ZERO),
        ))
        .await;
        let peer = connection.unwrap();
        assert!(remote.unwrap().local_addr().is_ipv6());
        sleep(cx.now(), Duration::from_millis(3100)).await;
        assert!(matches!(peer.into_connection(&cx), Err(Error::Expired)));
    });
}

#[test]
#[ignore = "explicit network namespace lane: scripts/verify-native-dial.sh"]
fn native_dial_falls_back_to_ipv6_with_one_original_timeout_and_no_application_replay() {
    fallback_after_silent_ipv4(Duration::from_secs(6));
}

#[test]
#[ignore = "explicit network namespace lane: scripts/verify-native-dial.sh"]
fn native_dial_inspection_budget_reaches_ipv6_after_a_silent_ipv4() {
    // The five-second inspection operation reserves at most four seconds for
    // dialing. Each family must have time for the pinned stack's 1.5 s Initial
    // PTO: the cold listener necessarily consumes its discovery packet. A 3 s
    // total splits into two 1.5 s attempts, neither of which can complete.
    fallback_after_silent_ipv4(Duration::from_secs(4));
}

fn fallback_after_silent_ipv4(budget: Duration) {
    let server = Server::new(
        vec![response(&dual_stack_metadata(), false)],
        Duration::ZERO,
    );
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let dialer = client(&server, budget);
        // The preferred endpoint receives UDP but never services a handshake.
        // Keeping it bound avoids turning silence into an ICMP/refusal fixture.
        let silent = endpoint(&cx).await;
        let port = silent.local_addr().port();
        let alternate = QuicUdpEndpoint::bind(&cx, ipv6_route(port).remote, endpoint_config())
            .await
            .unwrap();
        let started = now(&cx).unwrap();
        let (result, remote) = Box::pin(timeout(
            cx.now(),
            budget + Duration::from_secs(1),
            both(
                dialer
                    .dial_with_fallback(
                        &cx,
                        target(&server, &cx).await,
                        route(port),
                        Some(ipv6_route(port)),
                    )
                    .unwrap(),
                accept(&cx, alternate, "leaf", Duration::ZERO),
            ),
        ))
        .await
        .unwrap();
        let native = result.unwrap().into_connection(&cx).unwrap();
        let remote = remote.unwrap();
        assert_eq!(native.peer_addr(), ipv6_route(port).remote);
        assert_eq!(native.peer_addr(), remote.local_addr());
        assert_eq!(native.local_addr().ip(), ipv6_route(port).local);
        assert!(u128::from(now(&cx).unwrap() - started) < budget.as_micros());
        assert!(
            server.calls.load(Ordering::SeqCst) >= 10,
            "metadata renews through both handshakes"
        );
        assert!(
            native.connection().inner().streams().is_empty(),
            "no application action exists to replay"
        );
        drop(silent);
    });
}

#[test]
#[ignore = "explicit network namespace lane: scripts/verify-native-dial.sh"]
fn native_dial_falls_back_after_immediate_route_io_without_application_replay() {
    let mut data = dual_stack_metadata();
    // The isolated runner assigns only .1 and .2, with no route to .3.
    // Bind succeeds locally; the real UDP send must report a route I/O error.
    peer_mut(&mut data)["TailscaleIPs"] = json!(["100.64.0.3", "fd7a:115c:a1e0::2"]);
    let server = Server::new(vec![response(&data, false)], Duration::ZERO);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let dialer = client(&server, Duration::from_secs(6));
        let alternate = QuicUdpEndpoint::bind(
            &cx,
            "[fd7a:115c:a1e0::2]:0".parse().unwrap(),
            endpoint_config(),
        )
        .await
        .unwrap();
        let port = alternate.local_addr().port();
        let preferred = DialRoute {
            remote: SocketAddr::new("100.64.0.3".parse().unwrap(), port),
            ..route(port)
        };
        assert!(matches!(
            dialer
                .dial(&cx, target(&server, &cx).await, preferred)
                .unwrap()
                .await,
            Err(Error::NativeRouteIo)
        ));
        // The failed real attempt released its slot. The fallback operation
        // owns the same shared slot from call time through both address pairs.
        let selected = target(&server, &cx).await;
        let started = now(&cx).unwrap();
        let work = dialer
            .dial_with_fallback(&cx, selected, preferred, Some(ipv6_route(port)))
            .unwrap();
        let contender = dialer.clone();
        assert!(matches!(
            contender.dial(&cx, target(&server, &cx).await, preferred),
            Err(Error::Busy)
        ));
        let (result, remote) = Box::pin(timeout(
            cx.now(),
            Duration::from_secs(7),
            both(work, accept(&cx, alternate, "leaf", Duration::ZERO)),
        ))
        .await
        .unwrap();
        let native = result.unwrap().into_connection(&cx).unwrap();
        let remote = remote.unwrap();
        assert_eq!(native.peer_addr(), ipv6_route(port).remote);
        assert_eq!(native.peer_addr(), remote.local_addr());
        assert_eq!(native.local_addr(), remote.peer_addr());
        assert_eq!(native.local_addr().ip(), ipv6_route(port).local);
        assert_eq!(native.negotiated_alpn(), b"fr-remote/0");
        assert!(native.connection().can_send_app_data());
        assert!(now(&cx).unwrap() - started < 6_000_000);
        assert!(native.connection().inner().streams().is_empty());
        assert!(remote.connection().inner().streams().is_empty());
        drop(
            contender
                .dial(&cx, target(&server, &cx).await, preferred)
                .unwrap(),
        );
    });
}

#[test]
fn caller_cancellation_stops_dial_without_leaving_the_shared_slot_busy() {
    let server = Server::new(vec![response(&metadata(), false)], Duration::ZERO);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let dialer = client(&server, Duration::from_secs(5));
        let f = dialer
            .dial(&cx, target(&server, &cx).await, route(8443))
            .unwrap();
        cx.cancel_fast(CancelKind::User);
        assert!(matches!(f.await, Err(Error::Cancelled)));
        // The test's context is intentionally terminal. The implementation's
        // unpolled-drop test separately proves slot reclamation with a live one.
    });
}

fn peer_mut(s: &mut Value) -> &mut Value {
    s["Peer"]
        .as_object_mut()
        .unwrap()
        .values_mut()
        .next()
        .unwrap()
}
fn response(value: &Value, _chunked: bool) -> Vec<u8> {
    let body = serde_json::to_vec(value).unwrap();
    let mut data = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).into_bytes();
    data.extend(body);
    data
}
fn runtime() -> asupersync::runtime::Runtime {
    asupersync::runtime::RuntimeBuilder::new()
        .worker_threads(2)
        .enable_platform_reactor(true)
        .build()
        .unwrap()
}
struct Server {
    client: LocalApi,
    stop: Arc<AtomicBool>,
    calls: Arc<AtomicUsize>,
    task: Option<JoinHandle<()>>,
}
impl Server {
    fn new(replies: Vec<Vec<u8>>, pause: Duration) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = pki().join(format!("api-{}", NEXT.fetch_add(1, Ordering::SeqCst)));
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let (ended, counted) = (stop.clone(), calls.clone());
        let task = thread::spawn(move || {
            let mut index = 0;
            while !ended.load(Ordering::Acquire) {
                let (mut socket, _) = match listener.accept() {
                    Ok(value) => value,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(e) => panic!("fixture accept: {e}"),
                };
                socket
                    .set_read_timeout(Some(Duration::from_millis(300)))
                    .unwrap();
                let mut request = Vec::new();
                let mut byte = [0];
                while request.len() < 2048 && !request.ends_with(b"\r\n\r\n") {
                    match socket.read(&mut byte) {
                        Ok(1) => request.push(byte[0]),
                        _ => break,
                    }
                }
                if request.is_empty() {
                    continue;
                }
                let text = String::from_utf8(request).unwrap();
                assert!(text.starts_with("GET /localapi/v0/status?peers=true "));
                assert!(text.contains("Host: local-tailscaled.sock\r\n"));
                counted.fetch_add(1, Ordering::SeqCst);
                thread::sleep(pause);
                let _ = socket.write_all(&replies[index.min(replies.len() - 1)]);
                index += 1;
            }
        });
        let mut client = LocalApi::new(path).unwrap();
        client.daemon_uid = asupersync::net::unix::UnixStream::pair()
            .unwrap()
            .0
            .peer_cred()
            .unwrap()
            .uid;
        Self {
            client,
            stop,
            calls,
            task: Some(task),
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.task.take().unwrap().join().unwrap();
    }
}
