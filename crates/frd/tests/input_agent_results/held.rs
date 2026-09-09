//! The production native thread and mailbox, with explicit fault-injection sinks.
use super::*;
use asupersync::types::CancelKind;
use fr_core::held_state::{HeldState, HeldStateRequest};
use fr_wire::held_state::{HELD_STATE_BYTES, encode};
#[derive(Clone, Copy)]
enum Fault {
    CancelPreparation,
    UnknownSubmission,
    PanicSubmission,
}
struct Native {
    cx: Cx,
    fault: Fault,
    preparations: usize,
    submissions: usize,
    effects: Arc<Mutex<Vec<Operation>>>,
    restores: Arc<AtomicUsize>,
}
fn release(op: Operation) -> bool {
    matches!(
        op,
        Operation::Key {
            transition: KeyTransition::Release,
            ..
        }
    )
}
impl InputSink for Native {
    fn prepare(&mut self, op: Operation) -> Result<(), PlatformError> {
        if release(op) {
            self.preparations += 1;
            if self.preparations == 2 && matches!(self.fault, Fault::CancelPreparation) {
                self.cx.cancel_fast(CancelKind::User);
            }
        }
        Ok(())
    }
    fn submit(&mut self, op: Operation) -> Submission {
        if release(op) {
            self.submissions += 1;
            if self.submissions == 2 {
                match self.fault {
                    Fault::UnknownSubmission => return Submission::Unknown,
                    Fault::PanicSubmission => panic!("injected second-release native panic"),
                    Fault::CancelPreparation => {}
                }
            }
        }
        self.effects.lock().unwrap().push(op);
        Submission::Submitted
    }
    fn cancel_prepared(&mut self) {
        self.restores.fetch_add(1, Ordering::Relaxed);
    }
}
fn run(fault: Fault) -> Reconciliation {
    let rt = runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let native_cx = cx.clone();
    let effects = Arc::new(Mutex::new(Vec::new()));
    let native_effects = effects.clone();
    let restores = Arc::new(AtomicUsize::new(0));
    let native_restores = restores.clone();
    let seat = Seat::default();
    let (mut agent, driver) = seat
        .start(
            cx.clone(),
            session(&cx, 3_000_000),
            route(),
            move || {
                Ok(Native {
                    cx: native_cx,
                    fault,
                    preparations: 0,
                    submissions: 0,
                    effects: native_effects,
                    restores: native_restores,
                })
            },
            |_| true,
        )
        .unwrap();
    // Deliberately retain Driver without scheduling it. Final checks on the
    // native thread, not watchdog scheduling, must fence cancelled preparation.
    for (seq, usage) in [(0, 4), (1, 225)] {
        agent
            .submit(
                &bytes(
                    seq,
                    InputEvent::Key {
                        key: PhysicalKey::new(usage).unwrap(),
                        transition: KeyTransition::Press,
                    },
                ),
                InputDelivery::Reliable,
            )
            .unwrap();
        assert_eq!(
            record(input_reply(&mut agent)).outcome,
            InputOutcome::SubmittedToOs
        );
    }
    let c = credentials();
    let mut state = [0; HELD_STATE_BYTES];
    encode(
        HeldStateRequest {
            session: c.session,
            lease: c.lease,
            sequence: 0,
            next_action: 2,
            held: HeldState::empty(),
        },
        &mut state,
        &ProtocolLimits::ABSOLUTE,
        7,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .unwrap();
    agent.reconcile_held(&state).unwrap();
    // Projection selection cannot consume the report or invent action sequence 2.
    assert_eq!(agent.try_input_result(), Err(Error::NotInputCommand));
    let result = reply(&mut agent);
    assert!(agent.control().is_stopped());
    let shutdown = rt.block_on(driver);
    assert!(shutdown.handoff_safe());
    assert!(!seat.is_occupied());
    assert_eq!(shutdown.last_cleanup.unwrap().remaining, 0);
    let ((
        Fault::PanicSubmission,
        Reply::ReconciliationPanic {
            report: Some(report),
        },
    )
    | (Fault::CancelPreparation | Fault::UnknownSubmission, Reply::Reconciliation(Ok(report)))) =
        (fault, result)
    else {
        panic!("unexpected reconciliation reply: {result:?}");
    };
    assert_eq!(report.sequence, 0);
    assert_eq!(report.submitted_releases, 1);
    assert_eq!(report.remaining_releases, 1);
    let effects = effects.lock().unwrap();
    assert_eq!(
        effects.len(),
        4,
        "two presses and two confirmed releases, never replayed presses"
    );
    assert!(effects[..2].iter().all(|op| !release(*op)));
    assert!(effects[2..].iter().all(|op| release(*op)));
    assert!(restores.load(Ordering::Relaxed) >= 2);
    report
}
#[test]
fn parent_cancellation_after_second_release_preparation_preserves_first_release() {
    let report = run(Fault::CancelPreparation);
    assert_eq!(report.outcome, ReconciliationOutcome::Refused);
    assert!(report.refusal.is_some());
}
#[test]
fn uncertain_release_reports_prefix_and_independent_cleanup_keeps_ownership() {
    let report = run(Fault::UnknownSubmission);
    assert_eq!(report.outcome, ReconciliationOutcome::EffectUnknown);
    assert_eq!(report.refusal, Some(Refusal::UnknownEffect));
}
#[test]
fn native_release_panic_keeps_reconciliation_prefix_not_an_action_receipt() {
    let report = run(Fault::PanicSubmission);
    assert_eq!(report.outcome, ReconciliationOutcome::EffectUnknown);
}
