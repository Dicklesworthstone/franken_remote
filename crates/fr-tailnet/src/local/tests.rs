//! Synthetic authority fixtures over real Unix/HTTP and the real runtime.
//! These are not captured Tailscale sharing fixtures or live-tailnet evidence.
use super::*;
use crate::{
    DESKTOP_CAPABILITY, Permissions, Scope,
    metadata::{Status, WhoIs, evaluate},
};
use asupersync::{runtime::RuntimeBuilder, types::CancelKind};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    os::unix::net::UnixListener,
    sync::atomic::AtomicUsize,
    thread::{self, JoinHandle},
};

fn endpoints() -> ConnectionAddresses {
    ConnectionAddresses {
        local: "100.64.0.1:4710".parse().unwrap(),
        peer: "100.64.0.2:30001".parse().unwrap(),
    }
}
fn fixtures() -> (Value, Value) {
    let host_key = format!("nodekey:{}", "1".repeat(64));
    let key = format!("nodekey:{}", "2".repeat(64));
    let peer = json!({"ID":"n-peer", "NodeID":2, "PublicKey":key, "UserID":7,
        "TailscaleIPs":["100.64.0.2"], "InNetworkMap":true});
    let status = json!({"Version":"test-wire-fixture-not-a-version-qualification", "BackendState":"Running",
        "TailscaleIPs":["100.64.0.1"], "CurrentTailnet":{"Name":"test.invalid","MagicDNSSuffix":"fixture.ts.net"},
        "Self":{"ID":"n-host", "NodeID":1,"PublicKey":host_key,"UserID":7,
                "TailscaleIPs":["100.64.0.1"],"InNetworkMap":true}, "Peer": {key.clone():peer}});
    let who = json!({"Node":{"ID":2,"StableID":"n-peer","Key":key,"User":7,
        "Addresses":["100.64.0.2/32"],"MachineAuthorized":true},
        "CapMap":{DESKTOP_CAPABILITY:[{"version":1,"observe":true,"control":true}]},
        "UserProfile":{"ID":7,"LoginName":"private-name@fixture.invalid"}});
    (status, who)
}
fn evaluate_fixture(status: &Value, who: &Value, scope: Scope) -> Result<Permissions, Error> {
    evaluate(
        &Status::parse(&serde_json::to_vec(status).unwrap())?,
        &WhoIs::parse(&serde_json::to_vec(who).unwrap())?,
        endpoints(),
        GrantPolicy {
            scope,
            ..Default::default()
        },
    )
    .map(|(_, p, _)| p)
}
fn peer_mut(status: &mut Value) -> &mut Value {
    status["Peer"]
        .as_object_mut()
        .unwrap()
        .values_mut()
        .next()
        .unwrap()
}

struct Server {
    client: LocalApi,
    stop: Arc<AtomicBool>,
    calls: Arc<AtomicUsize>,
    thread: Option<JoinHandle<()>>,
}
impl Server {
    fn new(replies: Vec<Vec<u8>>, pause: Duration) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let unique = NEXT.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "fr-localapi-{}-{unique}-{}.sock",
            std::process::id(),
            wall_now().unwrap()
        ));
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let (stopped, counted) = (stop.clone(), calls.clone());
        let thread = thread::spawn(move || {
            let mut index = 0usize;
            while !stopped.load(Ordering::Acquire) {
                let (mut socket, _) = match listener.accept() {
                    Ok(v) => v,
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
                } // Expected credential-refusal test.
                let request = String::from_utf8(request).unwrap();
                assert!(
                    request.contains("Host: local-tailscaled.sock\r\n"),
                    "exact LocalAPI Host required"
                );
                assert!(
                    request.starts_with("GET /localapi/v0/status?peers=true ")
                        || request.starts_with("GET /localapi/v0/whois?addr=100.64.0.2%3A30001 ")
                );
                counted.fetch_add(1, Ordering::SeqCst);
                thread::sleep(pause);
                let reply = &replies[index.min(replies.len() - 1)];
                index += 1;
                // The client may cancel or refuse an oversize response partway through.
                if let Err(e) = socket.write_all(reply) {
                    assert!(matches!(
                        e.kind(),
                        std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                    ));
                }
            }
        });
        let mut client = LocalApi::new(&path).unwrap();
        // Private test setup allows tests to run under an unprivileged CI UID.
        // Production LocalApi::new ALWAYS requires root; no public override.
        client.daemon_uid = UnixStream::pair().unwrap().0.peer_cred().unwrap().uid;
        Self {
            client,
            stop,
            calls,
            thread: Some(thread),
        }
    }
    fn fixture(status: &Value, who: &Value, chunked: bool) -> Self {
        Self::new(
            vec![
                response(status, chunked),
                response(who, chunked),
                response(status, chunked),
            ],
            Duration::ZERO,
        )
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.thread.take().unwrap().join().unwrap();
    }
}
fn response(value: &Value, chunked: bool) -> Vec<u8> {
    let data = serde_json::to_vec(value).unwrap();
    if chunked {
        let mut out = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
        for part in data.chunks(17) {
            write!(out, "{:x}\r\n", part.len()).unwrap();
            out.extend_from_slice(part);
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(b"0\r\n\r\n");
        out
    } else {
        let mut out = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",data.len()).into_bytes();
        out.extend_from_slice(&data);
        out
    }
}
fn runtime() -> asupersync::runtime::Runtime {
    RuntimeBuilder::new()
        .worker_threads(2)
        .enable_platform_reactor(true)
        .build()
        .unwrap()
}

#[test]
fn real_unix_http_snapshot_has_exact_source_binding_and_exclusive_expiry() {
    for chunked in [false, true] {
        let (status, who) = fixtures();
        let server = Server::fixture(&status, &who, chunked);
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let proof = server
                .client
                .authorize_app_capability(&cx, endpoints(), GrantPolicy::default())
                .await
                .unwrap();
            assert!(proof.permissions().observe());
            assert!(proof.permissions().control());
            assert_eq!(server.calls.load(Ordering::SeqCst), 3);
            assert!(proof.check_at(endpoints(), proof.expires_us() - 1).is_ok());
            assert_eq!(
                proof.check_at(endpoints(), proof.expires_us()),
                Err(Error::Expired)
            );
            let mut wrong = endpoints();
            wrong.peer.set_port(30002);
            assert_eq!(
                proof.check_at(wrong, now(&cx).unwrap()),
                Err(Error::AddressMismatch)
            );
            assert_eq!(
                proof.check_at(endpoints(), proof.issued_us - 1),
                Err(Error::Clock)
            );
            let debug = format!("{proof:?} {:?} {:?}", server.client, endpoints());
            for secret in [
                "100.64.",
                "n-peer",
                "private-name",
                "test.invalid",
                "nodekey:",
                ".sock",
            ] {
                assert!(!debug.contains(secret));
            }
        });
    }
}
#[test]
fn source_membership_is_not_inferred_from_names_prefixes_or_node_capabilities() {
    let (mut status, mut who) = fixtures();
    who["CapMap"] = json!({});
    who["Node"]["Name"] = json!("peer.fixture.ts.net.");
    who["Node"]["CapMap"] =
        json!({DESKTOP_CAPABILITY:[{"version":1,"observe":true,"control":true}]});
    peer_mut(&mut status)["Capabilities"] = json!([DESKTOP_CAPABILITY]);
    assert_eq!(
        evaluate_fixture(&status, &who, Scope::Tailnet),
        Err(Error::CapabilityDenied)
    );
}
#[test]
fn same_user_default_different_owners_and_tagged_hosts_have_explicit_scope() {
    let (mut s, mut w) = fixtures();
    assert!(evaluate_fixture(&s, &w, Scope::OwnUser).is_ok());
    peer_mut(&mut s)["UserID"] = json!(8);
    w["Node"]["User"] = json!(8);
    assert_eq!(
        evaluate_fixture(&s, &w, Scope::OwnUser),
        Err(Error::ScopeDenied)
    );
    assert!(evaluate_fixture(&s, &w, Scope::Tailnet).is_ok());
    peer_mut(&mut s)["Tags"] = json!(["tag:client"]);
    w["Node"]["Tags"] = json!(["tag:client"]);
    assert_eq!(
        evaluate_fixture(&s, &w, Scope::OwnUser),
        Err(Error::ScopeDenied)
    );
    assert!(evaluate_fixture(&s, &w, Scope::Tailnet).is_ok());
    s["Self"]["Tags"] = json!(["tag:host"]);
    assert_eq!(
        evaluate_fixture(&s, &w, Scope::OwnUser),
        Err(Error::ExplicitScopeRequired)
    );
    assert!(evaluate_fixture(&s, &w, Scope::Tailnet).is_ok());
}
#[test]
fn shared_in_and_external_sharees_are_refused_even_with_grants() {
    for field in ["ShareeNode", "AltSharerUserID"] {
        let (mut s, w) = fixtures();
        peer_mut(&mut s)[field] = if field == "ShareeNode" {
            json!(true)
        } else {
            json!(11)
        };
        assert_eq!(
            evaluate_fixture(&s, &w, Scope::Tailnet),
            Err(Error::SharedPeer)
        );
    }
    let (s, mut w) = fixtures();
    w["Node"]["Sharer"] = json!(11);
    assert_eq!(
        evaluate_fixture(&s, &w, Scope::Tailnet),
        Err(Error::SharedPeer)
    );
}
#[test]
fn routed_sources_cannot_borrow_a_subnet_routers_identity() {
    let (s, mut w) = fixtures();
    w["Node"]["Addresses"] = json!(["100.64.0.2/24"]);
    w["Node"]["AllowedIPs"] = json!(["0.0.0.0/0"]);
    assert_eq!(
        evaluate_fixture(&s, &w, Scope::Tailnet),
        Err(Error::AddressMismatch)
    );
    w["Node"]["Addresses"] = json!(["100.64.0.3/32"]);
    assert_eq!(
        evaluate_fixture(&s, &w, Scope::Tailnet),
        Err(Error::AddressMismatch)
    );
}
#[test]
fn ipv6_source_and_own_address_prefixes_are_checked_exactly() {
    let (mut s, mut w) = fixtures();
    let e = ConnectionAddresses {
        local: "[fd7a:115c:a1e0::1]:4710".parse().unwrap(),
        peer: "[fd7a:115c:a1e0::2]:30001".parse().unwrap(),
    };
    s["Self"]["TailscaleIPs"] = json!([e.local.ip().to_string()]);
    s["TailscaleIPs"] = s["Self"]["TailscaleIPs"].clone();
    peer_mut(&mut s)["TailscaleIPs"] = json!([e.peer.ip().to_string()]);
    w["Node"]["Addresses"] = json!([format!("{}/128", e.peer.ip())]);
    assert!(
        evaluate(
            &Status::parse(&serde_json::to_vec(&s).unwrap()).unwrap(),
            &WhoIs::parse(&serde_json::to_vec(&w).unwrap()).unwrap(),
            e,
            GrantPolicy::default()
        )
        .is_ok()
    );
    assert_eq!(
        ConnectionAddresses {
            local: e.local,
            peer: "[::ffff:100.64.0.2]:30001".parse().unwrap()
        }
        .validate(),
        Err(Error::InvalidEndpoint)
    );
}
#[test]
fn changed_identity_backend_and_known_key_expiry_refuse() {
    let (s, w) = fixtures();
    for (field, value) in [
        ("ID", json!(55)),
        ("StableID", json!("foreign")),
        ("User", json!(99)),
        ("Tags", json!(["tag:foreign"])),
    ] {
        let mut bad = w.clone();
        bad["Node"][field] = value;
        assert_eq!(
            evaluate_fixture(&s, &bad, Scope::Tailnet),
            Err(Error::IdentityMismatch)
        );
    }
    let mut bad = s.clone();
    bad["BackendState"] = json!("NeedsLogin");
    assert_eq!(
        evaluate_fixture(&bad, &w, Scope::Tailnet),
        Err(Error::BackendNotRunning)
    );
    peer_mut(&mut bad)["Expired"] = json!(true);
    bad["BackendState"] = json!("Running");
    assert_eq!(
        evaluate_fixture(&bad, &w, Scope::Tailnet),
        Err(Error::KeyExpired)
    );
    let mut bad = w.clone();
    bad["Node"]["MachineAuthorized"] = json!(false);
    assert_eq!(
        evaluate_fixture(&s, &bad, Scope::Tailnet),
        Err(Error::CapabilityDenied)
    );
}
#[test]
fn grant_union_is_bounded_and_cannot_ignore_unknown_restrictions() {
    let (s, mut w) = fixtures();
    w["CapMap"][DESKTOP_CAPABILITY] = json!([{"version":1,"observe":true,"control":false}]);
    let permissions = evaluate_fixture(&s, &w, Scope::OwnUser).unwrap();
    assert!(permissions.observe());
    assert!(!permissions.control());
    w["CapMap"][DESKTOP_CAPABILITY][0]["control"] = json!(true);
    w["CapMap"][DESKTOP_CAPABILITY][0]["observe"] = json!(false);
    assert_eq!(
        evaluate_fixture(&s, &w, Scope::OwnUser),
        Err(Error::InvalidCapability)
    );
    w["CapMap"][DESKTOP_CAPABILITY][0]["observe"] = json!(true);
    w["CapMap"][DESKTOP_CAPABILITY][0]["only_window"] = json!("terminal");
    assert_eq!(
        evaluate_fixture(&s, &w, Scope::OwnUser),
        Err(Error::MalformedMetadata)
    );
    let (_, valid) = fixtures();
    w["CapMap"][DESKTOP_CAPABILITY] =
        Value::Array(vec![valid["CapMap"][DESKTOP_CAPABILITY][0].clone(); 9]);
    assert_eq!(
        evaluate_fixture(&s, &w, Scope::OwnUser),
        Err(Error::MalformedMetadata)
    );
}
#[test]
fn truncation_duplicates_and_oversized_metadata_never_admit() {
    let (s, w) = fixtures();
    let sb = serde_json::to_vec(&s).unwrap();
    let wb = serde_json::to_vec(&w).unwrap();
    for cut in 0..sb.len() {
        assert!(Status::parse(&sb[..cut]).is_err());
    }
    for cut in 0..wb.len() {
        assert!(WhoIs::parse(&wb[..cut]).is_err());
    }
    let duplicate = String::from_utf8(wb.clone())
        .unwrap()
        .replace("\"version\":1", "\"version\":1,\"version\":1");
    assert!(WhoIs::parse(duplicate.as_bytes()).is_err());
    let mut too_long = s.clone();
    too_long["Self"]["ID"] = json!("x".repeat(129));
    assert!(Status::parse(&serde_json::to_vec(&too_long).unwrap()).is_err());
    assert!(Status::parse(&vec![b' '; metadata::STATUS_BYTES + 1]).is_err());
    let key = s["Peer"].as_object().unwrap().keys().next().unwrap();
    let duplicate_peer = format!(
        "{{\"Version\":\"x\",\"BackendState\":\"Running\",\"Self\":{},\"CurrentTailnet\":{},\"TailscaleIPs\":[\"100.64.0.1\"],\"Peer\":{{\"{key}\":{},\"{key}\":{}}}}}",
        s["Self"], s["CurrentTailnet"], s["Peer"][key], s["Peer"][key]
    );
    assert!(Status::parse(duplicate_peer.as_bytes()).is_err());
}
#[test]
fn root_peer_credentials_are_verified_before_sending_any_query() {
    let (s, w) = fixtures();
    let mut server = Server::fixture(&s, &w, false);
    server.client.daemon_uid = server.client.daemon_uid.checked_add(1).unwrap();
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        assert_eq!(
            server
                .client
                .authorize_app_capability(&cx, endpoints(), GrantPolicy::default())
                .await
                .unwrap_err(),
            Error::UntrustedLocalApi
        );
    });
    assert_eq!(server.calls.load(Ordering::SeqCst), 0);
    assert_eq!(LocalApi::installed().daemon_uid, 0);
}
#[test]
fn actual_http_rejects_oversize_compression_redirects_and_truncation() {
    for reply in [b"HTTP/1.1 302 Found\r\nLocation: http://bad.invalid\r\nContent-Length: 0\r\n\r\n".to_vec(),
        format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",metadata::STATUS_BYTES+1).into_bytes(),
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 20\r\n\r\n{}".to_vec(),
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Encoding: gzip\r\nContent-Length: 2\r\n\r\n{}".to_vec()] {
        let server=Server::new(vec![reply],Duration::ZERO);
        runtime().block_on(async {let cx=Cx::current().unwrap();assert!(server.client.authorize_app_capability(&cx,endpoints(),GrantPolicy::default()).await.is_err());});
    }
}
#[test]
fn inconsistent_snapshots_retry_once_without_unbounded_work() {
    let (s, w) = fixtures();
    let mut changed = s.clone();
    changed["Version"] = json!("fixture-restarted");
    let server = Server::new(
        vec![
            response(&s, false),
            response(&w, false),
            response(&changed, false),
            response(&changed, false),
            response(&w, false),
            response(&changed, false),
        ],
        Duration::ZERO,
    );
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let proof = server
            .client
            .authorize_app_capability(&cx, endpoints(), GrantPolicy::default())
            .await
            .unwrap();
        assert!(proof.permissions().observe());
    });
    assert_eq!(server.calls.load(Ordering::SeqCst), 6);
    let server = Server::new(
        vec![
            response(&s, false),
            response(&w, false),
            response(&changed, false),
            response(&s, false),
            response(&w, false),
            response(&changed, false),
        ],
        Duration::ZERO,
    );
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        assert_eq!(
            server
                .client
                .authorize_app_capability(&cx, endpoints(), GrantPolicy::default())
                .await
                .unwrap_err(),
            Error::SnapshotChanged
        );
    });
    assert_eq!(server.calls.load(Ordering::SeqCst), 6);
}
#[test]
fn delayed_snapshot_cannot_slide_issued_deadline_and_expired_proof_cannot_renew() {
    let (s, w) = fixtures();
    let server = Server::new(
        vec![
            response(&s, false),
            response(&w, false),
            response(&s, false),
        ],
        Duration::from_millis(25),
    );
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let p = GrantPolicy {
            validity: Duration::from_millis(30),
            ..Default::default()
        };
        assert_eq!(
            server
                .client
                .authorize_app_capability(&cx, endpoints(), p)
                .await
                .unwrap_err(),
            Error::Expired
        );
    });
    let server = Server::fixture(&s, &w, false);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let proof = server
            .client
            .authorize_app_capability(
                &cx,
                endpoints(),
                GrantPolicy {
                    validity: Duration::from_millis(80),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        sleep(cx.timer_driver().unwrap().now(), Duration::from_millis(100)).await;
        assert_eq!(
            server
                .client
                .revalidate(&cx, &proof, endpoints())
                .await
                .unwrap_err(),
            Error::Expired
        );
        assert_eq!(server.calls.load(Ordering::SeqCst), 3);
    });
}
#[test]
fn cancellation_timeout_and_dropped_future_release_single_lookup_credit() {
    for cancel in [false, true] {
        let (s, _) = fixtures();
        let server = Server::new(vec![response(&s, false)], Duration::from_millis(200));
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let policy = GrantPolicy {
                lookup_timeout: Duration::from_millis(35),
                ..Default::default()
            };
            let mut lookup = Box::pin(server.client.authorize_app_capability(
                &cx,
                endpoints(),
                policy,
            ));
            poll_fn(|task| {
                assert!(lookup.as_mut().poll(task).is_pending());
                Poll::Ready(())
            })
            .await;
            assert_eq!(
                server
                    .client
                    .authorize_app_capability(&cx, endpoints(), policy)
                    .await
                    .unwrap_err(),
                Error::Busy
            );
            if cancel {
                cx.cancel_fast(CancelKind::User);
            }
            assert_eq!(
                lookup.await.unwrap_err(),
                if cancel {
                    Error::Cancelled
                } else {
                    Error::Timeout
                }
            );
            assert!(!server.client.busy.load(Ordering::Acquire));
        });
    }
    let (s, _) = fixtures();
    let server = Server::new(vec![response(&s, false)], Duration::from_millis(200));
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut lookup = Box::pin(server.client.authorize_app_capability(
            &cx,
            endpoints(),
            GrantPolicy::default(),
        ));
        poll_fn(|task| {
            assert!(lookup.as_mut().poll(task).is_pending());
            Poll::Ready(())
        })
        .await;
        drop(lookup);
        assert!(!server.client.busy.load(Ordering::Acquire));
    });
}
#[test]
fn go_expiry_timestamp_is_checked_and_clamps_local_authorization() {
    assert_eq!(
        expiry::unix_micros("1970-01-01T00:00:00Z").unwrap(),
        Some(0)
    );
    assert_eq!(
        expiry::unix_micros("1970-01-01T01:30:00.123456789+01:30").unwrap(),
        Some(123_456)
    );
    assert_eq!(
        expiry::unix_micros("2000-02-29T00:00:00Z").unwrap(),
        Some(951_782_400_000_000)
    );
    assert_eq!(expiry::unix_micros("0001-01-01T00:00:00Z").unwrap(), None);
    for value in [
        "2001-02-29T00:00:00Z",
        "2000-13-01T00:00:00Z",
        "2000-01-01T24:00:00Z",
        "2000-01-01T00:00:60Z",
        "2000-01-01T00:00:00.Z",
        "2000-01-01T00:00:00+25:00",
        "2000-01-01T00:00:00Zjunk",
        "2026-01-01",
    ] {
        assert!(expiry::unix_micros(value).is_err(), "{value}");
    }
    let (s, mut w) = fixtures();
    w["Node"]["KeyExpiry"] = json!("1970-01-01T00:00:01Z");
    let server = Server::fixture(&s, &w, false);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        assert_eq!(
            server
                .client
                .authorize_app_capability(&cx, endpoints(), GrantPolicy::default())
                .await
                .unwrap_err(),
            Error::KeyExpired
        );
    });
}

#[test]
fn admission_owner_drop_revocation_and_cancellation_stop_every_clone() {
    for action in 0..3 {
        let (s, w) = fixtures();
        let server = Server::fixture(&s, &w, false);
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let proof = server
                .client
                .authorize_app_capability(&cx, endpoints(), GrantPolicy::default())
                .await
                .unwrap();
            let owner = crate::Admission::new(server.client.clone(), cx.clone(), proof).unwrap();
            let gate = owner.lease();
            let other = gate.clone();
            assert!(gate.observe().is_ok());
            assert!(other.control().is_ok());
            match action {
                0 => drop(owner),
                1 => {
                    owner.revoke();
                    assert!(gate.check().is_err());
                }
                _ => {
                    cx.cancel_fast(CancelKind::User);
                    assert_eq!(gate.check(), Err(Error::Cancelled));
                }
            }
            assert!(gate.observe().is_err());
            assert!(other.control().is_err());
        });
    }
}
#[test]
fn admission_proof_cannot_be_moved_to_a_different_local_authority_instance() {
    let (s, w) = fixtures();
    let server = Server::fixture(&s, &w, false);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let proof = server
            .client
            .authorize_app_capability(&cx, endpoints(), GrantPolicy::default())
            .await
            .unwrap();
        let mut foreign = LocalApi::new(&*server.client.path).unwrap();
        foreign.daemon_uid = server.client.daemon_uid;
        assert_eq!(
            crate::Admission::new(foreign, cx, proof).unwrap_err(),
            Error::IdentityChanged
        );
    });
}
#[test]
fn unchanged_revalidation_extends_only_an_unexpired_owned_admission() {
    let (s, w) = fixtures();
    let replies = (0..3)
        .flat_map(|_| {
            [
                response(&s, false),
                response(&w, false),
                response(&s, false),
            ]
        })
        .collect();
    let server = Server::new(replies, Duration::ZERO);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let proof = server
            .client
            .authorize_app_capability(&cx, endpoints(), GrantPolicy::default())
            .await
            .unwrap();
        let original = proof.expires_us();
        let mut owner = crate::Admission::new(server.client.clone(), cx.clone(), proof).unwrap();
        let gate = owner.lease();
        sleep(cx.timer_driver().unwrap().now(), Duration::from_millis(15)).await;
        owner.refresh().await.unwrap();
        assert!(gate.observe().unwrap() > original);
        assert_eq!(gate.addresses(), endpoints());
        assert_eq!(server.calls.load(Ordering::SeqCst), 6);
        owner.revoke();
        assert_eq!(owner.refresh().await, Err(Error::Revoked));
        assert_eq!(server.calls.load(Ordering::SeqCst), 6);
    });
}
#[test]
fn capability_removal_permission_changes_and_identity_switch_close_old_admission() {
    for change in 0..4 {
        let (s, w) = fixtures();
        let mut next = s.clone();
        let mut who = w.clone();
        match change {
            0 => who["CapMap"] = json!({}),
            1 => who["CapMap"][DESKTOP_CAPABILITY][0]["control"] = json!(false),
            2 => next["Self"]["ID"] = json!("different-host"),
            _ => next["Version"] = json!("different-daemon-version"),
        }
        let server = Server::new(
            vec![
                response(&s, false),
                response(&w, false),
                response(&s, false),
                response(&next, false),
                response(&who, false),
                response(&next, false),
            ],
            Duration::ZERO,
        );
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let proof = server
                .client
                .authorize_app_capability(&cx, endpoints(), GrantPolicy::default())
                .await
                .unwrap();
            let mut owner = crate::Admission::new(server.client.clone(), cx, proof).unwrap();
            let lease = owner.lease();
            assert!(owner.refresh().await.is_err());
            assert!(lease.observe().is_err());
            assert!(lease.control().is_err());
        });
    }
}
#[test]
fn read_only_admission_does_not_turn_a_control_refusal_into_observation_revocation() {
    let (s, mut w) = fixtures();
    w["CapMap"][DESKTOP_CAPABILITY][0]["control"] = json!(false);
    let server = Server::fixture(&s, &w, false);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let proof = server
            .client
            .authorize_app_capability(&cx, endpoints(), GrantPolicy::default())
            .await
            .unwrap();
        let owner = crate::Admission::new(server.client.clone(), cx, proof).unwrap();
        let lease = owner.lease();
        assert_eq!(lease.control(), Err(Error::CapabilityDenied));
        assert!(lease.observe().is_ok());
    });
}
#[test]
fn expired_shared_admission_is_terminal_without_additional_network_traffic() {
    let (s, w) = fixtures();
    let server = Server::fixture(&s, &w, false);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let proof = server
            .client
            .authorize_app_capability(
                &cx,
                endpoints(),
                GrantPolicy {
                    validity: Duration::from_millis(120),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let mut owner = crate::Admission::new(server.client.clone(), cx.clone(), proof).unwrap();
        let gate = owner.lease();
        sleep(cx.timer_driver().unwrap().now(), Duration::from_millis(160)).await;
        assert_eq!(gate.observe(), Err(Error::Expired));
        assert_eq!(owner.refresh().await, Err(Error::Expired));
        assert_eq!(server.calls.load(Ordering::SeqCst), 3);
    });
}
#[test]
fn dropped_started_refresh_cannot_leave_an_admission_active() {
    let (s, w) = fixtures();
    let server = Server::new(
        vec![
            response(&s, false),
            response(&w, false),
            response(&s, false),
        ],
        Duration::ZERO,
    );
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let proof = server
            .client
            .authorize_app_capability(&cx, endpoints(), GrantPolicy::default())
            .await
            .unwrap();
        let mut owner = crate::Admission::new(server.client.clone(), cx, proof).unwrap();
        let gate = owner.lease();
        let mut refresh = Box::pin(owner.refresh());
        poll_fn(|task| {
            assert!(refresh.as_mut().poll(task).is_pending());
            Poll::Ready(())
        })
        .await;
        drop(refresh);
        assert_eq!(gate.observe(), Err(Error::Revoked));
        assert!(!server.client.busy.load(Ordering::Acquire));
    });
}
#[test]
fn revocation_during_refresh_cannot_be_overwritten_by_a_successful_response() {
    let (s, w) = fixtures();
    let replies = (0..2)
        .flat_map(|_| {
            [
                response(&s, false),
                response(&w, false),
                response(&s, false),
            ]
        })
        .collect();
    let server = Server::new(replies, Duration::ZERO);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let proof = server
            .client
            .authorize_app_capability(&cx, endpoints(), GrantPolicy::default())
            .await
            .unwrap();
        let mut owner = crate::Admission::new(server.client.clone(), cx, proof).unwrap();
        let gate = owner.lease();
        let mut refresh = Box::pin(owner.refresh());
        poll_fn(|task| {
            assert!(refresh.as_mut().poll(task).is_pending());
            Poll::Ready(())
        })
        .await;
        gate.revoke();
        assert_eq!(refresh.await, Err(Error::Revoked));
        assert_eq!(gate.observe(), Err(Error::Revoked));
    });
}
