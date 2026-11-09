//! macOS Unix-socket `LocalAPI` over a real socket, real HTTP and the real
//! runtime. The fixture daemon is this test process, so it is never root: a
//! production client must refuse it, and only the test-only UID substitution
//! (as in the shared Linux tests) lets reads through. None of this qualifies
//! an installed `tailscaled`; only a live run against one does.
use super::support::{Http, request};
use super::*;
use crate::{DiscoveredPeer, PeerSelector};
use std::{io::Write, sync::Mutex, thread, time::Instant};

/// A fixture daemon that records each request line (without the version).
struct Daemon {
    http: Http,
    path: PathBuf,
    lines: Arc<Mutex<Vec<String>>>,
}
impl Daemon {
    fn start(pause: Duration) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "fr-macsock-{}-{}-{}.sock",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst),
            wall_now().unwrap()
        ));
        let (mut status, who) = fixtures();
        status["Self"]["DNSName"] = json!("local.fixture.ts.net.");
        peer_mut(&mut status)["DNSName"] = json!("remote.fixture.ts.net.");
        let lines = Arc::new(Mutex::new(Vec::new()));
        let seen = lines.clone();
        let http = Http::start(&path, move |mut socket, _| {
            if let Some(text) = request(&mut socket, Duration::from_millis(300), None) {
                let line = text.lines().next().unwrap_or_default();
                let line = line.trim_end_matches(" HTTP/1.1").to_owned();
                let reply = if line.starts_with("GET /localapi/v0/whois?") {
                    response(&who, false)
                } else {
                    response(&status, false)
                };
                seen.lock().unwrap().push(line);
                thread::sleep(pause);
                // A client past its deadline has closed; that is its result.
                let _ = socket.write_all(&reply);
            }
            None
        });
        Self { http, path, lines }
    }
    fn lines(&self) -> Vec<String> {
        self.lines.lock().unwrap().clone()
    }
    /// The test user's UID, substituted only where a test says so.
    fn uid(&self) -> u32 {
        self.http.client.daemon_uid
    }
}

const STATUS: &str = "GET /localapi/v0/status?peers=true";

#[test]
fn a_socket_the_test_user_serves_is_untrusted_before_any_request() {
    let daemon = Daemon::start(Duration::ZERO);
    assert_ne!(daemon.uid(), 0, "run as an unprivileged user");
    // The production constructor: root (0) is the only trusted server UID.
    let production = LocalApi::new(&daemon.path).unwrap();
    assert_eq!(production.daemon_uid, 0);
    assert!(production.app.is_none());
    run_async!(cx, {
        assert_eq!(
            production.node_identity(&cx).await.unwrap_err(),
            Error::UntrustedLocalApi
        );
        assert_eq!(
            production.discover(&cx).await.unwrap_err(),
            Error::UntrustedLocalApi
        );
        assert_eq!(
            production
                .peer_target(&cx, PeerSelector::StableId("n-peer"))
                .await
                .unwrap_err(),
            Error::UntrustedLocalApi
        );
        assert_eq!(
            production
                .authorize_membership(&cx, endpoints(), GrantPolicy::default())
                .await
                .unwrap_err(),
            Error::UntrustedLocalApi
        );
        assert!(!production.busy.load(Ordering::Acquire));
    });
    // Refused on kernel credentials alone: no request was ever written.
    assert_eq!(daemon.lines(), Vec::<String>::new());
}

#[test]
fn socket_reads_carry_the_kernel_reported_server_pid() {
    let daemon = Daemon::start(Duration::ZERO);
    // The fixture server runs in this process, so LOCAL_PEERPID must name it.
    let pid = i32::try_from(std::process::id()).unwrap();
    run_async!(cx, {
        let (body, source) = crate::local::socket::get(
            &daemon.path,
            daemon.uid(),
            "/localapi/v0/status?peers=false",
            metadata::STATUS_BYTES,
            "application/json",
        )
        .await
        .unwrap();
        assert_eq!(source, pid);
        assert!(Status::parse(&body).is_ok());
        let node = daemon.http.client.node_identity(&cx).await.unwrap();
        assert!(node.matches_process(Some(pid)));
        assert!(!node.matches_process(Some(pid + 1)));
        assert_eq!(node.certificate_name(), "local.fixture.ts.net");
        // Admission binds the same daemon PID into its proof.
        let proof = daemon
            .http
            .client
            .authorize_membership(&cx, endpoints(), GrantPolicy::default())
            .await
            .unwrap();
        assert_eq!(proof.daemon_pid, pid);
    });
}

#[test]
fn client_lookups_read_status_whois_status_over_a_configured_socket() {
    let daemon = Daemon::start(Duration::ZERO);
    let api = &daemon.http.client;
    run_async!(cx, {
        let discovery = api.discover(&cx).await.unwrap();
        let peers: Vec<_> = discovery
            .peers()
            .iter()
            .map(DiscoveredPeer::stable_id)
            .collect();
        assert_eq!(peers, ["n-peer"]);
        let target = api
            .peer_target(&cx, PeerSelector::StableId("n-peer"))
            .await
            .unwrap();
        assert_eq!(target.certificate_name(), "remote.fixture.ts.net");
        assert_eq!(
            target.addresses(),
            &["100.64.0.2".parse::<std::net::IpAddr>().unwrap()]
        );
        api.check_peer_target(&cx, &target).unwrap();
        assert!(!api.busy.load(Ordering::Acquire));
    });
    assert_eq!(
        daemon.lines(),
        [
            STATUS,
            STATUS,
            "GET /localapi/v0/whois?addr=100.64.0.2",
            STATUS
        ]
    );
}

#[test]
fn the_one_second_client_budget_bounds_a_stalled_socket_daemon() {
    // The daemon answers after 3 s; the lookup's original second ends it.
    let daemon = Daemon::start(Duration::from_secs(3));
    let api = &daemon.http.client;
    run_async!(cx, {
        let started = Instant::now();
        assert_eq!(api.discover(&cx).await.unwrap_err(), Error::Timeout);
        let elapsed = started.elapsed();
        assert!(elapsed >= Duration::from_millis(900), "{elapsed:?}");
        assert!(elapsed < Duration::from_millis(2500), "{elapsed:?}");
        assert!(!api.busy.load(Ordering::Acquire));
    });
    assert_eq!(daemon.lines(), [STATUS]);
}

#[test]
fn installed_reads_the_fixed_socket_only_while_the_app_publishes_nothing() {
    let daemon = Daemon::start(Duration::ZERO);
    let mut api = LocalApi::installed();
    assert_eq!(
        api.app.as_deref().map(PathBuf::as_path),
        Some(Path::new("/Library/Tailscale"))
    );
    assert_eq!(api.path.as_path(), Path::new("/var/run/tailscaled.socket"));
    assert_eq!(api.daemon_uid, 0);
    // Test-only substitution: the fixed paths, the UID, and a private slot
    // instead of the process-wide installed lookup slot.
    api.path = Arc::new(daemon.path.clone());
    api.busy = Arc::new(AtomicBool::new(false));
    run_async!(cx, {
        // Root daemon required on this route too: the test user is refused.
        api.app = Some(Arc::new(PathBuf::from("/nonexistent/fr-tailnet")));
        assert_eq!(
            api.discover(&cx).await.unwrap_err(),
            Error::UntrustedLocalApi
        );
        api.daemon_uid = daemon.uid();
        // No directory; root-owned without `ipnport`; root-only (0750 root,
        // like the open-source daemon's 0700 state directory).
        for unpublished in ["/nonexistent/fr-tailnet", "/usr/lib", "/private/var/root"] {
            api.app = Some(Arc::new(PathBuf::from(unpublished)));
            api.discover(&cx).await.unwrap();
            api.node_identity(&cx).await.unwrap();
        }
        // A state directory this user can write is refused, never skipped.
        api.app = Some(Arc::new(std::env::temp_dir()));
        assert_eq!(
            api.discover(&cx).await.unwrap_err(),
            Error::UntrustedLocalApi
        );
        assert_eq!(
            api.node_identity(&cx).await.unwrap_err(),
            Error::UntrustedLocalApi
        );
    });
    let lines = daemon.lines();
    // Three discoveries (one read each) and three node snapshots (two each).
    assert_eq!(lines.len(), 9, "{lines:?}");
    assert_eq!(lines.iter().filter(|line| *line == STATUS).count(), 3);
}

#[test]
fn configured_paths_are_validated_as_on_linux_with_the_darwin_sun_path() {
    for bad in [
        "tailscaled.socket",
        "/var/run/../run/tailscaled.socket",
        "/tmp/a\0b",
    ] {
        assert_eq!(
            LocalApi::new(bad).map(|_| ()),
            Err(Error::InvalidEndpoint),
            "{bad:?}"
        );
    }
    // sun_path holds 104 bytes with its NUL on macOS (108 on Linux).
    let fits = format!("/{}", "a".repeat(102));
    let long = format!("/{}", "a".repeat(103));
    assert_eq!((fits.len(), long.len()), (103, 104));
    let api = LocalApi::new(&fits).unwrap();
    assert!(api.app.is_none());
    assert_eq!(api.daemon_uid, 0);
    assert_eq!(
        LocalApi::new(&long).map(|_| ()),
        Err(Error::InvalidEndpoint)
    );
    assert_eq!(
        LocalApi::new("/var/run/tailscaled.socket").map(|api| api.daemon_uid),
        Ok(0)
    );
}

/// Cleared on drop: the live pair holds the node's private key.
struct Cleared(Vec<u8>);
impl Drop for Cleared {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

/// Live, by hand only (`--ignored --exact`): the installed open-source
/// `tailscaled` at its fixed socket through the production transport (root
/// server UID required; nothing substituted). Prints only this node's own
/// HostName/DNSName/online flag, its own `WhoIs` login and node name, a peer
/// count, and the certificate's subject, SAN, expiry and public-key algorithm
/// and size. The private key is never printed, logged or written: only its
/// PEM type label is read, and the buffer is cleared on drop. Only the
/// public leaf certificate is handed to `openssl`, on stdin.
#[test]
#[ignore = "live: needs the installed root tailscaled at /var/run/tailscaled.socket"]
fn live_installed_open_source_tailscaled() {
    use crate::{CertificatePolicy, trust};
    let api = LocalApi::new(crate::local::socket::MACOS_INSTALLED).unwrap();
    assert_eq!(api.daemon_uid, 0);
    run_async!(cx, {
        // (a) Status through the configured socket.
        let started = Instant::now();
        let (bytes, pid) = api
            .get("/localapi/v0/status?peers=false", metadata::STATUS_BYTES)
            .await
            .expect("status read");
        let status: Value = serde_json::from_slice(&bytes).unwrap();
        let me = &status["Self"];
        println!(
            "live status: transport=unix-socket server_uid=0 daemon_pid={pid} \
             HostName={} DNSName={} Online={} ({:?})",
            me["HostName"],
            me["DNSName"],
            me["Online"],
            started.elapsed()
        );
        let node = api.node_identity(&cx).await.expect("node identity");
        println!(
            "live node_identity: certificate_name={} addresses={} same_daemon_pid={}",
            node.certificate_name(),
            node.addresses().len(),
            node.matches_process(Some(pid))
        );
        // (b) WhoIs of this node's own first tailnet address.
        let uri = Request::get("/localapi/v0/whois")
            .query([("addr", node.addresses()[0].to_string())])
            .build()
            .uri;
        let (bytes, whois_pid) = api
            .get(&uri, metadata::WHOIS_BYTES)
            .await
            .expect("whois read");
        let who: Value = serde_json::from_slice(&bytes).unwrap();
        println!(
            "live whois(own address): UserProfile.LoginName={} Node.Name={} same_daemon_pid={}",
            who["UserProfile"]["LoginName"],
            who["Node"]["Name"],
            whois_pid == pid
        );
        // Client lookups: installed() selects this socket on its own when
        // the standalone app has published no endpoint.
        let installed = LocalApi::installed();
        let started = Instant::now();
        let discovery = installed.discover(&cx).await.expect("installed discovery");
        println!(
            "live installed().discover: peers={} ({:?})",
            discovery.peers().len(),
            started.elapsed()
        );
        if let Some(peer) = discovery.peers().first() {
            let started = Instant::now();
            let target = installed
                .peer_target(&cx, PeerSelector::StableId(peer.stable_id()))
                .await
                .expect("installed peer target");
            println!(
                "live installed().peer_target (status/WhoIs/status): ok addresses={} ({:?})",
                target.addresses().len(),
                started.elapsed()
            );
        }
        // (c) The production certificate path: pair fetch, parse, WebPKI
        // verification against the system roots and this node's name.
        let roots = trust::root_store(Path::new(trust::SYSTEM_BUNDLE)).expect("system roots");
        let identity = api
            .native_server_identity(&cx, roots, CertificatePolicy::default())
            .await
            .expect("native server identity");
        let credential = identity.status(&cx).expect("credential status");
        println!(
            "live native_server_identity: generation={} not_after_unix_s={:?}",
            credential.generation,
            credential.not_after_wall_us.map(|us| us / 1_000_000)
        );
        identity.stop();
        print_certificate(&api, node.certificate_name(), pid).await;
    });
}

/// The `openssl x509 -text` lines the live probe may print.
const SHOWN: [&str; 7] = [
    "Subject:",
    "Not After",
    "DNS:",
    "Public Key Algorithm:",
    "Public-Key:",
    "ASN1 OID:",
    "NIST CURVE:",
];

/// The node's cached pair, read once more for its public details only.
async fn print_certificate(api: &LocalApi, name: &str, pid: i32) {
    let uri = format!("/localapi/v0/cert/{name}?type=pair&min_validity=24h");
    let (pair, pair_pid) = api
        .get_typed(&uri, 64 * 1024, "text/plain")
        .await
        .expect("pair read");
    let pair = Cleared(pair);
    let text = std::str::from_utf8(&pair.0).expect("PEM text");
    let key_label = text
        .lines()
        .find(|line| line.starts_with("-----BEGIN ") && line.ends_with("PRIVATE KEY-----"))
        .map(|line| &line[11..line.len() - 5]);
    let end = "-----END CERTIFICATE-----";
    let leaf = text
        .find("-----BEGIN CERTIFICATE-----")
        .and_then(|start| {
            text[start..]
                .find(end)
                .map(|len| &text[start..start + len + end.len()])
        })
        .expect("leaf certificate");
    println!(
        "live pair: key PEM label={key_label:?} certificate_blocks={} same_daemon_pid={}",
        text.matches("-----BEGIN CERTIFICATE-----").count(),
        pair_pid == pid
    );
    // Only the public leaf certificate leaves this process, on stdin.
    let mut openssl = std::process::Command::new("/usr/bin/openssl")
        .args(["x509", "-noout", "-text"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .unwrap();
    openssl
        .stdin
        .take()
        .unwrap()
        .write_all(leaf.as_bytes())
        .unwrap();
    let output = openssl.wait_with_output().unwrap();
    assert!(output.status.success());
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let line = line.trim();
        if SHOWN.iter().any(|prefix| line.starts_with(prefix)) {
            println!("live certificate: {line}");
        }
    }
}
