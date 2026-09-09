#![cfg(all(target_os = "linux", feature = "linux-input-agent"))]
use asupersync::{
    cx::Cx,
    runtime::{Runtime, RuntimeBuilder},
    types::{Budget, CancelKind},
};
use core::ffi::{c_char, c_int, c_uint, c_ulong, c_void};
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::*,
    input::*,
    input_sequence::InputOutcome,
    input_submission::*,
    limits::ProtocolLimits,
    time::{HostDuration, HostInstant},
};
use fr_native::{input::X11Pointer, input_agent::start_x11};
use fr_wire::input::{InputDelivery, InputDirection, MAX_INPUT_RECORD_BYTES, encode_input};
use fr_wire::input_result::{
    INPUT_RESULT_BYTES, InputResult, ResultBinding, Stage, decode_input_result, encode_input_result,
};
use frd::{
    input_agent::{Agent, Driver, InputReply, Route, Seat, Shutdown},
    input_watchdog::StopReason,
};
use std::{
    ffi::CString,
    io::{BufRead, BufReader, Read},
    process::{Child, Command, Stdio},
    sync::{Mutex, MutexGuard, Once, mpsc},
    thread,
    time::{Duration, Instant},
};
const WAIT: Duration = Duration::from_secs(2);
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
    fn XInitThreads() -> c_int;
    fn XOpenDisplay(name: *const c_char) -> *mut c_void;
    fn XCloseDisplay(d: *mut c_void) -> c_int;
    fn XKeysymToKeycode(d: *mut c_void, sym: c_ulong) -> u8;
    fn XQueryKeymap(d: *mut c_void, keys: *mut u8) -> c_int;
    fn XGetKeyboardControl(d: *mut c_void, state: *mut KeyboardState) -> c_int;
}
struct Server {
    child: Child,
    name: String,
    _scenario: MutexGuard<'static, ()>,
}
impl Server {
    fn start() -> Self {
        static INIT: Once = Once::new();
        // PausedPreparation deliberately retains the process-wide XTest lock
        // through authority expiry and the driver's drain timeout. These are
        // separate input-process scenarios, so acquire isolation BEFORE grants
        // start ticking and retain it through native/display cleanup. Otherwise
        // the fault injection expires an unrelated test's valid input ticket.
        static SCENARIO: Mutex<()> = Mutex::new(());
        let scenario = SCENARIO
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // SAFETY: once, before any Xlib use by this test executable. Each
        // observer and input backend still owns its own thread-local display.
        INIT.call_once(|| assert_ne!(unsafe { XInitThreads() }, 0));
        let mut child = Command::new("Xvfb")
            .args([
                "-displayfd",
                "1",
                "-screen",
                "0",
                "320x240x24",
                "-nolisten",
                "tcp",
                "-noreset",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("Xvfb required");
        let mut number = String::new();
        BufReader::new(child.stdout.take().unwrap().take(16))
            .read_line(&mut number)
            .unwrap();
        Self {
            child,
            name: format!(":{}", number.trim().parse::<u16>().unwrap()),
            _scenario: scenario,
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
struct Observer {
    d: *mut c_void,
    pointer: X11Pointer,
}
impl Observer {
    fn new(name: &str) -> Self {
        let pointer = X11Pointer::open(name).unwrap();
        let name = CString::new(name).unwrap();
        // SAFETY: valid string, own this connection until Drop on this thread.
        let d = unsafe { XOpenDisplay(name.as_ptr()) };
        assert!(!d.is_null());
        Self { d, pointer }
    }
    fn code(&self, sym: c_ulong) -> u8 {
        // SAFETY: live exclusively owned local display; scalar output.
        let code = unsafe { XKeysymToKeycode(self.d, sym) };
        assert_ne!(code, 0);
        code
    }
    fn down(&self, code: u8) -> bool {
        let mut keys = [0; 32];
        // SAFETY: fixed XQueryKeymap output buffer, live display.
        assert_ne!(unsafe { XQueryKeymap(self.d, keys.as_mut_ptr()) }, 0);
        keys[usize::from(code) / 8] & (1 << (code % 8)) != 0
    }
    fn repeat(&self, code: u8) -> bool {
        let mut state = KeyboardState {
            key_click_percent: 0,
            bell_percent: 0,
            bell_pitch: 0,
            bell_duration: 0,
            led_mask: 0,
            global_auto_repeat: 0,
            auto_repeats: [0; 32],
        };
        // SAFETY: exact XKeyboardState layout and owned display; no retention.
        assert_ne!(unsafe { XGetKeyboardControl(self.d, &raw mut state) }, 0);
        state.auto_repeats[usize::from(code) / 8] & (1 << (code % 8)) != 0
    }
}
impl Drop for Observer {
    fn drop(&mut self) {
        // SAFETY: observer connection has no outstanding borrowed references.
        unsafe {
            XCloseDisplay(self.d);
        }
    }
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
fn session(cx: &Cx, observer: &Observer, life_ms: u64) -> InputSession {
    // Synthetic local grant only: actual Asupersync and X11 execution does not
    // establish Tailscale admission, consent or physical-compositor support.
    let c = credentials();
    let now = HostInstant::from_micros(cx.timer_driver().unwrap().now().as_nanos() / 1000);
    let mut authority = SessionAuthority::new(
        c.session,
        AuthorityPolicy {
            authorization_lifetime: HostDuration::from_micros(life_ms * 1000),
            ticket_lifetime: HostDuration::from_micros(life_ms * 900),
        },
    );
    authority.mark_capabilities_checked().unwrap();
    authority.authorize_observation(now).unwrap();
    authority.mark_view_ready(now).unwrap();
    authority.grant_lease(c.lease, now).unwrap();
    authority
        .issue_input_ticket(c.lease, c.ticket, now)
        .unwrap();
    InputSession::new(
        authority,
        c,
        observer.pointer.bounds(),
        observer.pointer.capabilities(),
        now,
    )
    .unwrap()
}
fn wire(sequence: u64, event: InputEvent<'_>) -> Vec<u8> {
    let mut out = [0; MAX_INPUT_RECORD_BYTES];
    let n = encode_input(
        InputRequest {
            credentials: credentials(),
            sequence,
            event,
        },
        &mut out,
        &ProtocolLimits::ABSOLUTE,
        7,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .unwrap();
    out[..n].to_vec()
}
fn key(usage: u16) -> InputEvent<'static> {
    InputEvent::Key {
        key: PhysicalKey::new(usage).unwrap(),
        transition: KeyTransition::Press,
    }
}
fn drag() -> InputEvent<'static> {
    InputEvent::Button {
        button: PointerButton::Primary,
        pressed: true,
        position: DesktopPoint { x: 50, y: 60 },
        barrier: 0,
    }
}
fn receipt(reply: InputReply) -> InputResult {
    let InputReply::Record(result) = reply else {
        panic!("wire input receipt required")
    };
    let expected = ResultBinding {
        channel: 7,
        session: credentials().session,
        lease: credentials().lease,
    };
    let mut bytes = [0; INPUT_RESULT_BYTES];
    let n = encode_input_result(
        result,
        &mut bytes,
        &ProtocolLimits::ABSOLUTE,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .unwrap();
    let decoded = decode_input_result(
        &bytes[..n],
        &ProtocolLimits::ABSOLUTE,
        expected,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .unwrap();
    assert_eq!(decoded, result);
    assert_ne!(decoded.stage, Stage::Observed);
    decoded
}

fn eventually(mut condition: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(4);
    while !condition() {
        assert!(Instant::now() < until, "native condition timed out");
        thread::sleep(Duration::from_millis(1));
    }
}
fn result(agent: &mut Agent) -> InputResult {
    let mut result = None;
    eventually(|| {
        result = agent.try_input_result().unwrap();
        result.is_some()
    });
    receipt(result.unwrap())
}
fn send(agent: &mut Agent, sequence: u64, event: InputEvent<'_>) -> InputResult {
    agent
        .submit(&wire(sequence, event), InputDelivery::Reliable)
        .unwrap();
    result(agent)
}
struct Runner {
    done: mpsc::Receiver<Shutdown>,
    join: thread::JoinHandle<()>,
}
impl Runner {
    fn start(rt: Runtime, driver: Driver) -> Self {
        let (tx, done) = mpsc::sync_channel(1);
        let join = thread::spawn(move || {
            let _ = tx.send(rt.block_on(driver));
        });
        Self { done, join }
    }
    fn finish(self) -> Shutdown {
        let r = self.done.recv_timeout(Duration::from_secs(4)).unwrap();
        self.join.join().unwrap();
        r
    }
}
fn runtime() -> Runtime {
    RuntimeBuilder::new().worker_threads(1).build().unwrap()
}
fn route() -> Route {
    Route::new(7, ProtocolLimits::ABSOLUTE)
}

#[test]
fn runtime_idle_expiry_releases_real_keys_drag_and_restores_repeat() {
    let server = Server::start();
    let mut observer = Observer::new(&server.name);
    let a = observer.code(0x61);
    let shift = observer.code(0xffe1);
    let repeat = observer.repeat(a);
    let rt = runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let seat = Seat::default();
    let name = server.name.clone();
    let (mut agent, driver) = start_x11(
        &seat,
        cx.clone(),
        session(&cx, &observer, 700),
        route(),
        &name,
    )
    .unwrap();
    let runner = Runner::start(rt, driver);
    assert_eq!(
        send(&mut agent, 0, key(225)).outcome,
        InputOutcome::SubmittedToOs
    );
    assert_eq!(
        send(&mut agent, 1, key(4)).outcome,
        InputOutcome::SubmittedToOs
    );
    assert_eq!(send(&mut agent, 2, drag()).submitted_operations, 2);
    eventually(|| {
        observer.down(a)
            && observer.down(shift)
            && observer.pointer.query_pointer().unwrap().1 & 256 != 0
    });
    assert!(!observer.repeat(a));
    // No further input, heartbeat, cleanup request or stop: only the runtime
    // authority deadline causes this real native release/restoration sequence.
    let shutdown = runner.finish();
    assert_eq!(shutdown.reason, StopReason::AuthorityEnded);
    assert!(shutdown.handoff_safe());
    assert_eq!(
        shutdown.exit.unwrap().cleanup.unwrap().submitted_releases,
        3
    );
    assert!(!observer.down(a));
    assert!(!observer.down(shift));
    assert_eq!(observer.pointer.query_pointer().unwrap().1 & 256, 0);
    assert_eq!(observer.repeat(a), repeat);
    assert!(!seat.is_occupied());
}
#[test]
fn runtime_lifecycle_revocation_releases_real_drag_without_another_packet() {
    let server = Server::start();
    let mut observer = Observer::new(&server.name);
    let seat = Seat::default();
    for reason in [
        StopReason::ViewInvalidated,
        StopReason::LocalRevoke,
        StopReason::Suspended,
        StopReason::ClientDisconnected,
    ] {
        let rt = runtime();
        let cx = rt.request_cx_with_budget(Budget::INFINITE);
        let name = server.name.clone();
        let (mut agent, driver) = start_x11(
            &seat,
            cx.clone(),
            session(&cx, &observer, 2000),
            route(),
            &name,
        )
        .unwrap();
        let runner = Runner::start(rt, driver);
        assert_eq!(
            send(&mut agent, 0, drag()).outcome,
            InputOutcome::SubmittedToOs
        );
        eventually(|| observer.pointer.query_pointer().unwrap().1 & 256 != 0);
        agent.control().stop(reason);
        let shutdown = runner.finish();
        assert_eq!(shutdown.reason, reason);
        assert!(shutdown.handoff_safe());
        assert_eq!(observer.pointer.query_pointer().unwrap().1 & 256, 0);
        assert!(!seat.is_occupied());
    }
}
struct PausedPreparation {
    native: X11Pointer,
    entered: Option<mpsc::SyncSender<()>>,
    resume: mpsc::Receiver<()>,
}
impl InputSink for PausedPreparation {
    fn prepare(&mut self, op: Operation) -> Result<(), PlatformError> {
        self.native.prepare(op)?;
        if let Some(tx) = self.entered.take() {
            tx.send(()).unwrap();
            self.resume.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        Ok(())
    }
    fn submit(&mut self, op: Operation) -> Submission {
        self.native.submit(op)
    }
    fn cancel_prepared(&mut self) {
        self.native.cancel_prepared();
    }
    fn repeat_requires_pair(&self) -> bool {
        self.native.repeat_requires_pair()
    }
}
#[test]
fn blocked_xkb_preparation_expires_independently_without_certifying_cleanup() {
    let server = Server::start();
    let observer = Observer::new(&server.name);
    let a = observer.code(0x61);
    let repeat = observer.repeat(a);
    let rt = runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let seat = Seat::default();
    let name = server.name.clone();
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = mpsc::sync_channel(1);
    let (mut agent, driver) = seat
        .start(
            cx.clone(),
            session(&cx, &observer, 300),
            route(),
            move || {
                Ok(PausedPreparation {
                    native: X11Pointer::open(&name)?,
                    entered: Some(entered_tx),
                    resume: resume_rx,
                })
            },
            |s| s.native.cleanup_native(),
        )
        .unwrap();
    let runner = Runner::start(rt, driver);
    agent
        .submit(&wire(0, key(4)), InputDelivery::Reliable)
        .unwrap();
    entered_rx.recv_timeout(WAIT).unwrap();
    assert!(!observer.down(a));
    assert!(!observer.repeat(a));
    let shutdown = runner.finish();
    assert_eq!(shutdown.reason, StopReason::AuthorityEnded);
    assert!(!shutdown.handoff_safe());
    assert!(shutdown.exit.is_none());
    assert!(seat.is_occupied());
    assert!(!observer.down(a));
    resume_tx.send(()).unwrap();
    assert_eq!(
        result(&mut agent).outcome,
        InputOutcome::CancelledBeforeSubmission
    );
    eventually(|| agent.status().exit.is_some());
    assert!(agent.status().exit.unwrap().handoff_safe());
    assert!(!seat.is_occupied());
    assert!(!observer.down(a));
    assert_eq!(observer.repeat(a), repeat);
}
struct CancelPreparation {
    native: X11Pointer,
    cx: Cx,
}
impl InputSink for CancelPreparation {
    fn prepare(&mut self, op: Operation) -> Result<(), PlatformError> {
        self.native.prepare(op)?;
        self.cx.cancel_fast(CancelKind::User);
        Ok(())
    }
    fn submit(&mut self, op: Operation) -> Submission {
        self.native.submit(op)
    }
    fn cancel_prepared(&mut self) {
        self.native.cancel_prepared();
    }
    fn repeat_requires_pair(&self) -> bool {
        self.native.repeat_requires_pair()
    }
}
#[test]
fn cancellation_during_real_xkb_preparation_never_submits_or_strands_repeat_state() {
    let server = Server::start();
    let observer = Observer::new(&server.name);
    let a = observer.code(0x61);
    let repeat = observer.repeat(a);
    let rt = runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let seat = Seat::default();
    let name = server.name.clone();
    let native_cx = cx.clone();
    let (mut agent, driver) = seat
        .start(
            cx.clone(),
            session(&cx, &observer, 2000),
            route(),
            move || {
                Ok(CancelPreparation {
                    native: X11Pointer::open(&name)?,
                    cx: native_cx,
                })
            },
            |s| s.native.cleanup_native(),
        )
        .unwrap();
    // Deliberately defer polling Driver to expose cancellation during native
    // preparation rather than relying on the watchdog winning a scheduler race.
    let result = send(&mut agent, 0, key(4));
    assert_eq!(result.outcome, InputOutcome::CancelledBeforeSubmission);
    assert_eq!(result.submitted_operations, 0);
    let shutdown = Runner::start(rt, driver).finish();
    assert!(shutdown.handoff_safe());
    assert_eq!(shutdown.reason, StopReason::Cancelled);
    assert!(!observer.down(a));
    assert_eq!(observer.repeat(a), repeat);
    assert!(!seat.is_occupied());
}
