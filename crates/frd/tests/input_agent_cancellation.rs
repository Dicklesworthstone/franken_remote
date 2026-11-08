use asupersync::{
    cx::Cx,
    types::{Budget, CancelKind},
};
use fr_core::{
    input::*, input_sequence::InputOutcome, input_submission::*, limits::ProtocolLimits,
};
use fr_wire::input::{InputDelivery, InputDirection};
use frd::input_agent::{Reply, Route, Seat};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

#[path = "support/input_agent.rs"]
#[allow(dead_code)] // Each suite consumes a different subset of the shared fixture.
mod support;
use support::*;

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

use fr_core::held_state::{HeldState, HeldStateRequest};
fn held_record(next_action: u64) -> [u8; fr_wire::held_state::HELD_STATE_BYTES] {
    let c = credentials();
    let mut bytes = [0; fr_wire::held_state::HELD_STATE_BYTES];
    let n = fr_wire::held_state::encode(
        HeldStateRequest {
            session: c.session,
            lease: c.lease,
            sequence: 0,
            next_action,
            held: HeldState::empty(),
        },
        &mut bytes,
        &ProtocolLimits::ABSOLUTE,
        7,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .unwrap();
    assert_eq!(n, bytes.len());
    bytes
}
#[test]
fn backpressured_snapshot_cannot_replace_the_original_action_result_binding() {
    let rt = runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let effects = Arc::new(Mutex::new(Vec::new()));
    let seat = Seat::default();
    let native_cx = cx.clone();
    let (mut agent, driver) = seat
        .start(
            cx.clone(),
            session(&cx, 3_000_000),
            Route::new(7, ProtocolLimits::ABSOLUTE),
            move || {
                Ok(CancelInPrepare {
                    cx: native_cx,
                    prepares: 0,
                    cancel_at: usize::MAX,
                    effects,
                    restored: Arc::new(AtomicUsize::new(0)),
                })
            },
            |_| true,
        )
        .unwrap();
    agent
        .submit(&bytes(0, key(true)), InputDelivery::Reliable)
        .unwrap();
    assert_eq!(
        agent.reconcile_held(&held_record(1)),
        Err(frd::input_agent::Error::Backpressure)
    );
    let result = rt.block_on(agent.input_response().unwrap()).unwrap();
    let frd::input_agent::InputReply::Record(result) = result else {
        panic!("uncollected action result was replaced")
    };
    assert_eq!(result.binding.session, credentials().session);
    assert_eq!(result.binding.lease, credentials().lease);
    assert_eq!(result.sequence, 0);
    assert_eq!(result.outcome, InputOutcome::SubmittedToOs);
    agent.reconcile_held(&held_record(1)).unwrap();
    assert!(matches!(
        reply(&mut agent),
        Reply::Reconciliation(Ok(Reconciliation {
            submitted_releases: 1,
            refusal: None,
            ..
        }))
    ));
    agent
        .control()
        .stop(frd::input_watchdog::StopReason::LocalRevoke);
    assert!(rt.block_on(driver).handoff_safe());
}
