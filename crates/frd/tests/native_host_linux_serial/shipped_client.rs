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

fn count(events: &Mutex<Vec<Event>>, want: fn(&Event) -> bool) -> usize {
    events.lock().unwrap().iter().filter(|e| want(e)).count()
}
fn wait_count(events: &Mutex<Vec<Event>>, want: fn(&Event) -> bool, n: usize) -> bool {
    let until = Instant::now() + Duration::from_secs(30);
    while count(events, want) < n {
        if Instant::now() >= until {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
    true
}
fn listening(e: &Event) -> bool {
    matches!(e, Event::Listening { .. })
}
fn peer_finished(e: &Event) -> bool {
    matches!(e, Event::PeerFinished { .. })
}
fn share_ended(e: &Event) -> bool {
    matches!(e, Event::ShareEnded { .. })
}

/// A real viewer: negotiates, decodes frames for one second, and (unless kept)
/// then vanishes WITHOUT a close, as a killed or unplugged viewer would.
fn view(
    keep: bool,
) -> Option<(
    asupersync::runtime::Runtime,
    Box<super::desktop::client::Client>,
)> {
    let runtime = network::runtime();
    let client = runtime.request_cx_with_budget(Budget::INFINITE);
    let viewer = runtime.block_on(async {
        timeout(client.now(), Duration::from_secs(20), async {
            let native = fixture::client(&client, address()).await;
            let viewer = Viewer::new(
                client.clone(),
                native,
                super::persistent_desktop::offer(),
                fr_transport::quic::Policy::default(),
                Duration::from_secs(3),
            )
            .unwrap();
            let mut viewer = Box::pin(super::desktop::client::Client::start(
                client.clone(),
                viewer,
            ))
            .await;
            viewer.ready().await;
            assert_eq!(viewer.frames.first(), Some(&0), "a fresh share");
            let until = network::clock(&client) + 1_000_000;
            while network::clock(&client) < until {
                viewer.turn().await;
            }
            assert!(viewer.frames.len() >= 3, "{:?}", viewer.frames);
            viewer
        })
        .await
        .unwrap()
    });
    keep.then_some((runtime, viewer))
}

/// `frd run` is a daemon: viewers leaving, before or after their share is up,
/// are peer outcomes. They are reported and never count as host failures.
#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress"]
fn frd_run_keeps_serving_sequential_viewers_and_departing_inspections() {
    let fr = shipped_client();
    let api = fixture::Api::new();
    let tools = Tools::new();
    let (worker, _trace) = super::persistent_desktop::source_script("changing");
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
        once: false,
        handle_signals: false,
    };
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let report: Reporter = Arc::new(move |event| sink.lock().unwrap().push(event));
    let stop = Arc::new(StopHandle::default());
    let host_stop = stop.clone();
    let host = thread::spawn(move || host_run::run(&options, &report, &host_stop));
    let dump = || format!("{:?}", events.lock().unwrap());

    // Viewer A, then viewer B, each served by a fresh share of the same daemon.
    for n in 1..=2 {
        assert!(wait_count(&events, listening, n), "share {n}: {}", dump());
        assert!(view(false).is_none());
        assert!(
            wait_count(&events, peer_finished, n),
            "viewer {n}: {}",
            dump()
        );
    }
    // More departing inspections than the consecutive-failure limit (5).
    let client_api = ClientApi::new();
    for n in 3..=8 {
        assert!(wait_count(&events, listening, n), "share {n}: {}", dump());
        let output = run_displays(&fr, &client_api.path, &roots);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "fr displays {n}: {stdout} {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(json["outcome"], "success");
        assert!(
            wait_count(&events, peer_finished, n),
            "inspection {n}: {}",
            dump()
        );
    }
    // Still listening, and still serving real viewers.
    assert!(wait_count(&events, listening, 9), "share 9: {}", dump());
    assert!(!host.is_finished(), "{}", dump());
    let kept = view(true);
    stop.request();
    let result = host.join().unwrap();
    drop(kept);

    assert_eq!(result, Ok(()), "{}", dump());
    let events = events.lock().unwrap();
    assert!(matches!(events.last(), Some(Event::Stopped)), "{events:?}");
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::CleanupFailed { .. })),
        "{events:?}"
    );
    let kinds = |want: fn(&Event) -> bool| events.iter().filter(|e| want(e)).count();
    assert_eq!(kinds(listening), 9, "{events:?}");
    assert_eq!(kinds(share_ended), 9, "{events:?}");
    // Each departed viewer is reported exactly once, before its share ends; the
    // last share ends by local stop with its viewer still connected.
    assert_eq!(kinds(peer_finished), 8, "{events:?}");
    let shares: Vec<_> = events
        .split(listening)
        .skip(1)
        .map(|share| {
            share
                .iter()
                .map(|e| (peer_finished(e), share_ended(e)))
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(shares.len(), 9, "{events:?}");
    for share in &shares[..8] {
        assert_eq!(share, &[(true, false), (false, true)], "{events:?}");
    }
    assert_eq!(shares[8][0], (false, true), "{events:?}");
    assert_eq!(tools.state()["created"], 9);
    assert_eq!(tools.state()["deleted"], 9);
    assert!(UdpSocket::bind(address()).is_ok(), "listener retired");
}

/// Planted negative for the renewal path: a viewer that stays connected but
/// stops answering renewal challenges must still lose its observation within
/// the provisional lease horizon. Departure handling must never make renewal
/// unconditional.
#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress"]
fn frd_run_expires_a_connected_viewer_that_stops_answering_renewal() {
    let api = fixture::Api::new();
    let tools = Tools::new();
    let (worker, _trace) = super::persistent_desktop::source_script("changing");
    let options = Options {
        socket: Some(api.path.clone()),
        port: address().port(),
        interface: "fr-fixture".into(),
        worker,
        display: ":0".into(),
        xauthority: None,
        trust_roots: fixture::pki().join("ca.pem"),
        sharing: fr_tailnet::Scope::OwnUser,
        fps: 30,
        bitrate: 2_000_000,
        ingress_tools: Some((tools.0.join("nft"), tools.0.join("ip"))),
        once: false,
        handle_signals: false,
    };
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let report: Reporter = Arc::new(move |event| sink.lock().unwrap().push(event));
    let stop = Arc::new(StopHandle::default());
    let host_stop = stop.clone();
    let host = thread::spawn(move || host_run::run(&options, &report, &host_stop));
    let dump = || format!("{:?}", events.lock().unwrap());

    assert!(wait_count(&events, listening, 1), "{}", dump());
    // Frames flowed; from here the viewer's session is never driven again.
    let idle = view(true);
    let silent_since = Instant::now();
    assert!(
        wait_count(&events, peer_finished, 1),
        "never expired: {}",
        dump()
    );
    let lapsed = silent_since.elapsed();
    assert!(lapsed < Duration::from_secs(10), "{lapsed:?}: {}", dump());
    assert_eq!(count(&events, share_ended), 1, "{}", dump());
    // The daemon listens again for the next viewer.
    assert!(wait_count(&events, listening, 2), "{}", dump());
    stop.request();
    assert_eq!(host.join().unwrap(), Ok(()), "{}", dump());
    drop(idle);
}
