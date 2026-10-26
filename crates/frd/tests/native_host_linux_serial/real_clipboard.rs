//! Default clipboard sharing with shipped `fr connect --control` against
//! production `frd run --input-agent`, with REAL X11 selections on both sides:
//! the client's own X11 clipboard owner on the viewer Xvfb, and on the host the
//! real per-lane `fr-input-agent --clipboard` child (frd itself loads no X11).
//! Text travels the real bounded begin/chunk/commit records on the clipboard
//! lane of the real UDP/TLS/QUIC session, attached under the controller's
//! input lease. Effects are observed by INDEPENDENT python3/ctypes Xlib peers
//! (no `FrankenRemote` code): one application per display that owns or reads
//! CLIPBOARD exactly as a desktop application would (ICCCM, real timestamps).
//!
//! Fixtures, stated: the tailnet `LocalAPI`, CA and ingress firewall are the
//! existing namespace fixtures, and the viewer harness plays the window manager
//! and the user as in `real_control`. Xvfb selections in a namespace are not a
//! desktop clipboard manager, Wayland, or a live tailnet.
#[path = "real_clipboard/privacy.rs"]
mod privacy;

use super::real_control::{Controlled, Harness, PRELUDE, eventually, indicator, signal};
use super::real_impairment::Netem;
use super::shipped_client::wait_for;
use super::*;
use std::{
    io::{BufRead, BufReader},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc,
    thread,
    time::Instant,
};

/// A debugger stops only the clipboard thread, at the timed park AFTER its
/// worker turn returned. No shared lock or native call remains in progress.
/// Closing our private pipe detaches, including on unwinding; the helper also
/// has its own ceiling. This never sends SIGSTOP to the containing process.
struct ParkedClipboard {
    child: Option<Child>,
    release: Option<ChildStdin>,
}
impl ParkedClipboard {
    fn start(client: &Child) -> Self {
        let mut child = Command::new("python3")
            .args([
                "-u",
                "-c",
                include_str!("real_clipboard/park_worker.py"),
                &client.id().to_string(),
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("python3 and namespace ptrace capability are required");
        let release = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let (send, receive) = mpsc::channel();
        thread::spawn(move || {
            let mut line = String::new();
            let result = BufReader::new(stdout).read_line(&mut line);
            let _ = send.send(result.map(|_| line));
        });
        // Custody precedes the assertion: a failed acknowledgement still
        // closes the pipe and detaches the original worker.
        let parked = Self {
            child: Some(child),
            release,
        };
        let line = receive
            .recv_timeout(Duration::from_secs(8))
            .expect("clipboard debugger never acknowledged its bounded park")
            .unwrap();
        assert!(line.starts_with("PARKED "), "{line:?}");
        parked
    }
    fn resume(mut self) {
        drop(self.release.take());
        let child = self.child.as_mut().unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "clipboard debugger failed to detach");
                break;
            }
            assert!(Instant::now() < until, "clipboard debugger did not detach");
            thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for ParkedClipboard {
    fn drop(&mut self) {
        drop(self.release.take());
        if let Some(mut child) = self.child.take() {
            let until = Instant::now() + Duration::from_secs(3);
            while matches!(child.try_wait(), Ok(None)) && Instant::now() < until {
                thread::sleep(Duration::from_millis(10));
            }
            // Tracer death also detaches: no EXITKILL option is installed.
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// One independent X11 application on one display: owns CLIPBOARD with a
/// given UTF-8 text (serving TARGETS, TIMESTAMP and `UTF8_STRING` with a real
/// server timestamp), or reads it with `XConvertSelection`.
const PEER: &str = r#"
sig(x.XInternAtom, W, D, c.c_char_p, I)
sig(x.XCreateSimpleWindow, W, D, W, I, I, U, U, U, W, W)
sig(x.XDestroyWindow, I, D, W)
sig(x.XSetSelectionOwner, I, D, W, W, W)
sig(x.XGetSelectionOwner, W, D, W)
sig(x.XConvertSelection, I, D, W, W, W, W, W)
sig(x.XChangeProperty, I, D, W, W, W, I, I, c.c_void_p, I)
sig(x.XDeleteProperty, I, D, W, W)
sig(x.XGetWindowProperty, I, D, W, W, L, L, I, W, c.POINTER(W), c.POINTER(I),
    c.POINTER(W), c.POINTER(W), c.POINTER(c.c_void_p))
sig(x.XSendEvent, I, D, W, I, L, c.c_void_p)
class Req(c.Structure):
    _fields_ = [("type", I), ("serial", W), ("send_event", I), ("display", D), ("owner", W),
                ("requestor", W), ("selection", W), ("target", W), ("property", W), ("time", W)]
class Sel(c.Structure):
    _fields_ = [("type", I), ("serial", W), ("send_event", I), ("display", D), ("requestor", W),
                ("selection", W), ("target", W), ("property", W), ("time", W)]
class Prop(c.Structure):
    _fields_ = [("type", I), ("serial", W), ("send_event", I), ("display", D), ("window", W),
                ("atom", W), ("time", W), ("state", I)]
class Clear(c.Structure):
    _fields_ = [("type", I), ("serial", W), ("send_event", I), ("display", D), ("window", W),
                ("selection", W), ("time", W)]
class CEvent(c.Union):
    _fields_ = [("type", I), ("req", Req), ("sel", Sel), ("prop", Prop), ("clear", Clear),
                ("pad", L * 24)]
def atom(name):
    return x.XInternAtom(d, name, 0)
CLIPBOARD, UTF8, TARGETS, TIMESTAMP = (atom(n) for n in
    (b"CLIPBOARD", b"UTF8_STRING", b"TARGETS", b"TIMESTAMP"))
INBOX, TICK, INCR = atom(b"FR_E2E_INBOX"), atom(b"FR_E2E_TICK"), atom(b"INCR")
MAX_READ_BYTES = 1 << 20
w = x.XCreateSimpleWindow(d, root, 0, 0, 1, 1, 0, 0, 0)
x.XSelectInput(d, w, 1 << 22)
x.XSync(d, 0)
state = {"text": None, "time": 0}
def event(e):
    v = CEvent.from_buffer(e)
    if v.type == 30:
        r = v.req
        prop = r.property or r.target
        ok = state["text"] is not None and r.selection == CLIPBOARD and r.owner == w
        if ok and r.target == TARGETS:
            x.XChangeProperty(d, r.requestor, prop, 4, 32, 0, (W * 3)(TARGETS, TIMESTAMP, UTF8), 3)
        elif ok and r.target == TIMESTAMP:
            x.XChangeProperty(d, r.requestor, prop, 19, 32, 0, (W * 1)(state["time"]), 1)
        elif ok and r.target == UTF8:
            text = state["text"]
            x.XChangeProperty(d, r.requestor, prop, UTF8, 8, 0, c.c_char_p(text), len(text))
        else:
            ok = False
        n = CEvent()
        n.sel.type = 31
        n.sel.display = d
        n.sel.requestor = r.requestor
        n.sel.selection = r.selection
        n.sel.target = r.target
        n.sel.property = prop if ok else 0
        n.sel.time = r.time
        x.XSendEvent(d, r.requestor, 0, 0, c.byref(n))
        x.XFlush(d)
    elif v.type == 29 and v.clear.selection == CLIPBOARD:
        state["text"] = None
def wait_until(match, until):
    while time.monotonic() < until:
        while x.XPending(d) > 0:
            if time.monotonic() >= until:
                return None
            e = Event()
            x.XNextEvent(d, c.byref(e))
            if match(CEvent.from_buffer(e)):
                return CEvent.from_buffer(e)
            event(e)
        time.sleep(min(0.005, max(0, until - time.monotonic())))
    return None
def server_time(until=None):
    # ICCCM: a zero-length append yields a PropertyNotify with the server time.
    if until is None:
        until = time.monotonic() + 3
    x.XChangeProperty(d, w, TICK, 19, 32, 2, (W * 1)(0), 0)
    x.XFlush(d)
    v = wait_until(lambda v: v.type == 28 and v.prop.window == w and v.prop.atom == TICK, until)
    return v.prop.time if v else 0
def read_property(window):
    kind, fmt, n, after, data = W(), I(), W(), W(), c.c_void_p()
    # XGetWindowProperty's length is in 32-bit units, not bytes. Never allocate
    # from the INCR size hint or fetch an unbounded property.
    result = x.XGetWindowProperty(d, window, INBOX, 0, MAX_READ_BYTES // 4, 0, 0,
                                 c.byref(kind), c.byref(fmt), c.byref(n),
                                 c.byref(after), c.byref(data))
    try:
        if result != 0 or after.value != 0 or n.value > MAX_READ_BYTES:
            return None
        if n.value and not data.value:
            return None
        if fmt.value == 8:
            value = c.string_at(data.value, n.value) if n.value else b""
        elif fmt.value == 32 and n.value == 1:
            # Xlib exposes format-32 values as native unsigned longs.
            value = c.cast(data, c.POINTER(W))[0]
        else:
            value = None
        return kind.value, fmt.value, n.value, value
    finally:
        if data.value:
            x.XFree(data)
def text_reply(value):
    try:
        value.decode("utf-8")
    except UnicodeDecodeError:
        return ("INVALID",)
    return ("TEXT", value.hex() or "-")
def read_selection():
    # One deadline includes the timestamp request, initial reply and EVERY INCR
    # chunk. A fresh requestor isolates retries from abandoned or late chunks.
    until = time.monotonic() + 3
    requestor = x.XCreateSimpleWindow(d, root, 0, 0, 1, 1, 0, 0, 0)
    if not requestor:
        return ("INVALID",)
    x.XSelectInput(d, requestor, 1 << 22)
    try:
        t = server_time(until)
        if not t:
            return ("TIMEOUT",)
        x.XConvertSelection(d, CLIPBOARD, UTF8, INBOX, requestor, t)
        x.XFlush(d)
        v = wait_until(lambda v: v.type == 31 and v.sel.requestor == requestor
                       and v.sel.selection == CLIPBOARD and v.sel.target == UTF8
                       and v.sel.time == t, until)
        if v is None:
            return ("TIMEOUT",)
        if v.sel.property == 0:
            return ("NONE",)
        if v.sel.property != INBOX:
            return ("INVALID",)
        prop = read_property(requestor)
        if time.monotonic() >= until:
            return ("TIMEOUT",)
        if prop is None:
            return ("INVALID",)
        kind, fmt, count, value = prop
        if kind == UTF8 and fmt == 8:
            return text_reply(value)
        if kind != INCR or fmt != 32 or count != 1 or value > MAX_READ_BYTES:
            return ("INVALID",)
        minimum = value
        received = bytearray()
        while True:
            # Deletion acknowledges this property and asks for the NEXT chunk.
            x.XDeleteProperty(d, requestor, INBOX)
            x.XFlush(d)
            v = wait_until(lambda v: v.type == 28 and v.prop.window == requestor
                           and v.prop.atom == INBOX and v.prop.state == 0, until)
            if v is None:
                return ("TIMEOUT",)
            prop = read_property(requestor)
            if time.monotonic() >= until:
                return ("TIMEOUT",)
            if prop is None:
                return ("INVALID",)
            kind, fmt, count, chunk = prop
            if kind != UTF8 or fmt != 8 or len(received) + count > MAX_READ_BYTES:
                return ("INVALID",)
            if not count:
                # Complete only at a typed terminator meeting the size hint.
                if len(received) < minimum:
                    return ("INVALID",)
                x.XDeleteProperty(d, requestor, INBOX)
                x.XFlush(d)
                return text_reply(bytes(received))
            received.extend(chunk)
    finally:
        x.XDestroyWindow(d, requestor)
        x.XFlush(d)
def command(cmd):
    if cmd[0] == "own":
        t = server_time()
        state["text"], state["time"] = bytes.fromhex(cmd[1]), t
        x.XSetSelectionOwner(d, CLIPBOARD, w, t)
        x.XSync(d, 0)
        say("OWNED" if t and x.XGetSelectionOwner(d, CLIPBOARD) == w else "NOTOWNED")
    elif cmd[0] == "read":
        say(*read_selection())
    elif cmd[0] == "owner":
        o = x.XGetSelectionOwner(d, CLIPBOARD)
        say("OWNER", "self" if o == w else ("none" if o == 0 else "other"))
say("PEER", w)
serve(event, command)
"#;

fn hex(text: &str) -> String {
    use std::fmt::Write;
    text.bytes().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}
/// A unique, non-ASCII item per test run (never a constant a stale
/// selection could already hold).
pub(super) fn unique(label: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{label} {nanos} · λ 👋 ünïcödé ✓")
}
pub(super) struct Peer(Harness);
impl Peer {
    pub(super) fn start(display: &str) -> Self {
        Self(Harness::start(&format!("{PRELUDE}{PEER}"), display, "PEER"))
    }
    /// This application copies `text` (takes CLIPBOARD ownership).
    pub(super) fn copy(&mut self, text: &str) {
        assert_eq!(self.0.ask(&format!("own {}", hex(text))), ["OWNED"]);
    }
    /// What an application pasting now would get as `UTF8_STRING`.
    fn paste(&mut self) -> Option<String> {
        let answer = self.0.ask("read");
        match answer[0].as_str() {
            "TEXT" if answer[1] == "-" => Some(String::new()),
            "TEXT" => {
                let bytes: Vec<u8> = (0..answer[1].len())
                    .step_by(2)
                    .map(|i| u8::from_str_radix(&answer[1][i..i + 2], 16).unwrap())
                    .collect();
                Some(String::from_utf8(bytes).unwrap())
            }
            _ => None,
        }
    }
    fn owner(&mut self) -> String {
        let answer = self.0.ask("owner");
        assert_eq!(answer[0], "OWNER", "{answer:?}");
        answer[1].clone()
    }
}
/// Poll an independent application's paste until it is `want`.
pub(super) fn pasted(peer: &mut Peer, want: &str, limit: Duration) -> Result<(), Option<String>> {
    let until = Instant::now() + limit;
    let mut last = None;
    while Instant::now() < until {
        last = peer.paste();
        if last.as_deref() == Some(want) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(last)
}
/// Live `fr-input-agent --clipboard` children (the host's per-lane X11 owners).
fn clipboard_children() -> usize {
    std::fs::read_dir("/proc")
        .unwrap()
        .filter_map(|entry| std::fs::read(entry.ok()?.path().join("cmdline")).ok())
        .filter(|cmdline| {
            let mut words = cmdline.split(|b| *b == 0);
            words
                .next()
                .is_some_and(|image| image.ends_with(b"/fr-input-agent"))
                && words.next() == Some(b"--clipboard")
        })
        .count()
}
/// The shipped client's JSON completion.
fn completion(output: &std::process::Output, host: &str) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(stdout.trim()).unwrap_or_else(|_| {
        panic!(
            "fr: {stdout} {}; host: {host}",
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress; two Xvfb displays; real input agent and clipboard child"]
fn fr_connect_clipboard_crosses_both_ways_through_frd_run() {
    let mut s = Controlled::start_with(true, true);
    let mut host = Peer::start(&s.host.display);
    let mut viewer = Peer::start(&s.viewer.display);

    // VIEWER copy -> HOST paste: an application on the viewer display owns
    // CLIPBOARD; an independent application on the host display pastes it.
    let first = unique("viewer→host");
    viewer.copy(&first);
    if let Err(last) = pasted(&mut host, &first, Duration::from_secs(20)) {
        panic!(
            "host never pasted the viewer's copy (last {last:?}); host: {}",
            s.daemon.dump()
        );
    }
    // The host item is owned by the per-lane child, not by the application.
    assert_eq!(host.owner(), "other");
    assert_eq!(clipboard_children(), 1, "one X11 owner for the one lane");

    // HOST copy -> VIEWER paste, a different unique non-ASCII text.
    let second = unique("host→viewer");
    assert_ne!(first, second);
    host.copy(&second);
    if let Err(last) = pasted(&mut viewer, &second, Duration::from_secs(20)) {
        panic!(
            "viewer never pasted the host's copy (last {last:?}); host: {}",
            s.daemon.dump()
        );
    }
    // Control still works alongside the clipboard.
    assert!(
        s.pointer_follows((222, 111), Duration::from_secs(10)),
        "control after clipboard: {}",
        s.daemon.dump()
    );

    // The user closes the viewer: content-free completion, then the lease's
    // executor AND the clipboard child go away with the lease.
    s.close_viewer();
    let output = wait_for(s.client, Duration::from_secs(30));
    let report = completion(&output, &s.daemon.dump());
    assert_eq!(report["outcome"], "stopped", "{report}");
    assert_eq!(report["control_granted"], true, "{report}");
    assert_eq!(report["clipboard_requested"], true, "{report}");
    assert_eq!(report["clipboard_active"], true, "{report}");
    assert!(
        report["clipboard_received"].as_u64().unwrap() >= 1,
        "{report}"
    );
    assert_eq!(report["clipboard_absence"], serde_json::Value::Null);
    let text = report.to_string();
    for secret in [&first, &second] {
        let nonce = secret.split(' ').nth(1).unwrap();
        assert!(!text.contains(nonce), "completion leaked clipboard text");
    }
    assert!(
        eventually(Duration::from_secs(15), || indicator(&mut s.observer)
            .is_none()),
        "indicator outlived the session"
    );
    assert!(
        eventually(Duration::from_secs(15), || clipboard_children() == 0),
        "the clipboard child outlived the lease"
    );
    // The host's own copy stays on its CLIPBOARD (never reset by teardown).
    assert_eq!(host.paste().as_deref(), Some(second.as_str()));
    drop((host, viewer));
    s.daemon.finish();
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress; two Xvfb displays; real input agent"]
fn a_host_without_clipboard_keeps_control_and_the_client_reports_typed_absence() {
    // `frd run --input-agent --no-clipboard`; the client requests it by default.
    let mut s = Controlled::start_with(false, true);
    let mut host = Peer::start(&s.host.display);
    let mut viewer = Peer::start(&s.viewer.display);
    let copied = unique("never crosses");
    viewer.copy(&copied);
    // Long enough for a working lane (the positive test crosses in ~1 s).
    thread::sleep(Duration::from_secs(4));
    assert_ne!(host.paste().as_deref(), Some(copied.as_str()));
    assert_eq!(host.owner(), "none", "nothing took the host CLIPBOARD");
    assert_eq!(clipboard_children(), 0, "no clipboard child was launched");
    // Control itself is unaffected.
    assert!(
        s.pointer_follows((150, 150), Duration::from_secs(10)),
        "control without clipboard: {}",
        s.daemon.dump()
    );
    s.close_viewer();
    let output = wait_for(s.client, Duration::from_secs(30));
    let report = completion(&output, &s.daemon.dump());
    assert_eq!(report["outcome"], "stopped", "{report}");
    assert_eq!(report["control_granted"], true, "{report}");
    assert_eq!(report["clipboard_requested"], true, "{report}");
    assert_eq!(report["clipboard_active"], false, "{report}");
    assert_eq!(report["clipboard_received"], 0, "{report}");
    assert_eq!(
        report["clipboard_absence"], "host_clipboard_unavailable",
        "{report}"
    );
    drop((host, viewer));
    s.daemon.finish();
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress; two Xvfb displays; real input agent and clipboard child"]
fn a_lost_lease_ends_the_clipboard_and_later_copies_cross_neither_way() {
    let mut s = Controlled::start_with(true, true);
    let mut host = Peer::start(&s.host.display);
    let mut viewer = Peer::start(&s.viewer.display);
    // The lane works first (so the fence below is not mere absence).
    let before = unique("before the freeze");
    viewer.copy(&before);
    if let Err(last) = pasted(&mut host, &before, Duration::from_secs(20)) {
        panic!(
            "lane never worked (last {last:?}); host: {}",
            s.daemon.dump()
        );
    }
    assert_eq!(clipboard_children(), 1);
    // Freeze the whole client past its 3 s lease: the host ends the lease,
    // and with it the clipboard lane and its X11 child.
    signal(&s.client, "-STOP");
    assert!(
        eventually(Duration::from_secs(15), || indicator(&mut s.observer)
            .is_none()),
        "lease survived a frozen client: {}",
        s.daemon.dump()
    );
    assert!(
        eventually(Duration::from_secs(15), || clipboard_children() == 0),
        "the clipboard child outlived the lease"
    );
    let on_host = unique("host after the lease");
    host.copy(&on_host);
    signal(&s.client, "-CONT");
    let on_viewer = unique("viewer after the lease");
    viewer.copy(&on_viewer);
    thread::sleep(Duration::from_secs(4));
    assert_eq!(
        host.paste().as_deref(),
        Some(on_host.as_str()),
        "a post-lease viewer copy reached the host"
    );
    assert_eq!(
        viewer.paste().as_deref(),
        Some(on_viewer.as_str()),
        "a post-lease host copy reached the viewer"
    );
    assert_eq!(clipboard_children(), 0, "the clipboard was reacquired");
    if s.client.try_wait().unwrap().is_none() {
        signal(&s.client, "-INT");
    }
    let output = wait_for(s.client, Duration::from_secs(30));
    println!(
        "fr after lease loss: {:?} {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout)
    );
    drop((host, viewer));
    s.daemon.finish();
}

/// A genuine original ingress deadline expires while only the viewer's native
/// clipboard worker is parked. The live QUIC bridge has admitted the host's
/// Begin into its one fixed handoff; it cannot renew that deadline or publish
/// after the worker resumes. Control and media keep running throughout.
/// This retains the regression for 145df7c without assuming a modern 200-KiB
/// copy must fail at 120 ms RTT (fr-clipboard-wan-item-ceiling-6mlb).
#[test]
#[ignore = "explicit isolated user/mount/network namespace with ptrace; synthetic ingress; two Xvfb displays; real input agent and clipboard child"]
fn a_copy_that_outlives_its_deadline_on_a_slow_path_ends_the_clipboard_not_control() {
    let mut s = Controlled::start_with(true, true);
    let mut host = Peer::start(&s.host.display);
    let mut viewer = Peer::start(&s.viewer.display);
    let before = unique("before the clipboard stall");
    host.copy(&before);
    if let Err(last) = pasted(&mut viewer, &before, Duration::from_secs(20)) {
        panic!(
            "lane never worked (last {last:?}); host: {}",
            s.daemon.dump()
        );
    }
    assert_eq!(clipboard_children(), 1, "the original host lane is active");
    let parked = ParkedClipboard::start(&s.client);
    let late = unique("host item awaiting the original viewer worker");
    host.copy(&late);
    // No outgoing viewer item exists. Its exact completion below can therefore
    // name HandoffExpired only after this incoming Begin was actually admitted
    // and its immutable local bound crossed; an idle parked worker cannot pass.
    assert!(
        eventually(Duration::from_secs(15), || clipboard_children() == 0),
        "the expired lane did not retire its original host child: {}",
        s.daemon.dump()
    );
    assert!(
        s.client.try_wait().unwrap().is_none(),
        "a failed copy ended the session: {}",
        s.daemon.dump()
    );
    assert!(
        s.pointer_follows((150, 150), Duration::from_secs(10)),
        "control while only the clipboard worker is stopped: {}",
        s.daemon.dump()
    );
    parked.resume();
    // Resuming the same owner cannot resurrect its queued record. Its X11
    // connection owned `before`; actual connection teardown loses that owner.
    assert!(
        eventually(Duration::from_secs(5), || viewer.owner() == "none"),
        "the original viewer clipboard owner did not finish cleanup"
    );
    assert_ne!(
        viewer.paste().as_deref(),
        Some(late.as_str()),
        "the late item was published"
    );
    assert_eq!(clipboard_children(), 0, "the retired lane was reacquired");
    assert!(
        s.pointer_follows((220, 180), Duration::from_secs(10)),
        "control after the clipboard worker resumed: {}",
        s.daemon.dump()
    );
    s.close_viewer();
    let output = wait_for(s.client, Duration::from_secs(30));
    let report = completion(&output, &s.daemon.dump());
    assert_eq!(report["outcome"], "stopped", "{report}");
    assert_eq!(report["control_granted"], true, "{report}");
    assert_eq!(report["attempts"], 1, "{report}");
    assert_eq!(report["clipboard_active"], true, "{report}");
    assert_eq!(
        report["clipboard_received"], 1,
        "only the initial host item was published: {report}"
    );
    assert_eq!(
        report["clipboard_ended"], "clipboard_item_expired",
        "{report}"
    );
    drop((host, viewer));
    s.daemon.finish();
}

/// Modern peers negotiate actual authority-bearing ticket renewal. A copy
/// larger than the original four-record window must complete on this impaired
/// namespace link; no claim is made about a live WAN or the one-MiB ceiling.
#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress; two Xvfb displays; real input agent and clipboard child; tc netem on loopback"]
fn a_modern_authority_copy_crosses_a_120ms_path_and_keeps_control() {
    let mut s = Controlled::start_with(true, true);
    let mut host = Peer::start(&s.host.display);
    let mut viewer = Peer::start(&s.viewer.display);
    // Start the copy after multiple real InputTicket renewal opportunities,
    // so the initial Granted proof alone cannot explain success.
    thread::sleep(Duration::from_secs(4));
    let copied = format!("{} {}", unique("modern impaired copy"), "x".repeat(200_000));
    let path = Netem::apply(60, 0.0, 0);
    viewer.copy(&copied);
    if let Err(last) = pasted(&mut host, &copied, Duration::from_secs(15)) {
        panic!(
            "modern copy did not cross (last size {:?}); host: {}",
            last.as_ref().map(String::len),
            s.daemon.dump()
        );
    }
    assert_eq!(
        clipboard_children(),
        1,
        "the same host native owner remains"
    );
    assert!(
        s.pointer_follows((180, 170), Duration::from_secs(10)),
        "control on the impaired path after the copy: {}",
        s.daemon.dump()
    );
    drop(path);
    s.close_viewer();
    let output = wait_for(s.client, Duration::from_secs(30));
    let report = completion(&output, &s.daemon.dump());
    assert_eq!(report["outcome"], "stopped", "{report}");
    assert_eq!(report["control_granted"], true, "{report}");
    assert_eq!(report["attempts"], 1, "{report}");
    assert_eq!(report["clipboard_active"], true, "{report}");
    assert_eq!(report["clipboard_absence"], serde_json::Value::Null);
    assert_eq!(report["clipboard_ended"], serde_json::Value::Null);
    drop((host, viewer));
    s.daemon.finish();
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress; two Xvfb displays; real input agent and clipboard child"]
fn a_capture_stall_keeps_the_clipboard_crossing_both_ways() {
    use super::real_control::{Painter, stall_capture};

    let mut s = Controlled::start_with(true, true);
    let mut host = Peer::start(&s.host.display);
    let mut viewer = Peer::start(&s.viewer.display);
    let before = unique("before the stall");
    viewer.copy(&before);
    if let Err(last) = pasted(&mut host, &before, Duration::from_secs(20)) {
        panic!(
            "clipboard never worked (last {last:?}); {}",
            s.daemon.dump()
        );
    }
    let painter = Painter::start(&s.host.display);
    thread::sleep(Duration::from_millis(1500));
    stall_capture(400);
    assert!(
        s.pointer_follows((300, 250), Duration::from_secs(20)),
        "control did not resume: {}",
        s.daemon.dump()
    );
    let to_host = unique("viewer→host after the stall");
    viewer.copy(&to_host);
    let host_result = pasted(&mut host, &to_host, Duration::from_secs(20));
    let to_viewer = unique("host→viewer after the stall");
    host.copy(&to_viewer);
    let viewer_result = pasted(&mut viewer, &to_viewer, Duration::from_secs(20));
    let children = clipboard_children();
    drop(painter);
    s.close_viewer();
    let output = wait_for(s.client, Duration::from_secs(30));
    let report = completion(&output, &s.daemon.dump());
    assert!(host_result.is_ok(), "viewer copy did not resume: {report}");
    assert!(viewer_result.is_ok(), "host copy did not resume: {report}");
    assert_eq!(children, 1, "the original lane keeps one native owner");
    assert_eq!(report["outcome"], "stopped", "{report}");
    assert_eq!(report["attempts"], 1, "{report}");
    assert_eq!(report["clipboard_active"], true, "{report}");
    assert_eq!(report["clipboard_absence"], serde_json::Value::Null);
    assert!(
        report["last_attempt_media"]["recovered_streams"]
            .as_u64()
            .is_some_and(|n| n >= 1),
        "the capture stall did not exercise reference recovery: {report}"
    );
    assert!(
        report["input_suspensions"].as_u64().is_some_and(|n| n >= 1),
        "the capture stall did not suspend input: {report}"
    );
    drop((host, viewer));
    s.daemon.finish();
}
