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
use fr_wire::input::{InputDelivery, InputDirection, MAX_INPUT_RECORD_BYTES, encode_input};
use frd::{
    input_agent::{
        Agent, AuthorityCommand, Driver, Error, InputReply, Reply, Route, Seat, Shutdown,
    },
    input_watchdog::{StopReason, host_now},
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

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
fn session(cx: &Cx, lease_us: u64) -> InputSession {
    let now = host_now(cx).unwrap();
    let c = credentials();
    let mut a = SessionAuthority::new(
        c.session,
        AuthorityPolicy {
            authorization_lifetime: HostDuration::from_micros(lease_us),
            ticket_lifetime: HostDuration::from_micros(lease_us / 2),
        },
    );
    a.mark_capabilities_checked().unwrap();
    a.authorize_observation(now).unwrap();
    a.mark_view_ready(now).unwrap();
    a.grant_lease(c.lease, now).unwrap();
    a.issue_input_ticket(c.lease, c.ticket, now).unwrap();
    InputSession::new(
        a,
        c,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
        Capabilities::default()
            .with(Capability::Keys)
            .with(Capability::Buttons)
            .with(Capability::Absolute)
            .with(Capability::Text),
        now,
    )
    .unwrap()
}
fn key(pressed: bool) -> InputEvent<'static> {
    InputEvent::Key {
        key: PhysicalKey::new(4).unwrap(),
        transition: if pressed {
            KeyTransition::Press
        } else {
            KeyTransition::Release
        },
    }
}
fn bytes(seq: u64, event: InputEvent<'_>) -> Vec<u8> {
    let mut b = vec![0; MAX_INPUT_RECORD_BYTES];
    let n = encode_input(
        InputRequest {
            credentials: credentials(),
            sequence: seq,
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
fn eventually(mut condition: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(3);
    while !condition() {
        assert!(Instant::now() < until, "native condition timed out");
        thread::sleep(Duration::from_millis(1));
    }
}
fn reply(agent: &mut Agent) -> Reply {
    let mut value = None;
    eventually(|| {
        value = agent.try_reply().unwrap();
        value.is_some()
    });
    value.unwrap()
}
struct Runner {
    done: mpsc::Receiver<Shutdown>,
    join: thread::JoinHandle<()>,
}
impl Runner {
    fn start(runtime: Runtime, driver: Driver) -> Self {
        let (tx, done) = mpsc::sync_channel(1);
        let join = thread::spawn(move || {
            let _ = tx.send(runtime.block_on(driver));
        });
        Self { done, join }
    }
    fn finish(self) -> Shutdown {
        let r = self.done.recv_timeout(Duration::from_secs(4)).unwrap();
        self.join.join().unwrap();
        r
    }
}
#[derive(Default)]
struct Trace {
    held: AtomicBool,
    effects: Mutex<Vec<Operation>>,
    clean: AtomicBool,
    drops: AtomicUsize,
}
struct Sink {
    trace: Arc<Trace>,
    block_prepare: Option<(mpsc::SyncSender<()>, mpsc::Receiver<()>)>,
    block_submit: Option<(mpsc::SyncSender<()>, mpsc::Receiver<()>)>,
    drop_block: Option<(mpsc::SyncSender<()>, mpsc::Receiver<()>)>,
    panic_on_second: bool,
}
impl Sink {
    fn new(t: Arc<Trace>) -> Self {
        Self {
            trace: t,
            block_prepare: None,
            block_submit: None,
            drop_block: None,
            panic_on_second: false,
        }
    }
}
impl InputSink for Sink {
    fn prepare(&mut self, _: Operation) -> Result<(), PlatformError> {
        if let Some((entered, resume)) = self.block_prepare.take() {
            entered.send(()).unwrap();
            resume.recv_timeout(Duration::from_secs(4)).unwrap();
        }
        Ok(())
    }
    fn submit(&mut self, op: Operation) -> Submission {
        if let Some((entered, resume)) = self.block_submit.take() {
            entered.send(()).unwrap();
            resume.recv_timeout(Duration::from_secs(4)).unwrap();
        }
        let mut e = self.trace.effects.lock().unwrap();
        assert!(
            !(self.panic_on_second && e.len() == 1),
            "injected native failure"
        );
        if let Operation::Key { transition, .. } = op {
            self.trace
                .held
                .store(transition != KeyTransition::Release, Ordering::Release);
        }
        e.push(op);
        Submission::Submitted
    }
}
impl Drop for Sink {
    fn drop(&mut self) {
        if let Some((entered, resume)) = self.drop_block.take() {
            entered.send(()).unwrap();
            resume.recv_timeout(Duration::from_secs(4)).unwrap();
        }
        self.trace.drops.fetch_add(1, Ordering::Release);
    }
}
fn clean(s: &mut Sink) -> bool {
    s.trace.clean.load(Ordering::Acquire)
}
fn trace() -> Arc<Trace> {
    Arc::new(Trace {
        clean: AtomicBool::new(true),
        ..Trace::default()
    })
}
fn route() -> Route {
    Route::new(7, ProtocolLimits::ABSOLUTE)
}

use fr_wire::input_result::{
    INPUT_RESULT_BYTES, InputResult, SequenceSpace, Stage, decode_input_result, encode_input_result,
};
fn record(reply: InputReply) -> InputResult {
    let InputReply::Record(result) = reply else {
        panic!("expected a retained input receipt: {reply:?}")
    };
    let mut b = [0; INPUT_RESULT_BYTES];
    let n = encode_input_result(
        result,
        &mut b,
        &ProtocolLimits::ABSOLUTE,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .unwrap();
    assert_eq!(n, INPUT_RESULT_BYTES);
    assert_eq!(
        decode_input_result(
            &b,
            &ProtocolLimits::ABSOLUTE,
            result.binding,
            InputDirection::HostToViewer,
            InputDelivery::Reliable
        )
        .unwrap(),
        result
    );
    assert_ne!(result.stage, Stage::Observed);
    result
}
fn input_reply(agent: &mut Agent) -> InputReply {
    let mut result = None;
    eventually(|| {
        result = agent.try_input_result().unwrap();
        result.is_some()
    });
    result.unwrap()
}
#[test]
fn wire_results_retain_original_binding_and_distinguish_pointer_sequence_space() {
    let rt = runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let t = trace();
    let native = t.clone();
    let seat = Seat::default();
    let (mut agent, driver) = seat
        .start(
            cx.clone(),
            session(&cx, 3_000_000),
            route(),
            move || Ok(Sink::new(native)),
            clean,
        )
        .unwrap();
    for (event, space) in [
        (key(true), SequenceSpace::Action),
        (
            InputEvent::Pointer {
                position: DesktopPoint { x: 12, y: 34 },
            },
            SequenceSpace::Pointer,
        ),
    ] {
        agent
            .submit(&bytes(0, event), InputDelivery::Reliable)
            .unwrap();
        assert_eq!(
            agent.authority(AuthorityCommand::Ticket(InputTicketId::from_raw(5))),
            Err(Error::Backpressure)
        );
        let value = record(rt.block_on(agent.input_response().unwrap()).unwrap());
        assert_eq!(value.binding.channel, 7);
        assert_eq!(value.binding.session, credentials().session);
        assert_eq!(value.binding.lease, credentials().lease);
        assert_eq!(value.space, space);
        assert_eq!(value.sequence, 0);
        assert_eq!(value.stage, Stage::SubmittedToOs);
        assert_eq!(value.submitted_operations, 1);
    }
    agent.control().stop(StopReason::LocalRevoke);
    assert!(rt.block_on(driver).handoff_safe());
}
#[test]
fn late_receipt_does_not_take_the_successor_controllers_binding() {
    let rt = runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let t = trace();
    let native = t.clone();
    let seat = Seat::default();
    let (mut old, driver) = seat
        .start(
            cx.clone(),
            session(&cx, 3_000_000),
            route(),
            move || Ok(Sink::new(native)),
            clean,
        )
        .unwrap();
    old.submit(&bytes(0, key(true)), InputDelivery::Reliable)
        .unwrap();
    eventually(|| t.held.load(Ordering::Acquire));
    old.control().stop(StopReason::LocalRevoke);
    assert!(Runner::start(rt, driver).finish().handoff_safe());
    let rt = runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let native = trace();
    let (next, driver) = seat
        .start(
            cx.clone(),
            session(&cx, 3_000_000),
            Route::new(8, ProtocolLimits::ABSOLUTE),
            move || Ok(Sink::new(native)),
            clean,
        )
        .unwrap();
    let value = record(input_reply(&mut old));
    assert_eq!(value.binding.channel, 7);
    assert_eq!(value.outcome, InputOutcome::SubmittedToOs);
    assert_eq!(value.submitted_operations, 1);
    assert!(!next.control().is_stopped());
    next.control().stop(StopReason::LocalRevoke);
    assert!(rt.block_on(driver).handoff_safe());
}
#[test]
fn selecting_input_projection_never_consumes_an_authority_reply() {
    let rt = runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let native = trace();
    let seat = Seat::default();
    let (mut agent, driver) = seat
        .start(
            cx.clone(),
            session(&cx, 3_000_000),
            route(),
            move || Ok(Sink::new(native)),
            clean,
        )
        .unwrap();
    agent
        .authority(AuthorityCommand::Ticket(InputTicketId::from_raw(5)))
        .unwrap();
    assert_eq!(agent.try_input_result(), Err(Error::NotInputCommand));
    assert!(matches!(
        agent.input_response(),
        Err(Error::NotInputCommand)
    ));
    assert!(matches!(reply(&mut agent), Reply::Authority(Ok(_))));
    assert_eq!(agent.try_input_result(), Err(Error::NoPendingCommand));
    agent.control().stop(StopReason::LocalRevoke);
    assert!(rt.block_on(driver).handoff_safe());
}
#[test]
fn dropping_projected_wait_revokes_but_retains_completed_result_and_context() {
    let rt = runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let t = trace();
    let native = t.clone();
    let seat = Seat::default();
    let (mut agent, driver) = seat
        .start(
            cx.clone(),
            session(&cx, 3_000_000),
            route(),
            move || Ok(Sink::new(native)),
            clean,
        )
        .unwrap();
    agent
        .submit(&bytes(0, key(true)), InputDelivery::Reliable)
        .unwrap();
    eventually(|| t.held.load(Ordering::Acquire));
    drop(agent.input_response().unwrap());
    assert!(agent.control().is_stopped());
    assert!(rt.block_on(driver).handoff_safe());
    let result = record(input_reply(&mut agent));
    assert_eq!(result.outcome, InputOutcome::SubmittedToOs);
    assert_eq!(result.binding.channel, 7);
    assert_eq!(t.effects.lock().unwrap().len(), 2);
}
#[test]
fn panic_receipt_preserves_confirmed_text_prefix_in_the_existing_wire_codec() {
    let rt = runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let t = trace();
    let native = t.clone();
    let seat = Seat::default();
    let (mut agent, driver) = seat
        .start(
            cx.clone(),
            session(&cx, 3_000_000),
            route(),
            move || {
                let mut s = Sink::new(native);
                s.panic_on_second = true;
                Ok(s)
            },
            clean,
        )
        .unwrap();
    agent
        .submit(&bytes(0, InputEvent::Text("ab")), InputDelivery::Reliable)
        .unwrap();
    let result = record(input_reply(&mut agent));
    assert_eq!(result.outcome, InputOutcome::EffectUnknown);
    assert_eq!(result.submitted_operations, 1);
    assert!(result.unknown_next_operation);
    assert_eq!(
        t.effects
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len(),
        1
    );
    assert!(rt.block_on(driver).handoff_safe());
}
#[test]
fn evicted_receipts_do_not_become_fabricated_zero_effect_wire_results() {
    let rt = runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let t = trace();
    let native = t.clone();
    let seat = Seat::default();
    let (mut agent, driver) = seat
        .start(
            cx.clone(),
            session(&cx, 5_000_000),
            route(),
            move || Ok(Sink::new(native)),
            clean,
        )
        .unwrap();
    for i in 0..100 {
        agent
            .submit(&bytes(i, key(i % 2 == 0)), InputDelivery::Reliable)
            .unwrap();
        let result = record(input_reply(&mut agent));
        assert_eq!(result.outcome, InputOutcome::SubmittedToOs);
    }
    agent
        .submit(&bytes(0, key(true)), InputDelivery::Reliable)
        .unwrap();
    assert_eq!(input_reply(&mut agent), InputReply::ConsumedWithoutReceipt);
    assert_eq!(t.effects.lock().unwrap().len(), 100);
    agent.control().stop(StopReason::LocalRevoke);
    assert!(rt.block_on(driver).handoff_safe());
}
