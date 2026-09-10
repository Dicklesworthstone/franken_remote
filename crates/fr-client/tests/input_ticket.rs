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
fn wire(id: u128, sequence: u64, issued: u64, expires: u64) -> Vec<u8> {
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
#[test]
fn delayed_receipt_subtracts_age_instead_of_starting_a_new_lifetime() {
    let b = wire(5, 0, 1_000_000, 2_000_000);
    let mut a = setup();
    let mut delayed = setup();
    a.accept_ticket(&b, clock(), ClientInstant(10_100)).unwrap();
    delayed
        .accept_ticket(&b, clock(), ClientInstant(310_100))
        .unwrap();
    assert_eq!(a.ticket_deadline(), Some(ClientInstant(1_010_000)));
    assert_eq!(a.ticket_deadline(), delayed.ticket_deadline());
    let mut out = [0; MAX_INPUT_RECORD_BYTES];
    let n = delayed
        .action(press(), &mut out, ClientInstant(310_101))
        .unwrap();
    assert_eq!(
        parse(&out[..n.bytes]).credentials.ticket,
        InputTicketId::from_raw(5)
    );
}
#[test]
fn expiry_pauses_without_consuming_identity_and_renewal_keeps_pending_receipts() {
    let mut v = setup();
    let mut out = [0; MAX_INPUT_RECORD_BYTES];
    let first = v.action(press(), &mut out, ClientInstant(10_100)).unwrap();
    assert_eq!(first.sequence, 0);
    v.accept_ticket(
        &wire(5, 0, 1_000_000, 1_100_000),
        clock(),
        ClientInstant(10_101),
    )
    .unwrap();
    let release = Action::Key {
        key: PhysicalKey::new(4).unwrap(),
        transition: KeyTransition::Release,
    };
    assert_eq!(
        v.action(release, &mut out, ClientInstant(110_000)),
        Err(Error::TicketExpired)
    );
    assert_eq!(v.pending_actions(), 1);
    assert_eq!(v.stopped(), None);
    v.accept_ticket(
        &wire(6, 1, 1_100_000, 1_900_000),
        clock(),
        ClientInstant(110_000),
    )
    .unwrap();
    let next = v.action(release, &mut out, ClientInstant(110_001)).unwrap();
    assert_eq!(next.sequence, 1);
    assert_eq!(v.pending_actions(), 2);
    assert_eq!(
        parse(&out[..next.bytes]).credentials.ticket,
        InputTicketId::from_raw(6)
    );
    assert_eq!(
        v.ticket(InputTicketId::from_raw(9), ClientInstant(110_002)),
        Err(Error::Stopped(StopReason::InvalidTicket))
    );
}
#[test]
fn replay_and_foreign_generations_cannot_reset_an_expired_ticket() {
    for offset in [24, 40, 72, 80, 88, 96] {
        let mut v = setup();
        let mut b = wire(5, 0, 1_000_000, 1_100_000);
        b[offset] ^= 0x01;
        assert!(v.accept_ticket(&b, clock(), ClientInstant(10_100)).is_err());
        assert_eq!(v.stopped(), Some(StopReason::InvalidTicket));
    }
    let b = wire(5, 0, 1_000_000, 1_100_000);
    let mut v = setup();
    assert_eq!(
        v.accept_ticket(&b, clock(), ClientInstant(110_000)),
        Err(Error::TicketExpired)
    );
    assert_eq!(v.stopped(), None);
    assert_eq!(
        v.accept_ticket(&b, clock(), ClientInstant(110_001)),
        Err(Error::Stopped(StopReason::InvalidTicket))
    );
}
#[test]
fn hidden_unfocused_and_stale_views_cannot_be_reopened_with_a_ticket() {
    for stop in [
        StopReason::FocusLost,
        StopReason::Suspended,
        StopReason::Disconnected,
        StopReason::ViewStale,
    ] {
        let mut v = setup();
        v.stop(stop);
        assert_eq!(
            v.accept_ticket(
                &wire(5, 0, 1_000_000, 2_000_000),
                clock(),
                ClientInstant(10_100)
            ),
            Err(Error::Stopped(stop))
        );
    }
    let c = credentials();
    let mut v = InputClient::new(
        c,
        7,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 100, 100).unwrap(),
        Capabilities::default().with(Capability::Keys),
        ProtocolLimits::ABSOLUTE,
        Policy::default(),
        ClientInstant(10_100),
    )
    .unwrap();
    v.accept_ticket(
        &wire(5, 0, 1_000_000, 2_000_000),
        clock(),
        ClientInstant(10_100),
    )
    .unwrap();
    assert_eq!(
        v.action(
            press(),
            &mut [0; MAX_INPUT_RECORD_BYTES],
            ClientInstant(10_101)
        ),
        Err(Error::MappingUnconfirmed)
    );
    v.confirm_mapping(c.session, c.view, ClientInstant(10_101))
        .unwrap();
    assert_eq!(
        v.action(
            press(),
            &mut [0; MAX_INPUT_RECORD_BYTES],
            ClientInstant(10_101)
        ),
        Err(Error::NoPresentedView)
    );
}
#[test]
fn corrupt_body_future_issue_or_wrong_boot_is_terminal() {
    let mut bad = wire(5, 0, 1_000_000, 2_000_000);
    bad.pop();
    let mut v = setup();
    assert!(
        v.accept_ticket(&bad, clock(), ClientInstant(10_100))
            .is_err()
    );
    assert!(v.stopped().is_some());
    let mut v = setup();
    assert!(
        v.accept_ticket(
            &wire(5, 0, 1_001_000, 2_000_000),
            clock(),
            ClientInstant(10_100)
        )
        .is_err()
    );
    let mut v = setup();
    v.accept_ticket(
        &wire(5, 0, 1_000_000, 2_000_000),
        clock(),
        ClientInstant(10_100),
    )
    .unwrap();
    let wrong = ClockCorrelation::new(
        ClockSample {
            host_boot: HostBootId::from_raw(8),
            client_sent_us: 10_100,
            client_received_us: 10_101,
            host_sample_us: 1_000_100,
        },
        ClockPolicy::default(),
    )
    .unwrap();
    assert!(
        v.accept_ticket(
            &wire(6, 1, 1_000_100, 2_000_000),
            wrong,
            ClientInstant(10_101)
        )
        .is_err()
    );
    assert_eq!(v.stopped(), Some(StopReason::InvalidTicket));
}

#[test]
fn initial_matching_ticket_is_bounded_once_and_cannot_be_retimed_by_replay() {
    let mut v = setup();
    let initial = wire(3, 0, 1_000_000, 1_100_000);
    v.accept_ticket(&initial, clock(), ClientInstant(10_100))
        .unwrap();
    assert_eq!(v.ticket_deadline(), Some(ClientInstant(110_000)));
    assert_eq!(
        v.accept_ticket(&initial, clock(), ClientInstant(10_101)),
        Err(Error::Stopped(StopReason::InvalidTicket))
    );
    assert_eq!(v.stopped(), Some(StopReason::InvalidTicket));
}
