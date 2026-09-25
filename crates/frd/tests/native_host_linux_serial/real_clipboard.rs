//! The shipped `fr connect --control --clipboard` against the production
//! `frd run --input-agent --clipboard`, with REAL X11 selections on both sides:
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
use super::real_control::{Controlled, Harness, PRELUDE, eventually, indicator, signal};
use super::real_media::close_window;
use super::shipped_client::wait_for;
use super::*;
use std::{thread, time::Instant};

/// One independent X11 application on one display: owns CLIPBOARD with a
/// given UTF-8 text (serving TARGETS, TIMESTAMP and `UTF8_STRING` with a real
/// server timestamp), or reads it with `XConvertSelection`.
const PEER: &str = r#"
sig(x.XInternAtom, W, D, c.c_char_p, I)
sig(x.XCreateSimpleWindow, W, D, W, I, I, U, U, U, W, W)
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
INBOX, TICK = atom(b"FR_E2E_INBOX"), atom(b"FR_E2E_TICK")
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
def wait(match, limit):
    until = time.monotonic() + limit
    while time.monotonic() < until:
        while x.XPending(d) > 0:
            e = Event()
            x.XNextEvent(d, c.byref(e))
            if match(CEvent.from_buffer(e)):
                return CEvent.from_buffer(e)
            event(e)
        time.sleep(0.005)
    return None
def server_time():
    # ICCCM: a zero-length append yields a PropertyNotify with the server time.
    x.XChangeProperty(d, w, TICK, 19, 32, 2, (W * 1)(0), 0)
    x.XFlush(d)
    v = wait(lambda v: v.type == 28 and v.prop.window == w and v.prop.atom == TICK, 3)
    return v.prop.time if v else 0
def command(cmd):
    if cmd[0] == "own":
        t = server_time()
        state["text"], state["time"] = bytes.fromhex(cmd[1]), t
        x.XSetSelectionOwner(d, CLIPBOARD, w, t)
        x.XSync(d, 0)
        say("OWNED" if t and x.XGetSelectionOwner(d, CLIPBOARD) == w else "NOTOWNED")
    elif cmd[0] == "read":
        t = server_time()
        x.XDeleteProperty(d, w, INBOX)
        x.XConvertSelection(d, CLIPBOARD, UTF8, INBOX, w, t)
        x.XFlush(d)
        v = wait(lambda v: v.type == 31 and v.sel.requestor == w and v.sel.selection == CLIPBOARD, 3)
        if v is None:
            say("TIMEOUT")
        elif v.sel.property == 0:
            say("NONE")
        else:
            kind, fmt, n, after, data = W(), I(), W(), W(), c.c_void_p()
            x.XGetWindowProperty(d, w, INBOX, 0, 1 << 20, 1, 0, c.byref(kind), c.byref(fmt),
                                 c.byref(n), c.byref(after), c.byref(data))
            value = c.string_at(data.value, n.value) if data.value else b""
            if data.value:
                x.XFree(data)
            if kind.value == UTF8:
                say("TEXT", value.hex() or "-")
            else:
                say("TYPE", kind.value)
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
fn unique(label: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{label} {nanos} · λ 👋 ünïcödé ✓")
}
struct Peer(Harness);
impl Peer {
    fn start(display: &str) -> Self {
        Self(Harness::start(&format!("{PRELUDE}{PEER}"), display, "PEER"))
    }
    /// This application copies `text` (takes CLIPBOARD ownership).
    fn copy(&mut self, text: &str) {
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
fn pasted(peer: &mut Peer, want: &str, limit: Duration) -> Result<(), Option<String>> {
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
    close_window(&s.viewer.display, s.window.0);
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
    // `frd run --input-agent` WITHOUT --clipboard; the client asks for it.
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
    close_window(&s.viewer.display, s.window.0);
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
