//! Client validation distinguishes one atomic platform scroll from a complete
//! bounded wheel expansion. These are receipt-validation fixtures, not OS proof.
use fr_client::input::{
    Action, ClientInstant, Error, InputClient, Policy, PresentedObservation, ResultEvent,
    StopReason,
};
use fr_core::{
    ids::*,
    input::*,
    input_sequence::InputOutcome,
    input_submission::{Capabilities, Capability, PlatformError, Receipt, Refusal, scroll::LINE},
    limits::ProtocolLimits,
};
use fr_wire::{input::*, input_result::*};
fn binding() -> ResultBinding {
    ResultBinding {
        channel: 7,
        session: RemoteSessionId::from_raw(1),
        lease: InputLeaseId::from_raw(2),
    }
}
fn pending(x: i32, y: i32, unit: ScrollUnit) -> InputClient {
    let b = binding();
    let view = InputView {
        geometry: DisplayGeometryGeneration::INITIAL,
        viewport: ViewportMappingGeneration::INITIAL,
        configuration: CodecConfigurationGeneration::INITIAL,
        recovery: RecoveryGeneration::INITIAL,
    };
    let c = InputCredentials {
        session: b.session,
        lease: b.lease,
        ticket: InputTicketId::from_raw(3),
        view,
    };
    let mut client = InputClient::new(
        c,
        b.channel,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
        Capabilities::default()
            .with(Capability::Absolute)
            .with(Capability::LineScroll)
            .with(Capability::PixelScroll),
        ProtocolLimits::ABSOLUTE,
        Policy::default(),
        ClientInstant(0),
    )
    .unwrap();
    client
        .confirm_mapping(c.session, c.view, ClientInstant(0))
        .unwrap();
    client
        .presented(
            PresentedObservation {
                session: c.session,
                serial: 0,
                view,
                received_at: ClientInstant(0),
                source_age_upper_us: 0,
            },
            ClientInstant(0),
        )
        .unwrap();
    let mut bytes = [0; MAX_INPUT_RECORD_BYTES];
    let result = client
        .action(
            Action::Scroll {
                position: DesktopPoint { x: 20, y: 30 },
                x,
                y,
                unit,
            },
            &mut bytes,
            ClientInstant(1),
        )
        .unwrap();
    assert_eq!(result.sequence, 0);
    let decoded = decode_input(
        &bytes[..result.bytes],
        &ProtocolLimits::ABSOLUTE,
        b.channel,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .unwrap();
    assert_eq!(
        decoded.event,
        InputEvent::Scroll {
            position: DesktopPoint { x: 20, y: 30 },
            barrier: 0,
            x,
            y,
            unit
        }
    );
    client
}
fn bytes(count: u32, outcome: InputOutcome) -> Vec<u8> {
    let r = Receipt {
        sequence: 0,
        outcome,
        submitted_operations: count,
        refusal: if outcome == InputOutcome::SubmittedToOs {
            None
        } else {
            Some(Refusal::Platform(PlatformError::Unavailable))
        },
    };
    let r = InputResult::from_receipt(binding(), SequenceSpace::Action, r).unwrap();
    let mut bytes = vec![0; INPUT_RESULT_BYTES];
    let n = encode_input_result(
        r,
        &mut bytes,
        &ProtocolLimits::ABSOLUTE,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .unwrap();
    bytes.truncate(n);
    bytes
}
#[test]
fn complete_atomic_or_discrete_counts_and_zero_are_accepted_without_new_identity() {
    for (x, y, counts) in [
        (LINE, LINE, vec![2, 5]),
        (0, 0, vec![1, 2]),
        (32 * LINE, 0, vec![2, 65]),
        (LINE / 2, 0, vec![2]),
    ] {
        for count in counts {
            let mut c = pending(x, y, ScrollUnit::Lines);
            let b = bytes(count, InputOutcome::SubmittedToOs);
            assert!(matches!(
                c.result(&b, ClientInstant(2)).unwrap(),
                ResultEvent::Completed(_)
            ));
            assert_eq!(c.pending_actions(), 0);
            assert_eq!(c.stopped(), None);
            assert!(matches!(
                c.result(&b, ClientInstant(3)).unwrap(),
                ResultEvent::Duplicate(_)
            ));
            assert_eq!(c.pending_actions(), 0);
        }
    }
}
#[test]
fn incomplete_or_impossible_wheel_count_cannot_be_called_a_completed_scroll() {
    for count in [1, 3, 4, 6, 65] {
        let mut c = pending(LINE, LINE, ScrollUnit::Lines);
        assert_eq!(
            c.result(&bytes(count, InputOutcome::SubmittedToOs), ClientInstant(2)),
            Err(Error::Stopped(StopReason::InvalidReceipt))
        );
    }
}
#[test]
fn partial_native_prefix_is_retained_and_stops_further_actions() {
    for count in 1..5 {
        let mut c = pending(LINE, LINE, ScrollUnit::Lines);
        let b = bytes(count, InputOutcome::PartiallySubmittedToOs);
        let ResultEvent::Completed(r) = c.result(&b, ClientInstant(2)).unwrap() else {
            panic!("missing prefix")
        };
        assert_eq!(r.submitted_operations, count);
        assert_eq!(c.stopped(), Some(StopReason::ActionFailed));
        assert!(matches!(
            c.result(&b, ClientInstant(3)).unwrap(),
            ResultEvent::Duplicate(_)
        ));
    }
}
#[test]
fn pixel_or_fractional_lines_do_not_admit_an_invented_discrete_realization() {
    for (x, y, unit) in [
        (LINE, LINE, ScrollUnit::Pixels),
        (LINE / 2, 0, ScrollUnit::Lines),
        (33 * LINE, 0, ScrollUnit::Lines),
    ] {
        let mut c = pending(x, y, unit);
        assert_eq!(
            c.result(&bytes(3, InputOutcome::SubmittedToOs), ClientInstant(2)),
            Err(Error::Stopped(StopReason::InvalidReceipt))
        );
    }
}
