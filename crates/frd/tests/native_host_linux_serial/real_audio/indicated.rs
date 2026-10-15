//! An audio share under `frd run --observation-indicator`: the real
//! `fr-observation-indicator` child shows "Stop sharing" while host playback
//! reaches the viewer, and the local user's Stop click (attributed to Xvfb's
//! own pointer, an explicit fixture) ends video and audio together.
use super::super::real_observation_indicator::{indicator, peer};
use super::*;
use frd::host_policy::{Approval, Change, Store};
use frd::host_run::policy::Configuration;

#[test]
#[ignore = "explicit isolated namespace; synthetic ingress; two Xvfb and two private PulseAudio daemons; Xvfb device attribution fixture"]
fn the_observation_indicator_shows_during_an_audio_share_and_its_stop_ends_it() {
    private_root_home();
    let (fr, worker) = (sibling("fr"), sibling("fr-media-worker"));
    let host_pulse = Pulse::start("fr_host");
    let client_pulse = Pulse::start("fr_view");
    let _player = Player::start(&host_pulse, WARMUP_HZ);
    let host = Xvfb::start("640x480x24");
    let viewer = Xvfb::start("800x600x24");
    set_root(&host.display, COLOUR);
    warp_pointer(&host.display, 16, 16);
    let recorder = Recorder::start(&client_pulse);
    let api = fixture::Api::new();
    let tools = Tools::new();
    let roots = fixture::pki().join("ca.pem");
    let options = options(
        &api,
        &tools,
        &worker,
        &host.display,
        &roots,
        Some(AudioOptions {
            server: host_pulse.socket.clone(),
            sink: None,
        }),
    );
    let (daemon, events) = indicated_host(options, &tools);
    let dump = || format!("{:?}", events.lock().unwrap());
    assert!(
        wait_for_event(&events, Duration::from_secs(20), |e| matches!(
            e,
            Event::Listening { .. }
        )),
        "frd never listened: {}",
        dump()
    );
    let (shown, ..) = indicator(&host.display).expect("no Stop sharing indicator while listening");
    let client_api = ClientApi::new();
    let mut client = connect_audio(
        &fr,
        &client_api.path,
        &roots,
        &viewer.display,
        &worker,
        &client_pulse,
        false,
    );
    let window = await_window(&viewer.display, &mut client);
    let heard = recorder.await_tone(WARMUP_HZ, Duration::from_secs(20));
    let during = indicator(&host.display).map(|(w, ..)| w);
    // The local user's Stop: video and audio end with the share.
    let stopped = Instant::now();
    peer(&host.display, shown, "device-click");
    let output = wait_for(client, Duration::from_secs(30));
    let ended = stopped.elapsed();
    let result = daemon.join().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    println!(
        "indicated audio share: Stop -> client end {ended:?}; fr: {}",
        stdout.trim()
    );
    assert!(
        window.is_some(),
        "no host picture: {stdout}; host: {}",
        dump()
    );
    assert!(
        heard.is_some(),
        "host tone never reached the client: {stdout}"
    );
    assert_eq!(
        during,
        Some(shown),
        "the indicator left during the audio share"
    );
    assert!(result.is_ok(), "frd run: {result:?}; host: {}", dump());
    assert!(
        stdout.contains("\"host_stopping\""),
        "the viewer was not told the host stopped sharing: {stdout}; host: {}",
        dump()
    );
    // Nothing plays after the share ended (allow the sink's own tail).
    thread::sleep(Duration::from_millis(500));
    recorder.drain();
    let (seen, total) = recorder.count_tone(WARMUP_HZ, Duration::from_secs(1));
    assert_eq!(seen, 0, "{seen} of {total} blocks still carried the tone");
    assert_eq!(indicator(&host.display), None, "indicator outlived the run");
}

type Daemon = thread::JoinHandle<Result<(), frd::host_indicator::Error>>;
/// `frd run --observation-indicator` with no local approval, on its own thread.
fn indicated_host(options: Options, tools: &Tools) -> (Daemon, Arc<Mutex<Vec<Event>>>) {
    let policy = tools.0.join("host-policy.json");
    Store::new(&policy)
        .unwrap()
        .update(Change::Approval(Approval::None))
        .unwrap();
    let image = sibling("fr-observation-indicator");
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let report: Reporter = Arc::new(move |event| sink.lock().unwrap().push(event));
    // A local Stop on the indicator ends the run; nothing else stops it.
    let stop = Arc::new(StopHandle::default());
    let daemon = thread::spawn(move || {
        frd::host_indicator::run_with_policy(
            &options,
            &report,
            &stop,
            Configuration {
                path: policy,
                approval: None,
                sharing: None,
            },
            &image,
        )
    });
    (daemon, events)
}
