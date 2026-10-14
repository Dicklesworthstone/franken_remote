//! `frd run --observation-indicator` with REAL media: the production
//! host-indicator wrapper maps the real `fr-observation-indicator` child on the
//! host Xvfb before listening, a shipped view-only `fr connect` sees real host
//! pixels, and the local "Stop sharing" surface ends the share. The tailnet
//! `LocalAPI`, CA and ingress firewall are FIXTURES; an Xvfb window is not a
//! physical privacy indicator.
use super::real_media::{
    COLOUR, Xvfb, await_colour, close_window, connect, set_root, sibling, warp_pointer,
};
use super::shipped_client::{ClientApi, wait_for, wait_for_event};
use super::*;
use frd::host_policy::{Approval, Change, Store};
use frd::host_run::{Event, Options, Reporter, StopHandle, policy::Configuration};
use std::{process::Command, thread, time::Instant};

const TITLE: &str = "FrankenRemote - Stop sharing";

/// (window, x, y) of the mapped top-level window titled `TITLE`, read by an
/// independent Xlib client on the host display.
fn indicator(display: &str) -> Option<(u64, i32, i32)> {
    const FIND: &str = r#"
import ctypes as c, sys
x = c.CDLL("libX11.so.6")
D, W = c.c_void_p, c.c_ulong
x.XOpenDisplay.restype = D
x.XDefaultRootWindow.restype = W; x.XDefaultRootWindow.argtypes = [D]
x.XQueryTree.argtypes = [D, W, c.POINTER(W), c.POINTER(W), c.POINTER(c.POINTER(W)), c.POINTER(c.c_uint)]
x.XFetchName.argtypes = [D, W, c.POINTER(c.c_char_p)]
class A(c.Structure):
    _fields_ = [("x", c.c_int), ("y", c.c_int), ("w", c.c_int), ("h", c.c_int), ("bw", c.c_int),
                ("depth", c.c_int), ("visual", c.c_void_p), ("root", W), ("cls", c.c_int),
                ("bit_gravity", c.c_int), ("win_gravity", c.c_int), ("backing_store", c.c_int),
                ("planes", c.c_ulong), ("pixel", c.c_ulong), ("save_under", c.c_int), ("cmap", W),
                ("installed", c.c_int), ("map_state", c.c_int), ("all_events", c.c_long),
                ("your_events", c.c_long), ("no_propagate", c.c_long), ("override", c.c_int),
                ("screen", c.c_void_p)]
x.XGetWindowAttributes.argtypes = [D, W, c.POINTER(A)]
d = x.XOpenDisplay(sys.argv[1].encode())
assert d
root, parent, children, n = W(), W(), c.POINTER(W)(), c.c_uint()
x.XQueryTree(d, x.XDefaultRootWindow(d), c.byref(root), c.byref(parent), c.byref(children), c.byref(n))
for i in range(n.value):
    name = c.c_char_p()
    a = A()
    if (x.XFetchName(d, children[i], c.byref(name)) and name.value == sys.argv[2].encode()
            and x.XGetWindowAttributes(d, children[i], c.byref(a)) and a.map_state == 2):
        print(children[i], a.x, a.y)
"#;
    let output = Command::new("python3")
        .args(["-c", FIND, display, TITLE])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8_lossy(&output.stdout);
    let mut words = text.split_whitespace();
    Some((
        words.next()?.parse().unwrap(),
        words.next()?.parse().unwrap(),
        words.next()?.parse().unwrap(),
    ))
}

/// A core `XTest` click, exactly the kind of input a remote controller injects.
fn xtest_click(display: &str, x: i32, y: i32) {
    const CLICK: &str = r#"
import ctypes as c, sys
x = c.CDLL("libX11.so.6"); t = c.CDLL("libXtst.so.6")
x.XOpenDisplay.restype = c.c_void_p
x.XSync.argtypes = [c.c_void_p, c.c_int]
t.XTestFakeMotionEvent.argtypes = [c.c_void_p, c.c_int, c.c_int, c.c_int, c.c_ulong]
t.XTestFakeButtonEvent.argtypes = [c.c_void_p, c.c_uint, c.c_int, c.c_ulong]
d = x.XOpenDisplay(sys.argv[1].encode())
assert d
t.XTestFakeMotionEvent(d, 0, int(sys.argv[2]), int(sys.argv[3]), 0)
t.XTestFakeButtonEvent(d, 1, 1, 0)
t.XTestFakeButtonEvent(d, 1, 0, 0)
x.XSync(d, 0)
"#;
    assert!(
        Command::new("python3")
            .args(["-c", CLICK, display, &x.to_string(), &y.to_string()])
            .status()
            .unwrap()
            .success()
    );
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress; two Xvfb displays"]
fn the_observation_indicator_shows_for_a_view_only_share_and_its_close_stops_sharing() {
    let (fr, worker) = (sibling("fr"), sibling("fr-media-worker"));
    let image = sibling("fr-observation-indicator");
    let host = Xvfb::start("640x480x24");
    let viewer = Xvfb::start("800x600x24");
    set_root(&host.display, COLOUR);
    warp_pointer(&host.display, 16, 16);

    let api = fixture::Api::new();
    let tools = Tools::new();
    let roots = fixture::pki().join("ca.pem");
    let policy = tools.0.join("host-policy.json");
    Store::new(&policy)
        .unwrap()
        .update(Change::Approval(Approval::None))
        .unwrap();
    let options = Options {
        socket: Some(api.path.clone()),
        port: address().port(),
        interface: "fr-fixture".into(),
        worker: worker.clone(),
        display: host.display.clone(),
        xauthority: None,
        trust_roots: roots.clone(),
        sharing: fr_tailnet::Scope::OwnUser,
        fps: 15,
        bitrate: 2_000_000,
        ingress_tools: Some((tools.0.join("nft"), tools.0.join("ip"))),
        once: false,
        handle_signals: false,
        input_agent: None,
        clipboard: false,
        audio: None,
        files: None,
        session_monitor: None,
    };
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let report: Reporter = Arc::new(move |event| sink.lock().unwrap().push(event));
    let stop = Arc::new(StopHandle::default());
    let host_stop = stop.clone();
    let daemon = thread::spawn(move || {
        frd::host_indicator::run_with_policy(
            &options,
            &report,
            &host_stop,
            Configuration {
                path: policy,
                approval: None,
                sharing: None,
            },
            &image,
        )
    });
    let dump = || format!("{:?}", events.lock().unwrap());
    assert!(
        wait_for_event(&events, Duration::from_secs(20), |e| matches!(
            e,
            Event::Listening { .. }
        )),
        "frd never listened: {}",
        dump()
    );
    // Mapped before the listener started, seen by an independent X client.
    let (window, ix, iy) =
        indicator(&host.display).expect("no Stop sharing indicator while listening");

    let client_api = ClientApi::new();
    let mut client = connect(&fr, &client_api.path, &roots, &viewer.display, &worker);
    let shown = await_colour(&viewer.display, COLOUR, &mut client);
    assert!(
        shown.is_some(),
        "the view-only share did not show: {}",
        dump()
    );
    assert_eq!(indicator(&host.display).map(|(w, ..)| w), Some(window));

    // Planted negative: a core XTest click (what a remote controller injects)
    // on the stop button does not end sharing.
    xtest_click(&host.display, ix + 50, iy + 95);
    thread::sleep(Duration::from_millis(500));
    assert!(
        client.try_wait().unwrap().is_none(),
        "an XTest click on the indicator ended sharing: {}",
        dump()
    );
    assert!(indicator(&host.display).is_some());

    // The local user closes the indicator: the original host stops sharing
    // with local priority, the viewer is told, and the indicator goes away.
    let closed = Instant::now();
    close_window(&host.display, window);
    let output = wait_for(client, Duration::from_secs(30));
    let stopped = closed.elapsed();
    let result = daemon.join().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    println!(
        "indicator close -> client end {stopped:?}; fr: {}",
        stdout.trim()
    );
    assert!(result.is_ok(), "frd run: {result:?}; host: {}", dump());
    assert!(
        stdout.contains("\"host_stopping\""),
        "the viewer was not told the host stopped sharing: {stdout}; host: {}",
        dump()
    );
    assert_eq!(indicator(&host.display), None, "indicator outlived the run");
    stop.request();
}

/// A local stop of `frd run` with a view-only viewer attached tells that
/// viewer the host is stopping, instead of leaving it to infer silence.
#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress; two Xvfb displays"]
fn a_local_stop_tells_the_view_only_viewer_the_host_is_stopping() {
    let (fr, worker) = (sibling("fr"), sibling("fr-media-worker"));
    let host = Xvfb::start("640x480x24");
    let viewer = Xvfb::start("800x600x24");
    set_root(&host.display, COLOUR);
    warp_pointer(&host.display, 16, 16);
    let api = fixture::Api::new();
    let tools = Tools::new();
    let roots = fixture::pki().join("ca.pem");
    let options = Options {
        socket: Some(api.path.clone()),
        port: address().port(),
        interface: "fr-fixture".into(),
        worker: worker.clone(),
        display: host.display.clone(),
        xauthority: None,
        trust_roots: roots.clone(),
        sharing: fr_tailnet::Scope::OwnUser,
        fps: 15,
        bitrate: 2_000_000,
        ingress_tools: Some((tools.0.join("nft"), tools.0.join("ip"))),
        once: false,
        handle_signals: false,
        input_agent: None,
        clipboard: false,
        audio: None,
        files: None,
        session_monitor: None,
    };
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let report: Reporter = Arc::new(move |event| sink.lock().unwrap().push(event));
    let stop = Arc::new(StopHandle::default());
    let host_stop = stop.clone();
    let daemon = thread::spawn(move || frd::host_run::run(&options, &report, &host_stop));
    let dump = || format!("{:?}", events.lock().unwrap());
    assert!(
        wait_for_event(&events, Duration::from_secs(20), |e| matches!(
            e,
            Event::Listening { .. }
        )),
        "frd never listened: {}",
        dump()
    );
    let client_api = ClientApi::new();
    let mut client = connect(&fr, &client_api.path, &roots, &viewer.display, &worker);
    assert!(
        await_colour(&viewer.display, COLOUR, &mut client).is_some(),
        "the view-only share did not show: {}",
        dump()
    );
    let stopped = Instant::now();
    stop.request();
    let output = wait_for(client, Duration::from_secs(30));
    let elapsed = stopped.elapsed();
    let result = daemon.join().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    println!(
        "local stop -> client end {elapsed:?}; fr: {}",
        stdout.trim()
    );
    assert!(result.is_ok(), "frd run: {result:?}; host: {}", dump());
    assert!(
        stdout.contains("\"host_stopping\""),
        "the viewer was not told the host stopped: {stdout}; host: {}",
        dump()
    );
}
