//! Remote cursor forwarding through the shipped path. The host's OWN pointer
//! and cursor image on the host Xvfb are driven by an independent X client (no
//! `FrankenRemote` input authority is involved). The real `fr-media-worker`
//! observes them through XFIXES, `frd run` forwards shape + position over real
//! UDP/TLS/QUIC, and the shipped `fr connect --view-only` presenter composites
//! the ONE client-rendered cursor on a second Xvfb, where another independent X
//! client reads the window pixels back. The host capture excludes the pointer,
//! so cursor-coloured pixels can only come from the forwarded overlay.
//!
//! Scope: the tailnet `LocalAPI`, CA and ingress firewall are FIXTURES, and
//! Xvfb is neither a GPU compositor nor `HiDPI` nor Wayland. This is not
//! live-tailnet, real-compositor or scaled-display evidence.
use super::real_media::{
    COLOUR, Xvfb, await_colour, close_window, connect, near, set_root, sibling,
};
use super::shipped_client::{ClientApi, wait_for_event};
use super::*;
use frd::host_run::{self, Event, Options, Reporter, StopHandle};
use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    thread,
    time::Instant,
};

/// Opaque cursor colours, distinct from the desktop colour and from the
/// viewer's built-in black/white fallback crosshair.
const MAGENTA: (i32, i32, i32) = (0xff, 0x00, 0xff);
const GREEN: (i32, i32, i32) = (0x00, 0xff, 0x00);
/// Cursor image side in pixels; the hotspot is its top-left pixel.
const SIZE: i32 = 16;

/// An independent host-side X client owning the root cursor and the host's
/// own pointer position. Commands are acknowledged after `XSync`.
struct HostPointer {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}
impl HostPointer {
    const SCRIPT: &str = r#"
import ctypes as c, sys
x = c.CDLL("libX11.so.6")
xc = c.CDLL("libXcursor.so.1")
D, W = c.c_void_p, c.c_ulong
class Image(c.Structure):
    _fields_ = [("version", c.c_uint32), ("size", c.c_uint32), ("width", c.c_uint32),
                ("height", c.c_uint32), ("xhot", c.c_uint32), ("yhot", c.c_uint32),
                ("delay", c.c_uint32), ("pixels", c.POINTER(c.c_uint32))]
x.XOpenDisplay.restype = D; x.XOpenDisplay.argtypes = [c.c_char_p]
x.XDefaultRootWindow.restype = W; x.XDefaultRootWindow.argtypes = [D]
x.XDefineCursor.argtypes = [D, W, W]
x.XWarpPointer.argtypes = [D, W, W, c.c_int, c.c_int, c.c_uint, c.c_uint, c.c_int, c.c_int]
x.XSync.argtypes = [D, c.c_int]
xc.XcursorImageCreate.restype = c.POINTER(Image); xc.XcursorImageCreate.argtypes = [c.c_int, c.c_int]
xc.XcursorImageLoadCursor.restype = W; xc.XcursorImageLoadCursor.argtypes = [D, c.POINTER(Image)]
xc.XcursorImageDestroy.argtypes = [c.POINTER(Image)]
d = x.XOpenDisplay(sys.argv[1].encode())
assert d
root = x.XDefaultRootWindow(d)
for line in sys.stdin:
    cmd = line.split()
    if cmd[0] == "cursor":
        argb, size = int(cmd[1], 16), int(cmd[2])
        image = xc.XcursorImageCreate(size, size)
        image.contents.xhot = 0
        image.contents.yhot = 0
        for i in range(size * size):
            image.contents.pixels[i] = argb
        cursor = xc.XcursorImageLoadCursor(d, image)
        xc.XcursorImageDestroy(image)
        x.XDefineCursor(d, root, cursor)
    elif cmd[0] == "warp":
        x.XWarpPointer(d, 0, root, 0, 0, 0, 0, int(cmd[1]), int(cmd[2]))
    x.XSync(d, 0)
    print("ok", flush=True)
"#;
    fn start(display: &str) -> Self {
        let mut child = Command::new("python3")
            .args(["-u", "-c", Self::SCRIPT, display])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        Self {
            input: child.stdin.take().unwrap(),
            output: BufReader::new(child.stdout.take().unwrap()),
            child,
        }
    }
    fn command(&mut self, line: &str) {
        writeln!(self.input, "{line}").unwrap();
        self.input.flush().unwrap();
        let mut reply = String::new();
        self.output.read_line(&mut reply).unwrap();
        assert_eq!(reply.trim(), "ok", "host pointer helper failed: {line}");
    }
    fn cursor(&mut self, (r, g, b): (i32, i32, i32)) {
        self.command(&format!("cursor ff{r:02x}{g:02x}{b:02x} {SIZE}"));
    }
    fn warp(&mut self, x: i32, y: i32) {
        self.command(&format!("warp {x} {y}"));
    }
}
impl Drop for HostPointer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 0xRRGGBB pixels of `window` at window-relative points, read by an
/// independent Xlib client (`None` when the read fails).
fn pixels_at(display: &str, window: u64, points: &[(i32, i32)]) -> Vec<Option<u32>> {
    const PEER: &str = r#"
import ctypes as c, sys
x = c.CDLL("libX11.so.6")
D, W = c.c_void_p, c.c_ulong
x.XOpenDisplay.restype = D; x.XOpenDisplay.argtypes = [c.c_char_p]
x.XGetImage.restype = D
x.XGetImage.argtypes = [D, W, c.c_int, c.c_int, c.c_uint, c.c_uint, W, c.c_int]
x.XGetPixel.restype = W; x.XGetPixel.argtypes = [D, c.c_int, c.c_int]
x.XDestroyImage.argtypes = [D]
d = x.XOpenDisplay(sys.argv[1].encode())
assert d
for point in sys.argv[3:]:
    px, py = map(int, point.split(","))
    image = x.XGetImage(d, int(sys.argv[2]), px, py, 1, 1, W(-1).value, 2)
    print(x.XGetPixel(image, 0, 0) if image else -1)
    if image:
        x.XDestroyImage(image)
"#;
    let output = Command::new("python3")
        .args(["-c", PEER, display, &window.to_string()])
        .args(points.iter().map(|(x, y)| format!("{x},{y}")))
        .output()
        .unwrap();
    let pixels: Vec<_> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| line.trim().parse::<u32>().ok())
        .collect();
    if pixels.len() == points.len() {
        pixels
    } else {
        vec![None; points.len()]
    }
}

/// Poll until `accept` holds for the sampled pixels. `Err` carries the last
/// sample for the failure message.
fn await_pixels(
    display: &str,
    window: u64,
    client: &mut Child,
    points: &[(i32, i32)],
    accept: impl Fn(&[u32]) -> bool,
) -> Result<Vec<u32>, Vec<Option<u32>>> {
    let until = Instant::now() + Duration::from_secs(20);
    let mut last = vec![None; points.len()];
    while Instant::now() < until {
        assert!(
            client.try_wait().unwrap().is_none(),
            "fr connect exited early"
        );
        last = pixels_at(display, window, points);
        let sample: Option<Vec<u32>> = last.iter().copied().collect();
        if let Some(sample) = sample
            && accept(&sample)
        {
            return Ok(sample);
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(last)
}

/// The same `frd run` composition as `real_media.rs` (fixture tailnet/ingress).
fn options(
    api: &fixture::Api,
    tools: &Tools,
    worker: &std::path::Path,
    display: &str,
    roots: &std::path::Path,
) -> Options {
    Options {
        socket: Some(api.path.clone()),
        port: address().port(),
        interface: "fr-fixture".into(),
        worker: worker.to_path_buf(),
        display: display.to_owned(),
        xauthority: None,
        trust_roots: roots.to_path_buf(),
        sharing: fr_tailnet::Scope::OwnUser,
        fps: 15,
        bitrate: 2_000_000,
        ingress_tools: Some((tools.0.join("nft"), tools.0.join("ip"))),
        once: false,
        handle_signals: false,
        input_agent: None,
        clipboard: false,
        audio: None,
    }
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress; two Xvfb displays"]
fn fr_connect_draws_the_host_cursor_where_the_host_pointer_is_and_follows_it() {
    let (fr, worker) = (sibling("fr"), sibling("fr-media-worker"));
    let host = Xvfb::start("640x480x24");
    let viewer = Xvfb::start("800x600x24");
    set_root(&host.display, COLOUR);
    let mut pointer = HostPointer::start(&host.display);
    pointer.cursor(MAGENTA);
    pointer.warp(200, 150);

    let api = fixture::Api::new();
    let tools = Tools::new();
    let roots = fixture::pki().join("ca.pem");
    let options = options(&api, &tools, &worker, &host.display, &roots);
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
    let window = await_colour(&viewer.display, COLOUR, &mut client);

    // The viewer presents 1:1 at the window origin, so the host hotspot maps
    // to the same window coordinates. Inside the 16x16 image, just past its
    // corner, and far away.
    let at = |(x, y): (i32, i32)| {
        [
            (x + 4, y + 4),
            (x + SIZE + 4, y + SIZE + 4),
            (x - 60, y + 90),
        ]
    };
    let drawn_at =
        |p: &[u32], colour| near(p[0], colour) && near(p[1], COLOUR) && near(p[2], COLOUR);
    let first = window.map(|w| {
        await_pixels(&viewer.display, w, &mut client, &at((200, 150)), |p| {
            drawn_at(p, MAGENTA)
        })
    });
    // The host's own pointer moves: the overlay follows and the old location
    // is restored to the desktop colour (no residue, no second cursor).
    let moved = window.filter(|_| matches!(first, Some(Ok(_)))).map(|w| {
        pointer.warp(420, 300);
        let mut points = at((420, 300)).to_vec();
        points.push((204, 154));
        await_pixels(&viewer.display, w, &mut client, &points, |p| {
            drawn_at(p, MAGENTA) && near(p[3], COLOUR)
        })
    });
    // A new host cursor image reaches the viewer as a new reliable shape.
    let reshaped = window.filter(|_| matches!(moved, Some(Ok(_)))).map(|w| {
        pointer.cursor(GREEN);
        await_pixels(&viewer.display, w, &mut client, &at((420, 300)), |p| {
            drawn_at(p, GREEN)
        })
    });

    if let Some(window) = window {
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
    assert!(
        matches!(
            (&first, &moved, &reshaped),
            (Some(Ok(_)), Some(Ok(_)), Some(Ok(_)))
        ),
        "cursor overlay not observed (window {window:?}): first {first:x?}, moved {moved:x?}, \
         reshaped {reshaped:x?}; fr: {stdout} {}; host: {}",
        String::from_utf8_lossy(&output.stderr),
        dump()
    );
    let report: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(report["outcome"], "stopped", "{report}");
    assert_eq!(result, Ok(()), "{}", dump());
    let events = events.lock().unwrap();
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::CleanupFailed { .. })),
        "{events:?}"
    );
    assert!(matches!(events.last(), Some(Event::Stopped)), "{events:?}");
}
