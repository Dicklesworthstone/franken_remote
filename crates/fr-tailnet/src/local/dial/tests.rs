//! Synthetic installed-daemon metadata and ephemeral CA. Real Unix HTTP and
//! (in the explicit namespace lane) real non-loopback-address UDP/TLS.
use super::*;
use crate::PeerSelector;
use asupersync::types::CancelKind;
use asupersync::{
    net::{
        quic_core::{ConnectionId, PacketHeader},
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
async fn accept(
    cx: &Cx,
    mut endpoint: QuicUdpEndpoint,
    leaf: &str,
    delay: Duration,
) -> Result<NativeQuicUdpConnection, ()> {
    let packets = endpoint.receive_batch(cx, 1).await.map_err(|_| ())?;
    let (PacketHeader::Long(header), _) =
        PacketHeader::decode(&packets[0].data, 0).map_err(|_| ())?
    else {
        return Err(());
    };
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
        header.dst_cid,
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
        assert!(matches!(
            result,
            Err(Error::NativeHandshake | Error::Timeout)
        ));
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
        assert!(matches!(r, Err(Error::Timeout | Error::NativeHandshake)));
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
