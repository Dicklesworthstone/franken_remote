//! The real frd side (spawn, socketpairs, fence, timeouts, poisoning, Seat
//! custody) against a Python PROTOCOL fixture child. The fixture performs no
//! native input; real X11 effects are qualified by fr-native's Xvfb test.
use super::*;
use crate::{
    input_agent::{Agent, Driver, Reply as AgentReply, Route, Seat, Shutdown},
    input_watchdog::{StopReason, host_now},
};
use asupersync::{
    runtime::{Runtime, RuntimeBuilder},
    types::Budget,
};
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::*,
    input::*,
    input_sequence::InputOutcome,
    input_submission::{Capability, Dispatch, InputSession, Receipt},
    limits::ProtocolLimits,
    time::HostDuration,
};
use fr_wire::input::{InputDelivery, InputDirection, MAX_INPUT_RECORD_BYTES, encode_input};
use std::{
    os::unix::fs::PermissionsExt,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
};

/// Write the fixture with its mode and a fresh log path. Returns both paths.
pub(crate) fn fixture(mode: &str) -> (PathBuf, PathBuf) {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let base = std::env::temp_dir().join(format!(
        "fr-input-fixture-{}-{n}-{mode}",
        std::process::id()
    ));
    let image = base.with_extension("py");
    let log = base.with_extension("log");
    let script = include_str!("agent_fixture.py")
        .replace("@MODE@", mode)
        .replace("@LOG@", log.to_str().unwrap());
    std::fs::write(&image, script).unwrap();
    std::fs::set_permissions(&image, std::fs::Permissions::from_mode(0o700)).unwrap();
    (image, log)
}
pub(crate) fn transcript(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}
fn eventually(mut condition: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(Instant::now() < until, "fixture condition timed out");
        thread::sleep(Duration::from_millis(1));
    }
}
fn runtime() -> Runtime {
    RuntimeBuilder::new().worker_threads(1).build().unwrap()
}
fn launch(image: &Path) -> ProcessLaunch {
    ProcessLaunch::new(image, ":0", None, 0x1234_5678_9abc).unwrap()
}
fn caps() -> Capabilities {
    Capabilities::default()
        .with(Capability::Keys)
        .with(Capability::Absolute)
        .with(Capability::Buttons)
}
fn bounds() -> InputBounds {
    InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap()
}
fn key(transition: KeyTransition) -> Operation {
    Operation::Key {
        key: PhysicalKey::new(4).unwrap(),
        transition,
    }
}
fn later(cx: &Cx, micros: u64) -> HostInstant {
    host_now(cx)
        .unwrap()
        .checked_add(HostDuration::from_micros(micros))
        .unwrap()
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
fn session(cx: &Cx) -> InputSession {
    let now = host_now(cx).unwrap();
    let c = credentials();
    let mut a = SessionAuthority::new(c.session, AuthorityPolicy::plan_defaults());
    a.mark_capabilities_checked().unwrap();
    a.authorize_observation(now).unwrap();
    a.mark_view_ready(now).unwrap();
    a.grant_lease(c.lease, now).unwrap();
    a.issue_input_ticket(c.lease, c.ticket, now).unwrap();
    InputSession::new(a, c, bounds(), caps(), now).unwrap()
}
fn bytes(sequence: u64, event: InputEvent<'_>) -> Vec<u8> {
    let mut b = vec![0; MAX_INPUT_RECORD_BYTES];
    let n = encode_input(
        InputRequest {
            credentials: credentials(),
            sequence,
            event,
        },
        &mut b,
        &ProtocolLimits::ABSOLUTE,
        7,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .unwrap();
    b.truncate(n);
    b
}
fn press() -> InputEvent<'static> {
    InputEvent::Key {
        key: PhysicalKey::new(4).unwrap(),
        transition: KeyTransition::Press,
    }
}
fn receipt(agent: &mut Agent) -> Receipt {
    let mut reply = None;
    eventually(|| {
        reply = agent.try_reply().unwrap();
        reply.is_some()
    });
    match reply.unwrap() {
        AgentReply::Input(Ok(Dispatch::Completed(r))) => r,
        other => panic!("receipt required: {other:?}"),
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
        let shutdown = self.done.recv_timeout(Duration::from_secs(6)).unwrap();
        self.join.join().unwrap();
        shutdown
    }
}
/// Start the canonical owner exactly as production composes it: the lease's
/// fence installed on its Control before any input can be queued.
fn start(mode: &str, seat: &Seat) -> (Agent, Running, PathBuf) {
    let (image, log) = fixture(mode);
    let rt = runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let owner = session(&cx);
    let fence = Fence::default();
    let make = factory(
        launch(&image),
        cx.clone(),
        owner.bounds(),
        owner.capabilities(),
        fence.clone(),
    );
    let (agent, driver) = seat
        .start(
            cx,
            owner,
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
    (agent, Running::new(rt, driver), log)
}

#[test]
fn launch_is_private_local_and_never_accepts_a_remote_display() {
    let image = Path::new("/usr/bin/true");
    for display in ["", ":", "host:0", ":0.1.2", ":99999", ":0\0", "localhost:0"] {
        assert_eq!(
            ProcessLaunch::new(image, display, None, 1).err(),
            Some(InvalidLaunch),
            "{display:?}"
        );
    }
    assert!(ProcessLaunch::new(Path::new("relative"), ":0", None, 1).is_err());
    assert!(ProcessLaunch::new(image, ":0", None, 0).is_err());
    assert!(ProcessLaunch::new(image, ":0", Some(Path::new("xauth")), 1).is_err());
    let launch = ProcessLaunch::new(image, ":0.1", Some(Path::new("/x/auth")), 1).unwrap();
    assert_eq!(format!("{launch:?}"), "ProcessLaunch");
}

#[test]
fn child_gets_only_the_private_channels_display_and_parent_binding() {
    let (image, log) = fixture("normal");
    let rt = runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let sink = factory(launch(&image), cx, bounds(), caps(), Fence::default())().unwrap();
    assert!(sink.capabilities().contains_all(caps()));
    drop(sink);
    let lines = transcript(&log);
    // The fixture exits with BADARGS unless argv is exactly --parent-pid <frd>.
    let start = lines.iter().find(|l| l.starts_with("START ")).unwrap();
    let env = start.rsplit(' ').next().unwrap();
    // DISPLAY only (Python's own C-locale coercion may add LC_CTYPE).
    assert!(
        env.split(',').all(|k| k == "DISPLAY" || k == "LC_CTYPE"),
        "{env}"
    );
    assert!(env.split(',').any(|k| k == "DISPLAY"));
    assert_eq!(lines[1..], ["HELLO", "STOP"]);
}

#[test]
fn a_fence_before_spawn_refuses_the_launch_and_a_refused_hello_is_typed() {
    let (image, log) = fixture("normal");
    let rt = runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let fence = Fence::default();
    fence.signal();
    assert_eq!(
        factory(launch(&image), cx.clone(), bounds(), caps(), fence)().err(),
        Some(PlatformError::Permission)
    );
    assert_eq!(transcript(&log), [] as [String; 0], "nothing was spawned");
    let missing = Path::new("/nonexistent/fr-input-agent");
    assert_eq!(
        factory(
            ProcessLaunch::new(missing, ":0", None, 1).unwrap(),
            cx,
            bounds(),
            caps(),
            Fence::default()
        )()
        .err(),
        Some(PlatformError::Unsupported)
    );
}

#[test]
fn fence_reaches_the_child_during_an_in_flight_submit() {
    let (image, log) = fixture("fence-wait");
    let rt = runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let fence = Fence::default();
    let mut sink = factory(launch(&image), cx.clone(), bounds(), caps(), fence.clone())().unwrap();
    let op = key(KeyTransition::Press);
    sink.prepare(op).unwrap();
    let watcher = {
        let log = log.clone();
        thread::spawn(move || {
            eventually(|| transcript(&log).iter().any(|l| l.starts_with("WAITING")));
            // Only the separate datagram channel can reach the busy child.
            fence.signal();
        })
    };
    assert_eq!(
        sink.submit_until(op, later(&cx, 2_000_000)),
        Submission::Fenced
    );
    watcher.join().unwrap();
    // Later presses are refused without a native call or another request.
    let before = transcript(&log).len();
    sink.prepare(op).unwrap();
    assert_eq!(
        sink.submit_until(op, later(&cx, 2_000_000)),
        Submission::Fenced
    );
    assert_eq!(transcript(&log).len(), before);
    assert!(!sink.native_failed());
    drop(sink);
    let lines = transcript(&log);
    assert!(lines.iter().all(|l| !l.starts_with("EFFECT")), "{lines:?}");
    let fenced = lines.iter().position(|l| l == "FENCE").unwrap();
    assert!(lines[fenced + 1].starts_with("SUBMIT-FENCED"), "{lines:?}");
}

#[test]
fn press_is_refused_after_a_local_revoke_while_releases_still_reach_the_child() {
    let (image, log) = fixture("local-revoke");
    let rt = runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let mut sink = factory(
        launch(&image),
        cx.clone(),
        bounds(),
        caps(),
        Fence::default(),
    )()
    .unwrap();
    let press = key(KeyTransition::Press);
    sink.prepare(press).unwrap();
    assert_eq!(
        sink.submit_until(press, later(&cx, 2_000_000)),
        Submission::Submitted
    );
    // Before frd has even read the revoke, the child refuses a new press at
    // preparation; it is reported as Fenced (revoked), never as submitted.
    let button = Operation::Button {
        button: PointerButton::Primary,
        pressed: true,
    };
    sink.prepare(button).unwrap();
    assert_eq!(
        sink.submit_until(button, later(&cx, 2_000_000)),
        Submission::Fenced
    );
    // The child's indicator revoke is then reported locally, not as a failure.
    eventually(|| sink.locally_revoked());
    assert!(!sink.native_failed());
    // Release-only cleanup of the key it already holds is still accepted,
    // without a deadline, and only for release transitions.
    let release = key(KeyTransition::Release);
    assert_eq!(
        sink.submit(press),
        Submission::NotSubmitted(PlatformError::Unsupported)
    );
    sink.prepare(release).unwrap();
    assert_eq!(sink.submit(release), Submission::Submitted);
    assert!(sink.native_cleanup());
    drop(sink);
    let lines = transcript(&log);
    let effects: Vec<_> = lines
        .iter()
        .filter(|l| l.contains("EFFECT"))
        .map(String::as_str)
        .collect();
    assert_eq!(effects, ["EFFECT KEY", "CLEANUP-EFFECT KEY RELEASE"]);
    assert!(lines.iter().any(|l| l == "PREPARE-FENCED BUTTON"));
    assert_eq!(lines.last().map(String::as_str), Some("STOP"));
}

#[test]
fn executor_side_expiry_is_reported_without_any_native_call() {
    let (image, log) = fixture("slow");
    let rt = runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let mut sink = factory(
        launch(&image),
        cx.clone(),
        bounds(),
        caps(),
        Fence::default(),
    )()
    .unwrap();
    let op = key(KeyTransition::Press);
    // Already past: nothing is sent, and the preparation is cancelled.
    sink.prepare(op).unwrap();
    let past = host_now(&cx).unwrap();
    assert_eq!(sink.submit_until(op, past), Submission::Expired);
    sink.cancel_prepared();
    // Still 20ms in the future when sent; the child, descheduled for 50ms
    // before its own final check, refuses it with no native call.
    sink.prepare(op).unwrap();
    assert_eq!(
        sink.submit_until(op, later(&cx, 20_000)),
        Submission::Expired
    );
    drop(sink);
    let lines = transcript(&log);
    assert!(lines.iter().all(|l| !l.starts_with("EFFECT")), "{lines:?}");
    assert!(lines.iter().any(|l| l == "CANCEL"));
    assert!(lines.iter().any(|l| l == "SUBMIT-EXPIRED KEY"));
}

#[test]
fn control_stop_fences_an_in_flight_submit_and_hands_off_after_cleanup() {
    let seat = Seat::default();
    let (mut agent, running, log) = start("fence-wait", &seat);
    agent
        .submit(&bytes(0, press()), InputDelivery::Reliable)
        .unwrap();
    eventually(|| transcript(&log).iter().any(|l| l.starts_with("WAITING")));
    agent.control().stop(StopReason::LocalRevoke);
    let r = receipt(&mut agent);
    assert_eq!(r.outcome, InputOutcome::CancelledBeforeSubmission);
    assert_eq!(r.submitted_operations, 0);
    let shutdown = running.finish();
    assert_eq!(shutdown.reason, StopReason::LocalRevoke);
    assert!(shutdown.handoff_safe(), "{shutdown:?}");
    assert!(!seat.is_occupied());
    let lines = transcript(&log);
    assert!(lines.iter().all(|l| !l.starts_with("EFFECT")), "{lines:?}");
    assert!(lines.iter().any(|l| l == "CLEANUP"));
}

#[test]
fn child_indicator_revoke_stops_control_and_releases_through_the_child() {
    let seat = Seat::default();
    let (mut agent, running, log) = start("local-revoke", &seat);
    agent
        .submit(&bytes(0, press()), InputDelivery::Reliable)
        .unwrap();
    assert_eq!(receipt(&mut agent).outcome, InputOutcome::SubmittedToOs);
    let shutdown = running.finish();
    assert_eq!(shutdown.reason, StopReason::LocalRevoke);
    assert!(shutdown.handoff_safe(), "{shutdown:?}");
    assert!(!seat.is_occupied());
    let lines = transcript(&log);
    let effects: Vec<_> = lines
        .iter()
        .filter(|l| l.contains("EFFECT"))
        .map(String::as_str)
        .collect();
    assert_eq!(effects, ["EFFECT KEY", "CLEANUP-EFFECT KEY RELEASE"]);
}

#[test]
fn unanswered_submit_is_unknown_kills_the_child_and_retains_the_seat() {
    let seat = Seat::default();
    let (mut agent, running, log) = start("hang", &seat);
    agent
        .submit(&bytes(0, press()), InputDelivery::Reliable)
        .unwrap();
    let r = receipt(&mut agent);
    assert_eq!(r.outcome, InputOutcome::EffectUnknown);
    let shutdown = running.finish();
    assert_eq!(shutdown.reason, StopReason::NativeFailure);
    assert!(!shutdown.handoff_safe(), "{shutdown:?}");
    assert!(seat.is_occupied(), "uncertain press released the Seat");
    // Nothing was resent to a replacement: one hung submission, then death.
    let lines = transcript(&log);
    assert_eq!(
        lines.iter().filter(|l| l.starts_with("HANG")).count(),
        1,
        "{lines:?}"
    );
    assert!(lines.iter().all(|l| !l.contains("RELEASE") && l != "STOP"));
    // The hung child was killed at the timeout, not left running.
    let pid = child_pid(&lines);
    drop(agent);
    // Abandonment destroys the sink (child reaped) but can never certify the
    // uncertain press: the Seat stays occupied.
    eventually(|| !Path::new(&format!("/proc/{pid}")).exists());
    thread::sleep(Duration::from_millis(50));
    assert!(seat.is_occupied());
}
fn child_pid(lines: &[String]) -> u32 {
    lines
        .iter()
        .find_map(|l| l.strip_prefix("START "))
        .and_then(|rest| rest.split(' ').next())
        .unwrap()
        .parse()
        .unwrap()
}

#[test]
fn a_crashed_child_is_a_native_failure_with_uncertain_cleanup() {
    let seat = Seat::default();
    let (mut agent, running, log) = start("die-after-effect", &seat);
    agent
        .submit(&bytes(0, press()), InputDelivery::Reliable)
        .unwrap();
    assert_eq!(receipt(&mut agent).outcome, InputOutcome::SubmittedToOs);
    let shutdown = running.finish();
    assert_eq!(shutdown.reason, StopReason::NativeFailure);
    assert!(!shutdown.handoff_safe());
    assert!(seat.is_occupied());
    assert!(transcript(&log).iter().any(|l| l == "DIE"));
}
