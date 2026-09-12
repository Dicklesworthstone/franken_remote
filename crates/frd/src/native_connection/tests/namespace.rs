//! Explicit isolated network namespace. Uses the PUBLIC `Client::run` unchanged:
//! real credential-checked Unix HTTP, exact non-loopback socket addresses, real
//! TLS and production startup/renewal. Identity, CA and host admission are fixtures.
use super::*;
use asupersync::{
    net::{
        quic_core::{ConnectionId, PacketHeader, TransportParameters},
        quic_native::{
            NativeQuicConnectionConfig, QuicUdpEndpoint, QuicUdpEndpointConfig,
            handshake_driver::{QuicHandshakeDriver, server_config},
        },
    },
    time::{sleep, timeout},
};
use std::{
    io::{Read, Write},
    os::unix::net::UnixListener,
    path::{Path, PathBuf},
    process::Command,
    sync::{OnceLock, atomic::AtomicUsize},
    thread::{self, JoinHandle},
};
const ALPN: &[u8] = fr_transport::quic::ALPN;
fn metadata(changed: bool) -> String {
    format!(
        r#"{{"Version":"synthetic-viewer-fixture-not-live-qualification","BackendState":"Running",
"TailscaleIPs":["100.64.0.1"],"CurrentTailnet":{{"Name":"test.invalid","MagicDNSSuffix":"fixture.ts.net"}},
"Self":{{"ID":"n-host","NodeID":1,"PublicKey":"nodekey:{one}","UserID":7,"TailscaleIPs":["100.64.0.1"],"InNetworkMap":true,"DNSName":"local.fixture.ts.net."}},
"Peer":{{"nodekey:{two}":{{"ID":"n-peer","NodeID":2,"PublicKey":"nodekey:{two}","UserID":7,"TailscaleIPs":["100.64.0.2"],"InNetworkMap":true,"DNSName":"{name}.fixture.ts.net."}}}}}}"#,
        one = "1".repeat(64),
        two = "2".repeat(64),
        name = if changed { "replaced" } else { "remote" }
    )
}
fn response(changed: bool) -> Vec<u8> {
    let body = metadata(changed);
    let mut bytes = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).into_bytes();
    bytes.extend(body.bytes());
    bytes
}
fn pki() -> &'static Path {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let dir = network::pki();
        let run = |args: &[&str]| {
            let result = Command::new("openssl").current_dir(dir).args(args).output().unwrap();
            assert!(result.status.success(), "fixture OpenSSL failed: {}", String::from_utf8_lossy(&result.stderr));
        };
        run(&["req","-newkey","ec","-pkeyopt","ec_paramgen_curve:P-256","-nodes","-keyout","target.key","-out","target.csr","-subj","/CN=Remote fixture"]);
        std::fs::write(dir.join("target.ext"), "subjectAltName=DNS:remote.fixture.ts.net\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\n").unwrap();
        run(&["x509","-req","-in","target.csr","-CA","ca.pem","-CAkey","ca.key","-CAcreateserial","-days","1","-extfile","target.ext","-out","target.pem"]);
        run(&["x509","-in","target.pem","-outform","DER","-out","target.der"]);
        run(&["pkcs8","-topk8","-nocrypt","-in","target.key","-outform","DER","-out","target.key.der"]);
        dir.to_owned()
    })
}
struct Api {
    client: LocalApi,
    changed: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    calls: Arc<AtomicUsize>,
    worker: Option<JoinHandle<()>>,
}
impl Api {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = pki().join(format!(
            "viewer-api-{}",
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let server = UnixListener::bind(&path).unwrap();
        server.set_nonblocking(true).unwrap();
        let client = LocalApi::new(path).unwrap();
        let changed = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let (a, b, c) = (changed.clone(), stopped.clone(), calls.clone());
        let worker = thread::spawn(move || {
            while !b.load(Ordering::Acquire) {
                let (mut socket, _) = match server.accept() {
                    Ok(s) => s,
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
                    if socket.read(&mut byte).ok() != Some(1) {
                        break;
                    }
                    request.push(byte[0]);
                }
                if request.is_empty() {
                    continue;
                }
                assert!(request.starts_with(b"GET /localapi/v0/status?peers=true "));
                c.fetch_add(1, Ordering::SeqCst);
                let _ = socket.write_all(&response(a.load(Ordering::Acquire)));
            }
        });
        Self {
            client,
            changed,
            stopped,
            calls,
            worker: Some(worker),
        }
    }
}
impl Drop for Api {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        self.worker.take().unwrap().join().unwrap();
    }
}
async fn endpoint(cx: &Cx) -> QuicUdpEndpoint {
    QuicUdpEndpoint::bind(
        cx,
        "100.64.0.2:0".parse().unwrap(),
        QuicUdpEndpointConfig {
            max_packet_size: 1200,
            max_batch_size: 16,
            ..Default::default()
        },
    )
    .await
    .unwrap()
}
async fn accept(cx: &Cx, mut endpoint: QuicUdpEndpoint) -> NativeQuicUdpConnection {
    // Test-only first-flight loss to discover the random DCID. This is not the
    // pending production multiplexing listener or an ingress qualification.
    let packets = endpoint.receive_batch(cx, 1).await.unwrap();
    let (PacketHeader::Long(header), _) = PacketHeader::decode(&packets[0].data, 0).unwrap() else {
        panic!("expected Initial")
    };
    let cfg = NativeQuicConnectionConfig {
        max_local_bidi: 0,
        max_local_uni: 8,
        send_window: 65_536,
        recv_window: 65_536,
        connection_send_limit: 524_288,
        connection_recv_limit: 524_288,
        max_datagram_frame_size: 1200,
        ..Default::default()
    };
    let mut parameters = Vec::new();
    TransportParameters {
        initial_max_data: Some(cfg.connection_recv_limit),
        initial_max_stream_data_bidi_local: Some(cfg.recv_window),
        initial_max_stream_data_bidi_remote: Some(cfg.recv_window),
        initial_max_stream_data_uni: Some(cfg.recv_window),
        initial_max_streams_bidi: Some(0),
        initial_max_streams_uni: Some(8),
        max_datagram_frame_size: Some(1200),
        ..Default::default()
    }
    .encode(&mut parameters)
    .unwrap();
    let sc = server_config(
        vec![std::fs::read(pki().join("target.der")).unwrap().into()],
        std::fs::read(pki().join("target.key.der"))
            .unwrap()
            .try_into()
            .unwrap(),
        vec![ALPN.to_vec()],
    )
    .unwrap();
    NativeQuicUdpConnection::accept(
        cx,
        endpoint,
        QuicHandshakeDriver::server(sc, parameters).unwrap(),
        header.dst_cid,
        ConnectionId::new(b"namespace-host1").unwrap(),
        cfg,
        ALPN,
    )
    .await
    .unwrap()
}
async fn host(cx: &Cx, endpoint: QuicUdpEndpoint, done: &AtomicBool) {
    let native = accept(cx, endpoint).await;
    let mut host = crate::session_startup::connection_test_host(
        cx.clone(),
        native,
        offer(),
        Policy::default(),
    );
    let mut approved = false;
    while !host.is_complete() {
        host.drive(Duration::from_millis(5)).await.unwrap();
        if !approved && let Some(approval) = host.approval() {
            approval.decide(true).unwrap();
            approved = true;
        }
    }
    assert!(approved);
    let mut running = host.finish().unwrap().into_running().unwrap();
    let mut nonce = 0;
    while !done.load(Ordering::Acquire) {
        if running
            .drive(
                Duration::from_millis(5),
                || {
                    nonce += 1;
                    Ok(nonce)
                },
                |_, _| Err(()),
            )
            .await
            .is_err()
        {
            assert!(done.load(Ordering::Acquire));
            break;
        }
    }
    running.close();
}
#[test]
#[ignore = "explicit isolated namespace: scripts/verify-native-session.sh"]
fn public_client_runs_approved_viewer_past_initial_target_and_observation_expiry() {
    let api = Api::new();
    let runtime = network::runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let hc = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let endpoint = endpoint(&hc).await;
        let port = endpoint.local_addr().port();
        let mut client = Client::new(api.client.clone(), roots(), Duration::from_secs(8)).unwrap();
        let done = Arc::new(AtomicBool::new(false));
        let app_done = done.clone();
        let vc = cx.clone();
        let client_run = async {
            let result = client
                .run(
                    cx.clone(),
                    PeerSelector::StableId("n-peer"),
                    Configuration {
                        port,
                        ..Configuration::default()
                    },
                    offer(),
                    move |mut viewer| async move {
                        while !viewer.is_complete() {
                            viewer.drive(Duration::from_millis(5)).await.unwrap();
                        }
                        let mut running = viewer.finish().unwrap();
                        let end = network::clock(&vc) + 3_200_000;
                        while network::clock(&vc) < end {
                            running
                                .drive(Duration::from_millis(5), |_, _| Err(()))
                                .await
                                .unwrap();
                        }
                        assert!(running.check().is_ok());
                        app_done.store(true, Ordering::Release);
                        17
                    },
                )
                .await;
            done.store(true, Ordering::Release);
            result
        };
        let (result, ()) = Box::pin(timeout(
            hc.now(),
            Duration::from_secs(15),
            network::both(client_run, host(&hc, endpoint, &done)),
        ))
        .await
        .unwrap();
        assert_eq!(result, Ok(17));
        assert!(cx.checkpoint().is_err());
        assert!(
            api.calls.load(Ordering::Acquire) >= 12,
            "discovery, TLS and running-target renewals must all happen"
        );
    });
}
#[test]
#[ignore = "explicit isolated namespace: scripts/verify-native-session.sh"]
fn identity_reassignment_interrupts_pending_application_and_fences_before_drop() {
    let api = Api::new();
    let runtime = network::runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let hc = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let endpoint = endpoint(&hc).await;
        let port = endpoint.local_addr().port();
        let mut client = Client::new(api.client.clone(), roots(), Duration::from_secs(8)).unwrap();
        let done = Arc::new(AtomicBool::new(false));
        let changed = api.changed.clone();
        let dropped = Arc::new(AtomicBool::new(false));
        let d = dropped.clone();
        let vc = cx.clone();
        let client_run = async {
            let result = client
                .run(
                    cx.clone(),
                    PeerSelector::StableId("n-peer"),
                    Configuration {
                        port,
                        ..Configuration::default()
                    },
                    offer(),
                    move |mut viewer| async move {
                        while !viewer.is_complete() {
                            viewer.drive(Duration::from_millis(5)).await.unwrap();
                        }
                        let _running = viewer.finish().unwrap();
                        changed.store(true, Ordering::Release);
                        PendingApplication { cx: vc, dropped: d }.await;
                    },
                )
                .await;
            done.store(true, Ordering::Release);
            result
        };
        let (result, ()) = Box::pin(timeout(
            hc.now(),
            Duration::from_secs(12),
            network::both(client_run, host(&hc, endpoint, &done)),
        ))
        .await
        .unwrap();
        assert_eq!(
            result,
            Err(Error::Tailnet(fr_tailnet::Error::IdentityChanged))
        );
        assert!(dropped.load(Ordering::Acquire));
        assert!(cx.checkpoint().is_err());
    });
}
#[test]
#[ignore = "explicit isolated namespace: scripts/verify-native-session.sh"]
fn expired_connected_peer_cannot_be_promoted_to_a_renewal_owner() {
    let api = Api::new();
    let runtime = network::runtime();
    runtime.block_on(async {
        let cx = Cx::current().unwrap();
        let endpoint = endpoint(&cx).await;
        let target = api
            .client
            .peer_target(&cx, PeerSelector::StableId("n-peer"))
            .await
            .unwrap();
        let route = DialRoute {
            local: "100.64.0.1".parse().unwrap(),
            remote: endpoint.local_addr(),
        };
        let client =
            NativeClient::new(api.client.clone(), roots(), Duration::from_secs(8)).unwrap();
        let (connected, _remote) = Box::pin(network::both(
            client.dial(&cx, target, route).unwrap(),
            accept(&cx, endpoint),
        ))
        .await;
        let connected = connected.unwrap();
        let count = api.calls.load(Ordering::Acquire);
        sleep(cx.now(), Duration::from_millis(3100)).await;
        assert!(matches!(
            connected.into_owned_connection(cx),
            Err(fr_tailnet::Error::Expired)
        ));
        assert_eq!(api.calls.load(Ordering::Acquire), count);
    });
}
