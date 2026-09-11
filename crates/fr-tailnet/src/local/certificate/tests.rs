//! Real Unix/HTTP, `WebPKI` and UDP/TLS; identity and CA are explicit fixtures.
use super::*;
use asupersync::{
    net::{
        quic_core::{ConnectionId, TransportParameters},
        quic_native::{
            NativeQuicConnectionConfig, NativeQuicUdpConnection, QuicUdpEndpoint,
            QuicUdpEndpointConfig, handshake_driver::client_config,
        },
        unix::UnixStream,
    },
    runtime::{Runtime, RuntimeBuilder},
    time::{sleep, timeout},
};
use serde_json::{Value, json};
use std::{
    future::{Future, poll_fn},
    io::{Read, Write},
    os::unix::{fs::DirBuilderExt, net::UnixListener},
    path::{Path, PathBuf},
    pin::pin,
    process::Command,
    sync::{
        OnceLock,
        atomic::{AtomicBool, AtomicUsize},
    },
    task::Poll,
    thread::{self, JoinHandle},
    time::{SystemTime, UNIX_EPOCH},
};
const HOST: &str = "host.fixture.ts.net";
fn runtime() -> Runtime {
    RuntimeBuilder::new()
        .worker_threads(2)
        .enable_platform_reactor(true)
        .build()
        .unwrap()
}
async fn both<A: Future, B: Future>(a: A, b: B) -> (A::Output, B::Output) {
    let (mut a, mut b) = (pin!(a), pin!(b));
    let (mut ar, mut br) = (None, None);
    poll_fn(|cx| {
        if ar.is_none()
            && let Poll::Ready(value) = a.as_mut().poll(cx)
        {
            ar = Some(value);
        }
        if br.is_none()
            && let Poll::Ready(value) = b.as_mut().poll(cx)
        {
            br = Some(value);
        }
        if ar.is_some() && br.is_some() {
            Poll::Ready((ar.take().unwrap(), br.take().unwrap()))
        } else {
            Poll::Pending
        }
    })
    .await
}
fn openssl(dir: &Path, args: &[&str]) {
    let result = Command::new("openssl")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "fixture OpenSSL failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}
fn pki() -> &'static Path {
    static PKI: OnceLock<PathBuf> = OnceLock::new();
    PKI.get_or_init(|| {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("fr-cert-{}-{stamp}",std::process::id()));
        std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        openssl(&dir, &["req","-x509","-newkey","ec","-pkeyopt","ec_paramgen_curve:P-256","-nodes","-keyout","ca.key","-out","ca.pem","-days","2","-subj","/CN=Ephemeral FR certificate fixture","-addext","basicConstraints=critical,CA:TRUE"]);
        for name in ["one", "two", "wrong", "expired"] {
            let key = format!("{name}.key"); let csr = format!("{name}.csr"); let pem = format!("{name}.pem");
            openssl(&dir, &["req","-newkey","ec","-pkeyopt","ec_paramgen_curve:P-256","-nodes","-keyout",&key,"-out",&csr,"-subj","/CN=Fixture leaf"]);
            let host = if name == "wrong" { "wrong.fixture.ts.net" } else { HOST };
            std::fs::write(dir.join("extensions"), format!("subjectAltName=DNS:{host}\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\n")).unwrap();
            if name == "expired" {
                std::fs::write(dir.join("index"), "").unwrap();
                std::fs::write(dir.join("serial"), "1000\n").unwrap();
                std::fs::write(dir.join("ca.cnf"), format!("[ca]\ndefault_ca=fixture\n[fixture]\ndatabase=index\nserial=serial\nnew_certs_dir=.\ncertificate=ca.pem\nprivate_key=ca.key\ndefault_md=sha256\npolicy=subject\n[subject]\ncommonName=supplied\n[leaf]\nsubjectAltName=DNS:{HOST}\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\n")).unwrap();
                openssl(&dir, &["ca","-batch","-config","ca.cnf","-in",&csr,"-startdate","20200101000000Z","-enddate","20200102000000Z","-out",&pem,"-notext","-extensions","leaf"]);
            } else {
                openssl(&dir, &["x509","-req","-in",&csr,"-CA","ca.pem","-CAkey","ca.key","-CAcreateserial","-days","2","-extfile","extensions","-out",&pem]);
            }
        }
        openssl(&dir, &["x509","-in","ca.pem","-outform","DER","-out","ca.der"]);
        dir
    })
}
fn pem(name: &str) -> Vec<u8> {
    let mut data = std::fs::read(pki().join(format!("{name}.key"))).unwrap();
    data.extend(std::fs::read(pki().join(format!("{name}.pem"))).unwrap());
    data
}
fn roots() -> RootCertStore {
    let mut roots = RootCertStore::empty();
    for cert in Certificate::from_pem(&std::fs::read(pki().join("ca.pem")).unwrap()).unwrap() {
        roots.add(&cert).unwrap();
    }
    roots
}
fn status() -> Value {
    json!({"Version":"synthetic-v1.102.3-shape-not-live-qualification", "BackendState":"Running",
        "TailscaleIPs":["100.64.0.1"], "CurrentTailnet":{"Name":"test.invalid","MagicDNSSuffix":"fixture.ts.net"},
        "Self":{"ID":"n-host","NodeID":1,"PublicKey":format!("nodekey:{}","1".repeat(64)),"UserID":7,
        "TailscaleIPs":["100.64.0.1"],"InNetworkMap":true,"DNSName":format!("{HOST}.")}})
}
#[derive(Clone)]
struct Reply {
    body: Vec<u8>,
    content_type: String,
    code: u16,
    chunked: bool,
}
impl Reply {
    fn pair(name: &str) -> Self {
        Self {
            body: pem(name),
            content_type: "text/plain".into(),
            code: 200,
            chunked: false,
        }
    }
    fn bytes(&self) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 {} Fixture\r\nContent-Type: {}\r\nConnection: close\r\n",
            self.code, self.content_type
        )
        .into_bytes();
        if self.chunked {
            out.extend_from_slice(b"Transfer-Encoding: chunked\r\n\r\n");
            for part in self.body.chunks(37) {
                write!(out, "{:x}\r\n", part.len()).unwrap();
                out.extend_from_slice(part);
                out.extend_from_slice(b"\r\n");
            }
            out.extend_from_slice(b"0\r\n\r\n");
        } else {
            write!(out, "Content-Length: {}\r\n\r\n", self.body.len()).unwrap();
            out.extend_from_slice(&self.body);
        }
        out
    }
}
struct Fixture {
    api: LocalApi,
    reply: Arc<Mutex<Reply>>,
    status: Arc<Mutex<Value>>,
    hold: Arc<AtomicBool>,
    cert_calls: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    task: Option<JoinHandle<()>>,
}
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = pki().join(format!("api-{}", NEXT.fetch_add(1, Ordering::SeqCst)));
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let reply = Arc::new(Mutex::new(Reply::pair("one")));
        let status = Arc::new(Mutex::new(status()));
        let hold = Arc::new(AtomicBool::new(false));
        let cert_calls = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (responses, metadata, blocked, counter, ended) = (
            reply.clone(),
            status.clone(),
            hold.clone(),
            cert_calls.clone(),
            stop.clone(),
        );
        let task = thread::spawn(move || {
            let mut requests = Vec::new();
            while !ended.load(Ordering::Acquire) {
                let (mut socket, _) = match listener.accept() {
                    Ok(v) => v,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(e) => panic!("fixture accept: {e}"),
                };
                let (responses, metadata, blocked, counter, ended) = (
                    responses.clone(),
                    metadata.clone(),
                    blocked.clone(),
                    counter.clone(),
                    ended.clone(),
                );
                requests.push(thread::spawn(move || {
                    socket
                        .set_read_timeout(Some(Duration::from_secs(1)))
                        .unwrap();
                    socket
                        .set_write_timeout(Some(Duration::from_secs(1)))
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
                        return;
                    }
                    let text = String::from_utf8(request).unwrap();
                    assert!(text.contains("Host: local-tailscaled.sock\r\n"));
                    let response = if text.starts_with("GET /localapi/v0/status?peers=false ") {
                        Reply {
                            body: serde_json::to_vec(&*metadata.lock().unwrap()).unwrap(),
                            content_type: "application/json".into(),
                            code: 200,
                            chunked: false,
                        }
                    } else {
                        assert!(text.starts_with(&format!(
                            "GET /localapi/v0/cert/{HOST}?type=pair&min_validity=24h "
                        )));
                        assert!(text.contains("Accept: text/plain\r\n"));
                        counter.fetch_add(1, Ordering::SeqCst);
                        while blocked.load(Ordering::Acquire) && !ended.load(Ordering::Acquire) {
                            thread::sleep(Duration::from_millis(1));
                        }
                        responses.lock().unwrap().clone()
                    };
                    let _ = socket.write_all(&response.bytes());
                }));
            }
            for task in requests {
                task.join().unwrap();
            }
        });
        let mut api = LocalApi::new(path).unwrap();
        api.daemon_uid = UnixStream::pair().unwrap().0.peer_cred().unwrap().uid;
        Self {
            api,
            reply,
            status,
            hold,
            cert_calls,
            stop,
            task: Some(task),
        }
    }
    async fn identity(&self, cx: &Cx) -> NativeServerIdentity {
        self.api
            .native_server_identity(cx, roots(), CertificatePolicy::default())
            .await
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.task.take().unwrap().join().unwrap();
    }
}
fn due(identity: &NativeServerIdentity, cx: &Cx) {
    identity.state().unwrap().status.next_refresh_us = now(cx).unwrap();
}
async fn until_call(cx: &Cx, fixture: &Fixture, count: usize) {
    timeout(cx.now(), Duration::from_secs(2), async {
        while fixture.cert_calls.load(Ordering::SeqCst) < count {
            sleep(cx.now(), Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
}
#[test]
fn exact_pair_shape_rejects_extra_keys_blocks_and_unbounded_chain() {
    let data = pem("one");
    assert!(Pair::parse(&data).is_ok());
    let key = std::fs::read(pki().join("one.key")).unwrap();
    let cert = std::fs::read(pki().join("one.pem")).unwrap();
    for bad in [
        key.clone(),
        cert.clone(),
        [cert.clone(), key.clone()].concat(),
        [key.clone(), key.clone(), cert.clone()].concat(),
        [data.clone(), b"PRIVATE TEXT".to_vec()].concat(),
        [b"garbage".to_vec(), data.clone()].concat(),
        [data.clone(), vec![0xff]].concat(),
        [key.clone(), cert.repeat(MAX_CERTIFICATES + 1)].concat(),
        vec![b'x'; PAIR_LIMIT + 1],
    ] {
        assert!(matches!(Pair::parse(&bad), Err(Error::CertificateRejected)));
    }
    for size in [0, 1, 25, key.len() - 1, key.len(), data.len() - 26] {
        assert!(Pair::parse(&data[..size]).is_err());
    }
}
#[test]
fn acquisition_validates_trust_name_key_validity_and_redacts_diagnostics() {
    let fixture = Fixture::new();
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let identity = fixture.identity(&cx).await;
        let node = fixture.api.node_identity(&cx).await.unwrap();
        assert!(identity.quic_handshake(&cx, &node, vec![]).is_ok());
        assert_eq!(identity.status(&cx).unwrap().generation, 1);
        for secret in [HOST, "PRIVATE KEY", "100.64.", ".sock", "nodekey:"] {
            assert!(!format!("{identity:?} {:?}", Error::CertificateRejected).contains(secret));
        }
        assert!(matches!(
            fixture
                .api
                .native_server_identity(&cx, RootCertStore::empty(), CertificatePolicy::default())
                .await,
            Err(Error::InvalidTrustStore)
        ));
        for bad in [
            pem("wrong"),
            pem("expired"),
            [
                std::fs::read(pki().join("two.key")).unwrap(),
                std::fs::read(pki().join("one.pem")).unwrap(),
            ]
            .concat(),
        ] {
            fixture.reply.lock().unwrap().body = bad;
            assert!(matches!(
                fixture
                    .api
                    .native_server_identity(&cx, roots(), CertificatePolicy::default())
                    .await,
                Err(Error::CertificateRejected)
            ));
        }
        fixture.reply.lock().unwrap().body = pem("one");
        let mut untrusted = RootCertStore::empty();
        untrusted
            .add(&Certificate::from_pem(&std::fs::read(pki().join("two.pem")).unwrap()).unwrap()[0])
            .unwrap();
        assert!(matches!(
            fixture
                .api
                .native_server_identity(&cx, untrusted, CertificatePolicy::default())
                .await,
            Err(Error::CertificateRejected)
        ));
    });
}
#[test]
fn pair_http_is_bounded_typed_and_denials_do_not_fall_back() {
    let fixture = Fixture::new();
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        fixture.reply.lock().unwrap().chunked = true;
        let _ = fixture.identity(&cx).await;
        for (code,kind,body,error) in [(403,"text/plain",pem("one"),Error::LocalApiDenied),
            (200,"application/json",pem("one"),Error::Http),
            (200,"text/plain",vec![b'x';PAIR_LIMIT+1],Error::Http)] {
            *fixture.reply.lock().unwrap() = Reply { body, content_type:kind.into(),code,chunked:false };
            assert!(matches!(fixture.api.native_server_identity(&cx,roots(),CertificatePolicy::default()).await,Err(e) if e==error));
        }
        *fixture.reply.lock().unwrap() = Reply::pair("one");
        let _ = fixture.identity(&cx).await;
    });
}
#[test]
fn renewal_atomically_rotates_the_complete_pair_without_replacing_session_authority() {
    let fixture = Fixture::new();
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let identity = fixture.identity(&cx).await;
        let before = identity.state().unwrap().active.clone().unwrap();
        assert_eq!(identity.refresh(&cx).await, Err(Error::CertificateNotDue));
        *fixture.reply.lock().unwrap() = Reply::pair("two");
        due(&identity, &cx);
        identity.refresh(&cx).await.unwrap();
        let after = identity.state().unwrap().active.clone().unwrap();
        assert!(!Arc::ptr_eq(&before, &after));
        assert_ne!(
            before.chain.clone().into_iter().next().unwrap().as_der(),
            after.chain.clone().into_iter().next().unwrap().as_der()
        );
        assert_eq!(identity.status(&cx).unwrap().generation, 2);
        let node = fixture.api.node_identity(&cx).await.unwrap();
        assert!(identity.quic_handshake(&cx, &node, vec![]).is_ok());
        assert!(before.verify(&identity.verifier, HOST).is_ok());
    });
}
#[test]
fn renewal_failure_keeps_old_pair_but_never_extends_its_validity_or_retry_budget() {
    let fixture = Fixture::new();
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let identity = fixture.identity(&cx).await;
        let before = identity.state().unwrap().active.clone().unwrap();
        fixture.reply.lock().unwrap().code = 403;
        due(&identity, &cx);
        assert_eq!(identity.refresh(&cx).await, Err(Error::LocalApiDenied));
        assert!(Arc::ptr_eq(
            &before,
            identity.state().unwrap().active.as_ref().unwrap()
        ));
        let checkpoint = identity.status(&cx).unwrap();
        assert_eq!(checkpoint.generation, 1);
        assert_eq!(identity.refresh(&cx).await, Err(Error::CertificateNotDue));
        assert_eq!(identity.status(&cx).unwrap(), checkpoint);
        let node = fixture.api.node_identity(&cx).await.unwrap();
        assert!(identity.quic_handshake(&cx, &node, vec![]).is_ok());
        for _ in 0..10 {
            due(&identity, &cx);
            assert_eq!(identity.refresh(&cx).await, Err(Error::LocalApiDenied));
        }
        assert_eq!(
            identity.state().unwrap().retry_us,
            micros(identity.policy.retry_maximum).unwrap()
        );
    });
}
#[test]
fn slow_issuance_keeps_identity_lane_available_and_stop_prevents_late_publication() {
    let fixture = Fixture::new();
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let identity = fixture.identity(&cx).await;
        let before = identity.state().unwrap().active.clone().unwrap();
        fixture.hold.store(true, Ordering::Release);
        due(&identity, &cx);
        let other = async {
            until_call(&cx, &fixture, 2).await;
            let node = fixture.api.node_identity(&cx).await.unwrap();
            assert!(identity.quic_handshake(&cx, &node, vec![]).is_ok());
            assert!(Arc::ptr_eq(
                &before,
                identity.state().unwrap().active.as_ref().unwrap()
            ));
            assert_eq!(identity.refresh(&cx).await, Err(Error::Busy));
            assert!(matches!(
                fixture
                    .api
                    .native_server_identity(&cx, roots(), CertificatePolicy::default())
                    .await,
                Err(Error::Busy)
            ));
            identity.clone().stop();
            fixture.hold.store(false, Ordering::Release);
        };
        let (result, ()) = both(identity.refresh(&cx), other).await;
        assert_eq!(result, Err(Error::Revoked));
        assert_eq!(identity.status(&cx), Err(Error::Revoked));
        assert!(identity.state().unwrap().active.is_none());
    });
}
#[test]
fn cancelled_refresh_releases_only_its_request_and_retains_old_valid_pair() {
    let fixture = Fixture::new();
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let identity = fixture.identity(&cx).await;
        let before = identity.state().unwrap().active.clone().unwrap();
        fixture.hold.store(true, Ordering::Release);
        due(&identity, &cx);
        let mut refreshing = Box::pin(identity.refresh(&cx));
        poll_fn(|task| {
            assert!(refreshing.as_mut().poll(task).is_pending());
            if fixture.cert_calls.load(Ordering::SeqCst) == 2 {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
        drop(refreshing);
        assert!(!fixture.api.certificate_busy.load(Ordering::Acquire));
        assert!(!identity.state().unwrap().renewing);
        assert!(Arc::ptr_eq(
            &before,
            identity.state().unwrap().active.as_ref().unwrap()
        ));
        assert_eq!(identity.refresh(&cx).await, Err(Error::CertificateNotDue));
        fixture.hold.store(false, Ordering::Release);
        let node = fixture.api.node_identity(&cx).await.unwrap();
        assert!(identity.quic_handshake(&cx, &node, vec![]).is_ok());
    });
}
#[test]
fn issuance_timeout_and_identity_change_refuse_without_publishing_keys() {
    let fixture = Fixture::new();
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        fixture.hold.store(true, Ordering::Release);
        let policy = CertificatePolicy {
            request_timeout: Duration::from_millis(100),
            ..Default::default()
        };
        assert!(matches!(
            fixture
                .api
                .native_server_identity(&cx, roots(), policy)
                .await,
            Err(Error::Timeout)
        ));
        assert!(!fixture.api.certificate_busy.load(Ordering::Acquire));
        fixture.hold.store(false, Ordering::Release);
        let identity = fixture.identity(&cx).await;
        fixture.hold.store(true, Ordering::Release);
        due(&identity, &cx);
        let calls = fixture.cert_calls.load(Ordering::SeqCst);
        let changed = async {
            until_call(&cx, &fixture, calls + 1).await;
            fixture.status.lock().unwrap()["Self"]["PublicKey"] =
                json!(format!("nodekey:{}", "9".repeat(64)));
            fixture.hold.store(false, Ordering::Release);
        };
        let (result, ()) = both(identity.refresh(&cx), changed).await;
        assert_eq!(result, Err(Error::IdentityChanged));
        assert_eq!(identity.status(&cx), Err(Error::Revoked));
    });
}
#[test]
fn fresh_host_metadata_and_original_origin_are_required_for_every_handshake() {
    let fixture = Fixture::new();
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let identity = fixture.identity(&cx).await;
        let foreign = Fixture::new();
        let foreign_node = foreign.api.node_identity(&cx).await.unwrap();
        assert!(matches!(
            identity.quic_handshake(&cx, &foreign_node, vec![]),
            Err(Error::IdentityMismatch)
        ));
        let mut node = fixture.api.node_identity(&cx).await.unwrap();
        node.expires_us = now(&cx).unwrap();
        assert!(matches!(
            identity.quic_handshake(&cx, &node, vec![]),
            Err(Error::Expired)
        ));
        let node = fixture.api.node_identity(&cx).await.unwrap();
        assert!(matches!(
            identity.quic_handshake(&cx, &node, vec![0; PARAMETER_LIMIT + 1]),
            Err(Error::InvalidPolicy)
        ));
        assert!(identity.quic_handshake(&cx, &node, vec![]).is_ok());
        fixture.status.lock().unwrap()["Self"]["DNSName"] = json!("replacement.fixture.ts.net.");
        let node = fixture.api.node_identity(&cx).await.unwrap();
        assert!(matches!(
            identity.quic_handshake(&cx, &node, vec![]),
            Err(Error::IdentityChanged)
        ));
        assert_eq!(identity.status(&cx), Err(Error::Revoked));
    });
}
#[test]
fn root_credentials_are_checked_before_any_http_request() {
    let mut fixture = Fixture::new();
    fixture.api.daemon_uid = fixture.api.daemon_uid.wrapping_add(1);
    runtime().block_on(async {
        assert!(matches!(
            fixture
                .api
                .native_server_identity(
                    &Cx::current().unwrap(),
                    roots(),
                    CertificatePolicy::default()
                )
                .await,
            Err(Error::UntrustedLocalApi)
        ));
    });
    assert_eq!(fixture.cert_calls.load(Ordering::SeqCst), 0);
}
#[test]
fn actual_quic_uses_the_localapi_pair_and_rotated_leaf() {
    let fixture = Fixture::new();
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let identity = fixture.identity(&cx).await;
        for generation in 1..=2 {
            if generation == 2 {
                *fixture.reply.lock().unwrap() = Reply::pair("two");
                due(&identity, &cx);
                identity.refresh(&cx).await.unwrap();
            }
            let cfg = NativeQuicConnectionConfig {
                max_local_bidi: 0,
                max_local_uni: 8,
                send_window: 65536,
                recv_window: 65536,
                connection_send_limit: 524_288,
                connection_recv_limit: 524_288,
                max_datagram_frame_size: 1200,
                ..Default::default()
            };
            let mut parameters = vec![];
            TransportParameters {
                initial_max_data: Some(cfg.connection_recv_limit),
                initial_max_stream_data_uni: Some(cfg.recv_window),
                initial_max_streams_bidi: Some(0),
                initial_max_streams_uni: Some(8),
                max_datagram_frame_size: Some(1200),
                ..Default::default()
            }
            .encode(&mut parameters)
            .unwrap();
            let client_config = client_config(
                vec![std::fs::read(pki().join("ca.der")).unwrap().into()],
                vec![ALPN.to_vec()],
            )
            .unwrap();
            let cd = QuicHandshakeDriver::client(
                client_config,
                HOST.to_owned().try_into().unwrap(),
                parameters.clone(),
            )
            .unwrap();
            let node = fixture.api.node_identity(&cx).await.unwrap();
            let sd = identity.quic_handshake(&cx, &node, parameters).unwrap();
            let ce = QuicUdpEndpoint::bind(
                &cx,
                "127.0.0.1:0".parse().unwrap(),
                QuicUdpEndpointConfig::default(),
            )
            .await
            .unwrap();
            let se = QuicUdpEndpoint::bind(
                &cx,
                "127.0.0.1:0".parse().unwrap(),
                QuicUdpEndpointConfig::default(),
            )
            .await
            .unwrap();
            let address = se.local_addr();
            let dcid = ConnectionId::new(b"fr-cert1").unwrap();
            let c = NativeQuicUdpConnection::connect(
                &cx,
                ce,
                address,
                cd,
                dcid,
                ConnectionId::new(b"cert-cli").unwrap(),
                cfg,
                ALPN,
            );
            let s = NativeQuicUdpConnection::accept(
                &cx,
                se,
                sd,
                dcid,
                ConnectionId::new(b"cert-srv").unwrap(),
                cfg,
                ALPN,
            );
            let (c, s) = Box::pin(both(
                timeout(cx.now(), Duration::from_secs(3), c),
                timeout(cx.now(), Duration::from_secs(3), s),
            ))
            .await;
            assert!(c.unwrap().is_ok());
            assert!(s.unwrap().is_ok());
            assert_eq!(identity.status(&cx).unwrap().generation, generation);
        }
    });
}
