use fr_client::input::{
    Action, ClientInstant, Error, InputClient, Policy, PresentedObservation, StopReason,
    held::HELD_STATE_INTERVAL_US,
};
use fr_core::{
    held_state::HeldState,
    ids::*,
    input::*,
    input_submission::{Capabilities, Capability},
    limits::ProtocolLimits,
};
use fr_wire::{
    held_state::{HELD_STATE_BYTES, decode},
    input::{InputDelivery, InputDirection, MAX_INPUT_RECORD_BYTES},
};
fn key(usage: u16) -> PhysicalKey {
    PhysicalKey::new(usage).unwrap()
}
fn press(usage: u16) -> Action<'static> {
    Action::Key {
        key: key(usage),
        transition: KeyTransition::Press,
    }
}
fn setup() -> InputClient {
    let c = InputCredentials {
        session: RemoteSessionId::from_raw(1),
        lease: InputLeaseId::from_raw(2),
        ticket: InputTicketId::from_raw(3),
        view: InputView {
            geometry: DisplayGeometryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
    };
    let mut client = InputClient::new(
        c,
        7,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
        Capabilities::default()
            .with(Capability::Keys)
            .with(Capability::Buttons)
            .with(Capability::Absolute),
        ProtocolLimits::ABSOLUTE,
        Policy {
            view_age_us: 1_500_000,
            receipt_timeout_us: 2_000_000,
        },
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
                view: c.view,
                received_at: ClientInstant(0),
                source_age_upper_us: 0,
            },
            ClientInstant(0),
        )
        .unwrap();
    client
}
fn parse(data: &[u8]) -> fr_core::held_state::HeldStateRequest {
    decode(
        data,
        &ProtocolLimits::ABSOLUTE,
        7,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .unwrap()
}
#[test]
fn missed_release_clears_remembered_state_without_consuming_an_action_or_receipt() {
    let mut c = setup();
    let mut out = [0; MAX_INPUT_RECORD_BYTES];
    assert_eq!(
        c.action(press(4), &mut out, ClientInstant(1))
            .unwrap()
            .sequence,
        0
    );
    let r = c
        .reconcile_held(HeldState::empty(), &mut out, ClientInstant(2))
        .unwrap()
        .unwrap();
    assert_eq!(
        (r.sequence, r.next_action, r.bytes),
        (0, 1, HELD_STATE_BYTES)
    );
    assert_eq!(parse(&out[..r.bytes]).held, HeldState::empty());
    assert_eq!(c.pending_actions(), 1);
    assert_eq!(
        c.action(press(4), &mut out, ClientInstant(3))
            .unwrap()
            .sequence,
        1
    );
    assert_eq!(c.pending_actions(), 2);
}
#[test]
fn snapshot_never_introduces_a_key_or_button_not_sent_by_this_grant() {
    let mut c = setup();
    let mut out = [0; MAX_INPUT_RECORD_BYTES];
    assert_eq!(
        c.action(press(4), &mut out, ClientInstant(0))
            .unwrap()
            .sequence,
        0
    );
    let mut actual = HeldState::empty();
    actual.set_key(key(4), true);
    actual.set_key(key(5), true);
    actual.set_button(PointerButton::Primary, true);
    let r = c
        .reconcile_held(actual, &mut out, ClientInstant(1))
        .unwrap()
        .unwrap();
    let got = parse(&out[..r.bytes]);
    assert!(got.held.key(key(4)));
    assert!(!got.held.key(key(5)));
    assert_eq!(got.held.button_bits(), 0);
    assert_eq!(
        c.action(press(4), &mut out, ClientInstant(2)),
        Err(Error::InvalidTransition)
    );
    assert_eq!(
        c.action(press(5), &mut out, ClientInstant(2))
            .unwrap()
            .sequence,
        1
    );
}
#[test]
fn encoding_failure_does_not_clear_keys_consume_snapshot_or_start_cadence() {
    let mut c = setup();
    let mut out = [0; MAX_INPUT_RECORD_BYTES];
    assert_eq!(
        c.action(press(4), &mut out, ClientInstant(0))
            .unwrap()
            .sequence,
        0
    );
    assert!(
        c.reconcile_held(HeldState::empty(), &mut out[..24], ClientInstant(1))
            .is_err()
    );
    assert_eq!(
        c.action(press(4), &mut out, ClientInstant(1)),
        Err(Error::InvalidTransition)
    );
    let r = c
        .reconcile_held(HeldState::empty(), &mut out, ClientInstant(1))
        .unwrap()
        .unwrap();
    assert_eq!((r.sequence, r.next_action), (0, 1));
    assert!(c.action(press(4), &mut out, ClientInstant(2)).is_ok());
}
#[test]
fn bounded_cadence_does_not_rewrite_pending_bytes_or_consume_input_positions() {
    let mut c = setup();
    let mut out = [0; HELD_STATE_BYTES];
    let first = c
        .reconcile_held(HeldState::empty(), &mut out, ClientInstant(0))
        .unwrap()
        .unwrap();
    let original = out;
    assert!(
        c.reconcile_held(
            HeldState::empty(),
            &mut out,
            ClientInstant(HELD_STATE_INTERVAL_US - 1)
        )
        .unwrap()
        .is_none()
    );
    assert_eq!(original, out);
    let second = c
        .reconcile_held(
            HeldState::empty(),
            &mut out,
            ClientInstant(HELD_STATE_INTERVAL_US),
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        (first.sequence, second.sequence, second.next_action),
        (0, 1, 0)
    );
}
#[test]
fn stopped_or_stale_view_cannot_be_resurrected_by_reconciliation() {
    for reason in [
        StopReason::FocusLost,
        StopReason::Suspended,
        StopReason::Disconnected,
    ] {
        let mut c = setup();
        let mut out = [0x99; HELD_STATE_BYTES];
        c.stop(reason);
        assert_eq!(
            c.reconcile_held(HeldState::empty(), &mut out, ClientInstant(1)),
            Err(Error::Stopped(reason))
        );
        assert_eq!(out, [0x99; HELD_STATE_BYTES]);
    }
    let mut c = setup();
    let mut out = [0; HELD_STATE_BYTES];
    assert_eq!(
        c.reconcile_held(HeldState::empty(), &mut out, ClientInstant(1_500_000)),
        Err(Error::Stopped(StopReason::ViewStale))
    );
}
#[test]
fn full_action_receipt_window_does_not_block_release_only_snapshot() {
    let mut c = setup();
    let mut out = [0; MAX_INPUT_RECORD_BYTES];
    for seq in 0..32 {
        let encoded = c
            .action(
                Action::Key {
                    key: key(4),
                    transition: if seq % 2 == 0 {
                        KeyTransition::Press
                    } else {
                        KeyTransition::Release
                    },
                },
                &mut out,
                ClientInstant(0),
            )
            .unwrap();
        assert_eq!(encoded.sequence, seq);
    }
    assert_eq!(c.pending_actions(), 32);
    let r = c
        .reconcile_held(HeldState::empty(), &mut out, ClientInstant(1))
        .unwrap()
        .unwrap();
    assert_eq!(r.next_action, 32);
    assert_eq!(c.pending_actions(), 32);
    assert_eq!(
        c.action(press(4), &mut out, ClientInstant(2)),
        Err(Error::Backpressure)
    );
}
