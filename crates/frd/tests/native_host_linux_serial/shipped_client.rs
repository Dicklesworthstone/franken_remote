//! The shipped `fr` binary against the production `frd run` composition: the
//! client resolves the host through its own `LocalAPI`, dials with strict TLS
//! from its own tailnet address, negotiates with the shipped client offer and
//! receives the display catalog. Both `LocalAPI`s, the CA and ingress are
//! FIXTURES; UDP/TLS/QUIC, admission, negotiation and both processes are real.
use super::*;
use frd::host_run::{self, Event, Options, Reporter, StopHandle};
use std::{
    io::{Read, Write},
    os::unix::net::UnixListener,
    path::Path,
    process::{Command, Stdio},
    thread::{self, JoinHandle},
    time::Instant,
};

/// The client node's installed-daemon view: it is the fixture peer (n-peer,
/// 100.64.0.2) and the host is its only peer.
fn client_status() -> Vec<u8> {
    let own = format!("nodekey:{}", "2".repeat(64));
    let host = format!("nodekey:{}", "1".repeat(64));
    serde_json::to_vec(&serde_json::json!({
        "Version": "synthetic-namespace-not-live-tailnet-qualification",
        "BackendState": "Running",
        "TailscaleIPs": ["100.64.0.2"],
        "CurrentTailnet": {"Name": "test.invalid", "MagicDNSSuffix": "fixture.ts.net"},
        "Self": {"ID": "n-peer", "NodeID": 2, "PublicKey": own, "UserID": 7,
            "TailscaleIPs": ["100.64.0.2"], "InNetworkMap": true,
            "DNSName": "client.fixture.ts.net."},
        "Peer": {host.clone(): {"ID": "n-host", "NodeID": 1, "PublicKey": host, "UserID": 7,
            "TailscaleIPs": ["100.64.0.1"], "InNetworkMap": true,
            "DNSName": format!("{}.", fixture::HOST)}},
    }))
    .unwrap()
}

struct ClientApi {
    path: PathBuf,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl ClientApi {
    fn new() -> Self {
        let path = fixture::pki().join("client-api");
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (seen, ended) = (requests.clone(), stop.clone());
        let worker = thread::spawn(move || {
            while !ended.load(Ordering::Acquire) {
                let mut socket = match listener.accept() {
                    Ok((socket, _)) => socket,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(e) => panic!("client fixture accept: {e}"),
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
                let text = String::from_utf8_lossy(&request).into_owned();
                let path = text.split_whitespace().nth(1).unwrap_or("").to_string();
                let reply = if path == "/localapi/v0/status?peers=true" {
                    let body = client_status();
                    let mut reply = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .into_bytes();
                    reply.extend(body);
                    reply
                } else {
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_vec()
                };
                seen.lock().unwrap().push(path);
                let _ = socket.write_all(&reply);
            }
        });
        Self {
            path,
            requests,
            stop,
            worker: Some(worker),
        }
    }
}
impl Drop for ClientApi {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        let _ = fs::remove_file(&self.path);
    }
}

fn shipped_client() -> PathBuf {
    // The profile directory (target/debug) holds both `fr` and, some levels
    // down, this test executable.
    std::env::current_exe()
        .unwrap()
        .ancestors()
        .map(|dir| dir.join("fr"))
        .find(|fr| fr.is_file())
        .expect("build the shipped client first: cargo build -p fr-native --bin fr --locked")
}

fn wait_for(child: std::process::Child, limit: Duration) -> std::process::Output {
    let mut child = child;
    let until = Instant::now() + limit;
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= until {
            let _ = child.kill();
            panic!("fr did not finish within {limit:?}");
        }
        thread::sleep(Duration::from_millis(10));
    }
    child.wait_with_output().unwrap()
}

fn wait_for_event(events: &Mutex<Vec<Event>>, limit: Duration, want: fn(&Event) -> bool) -> bool {
    let until = Instant::now() + limit;
    while !events.lock().unwrap().iter().any(want) {
        if Instant::now() >= until {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
    true
}

fn run_displays(fr: &Path, api: &Path, roots: &Path) -> std::process::Output {
    let port = address().port().to_string();
    wait_for(
        Command::new(fr)
            .args(["displays", "n-host", "--experimental-native", "--socket"])
            .arg(api)
            .arg("--trust-roots")
            .arg(roots)
            .args(["--port", &port, "--json"])
            .env_remove("DISPLAY")
            .env_remove("XAUTHORITY")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
        Duration::from_secs(30),
    )
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress"]
fn shipped_fr_displays_negotiates_with_frd_run() {
    let fr = shipped_client();
    let api = fixture::Api::new();
    let tools = Tools::new();
    let (worker, _trace) = super::persistent_desktop::source_script("normal");
    let roots = fixture::pki().join("ca.pem");
    let options = Options {
        socket: Some(api.path.clone()),
        port: address().port(),
        interface: "fr-fixture".into(),
        worker,
        display: ":0".into(),
        xauthority: None,
        trust_roots: roots.clone(),
        sharing: fr_tailnet::Scope::OwnUser,
        fps: 30,
        bitrate: 2_000_000,
        ingress_tools: Some((tools.0.join("nft"), tools.0.join("ip"))),
        once: true,
        handle_signals: false,
    };
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let report: Reporter = Arc::new(move |event| sink.lock().unwrap().push(event));
    let stop = Arc::new(StopHandle::default());
    let host_stop = stop.clone();
    let host = thread::spawn(move || host_run::run(&options, &report, &host_stop));

    assert!(
        wait_for_event(&events, Duration::from_secs(20), |e| matches!(
            e,
            Event::Listening { .. }
        )),
        "frd never listened: {events:?}"
    );
    let client_api = ClientApi::new();
    let output = run_displays(&fr, &client_api.path, &roots);
    // The client closed its session; the host must notice and finish that peer
    // on its own (it then keeps listening for the next one).
    wait_for_event(&events, Duration::from_secs(10), |e| {
        matches!(e, Event::PeerFinished { .. })
    });
    stop.request();
    let result = host.join().unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "fr displays failed: {stdout} {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["outcome"], "success");
    assert_eq!(report["session_closed"], true);
    assert_eq!(report["decoder_started"], false);
    assert_eq!(report["input_requested"], false);
    assert_eq!(report["transport_qualified"], false);
    let displays = report["displays"].as_array().unwrap();
    assert_eq!(displays.len(), 1, "{report}");
    assert!(displays[0]["pixel_width"].as_u64().unwrap() > 0);

    let requests = client_api.requests.lock().unwrap().clone();
    assert_ne!(requests.len(), 0);
    assert!(
        requests
            .iter()
            .all(|p| p == "/localapi/v0/status?peers=true"),
        "{requests:?}"
    );
    assert_ne!(api.whois.load(Ordering::SeqCst), 0, "host asked WhoIs");

    assert_eq!(result, Ok(()));
    let events = events.lock().unwrap();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::PeerFinished { .. })),
        "{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::CleanupFailed { .. })),
        "{events:?}"
    );
    assert!(matches!(events.last(), Some(Event::Stopped)), "{events:?}");
    assert_eq!(tools.state()["deleted"], 1);
}
