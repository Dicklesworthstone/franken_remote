//! Repeated real CLI inspections and a deliberately silent observer on the
//! existing synthetic-boundary / real UDP+TLS host-run lane.
use super::super::desktop::client::Client;
use super::*;

struct Hosted {
    tools: Tools,
    _api: fixture::Api,
    roots: PathBuf,
    events: Arc<Mutex<Vec<Event>>>,
    stop: Arc<StopHandle>,
    thread: Option<JoinHandle<Result<(), host_run::Error>>>,
}
impl Hosted {
    fn new() -> Self {
        let api = fixture::Api::new();
        let tools = Tools::new();
        let (worker, _) = super::super::persistent_desktop::source_script("normal");
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
            input_agent: None,
        };
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let report: Reporter = Arc::new(move |event| sink.lock().unwrap().push(event));
        let stop = Arc::new(StopHandle::default());
        let host_stop = stop.clone();
        let thread = thread::spawn(move || host_run::run(&options, &report, &host_stop));
        let hosted = Self {
            tools,
            _api: api,
            roots,
            events,
            stop,
            thread: Some(thread),
        };
        hosted.wait_count(1, |e| matches!(e, Event::Listening { .. }));
        hosted
    }
    fn count(&self, want: fn(&Event) -> bool) -> usize {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| want(e))
            .count()
    }
    fn wait_count(&self, count: usize, want: fn(&Event) -> bool) {
        let until = Instant::now() + Duration::from_secs(10);
        while self.count(want) < count {
            assert!(
                Instant::now() < until,
                "host stopped making progress: {:?}",
                self.events
            );
            assert!(
                !self.thread.as_ref().unwrap().is_finished(),
                "host exited: {:?}",
                self.events
            );
            thread::sleep(Duration::from_millis(5));
        }
    }
    fn finish(mut self) {
        self.stop.request();
        assert_eq!(self.thread.take().unwrap().join().unwrap(), Ok(()));
        assert_eq!(self.count(|e| matches!(e, Event::CleanupFailed { .. })), 0);
        assert!(matches!(
            self.events.lock().unwrap().last(),
            Some(Event::Stopped)
        ));
        assert_eq!(self.tools.state()["created"], self.tools.state()["deleted"]);
        assert!(
            UdpSocket::bind(address()).is_ok(),
            "original listener retired"
        );
    }
}
impl Drop for Hosted {
    fn drop(&mut self) {
        self.stop.request();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
async fn viewer(cx: &Cx) -> Box<Client> {
    let native = fixture::client(cx, address()).await;
    let viewer = Viewer::new(
        cx.clone(),
        native,
        super::super::persistent_desktop::offer(),
        fr_transport::quic::Policy::default(),
        Duration::from_secs(3),
    )
    .unwrap();
    let mut viewer = Box::pin(Client::start(cx.clone(), viewer)).await;
    viewer.ready().await;
    assert_eq!(viewer.frames.first(), Some(&0));
    viewer
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress"]
fn six_shipped_inspections_do_not_exhaust_the_host_and_a_fresh_viewer_still_streams() {
    let host = Hosted::new();
    let api = ClientApi::new();
    let fr = shipped_client();
    for inspection in 1..=6 {
        let output = run_displays(&fr, &api.path, &host.roots);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["outcome"], "success");
        assert_eq!(report["session_closed"], true);
        assert_eq!(report["decoder_started"], false);
        assert_eq!(report["input_requested"], false);
        assert_eq!(report["transport_qualified"], false);
        assert_eq!(report["displays"].as_array().unwrap().len(), 1);
        host.wait_count(inspection, |e| matches!(e, Event::PeerFinished { .. }));
        host.wait_count(inspection + 1, |e| matches!(e, Event::Listening { .. }));
        assert_eq!(host.tools.state()["deleted"], inspection);
        assert_eq!(host.tools.state()["created"], inspection + 1);
    }
    let runtime = network::runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        timeout(cx.now(), Duration::from_secs(12), async {
            let mut viewer = viewer(&cx).await;
            let until = network::clock(&cx) + 4_000_000;
            while network::clock(&cx) < until {
                viewer.turn().await;
            }
            assert_eq!(host.count(|e| matches!(e, Event::ShareEnded { .. })), 6);
            assert_eq!(host.count(|e| matches!(e, Event::PeerFinished { .. })), 6);
            host.stop.request();
        })
        .await
        .unwrap();
    });
    host.finish();
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress"]
fn an_established_viewer_that_stops_answering_renewal_is_still_retired_on_time() {
    let host = Hosted::new();
    let runtime = network::runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        timeout(cx.now(), Duration::from_secs(12), async {
            let viewer = viewer(&cx).await;
            let paused = Instant::now();
            // Retain the actual viewer/connection without polling, closing it,
            // requesting host stop, or answering any subsequent challenge.
            while host.count(|e| matches!(e, Event::PeerFinished { .. })) == 0
                || host.count(|e| matches!(e, Event::ShareEnded { .. })) == 0
            {
                assert!(
                    paused.elapsed() < Duration::from_secs(4),
                    "silent observer retained authority"
                );
                sleep(cx.now(), Duration::from_millis(5)).await;
            }
            assert!(!host.stop.is_requested());
            assert_eq!(host.count(|e| matches!(e, Event::ShareEnded { .. })), 1);
            drop(viewer);
        })
        .await
        .unwrap();
    });
    host.wait_count(2, |e| matches!(e, Event::Listening { .. }));
    assert_eq!(host.tools.state()["deleted"], 1);
    host.finish();
}
