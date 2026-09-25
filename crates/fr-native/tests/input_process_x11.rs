//! The real `fr-input-agent` child on a private Xvfb, driven by frd's real
//! `RemoteSink`/`Seat`/`InputSession`. Effects are observed by an INDEPENDENT
//! Xlib connection (pointer position, button mask, keymap, delivered events);
//! nothing here fabricates a native result. Authority below is an explicit
//! fixture grant, not Tailscale admission or local approval.
#![cfg(all(target_os = "linux", feature = "linux-input"))]
use asupersync::{
    cx::Cx,
    runtime::{Runtime, RuntimeBuilder},
    types::Budget,
};
use core::ffi::{c_char, c_int, c_long, c_uint, c_ulong, c_void};
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::*,
    input::*,
    input_sequence::InputOutcome,
    input_submission::*,
    limits::ProtocolLimits,
    time::HostDuration,
};
use fr_native::input::{X11Pointer, emergency};
use fr_wire::input::{InputDelivery, InputDirection, MAX_INPUT_RECORD_BYTES, encode_input};
use frd::{
    input_agent::{Agent, Driver, Error as AgentError, Phase, Reply, Route, Seat, Shutdown},
    input_process::{Fence, ProcessLaunch, RemoteSink, factory},
    input_watchdog::{StopReason, host_now},
};
use std::{
    ffi::CString,
    io::{BufRead, BufReader, Read},
    path::Path,
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

const AGENT: &str = env!("CARGO_BIN_EXE_fr-input-agent");

struct Server {
    child: Child,
    display: String,
}
impl Server {
    fn start() -> Self {
        fr_native::xlib::initialize_threads().unwrap();
        // Large enough for the executor's 480x148 sharing indicator.
        let mut child = Command::new("Xvfb")
            .args([
                "-displayfd",
                "1",
                "-screen",
                "0",
                "640x480x24",
                "-nolisten",
                "tcp",
                "-noreset",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("install Xvfb for native input tests");
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
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// Independent Xlib observer (Xlib.h layouts), never a production input path.
#[repr(C)]
#[derive(Clone, Copy)]
struct InputEventRecord {
    kind: c_int,
    serial: c_ulong,
    send_event: c_int,
    display: *mut c_void,
    window: c_ulong,
    root: c_ulong,
    subwindow: c_ulong,
    time: c_ulong,
    x: c_int,
    y: c_int,
    x_root: c_int,
    y_root: c_int,
    state: c_uint,
    detail: c_uint,
    same_screen: c_int,
}
#[repr(C)]
union Event {
    kind: c_int,
    input: InputEventRecord,
    padding: [c_long; 24],
}
#[repr(C)]
struct KeyboardState {
    key_click_percent: c_int,
    bell_percent: c_int,
    bell_pitch: c_uint,
    bell_duration: c_uint,
    led_mask: c_ulong,
    global_auto_repeat: c_int,
    auto_repeats: [u8; 32],
}
#[link(name = "X11")]
unsafe extern "C" {
    fn XOpenDisplay(name: *const c_char) -> *mut c_void;
    fn XCloseDisplay(d: *mut c_void) -> c_int;
    fn XDefaultRootWindow(d: *mut c_void) -> c_ulong;
    fn XCreateSimpleWindow(
        d: *mut c_void,
        parent: c_ulong,
        x: c_int,
        y: c_int,
        w: c_uint,
        h: c_uint,
        border: c_uint,
        border_pixel: c_ulong,
        bg: c_ulong,
    ) -> c_ulong;
    fn XMapWindow(d: *mut c_void, w: c_ulong) -> c_int;
    fn XLowerWindow(d: *mut c_void, w: c_ulong) -> c_int;
    fn XSetInputFocus(d: *mut c_void, w: c_ulong, revert: c_int, t: c_ulong) -> c_int;
    fn XSelectInput(d: *mut c_void, w: c_ulong, mask: c_long) -> c_int;
    fn XSync(d: *mut c_void, discard: c_int) -> c_int;
    fn XKeysymToKeycode(d: *mut c_void, sym: c_ulong) -> u8;
    fn XQueryKeymap(d: *mut c_void, keys: *mut u8) -> c_int;
    fn XQueryPointer(
        d: *mut c_void,
        w: c_ulong,
        root: *mut c_ulong,
        child: *mut c_ulong,
        rx: *mut c_int,
        ry: *mut c_int,
        wx: *mut c_int,
        wy: *mut c_int,
        state: *mut c_uint,
    ) -> c_int;
    fn XPending(d: *mut c_void) -> c_int;
    fn XNextEvent(d: *mut c_void, e: *mut Event) -> c_int;
    fn XGetKeyboardControl(d: *mut c_void, state: *mut KeyboardState) -> c_int;
    fn XConnectionNumber(d: *mut c_void) -> c_int;
}
#[link(name = "libXtst.so.6", kind = "dylib", modifiers = "+verbatim")]
unsafe extern "C" {
    fn XTestFakeKeyEvent(d: *mut c_void, code: c_uint, pressed: c_int, delay: c_ulong) -> c_int;
    fn XTestFakeButtonEvent(d: *mut c_void, b: c_uint, pressed: c_int, delay: c_ulong) -> c_int;
}
unsafe extern "C" {
    fn close(fd: c_int) -> c_int;
}
struct Observer {
    d: *mut c_void,
    window: c_ulong,
    root: c_ulong,
}
impl Observer {
    fn new(name: &str) -> Self {
        let name = CString::new(name).unwrap();
        // SAFETY: test-owned connection on this thread; closed in Drop.
        unsafe {
            let d = XOpenDisplay(name.as_ptr());
            assert!(!d.is_null());
            let root = XDefaultRootWindow(d);
            let window = XCreateSimpleWindow(d, root, 0, 0, 640, 480, 0, 0, 0);
            // Key, button and motion events on a full-screen focused window.
            XSelectInput(d, window, 1 | 2 | 4 | 8 | 64);
            XMapWindow(d, window);
            XLowerWindow(d, window);
            XSync(d, 0);
            XSetInputFocus(d, window, 1, 0);
            XSync(d, 0);
            Self { d, window, root }
        }
    }
    fn code(&self, sym: c_ulong) -> u8 {
        // SAFETY: live observer connection.
        unsafe { XKeysymToKeycode(self.d, sym) }
    }
    fn down(&self, code: u8) -> bool {
        let mut keys = [0; 32];
        // SAFETY: XQueryKeymap-sized writable buffer.
        unsafe {
            XQueryKeymap(self.d, keys.as_mut_ptr());
        }
        keys[usize::from(code) / 8] & (1 << (code % 8)) != 0
    }
    fn repeat(&self, code: u8) -> bool {
        let mut s = KeyboardState {
            key_click_percent: 0,
            bell_percent: 0,
            bell_pitch: 0,
            bell_duration: 0,
            led_mask: 0,
            global_auto_repeat: 0,
            auto_repeats: [0; 32],
        };
        // SAFETY: exact XKeyboardState layout, fully writable.
        unsafe {
            XGetKeyboardControl(self.d, &raw mut s);
        }
        s.auto_repeats[usize::from(code) / 8] & (1 << (code % 8)) != 0
    }
    fn pointer(&self) -> (i32, i32, u32) {
        let (mut root, mut child, mut rx, mut ry, mut wx, mut wy, mut mask) = (0, 0, 0, 0, 0, 0, 0);
        // SAFETY: live connection and independent scalar outputs.
        let ok = unsafe {
            XQueryPointer(
                self.d,
                self.root,
                &raw mut root,
                &raw mut child,
                &raw mut rx,
                &raw mut ry,
                &raw mut wx,
                &raw mut wy,
                &raw mut mask,
            )
        };
        assert_ne!(ok, 0);
        (rx, ry, mask & 0x1f00)
    }
    /// Server-delivered (kind, detail, synthetic) key/button events.
    fn events(&self) -> Vec<(i32, u32, bool)> {
        let mut out = vec![];
        // SAFETY: XNextEvent writes one XEvent into the padded union.
        unsafe {
            XSync(self.d, 0);
            while XPending(self.d) > 0 {
                let mut e = Event { padding: [0; 24] };
                XNextEvent(self.d, &raw mut e);
                if [2, 3, 4, 5].contains(&e.kind) {
                    assert_eq!(e.input.window, self.window);
                    out.push((e.kind, e.input.detail, e.input.send_event != 0));
                }
            }
        }
        out
    }
    fn release_key(&self, code: u8) {
        // SAFETY: cleanup of a test-induced leftover press on the private server.
        unsafe {
            XTestFakeKeyEvent(self.d, c_uint::from(code), 0, 0);
            XSync(self.d, 0);
        }
    }
    fn release_button(&self, button: c_uint) {
        // SAFETY: as above.
        unsafe {
            XTestFakeButtonEvent(self.d, button, 0, 0);
            XSync(self.d, 0);
        }
    }
}
impl Drop for Observer {
    fn drop(&mut self) {
        // SAFETY: closes the connection opened in `new`, once.
        unsafe {
            XCloseDisplay(self.d);
        }
    }
}
fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(Instant::now() < until, "timed out: {what}");
        thread::sleep(Duration::from_millis(1));
    }
}
const BUTTON1: u32 = 1 << 8;

fn runtime() -> Runtime {
    RuntimeBuilder::new().worker_threads(1).build().unwrap()
}
fn credentials() -> InputCredentials {
    InputCredentials {
        session: RemoteSessionId::from_raw(1),
        lease: InputLeaseId::from_raw(2),
        ticket: InputTicketId::from_raw(3),
        view: InputView {
            geometry: DisplayGeometryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
    }
}
fn probe(server: &Server) -> (InputBounds, Capabilities) {
    let native = X11Pointer::open(&server.display).unwrap();
    let caps = native.capabilities();
    assert!(
        caps.contains_all(
            Capabilities::default()
                .with(Capability::Keys)
                .with(Capability::Absolute)
                .with(Capability::Buttons)
        )
    );
    (native.bounds(), caps)
}
fn owner(cx: &Cx, bounds: InputBounds, caps: Capabilities, lifetime: u64) -> InputSession {
    let c = credentials();
    let now = host_now(cx).unwrap();
    // Explicit fixture grant, not a substitute for Tailscale or local approval.
    let mut a = SessionAuthority::new(
        c.session,
        AuthorityPolicy {
            authorization_lifetime: HostDuration::from_micros(lifetime),
            ticket_lifetime: HostDuration::from_micros(lifetime / 2),
        },
    );
    a.mark_capabilities_checked().unwrap();
    a.authorize_observation(now).unwrap();
    a.mark_view_ready(now).unwrap();
    a.grant_lease(c.lease, now).unwrap();
    a.issue_input_ticket(c.lease, c.ticket, now).unwrap();
    InputSession::new(a, c, bounds, caps, now).unwrap()
}
fn launch(server: &Server) -> ProcessLaunch {
    ProcessLaunch::new(Path::new(AGENT), &server.display, None, 0x5eed_0001).unwrap()
}
fn bytes(sequence: u64, event: InputEvent<'_>) -> Vec<u8> {
    let mut data = vec![0; MAX_INPUT_RECORD_BYTES];
    let n = encode_input(
        InputRequest {
            credentials: credentials(),
            sequence,
            event,
        },
        &mut data,
        &ProtocolLimits::ABSOLUTE,
        7,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .unwrap();
    data.truncate(n);
    data
}
fn receipt(agent: &mut Agent) -> Receipt {
    let mut reply = None;
    eventually("input receipt", || {
        reply = agent.try_reply().unwrap();
        reply.is_some()
    });
    match reply.unwrap() {
        Reply::Input(Ok(Dispatch::Completed(r))) => r,
        other => panic!("receipt required: {other:?}"),
    }
}
fn send(agent: &mut Agent, sequence: u64, event: InputEvent<'_>) -> Receipt {
    agent
        .submit(&bytes(sequence, event), InputDelivery::Reliable)
        .unwrap();
    receipt(agent)
}
fn key(transition: KeyTransition) -> InputEvent<'static> {
    InputEvent::Key {
        key: PhysicalKey::new(4).unwrap(),
        transition,
    }
}
fn button(pressed: bool, x: i32, y: i32) -> InputEvent<'static> {
    InputEvent::Button {
        button: PointerButton::Primary,
        pressed,
        position: DesktopPoint { x, y },
        barrier: 0,
    }
}
struct Running {
    done: mpsc::Receiver<Shutdown>,
    join: thread::JoinHandle<()>,
}
impl Running {
    fn new(rt: Runtime, driver: Driver) -> Self {
        let (tx, done) = mpsc::sync_channel(1);
        let join = thread::spawn(move || {
            let _ = tx.send(rt.block_on(driver));
        });
        Self { done, join }
    }
    fn finish(self) -> Shutdown {
        let shutdown = self.done.recv_timeout(Duration::from_secs(8)).unwrap();
        self.join.join().unwrap();
        shutdown
    }
}
/// Production composition: canonical owner on frd's native thread, the real
/// child as its sink, and the lease's fence installed before any input.
fn start(server: &Server, seat: &Seat, lifetime: u64) -> (Agent, Running) {
    let (bounds, caps) = probe(server);
    let rt = runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let session = owner(&cx, bounds, caps, lifetime);
    let fence = Fence::default();
    let make = factory(launch(server), cx.clone(), bounds, caps, fence.clone());
    let (agent, driver) = seat
        .start(
            cx,
            session,
            Route::new(7, ProtocolLimits::ABSOLUTE),
            make,
            RemoteSink::native_cleanup,
        )
        .unwrap();
    assert!(
        agent
            .control()
            .install_fence(Box::new(move || fence.signal()))
    );
    (agent, Running::new(rt, driver))
}
/// The one child this test launched on its private display (argv/env are
/// private; the display selects between concurrently running tests).
fn agent_pid(display: &str) -> u32 {
    let wanted = format!("DISPLAY={display}\0");
    let me = std::process::id();
    let mut found = None;
    eventually("executor process", || {
        found = std::fs::read_dir("/proc")
            .unwrap()
            .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<u32>().ok())
            .find(|pid| {
                let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
                let parent = stat
                    .rsplit_once(')')
                    .and_then(|(_, rest)| rest.split_whitespace().nth(1))
                    .and_then(|p| p.parse::<u32>().ok());
                let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
                let env = std::fs::read(format!("/proc/{pid}/environ")).unwrap_or_default();
                parent == Some(me)
                    && comm.trim_end() == "fr-input-agent"
                    && env.windows(wanted.len()).any(|w| w == wanted.as_bytes())
            });
        found.is_some()
    });
    found.unwrap()
}
fn signal(pid: u32, name: &str) {
    assert!(
        Command::new("kill")
            .args([name, &pid.to_string()])
            .status()
            .unwrap()
            .success()
    );
}

#[test]
fn real_child_moves_clicks_and_types_on_an_independently_observed_display() {
    let server = Server::start();
    let o = Observer::new(&server.display);
    let a = o.code(0x61);
    let repeat_before = o.repeat(a);
    let seat = Seat::default();
    let (mut agent, running) = start(&server, &seat, 3_000_000);
    let moved = send(
        &mut agent,
        0,
        InputEvent::Pointer {
            position: DesktopPoint { x: 300, y: 350 },
        },
    );
    assert_eq!(moved.outcome, InputOutcome::SubmittedToOs);
    eventually("pointer moved", || o.pointer() == (300, 350, 0));
    assert_eq!(
        send(&mut agent, 0, button(true, 310, 360)).submitted_operations,
        2
    );
    eventually("button held", || o.pointer() == (310, 360, BUTTON1));
    assert_eq!(o.events(), [(4, 1, false)]);
    send(&mut agent, 1, button(false, 310, 360));
    eventually("button released", || o.pointer() == (310, 360, 0));
    assert_eq!(o.events(), [(5, 1, false)]);
    assert_eq!(
        send(&mut agent, 2, key(KeyTransition::Press)).outcome,
        InputOutcome::SubmittedToOs
    );
    eventually("key held", || o.down(a));
    assert!(!o.repeat(a), "server auto-repeat suppressed while held");
    assert_eq!(o.events(), [(2, u32::from(a), false)]);
    send(&mut agent, 3, key(KeyTransition::Release));
    eventually("key released", || !o.down(a));
    assert_eq!(o.events(), [(3, u32::from(a), false)]);
    assert_eq!(o.repeat(a), repeat_before);
    agent.control().stop(StopReason::ClientDisconnected);
    let shutdown = running.finish();
    assert!(shutdown.handoff_safe(), "{shutdown:?}");
    assert!(!seat.is_occupied());
}

#[test]
fn expired_lease_submits_nothing_and_the_pointer_never_moves() {
    let server = Server::start();
    let o = Observer::new(&server.display);
    let before = o.pointer();
    let seat = Seat::default();
    let (mut agent, running) = start(&server, &seat, 300_000);
    thread::sleep(Duration::from_millis(400));
    let late = agent.submit(
        &bytes(
            0,
            InputEvent::Pointer {
                position: DesktopPoint { x: 400, y: 400 },
            },
        ),
        InputDelivery::Reliable,
    );
    assert_eq!(late, Err(AgentError::Stopped));
    let shutdown = running.finish();
    assert_eq!(shutdown.reason, StopReason::AuthorityEnded);
    assert!(shutdown.handoff_safe());
    thread::sleep(Duration::from_millis(50));
    assert_eq!(o.pointer(), before);
    assert!(!seat.is_occupied());
}

#[test]
fn sigstopped_child_resumed_past_its_deadline_reports_expired_and_moves_nothing() {
    let server = Server::start();
    let o = Observer::new(&server.display);
    let (bounds, caps) = probe(&server);
    let rt = runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let mut sink = factory(launch(&server), cx.clone(), bounds, caps, Fence::default())().unwrap();
    let pid = agent_pid(&server.display);
    let before = o.pointer();
    let target = Operation::Absolute(DesktopPoint { x: 500, y: 420 });
    sink.prepare(target).unwrap();
    signal(pid, "-STOP");
    let resume = thread::spawn(move || {
        thread::sleep(Duration::from_millis(300));
        signal(pid, "-CONT");
    });
    let until = host_now(&cx)
        .unwrap()
        .checked_add(HostDuration::from_micros(100_000))
        .unwrap();
    // The frame waits in the private socket while the child is stopped; the
    // child's OWN clock check after SIGCONT refuses it without a native call.
    assert_eq!(sink.submit_until(target, until), Submission::Expired);
    resume.join().unwrap();
    thread::sleep(Duration::from_millis(50));
    assert_eq!(o.pointer(), before);
    // Not a dead child: a fresh, in-time submission is performed.
    sink.prepare(target).unwrap();
    let until = host_now(&cx)
        .unwrap()
        .checked_add(HostDuration::from_micros(1_000_000))
        .unwrap();
    assert_eq!(sink.submit_until(target, until), Submission::Submitted);
    eventually("in-time move", || o.pointer() == (500, 420, 0));
    assert!(sink.native_cleanup());
}

#[test]
fn revoke_with_a_held_button_releases_it_and_later_presses_are_refused() {
    let server = Server::start();
    let o = Observer::new(&server.display);
    let seat = Seat::default();
    let (mut agent, running) = start(&server, &seat, 3_000_000);
    send(&mut agent, 0, button(true, 320, 300));
    eventually("button held", || o.pointer() == (320, 300, BUTTON1));
    agent.control().stop(StopReason::LocalRevoke);
    eventually("revoked button released", || o.pointer() == (320, 300, 0));
    assert_eq!(
        agent.submit(&bytes(1, button(true, 330, 300)), InputDelivery::Reliable),
        Err(AgentError::Stopped)
    );
    let shutdown = running.finish();
    assert_eq!(shutdown.reason, StopReason::LocalRevoke);
    assert!(shutdown.handoff_safe(), "{shutdown:?}");
    assert!(!seat.is_occupied());
    assert_eq!(o.pointer(), (320, 300, 0));
    let events = o.events();
    assert_eq!(events, [(4, 1, false), (5, 1, false)]);
}

#[test]
fn a_fenced_child_itself_refuses_presses_but_performs_release_only_cleanup() {
    let server = Server::start();
    let o = Observer::new(&server.display);
    let a = o.code(0x61);
    let (bounds, caps) = probe(&server);
    let rt = runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let fence = Fence::default();
    let mut sink = factory(launch(&server), cx.clone(), bounds, caps, fence.clone())().unwrap();
    let until = || {
        host_now(&cx)
            .unwrap()
            .checked_add(HostDuration::from_micros(1_000_000))
            .unwrap()
    };
    let press = Operation::Key {
        key: PhysicalKey::new(4).unwrap(),
        transition: KeyTransition::Press,
    };
    sink.prepare(press).unwrap();
    assert_eq!(sink.submit_until(press, until()), Submission::Submitted);
    eventually("key held", || o.down(a));
    // Fence without revoking any frd-side authority: only the child enforces.
    fence.signal();
    let click = Operation::Button {
        button: PointerButton::Primary,
        pressed: true,
    };
    sink.prepare(click).unwrap();
    assert_eq!(sink.submit_until(click, until()), Submission::Fenced);
    let moved = Operation::Absolute(DesktopPoint { x: 10, y: 400 });
    sink.prepare(moved).unwrap();
    assert_eq!(sink.submit_until(moved, until()), Submission::Fenced);
    let release = Operation::Key {
        key: PhysicalKey::new(4).unwrap(),
        transition: KeyTransition::Release,
    };
    sink.prepare(release).unwrap();
    assert_eq!(sink.submit(release), Submission::Submitted);
    eventually("key released", || !o.down(a));
    assert!(sink.native_cleanup());
    assert_eq!(o.pointer().2, 0, "no button was pressed after the fence");
    assert_ne!((o.pointer().0, o.pointer().1), (10, 400));
    assert_eq!(
        o.events(),
        [(2, u32::from(a), false), (3, u32::from(a), false)]
    );
}

#[test]
fn killed_child_is_a_native_failure_and_the_seat_stays_occupied() {
    let server = Server::start();
    let o = Observer::new(&server.display);
    let seat = Seat::default();
    let (mut agent, running) = start(&server, &seat, 3_000_000);
    send(&mut agent, 0, button(true, 200, 300));
    eventually("button held", || o.pointer() == (200, 300, BUTTON1));
    signal(agent_pid(&server.display), "-KILL");
    let shutdown = running.finish();
    assert_eq!(shutdown.reason, StopReason::NativeFailure);
    assert!(!shutdown.handoff_safe(), "{shutdown:?}");
    assert!(seat.is_occupied(), "uncertain cleanup released the Seat");
    // Why the Seat is retained: the server keeps the killed client's XTest
    // press; nothing in this process claims it was released.
    thread::sleep(Duration::from_millis(50));
    assert_eq!(o.pointer().2, BUTTON1);
    o.release_button(1);
    drop(agent);
}

// ---- Emergency release (panic hook / Xlib error / Xlib I/O error) ----------
// Each case runs in a re-executed copy of this test binary ("victim"), so the
// real process-global handlers and exit paths run in a real process.
const VICTIM: &str = "FR_INPUT_EMERGENCY_VICTIM";
fn victim(display: &str, fault: &str) -> ! {
    let mut native = X11Pointer::open(display).unwrap();
    for op in [
        Operation::Absolute(DesktopPoint { x: 250, y: 250 }),
        Operation::Button {
            button: PointerButton::Primary,
            pressed: true,
        },
        Operation::Key {
            key: PhysicalKey::new(4).unwrap(),
            transition: KeyTransition::Press,
        },
    ] {
        native.prepare(op).unwrap();
        assert_eq!(native.submit(op), Submission::Submitted);
    }
    if fault != "abort-unprotected" {
        emergency::install(display).unwrap();
        emergency::record(native.native_held());
        assert!(!emergency::recorded().is_empty());
    }
    match fault {
        "panic" => panic!("victim panic with held input"),
        "xlib-error" => {
            let name = CString::new(display).unwrap();
            // SAFETY: a deliberately invalid request on a victim connection.
            unsafe {
                let d = XOpenDisplay(name.as_ptr());
                XMapWindow(d, 0x7fff_fff0);
                XSync(d, 0);
            }
        }
        "xlib-io" => {
            let name = CString::new(display).unwrap();
            // SAFETY: the socket is closed behind Xlib's back to force a fatal
            // I/O error on the next round trip; this process then exits.
            unsafe {
                let d = XOpenDisplay(name.as_ptr());
                close(XConnectionNumber(d));
                XSync(d, 0);
            }
        }
        "abort-unprotected" => {}
        other => panic!("unknown victim fault {other}"),
    }
    std::process::abort()
}
fn run_victim(test: &str, server: &Server, fault: &str) -> Option<i32> {
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env(VICTIM, format!("{fault}@{}", server.display))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    status.code()
}
fn victim_mode() -> Option<(String, String)> {
    let value = std::env::var(VICTIM).ok()?;
    let (fault, display) = value.split_once('@')?;
    Some((fault.into(), display.into()))
}
#[test]
fn emergency_path_releases_held_input_on_panic_xlib_error_and_io_error() {
    if let Some((fault, display)) = victim_mode() {
        victim(&display, &fault);
    }
    let name = "emergency_path_releases_held_input_on_panic_xlib_error_and_io_error";
    let server = Server::start();
    let o = Observer::new(&server.display);
    let a = o.code(0x61);
    // Negative control: an executor dying WITHOUT the emergency path leaves
    // the X server holding its XTest key and button.
    assert_eq!(run_victim(name, &server, "abort-unprotected"), None);
    assert!(o.down(a), "server released a dead client's XTest key");
    assert_eq!(o.pointer().2, BUTTON1);
    o.release_key(a);
    o.release_button(1);
    for (fault, code) in [
        ("panic", emergency::EXIT_PANIC),
        ("xlib-error", emergency::EXIT_XLIB_ERROR),
        ("xlib-io", emergency::EXIT_XLIB_IO),
    ] {
        assert!(!o.down(a) && o.pointer().2 == 0);
        assert_eq!(run_victim(name, &server, fault), Some(code), "{fault}");
        assert!(!o.down(a), "{fault}: key left held");
        assert_eq!(o.pointer().2, 0, "{fault}: button left held");
    }
}

// ---- The executor's remote-control indicator (plan 15.2; bead
// fr-rc-sec-approval-synthetic-input-t2r) -----------------------------------
const INDICATOR: &str = "FrankenRemote - Stop remote control";
#[repr(C)]
struct WindowAttributes {
    x: c_int,
    y: c_int,
    width: c_int,
    height: c_int,
    border_width: c_int,
    depth: c_int,
    visual: *mut c_void,
    root: c_ulong,
    class: c_int,
    bit_gravity: c_int,
    win_gravity: c_int,
    backing_store: c_int,
    backing_planes: c_ulong,
    backing_pixel: c_ulong,
    save_under: c_int,
    colormap: c_ulong,
    map_installed: c_int,
    map_state: c_int,
    all_event_masks: c_long,
    your_event_mask: c_long,
    do_not_propagate_mask: c_long,
    override_redirect: c_int,
    screen: *mut c_void,
}
#[repr(C)]
struct DeviceInfo {
    id: c_ulong,
    kind: c_ulong,
    name: *const c_char,
    num_classes: c_int,
    usage: c_int,
    classes: *mut c_void,
}
#[link(name = "X11")]
unsafe extern "C" {
    fn XQueryTree(
        d: *mut c_void,
        w: c_ulong,
        root: *mut c_ulong,
        parent: *mut c_ulong,
        children: *mut *mut c_ulong,
        n: *mut c_uint,
    ) -> c_int;
    fn XFetchName(d: *mut c_void, w: c_ulong, name: *mut *mut c_char) -> c_int;
    fn XFree(data: *mut c_void) -> c_int;
    fn XGetWindowAttributes(d: *mut c_void, w: c_ulong, a: *mut WindowAttributes) -> c_int;
    fn XSendEvent(
        d: *mut c_void,
        w: c_ulong,
        propagate: c_int,
        mask: c_long,
        event: *mut Event,
    ) -> c_int;
}
#[link(name = "libXtst.so.6", kind = "dylib", modifiers = "+verbatim")]
unsafe extern "C" {
    fn XTestFakeMotionEvent(
        d: *mut c_void,
        screen: c_int,
        x: c_int,
        y: c_int,
        delay: c_ulong,
    ) -> c_int;
    fn XTestFakeDeviceButtonEvent(
        d: *mut c_void,
        device: *mut c_void,
        button: c_uint,
        pressed: c_int,
        axes: *const c_int,
        count: c_int,
        delay: c_ulong,
    ) -> c_int;
    fn XTestFakeDeviceKeyEvent(
        d: *mut c_void,
        device: *mut c_void,
        code: c_uint,
        pressed: c_int,
        axes: *const c_int,
        count: c_int,
        delay: c_ulong,
    ) -> c_int;
}
#[link(name = "libXi.so.6", kind = "dylib", modifiers = "+verbatim")]
unsafe extern "C" {
    fn XListInputDevices(d: *mut c_void, count: *mut c_int) -> *mut DeviceInfo;
    fn XFreeDeviceList(list: *mut DeviceInfo);
    fn XOpenDevice(d: *mut c_void, id: c_ulong) -> *mut c_void;
    fn XCloseDevice(d: *mut c_void, device: *mut c_void) -> c_int;
}
impl Observer {
    /// The executor's mapped indicator (top-level window, by its fixed title).
    fn indicator(&self) -> Option<(c_ulong, i32, i32)> {
        let (mut root, mut parent, mut children, mut n) = (0, 0, std::ptr::null_mut(), 0);
        // SAFETY: live connection; XQueryTree/XFetchName outputs are freed here.
        unsafe {
            XSync(self.d, 0);
            if XQueryTree(
                self.d,
                self.root,
                &raw mut root,
                &raw mut parent,
                &raw mut children,
                &raw mut n,
            ) == 0
            {
                return None;
            }
            let windows = if children.is_null() {
                &[][..]
            } else {
                std::slice::from_raw_parts(children, n as usize)
            };
            let mut found = None;
            for &window in windows {
                let mut name = std::ptr::null_mut();
                if XFetchName(self.d, window, &raw mut name) != 0 && !name.is_null() {
                    let title = std::ffi::CStr::from_ptr(name).to_bytes() == INDICATOR.as_bytes();
                    XFree(name.cast());
                    let mut a: WindowAttributes = std::mem::zeroed();
                    if title
                        && XGetWindowAttributes(self.d, window, &raw mut a) != 0
                        && a.map_state == 2
                    {
                        found = Some((window, a.x, a.y));
                    }
                }
            }
            if !children.is_null() {
                XFree(children.cast());
            }
            found
        }
    }
    fn xtest_click(&self, x: i32, y: i32) {
        // SAFETY: private server; core XTest input exactly like the controller's.
        unsafe {
            XTestFakeMotionEvent(self.d, 0, x, y, 0);
            XTestFakeButtonEvent(self.d, 1, 1, 0);
            XTestFakeButtonEvent(self.d, 1, 0, 0);
            XSync(self.d, 0);
        }
    }
    fn focus(&self, window: c_ulong) {
        // SAFETY: private server; RevertToParent focus on an existing window.
        unsafe {
            XSetInputFocus(self.d, window, 2, 0);
            XSync(self.d, 0);
        }
    }
    fn xtest_key(&self, code: u8) {
        // SAFETY: private server; core XTest key press/release.
        unsafe {
            XTestFakeKeyEvent(self.d, c_uint::from(code), 1, 0);
            XTestFakeKeyEvent(self.d, c_uint::from(code), 0, 0);
            XSync(self.d, 0);
        }
    }
    /// A legacy `SendEvent` `ButtonPress` aimed at the indicator's creator.
    fn send_event_click(&self, window: c_ulong) {
        // SAFETY: XSendEvent copies one fully initialized padded XEvent.
        unsafe {
            let mut event = Event { padding: [0; 24] };
            event.input = InputEventRecord {
                kind: 4,
                serial: 0,
                send_event: 1,
                display: self.d,
                window,
                root: self.root,
                subwindow: 0,
                time: 0,
                x: 50,
                y: 95,
                x_root: 50,
                y_root: 95,
                state: 0,
                detail: 1,
                same_screen: 1,
            };
            XSendEvent(self.d, window, 0, 0, &raw mut event);
            XSync(self.d, 0);
        }
    }
    /// Xvfb has no physical devices. `XTest`'s DEVICE request attributes this
    /// event to the named non-XTEST slave ("Xvfb mouse"/"Xvfb keyboard"),
    /// standing in for local hardware: it exercises the source-device filter,
    /// not a physical device.
    fn device(&self, name: &str) -> *mut c_void {
        let mut count = 0;
        // SAFETY: the device list is read, then freed; the device is closed by
        // the caller through `close_device`.
        unsafe {
            let list = XListInputDevices(self.d, &raw mut count);
            assert!(!list.is_null());
            let devices = std::slice::from_raw_parts(list, usize::try_from(count).unwrap());
            let id = devices
                .iter()
                .find(|d| std::ffi::CStr::from_ptr(d.name).to_bytes() == name.as_bytes())
                .map(|d| d.id);
            XFreeDeviceList(list);
            let device = XOpenDevice(self.d, id.expect("Xvfb provides its core slave devices"));
            assert!(!device.is_null());
            device
        }
    }
    fn close_device(&self, device: *mut c_void) {
        // SAFETY: opened by `device` on this connection, closed once.
        unsafe {
            XCloseDevice(self.d, device);
        }
    }
    fn device_click(&self, x: i32, y: i32) {
        let mouse = self.device("Xvfb mouse");
        // SAFETY: positioned by core motion, then a press/release attributed
        // to the non-XTEST mouse slave; no axes.
        unsafe {
            XTestFakeMotionEvent(self.d, 0, x, y, 0);
            XTestFakeDeviceButtonEvent(self.d, mouse, 1, 1, std::ptr::null(), 0, 0);
            XTestFakeDeviceButtonEvent(self.d, mouse, 1, 0, std::ptr::null(), 0, 0);
            XSync(self.d, 0);
        }
        self.close_device(mouse);
    }
    fn device_key(&self, code: u8) {
        let keyboard = self.device("Xvfb keyboard");
        // SAFETY: as above, for the non-XTEST keyboard slave.
        unsafe {
            XTestFakeDeviceKeyEvent(
                self.d,
                keyboard,
                c_uint::from(code),
                1,
                std::ptr::null(),
                0,
                0,
            );
            XTestFakeDeviceKeyEvent(
                self.d,
                keyboard,
                c_uint::from(code),
                0,
                std::ptr::null(),
                0,
                0,
            );
            XSync(self.d, 0);
        }
        self.close_device(keyboard);
    }
}
fn still_controlling(agent: &mut Agent, o: &Observer, x: i32, sequence: u64) {
    thread::sleep(Duration::from_millis(250));
    assert!(
        !agent.control().is_stopped(),
        "synthetic input revoked control"
    );
    let moved = send(
        agent,
        sequence,
        InputEvent::Pointer {
            position: DesktopPoint { x, y: 300 },
        },
    );
    assert_eq!(moved.outcome, InputOutcome::SubmittedToOs);
    eventually("control still live", || o.pointer() == (x, 300, 0));
}

#[test]
fn indicator_is_shown_for_the_lease_and_only_a_real_device_click_stops_control() {
    let server = Server::start();
    let o = Observer::new(&server.display);
    let escape = o.code(0xff1b);
    let seat = Seat::default();
    let (mut agent, running) = start(&server, &seat, 3_000_000);
    // The native owner runs only after the child's Ready, and Ready is only
    // sent once the indicator is mapped.
    eventually("executor ready", || agent.status().phase == Phase::Running);
    let (window, x, y) = o.indicator().expect("indicator mapped before any input");
    let (stop_x, stop_y) = (x + 50, y + 95);
    // Planted negatives: the controller's own XTest click and Esc on the
    // indicator, and a legacy SendEvent click, do NOT revoke.
    o.xtest_click(stop_x, stop_y);
    still_controlling(&mut agent, &o, 300, 0);
    o.focus(window);
    o.xtest_key(escape);
    still_controlling(&mut agent, &o, 310, 1);
    o.send_event_click(window);
    still_controlling(&mut agent, &o, 320, 2);
    assert!(o.indicator().is_some(), "shown for the whole lease");
    // A click from a non-XTEST source device is the local user's revoke.
    o.device_click(stop_x, stop_y);
    let shutdown = running.finish();
    assert_eq!(shutdown.reason, StopReason::LocalRevoke);
    assert!(shutdown.handoff_safe(), "{shutdown:?}");
    assert!(!seat.is_occupied());
    assert_eq!(
        agent.submit(
            &bytes(
                3,
                InputEvent::Pointer {
                    position: DesktopPoint { x: 330, y: 300 },
                },
            ),
            InputDelivery::Reliable,
        ),
        Err(AgentError::Stopped)
    );
    eventually("indicator removed with its lease", || {
        o.indicator().is_none()
    });
}

#[test]
fn a_real_device_accelerator_on_the_indicator_revokes_and_releases_held_input() {
    let server = Server::start();
    let o = Observer::new(&server.display);
    let escape = o.code(0xff1b);
    let seat = Seat::default();
    let (mut agent, running) = start(&server, &seat, 3_000_000);
    eventually("executor ready", || agent.status().phase == Phase::Running);
    let (window, _, _) = o.indicator().expect("indicator mapped");
    send(&mut agent, 0, button(true, 400, 300));
    eventually("button held", || o.pointer() == (400, 300, BUTTON1));
    o.focus(window);
    o.device_key(escape);
    // Local revoke fences first, then the held button is released.
    eventually("revoked button released", || o.pointer() == (400, 300, 0));
    let shutdown = running.finish();
    assert_eq!(shutdown.reason, StopReason::LocalRevoke);
    assert!(shutdown.handoff_safe(), "{shutdown:?}");
    assert!(!seat.is_occupied());
}

#[test]
fn local_indicator_failures_always_revoke_their_owner() {
    use fr_native::sharing_indicator::{Error, Status, StopReason as IndicatorStop, start_with};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = || {
        let calls = calls.clone();
        move || {
            calls.fetch_add(1, Ordering::SeqCst);
        }
    };
    for display in ["", "host:0", ":0.1.2", ":0\0"] {
        assert_eq!(
            start_with(display, counted()).err(),
            Some(Error::InvalidDisplay)
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 4, "refusal revokes first");
    // No X server behind this display: the UI thread fails closed.
    let mut missing = start_with(":59998", counted()).unwrap();
    eventually("native failure", || missing.finish().is_some());
    assert_eq!(
        missing.control().status(),
        Status::Stopped(IndicatorStop::NativeFailure)
    );
    assert!(calls.load(Ordering::SeqCst) >= 5);
}
