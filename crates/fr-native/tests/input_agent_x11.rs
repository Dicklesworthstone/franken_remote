#![cfg(all(target_os = "linux", feature = "linux-input-agent"))]
use asupersync::{
    cx::Cx,
    runtime::{Runtime, RuntimeBuilder},
    types::Budget,
};
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::*,
    input::*,
    input_sequence::InputOutcome,
    input_submission::*,
    limits::ProtocolLimits,
    time::HostDuration,
};
use fr_native::{input::X11Pointer, input_agent::start_x11};
use fr_wire::input::{InputDelivery, InputDirection, MAX_INPUT_RECORD_BYTES, encode_input};
use frd::{
    input_agent::{Agent, Driver, Reply, Route, Seat, Shutdown},
    input_watchdog::{StopReason, host_now},
};
use std::{
    ffi::{CString, c_char, c_int, c_uint, c_ulong, c_void},
    io::{BufRead, BufReader, Read},
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

struct Server {
    child: Child,
    display: String,
}
impl Server {
    fn start() -> Self {
        fr_native::xlib::initialize_threads().unwrap();
        let mut child = Command::new("Xvfb")
            .args([
                "-displayfd",
                "1",
                "-screen",
                "0",
                "320x240x24",
                "-nolisten",
                "tcp",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
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
// An independent Xlib connection observes server state. It never fabricates
// submission, release, or keyboard repeat evidence for the production adapter.
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
    fn XCloseDisplay(display: *mut c_void) -> c_int;
    fn XKeysymToKeycode(display: *mut c_void, sym: c_ulong) -> u8;
    fn XQueryKeymap(display: *mut c_void, keys: *mut u8) -> c_int;
    fn XGetKeyboardControl(display: *mut c_void, state: *mut KeyboardState) -> c_int;
}
struct Observer {
    display: *mut c_void,
    code: u8,
    pointer: X11Pointer,
}
impl Observer {
    fn open(name: &str) -> Self {
        let pointer = X11Pointer::open(name).unwrap();
        let name = CString::new(name).unwrap();
        // SAFETY: live NUL-terminated name; this test owns and closes the display
        // on the same thread, after process-wide Xlib initialization.
        let display = unsafe { XOpenDisplay(name.as_ptr()) };
        assert!(!display.is_null());
        // The private Xvfb server's default US map supplies A for USB usage 4.
        let code = unsafe { XKeysymToKeycode(display, 0x61) };
        assert!(code >= 8);
        Self {
            display,
            code,
            pointer,
        }
    }
    fn held(&self) -> bool {
        let mut keys = [0u8; 32];
        // SAFETY: writable XQueryKeymap-sized buffer and our live connection.
        assert_ne!(unsafe { XQueryKeymap(self.display, keys.as_mut_ptr()) }, 0);
        keys[usize::from(self.code) / 8] & (1 << (self.code % 8)) != 0
    }
    fn repeats(&self) -> bool {
        let mut state = KeyboardState {
            key_click_percent: 0,
            bell_percent: 0,
            bell_pitch: 0,
            bell_duration: 0,
            led_mask: 0,
            global_auto_repeat: 0,
            auto_repeats: [0; 32],
        };
        // SAFETY: exact native XKeyboardState layout, fully writable.
        unsafe { XGetKeyboardControl(self.display, &raw mut state) };
        state.auto_repeats[usize::from(self.code) / 8] & (1 << (self.code % 8)) != 0
    }
    fn dragging(&mut self) -> bool {
        self.pointer.query_pointer().unwrap().1 & 256 != 0
    }
}
impl Drop for Observer {
    fn drop(&mut self) {
        unsafe { XCloseDisplay(self.display) };
    }
}
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
fn eventually(mut predicate: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(5);
    while !predicate() {
        assert!(Instant::now() < until, "native X11 condition timed out");
        thread::sleep(Duration::from_millis(1));
    }
}
fn reply(agent: &mut Agent) -> Reply {
    let mut r = None;
    eventually(|| {
        r = agent.try_reply().unwrap();
        r.is_some()
    });
    r.unwrap()
}
fn send(agent: &mut Agent, sequence: u64, event: InputEvent<'_>) {
    agent
        .submit(&bytes(sequence, event), InputDelivery::Reliable)
        .unwrap();
    let Reply::Input(Ok(Dispatch::Completed(r))) = reply(agent) else {
        panic!("native receipt required")
    };
    assert_eq!(r.outcome, InputOutcome::SubmittedToOs);
}
fn press() -> InputEvent<'static> {
    InputEvent::Key {
        key: PhysicalKey::new(4).unwrap(),
        transition: KeyTransition::Press,
    }
}
fn drag() -> InputEvent<'static> {
    InputEvent::Button {
        button: PointerButton::Primary,
        pressed: true,
        position: DesktopPoint { x: 20, y: 30 },
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
        let result = self.done.recv_timeout(Duration::from_secs(5)).unwrap();
        self.join.join().unwrap();
        result
    }
}
fn launch(server: &Server, probe: &X11Pointer, seat: &Seat, lifetime: u64) -> (Agent, Running) {
    let rt = runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let session = owner(&cx, probe.bounds(), probe.capabilities(), lifetime);
    let (agent, driver) = start_x11(
        seat,
        cx,
        session,
        Route::new(7, ProtocolLimits::ABSOLUTE),
        &server.display,
    )
    .unwrap();
    (agent, Running::new(rt, driver))
}
#[test]
fn actual_idle_expiry_releases_key_and_drag_and_restores_repeat() {
    let server = Server::start();
    let mut observer = Observer::open(&server.display);
    let before = observer.repeats();
    let seat = Seat::default();
    let (mut agent, running) = launch(&server, &observer.pointer, &seat, 1_000_000);
    send(&mut agent, 0, press());
    send(&mut agent, 1, drag());
    assert!(observer.held());
    assert!(observer.dragging());
    assert!(!observer.repeats());
    // No more packets, explicit release, or locally invoked core maintain call.
    let shutdown = running.finish();
    assert_eq!(shutdown.reason, StopReason::AuthorityEnded);
    assert!(shutdown.handoff_safe());
    assert!(!observer.held());
    assert!(!observer.dragging());
    assert_eq!(observer.repeats(), before);
    assert!(!seat.is_occupied());
}
#[test]
fn local_revoke_releases_before_a_second_native_controller_is_admitted() {
    let server = Server::start();
    let mut observer = Observer::open(&server.display);
    let seat = Seat::default();
    let (mut agent, running) = launch(&server, &observer.pointer, &seat, 3_000_000);
    send(&mut agent, 0, press());
    send(&mut agent, 1, drag());
    let old = agent.control();
    old.stop(StopReason::LocalRevoke);
    assert!(running.finish().handoff_safe());
    assert!(!observer.held());
    assert!(!observer.dragging());
    let (mut next, running) = launch(&server, &observer.pointer, &seat, 3_000_000);
    old.stop(StopReason::Suspended);
    assert!(!next.control().is_stopped());
    send(&mut next, 0, press());
    assert!(observer.held());
    next.control().stop(StopReason::ClientDisconnected);
    assert!(running.finish().handoff_safe());
    assert!(!observer.held());
}
#[test]
fn mismatched_geometry_or_unimplemented_native_capability_refuses_before_input() {
    let server = Server::start();
    let mut observer = Observer::open(&server.display);
    let initial = observer.pointer.query_pointer().unwrap();
    for geometry in [true, false] {
        let rt = runtime();
        let cx = rt.request_cx_with_budget(Budget::INFINITE);
        let seat = Seat::default();
        let bounds = if geometry {
            InputBounds::new(DesktopPoint { x: 0, y: 0 }, 300, 240).unwrap()
        } else {
            observer.pointer.bounds()
        };
        let caps = if geometry {
            observer.pointer.capabilities()
        } else {
            observer.pointer.capabilities().with(Capability::Text)
        };
        let session = owner(&cx, bounds, caps, 3_000_000);
        let (agent, driver) = start_x11(
            &seat,
            cx,
            session,
            Route::new(7, ProtocolLimits::ABSOLUTE),
            &server.display,
        )
        .unwrap();
        let shutdown = Running::new(rt, driver).finish();
        assert!(shutdown.handoff_safe());
        assert!(agent.control().is_stopped());
        assert!(!observer.held());
        assert_eq!(observer.pointer.query_pointer().unwrap(), initial);
    }
}
#[test]
fn dropping_the_client_with_held_input_triggers_real_native_cleanup() {
    let server = Server::start();
    let mut observer = Observer::open(&server.display);
    let before = observer.repeats();
    let seat = Seat::default();
    let (mut agent, running) = launch(&server, &observer.pointer, &seat, 3_000_000);
    send(&mut agent, 0, press());
    send(&mut agent, 1, drag());
    drop(agent);
    assert!(running.finish().handoff_safe());
    assert!(!observer.held());
    assert!(!observer.dragging());
    assert_eq!(observer.repeats(), before);
}
// Only the private child created above is signalled; Drop resumes it before any
// observer can close its Xlib connection during a failing test's unwinding.
struct Paused(u32);
impl Paused {
    fn new(server: &Server) -> Self {
        let pid = server.child.id();
        assert!(
            Command::new("kill")
                .args(["-STOP", &pid.to_string()])
                .status()
                .unwrap()
                .success()
        );
        Self(pid)
    }
}
impl Drop for Paused {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .args(["-CONT", &self.0.to_string()])
            .status();
    }
}
#[test]
fn unresponsive_x_server_cannot_block_authority_expiry_or_free_the_controller_slot() {
    let server = Server::start();
    let mut observer = Observer::open(&server.display);
    let seat = Seat::default();
    let (mut agent, running) = launch(&server, &observer.pointer, &seat, 2_000_000);
    send(&mut agent, 0, press());
    send(&mut agent, 1, drag());
    let paused = Paused::new(&server);
    agent
        .submit(
            &bytes(
                10,
                InputEvent::Pointer {
                    position: DesktopPoint { x: 100, y: 120 },
                },
            ),
            InputDelivery::Reliable,
        )
        .unwrap();
    eventually(|| agent.control().is_stopped());
    assert!(seat.is_occupied());
    let shutdown = running.finish();
    assert!(!shutdown.handoff_safe());
    assert!(shutdown.exit.is_none());
    assert!(seat.is_occupied());
    drop(paused);
    eventually(|| agent.status().exit.is_some());
    assert!(agent.status().exit.unwrap().handoff_safe());
    assert!(!observer.held());
    assert!(!observer.dragging());
    assert_eq!(
        observer.pointer.query_pointer().unwrap().0,
        DesktopPoint { x: 20, y: 30 }
    );
    assert!(!seat.is_occupied());
}
