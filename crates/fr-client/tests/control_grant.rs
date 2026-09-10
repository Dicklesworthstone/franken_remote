use fr_client::{
    control_grant::{Error, RequestControl},
    input::{self, Action, ClientInstant, Policy, PresentedObservation},
};
use fr_core::{
    ids::*,
    input::*,
    input_submission::{Capabilities, Capability},
    limits::ProtocolLimits,
};
use fr_media::freshness::{ClockCorrelation, ClockPolicy, ClockSample};
use fr_wire::{
    control::{self, GRANTED_BYTES, Granted, Request, Target},
    input::{InputDelivery as T, InputDirection as D},
    negotiation::ControlBinding,
};
const L: ProtocolLimits = ProtocolLimits::ABSOLUTE;
fn request() -> Request {
    Request {
        parent: ControlBinding {
            id: 7,
            host_boot: HostBootId::from_raw(1),
            os_session: OsSessionId::from_raw(2),
            remote_session: RemoteSessionId::from_raw(3),
        },
        sequence: 9,
        target: Target {
            display_binding: 8,
            view: InputView {
                geometry: DisplayGeometryGeneration::INITIAL,
                viewport: ViewportMappingGeneration::INITIAL,
                configuration: CodecConfigurationGeneration::INITIAL,
                recovery: RecoveryGeneration::INITIAL,
            },
            bounds: InputBounds::new(DesktopPoint { x: 0, y: 0 }, 100, 100).unwrap(),
            capabilities: Capabilities::default().with(Capability::Keys),
        },
    }
}
fn grant() -> Granted {
    Granted {
        request: request(),
        input_channel: 9,
        lease: InputLeaseId::from_raw(10),
        ticket: InputTicketId::from_raw(11),
        issued_at_us: 1_000_000,
        lease_until_us: 3_000_000,
        ticket_until_us: 2_000_000,
        first_action: 0,
        first_pointer: 0,
    }
}
fn bytes(g: Granted) -> [u8; GRANTED_BYTES] {
    let mut b = [0; GRANTED_BYTES];
    control::encode_granted(g, &mut b, &L, D::HostToViewer, T::Reliable).unwrap();
    b
}
fn clock() -> ClockCorrelation {
    ClockCorrelation::new(
        ClockSample {
            host_boot: request().parent.host_boot,
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
fn setup() -> RequestControl {
    let mut r = RequestControl::new(request(), 9, L, ClientInstant(10_000)).unwrap();
    r.sent(ClientInstant(10_001)).unwrap();
    r
}
fn key() -> Action<'static> {
    Action::Key {
        key: PhysicalKey::new(4).unwrap(),
        transition: KeyTransition::Press,
    }
}
#[test]
fn grant_needs_local_mapping_and_visibility_and_starts_with_bounded_ticket() {
    let mut r = setup();
    let (g, mut input) = r
        .accept(
            &bytes(grant()),
            clock(),
            Policy::default(),
            ClientInstant(10_100),
        )
        .unwrap();
    let mut b = [0; 256];
    assert_eq!(
        input.action(key(), &mut b, ClientInstant(10_100)),
        Err(input::Error::MappingUnconfirmed)
    );
    input
        .confirm_mapping(
            g.credentials().session,
            g.credentials().view,
            ClientInstant(10_100),
        )
        .unwrap();
    assert_eq!(
        input.action(key(), &mut b, ClientInstant(10_100)),
        Err(input::Error::NoPresentedView)
    );
    input
        .presented(
            PresentedObservation {
                session: g.credentials().session,
                serial: 0,
                view: g.credentials().view,
                received_at: ClientInstant(10_100),
                source_age_upper_us: 100,
            },
            ClientInstant(10_100),
        )
        .unwrap();
    assert_eq!(input.ticket_deadline(), Some(ClientInstant(1_010_000)));
    assert_eq!(
        input
            .action(key(), &mut b, ClientInstant(10_100))
            .unwrap()
            .sequence,
        0
    );
    assert!(matches!(
        r.accept(&bytes(g), clock(), Policy::default(), ClientInstant(10_100)),
        Err(Error::Stopped)
    ));
}
#[test]
fn queueing_keeps_request_bytes_and_original_exclusive_deadline() {
    let mut r = RequestControl::new(request(), 9, L, ClientInstant(10_000)).unwrap();
    let original = r.pending(ClientInstant(10_000)).unwrap().unwrap().to_vec();
    assert_eq!(
        r.pending(ClientInstant(900_000)).unwrap(),
        Some(original.as_slice())
    );
    r.sent(ClientInstant(1_900_000)).unwrap();
    assert_eq!(r.deadline(), ClientInstant(2_010_000));
    assert!(matches!(
        r.accept(
            &bytes(grant()),
            clock(),
            Policy::default(),
            ClientInstant(2_010_000)
        ),
        Err(Error::Expired)
    ));
}
#[test]
fn delayed_grant_does_not_reset_ticket_lifetime_and_expired_grant_cannot_retry() {
    let (_, a) = setup()
        .accept(
            &bytes(grant()),
            clock(),
            Policy::default(),
            ClientInstant(10_100),
        )
        .unwrap();
    let (_, b) = setup()
        .accept(
            &bytes(grant()),
            clock(),
            Policy::default(),
            ClientInstant(500_000),
        )
        .unwrap();
    assert_eq!(a.ticket_deadline(), b.ticket_deadline());
    let mut r = setup();
    assert!(matches!(
        r.accept(
            &bytes(grant()),
            clock(),
            Policy::default(),
            ClientInstant(1_010_000)
        ),
        Err(Error::Input(input::Error::TicketExpired))
    ));
    assert!(matches!(
        r.accept(
            &bytes(grant()),
            clock(),
            Policy::default(),
            ClientInstant(1_010_001)
        ),
        Err(Error::Stopped)
    ));
}
#[test]
fn foreign_target_sequence_channel_and_boot_cannot_create_input_owner() {
    for g in [
        Granted {
            request: Request {
                sequence: 10,
                ..request()
            },
            ..grant()
        },
        Granted {
            input_channel: 12,
            ..grant()
        },
        Granted {
            request: Request {
                target: Target {
                    display_binding: 12,
                    ..request().target
                },
                ..request()
            },
            ..grant()
        },
    ] {
        let mut r = setup();
        assert!(matches!(
            r.accept(&bytes(g), clock(), Policy::default(), ClientInstant(10_100)),
            Err(Error::WrongGrant)
        ));
    }
    let other = ClockCorrelation::new(
        ClockSample {
            host_boot: HostBootId::from_raw(100),
            client_sent_us: 10_000,
            client_received_us: 10_100,
            host_sample_us: 1_000_000,
        },
        ClockPolicy::default(),
    )
    .unwrap();
    assert!(matches!(
        setup().accept(
            &bytes(grant()),
            other,
            Policy::default(),
            ClientInstant(10_100)
        ),
        Err(Error::WrongGrant)
    ));
}
#[test]
fn unsent_stopped_regressed_and_malformed_responses_are_terminal() {
    let mut r = RequestControl::new(request(), 9, L, ClientInstant(10_000)).unwrap();
    assert!(matches!(
        r.accept(
            &bytes(grant()),
            clock(),
            Policy::default(),
            ClientInstant(10_100)
        ),
        Err(Error::Order)
    ));
    let mut r = setup();
    r.stop();
    assert!(matches!(
        r.accept(
            &bytes(grant()),
            clock(),
            Policy::default(),
            ClientInstant(10_100)
        ),
        Err(Error::Stopped)
    ));
    let mut r = setup();
    assert_eq!(r.pending(ClientInstant(0)), Err(Error::Clock));
    assert_eq!(r.pending(ClientInstant(10_100)), Err(Error::Stopped));
    let mut r = setup();
    assert!(matches!(
        r.accept(&[0; 24], clock(), Policy::default(), ClientInstant(10_100)),
        Err(Error::Wire(_))
    ));
    assert_eq!(r.pending(ClientInstant(10_100)), Err(Error::Stopped));
}
#[test]
fn first_network_renewal_continues_bootstrap_identity_without_recreating_input() {
    let (g, mut v) = setup()
        .accept(
            &bytes(grant()),
            clock(),
            Policy::default(),
            ClientInstant(10_100),
        )
        .unwrap();
    let mut t = g.initial_ticket();
    t.sequence = 1;
    t.credentials.ticket = InputTicketId::from_raw(12);
    t.issued_at_us += 250_000;
    t.expires_at_us += 250_000;
    let mut b = [0; 128];
    fr_wire::input_ticket::encode(t, &mut b, &L, 9, D::HostToViewer, T::Reliable).unwrap();
    v.accept_ticket(&b, clock(), ClientInstant(260_100))
        .unwrap();
    assert_eq!(v.ticket_deadline(), Some(ClientInstant(1_260_000)));
}
