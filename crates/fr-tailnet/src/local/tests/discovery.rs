//! Synthetic Tailscale metadata over real credential-checked Unix/HTTP/runtime.
use super::*;
use asupersync::{cx::Cx, net::unix::UnixStream, runtime::RuntimeBuilder, types::CancelKind};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    os::unix::net::UnixListener,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize},
    },
    thread::{self, JoinHandle},
};
fn fixtures() -> (Value, Value) {
    let key = format!("nodekey:{}", "2".repeat(64));
    (
        json!({"Version":"discovery-fixture-not-version-qualification","BackendState":"Running",
        "TailscaleIPs":["100.64.0.1"],"CurrentTailnet":{"Name":"fixture.invalid","MagicDNSSuffix":"fixture.ts.net"},
        "Self":{"ID":"n-host","NodeID":1,"PublicKey":format!("nodekey:{}", "1".repeat(64)),"UserID":7,"TailscaleIPs":["100.64.0.1"],"InNetworkMap":true},
        "Peer":{key.clone():{"ID":"n-peer","NodeID":2,"PublicKey":key,"UserID":7,"TailscaleIPs":["100.64.0.2"],"InNetworkMap":true}}}),
        Value::Null,
    )
}
fn peer_mut(s: &mut Value) -> &mut Value {
    s["Peer"]
        .as_object_mut()
        .unwrap()
        .values_mut()
        .next()
        .unwrap()
}
fn runtime() -> asupersync::runtime::Runtime {
    RuntimeBuilder::new()
        .worker_threads(2)
        .enable_platform_reactor(true)
        .build()
        .unwrap()
}
fn response(s: &Value, chunked: bool) -> Vec<u8> {
    let bytes = serde_json::to_vec(s).unwrap();
    if chunked {
        let mut out = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
        for part in bytes.chunks(17) {
            write!(out, "{:x}\r\n", part.len()).unwrap();
            out.extend_from_slice(part);
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(b"0\r\n\r\n");
        out
    } else {
        let mut out = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",bytes.len()).into_bytes();
        out.extend_from_slice(&bytes);
        out
    }
}
struct Server {
    client: LocalApi,
    calls: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl Server {
    fn new(replies: Vec<Vec<u8>>, pause: Duration) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "fr-discover-{}-{}.sock",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let done = stop.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let worker = thread::spawn(move || {
            while !done.load(Ordering::Acquire) {
                let (mut socket, _) = match listener.accept() {
                    Ok(pair) => pair,
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
                    if socket.read(&mut byte).unwrap_or(0) != 1 {
                        break;
                    }
                    request.push(byte[0]);
                }
                if request.is_empty() {
                    continue;
                }
                assert!(request.starts_with(b"GET /localapi/v0/status?peers=true "));
                assert!(
                    String::from_utf8(request)
                        .unwrap()
                        .contains("Host: local-tailscaled.sock\r\n")
                );
                let index = count.fetch_add(1, Ordering::SeqCst);
                thread::sleep(pause);
                if let Err(e) = socket.write_all(&replies[index.min(replies.len() - 1)]) {
                    assert!(matches!(
                        e.kind(),
                        std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                    ));
                }
            }
        });
        let mut client = LocalApi::new(path).unwrap();
        // Private test-only endpoint ownership. No production UID override exists.
        client.daemon_uid = UnixStream::pair().unwrap().0.peer_cred().unwrap().uid;
        Self {
            client,
            calls,
            stop,
            worker: Some(worker),
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.worker.take().unwrap().join().unwrap();
    }
}

fn status() -> Value {
    let (mut s, _) = fixtures();
    s["Self"]["DNSName"] = json!("local.fixture.ts.net.");
    peer_mut(&mut s)["DNSName"] = json!("remote.fixture.ts.net.");
    s
}
#[test]
fn discovery_reads_once_and_reports_machines_not_desktop_capabilities() {
    let server = Server::new(vec![response(&status(), true)], Duration::ZERO);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let before = now(&cx).unwrap();
        let discovery = server.client.discover(&cx).await.unwrap();
        assert!(discovery.lookup_started_us() >= before);
        assert!(discovery.lookup_completed_us() >= discovery.lookup_started_us());
        assert_eq!(discovery.excluded(), crate::DiscoveryExclusions::default());
        assert_eq!(discovery.peers().len(), 1);
        let peer = &discovery.peers()[0];
        assert_eq!(peer.stable_id(), "n-peer");
        assert_eq!(peer.certificate_name(), "remote.fixture.ts.net");
        assert_eq!(
            peer.addresses(),
            &["100.64.0.2".parse::<std::net::IpAddr>().unwrap()]
        );
        let debug = format!("{discovery:?} {peer:?}");
        for private in ["n-peer", "fixture", "100.64", "nodekey"] {
            assert!(!debug.contains(private));
        }
        assert_eq!(server.calls.load(Ordering::SeqCst), 1);
        // Discovery did not consume an authority slot. Fresh resolution still
        // performs its own TWO reads and produces its separate expiring target.
        let target = server
            .client
            .peer_target(&cx, crate::PeerSelector::StableId(peer.stable_id()))
            .await
            .unwrap();
        assert_eq!(target.stable_id(), "n-peer");
        assert_eq!(server.calls.load(Ordering::SeqCst), 3);
    });
}
#[test]
fn discovery_never_guesses_shared_expired_ambiguous_or_subnet_destinations() {
    for (field, value, expected) in [
        (
            "ShareeNode",
            json!(true),
            crate::DiscoveryExclusions {
                shared: 1,
                ..Default::default()
            },
        ),
        (
            "AltSharerUserID",
            json!(19),
            crate::DiscoveryExclusions {
                shared: 1,
                ..Default::default()
            },
        ),
        (
            "Expired",
            json!(true),
            crate::DiscoveryExclusions {
                expired: 1,
                ..Default::default()
            },
        ),
        (
            "KeyExpiry",
            json!("2020-01-01T00:00:00Z"),
            crate::DiscoveryExclusions {
                expired: 1,
                ..Default::default()
            },
        ),
        (
            "InNetworkMap",
            json!(false),
            crate::DiscoveryExclusions {
                unusable: 1,
                ..Default::default()
            },
        ),
        (
            "DNSName",
            json!("remote.attacker.invalid"),
            crate::DiscoveryExclusions {
                unusable: 1,
                ..Default::default()
            },
        ),
        (
            "TailscaleIPs",
            json!(["100.64.0.1"]),
            crate::DiscoveryExclusions {
                unusable: 1,
                ..Default::default()
            },
        ),
        (
            "TailscaleIPs",
            json!(["::ffff:100.64.0.2"]),
            crate::DiscoveryExclusions {
                unusable: 1,
                ..Default::default()
            },
        ),
    ] {
        let mut s = status();
        peer_mut(&mut s)[field] = value;
        let server = Server::new(vec![response(&s, false)], Duration::ZERO);
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let discovery = server.client.discover(&cx).await.unwrap();
            assert!(discovery.peers().is_empty(), "{field}");
            assert_eq!(discovery.excluded(), expected, "{field}");
        });
    }
}
#[test]
fn discovery_is_stably_ordered_and_conflicting_identities_are_not_arbitrarily_selected() {
    let mut s = status();
    let key = format!("nodekey:{}", "3".repeat(64));
    let mut other = peer_mut(&mut s).clone();
    other["ID"] = json!("a-first");
    other["NodeID"] = json!(3);
    other["PublicKey"] = json!(key);
    other["TailscaleIPs"] = json!(["100.64.0.3"]);
    other["DNSName"] = json!("other.fixture.ts.net.");
    s["Peer"][&key] = other;
    let server = Server::new(vec![response(&s, false)], Duration::ZERO);
    runtime().block_on(async {
        let d = server
            .client
            .discover(&Cx::current().unwrap())
            .await
            .unwrap();
        assert_eq!(
            d.peers()
                .iter()
                .map(crate::DiscoveredPeer::stable_id)
                .collect::<Vec<_>>(),
            ["a-first", "n-peer"]
        );
    });
    s["Peer"][&key]["TailscaleIPs"] = json!(["100.64.0.2"]);
    let server = Server::new(vec![response(&s, false)], Duration::ZERO);
    runtime().block_on(async {
        let d = server
            .client
            .discover(&Cx::current().unwrap())
            .await
            .unwrap();
        assert!(d.peers().is_empty());
        assert_eq!(d.excluded().unusable, 2);
    });
}
#[test]
fn failed_host_identity_refuses_all_discovery_instead_of_returning_an_empty_tailnet() {
    for (field, value, expected) in [
        ("DNSName", json!("localhost"), Error::MalformedMetadata),
        ("Expired", json!(true), Error::KeyExpired),
        (
            "KeyExpiry",
            json!("2020-01-01T00:00:00Z"),
            Error::KeyExpired,
        ),
    ] {
        let mut s = status();
        s["Self"][field] = value;
        let server = Server::new(vec![response(&s, false)], Duration::ZERO);
        runtime().block_on(async {
            assert_eq!(
                server
                    .client
                    .discover(&Cx::current().unwrap())
                    .await
                    .unwrap_err(),
                expected
            );
        });
    }
}
#[test]
fn discovery_respects_cancellation_busy_and_kernel_peer_credentials() {
    let mut server = Server::new(vec![response(&status(), false)], Duration::ZERO);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        server.client.busy.store(true, Ordering::Release);
        assert_eq!(server.client.discover(&cx).await.unwrap_err(), Error::Busy);
        assert_eq!(server.calls.load(Ordering::SeqCst), 0);
        server.client.busy.store(false, Ordering::Release);
        server.client.daemon_uid = server.client.daemon_uid.wrapping_add(1);
        assert_eq!(
            server.client.discover(&cx).await.unwrap_err(),
            Error::UntrustedLocalApi
        );
        assert_eq!(server.calls.load(Ordering::SeqCst), 0);
        assert!(!server.client.busy.load(Ordering::Acquire));
        cx.cancel_fast(CancelKind::User);
        assert_eq!(
            server.client.discover(&cx).await.unwrap_err(),
            Error::Cancelled
        );
    });
}
#[test]
fn stalled_discovery_has_a_fixed_deadline_and_releases_the_lookup_slot() {
    let server = Server::new(
        vec![response(&status(), false)],
        Duration::from_millis(1200),
    );
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        assert_eq!(
            server.client.discover(&cx).await.unwrap_err(),
            Error::Timeout
        );
        assert!(!server.client.busy.load(Ordering::Acquire));
        assert_eq!(server.calls.load(Ordering::SeqCst), 1);
    });
}
#[test]
fn malformed_or_over_limit_snapshots_refuse_instead_of_partially_discovering() {
    let mut s = status();
    // Parser bound is checked before any candidate is returned.
    let peer = peer_mut(&mut s).clone();
    s["Peer"] = Value::Object(
        (0..1025)
            .map(|i| (format!("{i:080}"), peer.clone()))
            .collect(),
    );
    let server = Server::new(vec![response(&s, false)], Duration::ZERO);
    runtime().block_on(async {
        assert_eq!(
            server
                .client
                .discover(&Cx::current().unwrap())
                .await
                .unwrap_err(),
            Error::MalformedMetadata
        );
    });
}
#[test]
fn discovery_reports_transport_path_direct_and_derp_relay() {
    let mut s = status();
    peer_mut(&mut s)["Relay"] = json!("nyc");
    peer_mut(&mut s)["CurAddr"] = json!("192.168.1.50:41641");
    let server = Server::new(vec![response(&s, false)], Duration::ZERO);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let discovery = server.client.discover(&cx).await.unwrap();
        assert_eq!(discovery.peers().len(), 1);
        let peer = &discovery.peers()[0];
        assert_eq!(
            peer.transport_path(),
            &PeerTransportPath::DerpRelayed {
                relay: "nyc".to_string()
            }
        );
    });

    let mut s2 = status();
    peer_mut(&mut s2)["CurAddr"] = json!("192.168.1.50:41641");
    let server2 = Server::new(vec![response(&s2, false)], Duration::ZERO);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let discovery = server2.client.discover(&cx).await.unwrap();
        assert_eq!(discovery.peers().len(), 1);
        let peer = &discovery.peers()[0];
        assert_eq!(
            peer.transport_path(),
            &PeerTransportPath::Direct {
                cur_addr: "192.168.1.50:41641".to_string()
            }
        );
    });
}
