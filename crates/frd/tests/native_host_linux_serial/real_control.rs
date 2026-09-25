//! The shipped `fr connect --control` against the production `frd run` with an
//! input agent, with REAL media AND REAL input: the real `fr-media-worker`
//! captures a host Xvfb in software HEVC, the shipped client presents it on a
//! second (viewer) Xvfb, and `XTest` input on the VIEWER display travels through
//! the client's X11 capture, the real grant/lease over UDP/TLS/QUIC and frd's
//! input thread to the real per-lease `fr-input-agent`, which injects it on the
//! HOST display. Effects are observed by independent python/Xlib clients (no
//! `FrankenRemote` code): pointer position and delivered Key/Button events.
//!
//! Fixtures, stated: the tailnet `LocalAPI`, CA and ingress firewall are the
//! existing namespace fixtures; the viewer harness plays the user's window
//! manager (it focuses the newly mapped viewer window, as focus-new-windows
//! would) and the user (it types and clicks with `XTest` on the viewer display).
//! Nothing here is live-tailnet, hardware or physical-device evidence.
use super::real_media::{Xvfb, close_window, sibling};
use super::shipped_client::{ClientApi, wait_for, wait_for_event};
use super::*;
use frd::host_run::{self, Event, Options, Reporter, StopHandle};
use std::{
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc,
    thread::{self, JoinHandle},
    time::Instant,
};

/// Shared python3/ctypes Xlib prelude for both harnesses.
pub(super) const PRELUDE: &str = r#"
import ctypes as c, os, select, sys, time
x = c.CDLL("libX11.so.6")
t = c.CDLL("libXtst.so.6")
D, W, I, U, L = c.c_void_p, c.c_ulong, c.c_int, c.c_uint, c.c_long
def sig(f, res, *args):
    f.restype = res
    f.argtypes = list(args)
ERRORS = c.CFUNCTYPE(I, D, c.c_void_p)
sig(x.XSetErrorHandler, c.c_void_p, ERRORS)
sig(x.XOpenDisplay, D, c.c_char_p)
sig(x.XDefaultRootWindow, W, D)
sig(x.XSync, I, D, I)
sig(x.XFlush, I, D)
sig(x.XPending, I, D)
sig(x.XNextEvent, I, D, c.c_void_p)
sig(x.XConnectionNumber, I, D)
sig(x.XSelectInput, I, D, W, L)
sig(x.XSetInputFocus, I, D, W, I, W)
sig(x.XGetInputFocus, I, D, c.POINTER(W), c.POINTER(I))
sig(x.XFetchName, I, D, W, c.POINTER(c.c_void_p))
sig(x.XFree, I, c.c_void_p)
sig(x.XKeysymToKeycode, c.c_ubyte, D, W)
sig(x.XQueryTree, I, D, W, c.POINTER(W), c.POINTER(W), c.POINTER(c.POINTER(W)), c.POINTER(U))
sig(t.XTestFakeMotionEvent, I, D, I, I, I, W)
sig(t.XTestFakeButtonEvent, I, D, U, I, W)
sig(t.XTestFakeKeyEvent, I, D, U, I, W)
class A(c.Structure):
    _fields_ = [("x", I), ("y", I), ("w", I), ("h", I), ("bw", I), ("depth", I),
                ("visual", c.c_void_p), ("root", W), ("cls", I), ("bit_gravity", I),
                ("win_gravity", I), ("backing_store", I), ("planes", W), ("pixel", W),
                ("save_under", I), ("cmap", W), ("installed", I), ("map_state", I),
                ("all_events", L), ("your_events", L), ("no_propagate", L),
                ("override", I), ("screen", c.c_void_p)]
sig(x.XGetWindowAttributes, I, D, W, c.POINTER(A))
class Input(c.Structure):
    _fields_ = [("type", I), ("serial", W), ("send_event", I), ("display", D),
                ("window", W), ("root", W), ("subwindow", W), ("time", W), ("x", I),
                ("y", I), ("x_root", I), ("y_root", I), ("state", U), ("detail", U),
                ("same_screen", I)]
class Map(c.Structure):
    _fields_ = [("type", I), ("serial", W), ("send_event", I), ("display", D),
                ("event", W), ("window", W), ("override_redirect", I)]
class Event(c.Union):
    _fields_ = [("type", I), ("input", Input), ("map", Map), ("pad", L * 24)]
# A window destroyed between a query and its use is expected, never fatal.
IGNORE = ERRORS(lambda d, e: 0)
x.XSetErrorHandler(IGNORE)
d = x.XOpenDisplay(sys.argv[1].encode())
assert d
root = x.XDefaultRootWindow(d)
def say(*words):
    print(*words, flush=True)
def title(w):
    name = c.c_void_p()
    if not x.XFetchName(d, w, c.byref(name)) or not name.value:
        return b""
    value = c.string_at(name.value)
    x.XFree(name)
    return value
def mapped(want):
    found = []
    rr, pp, kids, n = W(), W(), c.POINTER(W)(), U()
    if x.XQueryTree(d, root, c.byref(rr), c.byref(pp), c.byref(kids), c.byref(n)):
        for i in range(n.value):
            a = A()
            if (title(kids[i]) == want and x.XGetWindowAttributes(d, kids[i], c.byref(a))
                    and a.map_state == 2):
                found.append((kids[i], a.x, a.y, a.w, a.h))
        if kids:
            x.XFree(c.cast(kids, c.c_void_p))
    return found
def serve(on_event, on_command, idle=lambda: None):
    fd = x.XConnectionNumber(d)
    pending = b""
    while True:
        idle()
        while x.XPending(d) > 0:
            e = Event()
            x.XNextEvent(d, c.byref(e))
            on_event(e)
        ready, _, _ = select.select([0, fd], [], [], 0.05)
        if 0 in ready:
            chunk = os.read(0, 4096)
            if not chunk:
                return
            pending += chunk
            while b"\n" in pending:
                line, pending = pending.split(b"\n", 1)
                on_command(line.decode().split())
"#;

/// HOST display: a full-screen focused window that reports every delivered
/// Key/Button event, the pointer, and the executor's indicator. It repaints a
/// corner square so the host keeps producing fresh pictures for the viewer.
const HOST_OBSERVER: &str = r#"
INDICATOR = b"FrankenRemote - Stop remote control"
sig(x.XCreateSimpleWindow, W, D, W, I, I, U, U, U, W, W)
sig(x.XMapWindow, I, D, W)
sig(x.XLowerWindow, I, D, W)
sig(x.XDefaultGC, c.c_void_p, D, I)
sig(x.XSetForeground, I, D, c.c_void_p, W)
sig(x.XFillRectangle, I, D, W, c.c_void_p, I, I, U, U)
sig(x.XQueryPointer, I, D, W, c.POINTER(W), c.POINTER(W), c.POINTER(I), c.POINTER(I),
    c.POINTER(I), c.POINTER(I), c.POINTER(U))
w = x.XCreateSimpleWindow(d, root, 0, 0, 640, 480, 0, 0, 0)
x.XSelectInput(d, w, 1 | 2 | 4 | 8)
x.XMapWindow(d, w)
x.XLowerWindow(d, w)
x.XSync(d, 0)
x.XSetInputFocus(d, w, 1, 0)
x.XSync(d, 0)
gc = x.XDefaultGC(d, 0)
paint = {"tick": 0, "last": 0.0}
def idle():
    now = time.monotonic()
    if now - paint["last"] >= 0.25:
        paint["tick"] ^= 1
        paint["last"] = now
        x.XSetForeground(d, gc, 0x3172b4 if paint["tick"] else 0xb45a31)
        x.XFillRectangle(d, w, gc, 560, 400, 64, 64)
        x.XFlush(d)
def event(e):
    if e.type in (2, 3, 4, 5):
        v = e.input
        say("EV", e.type, v.detail, v.x_root, v.y_root, v.send_event)
def command(cmd):
    if cmd[0] == "pointer":
        r, ch, rx, ry, wx, wy, m = W(), W(), I(), I(), I(), I(), U()
        x.XQueryPointer(d, root, c.byref(r), c.byref(ch), c.byref(rx), c.byref(ry),
                        c.byref(wx), c.byref(wy), c.byref(m))
        say("PTR", rx.value, ry.value, m.value & 0x1f00)
    elif cmd[0] == "indicator":
        found = mapped(INDICATOR)
        say("IND", *(found[0][:3] if found else ("none",)))
    elif cmd[0] == "click":
        # Core XTest, exactly like the controller's own injected input.
        t.XTestFakeMotionEvent(d, 0, int(cmd[1]), int(cmd[2]), 0)
        t.XTestFakeButtonEvent(d, 1, 1, 0)
        t.XTestFakeButtonEvent(d, 1, 0, 0)
        x.XSync(d, 0)
        say("OK")
    elif cmd[0] == "keycode":
        say("CODE", x.XKeysymToKeycode(d, int(cmd[1], 0)))
say("READY", w)
serve(event, command, idle)
"#;

/// VIEWER display: the user's window manager and hands. Focuses the newly
/// mapped `FrankenRemote` window, then moves, clicks and types with `XTest`.
const VIEWER_DRIVER: &str = r#"
TITLE = b"FrankenRemote"
x.XSelectInput(d, root, 1 << 19)
x.XSync(d, 0)
state = {"window": 0}
def event(e):
    if e.type == 19 and title(e.map.window) == TITLE:
        x.XSetInputFocus(d, e.map.window, 2, 0)
        x.XSync(d, 0)
        state["window"] = e.map.window
        say("FOCUSED", e.map.window)
def command(cmd):
    if cmd[0] == "window":
        found = [f for f in mapped(TITLE) if f[0] == state["window"]]
        say("WIN", *(found[0] if found else ("none",)))
        return
    if cmd[0] == "focus":
        f, r = W(), I()
        x.XGetInputFocus(d, c.byref(f), c.byref(r))
        say("FOCUS", f.value)
        return
    if cmd[0] == "move":
        t.XTestFakeMotionEvent(d, 0, int(cmd[1]), int(cmd[2]), 0)
    elif cmd[0] == "button":
        t.XTestFakeButtonEvent(d, int(cmd[1]), int(cmd[2]), 0)
    elif cmd[0] == "key":
        t.XTestFakeKeyEvent(d, x.XKeysymToKeycode(d, int(cmd[1], 0)), int(cmd[2]), 0)
    x.XSync(d, 0)
    say("OK")
say("WATCHING")
serve(event, command)
"#;

/// One persistent independent Xlib client with a line protocol. `EV` and
/// `FOCUSED` lines are asynchronous notices; everything else answers a command.
pub(super) struct Harness {
    child: Child,
    stdin: ChildStdin,
    lines: mpsc::Receiver<String>,
    notices: Vec<String>,
}
impl Harness {
    pub(super) fn start(body: &str, display: &str, ready: &str) -> Self {
        let mut child = Command::new("python3")
            .args(["-u", "-c", &format!("{PRELUDE}{body}"), display])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("python3 required");
        let stdout = child.stdout.take().unwrap();
        let (sender, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        let stdin = child.stdin.take().unwrap();
        let mut harness = Self {
            child,
            stdin,
            lines,
            notices: Vec::new(),
        };
        let first = harness.answer(Duration::from_secs(20));
        assert!(
            first.as_deref().is_some_and(|l| l.starts_with(ready)),
            "harness did not start: {first:?}"
        );
        harness
    }
    fn answer(&mut self, limit: Duration) -> Option<String> {
        let until = Instant::now() + limit;
        loop {
            let left = until.checked_duration_since(Instant::now())?;
            let line = self.lines.recv_timeout(left).ok()?;
            if line.starts_with("EV ") || line.starts_with("FOCUSED ") {
                self.notices.push(line);
            } else {
                return Some(line);
            }
        }
    }
    pub(super) fn ask(&mut self, command: &str) -> Vec<String> {
        writeln!(self.stdin, "{command}").unwrap();
        self.stdin.flush().unwrap();
        self.answer(Duration::from_secs(20))
            .unwrap_or_else(|| panic!("harness did not answer {command}"))
            .split_whitespace()
            .map(str::to_owned)
            .collect()
    }
    fn notices(&mut self, prefix: &str) -> Vec<Vec<String>> {
        while let Ok(line) = self.lines.try_recv() {
            assert!(
                line.starts_with("EV ") || line.starts_with("FOCUSED "),
                "unsolicited harness line {line}"
            );
            self.notices.push(line);
        }
        self.notices
            .iter()
            .filter(|line| line.starts_with(prefix))
            .map(|line| line.split_whitespace().map(str::to_owned).collect())
            .collect()
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

type Point = (i32, i32);
fn number(word: &str) -> i32 {
    word.parse().unwrap()
}
/// (position, held-button mask) on the host, queried independently.
fn host_pointer(observer: &mut Harness) -> (Point, u32) {
    let answer = observer.ask("pointer");
    assert_eq!(answer[0], "PTR", "{answer:?}");
    (
        (number(&answer[1]), number(&answer[2])),
        answer[3].parse().unwrap(),
    )
}
/// The executor's mapped indicator on the host: (window, x, y).
pub(super) fn indicator(observer: &mut Harness) -> Option<(u64, i32, i32)> {
    let answer = observer.ask("indicator");
    assert_eq!(answer[0], "IND", "{answer:?}");
    (answer[1] != "none").then(|| {
        (
            answer[1].parse().unwrap(),
            number(&answer[2]),
            number(&answer[3]),
        )
    })
}
/// Host-delivered (type, detail, root position, `SendEvent`) key/button events.
fn host_events(observer: &mut Harness) -> Vec<(u32, u32, Point, bool)> {
    observer
        .notices("EV ")
        .iter()
        .map(|w| {
            (
                w[1].parse().unwrap(),
                w[2].parse().unwrap(),
                (number(&w[3]), number(&w[4])),
                w[5] != "0",
            )
        })
        .collect()
}
pub(super) fn eventually(limit: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let until = Instant::now() + limit;
    loop {
        if condition() {
            return true;
        }
        if Instant::now() >= until {
            return false;
        }
        thread::sleep(Duration::from_millis(50));
    }
}
pub(super) fn signal(child: &Child, name: &str) {
    assert!(
        Command::new("kill")
            .args([name, &child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
}
/// Linked shared objects of an executable, as `ldd` reports them.
fn linked(image: &Path) -> String {
    let output = Command::new("ldd").arg(image).output().unwrap();
    assert!(output.status.success(), "ldd {}", image.display());
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The shipped client with an explicit role on the given local display.
fn connect(
    fr: &Path,
    api: &Path,
    roots: &Path,
    display: &str,
    worker: &Path,
    clipboard: bool,
) -> Child {
    let port = address().port().to_string();
    Command::new(fr)
        .args(["connect", "n-host", "--control", "--experimental-native"])
        .args(clipboard.then_some("--clipboard"))
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

/// `frd run` (in process, the production composition) on the host display.
pub(super) struct Daemon {
    events: Arc<Mutex<Vec<Event>>>,
    stop: Arc<StopHandle>,
    thread: Option<JoinHandle<Result<(), host_run::Error>>>,
    _api: fixture::Api,
    _tools: Tools,
}
impl Daemon {
    fn start(display: &str, worker: &Path, input_agent: Option<PathBuf>, clipboard: bool) -> Self {
        let api = fixture::Api::new();
        let tools = Tools::new();
        let options = Options {
            socket: Some(api.path.clone()),
            port: address().port(),
            interface: "fr-fixture".into(),
            worker: worker.to_path_buf(),
            display: display.into(),
            xauthority: None,
            trust_roots: fixture::pki().join("ca.pem"),
            sharing: fr_tailnet::Scope::OwnUser,
            fps: 15,
            bitrate: 2_000_000,
            ingress_tools: Some((tools.0.join("nft"), tools.0.join("ip"))),
            once: false,
            handle_signals: false,
            input_agent,
            clipboard,
        };
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let report: Reporter = Arc::new(move |event| sink.lock().unwrap().push(event));
        let stop = Arc::new(StopHandle::default());
        let host_stop = stop.clone();
        let thread = thread::spawn(move || host_run::run(&options, &report, &host_stop));
        let daemon = Self {
            events,
            stop,
            thread: Some(thread),
            _api: api,
            _tools: tools,
        };
        assert!(
            wait_for_event(&daemon.events, Duration::from_secs(20), |e| matches!(
                e,
                Event::Listening { .. }
            )),
            "frd never listened: {}",
            daemon.dump()
        );
        daemon
    }
    pub(super) fn dump(&self) -> String {
        format!("{:?}", self.events.lock().unwrap())
    }
    /// Stop, then require a clean, typed end: no cleanup failure, `Stopped`.
    pub(super) fn finish(mut self) {
        self.stop.request();
        let result = self.thread.take().unwrap().join().unwrap();
        assert_eq!(result, Ok(()), "{}", self.dump());
        let events = self.events.lock().unwrap();
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Event::CleanupFailed { .. })),
            "{events:?}"
        );
        assert!(matches!(events.last(), Some(Event::Stopped)), "{events:?}");
        assert!(UdpSocket::bind(address()).is_ok(), "listener retired");
    }
}

/// Everything up to a live controlled session: both displays, both
/// harnesses, `frd run --input-agent`, the shipped client, the host pointer
/// following viewer motion, and the executor's indicator shown on the host.
/// Fields drop in order: the X clients (harnesses) end before their servers.
pub(super) struct Controlled {
    pub(super) observer: Harness,
    driver: Harness,
    pub(super) daemon: Daemon,
    pub(super) client: Child,
    pub(super) window: (u64, Point),
    _client_api: ClientApi,
    pub(super) viewer: Xvfb,
    pub(super) host: Xvfb,
}
impl Controlled {
    fn start() -> Self {
        Self::start_with(false, false)
    }
    /// `frd run --input-agent [--clipboard]` and `fr connect --control [--clipboard]`.
    pub(super) fn start_with(host_clipboard: bool, client_clipboard: bool) -> Self {
        let (fr, worker, agent) = (
            sibling("fr"),
            sibling("fr-media-worker"),
            sibling("fr-input-agent"),
        );
        let host = Xvfb::start("640x480x24");
        let viewer = Xvfb::start("800x600x24");
        let mut observer = Harness::start(HOST_OBSERVER, &host.display, "READY");
        let driver = Harness::start(VIEWER_DRIVER, &viewer.display, "WATCHING");
        let daemon = Daemon::start(&host.display, &worker, Some(agent), host_clipboard);
        // No viewer, no lease: the executor (and its indicator) is per lease.
        assert_eq!(indicator(&mut observer), None);
        let client_api = ClientApi::new();
        let roots = fixture::pki().join("ca.pem");
        let client = connect(
            &fr,
            &client_api.path,
            &roots,
            &viewer.display,
            &worker,
            client_clipboard,
        );
        let mut session = Self {
            observer,
            driver,
            daemon,
            client,
            window: (0, (0, 0)),
            _client_api: client_api,
            viewer,
            host,
        };
        session.window = session.viewer_window();
        session.await_control();
        session
    }
    fn alive(&mut self) {
        if let Some(status) = self.client.try_wait().unwrap() {
            let mut stdout = String::new();
            if let Some(out) = self.client.stdout.as_mut() {
                let _ = std::io::Read::read_to_string(out, &mut stdout);
            }
            panic!(
                "fr connect --control exited early ({status}): {stdout}; host: {}",
                self.daemon.dump()
            );
        }
    }
    /// The focused, exact-size viewer window: (XID, origin on the viewer display).
    fn viewer_window(&mut self) -> (u64, Point) {
        let mut found = None;
        let until = Instant::now() + Duration::from_secs(60);
        while found.is_none() && Instant::now() < until {
            self.alive();
            if let Some(focused) = self.driver.notices("FOCUSED ").first() {
                let answer = self.driver.ask("window");
                if answer[1] != "none" && answer[1] == focused[1] {
                    found = Some(answer);
                }
            }
            thread::sleep(Duration::from_millis(50));
        }
        let answer = found.unwrap_or_else(|| panic!("no viewer window: {}", self.daemon.dump()));
        // Native pixels: the window is exactly the 640x480 host display.
        assert_eq!(
            (&answer[4][..], &answer[5][..]),
            ("640", "480"),
            "{answer:?}"
        );
        let focus = self.driver.ask("focus");
        assert_eq!(
            focus[1], answer[1],
            "the viewer window holds keyboard focus"
        );
        (
            answer[1].parse().unwrap(),
            (number(&answer[2]), number(&answer[3])),
        )
    }
    /// Move the VIEWER pointer to window-local `local`.
    fn viewer_move(&mut self, local: Point) {
        let (_, (wx, wy)) = self.window;
        let answer = self
            .driver
            .ask(&format!("move {} {}", wx + local.0, wy + local.1));
        assert_eq!(answer, ["OK"]);
    }
    fn viewer(&mut self, command: &str) {
        assert_eq!(self.driver.ask(command), ["OK"], "{command}");
    }
    /// Capture attaches only after the grant, mapping and visibility evidence:
    /// until then viewer motion has no host effect. Re-send motion until the
    /// independently queried host pointer follows the 1:1 mapping.
    fn await_control(&mut self) {
        let until = Instant::now() + Duration::from_secs(90);
        let mut nudge = 0;
        loop {
            self.alive();
            nudge ^= 1;
            let target = (300 + nudge, 300);
            self.viewer_move(target);
            thread::sleep(Duration::from_millis(100));
            if host_pointer(&mut self.observer).0 == target {
                break;
            }
            assert!(
                Instant::now() < until,
                "viewer motion never reached the host: {}",
                self.daemon.dump()
            );
        }
        // The executor maps its indicator BEFORE it accepts any input.
        assert!(
            indicator(&mut self.observer).is_some(),
            "host input without the mandatory indicator"
        );
    }
    /// Viewer motion reaches exactly this host point (1:1 mapping, origin 0,0).
    pub(super) fn pointer_follows(&mut self, local: Point, limit: Duration) -> bool {
        let observer = &mut self.observer;
        let driver = &mut self.driver;
        let (_, (wx, wy)) = self.window;
        let mut nudge = 0;
        eventually(limit, || {
            nudge ^= 1;
            let target = (local.0 + nudge, local.1);
            assert_eq!(
                driver.ask(&format!("move {} {}", wx + target.0, wy + target.1)),
                ["OK"]
            );
            thread::sleep(Duration::from_millis(50));
            host_pointer(observer).0 == target
        })
    }
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress; two Xvfb displays; real input agent"]
fn fr_connect_control_drives_the_host_desktop_through_frd_run() {
    // AGENTS 3.2: neither the shipped daemon nor this in-process host links
    // Xlib/XCB/XTest; injection runs only in the per-lease fr-input-agent.
    for image in [sibling("frd"), std::env::current_exe().unwrap()] {
        let libraries = linked(&image);
        for gui in ["libX11", "libxcb", "libXtst", "libXi"] {
            assert!(!libraries.contains(gui), "{} links {gui}", image.display());
        }
    }
    let mut s = Controlled::start();

    // Exact 1:1 pointer mapping, observed on the HOST.
    assert!(
        s.pointer_follows((123, 234), Duration::from_secs(10)),
        "pointer mapping: {}",
        s.daemon.dump()
    );

    // A click on the VIEWER is delivered to the host window under the pointer.
    assert!(s.pointer_follows((320, 300), Duration::from_secs(10)));
    let (at, _) = host_pointer(&mut s.observer);
    let before = host_events(&mut s.observer).len();
    s.viewer("button 1 1");
    s.viewer("button 1 0");
    let clicked = eventually(Duration::from_secs(10), || {
        let events = host_events(&mut s.observer);
        let new = &events[before..];
        new.contains(&(4, 1, at, false)) && new.contains(&(5, 1, at, false))
    });
    assert!(
        clicked,
        "no host ButtonPress/Release at {at:?}: {:?}",
        host_events(&mut s.observer)
    );

    // A key typed on the VIEWER reaches the focused host window, same keycode.
    let code: u32 = s.observer.ask("keycode 0x61")[1].parse().unwrap();
    let before = host_events(&mut s.observer).len();
    s.viewer("key 0x61 1");
    s.viewer("key 0x61 0");
    let typed = eventually(Duration::from_secs(10), || {
        let events = host_events(&mut s.observer);
        let new = &events[before..];
        new.iter().any(|e| (e.0, e.1, e.3) == (2, code, false))
            && new.iter().any(|e| (e.0, e.1, e.3) == (3, code, false))
    });
    assert!(
        typed,
        "no host KeyPress/Release {code}: {:?}",
        host_events(&mut s.observer)
    );

    // Planted negative: the controller's own kind of input (core XTest) on
    // the host indicator's stop button does NOT revoke.
    let (_, ix, iy) = indicator(&mut s.observer).expect("indicator for the whole lease");
    assert_eq!(
        s.observer.ask(&format!("click {} {}", ix + 50, iy + 95)),
        ["OK"]
    );
    thread::sleep(Duration::from_millis(500));
    assert!(
        indicator(&mut s.observer).is_some(),
        "an XTest click revoked control"
    );
    assert!(
        s.pointer_follows((410, 330), Duration::from_secs(10)),
        "control did not survive the XTest indicator click: {}",
        s.daemon.dump()
    );

    // The user closes the viewer window: a clean stop, with content-free
    // host-reported result counts, then the lease's executor goes away.
    close_window(&s.viewer.display, s.window.0);
    let output = wait_for(s.client, Duration::from_secs(30));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let report: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|_| {
        panic!(
            "fr: {stdout} {}; host: {}",
            String::from_utf8_lossy(&output.stderr),
            s.daemon.dump()
        )
    });
    assert_eq!(report["outcome"], "stopped", "{report}");
    assert_eq!(report["role"], "control", "{report}");
    assert_eq!(report["control_requested"], true, "{report}");
    assert_eq!(report["control_granted"], true, "{report}");
    // Two button and two key transitions were each submitted to the host OS.
    assert!(
        report["input_submitted_to_os"].as_u64().unwrap() >= 4,
        "{report}"
    );
    assert_eq!(report["physical_visibility_proven"], false, "{report}");
    assert!(
        eventually(Duration::from_secs(15), || indicator(&mut s.observer)
            .is_none()),
        "indicator outlived the session"
    );
    s.daemon.finish();
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress; two Xvfb displays; real input agent"]
fn a_stopped_controller_loses_its_lease_and_later_input_has_no_host_effect() {
    let mut s = Controlled::start();
    // Planted negative: freeze the whole client longer than the 3 s lease.
    signal(&s.client, "-STOP");
    let frozen = Instant::now();
    // The host expires the unrenewed lease itself and ends the executor.
    let ended = eventually(Duration::from_secs(15), || {
        indicator(&mut s.observer).is_none()
    });
    assert!(ended, "lease survived a frozen client: {}", s.daemon.dump());
    if let Some(left) = Duration::from_secs(5).checked_sub(frozen.elapsed()) {
        thread::sleep(left);
    }
    let (resting, _) = host_pointer(&mut s.observer);
    let before = host_events(&mut s.observer).len();
    signal(&s.client, "-CONT");
    // Input right after resuming, when stale local state is most tempting.
    for local in [(150, 400), (160, 410), (170, 420)] {
        s.viewer_move(local);
    }
    s.viewer("button 1 1");
    s.viewer("button 1 0");
    s.viewer("key 0x62 1");
    s.viewer("key 0x62 0");
    for local in [(180, 430), (190, 440)] {
        s.viewer_move(local);
    }
    thread::sleep(Duration::from_secs(3));
    let (now, mask) = host_pointer(&mut s.observer);
    assert_eq!(
        (now, mask),
        (resting, 0),
        "resumed input moved the host: {}",
        s.daemon.dump()
    );
    let after = host_events(&mut s.observer);
    assert!(
        after.len() == before,
        "resumed input reached the host: {:?}",
        &after[before..]
    );
    assert_eq!(indicator(&mut s.observer), None, "control was reacquired");
    // The client ends on its own or on a local interrupt; its outcome here is
    // diagnostic, the host effects above are the evidence.
    if s.client.try_wait().unwrap().is_none() {
        signal(&s.client, "-INT");
    }
    let output = wait_for(s.client, Duration::from_secs(30));
    println!(
        "fr after lease loss: {:?} {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout)
    );
    s.daemon.finish();
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress; one Xvfb display"]
fn a_host_without_an_input_agent_refuses_fr_connect_control_by_type() {
    let (fr, worker) = (sibling("fr"), sibling("fr-media-worker"));
    let host = Xvfb::start("640x480x24");
    let daemon = Daemon::start(&host.display, &worker, None, false);
    let client_api = ClientApi::new();
    let roots = fixture::pki().join("ca.pem");
    let client = connect(&fr, &client_api.path, &roots, &host.display, &worker, false);
    let output = wait_for(client, Duration::from_secs(60));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let report: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|_| panic!("fr: {stdout} {}", String::from_utf8_lossy(&output.stderr)));
    // Never a silent view-only session: a typed host refusal.
    assert_eq!(output.status.code(), Some(1), "{report}");
    assert_eq!(report["outcome"], "refused", "{report}");
    assert_eq!(
        report["error"]["code"],
        "host_control_unavailable",
        "{report}; host: {}",
        daemon.dump()
    );
    daemon.finish();
}
