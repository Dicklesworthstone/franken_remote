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
sig(x.XGetPointerMapping, I, D, c.POINTER(c.c_ubyte), I)
sig(x.XSetPointerMapping, I, D, c.POINTER(c.c_ubyte), I)
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
    elif cmd[0] == "paint":
        x.XSetForeground(d, gc, int(cmd[1], 0))
        x.XFillRectangle(d, w, gc, 260, 180, 100, 100)
        x.XSync(d, 0)
        say("OK")
    elif cmd[0] == "click":
        # Core XTest, exactly like the controller's own injected input.
        t.XTestFakeMotionEvent(d, 0, int(cmd[1]), int(cmd[2]), 0)
        t.XTestFakeButtonEvent(d, 1, 1, 0)
        t.XTestFakeButtonEvent(d, 1, 0, 0)
        x.XSync(d, 0)
        say("OK")
    elif cmd[0] == "keycode":
        say("CODE", x.XKeysymToKeycode(d, int(cmd[1], 0)))
    elif cmd[0] == "unmap":
        # No physical button produces this logical one (a server whose map
        # lacks the horizontal wheel buttons).
        m = (c.c_ubyte * 256)()
        n = x.XGetPointerMapping(d, m, 256)
        for i in range(n):
            if m[i] == int(cmd[1]):
                m[i] = 0
        status = x.XSetPointerMapping(d, m, n)
        x.XSync(d, 0)
        say("OK" if status == 0 else "BUSY")
say("READY", w)
serve(event, command, idle)
"#;

/// VIEWER display: the user's window manager and hands. Focuses the newly
/// mapped `FrankenRemote` window, then moves, clicks and types with `XTest`.
const VIEWER_DRIVER: &str = r#"
TITLE = b"FrankenRemote"
CAPTURE_TITLE = b"FrankenRemote - Remote system shortcuts - Ctrl+Alt+Shift+Escape releases control"
VIEW_ONLY_TITLE = b"FrankenRemote - View only - F8 requests control"
VIEW_ONLY_UNAVAILABLE = b"FrankenRemote - View only - F8 unavailable"
x.XSelectInput(d, root, 1 << 19)
x.XSync(d, 0)
state = {"window": 0}
def event(e):
    if e.type == 19 and title(e.map.window) in (TITLE, VIEW_ONLY_TITLE, VIEW_ONLY_UNAVAILABLE):
        x.XSetInputFocus(d, e.map.window, 2, 0)
        x.XSync(d, 0)
        state["window"] = e.map.window
        say("FOCUSED", e.map.window)
def command(cmd):
    if cmd[0] == "window":
        found = [f for name in (TITLE, CAPTURE_TITLE, VIEW_ONLY_TITLE, VIEW_ONLY_UNAVAILABLE) for f in mapped(name)
                 if f[0] == state["window"]]
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
    elif cmd[0] == "tap":
        key = x.XKeysymToKeycode(d, int(cmd[1], 0))
        t.XTestFakeKeyEvent(d, key, 1, 0)
        t.XTestFakeKeyEvent(d, key, 0, 0)
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
pub(super) fn host_pointer(observer: &mut Harness) -> (Point, u32) {
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
pub(super) fn connect(
    fr: &Path,
    api: &Path,
    roots: &Path,
    display: &str,
    worker: &Path,
    clipboard: bool,
    send: &[PathBuf],
) -> Child {
    connect_command(fr, api, roots, display, worker, clipboard, send)
        .args(["--attempts", "1"])
        .spawn()
        .unwrap()
}

fn connect_command(
    fr: &Path,
    api: &Path,
    roots: &Path,
    display: &str,
    worker: &Path,
    clipboard: bool,
    send: &[PathBuf],
) -> Command {
    let port = address().port().to_string();
    let mut command = Command::new(fr);
    command
        .args(["connect", "n-host", "--control", "--experimental-native"])
        .args((!clipboard).then_some("--no-clipboard"))
        .args(
            send.iter()
                .flat_map(|path| [std::ffi::OsStr::new("--send"), path.as_os_str()]),
        )
        .args(["--display", "only", "--json"])
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
        .stderr(Stdio::piped());
    command
}

/// `frd run` (in process, the production composition) on the host display.
pub(super) struct Daemon {
    events: Arc<Mutex<Vec<Event>>>,
    stop: Arc<StopHandle>,
    thread: Option<JoinHandle<Result<(), host_run::Error>>>,
    cli: Option<DaemonCli>,
    _api: fixture::Api,
    tools: Tools,
}
impl Daemon {
    fn start(
        display: &str,
        worker: &Path,
        input_agent: Option<PathBuf>,
        clipboard: bool,
        files: Option<frd::native_files::Directory>,
    ) -> Self {
        Self::start_monitored(display, worker, input_agent, clipboard, files, None)
    }
    /// `start` plus `frd run --logind-session`: the selected session's
    /// lifetime evidence comes from `session_monitor`.
    pub(super) fn start_monitored(
        display: &str,
        worker: &Path,
        input_agent: Option<PathBuf>,
        clipboard: bool,
        files: Option<frd::native_files::Directory>,
        session_monitor: Option<frd::session_monitor::Configuration>,
    ) -> Self {
        Self::start_profile(
            display,
            worker,
            input_agent,
            clipboard,
            files,
            session_monitor,
            false,
        )
    }
    fn start_profile(
        display: &str,
        worker: &Path,
        input_agent: Option<PathBuf>,
        clipboard: bool,
        files: Option<frd::native_files::Directory>,
        session_monitor: Option<frd::session_monitor::Configuration>,
        dual_stack: bool,
    ) -> Self {
        let api = if dual_stack {
            fixture::Api::dual_stack()
        } else {
            fixture::Api::new()
        };
        let tools = if dual_stack {
            super::dual_stack::tools()
        } else {
            Tools::new()
        };
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
            adaptive_capture: false,
            clipboard,
            audio: None,
            files,
            file_send: None,
            session_monitor,
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
            cli: None,
            _api: api,
            tools,
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
        self.cli.as_ref().map_or_else(
            || format!("{:?}", self.events.lock().unwrap()),
            DaemonCli::dump,
        )
    }
    pub(super) fn events(&self) -> Vec<Event> {
        self.events.lock().unwrap().clone()
    }
    /// Wait for the run to end on its own (without a local stop request).
    pub(super) fn ended(&mut self, limit: Duration) -> Result<(), host_run::Error> {
        let thread = self.thread.take().unwrap();
        let until = Instant::now() + limit;
        while !thread.is_finished() {
            assert!(
                Instant::now() < until,
                "frd run did not end: {}",
                self.dump()
            );
            thread::sleep(Duration::from_millis(10));
        }
        thread.join().unwrap()
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
/// A daemon left running by a panicking test (or a retried setup) is stopped
/// and joined, so its listener is released; `finish`/`ended` take the thread
/// first and keep their own assertions.
impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            self.stop.request();
            let _ = thread.join();
        }
    }
}

/// The same namespace fixture through the shipped host CLI. Only this child's
/// private mount namespace substitutes the existing synthetic ingress tools;
/// the transport, input executor and file owners remain the production ones.
struct DaemonCli {
    child: Child,
    stdout: PathBuf,
    stderr: PathBuf,
}
impl DaemonCli {
    fn output(path: &Path) -> String {
        use std::io::Read as _;
        const LIMIT: u64 = 1024 * 1024;
        let mut text = String::new();
        fs::File::open(path)
            .unwrap()
            .take(LIMIT + 1)
            .read_to_string(&mut text)
            .unwrap();
        assert!(
            text.len() as u64 <= LIMIT,
            "unbounded frd diagnostic output"
        );
        text
    }
    fn dump(&self) -> String {
        format!(
            "frd stdout: {}; stderr: {}",
            Self::output(&self.stdout),
            Self::output(&self.stderr)
        )
    }
    fn listening(&mut self) -> bool {
        assert!(
            self.child.try_wait().unwrap().is_none(),
            "frd exited before listening: {}",
            self.dump()
        );
        Self::output(&self.stdout).lines().any(|line| {
            serde_json::from_str::<serde_json::Value>(line)
                .is_ok_and(|event| event["event"] == "listening")
        })
    }
}
impl Drop for DaemonCli {
    fn drop(&mut self) {
        if !matches!(self.child.try_wait(), Ok(None)) {
            return;
        }
        let _ = Command::new("kill")
            .args(["-INT", &self.child.id().to_string()])
            .status();
        if !eventually(Duration::from_secs(5), || {
            self.child.try_wait().is_ok_and(|status| status.is_some())
        }) {
            let _ = self.child.kill();
            let _ = eventually(Duration::from_secs(5), || {
                self.child.try_wait().is_ok_and(|status| status.is_some())
            });
        }
    }
}
impl Daemon {
    fn start_downloads(
        display: &str,
        worker: &Path,
        input_agent: &Path,
        clipboard: bool,
        sources: &[PathBuf],
    ) -> Self {
        const MOUNTS: &str = "\
mount --make-rprivate /
mount --bind \"$1\" /usr/sbin/nft
mount --bind \"$2\" /usr/sbin/ip
shift 2
exec \"$@\"
";
        let api = fixture::Api::new();
        let tools = Tools::new();
        // tools.py locates its state beside its real source. These wrappers
        // retain that location when mounted on the CLI's protected tool paths.
        let wrappers: Vec<_> = ["nft", "ip"]
            .into_iter()
            .map(|name| {
                let wrapper = tools.0.join(format!("cli-{name}"));
                crate::executable::write(
                    &wrapper,
                    format!(
                        "#!/bin/sh\nexec '{}' \"$@\"\n",
                        tools.0.join(name).display()
                    ),
                );
                wrapper
            })
            .collect();
        let stdout = tools.0.join("frd-stdout.jsonl");
        let stderr = tools.0.join("frd-stderr.txt");
        let child = Command::new("unshare")
            .args(["--mount", "--", "sh", "-ec", MOUNTS, "frd-download-fixture"])
            .args(&wrappers)
            .arg(sibling("frd"))
            .args(["run", "--once", "--json", "--encoder", "software"])
            .args(["--approval", "none", "--sharing", "own-user"])
            .args([
                "--interface",
                "fr-fixture",
                "--fps",
                "15",
                "--bitrate",
                "2000000",
            ])
            .arg("--config")
            .arg(tools.0.join("policy.json"))
            .arg("--socket")
            .arg(&api.path)
            .arg("--port")
            .arg(address().port().to_string())
            .arg("--display")
            .arg(display)
            .arg("--worker")
            .arg(worker)
            .arg("--input-agent")
            .arg(input_agent)
            .arg("--trust-roots")
            .arg(fixture::pki().join("ca.pem"))
            .args((!clipboard).then_some("--no-clipboard"))
            .args(
                sources
                    .iter()
                    .flat_map(|path| [std::ffi::OsStr::new("--send"), path.as_os_str()]),
            )
            .env_remove("DISPLAY")
            .env_remove("XAUTHORITY")
            .stdin(Stdio::null())
            .stdout(fs::File::create_new(&stdout).unwrap())
            .stderr(fs::File::create_new(&stderr).unwrap())
            .spawn()
            .unwrap();
        let mut daemon = Self {
            events: Arc::new(Mutex::new(Vec::new())),
            stop: Arc::new(StopHandle::default()),
            thread: None,
            cli: Some(DaemonCli {
                child,
                stdout,
                stderr,
            }),
            _api: api,
            tools,
        };
        assert!(
            eventually(Duration::from_secs(30), || daemon
                .cli
                .as_mut()
                .unwrap()
                .listening()),
            "frd never listened: {}",
            daemon.dump()
        );
        daemon
    }

    /// Require the CLI's one-shot run to exit by itself after viewer departure.
    /// No stop request hides a host that accidentally keeps serving selections.
    pub(super) fn finish_once(mut self) -> String {
        assert!(
            eventually(Duration::from_secs(30), || {
                self.cli
                    .as_mut()
                    .unwrap()
                    .child
                    .try_wait()
                    .unwrap()
                    .is_some()
            }),
            "frd --once did not exit: {}",
            self.dump()
        );
        let cli = self.cli.as_mut().unwrap();
        let status = cli.child.try_wait().unwrap().unwrap();
        let diagnostic = cli.dump();
        assert!(status.success(), "{diagnostic}");
        let events: Vec<serde_json::Value> = DaemonCli::output(&cli.stdout)
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(
            events
                .iter()
                .all(|event| event["event"] != "cleanup_failed"),
            "{diagnostic}"
        );
        assert_eq!(events.last().unwrap()["event"], "stopped", "{diagnostic}");
        assert_eq!(self.tools.state()["created"], 1, "{diagnostic}");
        assert_eq!(self.tools.state()["deleted"], 1, "{diagnostic}");
        assert!(UdpSocket::bind(address()).is_ok(), "listener retired");
        diagnostic
    }
}

struct Downloads<'a> {
    sources: &'a [PathBuf],
    destination: Option<&'a Path>,
    options: &'a [&'a str],
}

#[derive(Clone, Copy, Default)]
struct InputSettings {
    unmap_wheel: bool,
    capture_system_shortcuts: bool,
    dual_stack: bool,
    reconnect: bool,
}

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
    fn start_dual_stack() -> Self {
        Self::start_all(
            false,
            false,
            None,
            &[],
            None,
            InputSettings {
                dual_stack: true,
                ..InputSettings::default()
            },
            None,
        )
    }
    /// Default clipboard with `frd run --input-agent` and `fr connect --control`;
    /// false explicitly selects `--no-clipboard` at that endpoint.
    pub(super) fn start_with(host_clipboard: bool, client_clipboard: bool) -> Self {
        Self::start_full(host_clipboard, client_clipboard, None, &[])
    }
    /// Also `frd run --files DIR` (a pinned `Directory`) and `--send PATH...`.
    pub(super) fn start_full(
        host_clipboard: bool,
        client_clipboard: bool,
        files: Option<frd::native_files::Directory>,
        send: &[PathBuf],
    ) -> Self {
        Self::start_all(
            host_clipboard,
            client_clipboard,
            files,
            send,
            None,
            InputSettings::default(),
            None,
        )
    }
    /// Real host --once --send selections and viewer --receive consent, using
    /// the same display, input and clipboard fixtures as the upload tests.
    pub(super) fn start_downloads(
        host_clipboard: bool,
        client_clipboard: bool,
        sources: &[PathBuf],
        destination: Option<&Path>,
    ) -> Self {
        Self::start_downloads_with_options(
            host_clipboard,
            client_clipboard,
            sources,
            destination,
            &[],
        )
    }
    pub(super) fn start_downloads_with_options(
        host_clipboard: bool,
        client_clipboard: bool,
        sources: &[PathBuf],
        destination: Option<&Path>,
        options: &[&str],
    ) -> Self {
        Self::start_all(
            host_clipboard,
            client_clipboard,
            None,
            &[],
            None,
            InputSettings::default(),
            Some(Downloads {
                sources,
                destination,
                options,
            }),
        )
    }
    /// A plain controlled session whose host also watches the selected
    /// session through `image` (a monitor image) and logind session name.
    pub(super) fn start_monitored(image: PathBuf, session: String) -> Self {
        Self::start_all(
            false,
            false,
            None,
            &[],
            Some((image, session)),
            InputSettings::default(),
            None,
        )
    }
    /// A host display whose pointer map lacks the horizontal wheel buttons
    /// (logical 6/7) BEFORE `frd run` starts and probes its input executor.
    fn start_without_wheel() -> Self {
        Self::start_all(
            false,
            false,
            None,
            &[],
            None,
            InputSettings {
                unmap_wheel: true,
                ..InputSettings::default()
            },
            None,
        )
    }
    /// The shipped CLI explicitly requests the original attachment's X11 grab.
    fn start_system_shortcuts() -> Self {
        Self::start_all(
            false,
            false,
            None,
            &[],
            None,
            InputSettings {
                capture_system_shortcuts: true,
                ..InputSettings::default()
            },
            None,
        )
    }
    fn start_all(
        host_clipboard: bool,
        client_clipboard: bool,
        files: Option<frd::native_files::Directory>,
        send: &[PathBuf],
        monitor: Option<(PathBuf, String)>,
        input: InputSettings,
        downloads: Option<Downloads<'_>>,
    ) -> Self {
        let (fr, worker, agent) = (
            sibling("fr"),
            sibling("fr-media-worker"),
            sibling("fr-input-agent"),
        );
        let host = Xvfb::start("640x480x24");
        // Keep the exact 640x480 decoder/input drawable, with room beside it
        // for the optional transfer panel without obscuring the controlled view.
        let viewer = Xvfb::start("1600x900x24");
        let mut observer = Harness::start(HOST_OBSERVER, &host.display, "READY");
        let driver = Harness::start(VIEWER_DRIVER, &viewer.display, "WATCHING");
        if input.unmap_wheel {
            for logical in [6, 7] {
                assert_eq!(observer.ask(&format!("unmap {logical}")), ["OK"]);
            }
        }
        let monitor = monitor.map(|(image, session)| frd::session_monitor::Configuration {
            image,
            selection: frd::session_monitor::Selection {
                session,
                uid: rustix::process::geteuid().as_raw(),
                seat: "seat0".into(),
                display: host.display.clone(),
            },
        });
        let daemon = if let Some(downloads) = &downloads {
            Daemon::start_downloads(
                &host.display,
                &worker,
                &agent,
                host_clipboard,
                downloads.sources,
            )
        } else {
            Daemon::start_profile(
                &host.display,
                &worker,
                Some(agent),
                host_clipboard,
                files,
                monitor,
                input.dual_stack,
            )
        };
        // No viewer, no lease: the executor (and its indicator) is per lease.
        assert_eq!(indicator(&mut observer), None);
        let client_api = if input.dual_stack {
            ClientApi::dual_stack()
        } else {
            ClientApi::new()
        };
        let roots = fixture::pki().join("ca.pem");
        let mut command = connect_command(
            &fr,
            &client_api.path,
            &roots,
            &viewer.display,
            &worker,
            client_clipboard,
            send,
        );
        command.args(["--attempts", if input.reconnect { "4" } else { "1" }]);
        if input.dual_stack {
            command.arg("--ipv6");
        }
        if let Some(downloads) = downloads {
            if let Some(destination) = downloads.destination {
                command.arg("--receive").arg(destination);
            }
            command.args(downloads.options);
        }
        if input.capture_system_shortcuts {
            command.arg("--capture-system-shortcuts");
        }
        let client = command.spawn().unwrap();
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
    /// Close the viewer window as a window manager would. A client that already
    /// ended reports its own completion here rather than a missing window.
    pub(super) fn close_viewer(&mut self) {
        self.alive();
        close_window(&self.viewer.display, self.window.0);
    }
    /// The focused, exact-size viewer window: (XID, origin on the viewer display).
    fn viewer_window(&mut self) -> (u64, Point) {
        let mut found = None;
        let until = Instant::now() + Duration::from_secs(60);
        while found.is_none() && Instant::now() < until {
            self.alive();
            // The LATEST focus notice, and the window must hold focus NOW: a
            // window that is briefly unviewable right after its first map
            // reverts focus to the root until its next map is focused again.
            // Asserting at the first notice failed about one run in five.
            if let Some(focused) = self.driver.notices("FOCUSED ").last() {
                let answer = self.driver.ask("window");
                if answer[1] != "none"
                    && answer[1] == focused[1]
                    && self.driver.ask("focus")[1] == answer[1]
                {
                    found = Some(answer);
                }
            }
            thread::sleep(Duration::from_millis(50));
        }
        let answer = found.unwrap_or_else(|| {
            panic!(
                "no viewer window holding keyboard focus: {}",
                self.daemon.dump()
            )
        });
        // Native pixels: the window is exactly the 640x480 host display.
        assert_eq!(
            (&answer[4][..], &answer[5][..]),
            ("640", "480"),
            "{answer:?}"
        );
        (
            answer[1].parse().unwrap(),
            (number(&answer[2]), number(&answer[3])),
        )
    }
    /// Move the VIEWER pointer to window-local `local`.
    pub(super) fn viewer_move(&mut self, local: Point) {
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
        // The executor maps its indicator BEFORE it accepts any input. An absent
        // indicator right after input landed means that lease has already ended
        // (or a real ordering bug): report which, never retry past it.
        if indicator(&mut self.observer).is_none() {
            let client = self.client.try_wait().unwrap();
            panic!(
                "host input without the mandatory indicator (client exited: {client:?}): {}",
                self.daemon.dump()
            );
        }
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
            // Delivery takes at least one path traversal plus the pipeline:
            // watch the host for this exact target before moving again. With a
            // fixed 50 ms check, motion over a path slower than that always
            // landed one nudge behind and was reported as never arriving.
            let settle = Instant::now() + Duration::from_millis(400);
            loop {
                thread::sleep(Duration::from_millis(25));
                if host_pointer(observer).0 == target {
                    return true;
                }
                if Instant::now() >= settle {
                    return false;
                }
            }
        })
    }
}

#[path = "real_control/reconnect.rs"]
mod reconnect;
#[path = "real_control/system_shortcuts.rs"]
mod system_shortcuts;

/// A wheel notch on the VIEWER (logical button 4, one line up) reaches the
/// host window at `at` as one discrete wheel press/release: the host offered
/// line scrolling because its executor probed it, so the client asked for it.
/// The window close ran the controller's closing exchange (before, there was
/// none). It is best effort within its 250 ms budget: native send backlog can
/// refuse the request, or the report can miss the budget after the request was
/// acknowledged. A lease report, when one arrived, is reported by stage only.
fn controller_closed(report: &serde_json::Value) {
    let exchange = &report["close_exchange"];
    assert!(exchange.is_object(), "{report}");
    let lease = &report["lease_report"];
    assert!(
        lease.is_null() || (lease["reason"].is_string() && lease["cleanup_stage"].is_string()),
        "{report}"
    );
    println!(
        "controller close: request_acknowledged {} lease_report {}",
        exchange["request_acknowledged"], report["lease_report"]
    );
}
fn wheel_reaches_host(s: &mut Controlled, at: Point) {
    let before = host_events(&mut s.observer).len();
    s.viewer("button 4 1");
    s.viewer("button 4 0");
    let scrolled = eventually(Duration::from_secs(10), || {
        let events = host_events(&mut s.observer);
        let new = &events[before..];
        new.contains(&(4, 4, at, false)) && new.contains(&(5, 4, at, false))
    });
    assert!(
        scrolled,
        "no host wheel press/release at {at:?}: {:?}",
        host_events(&mut s.observer)
    );
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
    wheel_reaches_host(&mut s, at);

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
    s.close_viewer();
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
    assert_eq!(
        report["control_capabilities_granted"],
        serde_json::json!([
            "keys",
            "repeat",
            "absolute_pointer",
            "buttons",
            "line_scroll"
        ]),
        "{report}"
    );
    assert_eq!(report["wheel_unavailable"], false, "{report}");
    // Two button and two key transitions were each submitted to the host OS.
    assert!(
        report["input_submitted_to_os"].as_u64().unwrap() >= 4,
        "{report}"
    );
    assert_eq!(report["physical_visibility_proven"], false, "{report}");
    controller_closed(&report);
    assert!(
        eventually(Duration::from_secs(15), || indicator(&mut s.observer)
            .is_none()),
        "indicator outlived the session"
    );
    s.daemon.finish();
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress; two Xvfb displays; real input agent"]
fn fr_connect_control_uses_ipv6_while_host_also_listens_on_ipv4() {
    let mut s = Controlled::start_dual_stack();
    let listened: Vec<_> = s
        .daemon
        .events()
        .iter()
        .filter_map(|event| match event {
            Event::Listening { address } => Some(*address),
            _ => None,
        })
        .collect();
    assert!(listened.contains(&address()));
    assert!(listened.contains(&super::dual_stack::ipv6_address()));
    // This is the production HEVC view and input path, selected by --ipv6
    // even though both LocalAPIs also advertise IPv4 for the same stable node.
    assert!(
        s.pointer_follows((321, 301), Duration::from_secs(10)),
        "{}",
        s.daemon.dump()
    );
    let code: u32 = s.observer.ask("keycode 0x61")[1].parse().unwrap();
    let before = host_events(&mut s.observer).len();
    s.viewer("key 0x61 1");
    s.viewer("key 0x61 0");
    assert!(
        eventually(Duration::from_secs(10), || {
            let events = host_events(&mut s.observer);
            events[before..]
                .iter()
                .any(|event| (event.0, event.1, event.3) == (2, code, false))
                && events[before..]
                    .iter()
                    .any(|event| (event.0, event.1, event.3) == (3, code, false))
        }),
        "IPv6 control did not reach the independent host X11 observer"
    );
    let paths = s.daemon._api.requests.lock().unwrap().clone();
    assert!(paths.iter().any(|path| {
        path.starts_with("/localapi/v0/whois?addr=%5Bfd7a%3A115c%3Aa1e0%3A%3A2%5D%3A")
    }));
    assert!(
        !paths
            .iter()
            .any(|path| path.starts_with("/localapi/v0/whois?addr=100.64.0.2%3A")),
        "an explicit IPv6 target must not dial IPv4"
    );
    s.close_viewer();
    let output = wait_for(s.client, Duration::from_secs(30));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["outcome"], "stopped", "{report}");
    assert_eq!(report["control_granted"], true, "{report}");
    assert!(
        report["input_submitted_to_os"].as_u64().unwrap() >= 2,
        "{report}"
    );
    controller_closed(&report);
    let rules = s.daemon.tools.0.join("state.json");
    s.daemon.finish();
    let state: serde_json::Value = serde_json::from_slice(&fs::read(rules).unwrap()).unwrap();
    assert_eq!(state["tables"], serde_json::json!({}));
    assert_eq!(state["created"], state["deleted"]);
    assert!(state["deleted"].as_u64().unwrap() >= 2);
    assert!(UdpSocket::bind(super::dual_stack::ipv6_address()).is_ok());
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

/// Live host capture children (`fr-media-worker --capture`) of THIS test's
/// in-process daemon; the client's decoder runs `--present` and is never
/// selected. The suite runs as root without a PID namespace, so an unscoped
/// scan would also freeze the workers of any other suite on the machine.
fn capture_workers() -> Vec<u32> {
    std::fs::read_dir("/proc")
        .unwrap()
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let pid: u32 = entry.file_name().to_str()?.parse().ok()?;
            let cmdline = std::fs::read(entry.path().join("cmdline")).ok()?;
            let mut words = cmdline.split(|b| *b == 0);
            (words
                .next()
                .is_some_and(|image| image.ends_with(b"/fr-media-worker"))
                && words.next() == Some(b"--capture")
                && descends_from_this_test(pid))
            .then_some(pid)
        })
        .collect()
}

/// Whether `pid`'s parent chain reaches this test process.
fn descends_from_this_test(pid: u32) -> bool {
    let me = std::process::id();
    let mut current = pid;
    for _ in 0..64 {
        // "pid (comm) state ppid ...": comm may contain spaces and ')'.
        let Some(parent) = std::fs::read_to_string(format!("/proc/{current}/stat"))
            .ok()
            .and_then(|stat| {
                let (_, rest) = stat.rsplit_once(')')?;
                rest.split_whitespace().nth(1)?.parse::<u32>().ok()
            })
        else {
            return false;
        };
        if parent == me {
            return true;
        }
        if parent <= 1 {
            return false;
        }
        current = parent;
    }
    false
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress; two Xvfb displays; real input agent"]
fn a_stale_view_suspends_input_before_it_reaches_the_host() {
    let mut s = Controlled::start();
    let workers = capture_workers();
    assert!(!workers.is_empty(), "no host capture worker found");
    // A key held on the viewer reaches the host.
    let code: u32 = s.observer.ask("keycode 0x62")[1].parse().unwrap();
    let before = host_events(&mut s.observer).len();
    s.viewer("key 0x62 1");
    assert!(
        eventually(Duration::from_secs(10), || {
            host_events(&mut s.observer)[before..]
                .iter()
                .any(|e| (e.0, e.1, e.3) == (2, code, false))
        }),
        "the held key never reached the host: {}",
        s.daemon.dump()
    );
    let (resting, _) = host_pointer(&mut s.observer);
    let held = host_events(&mut s.observer).len();
    // Freeze the host's capture: no new picture or source observation reaches
    // the viewer, so its view ages past the source-age bound while the window
    // still shows the last picture and the connection stays up. Keep the
    // freeze under the shortest capture deadline: a capture is also capped at
    // the current tailnet admission, refreshed with 500 ms left, so a longer
    // stall can meet that cap as a hung worker and end observation by design.
    let frozen = Instant::now();
    for pid in &workers {
        assert!(
            Command::new("kill")
                .args(["-STOP", &pid.to_string()])
                .status()
                .unwrap()
                .success()
        );
    }
    // The views lapse by 250 ms into the freeze (view and host source-age
    // bounds). Marker input after that lands on a suspended view: none of it
    // may reach the host.
    thread::sleep(Duration::from_millis(310));
    for local in [(150, 400), (170, 420), (190, 440)] {
        s.viewer_move(local);
    }
    if let Some(rest) = Duration::from_millis(400).checked_sub(frozen.elapsed()) {
        thread::sleep(rest);
    }
    let (now, _) = host_pointer(&mut s.observer);
    for pid in &workers {
        let _ = Command::new("kill")
            .args(["-CONT", &pid.to_string()])
            .status();
    }
    // The host released the held key by itself at its lapse, with no event
    // from the viewer (the viewer's own release is only sent after resuming,
    // and dropped): suspension releases held input (plan 11.3).
    assert!(
        eventually(Duration::from_secs(2), || {
            host_events(&mut s.observer)[held..]
                .iter()
                .any(|e| (e.0, e.1, e.3) == (3, code, false))
        }),
        "the host kept a key held across a stale view: {:?}",
        &host_events(&mut s.observer)[held..]
    );
    assert_eq!(
        now,
        resting,
        "input on a stale view moved the host: {}",
        s.daemon.dump()
    );
    assert!(
        indicator(&mut s.observer).is_some(),
        "the lease ended instead of suspending: {}",
        s.daemon.dump()
    );
    // The user's own release of that key is dropped, not sent again.
    s.viewer("key 0x62 0");
    // The same session and lease resume control once the view is fresh
    // again: no reconnect, no new grant, the same executor.
    assert!(
        s.pointer_follows((300, 250), Duration::from_secs(20)),
        "control did not resume after the view refreshed: {}",
        s.daemon.dump()
    );
    assert!(indicator(&mut s.observer).is_some());
    let releases = host_events(&mut s.observer)[held..]
        .iter()
        .filter(|e| (e.0, e.1, e.3) == (3, code, false))
        .count();
    assert_eq!(releases, 1, "the key was released twice on the host");
    s.close_viewer();
    let output = wait_for(s.client, Duration::from_secs(30));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let report: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|_| panic!("fr: {stdout} {}", String::from_utf8_lossy(&output.stderr)));
    println!("fr after a suspended and resumed view: {report}");
    assert_eq!(report["outcome"], "stopped", "{report}");
    assert!(
        report["input_suspensions"].as_u64().is_some_and(|n| n >= 1),
        "{report}"
    );
    s.daemon.finish();
}

/// An independent host-side X client that repaints its own raised window every
/// 20 ms in a colour it never repeats, so every capture differs from the last
/// and is an encoded picture. Drawing on the root window instead was hidden
/// under the session's other host windows and every capture came back
/// unchanged.
pub(super) struct Painter(Child);
impl Painter {
    const SCRIPT: &str = r#"
import ctypes as c, sys, time
x = c.CDLL("libX11.so.6")
D, W = c.c_void_p, c.c_ulong
class Attributes(c.Structure):
    _fields_ = [("background_pixmap", W), ("background_pixel", W), ("border_pixmap", W),
                ("border_pixel", W), ("bit_gravity", c.c_int), ("win_gravity", c.c_int),
                ("backing_store", c.c_int), ("backing_planes", W), ("backing_pixel", W),
                ("save_under", c.c_int), ("event_mask", c.c_long),
                ("do_not_propagate_mask", c.c_long), ("override_redirect", c.c_int),
                ("colormap", W), ("cursor", W)]
x.XOpenDisplay.restype = D; x.XOpenDisplay.argtypes = [c.c_char_p]
x.XDefaultRootWindow.restype = W; x.XDefaultRootWindow.argtypes = [D]
x.XDefaultScreen.argtypes = [D]
x.XDefaultGC.restype = c.c_void_p; x.XDefaultGC.argtypes = [D, c.c_int]
x.XCreateSimpleWindow.restype = W
x.XCreateSimpleWindow.argtypes = [D, W, c.c_int, c.c_int, c.c_uint, c.c_uint, c.c_uint, W, W]
x.XChangeWindowAttributes.argtypes = [D, W, c.c_ulong, c.POINTER(Attributes)]
x.XMapRaised.argtypes = [D, W]
x.XSetForeground.argtypes = [D, c.c_void_p, c.c_ulong]
x.XFillRectangle.argtypes = [D, W, c.c_void_p, c.c_int, c.c_int, c.c_uint, c.c_uint]
x.XSync.argtypes = [D, c.c_int]
d = x.XOpenDisplay(sys.argv[1].encode())
assert d
root = x.XDefaultRootWindow(d)
gc = x.XDefaultGC(d, x.XDefaultScreen(d))
# Its own raised window: other host clients' windows would clip root drawing.
# Keep the host control indicator above and clipboard privacy strip below visible.
# Covering either surface correctly stops its owner instead of testing recovery.
w = x.XCreateSimpleWindow(d, root, 420, 180, 200, 120, 0, 0, 0)
attributes = Attributes(override_redirect=1)
x.XChangeWindowAttributes(d, w, 1 << 9, c.byref(attributes))
x.XMapRaised(d, w)
i = 0
while True:
    x.XSetForeground(d, gc, (i * 2654435761) & 0xffffff)
    x.XFillRectangle(d, w, gc, 0, 0, 200, 120)
    x.XSync(d, 0)
    i += 1
    time.sleep(0.02)
"#;
    pub(super) fn start(display: &str) -> Self {
        Self(
            Command::new("python3")
                .args(["-u", "-c", Self::SCRIPT, display])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap(),
        )
    }
}
impl Drop for Painter {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A host capture stall DURING MOTION (fr-2kv8). The late result is then an
/// encoded picture, which cannot be skipped like an unchanged one: later
/// P-frames reference it (AGENTS.md section 5). The host must drop it and
/// recover with an IDR on the same session and lease, not end the session.
///
/// The end-to-end acceptance of fr-4vf5. Three things had to hold together:
/// the controller suspends on the host's failure notice instead of ending
/// (`PresentedInput::progress`), its input lane follows the configuration
/// channel the recovery replaces (`NegotiatedInput::follow_recovery`), and the
/// host may restart view evidence under the held lease
/// (`SessionAuthority::require_view_evidence`).
#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress; two Xvfb displays"]
fn a_capture_stall_during_motion_recovers_with_an_idr_on_the_same_lease() {
    let mut s = Controlled::start();
    let painter = Painter::start(&s.host.display);
    // Encoded pictures flow while the host screen changes continuously.
    thread::sleep(Duration::from_millis(600));
    // Freeze the capture worker for 400 ms, under the shortest capture
    // deadline (see the stale-view test): the capture issued before or during
    // the freeze completes past its reference horizon.
    stall_capture(400);
    // The same session and lease: control resumes once the recovery IDR has
    // refreshed the view. No reconnect, no new grant, the same executor.
    assert!(
        s.pointer_follows((300, 250), Duration::from_secs(20)),
        "control did not resume after a capture stall during motion: {}",
        s.daemon.dump()
    );
    assert!(
        indicator(&mut s.observer).is_some(),
        "the lease ended instead of recovering: {}",
        s.daemon.dump()
    );
    drop(painter);
    s.close_viewer();
    let output = wait_for(s.client, Duration::from_secs(30));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let report: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|_| panic!("fr: {stdout} {}", String::from_utf8_lossy(&output.stderr)));
    println!("fr after a capture stall during motion: {report}");
    assert_eq!(report["outcome"], "stopped", "{report}");
    assert_eq!(report["attempts"], 1, "{report}");
    assert!(
        report["last_attempt_media"]["recovered_streams"]
            .as_u64()
            .is_some_and(|n| n >= 1),
        "the late encoded picture was not recovered with an IDR: {report}"
    );
    s.daemon.finish();
}

/// Freeze every host capture worker (SIGSTOP) for `ms`, then resume it.
pub(super) fn stall_capture(ms: u64) {
    let workers = capture_workers();
    assert!(!workers.is_empty(), "no host capture worker found");
    let frozen = Instant::now();
    for pid in &workers {
        assert!(
            Command::new("kill")
                .args(["-STOP", &pid.to_string()])
                .status()
                .unwrap()
                .success()
        );
    }
    if let Some(rest) = Duration::from_millis(ms).checked_sub(frozen.elapsed()) {
        thread::sleep(rest);
    }
    for pid in &workers {
        let _ = Command::new("kill")
            .args(["-CONT", &pid.to_string()])
            .status();
    }
}

/// Each reference recovery replaces three channel pairs, and QUIC never reuses
/// a stream id. With 16 lifetime streams (7 pairs) a controlled session with
/// clipboard lanes could not recover even once, and an input-only one only
/// once: both ended `video_reference_lost`. Here clipboard both ways and a
/// file lane (input, clipboard, files: 6 pairs before any recovery) survive
/// three stalls in a row. The directories live on the namespace's own /run.
#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress; two Xvfb displays"]
fn a_controlled_session_with_clipboard_and_files_recovers_from_repeated_capture_stalls() {
    let base = PathBuf::from(format!("/run/fr-recovery-{}", std::process::id()));
    let (drop_dir, source) = (base.join("drop"), base.join("source"));
    for dir in [&drop_dir, &source] {
        fs::create_dir_all(dir).unwrap();
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let file = source.join("report.bin");
    fs::write(&file, vec![0x5a; 65_536]).unwrap();
    let directory =
        frd::native_files::Directory::open(&drop_dir, frd::native_files::Limits::default())
            .unwrap();
    let mut s = Controlled::start_full(true, true, Some(directory), &[file]);
    let painter = Painter::start(&s.host.display);
    thread::sleep(Duration::from_millis(1500));
    for (n, target) in [(300, 250), (320, 260), (280, 240)].into_iter().enumerate() {
        stall_capture(400);
        assert!(
            s.pointer_follows(target, Duration::from_secs(20)),
            "control did not resume after stall {}: {}",
            n + 1,
            s.daemon.dump()
        );
        thread::sleep(Duration::from_millis(1500));
    }
    drop(painter);
    s.close_viewer();
    let output = wait_for(s.client, Duration::from_secs(30));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let report: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|_| panic!("fr: {stdout} {}", String::from_utf8_lossy(&output.stderr)));
    println!("fr after three capture stalls with clipboard and files: {report}");
    assert_eq!(report["outcome"], "stopped", "{report}");
    assert_eq!(report["attempts"], 1, "{report}");
    assert_eq!(
        report["files_sent"],
        serde_json::json!([{"index": 0, "bytes": 65_536, "durable": true}]),
        "the file lane was attached and used: {report}"
    );
    assert!(
        report["last_attempt_media"]["recovered_streams"]
            .as_u64()
            .is_some_and(|n| n >= 3),
        "all three late encoded pictures must be recovered on the same lease: {report}"
    );
    s.daemon.finish();
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress; one Xvfb display"]
fn a_host_without_an_input_agent_refuses_fr_connect_control_by_type() {
    let (fr, worker) = (sibling("fr"), sibling("fr-media-worker"));
    let host = Xvfb::start("640x480x24");
    let daemon = Daemon::start(&host.display, &worker, None, false, None);
    let client_api = ClientApi::new();
    let roots = fixture::pki().join("ca.pem");
    let client = connect(
        &fr,
        &client_api.path,
        &roots,
        &host.display,
        &worker,
        false,
        &[],
    );
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

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress; two Xvfb displays; real input agent"]
fn a_host_without_wheel_buttons_grants_control_without_the_wheel() {
    // The host's pointer map lacks logical 6/7, so its executor cannot scroll
    // by lines. Before sf8 this failed EVERY grant (the host granted the
    // client's LineScroll request statically and the lease executor then
    // refused it); now control works and only the wheel is unavailable.
    let mut s = Controlled::start_without_wheel();
    assert!(s.pointer_follows((200, 210), Duration::from_secs(10)));
    let (at, _) = host_pointer(&mut s.observer);
    let before = host_events(&mut s.observer).len();
    s.viewer("button 1 1");
    s.viewer("button 1 0");
    let clicked = eventually(Duration::from_secs(10), || {
        let events = host_events(&mut s.observer);
        let new = &events[before..];
        new.contains(&(4, 1, at, false)) && new.contains(&(5, 1, at, false))
    });
    assert!(clicked, "no host click: {:?}", host_events(&mut s.observer));
    // A wheel notch stays local: nothing was granted for it to become.
    let before = host_events(&mut s.observer).len();
    s.viewer("button 4 1");
    s.viewer("button 4 0");
    thread::sleep(Duration::from_secs(1));
    let events = host_events(&mut s.observer);
    assert!(
        events[before..].iter().all(|e| !(4..=7).contains(&e.1)),
        "a wheel event reached the host: {events:?}"
    );
    assert!(
        s.pointer_follows((260, 240), Duration::from_secs(10)),
        "control ended after a local wheel notch: {}",
        s.daemon.dump()
    );
    s.close_viewer();
    let output = wait_for(s.client, Duration::from_secs(30));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let report: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|_| {
        panic!(
            "fr: {stdout} {}; host: {}",
            String::from_utf8_lossy(&output.stderr),
            s.daemon.dump()
        )
    });
    eprintln!(
        "WHEEL granted {} wheel_unavailable {}",
        report["control_capabilities_granted"], report["wheel_unavailable"]
    );
    assert_eq!(report["outcome"], "stopped", "{report}");
    assert_eq!(report["control_granted"], true, "{report}");
    assert_eq!(
        report["control_capabilities_granted"],
        serde_json::json!(["keys", "repeat", "absolute_pointer", "buttons"]),
        "{report}"
    );
    assert_eq!(report["wheel_unavailable"], true, "{report}");
    s.daemon.finish();
}

#[path = "real_control/robot_input.rs"]
mod robot_input;
