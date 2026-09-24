//! The production `frd run` composition (`frd::host_run::run`) end to end: fixture
//! `LocalAPI`, fixture nft/ip, test CA and a scripted capture worker; actual
//! UDP/TLS, admission, Host negotiation, native source launch, media delivery to
//! a real viewer, stop, and the fixed cleanup order. FIXTURES, not live tailnet.
use super::*;
use frd::host_run::{self, Event, Options, Reporter, StopHandle};

// Share the already-registered protocol viewer; no parallel harness.
use super::desktop::client::Client;

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress"]
fn frd_run_serves_a_real_viewer_and_stops_in_cleanup_order() {
    let api = fixture::Api::new();
    let tools = Tools::new();
    let (worker, trace) = super::persistent_desktop::source_script("normal");
    let options = options(&api, &tools, worker);
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let report: Reporter = Arc::new(move |event| sink.lock().unwrap().push(event));
    let stop = Arc::new(StopHandle::default());
    let host_stop = stop.clone();
    let host = std::thread::spawn(move || host_run::run(&options, &report, &host_stop));

    let runtime = network::runtime();
    let client = runtime.request_cx_with_budget(Budget::INFINITE);
    let listening = || {
        events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, Event::Listening { .. }))
    };
    runtime.block_on(async {
        timeout(client.now(), Duration::from_secs(20), async {
            until(&client, listening).await;
            assert!(
                !trace.exists(),
                "no capture child before a peer is admitted"
            );
            let native = fixture::client(&client, address()).await;
            let viewer = Viewer::new(
                client.clone(),
                native,
                super::persistent_desktop::offer(),
                fr_transport::quic::Policy::default(),
                Duration::from_secs(3),
            )
            .unwrap();
            let mut viewer = Box::pin(Client::start(client.clone(), viewer)).await;
            viewer.ready().await;
            assert_eq!(viewer.frames.first(), Some(&0));
            assert!(trace.exists(), "capture child launched after admission");
            // The share must outlive the initial three-second authority lease:
            // source consent and viewer observation are renewed, not expired.
            let until = network::clock(&client) + 4_000_000;
            while network::clock(&client) < until {
                viewer.turn().await;
            }
            assert!(
                !events
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|e| matches!(e, Event::ShareEnded { .. })),
                "share ended before stop: {:?}",
                events.lock().unwrap()
            );
            stop.request();
        })
        .await
        .unwrap();
    });
    let result = host.join().unwrap();
    assert_eq!(result, Ok(()));
    let events = events.lock().unwrap();
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::CleanupFailed { .. })),
        "{events:?}"
    );
    assert!(matches!(events.last(), Some(Event::Stopped)), "{events:?}");
    assert_eq!(tools.state()["created"], 1);
    assert_eq!(tools.state()["deleted"], 1);
    assert!(UdpSocket::bind(address()).is_ok(), "listener retired");
}

fn options(api: &fixture::Api, tools: &Tools, worker: PathBuf) -> Options {
    Options {
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
        once: true,
        handle_signals: false,
    }
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress"]
fn frd_run_once_returns_the_source_failure_after_cleanup() {
    failed_share(false);
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress"]
fn frd_run_never_restarts_after_ingress_cleanup_failed() {
    failed_share(true);
}

fn failed_share(fail_cleanup: bool) {
    let api = fixture::Api::new();
    let tools = Tools::new();
    let (worker, _) = super::persistent_desktop::source_script("normal");
    // A real child exits before replying to discovery. This cannot be called a
    // successful one-shot share or justify restarting over unretired ingress.
    fs::write(&worker, "#!/usr/bin/python3\nraise SystemExit(7)\n").unwrap();
    let mut options = options(&api, &tools, worker);
    options.once = !fail_cleanup;
    if fail_cleanup {
        let nft = tools.0.join("nft");
        let script = fs::read_to_string(&nft).unwrap().replace(
            "script = sys.stdin.read(4096)",
            "script = sys.stdin.read(4096)\n    if script.startswith('delete table'): raise SystemExit(1)",
        );
        fs::write(nft, script).unwrap();
    }
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let report: Reporter = Arc::new(move |event| sink.lock().unwrap().push(event));
    let stop = Arc::new(StopHandle::default());
    let host_stop = stop.clone();
    let host = std::thread::spawn(move || host_run::run(&options, &report, &host_stop));
    let runtime = network::runtime();
    let client = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        timeout(client.now(), Duration::from_secs(12), async {
            until(&client, || {
                events
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|e| matches!(e, Event::Listening { .. }))
            })
            .await;
            let native = fixture::client(&client, address()).await;
            let mut viewer = Viewer::new(
                client.clone(),
                native,
                super::persistent_desktop::offer(),
                fr_transport::quic::Policy::default(),
                Duration::from_secs(3),
            )
            .unwrap();
            while !viewer.is_complete() && !host.is_finished() {
                if viewer.drive(Duration::from_millis(5)).await.is_err() {
                    break;
                }
            }
            // Keep the original connection alive until the host has handled its
            // real source failure. No fabricated decoder reply or reconnect.
            until(&client, || host.is_finished()).await;
        })
        .await
        .unwrap();
    });
    let result = host.join().unwrap();
    let events = events.lock().unwrap();
    if fail_cleanup {
        assert_eq!(result.as_ref().unwrap_err().code(), "cleanup_incomplete");
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::CleanupFailed { stage: "ingress" }))
        );
        assert_eq!(tools.state()["deleted"], 0);
    } else {
        assert!(
            matches!(result, Err(host_run::Error::Desktop(_))),
            "{result:?}"
        );
        assert_eq!(tools.state()["deleted"], 1);
    }
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::Listening { .. }))
            .count(),
        1
    );
    assert_eq!(tools.state()["created"], 1);
    assert!(matches!(events.last(), Some(Event::Stopped)), "{events:?}");
    assert!(UdpSocket::bind(address()).is_ok());
}
