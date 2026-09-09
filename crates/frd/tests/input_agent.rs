use asupersync::{
    cx::Cx,
    runtime::{Runtime, RuntimeBuilder},
    time::{TimerDriverHandle, VirtualClock},
    types::{Budget, Time},
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
    input_agent::{Agent, AuthorityCommand, Driver, Error, Reply, Route, Seat, Shutdown},
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
fn submitted(value: Reply) -> Receipt {
    let Reply::Input(Ok(Dispatch::Completed(r))) = value else {
        panic!("expected submission receipt: {value:?}")
    };
    r
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

#[test]
fn idle_expiry_releases_without_another_record_then_allows_new_controller() {
    let rt = runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let trace_state = trace();
    let factory = trace_state.clone();
    let seat = Seat::default();
    let (mut a, d) = seat
        .start(
            cx.clone(),
            session(&cx, 200_000),
            route(),
            move || Ok(Sink::new(factory)),
            clean,
        )
        .unwrap();
    let runner = Runner::start(rt, d);
    a.submit(&bytes(0, key(true)), InputDelivery::Reliable)
        .unwrap();
    assert_eq!(
        submitted(reply(&mut a)).outcome,
        InputOutcome::SubmittedToOs
    );
    assert!(trace_state.held.load(Ordering::Acquire));
    let s = runner.finish();
    assert!(s.handoff_safe());
    assert_eq!(s.reason, StopReason::AuthorityEnded);
    assert!(!trace_state.held.load(Ordering::Acquire));
    assert_eq!(trace_state.drops.load(Ordering::Acquire), 1);
    assert!(!seat.is_occupied());
    let rt = runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let other = trace();
    let (b, d) = seat
        .start(
            cx.clone(),
            session(&cx, 3_000_000),
            route(),
            move || Ok(Sink::new(other)),
            clean,
        )
        .unwrap();
    a.control().stop(StopReason::LocalRevoke);
    assert!(!b.control().is_stopped());
    b.control().stop(StopReason::LocalRevoke);
    assert!(Runner::start(rt, d).finish().handoff_safe());
}
#[test]
fn one_outstanding_includes_an_uncollected_reply_and_old_actions_never_retry() {
    let r = runtime();
    let cx = r.request_cx_with_budget(Budget::INFINITE);
    let trace_state = trace();
    let factory = trace_state.clone();
    let seat = Seat::default();
    let (mut a, d) = seat
        .start(
            cx.clone(),
            session(&cx, 3_000_000),
            route(),
            move || Ok(Sink::new(factory)),
            clean,
        )
        .unwrap();
    let runner = Runner::start(r, d);
    a.submit(&bytes(0, key(true)), InputDelivery::Reliable)
        .unwrap();
    eventually(|| trace_state.held.load(Ordering::Acquire));
    assert_eq!(
        a.submit(&bytes(1, key(false)), InputDelivery::Reliable),
        Err(Error::Backpressure)
    );
    let first = submitted(reply(&mut a));
    a.submit(&bytes(0, key(true)), InputDelivery::Reliable)
        .unwrap();
    assert_eq!(submitted(reply(&mut a)), first);
    assert_eq!(trace_state.effects.lock().unwrap().len(), 1);
    a.control().stop(StopReason::ClientDisconnected);
    assert!(runner.finish().handoff_safe());
}
#[test]
fn blocked_native_preparation_does_not_retain_authority_or_allow_handoff() {
    let clock = Arc::new(VirtualClock::new());
    let timer = TimerDriverHandle::with_virtual_clock(clock.clone());
    let r = RuntimeBuilder::new()
        .worker_threads(1)
        .with_timer_driver(timer.clone())
        .build()
        .unwrap();
    let cx = r.request_cx_with_budget(Budget::INFINITE);
    let trace_state = trace();
    let factory = trace_state.clone();
    let seat = Seat::default();
    let (tx, rx) = mpsc::sync_channel(1);
    let (resume, wait) = mpsc::sync_channel(1);
    let (mut a, d) = seat
        .start(
            cx.clone(),
            session(&cx, 100_000),
            route(),
            move || {
                let mut s = Sink::new(factory);
                s.block_prepare = Some((tx, wait));
                Ok(s)
            },
            clean,
        )
        .unwrap();
    let runner = Runner::start(r, d);
    a.submit(&bytes(0, key(true)), InputDelivery::Reliable)
        .unwrap();
    rx.recv_timeout(Duration::from_secs(2)).unwrap();
    // Native preparation must enter while its original 50 ms ticket is live.
    // Expire the 100 ms authority only after that synchronization point, so
    // runner scheduling cannot turn this into a pre-admission refusal test.
    assert!(!a.control().is_stopped());
    clock.advance_to(Time::from_millis(100));
    let _ = timer.process_timers();
    eventually(|| a.control().is_stopped());
    assert_eq!(a.control().reason(), Some(StopReason::AuthorityEnded));
    assert!(seat.is_occupied());
    let other = runtime();
    let other_cx = other.request_cx_with_budget(Budget::INFINITE);
    assert!(matches!(
        seat.start(
            other_cx.clone(),
            session(&other_cx, 3_000_000),
            route(),
            move || Ok(Sink::new(trace())),
            clean
        ),
        Err(Error::SeatBusy)
    ));
    resume.send(()).unwrap();
    assert_eq!(
        submitted(reply(&mut a)).outcome,
        InputOutcome::CancelledBeforeSubmission
    );
    assert!(runner.finish().handoff_safe());
    assert!(trace_state.effects.lock().unwrap().is_empty());
}
#[test]
fn blocked_entered_native_call_times_out_honestly_and_later_retains_its_receipt() {
    let r = runtime();
    let cx = r.request_cx_with_budget(Budget::INFINITE);
    let trace_state = trace();
    let factory = trace_state.clone();
    let seat = Seat::default();
    let (tx, rx) = mpsc::sync_channel(1);
    let (resume, wait) = mpsc::sync_channel(1);
    let (mut a, d) = seat
        .start(
            cx.clone(),
            session(&cx, 3_000_000),
            route(),
            move || {
                let mut s = Sink::new(factory);
                s.block_submit = Some((tx, wait));
                Ok(s)
            },
            clean,
        )
        .unwrap();
    let runner = Runner::start(r, d);
    a.submit(&bytes(0, key(true)), InputDelivery::Reliable)
        .unwrap();
    rx.recv_timeout(Duration::from_secs(2)).unwrap();
    a.control().stop(StopReason::LocalRevoke);
    let s = runner.finish();
    assert!(!s.handoff_safe());
    assert!(s.exit.is_none());
    assert!(seat.is_occupied());
    resume.send(()).unwrap();
    let receipt = submitted(reply(&mut a));
    assert_eq!(receipt.outcome, InputOutcome::SubmittedToOs);
    assert_eq!(receipt.submitted_operations, 1);
    eventually(|| !seat.is_occupied());
    assert!(!trace_state.held.load(Ordering::Acquire));
    assert_eq!(trace_state.effects.lock().unwrap().len(), 2);
}
#[test]
fn native_cleanup_failure_quarantines_until_explicit_local_retry() {
    let r = runtime();
    let cx = r.request_cx_with_budget(Budget::INFINITE);
    let trace_state = trace();
    trace_state.clean.store(false, Ordering::Release);
    let factory = trace_state.clone();
    let seat = Seat::default();
    let (mut a, d) = seat
        .start(
            cx.clone(),
            session(&cx, 3_000_000),
            route(),
            move || Ok(Sink::new(factory)),
            clean,
        )
        .unwrap();
    let runner = Runner::start(r, d);
    a.submit(&bytes(0, key(true)), InputDelivery::Reliable)
        .unwrap();
    let _ = reply(&mut a);
    a.control().stop(StopReason::ViewInvalidated);
    eventually(|| a.status().cleanup.is_some());
    assert_eq!(a.status().cleanup.unwrap().remaining, 0);
    assert!(!a.status().native_clean);
    assert!(seat.is_occupied());
    let s = runner.finish();
    assert!(!s.handoff_safe());
    assert!(s.exit.is_none());
    trace_state.clean.store(true, Ordering::Release);
    a.retry_cleanup().unwrap();
    eventually(|| !seat.is_occupied());
    assert!(a.status().exit.unwrap().handoff_safe());
}
#[test]
fn controller_reservation_survives_blocking_native_destructor() {
    let r = runtime();
    let cx = r.request_cx_with_budget(Budget::INFINITE);
    let trace_state = trace();
    let factory = trace_state.clone();
    let seat = Seat::default();
    let (tx, rx) = mpsc::sync_channel(1);
    let (resume, wait) = mpsc::sync_channel(1);
    let (a, d) = seat
        .start(
            cx.clone(),
            session(&cx, 3_000_000),
            route(),
            move || {
                let mut s = Sink::new(factory);
                s.drop_block = Some((tx, wait));
                Ok(s)
            },
            clean,
        )
        .unwrap();
    let runner = Runner::start(r, d);
    a.control().stop(StopReason::Suspended);
    rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(seat.is_occupied());
    assert!(a.status().exit.is_none());
    resume.send(()).unwrap();
    assert!(runner.finish().handoff_safe());
    assert_eq!(trace_state.drops.load(Ordering::Acquire), 1);
}
#[test]
fn dropping_an_unpolled_response_fences_but_does_not_erase_a_completed_receipt() {
    let r = runtime();
    let cx = r.request_cx_with_budget(Budget::INFINITE);
    let trace_state = trace();
    let factory = trace_state.clone();
    let seat = Seat::default();
    let (mut a, d) = seat
        .start(
            cx.clone(),
            session(&cx, 3_000_000),
            route(),
            move || Ok(Sink::new(factory)),
            clean,
        )
        .unwrap();
    let runner = Runner::start(r, d);
    a.submit(&bytes(0, key(true)), InputDelivery::Reliable)
        .unwrap();
    eventually(|| trace_state.held.load(Ordering::Acquire));
    drop(a.response());
    assert!(a.control().is_stopped());
    assert_eq!(
        submitted(reply(&mut a)).outcome,
        InputOutcome::SubmittedToOs
    );
    assert!(runner.finish().handoff_safe());
}
#[test]
fn native_panic_preserves_a_text_prefix_instead_of_fabricating_rollback() {
    let r = runtime();
    let cx = r.request_cx_with_budget(Budget::INFINITE);
    let trace_state = trace();
    let factory = trace_state.clone();
    let seat = Seat::default();
    let (mut a, d) = seat
        .start(
            cx.clone(),
            session(&cx, 3_000_000),
            route(),
            move || {
                let mut s = Sink::new(factory);
                s.panic_on_second = true;
                Ok(s)
            },
            clean,
        )
        .unwrap();
    let runner = Runner::start(r, d);
    a.submit(&bytes(0, InputEvent::Text("ab")), InputDelivery::Reliable)
        .unwrap();
    let Reply::NativePanic { receipt: Some(r) } = reply(&mut a) else {
        panic!("retained panic receipt required")
    };
    assert_eq!(r.outcome, InputOutcome::EffectUnknown);
    assert_eq!(r.submitted_operations, 1);
    assert_eq!(
        trace_state
            .effects
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len(),
        1
    );
    assert!(a.control().is_stopped());
    assert!(runner.finish().handoff_safe());
}
#[test]
fn malformed_oversized_and_wrong_channel_records_do_not_consume_the_slot() {
    let r = runtime();
    let cx = r.request_cx_with_budget(Budget::INFINITE);
    let seat = Seat::default();
    let (mut a, d) = seat
        .start(
            cx.clone(),
            session(&cx, 3_000_000),
            route(),
            move || Ok(Sink::new(trace())),
            clean,
        )
        .unwrap();
    let runner = Runner::start(r, d);
    assert!(matches!(
        a.submit(&[0; 16], InputDelivery::Reliable),
        Err(Error::Wire(_))
    ));
    assert_eq!(
        a.submit(
            &vec![0; MAX_INPUT_RECORD_BYTES + 1],
            InputDelivery::Reliable
        ),
        Err(Error::RecordTooLarge)
    );
    assert!(matches!(
        a.submit(&bytes(0, key(true)), InputDelivery::Datagram),
        Err(Error::Wire(_))
    ));
    assert!(!a.status().outstanding);
    a.submit(&bytes(0, key(true)), InputDelivery::Reliable)
        .unwrap();
    let _ = reply(&mut a);
    a.control().stop(StopReason::LocalRevoke);
    assert!(runner.finish().handoff_safe());
}
#[test]
fn local_authority_commands_use_the_session_clock_without_implicit_traffic_renewal() {
    let r = runtime();
    let cx = r.request_cx_with_budget(Budget::INFINITE);
    let seat = Seat::default();
    let (mut a, d) = seat
        .start(
            cx.clone(),
            session(&cx, 3_000_000),
            route(),
            move || Ok(Sink::new(trace())),
            clean,
        )
        .unwrap();
    let runner = Runner::start(r, d);
    a.authority(AuthorityCommand::ControlChallenge(17)).unwrap();
    let Reply::Authority(Ok(until)) = reply(&mut a) else {
        panic!("expected challenge")
    };
    a.authority(AuthorityCommand::ControlResponse(17)).unwrap();
    assert_eq!(reply(&mut a), Reply::Authority(Ok(until)));
    a.control().stop(StopReason::LocalRevoke);
    assert_eq!(
        a.authority(AuthorityCommand::Ticket(InputTicketId::from_raw(4))),
        Err(Error::Stopped)
    );
    assert!(runner.finish().handoff_safe());
}
#[test]
fn driver_drop_before_first_poll_stops_the_native_owner_and_cleans_up() {
    let r = runtime();
    let cx = r.request_cx_with_budget(Budget::INFINITE);
    let seat = Seat::default();
    let trace_state = trace();
    let factory = trace_state.clone();
    let (a, d) = seat
        .start(
            cx.clone(),
            session(&cx, 3_000_000),
            route(),
            move || Ok(Sink::new(factory)),
            clean,
        )
        .unwrap();
    drop(d);
    assert!(a.control().is_stopped());
    eventually(|| !seat.is_occupied());
    assert_eq!(trace_state.drops.load(Ordering::Acquire), 1);
}

#[test]
fn await_response_and_watchdog_together_on_the_real_runtime() {
    use std::{
        future::{Future, poll_fn},
        pin::Pin,
        task::Poll,
    };
    let rt = runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let seat = Seat::default();
    let (mut agent, mut driver) = seat
        .start(
            cx.clone(),
            session(&cx, 3_000_000),
            route(),
            || Ok(Sink::new(trace())),
            clean,
        )
        .unwrap();
    agent
        .submit(&bytes(0, key(true)), InputDelivery::Reliable)
        .unwrap();
    let mut response = agent.response();
    let result = rt
        .block_on(poll_fn(|task| {
            assert!(Pin::new(&mut driver).poll(task).is_pending());
            Pin::new(&mut response).poll(task)
        }))
        .unwrap();
    drop(response);
    assert_eq!(submitted(result).outcome, InputOutcome::SubmittedToOs);
    agent.control().stop(StopReason::LocalRevoke);
    let shutdown = rt.block_on(poll_fn(|task| match Pin::new(&mut driver).poll(task) {
        Poll::Ready(report) => Poll::Ready(report),
        Poll::Pending => Poll::Pending,
    }));
    assert!(shutdown.handoff_safe());
}
