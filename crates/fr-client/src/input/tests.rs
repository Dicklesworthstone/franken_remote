use super::*;
use fr_core::ids::*;
use fr_wire::input::MAX_INPUT_RECORD_BYTES;
fn client() -> InputClient {
    let view = InputView {
        geometry: DisplayGeometryGeneration::INITIAL,
        viewport: ViewportMappingGeneration::INITIAL,
        configuration: CodecConfigurationGeneration::INITIAL,
        recovery: RecoveryGeneration::INITIAL,
    };
    let session = RemoteSessionId::from_raw(1);
    let c = InputCredentials {
        session,
        lease: InputLeaseId::from_raw(2),
        ticket: InputTicketId::from_raw(3),
        view,
    };
    let mut client = InputClient::new(
        c,
        7,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 100, 100).unwrap(),
        Capabilities::default()
            .with(Capability::Text)
            .with(Capability::Absolute)
            .with(Capability::Buttons),
        ProtocolLimits::ABSOLUTE,
        Policy::default(),
        ClientInstant(0),
    )
    .unwrap();
    client
        .confirm_mapping(session, view, ClientInstant(0))
        .unwrap();
    client
        .presented(
            PresentedObservation {
                session,
                serial: 0,
                view,
                received_at: ClientInstant(0),
                source_age_upper_us: 0,
            },
            ClientInstant(0),
        )
        .unwrap();
    client
}
#[test]
fn reliable_and_pointer_counters_emit_the_last_identity_but_never_wrap() {
    let mut c = client();
    let mut out = [0; MAX_INPUT_RECORD_BYTES];
    c.next_action = Some(u64::MAX);
    assert_eq!(
        c.action(Action::Text("x"), &mut out, ClientInstant(0))
            .unwrap()
            .sequence,
        u64::MAX
    );
    assert_eq!(
        c.action(Action::Text("x"), &mut out, ClientInstant(0)),
        Err(Error::Stopped(StopReason::CounterExhausted))
    );
    let mut c = client();
    c.next_pointer = Some(u64::MAX);
    let p = DesktopPoint { x: 1, y: 1 };
    assert_eq!(
        c.pointer(p, &mut out, ClientInstant(0)).unwrap().sequence,
        u64::MAX
    );
    assert_eq!(
        c.pointer(p, &mut out, ClientInstant(0)),
        Err(Error::Stopped(StopReason::CounterExhausted))
    );
}
#[test]
fn click_barrier_exhaustion_cannot_revalidate_an_old_pointer_sequence() {
    let mut c = client();
    c.next_pointer = Some(u64::MAX);
    let mut out = [0; MAX_INPUT_RECORD_BYTES];
    let p = DesktopPoint { x: 1, y: 1 };
    let encoded = c
        .action(
            Action::Button {
                button: PointerButton::Primary,
                pressed: true,
                position: p,
            },
            &mut out,
            ClientInstant(0),
        )
        .unwrap();
    assert_eq!(encoded.sequence, 0);
    assert!(c.next_pointer.is_none());
    assert_eq!(
        c.pointer(p, &mut out, ClientInstant(0)),
        Err(Error::Stopped(StopReason::CounterExhausted))
    );
}
#[test]
fn deadline_overflow_is_terminal_and_metadata_storage_is_fixed() {
    let mut c = client();
    c.clock = ClientInstant(u64::MAX - 1);
    c.view_until = None;
    assert_eq!(
        c.presented(
            PresentedObservation {
                session: c.credentials.session,
                serial: 1,
                view: c.credentials.view,
                received_at: c.clock,
                source_age_upper_us: 0
            },
            c.clock
        ),
        Err(Error::Stopped(StopReason::CounterExhausted))
    );
    assert!(std::mem::size_of::<InputClient>() < 8 * 1024);
}
