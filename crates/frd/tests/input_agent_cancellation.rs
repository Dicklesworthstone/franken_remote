use asupersync::{
    cx::Cx,
    runtime::{Runtime, RuntimeBuilder},
    types::{Budget, CancelKind},
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
    input_agent::{Agent, Reply, Route, Seat},
    input_watchdog::host_now,
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
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

struct CancelInPrepare {
    cx: Cx,
    prepares: usize,
    cancel_at: usize,
    effects: Arc<Mutex<Vec<Operation>>>,
    restored: Arc<AtomicUsize>,
}
impl InputSink for CancelInPrepare {
    fn prepare(&mut self, _: Operation) -> Result<(), PlatformError> {
        self.prepares += 1;
        if self.prepares == self.cancel_at {
            self.cx.cancel_fast(CancelKind::User);
        }
        Ok(())
    }
    fn submit(&mut self, op: Operation) -> Submission {
        self.effects.lock().unwrap().push(op);
        Submission::Submitted
    }
    fn cancel_prepared(&mut self) {
        self.restored.fetch_add(1, Ordering::Relaxed);
    }
}
fn cancelled_during_prepare(
    event: InputEvent<'_>,
    cancel_at: usize,
    expected: InputOutcome,
    prefix: u32,
) {
    let rt = runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let native_cx = cx.clone();
    let effects = Arc::new(Mutex::new(Vec::new()));
    let native_effects = effects.clone();
    let restored = Arc::new(AtomicUsize::new(0));
    let native_restored = restored.clone();
    let seat = Seat::default();
    let (mut agent, driver) = seat
        .start(
            cx.clone(),
            session(&cx, 3_000_000),
            Route::new(7, ProtocolLimits::ABSOLUTE),
            move || {
                Ok(CancelInPrepare {
                    cx: native_cx,
                    prepares: 0,
                    cancel_at,
                    effects: native_effects,
                    restored: native_restored,
                })
            },
            |_| true,
        )
        .unwrap();
    // Keep Driver owned but deliberately unscheduled until the native result.
    // This models scheduler lag, not a cancellation callback in the watchdog.
    agent
        .submit(&bytes(0, event), InputDelivery::Reliable)
        .unwrap();
    let value = submitted(reply(&mut agent));
    let shutdown = rt.block_on(driver);
    assert!(shutdown.handoff_safe());
    assert!(!seat.is_occupied());
    assert_eq!(value.outcome, expected);
    assert_eq!(value.submitted_operations, prefix);
    assert!(restored.load(Ordering::Relaxed) >= cancel_at);
    let native = effects.lock().unwrap();
    assert_eq!(native.len(), prefix as usize);
}
#[test]
fn cancellation_in_preflight_refuses_press_even_before_watchdog_is_scheduled() {
    cancelled_during_prepare(key(true), 1, InputOutcome::CancelledBeforeSubmission, 0);
}
#[test]
fn cancellation_between_text_scalars_keeps_only_the_submitted_prefix() {
    cancelled_during_prepare(
        InputEvent::Text("ab"),
        2,
        InputOutcome::PartiallySubmittedToOs,
        1,
    );
}
