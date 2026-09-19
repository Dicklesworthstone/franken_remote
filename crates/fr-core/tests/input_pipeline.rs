//! Comprehensive unit tests for the input pipeline:
//! 1. Sequence spaces: independent replaceable pointer vs ordered reliable actions,
//!    pointer sequence gaps, pointer barriers, and gap fencing on actions.
//! 2. Ticket lifetime and floor logic: submission-time expiry, monotonic floor,
//!    no replay after expiry, and dependent action fencing.
//! 3. Dedupe-cache eviction: ring buffer eviction, in-flight deduplication,
//!    evicted receipts returning `ConsumedWithoutReceipt`, and stale lease rejection.

use fr_core::authority::{AuthorityError, AuthorityPolicy, SessionAuthority};
use fr_core::ids::{
    CodecConfigurationGeneration, DisplayGeometryGeneration, InputLeaseId, InputTicketId,
    RecoveryGeneration, RemoteSessionId, ViewportMappingGeneration,
};
use fr_core::input::*;
use fr_core::input_sequence::*;
use fr_core::input_submission::*;
use fr_core::time::HostInstant;

fn lease() -> InputLeaseId {
    InputLeaseId::from_raw(1)
}

fn credentials(ticket: InputTicketId) -> InputCredentials {
    InputCredentials {
        session: RemoteSessionId::from_raw(1),
        lease: lease(),
        ticket,
        view: InputView {
            geometry: DisplayGeometryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
    }
}

fn caps() -> Capabilities {
    Capabilities::default()
        .with(Capability::Absolute)
        .with(Capability::Buttons)
        .with(Capability::Keys)
        .with(Capability::Repeat)
        .with(Capability::Text)
        .with(Capability::LineScroll)
}

fn bounds() -> InputBounds {
    InputBounds::new(DesktopPoint { x: 0, y: 0 }, 1920, 1080).unwrap()
}

#[derive(Default)]
struct TestSink {
    operations: Vec<Operation>,
    fail_prepare: bool,
}

impl InputSink for TestSink {
    fn prepare(&mut self, _op: Operation) -> Result<(), PlatformError> {
        if self.fail_prepare {
            Err(PlatformError::Unavailable)
        } else {
            Ok(())
        }
    }
    fn submit(&mut self, op: Operation) -> Submission {
        self.operations.push(op);
        Submission::Submitted
    }
}

fn make_session(t0: HostInstant) -> (InputSession, InputTicketId, HostInstant) {
    let creds = credentials(InputTicketId::from_raw(10));
    let mut a = SessionAuthority::new(creds.session, AuthorityPolicy::plan_defaults());
    a.mark_capabilities_checked().unwrap();
    a.authorize_observation(t0).unwrap();
    a.mark_view_ready(t0).unwrap();
    a.grant_lease(creds.lease, t0).unwrap();
    let deadline = a.issue_input_ticket(creds.lease, creds.ticket, t0).unwrap();
    let session = InputSession::new(a, creds, bounds(), caps(), t0).unwrap();
    (session, creds.ticket, deadline)
}

#[test]
fn sequence_spaces_independent_pointer_and_reliable_actions() {
    let t0 = HostInstant::from_micros(0);
    let (mut session, ticket, _) = make_session(t0);
    let mut sink = TestSink::default();

    // 1. Pointer motions in independent sequence space: gaps are accepted as replaceable state
    let p0 = InputRequest {
        credentials: credentials(ticket),
        sequence: 0,
        event: InputEvent::Pointer {
            position: DesktopPoint { x: 10, y: 20 },
        },
    };
    assert!(matches!(
        session.dispatch(p0, &mut sink, || t0),
        Ok(Dispatch::Completed(_))
    ));
    assert_eq!(sink.operations.len(), 1);

    // Pointer gap: jump from sequence 0 directly to 5 (simulating packet loss of 1..=4)
    let p5 = InputRequest {
        credentials: credentials(ticket),
        sequence: 5,
        event: InputEvent::Pointer {
            position: DesktopPoint { x: 15, y: 25 },
        },
    };
    assert!(matches!(
        session.dispatch(p5, &mut sink, || t0),
        Ok(Dispatch::Completed(_))
    ));
    assert_eq!(sink.operations.len(), 2);

    // Obsolete pointer arrival (sequence 3 < floor of 5) is rejected as obsolete without error
    let p3 = InputRequest {
        credentials: credentials(ticket),
        sequence: 3,
        event: InputEvent::Pointer {
            position: DesktopPoint { x: 12, y: 22 },
        },
    };
    assert_eq!(
        session.dispatch(p3, &mut sink, || t0),
        Ok(Dispatch::ObsoletePointer)
    );
    // Sink received no new operation from obsolete pointer
    assert_eq!(sink.operations.len(), 2);

    // 2. Reliable action sequence space starts at 0 and is unaffected by pointer floor of 5
    let a0 = InputRequest {
        credentials: credentials(ticket),
        sequence: 0,
        event: InputEvent::Key {
            key: PhysicalKey::new(4).unwrap(),
            transition: KeyTransition::Press,
        },
    };
    assert!(matches!(
        session.dispatch(a0, &mut sink, || t0),
        Ok(Dispatch::Completed(_))
    ));
    assert_eq!(sink.operations.len(), 3);

    // Reliable action sequence 1 proceeds normally
    let a1 = InputRequest {
        credentials: credentials(ticket),
        sequence: 1,
        event: InputEvent::Key {
            key: PhysicalKey::new(4).unwrap(),
            transition: KeyTransition::Release,
        },
    };
    assert!(matches!(
        session.dispatch(a1, &mut sink, || t0),
        Ok(Dispatch::Completed(_))
    ));
    assert_eq!(sink.operations.len(), 4);
}

#[test]
fn click_barrier_prevents_late_preclick_motion_from_moving_pointer_backward() {
    let t0 = HostInstant::from_micros(0);
    let (mut session, ticket, _) = make_session(t0);
    let mut sink = TestSink::default();

    // 1. Motion at pointer sequence 10
    let p10 = InputRequest {
        credentials: credentials(ticket),
        sequence: 10,
        event: InputEvent::Pointer {
            position: DesktopPoint { x: 100, y: 100 },
        },
    };
    assert!(matches!(
        session.dispatch(p10, &mut sink, || t0),
        Ok(Dispatch::Completed(_))
    ));

    // 2. Click (action sequence 0) with coordinate (200, 200) and barrier 15
    let click = InputRequest {
        credentials: credentials(ticket),
        sequence: 0,
        event: InputEvent::Button {
            button: PointerButton::Primary,
            pressed: true,
            position: DesktopPoint { x: 200, y: 200 },
            barrier: 15,
        },
    };
    assert!(matches!(
        session.dispatch(click, &mut sink, || t0),
        Ok(Dispatch::Completed(_))
    ));

    // 3. Delayed pre-click motion with sequence 12 (<= barrier 15) arrives late:
    // It must be rejected as obsolete so it cannot move the pointer backward!
    let p12 = InputRequest {
        credentials: credentials(ticket),
        sequence: 12,
        event: InputEvent::Pointer {
            position: DesktopPoint { x: 120, y: 120 },
        },
    };
    assert_eq!(
        session.dispatch(p12, &mut sink, || t0),
        Ok(Dispatch::ObsoletePointer)
    );

    // 4. Fresh motion with sequence 16 (> barrier 15) is admitted normally
    let p16 = InputRequest {
        credentials: credentials(ticket),
        sequence: 16,
        event: InputEvent::Pointer {
            position: DesktopPoint { x: 250, y: 250 },
        },
    };
    assert!(matches!(
        session.dispatch(p16, &mut sink, || t0),
        Ok(Dispatch::Completed(_))
    ));
}

#[test]
fn action_sequence_gaps_fence_ledger_and_prevent_skipped_clicks() {
    let t0 = HostInstant::from_micros(0);
    let (mut session, ticket, _) = make_session(t0);
    let mut sink = TestSink::default();

    // Sequence 0 succeeds
    let a0 = InputRequest {
        credentials: credentials(ticket),
        sequence: 0,
        event: InputEvent::Button {
            button: PointerButton::Primary,
            pressed: true,
            position: DesktopPoint { x: 100, y: 100 },
            barrier: 0,
        },
    };
    assert!(matches!(
        session.dispatch(a0, &mut sink, || t0),
        Ok(Dispatch::Completed(_))
    ));

    // Sequence gap: skipping sequence 1 and sending sequence 2
    let a2 = InputRequest {
        credentials: credentials(ticket),
        sequence: 2,
        event: InputEvent::Button {
            button: PointerButton::Primary,
            pressed: false,
            position: DesktopPoint { x: 100, y: 100 },
            barrier: 0,
        },
    };
    // Must return SequenceGap and fence the ledger
    assert_eq!(
        session.dispatch(a2, &mut sink, || t0),
        Err(Refusal::Sequence(InputSequenceError::SequenceGap {
            expected: 1,
            received: 2,
        }))
    );

    // Once fenced, even the missing sequence 1 is rejected (never skipped clicks or partial recovery)
    let a1 = InputRequest {
        credentials: credentials(ticket),
        sequence: 1,
        event: InputEvent::Button {
            button: PointerButton::Primary,
            pressed: false,
            position: DesktopPoint { x: 100, y: 100 },
            barrier: 0,
        },
    };
    assert_eq!(
        session.dispatch(a1, &mut sink, || t0),
        Err(Refusal::Sequence(InputSequenceError::Fenced))
    );
}

#[test]
fn ticket_lifetime_and_submission_time_expiry() {
    let t0 = HostInstant::from_micros(100);
    let (mut session, ticket, deadline) = make_session(t0);
    let mut sink = TestSink::default();

    // Action arrived before deadline, but clock sampled at submission time is AT or AFTER deadline
    let a0 = InputRequest {
        credentials: credentials(ticket),
        sequence: 0,
        event: InputEvent::Text("a"),
    };

    // Clock returns deadline instant (ticket expired at submission checkpoint)
    let res = session.dispatch(a0, &mut sink, || deadline).unwrap();
    let Dispatch::Completed(receipt) = res else {
        panic!("expected completed receipt");
    };

    assert_eq!(receipt.outcome, InputOutcome::ExpiredBeforeSubmission);
    assert_eq!(receipt.submitted_operations, 0);
    assert_eq!(
        receipt.refusal,
        Some(Refusal::Authority(AuthorityError::TicketExpired))
    );
    // Nothing submitted to OS sink
    assert_eq!(sink.operations.len(), 0);

    // Expired submission fences the ledger; subsequent actions cannot be admitted
    let a1 = InputRequest {
        credentials: credentials(ticket),
        sequence: 1,
        event: InputEvent::Text("b"),
    };
    assert_eq!(
        session.dispatch(a1, &mut sink, || deadline),
        Err(Refusal::Sequence(InputSequenceError::Fenced))
    );
}

#[test]
fn monotonic_consumed_floor_and_dedupe_cache_eviction() {
    let capacity = 4;
    let mut ledger = InputSequenceLedger::new(lease(), capacity).unwrap();

    // Sequence starts at 0
    assert_eq!(ledger.next_sequence(), Some(0));

    // Admit and record outcomes for sequences 0, 1, 2, 3
    for seq in 0..4 {
        assert_eq!(ledger.admit(lease(), seq), Ok(InputAdmission::Admitted));
        ledger
            .record_outcome(lease(), seq, InputOutcome::SubmittedToOs)
            .unwrap();
    }
    assert_eq!(ledger.next_sequence(), Some(4));

    // All 4 receipts (0, 1, 2, 3) are in the dedupe cache
    for seq in 0..4 {
        assert_eq!(
            ledger.admit(lease(), seq),
            Ok(InputAdmission::Completed(InputOutcome::SubmittedToOs))
        );
    }

    // Now admit and complete sequence 4, causing ring buffer eviction of sequence 0
    assert_eq!(ledger.admit(lease(), 4), Ok(InputAdmission::Admitted));
    ledger
        .record_outcome(lease(), 4, InputOutcome::SubmittedToOs)
        .unwrap();
    assert_eq!(ledger.next_sequence(), Some(5));

    // Sequence 0 was evicted: re-admission returns ConsumedWithoutReceipt
    // Critical Invariant: eviction never lowers the consumed floor or permits re-execution!
    assert_eq!(
        ledger.admit(lease(), 0),
        Ok(InputAdmission::ConsumedWithoutReceipt)
    );

    // Sequences 1, 2, 3, 4 remain cached
    for seq in 1..=4 {
        assert_eq!(
            ledger.admit(lease(), seq),
            Ok(InputAdmission::Completed(InputOutcome::SubmittedToOs))
        );
    }

    // Advance 50 more sequences to test large-scale eviction
    for seq in 5..55 {
        assert_eq!(ledger.admit(lease(), seq), Ok(InputAdmission::Admitted));
        ledger
            .record_outcome(lease(), seq, InputOutcome::SubmittedToOs)
            .unwrap();
    }
    assert_eq!(ledger.next_sequence(), Some(55));

    // Old sequences 0..51 return ConsumedWithoutReceipt
    for seq in 0..51 {
        assert_eq!(
            ledger.admit(lease(), seq),
            Ok(InputAdmission::ConsumedWithoutReceipt)
        );
    }

    // Last 4 sequences (51..=54) return Completed
    for seq in 51..55 {
        assert_eq!(
            ledger.admit(lease(), seq),
            Ok(InputAdmission::Completed(InputOutcome::SubmittedToOs))
        );
    }
}

#[test]
fn dedupe_in_flight_and_pending_guards() {
    let mut ledger = InputSequenceLedger::new(lease(), 4).unwrap();

    // Admit sequence 0
    assert_eq!(ledger.admit(lease(), 0), Ok(InputAdmission::Admitted));
    assert_eq!(ledger.pending_sequence(), Some(0));

    // Duplicate while in flight returns InFlight
    assert_eq!(ledger.admit(lease(), 0), Ok(InputAdmission::InFlight));

    // Attempting to admit next sequence while sequence 0 is in flight is refused
    assert_eq!(
        ledger.admit(lease(), 1),
        Err(InputSequenceError::PreviousActionPending { sequence: 0 })
    );

    // Complete sequence 0
    ledger
        .record_outcome(lease(), 0, InputOutcome::AppliedLocally)
        .unwrap();
    assert_eq!(ledger.pending_sequence(), None);

    // Now re-admitting 0 returns Completed(AppliedLocally)
    assert_eq!(
        ledger.admit(lease(), 0),
        Ok(InputAdmission::Completed(InputOutcome::AppliedLocally))
    );

    // And sequence 1 can now be admitted
    assert_eq!(ledger.admit(lease(), 1), Ok(InputAdmission::Admitted));
}

#[test]
fn stale_and_foreign_lease_rejections() {
    let mut ledger = InputSequenceLedger::new(lease(), 4).unwrap();
    let foreign = InputLeaseId::from_raw(999);

    assert_eq!(
        ledger.admit(foreign, 0),
        Err(InputSequenceError::StaleLease)
    );
    assert_eq!(ledger.next_sequence(), Some(0));

    // Admitting on correct lease works
    assert_eq!(ledger.admit(lease(), 0), Ok(InputAdmission::Admitted));

    // Recording outcome with foreign lease is rejected
    assert_eq!(
        ledger.record_outcome(foreign, 0, InputOutcome::SubmittedToOs),
        Err(InputSequenceError::StaleLease)
    );
    assert_eq!(ledger.pending_sequence(), Some(0));
}

#[test]
fn non_successful_outcomes_fence_all_subsequent_admissions() {
    for outcome in [
        InputOutcome::RejectedBeforeSubmission,
        InputOutcome::ExpiredBeforeSubmission,
        InputOutcome::CancelledBeforeSubmission,
        InputOutcome::PartiallySubmittedToOs,
        InputOutcome::EffectUnknown,
    ] {
        let mut ledger = InputSequenceLedger::new(lease(), 4).unwrap();
        assert_eq!(ledger.admit(lease(), 0), Ok(InputAdmission::Admitted));
        ledger.record_outcome(lease(), 0, outcome).unwrap();

        assert!(ledger.is_fenced());
        assert_eq!(ledger.admit(lease(), 1), Err(InputSequenceError::Fenced));
        assert_eq!(
            ledger.admit(lease(), 0),
            Ok(InputAdmission::Completed(outcome))
        );
    }
}
