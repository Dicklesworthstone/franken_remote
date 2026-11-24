//! The actual client encoder, wire codec and host submission owner, with an
//! explicit recording OS boundary. Native XTest has its own integration test.
use super::*;
use fr_media::freshness::{ClockCorrelation, ClockPolicy, ClockSample};
use fr_wire::input_ticket::{self, INPUT_TICKET_BYTES, Ticket};

fn clock() -> ClockCorrelation {
    ClockCorrelation::new(
        ClockSample {
            host_boot: HostBootId::from_raw(4),
            client_sent_us: ORIGIN,
            client_received_us: ORIGIN,
            host_sample_us: 0,
        },
        ClockPolicy {
            drift_ppm: 0,
            ..ClockPolicy::default()
        },
    )
    .unwrap()
}

fn renew(viewer: &mut InputClient, host: &mut InputSession, id: u128, sequence: u64) {
    let ticket = InputTicketId::from_raw(id);
    let until = host.issue_ticket(ticket, HostInstant::ORIGIN).unwrap();
    let mut bytes = [0; INPUT_TICKET_BYTES];
    input_ticket::encode(
        Ticket {
            credentials: InputCredentials {
                ticket,
                ..credentials()
            },
            sequence,
            issued_at_us: 0,
            expires_at_us: until.as_micros(),
        },
        &mut bytes,
        &ProtocolLimits::ABSOLUTE,
        7,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .unwrap();
    viewer.accept_ticket(&bytes, clock(), local(1)).unwrap();
}

fn complete(
    viewer: &mut InputClient,
    host: &mut InputSession,
    sink: &mut Sink,
    op: Action<'_>,
) -> InputResult {
    let bytes = action(viewer, op);
    let result = dispatch(host, sink, &bytes, SequenceSpace::Action);
    assert_eq!(
        viewer.result(&result_bytes(result), local(1)).unwrap(),
        ResultEvent::Completed(result)
    );
    result
}

#[test]
fn relative_roundtrip_waits_for_mode_receipt_and_post_mode_ticket_then_restores_absolute() {
    let mut viewer = client(Policy::default());
    let mut host = host();
    let mut sink = Sink::default();
    let mut out = [0; MAX_INPUT_RECORD_BYTES];
    ready(&mut viewer);
    renew(&mut viewer, &mut host, 4, 0);
    let bytes = action(&mut viewer, Action::Mode(PointerMode::Relative));
    // A ticket issued before the host processes the mode may already be in
    // flight. Receiving it before the mode receipt cannot cross the barrier.
    renew(&mut viewer, &mut host, 5, 1);
    assert!(viewer.pointer_mode_pending());
    assert_eq!(
        viewer.action(Action::Relative { x: 9, y: -7 }, &mut out, local(1)),
        Err(Error::Backpressure)
    );
    let entered = dispatch(&mut host, &mut sink, &bytes, SequenceSpace::Action);
    assert_eq!(entered.outcome, InputOutcome::AppliedLocally);
    assert_eq!(entered.stage, Stage::Admitted);
    assert_eq!(entered.submitted_operations, 0);
    assert!(sink.operations.is_empty());
    viewer.result(&result_bytes(entered), local(1)).unwrap();
    assert!(viewer.pointer_mode_pending());
    assert_eq!(
        viewer.pointer(DesktopPoint { x: 1, y: 1 }, &mut out, local(1)),
        Err(Error::Backpressure)
    );
    assert_eq!(
        viewer.result(&result_bytes(entered), local(1)).unwrap(),
        ResultEvent::Duplicate(entered)
    );
    assert!(viewer.pointer_mode_pending());
    renew(&mut viewer, &mut host, 6, 2);
    assert!(!viewer.pointer_mode_pending());
    assert_eq!(
        viewer.pointer(DesktopPoint { x: 1, y: 1 }, &mut out, local(1)),
        Err(Error::InvalidTransition)
    );
    for (x, y) in [(9, -7), (-2, 3)] {
        let result = complete(&mut viewer, &mut host, &mut sink, Action::Relative { x, y });
        assert_eq!(result.outcome, InputOutcome::SubmittedToOs);
        assert_eq!(result.submitted_operations, 1);
    }
    assert_eq!(
        sink.operations,
        [
            Operation::Relative { x: 9, y: -7 },
            Operation::Relative { x: -2, y: 3 }
        ]
    );
    let restored = complete(
        &mut viewer,
        &mut host,
        &mut sink,
        Action::Mode(PointerMode::Absolute),
    );
    assert_eq!(restored.outcome, InputOutcome::AppliedLocally);
    assert_eq!(restored.submitted_operations, 0);
    renew(&mut viewer, &mut host, 7, 3);
    let bytes = pointer(&mut viewer, 12);
    let result = dispatch(&mut host, &mut sink, &bytes, SequenceSpace::Pointer);
    assert_eq!(result.outcome, InputOutcome::SubmittedToOs);
    viewer.result(&result_bytes(result), local(1)).unwrap();
    assert_eq!(
        sink.operations.last(),
        Some(&Operation::Absolute(DesktopPoint { x: 12, y: 1 }))
    );
    assert_eq!(viewer.stopped(), None);
}

#[test]
fn mode_requires_wire_tickets_and_a_drained_unheld_input_owner() {
    let mut viewer = client(Policy::default());
    let mut host = host();
    let mut sink = Sink::default();
    let mut out = [0; MAX_INPUT_RECORD_BYTES];
    ready(&mut viewer);
    assert_eq!(
        viewer.action(Action::Mode(PointerMode::Relative), &mut out, local(1)),
        Err(Error::Unsupported)
    );
    renew(&mut viewer, &mut host, 4, 0);
    let key = PhysicalKey::new(4).unwrap();
    let press = action(
        &mut viewer,
        Action::Key {
            key,
            transition: KeyTransition::Press,
        },
    );
    assert_eq!(
        viewer.action(Action::Mode(PointerMode::Relative), &mut out, local(1)),
        Err(Error::InvalidTransition)
    );
    let result = dispatch(&mut host, &mut sink, &press, SequenceSpace::Action);
    viewer.result(&result_bytes(result), local(1)).unwrap();
    assert_eq!(viewer.pending_actions(), 0);
    assert_eq!(
        viewer.action(Action::Mode(PointerMode::Relative), &mut out, local(1)),
        Err(Error::InvalidTransition),
        "a completed press is still held"
    );
    complete(
        &mut viewer,
        &mut host,
        &mut sink,
        Action::Key {
            key,
            transition: KeyTransition::Release,
        },
    );
    let encoded = viewer
        .action(Action::Mode(PointerMode::Relative), &mut out, local(1))
        .unwrap();
    assert_eq!(encoded.sequence, 2, "refused attempts consumed no identity");
}

#[test]
fn mode_cannot_claim_os_submission_and_local_receipts_cannot_acknowledge_movement() {
    for mode in [true, false] {
        let mut viewer = client(Policy::default());
        let mut host = host();
        let mut sink = Sink::default();
        ready(&mut viewer);
        renew(&mut viewer, &mut host, 4, 0);
        let action = if mode {
            Action::Mode(PointerMode::Relative)
        } else {
            Action::Text("x")
        };
        let bytes = super::action(&mut viewer, action);
        let mut result = dispatch(&mut host, &mut sink, &bytes, SequenceSpace::Action);
        result.outcome = if mode {
            InputOutcome::SubmittedToOs
        } else {
            InputOutcome::AppliedLocally
        };
        result.stage = if mode {
            Stage::SubmittedToOs
        } else {
            Stage::Admitted
        };
        result.submitted_operations = u32::from(mode);
        assert_eq!(
            viewer.result(&result_bytes(result), local(1)),
            Err(Error::Stopped(StopReason::InvalidReceipt))
        );
    }
}

#[test]
fn uncertain_relative_movement_fences_restoration_without_reencoding() {
    let mut viewer = client(Policy::default());
    let mut host = host();
    let mut sink = Sink::default();
    ready(&mut viewer);
    renew(&mut viewer, &mut host, 4, 0);
    complete(
        &mut viewer,
        &mut host,
        &mut sink,
        Action::Mode(PointerMode::Relative),
    );
    renew(&mut viewer, &mut host, 5, 1);
    sink.unknown_at = Some(0);
    let result = complete(
        &mut viewer,
        &mut host,
        &mut sink,
        Action::Relative { x: 5, y: 8 },
    );
    assert_eq!(result.outcome, InputOutcome::EffectUnknown);
    let mut out = [0; MAX_INPUT_RECORD_BYTES];
    for action in [
        Action::Relative { x: 5, y: 8 },
        Action::Mode(PointerMode::Absolute),
    ] {
        assert_eq!(
            viewer.action(action, &mut out, local(1)),
            Err(Error::Stopped(StopReason::ActionFailed))
        );
    }
    assert!(sink.operations.is_empty());
}
