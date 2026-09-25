//! The shipped `fr connect --view-only` against the production `frd run`
//! composition with REAL media: the real `fr-media-worker` captures a host Xvfb,
//! encodes software HEVC (libx265) and the frames cross real UDP/TLS/QUIC; the
//! client's sandboxed decode worker presents them in a window on a second Xvfb,
//! where an independent X client reads the pixels back. The tailnet `LocalAPI`,
//! CA and ingress firewall are FIXTURES; nothing here is live-tailnet evidence.
use super::shipped_client::{ClientApi, wait_for_event};
use super::*;
use frd::host_run::{self, Event, Options, Reporter, StopHandle};
use std::{
    io::{BufRead, BufReader, Read},
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::Instant,
};

/// Host desktop colours (first picture, then a changed desktop); after HEVC
/// 4:2:0 each channel may drift a little.
pub(super) const COLOUR: (i32, i32, i32) = (0x31, 0x72, 0xb4);
const CHANGED: (i32, i32, i32) = (0xb4, 0x5a, 0x31);
const TOLERANCE: i32 = 24;

/// A binary from the same cargo profile directory as this test executable.
pub(super) fn sibling(name: &str) -> PathBuf {
    std::env::current_exe()
        .unwrap()
        .ancestors()
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
        .unwrap_or_else(|| {
            panic!(
                "build {name} first: cargo build -p fr-native --features linux-desktop,linux-displays,linux-input,linux-clipboard,linux-audio --bin fr --bin fr-media-worker --bin fr-input-agent --locked (and cargo build -p frd --bin frd for the control e2e)"
            )
        })
}

pub(super) struct Xvfb {
    child: Child,
    pub(super) display: String,
}
impl Xvfb {
    pub(super) fn start(size: &str) -> Self {
        let mut child = Command::new("/usr/bin/Xvfb")
            .args([
                "-displayfd",
                "1",
                "-screen",
                "0",
                size,
                "-nolisten",
                "tcp",
                "-noreset",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("Xvfb required");
        let mut number = String::new();
        BufReader::new(child.stdout.take().unwrap().take(16))
            .read_line(&mut number)
            .unwrap();
        Self {
            child,
            display: format!(":{}", number.trim().parse::<u16>().unwrap()),
        }
    }
}
impl Drop for Xvfb {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// (window, centre pixel 0xRRGGBB) of every viewable top-level window, read by
/// an independent Xlib client.
pub(super) fn window_pixels(display: &str) -> Vec<(u64, u32)> {
    const PEER: &str = r#"
import ctypes as c, sys
x = c.CDLL("libX11.so.6")
D, W = c.c_void_p, c.c_ulong
x.XOpenDisplay.restype = D
x.XDefaultRootWindow.restype = W; x.XDefaultRootWindow.argtypes = [D]
x.XQueryTree.argtypes = [D, W, c.POINTER(W), c.POINTER(W), c.POINTER(c.POINTER(W)), c.POINTER(c.c_uint)]
class A(c.Structure):
    _fields_ = [("x", c.c_int), ("y", c.c_int), ("w", c.c_int), ("h", c.c_int), ("bw", c.c_int),
                ("depth", c.c_int), ("visual", c.c_void_p), ("root", W), ("cls", c.c_int),
                ("bit_gravity", c.c_int), ("win_gravity", c.c_int), ("backing_store", c.c_int),
                ("planes", c.c_ulong), ("pixel", c.c_ulong), ("save_under", c.c_int), ("cmap", W),
                ("installed", c.c_int), ("map_state", c.c_int), ("all_events", c.c_long),
                ("your_events", c.c_long), ("no_propagate", c.c_long), ("override", c.c_int),
                ("screen", c.c_void_p)]
x.XGetWindowAttributes.argtypes = [D, W, c.POINTER(A)]
x.XGetImage.restype = D
x.XGetImage.argtypes = [D, W, c.c_int, c.c_int, c.c_uint, c.c_uint, W, c.c_int]
x.XGetPixel.restype = W; x.XGetPixel.argtypes = [D, c.c_int, c.c_int]
x.XDestroyImage.argtypes = [D]
d = x.XOpenDisplay(sys.argv[1].encode())
assert d
root, parent, children, n = W(), W(), c.POINTER(W)(), c.c_uint()
x.XQueryTree(d, x.XDefaultRootWindow(d), c.byref(root), c.byref(parent), c.byref(children), c.byref(n))
for i in range(n.value):
    a = A()
    if not x.XGetWindowAttributes(d, children[i], c.byref(a)) or a.map_state != 2 or a.w < 32 or a.h < 32:
        continue
    image = x.XGetImage(d, children[i], a.w // 2, a.h // 2, 1, 1, W(-1).value, 2)
    if image:
        print(children[i], x.XGetPixel(image, 0, 0))
        x.XDestroyImage(image)
"#;
    let output = Command::new("python3")
        .args(["-c", PEER, display])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let (window, pixel) = line.trim().split_once(' ')?;
            Some((window.parse().ok()?, pixel.parse().ok()?))
        })
        .collect()
}

/// Ask the window to close as a window manager would (`WM_DELETE_WINDOW`).
pub(super) fn close_window(display: &str, window: u64) {
    const CLOSE: &str = r#"
import ctypes as c, sys
x = c.CDLL("libX11.so.6")
D, W = c.c_void_p, c.c_ulong
x.XOpenDisplay.restype = D
x.XInternAtom.restype = W; x.XInternAtom.argtypes = [D, c.c_char_p, c.c_int]
x.XSendEvent.argtypes = [D, W, c.c_int, c.c_long, c.c_void_p]
x.XSync.argtypes = [D, c.c_int]
class Client(c.Structure):
    _fields_ = [("type", c.c_int), ("serial", c.c_ulong), ("synthetic", c.c_int), ("display", D),
                ("window", W), ("message_type", W), ("format", c.c_int), ("data", c.c_long * 5)]
class Event(c.Union):
    _fields_ = [("client", Client), ("pad", c.c_long * 24)]
d = x.XOpenDisplay(sys.argv[1].encode())
w = int(sys.argv[2])
e = Event()
e.client.type = 33
e.client.display = d
e.client.window = w
e.client.message_type = x.XInternAtom(d, b"WM_PROTOCOLS", 0)
e.client.format = 32
e.client.data[0] = x.XInternAtom(d, b"WM_DELETE_WINDOW", 0)
assert x.XSendEvent(d, w, 0, 0, c.byref(e))
x.XSync(d, 0)
"#;
    assert!(
        Command::new("python3")
            .args(["-c", CLOSE, display, &window.to_string()])
            .status()
            .unwrap()
            .success()
    );
}

pub(super) fn near(pixel: u32, colour: (i32, i32, i32)) -> bool {
    let channel = |shift: u32| i32::try_from((pixel >> shift) & 0xff).unwrap();
    (channel(16) - colour.0).abs() <= TOLERANCE
        && (channel(8) - colour.1).abs() <= TOLERANCE
        && (channel(0) - colour.2).abs() <= TOLERANCE
}

pub(super) fn set_root(display: &str, (r, g, b): (i32, i32, i32)) {
    assert!(
        Command::new("xsetroot")
            .args([
                "-display",
                display,
                "-solid",
                &format!("#{r:02x}{g:02x}{b:02x}")
            ])
            .status()
            .unwrap()
            .success()
    );
}

/// Move the HOST's own pointer (X server state, not `FrankenRemote` input) with an
/// independent X client. The viewer composites the forwarded host cursor, so a
/// desktop-colour sample must not sit under the pointer.
pub(super) fn warp_pointer(display: &str, x: i32, y: i32) {
    const WARP: &str = r#"
import ctypes as c, sys
x = c.CDLL("libX11.so.6")
D, W = c.c_void_p, c.c_ulong
x.XOpenDisplay.restype = D; x.XOpenDisplay.argtypes = [c.c_char_p]
x.XDefaultRootWindow.restype = W; x.XDefaultRootWindow.argtypes = [D]
x.XWarpPointer.argtypes = [D, W, W, c.c_int, c.c_int, c.c_uint, c.c_uint, c.c_int, c.c_int]
x.XSync.argtypes = [D, c.c_int]
d = x.XOpenDisplay(sys.argv[1].encode())
assert d
x.XWarpPointer(d, 0, x.XDefaultRootWindow(d), 0, 0, 0, 0, int(sys.argv[2]), int(sys.argv[3]))
x.XSync(d, 0)
"#;
    assert!(
        Command::new("python3")
            .args(["-c", WARP, display, &x.to_string(), &y.to_string()])
            .status()
            .unwrap()
            .success()
    );
}

/// Poll the viewer display until a window shows `colour` (or time runs out).
pub(super) fn await_colour(
    display: &str,
    colour: (i32, i32, i32),
    client: &mut Child,
) -> Option<u64> {
    let until = Instant::now() + Duration::from_secs(45);
    while Instant::now() < until {
        assert!(
            client.try_wait().unwrap().is_none(),
            "fr connect exited early"
        );
        if let Some((window, _)) = window_pixels(display)
            .into_iter()
            .find(|(_, pixel)| near(*pixel, colour))
        {
            return Some(window);
        }
        thread::sleep(Duration::from_millis(250));
    }
    None
}

/// The shipped client, view-only, on the viewer's own display.
pub(super) fn connect(fr: &Path, api: &Path, roots: &Path, display: &str, worker: &Path) -> Child {
    let port = address().port().to_string();
    Command::new(fr)
        .args(["connect", "n-host", "--view-only", "--experimental-native"])
        .args(["--display", "only", "--attempts", "1", "--json"])
        .arg("--socket")
        .arg(api)
        .arg("--trust-roots")
        .arg(roots)
        .args(["--port", &port, "--x-display", display])
        .arg("--worker")
        .arg(worker)
        .env_remove("DISPLAY")
        .env_remove("XAUTHORITY")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress; two Xvfb displays"]
fn fr_connect_presents_real_host_pixels_through_frd_run() {
    let (fr, worker) = (sibling("fr"), sibling("fr-media-worker"));
    let host = Xvfb::start("640x480x24");
    let viewer = Xvfb::start("800x600x24");
    set_root(&host.display, COLOUR);
    // Xvfb starts its pointer at the screen centre, which is exactly where the
    // desktop colour is sampled; the viewer now draws the forwarded host cursor
    // there. Park the host pointer in a corner instead.
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
    };
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let report: Reporter = Arc::new(move |event| sink.lock().unwrap().push(event));
    let stop = Arc::new(StopHandle::default());
    let host_stop = stop.clone();
    let daemon = thread::spawn(move || host_run::run(&options, &report, &host_stop));
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

    // Real pixels: the viewer's window on the SECOND display shows the host
    // colour; then a changed host desktop reaches it as a subsequent picture.
    let first = await_colour(&viewer.display, COLOUR, &mut client);
    let changed = first.and_then(|_| {
        set_root(&host.display, CHANGED);
        await_colour(&viewer.display, CHANGED, &mut client)
    });
    // The user closes the viewer window: a clean, counted stop.
    if let Some(window) = changed.or(first) {
        close_window(&viewer.display, window);
    } else {
        let _ = Command::new("kill")
            .args(["-INT", &client.id().to_string()])
            .status();
    }
    let output = super::shipped_client::wait_for(client, Duration::from_secs(30));
    stop.request();
    let result = daemon.join().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let pixels = window_pixels(&viewer.display);
    assert!(
        first.is_some() && changed.is_some(),
        "viewer never showed the host colours (first {first:?}, changed {changed:?}): pixels {pixels:x?}; fr: {stdout} {}; host: {}",
        String::from_utf8_lossy(&output.stderr),
        dump()
    );
    let report: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(report["outcome"], "stopped", "{report}");
    assert!(report["opened"].as_u64().unwrap() >= 1, "{report}");
    assert!(
        report["subsequent_decoder_completions"].as_u64().unwrap() >= 1,
        "{report}"
    );
    assert_eq!(result, Ok(()), "{}", dump());
    let events = events.lock().unwrap();
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::CleanupFailed { .. })),
        "{events:?}"
    );
    assert!(matches!(events.last(), Some(Event::Stopped)), "{events:?}");
    assert!(UdpSocket::bind(address()).is_ok(), "listener retired");
}
