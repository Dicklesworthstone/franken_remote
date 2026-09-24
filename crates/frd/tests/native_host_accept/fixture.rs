//! Real credential-checked Unix HTTP; metadata and CA are explicit fixtures.
use super::*;
use asupersync::{
    net::quic_native::{
        NativeQuicConnectionConfig, NativeQuicUdpConnection, QuicUdpEndpoint,
        QuicUdpEndpointConfig,
        handshake_driver::{QuicHandshakeDriver, client_config},
    },
    tls::{Certificate, RootCertStore},
};
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    os::unix::net::UnixListener,
    path::{Path, PathBuf},
    process::Command,
    sync::{OnceLock, atomic::AtomicUsize},
    thread::{self, JoinHandle},
};
pub const HOST: &str = "host.fixture.ts.net";
pub fn pki() -> &'static Path {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let dir = network::pki();
        let run = |args: &[&str]| {
            let result = Command::new("openssl").current_dir(dir).args(args).output().unwrap();
            assert!(result.status.success(), "fixture openssl: {}", String::from_utf8_lossy(&result.stderr));
        };
        run(&["req","-newkey","ec","-pkeyopt","ec_paramgen_curve:P-256","-nodes","-keyout","host.key","-out","host.csr","-subj","/CN=Host fixture"]);
        fs::write(dir.join("host.ext"), format!("subjectAltName=DNS:{HOST}\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\n")).unwrap();
        run(&["x509","-req","-in","host.csr","-CA","ca.pem","-CAkey","ca.key","-CAcreateserial","-days","1","-extfile","host.ext","-out","host.pem"]);
        dir.to_owned()
    })
}
pub fn roots() -> RootCertStore {
    let mut roots = RootCertStore::empty();
    roots
        .add(&Certificate::from_der(
            fs::read(pki().join("ca.der")).unwrap(),
        ))
        .unwrap();
    roots
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Allowed,
    OtherUser,
    Unverifiable,
    ChangeAfterWhoIs,
}
fn metadata(mode: Mode, changed: bool) -> (Value, Value) {
    let key = format!("nodekey:{}", "2".repeat(64));
    let user = if mode == Mode::OtherUser { 8 } else { 7 };
    let peer = json!({"ID":"n-peer","NodeID":2,"PublicKey":key,"UserID":user,
        "TailscaleIPs":["100.64.0.2"],"InNetworkMap":true,"DNSName":"client.fixture.ts.net."});
    let status = json!({"Version":"synthetic-namespace-not-live-tailnet-qualification","BackendState":"Running",
        "TailscaleIPs":["100.64.0.1"],"CurrentTailnet":{"Name":"test.invalid","MagicDNSSuffix":"fixture.ts.net"},
        "Self":{"ID":"n-host","NodeID":1,"PublicKey":format!("nodekey:{}", "1".repeat(64)),"UserID":7,
            "TailscaleIPs":["100.64.0.1"],"InNetworkMap":true,"DNSName":if changed { "changed.fixture.ts.net.".to_string() } else { format!("{HOST}.") }},
        "Peer":{key.clone():peer}});
    let mut who = json!({"Node":{"ID":2,"StableID":"n-peer","Key":key,"User":user,"Addresses":["100.64.0.2/32"],"MachineAuthorized":true},
        "UserProfile":{"ID":user,"LoginName":"private@fixture.invalid"}});
    if mode == Mode::Unverifiable {
        who["Node"]
            .as_object_mut()
            .unwrap()
            .remove("MachineAuthorized");
    }
    (status, who)
}
pub struct Api {
    pub client: LocalApi,
    #[allow(dead_code)] // read by the frd-run composition test only
    pub path: PathBuf,
    pub mode: Arc<Mutex<Mode>>,
    pub whois: Arc<AtomicUsize>,
    pub calls: Arc<AtomicUsize>,
    pub requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl Api {
    pub fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = pki().join(format!("host-api-{}", NEXT.fetch_add(1, Ordering::SeqCst)));
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let client = LocalApi::new(&path).unwrap();
        let mode = Arc::new(Mutex::new(Mode::Allowed));
        let whois = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (state, counted, all, paths, ended) = (
            mode.clone(),
            whois.clone(),
            calls.clone(),
            requests.clone(),
            stop.clone(),
        );
        let worker = thread::spawn(move || {
            let mut changed = false;
            let mut pair = fs::read(pki().join("host.key")).unwrap();
            pair.extend(fs::read(pki().join("host.pem")).unwrap());
            while !ended.load(Ordering::Acquire) {
                let (mut socket, _) = match listener.accept() {
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
                socket
                    .set_write_timeout(Some(Duration::from_millis(300)))
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
                assert!(request.ends_with(b"\r\n\r\n"));
                let request = String::from_utf8(request).unwrap();
                assert!(request.contains("Host: local-tailscaled.sock\r\n"));
                let path = request.split_whitespace().nth(1).unwrap();
                all.fetch_add(1, Ordering::SeqCst);
                {
                    let mut paths = paths.lock().unwrap();
                    assert!(paths.len() < 1024);
                    paths.push(path.to_string());
                }
                let mode = *state.lock().unwrap();
                let (status, who) = metadata(mode, changed);
                let (body, kind) = if path.starts_with("/localapi/v0/status?") {
                    (serde_json::to_vec(&status).unwrap(), "application/json")
                } else if path.starts_with("/localapi/v0/whois?addr=100.64.0.2%3A") {
                    counted.fetch_add(1, Ordering::SeqCst);
                    if mode == Mode::ChangeAfterWhoIs {
                        changed = true;
                    }
                    (serde_json::to_vec(&who).unwrap(), "application/json")
                } else {
                    assert_eq!(
                        path,
                        format!("/localapi/v0/cert/{HOST}?type=pair&min_validity=24h")
                    );
                    (pair.clone(), "text/plain")
                };
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket
                    .write_all(header.as_bytes())
                    .and_then(|()| socket.write_all(&body));
            }
        });
        Self {
            path,
            client,
            mode,
            whois,
            calls,
            requests,
            stop,
            worker: Some(worker),
        }
    }
    pub async fn identity(&self, cx: &Cx) -> fr_tailnet::NativeServerIdentity {
        self.client
            .native_server_identity(cx, roots(), fr_tailnet::CertificatePolicy::default())
            .await
            .unwrap()
    }
}
impl Drop for Api {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.worker.take().unwrap().join().unwrap();
    }
}
pub fn request() -> Request {
    use fr_core::{
        authority::AuthorityPolicy,
        ids::{HostBootId, OsSessionId, RemoteSessionId},
    };
    Request {
        connection_id: asupersync::net::quic_core::ConnectionId::new(b"host-original-id").unwrap(),
        admission: fr_tailnet::GrantPolicy::default(),
        session: frd::session_startup::Configuration {
            offer: offer(),
            binding: fr_wire::negotiation::ControlBinding {
                id: 7,
                host_boot: HostBootId::from_raw(1),
                os_session: OsSessionId::from_raw(2),
                remote_session: RemoteSessionId::from_raw(3),
            },
            require_approval: true,
            startup_timeout: Duration::from_secs(3),
            authority: AuthorityPolicy::plan_defaults(),
            transport: fr_transport::quic::Policy::default(),
        },
    }
}
/// Installed daemons omit `MachineAuthorized`; under own-user scope equal user
/// ids are sufficient evidence, so "unverifiable" is exercised with the
/// tailnet-wide scope, where absent evidence must still refuse.
pub fn request_for(mode: Mode) -> Request {
    let mut request = request();
    if mode == Mode::Unverifiable {
        request.admission.scope = fr_tailnet::Scope::Tailnet;
    }
    request
}
pub fn offer() -> fr_wire::negotiation::Offer {
    fr_wire::negotiation::Offer {
        versions: vec![0],
        profile: 1,
        profile_version: 0,
        role: fr_wire::negotiation::Role::Observe,
        limits: fr_core::limits::ProtocolLimits::ABSOLUTE,
        capabilities: vec![],
    }
}
pub async fn listener(cx: &Cx) -> Listener {
    Listener::bind(
        cx,
        "100.64.0.1:0".parse().unwrap(),
        native_accept::Configuration::default(),
    )
    .await
    .unwrap()
}
pub async fn client(cx: &Cx, address: SocketAddr) -> NativeQuicUdpConnection {
    let tls = client_config(
        vec![fs::read(pki().join("ca.der")).unwrap().into()],
        vec![fr_transport::quic::ALPN.to_vec()],
    )
    .unwrap();
    let driver = QuicHandshakeDriver::client(
        tls,
        HOST.to_owned().try_into().unwrap(),
        native_accept::transport_parameters(),
    )
    .unwrap();
    let socket = QuicUdpEndpoint::bind(
        cx,
        "100.64.0.2:0".parse().unwrap(),
        QuicUdpEndpointConfig::default(),
    )
    .await
    .unwrap();
    let id = |bytes| asupersync::net::quic_core::ConnectionId::new(bytes).unwrap();
    NativeQuicUdpConnection::connect(
        cx,
        socket,
        address,
        driver,
        id(b"unknown-initial"),
        id(b"unknown-client"),
        NativeQuicConnectionConfig {
            max_local_bidi: 0,
            max_local_uni: 8,
            send_window: 65_536,
            recv_window: 65_536,
            connection_send_limit: 524_288,
            connection_recv_limit: 524_288,
            max_datagram_frame_size: 1200,
            ..Default::default()
        },
        fr_transport::quic::ALPN,
    )
    .await
    .unwrap()
}
pub fn boundary(address: SocketAddr, live: Arc<AtomicBool>) -> IngressCheck {
    // Explicit fixture lifetime only, NOT proof of production TUN ingress.
    Arc::new(move |bound| bound == address && live.load(Ordering::Acquire))
}
