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
        once: true,
        handle_signals: false,
    };
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
