use fr_client::input::{
    Action, ClientInstant, Error, InputClient, Policy, PresentedObservation, StopReason,
};
use fr_core::{
    ids::*,
    input::*,
    input_submission::{Capabilities, Capability},
    limits::ProtocolLimits,
};
use fr_media::freshness::{ClockCorrelation, ClockPolicy, ClockSample};
use fr_wire::{
    input::{self, InputDelivery as Delivery, InputDirection as Direction, MAX_INPUT_RECORD_BYTES},
    input_ticket::{self, INPUT_TICKET_BYTES, Ticket},
};
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
fn clock() -> ClockCorrelation {
    ClockCorrelation::new(
        ClockSample {
            host_boot: HostBootId::from_raw(4),
            client_sent_us: 10_000,
            client_received_us: 10_100,
            host_sample_us: 1_000_000,
        },
        ClockPolicy {
            drift_ppm: 0,
            ..ClockPolicy::default()
        },
    )
    .unwrap()
}
fn ticket_wire(id: u128, sequence: u64, issued: u64, expires: u64) -> Vec<u8> {
    let t = Ticket {
        credentials: InputCredentials {
            ticket: InputTicketId::from_raw(id),
            ..credentials()
        },
        sequence,
        issued_at_us: issued,
        expires_at_us: expires,
    };
    let mut b = vec![0; INPUT_TICKET_BYTES];
    input_ticket::encode(
        t,
        &mut b,
        &ProtocolLimits::ABSOLUTE,
        7,
        Direction::HostToViewer,
        Delivery::Reliable,
    )
    .unwrap();
    b
}
fn setup() -> InputClient {
    let c = credentials();
    let mut v = InputClient::new(
        c,
        7,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 100, 100).unwrap(),
        Capabilities::default().with(Capability::Keys),
        ProtocolLimits::ABSOLUTE,
        Policy {
            view_age_us: 1_500_000,
            receipt_timeout_us: 2_000_000,
        },
        ClientInstant(10_100),
    )
    .unwrap();
    v.confirm_mapping(c.session, c.view, ClientInstant(10_100))
        .unwrap();
    v.presented(
        PresentedObservation {
            session: c.session,
            serial: 0,
            view: c.view,
            received_at: ClientInstant(10_100),
            source_age_upper_us: 0,
        },
        ClientInstant(10_100),
    )
    .unwrap();
    v
}
fn press() -> Action<'static> {
    Action::Key {
        key: PhysicalKey::new(4).unwrap(),
        transition: KeyTransition::Press,
    }
}
fn parse(b: &[u8]) -> InputRequest<'_> {
    input::decode_input(
        b,
        &ProtocolLimits::ABSOLUTE,
        7,
        Direction::ViewerToHost,
        Delivery::Reliable,
    )
    .unwrap()
}

fn challenge(scope: fr_wire::authority::Scope, nonce: u128, deadline: u64) -> Vec<u8> {
    let mut out = vec![0; fr_wire::authority::MAX_AUTHORITY_BYTES];
    let n = fr_wire::authority::encode(
        fr_wire::authority::Message::Challenge {
            scope,
            nonce,
            deadline_micros: deadline,
        },
        fr_wire::authority::Binding {
            channel: 11,
            session: credentials().session,
        },
        &ProtocolLimits::ABSOLUTE,
        &mut out,
        Direction::HostToViewer,
        Delivery::Reliable,
    )
    .unwrap();
    out.truncate(n);
    out
}
fn control(nonce: u128, deadline: u64) -> Vec<u8> {
    challenge(
        fr_wire::authority::Scope::Control(credentials().lease),
        nonce,
        deadline,
    )
}
#[test]
fn exact_bound_response_preserves_actions_and_never_refreshes_view() {
    let mut v = setup();
    let now = ClientInstant(10_100);
    v.enable_control_renewal(11, now).unwrap();
    let mut out = [0; MAX_INPUT_RECORD_BYTES];
    let action = v.action(press(), &mut out, now).unwrap();
    assert_eq!(action.sequence, 0);
    assert_eq!(parse(&out[..action.bytes]).credentials, credentials());
    v.accept_control_challenge(&control(8, 4_000_000), now)
        .unwrap();
    assert_eq!(v.pending_actions(), 1);
    let response = v.pending_control_response(now).unwrap().unwrap();
    assert_eq!(
        response.len(),
        fr_wire::authority::OBSERVATION_RESPONSE_BYTES + 16
    );
    assert_eq!(
        fr_wire::authority::decode(
            response,
            fr_wire::authority::Binding {
                channel: 11,
                session: credentials().session
            },
            &ProtocolLimits::ABSOLUTE,
            Direction::ViewerToHost,
            Delivery::Reliable
        )
        .unwrap(),
        fr_wire::authority::Message::Response {
            scope: fr_wire::authority::Scope::Control(credentials().lease),
            nonce: 8
        }
    );
    v.control_response_sent(now).unwrap();
    assert_eq!(v.pending_actions(), 1);
    assert_eq!(
        v.tick(ClientInstant(1_510_100)),
        Err(Error::Stopped(StopReason::ViewStale))
    );
}
#[test]
fn ticket_expiry_does_not_stop_live_lease_response_or_authorize_an_action() {
    let mut v = setup();
    v.enable_control_renewal(11, ClientInstant(10_100)).unwrap();
    v.accept_ticket(
        &ticket_wire(5, 0, 1_000_000, 1_100_000),
        clock(),
        ClientInstant(10_100),
    )
    .unwrap();
    let now = ClientInstant(110_000);
    v.accept_control_challenge(&control(8, 4_000_000), now)
        .unwrap();
    assert!(v.pending_control_response(now).unwrap().is_some());
    v.control_response_sent(now).unwrap();
    assert_eq!(
        v.action(press(), &mut [0; MAX_INPUT_RECORD_BYTES], now),
        Err(Error::TicketExpired)
    );
    assert_eq!(v.pending_actions(), 0);
}
#[test]
fn blocked_response_keeps_exact_bytes_and_fixed_exclusive_deadline() {
    let mut v = setup();
    let now = ClientInstant(10_100);
    v.enable_control_renewal(11, now).unwrap();
    v.accept_control_challenge(&control(8, 4_000_000), now)
        .unwrap();
    let bytes = v.pending_control_response(now).unwrap().unwrap().to_vec();
    assert_eq!(
        v.accept_control_challenge(&control(9, 5_000_000), ClientInstant(20_000)),
        Err(Error::Control(fr_client::authority::Error::Backpressure))
    );
    assert_eq!(
        v.control_response_deadline(),
        Some(ClientInstant(1_010_100))
    );
    assert_eq!(
        v.pending_control_response(ClientInstant(1_010_099))
            .unwrap()
            .unwrap(),
        bytes
    );
    assert_eq!(
        v.tick(ClientInstant(1_010_100)),
        Err(Error::Control(fr_client::authority::Error::Expired))
    );
    assert_eq!(v.control_response_deadline(), None);
    assert!(
        v.enable_control_renewal(11, ClientInstant(1_010_100))
            .is_err()
    );
}
#[test]
fn observation_foreign_lease_malformed_and_replay_cannot_renew_control() {
    let variants = [
        challenge(fr_wire::authority::Scope::Observation, 8, 4_000_000),
        challenge(
            fr_wire::authority::Scope::Control(InputLeaseId::from_raw(99)),
            8,
            4_000_000,
        ),
        vec![0; 82],
    ];
    for wire in variants {
        let mut v = setup();
        let now = ClientInstant(10_100);
        v.enable_control_renewal(11, now).unwrap();
        assert!(v.accept_control_challenge(&wire, now).is_err());
        assert_eq!(v.stopped(), Some(StopReason::InvalidControl));
    }
    for (nonce, deadline) in [(8, 5_000_000), (9, 4_000_000)] {
        let mut v = setup();
        let now = ClientInstant(10_100);
        v.enable_control_renewal(11, now).unwrap();
        v.accept_control_challenge(&control(8, 4_000_000), now)
            .unwrap();
        v.control_response_sent(now).unwrap();
        assert!(
            v.accept_control_challenge(&control(nonce, deadline), now)
                .is_err()
        );
        assert_eq!(v.stopped(), Some(StopReason::InvalidControl));
    }
}
#[test]
fn stop_destroys_response_without_reopening_on_new_challenge() {
    for reason in [
        StopReason::FocusLost,
        StopReason::Suspended,
        StopReason::Disconnected,
        StopReason::ViewChanged,
    ] {
        let mut v = setup();
        let now = ClientInstant(10_100);
        v.enable_control_renewal(11, now).unwrap();
        v.accept_control_challenge(&control(8, 4_000_000), now)
            .unwrap();
        v.stop(reason);
        assert_eq!(v.control_response_deadline(), None);
        assert!(v.pending_control_response(now).is_err());
        assert!(
            v.accept_control_challenge(&control(9, 5_000_000), now)
                .is_err()
        );
        assert_eq!(v.stopped(), Some(reason));
    }
}
#[test]
fn attachment_is_unique_and_does_not_bypass_mapping_or_presentation() {
    let mut v = InputClient::new(
        credentials(),
        7,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 100, 100).unwrap(),
        Capabilities::default(),
        ProtocolLimits::ABSOLUTE,
        Policy::default(),
        ClientInstant(0),
    )
    .unwrap();
    v.enable_control_renewal(11, ClientInstant(0)).unwrap();
    assert_eq!(
        v.accept_control_challenge(&control(8, 4_000_000), ClientInstant(0)),
        Err(Error::MappingUnconfirmed)
    );
    v.confirm_mapping(credentials().session, credentials().view, ClientInstant(0))
        .unwrap();
    assert_eq!(
        v.accept_control_challenge(&control(8, 4_000_000), ClientInstant(0)),
        Err(Error::NoPresentedView)
    );
    assert!(v.enable_control_renewal(12, ClientInstant(0)).is_err());
}
