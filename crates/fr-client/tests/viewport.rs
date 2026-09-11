use fr_client::input::{
    ClientInstant, InputClient, Policy, PresentedObservation, StopReason, viewport::*,
};
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::*,
    input::*,
    input_submission::*,
    limits::ProtocolLimits,
    time::HostInstant,
};
use fr_wire::input::{InputDelivery, InputDirection, MAX_INPUT_RECORD_BYTES, decode_input};

fn bounds(x: i32, y: i32, w: u32, h: u32) -> InputBounds {
    InputBounds::new(DesktopPoint { x, y }, w, h).unwrap()
}
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
fn caps() -> Capabilities {
    Capabilities::default()
        .with(Capability::Absolute)
        .with(Capability::Buttons)
        .with(Capability::LineScroll)
}
fn client_with(creds: InputCredentials, desktop: InputBounds) -> InputClient {
    InputClient::new(
        creds,
        7,
        desktop,
        caps(),
        ProtocolLimits::ABSOLUTE,
        Policy::default(),
        ClientInstant(0),
    )
    .unwrap()
}
fn ready(client: &mut InputClient, creds: InputCredentials) {
    client
        .confirm_mapping(creds.session, creds.view, ClientInstant(0))
        .unwrap();
    client
        .presented(
            PresentedObservation {
                session: creds.session,
                view: creds.view,
                serial: 0,
                received_at: ClientInstant(0),
                source_age_upper_us: 0,
            },
            ClientInstant(0),
        )
        .unwrap();
}

#[test]
fn aspect_fit_reports_exact_odd_letterbox_and_rejects_all_exclusive_edges() {
    let desktop = bounds(-1920, -100, 1920, 1080);
    let mut viewport = client_with(credentials(), desktop).viewport();
    let layout = viewport
        .configure(desktop, SurfaceRect::new(20, 30, 1200, 800).unwrap())
        .unwrap();
    assert_eq!(
        layout.destination(),
        SurfaceRect::new(20, 92, 1200, 675).unwrap()
    );
    let center = layout.at(LocalPoint::pixels(620, 429));
    assert_eq!(viewport.map(&center), Err(Error::Unconfirmed));
    viewport.confirm_layout(&layout).unwrap();
    assert_eq!(
        viewport
            .map(&layout.at(LocalPoint::pixels(20, 92)))
            .unwrap(),
        DesktopPoint { x: -1920, y: -100 }
    );
    assert_eq!(
        viewport
            .map(&layout.at(LocalPoint::subpixels(1220 * 256 - 1, 767 * 256 - 1)))
            .unwrap(),
        DesktopPoint { x: -1, y: 979 }
    );
    for (x, y) in [
        (19, 92),
        (20, 91),
        (1220, 92),
        (20, 767),
        (0, 0),
        (620, 890),
    ] {
        assert_eq!(
            viewport.map(&layout.at(LocalPoint::pixels(x, y))),
            Err(Error::OutsideImage)
        );
    }
}

#[test]
fn dpi_conversion_is_fractional_checked_and_keeps_negative_edges_outside() {
    let desktop = bounds(-3840, -2160, 3840, 2160);
    let mut viewport = client_with(credentials(), desktop).viewport();
    let layout = viewport
        .configure(desktop, SurfaceRect::new(0, 0, 1920, 1080).unwrap())
        .unwrap();
    viewport.confirm_layout(&layout).unwrap();
    for (n, d, x, y) in [
        (3, 2, -3240, -1860),
        (2, 1, -3040, -1760),
        (5, 4, -3340, -1910),
    ] {
        let point = LocalPoint::logical(200 * 256, 100 * 256, n, d).unwrap();
        assert_eq!(
            viewport.map(&layout.at(point)).unwrap(),
            DesktopPoint { x, y }
        );
    }
    let negative = LocalPoint::logical(-1, 0, 1, 2).unwrap();
    assert_eq!(viewport.map(&layout.at(negative)), Err(Error::OutsideImage));
    assert_eq!(
        LocalPoint::logical(0, 0, 1, 0).err(),
        Some(Error::InvalidScale)
    );
    assert_eq!(
        LocalPoint::logical(0, 0, 0, 1).err(),
        Some(Error::InvalidScale)
    );
    assert_eq!(
        LocalPoint::logical(i64::MAX, 0, u32::MAX, 1).err(),
        Some(Error::Overflow)
    );
}

#[test]
fn local_zoom_selects_only_existing_pixels_without_changing_host_generations() {
    let desktop = bounds(-1920, 0, 1920, 1080);
    let zoom = bounds(-1700, 100, 800, 400);
    let mut creds = client_with(credentials(), desktop);
    ready(&mut creds, credentials());
    let mut viewport = creds.viewport();
    let layout = viewport
        .configure(zoom, SurfaceRect::new(0, 0, 1000, 600).unwrap())
        .unwrap();
    assert_eq!(layout.source(), zoom);
    assert_eq!(
        layout.destination(),
        SurfaceRect::new(0, 50, 1000, 500).unwrap()
    );
    viewport.confirm_layout(&layout).unwrap();
    let mut out = [0; MAX_INPUT_RECORD_BYTES];
    let encoded = creds
        .pointer_on(
            &viewport,
            &layout.at(LocalPoint::pixels(500, 300)),
            &mut out,
            ClientInstant(1),
        )
        .unwrap();
    let record = decode_input(
        &out[..encoded.bytes],
        &ProtocolLimits::ABSOLUTE,
        7,
        InputDirection::ViewerToHost,
        InputDelivery::Datagram,
    )
    .unwrap();
    assert_eq!(record.credentials, credentials());
    assert_eq!(
        record.event,
        InputEvent::Pointer {
            position: DesktopPoint { x: -1300, y: 300 }
        }
    );
}

#[test]
fn resize_and_same_value_reconfiguration_retire_old_events_and_callbacks() {
    let desktop = bounds(0, 0, 1280, 720);
    let creds = client_with(credentials(), desktop);
    let mut viewport = creds.viewport();
    let area = SurfaceRect::new(0, 0, 1280, 720).unwrap();
    let old = viewport.configure(desktop, area).unwrap();
    viewport.confirm_layout(&old).unwrap();
    let event = old.at(LocalPoint::pixels(100, 100));
    let new = viewport.configure(desktop, area).unwrap();
    assert_eq!(viewport.map(&event), Err(Error::Obsolete));
    assert_eq!(viewport.confirm_layout(&old), Err(Error::Obsolete));
    assert_eq!(
        viewport.map(&new.at(LocalPoint::pixels(100, 100))),
        Err(Error::Unconfirmed)
    );
    viewport.confirm_layout(&new).unwrap();
    assert_eq!(viewport.map(&event), Err(Error::Obsolete));
    let mut other = creds.viewport();
    let unrelated = other.configure(desktop, area).unwrap();
    assert_eq!(viewport.confirm_layout(&unrelated), Err(Error::Obsolete));
    viewport.invalidate();
    assert_eq!(viewport.confirm_layout(&new), Err(Error::Obsolete));
    viewport.stop();
    assert_eq!(
        viewport.configure(desktop, area).err(),
        Some(Error::Stopped)
    );
}

#[test]
fn invalid_zoom_retires_previous_layout_instead_of_clamping_or_reusing_it() {
    let desktop = bounds(-100, -50, 200, 100);
    let mut viewport = client_with(credentials(), desktop).viewport();
    let area = SurfaceRect::new(0, 0, 200, 100).unwrap();
    for bad in [
        bounds(-101, -50, 200, 100),
        bounds(-100, -51, 200, 100),
        bounds(-100, -50, 201, 100),
        bounds(-100, -50, 200, 101),
    ] {
        let old = viewport.configure(desktop, area).unwrap();
        viewport.confirm_layout(&old).unwrap();
        assert_eq!(
            viewport.configure(bad, area).err(),
            Some(Error::OutsideGrantedDisplay)
        );
        assert_eq!(
            viewport.map(&old.at(LocalPoint::pixels(10, 10))),
            Err(Error::Obsolete)
        );
    }
    assert_eq!(SurfaceRect::new(0, 0, 0, 1).err(), Some(Error::InvalidArea));
    assert_eq!(
        SurfaceRect::new(i32::MAX, 0, 2, 1).err(),
        Some(Error::InvalidArea)
    );
}

#[test]
fn extreme_rectangles_and_points_are_checked_without_overflow() {
    let desktop = bounds(i32::MIN, i32::MIN, u32::MAX, u32::MAX);
    let mut viewport = client_with(credentials(), desktop).viewport();
    let area = SurfaceRect::new(i32::MIN, i32::MIN, u32::MAX, u32::MAX).unwrap();
    let layout = viewport.configure(desktop, area).unwrap();
    viewport.confirm_layout(&layout).unwrap();
    for point in [
        DesktopPoint {
            x: i32::MIN,
            y: i32::MIN,
        },
        DesktopPoint { x: 0, y: 0 },
        DesktopPoint {
            x: i32::MAX - 1,
            y: i32::MAX - 1,
        },
    ] {
        assert_eq!(
            viewport
                .map(&layout.at(LocalPoint::pixels(point.x, point.y)))
                .unwrap(),
            point
        );
    }
    for x in [i64::MIN, i64::MAX] {
        assert_eq!(
            viewport.map(&layout.at(LocalPoint::subpixels(x, 0))),
            Err(Error::OutsideImage)
        );
    }
    let narrow = bounds(i32::MIN, i32::MIN, u32::MAX, 1);
    assert_eq!(
        viewport
            .configure(narrow, SurfaceRect::new(0, 0, 1, 1).unwrap())
            .err(),
        Some(Error::InvalidArea)
    );
}

#[test]
fn layout_confirmation_is_neither_host_acknowledgement_nor_freshness() {
    let creds = credentials();
    let desktop = bounds(0, 0, 100, 100);
    let mut input = client_with(creds, desktop);
    let mut viewport = input.viewport();
    let layout = viewport
        .configure(desktop, SurfaceRect::new(0, 0, 100, 100).unwrap())
        .unwrap();
    viewport.confirm_layout(&layout).unwrap();
    let point = layout.at(LocalPoint::pixels(50, 50));
    let mut out = [0; MAX_INPUT_RECORD_BYTES];
    assert_eq!(
        input.pointer_on(&viewport, &point, &mut out, ClientInstant(0)),
        Err(Error::Input(fr_client::input::Error::MappingUnconfirmed))
    );
    input
        .confirm_mapping(creds.session, creds.view, ClientInstant(0))
        .unwrap();
    assert_eq!(
        input.pointer_on(&viewport, &point, &mut out, ClientInstant(0)),
        Err(Error::Input(fr_client::input::Error::NoPresentedView))
    );
    ready(&mut input, creds);
    assert_eq!(
        input.pointer_on(&viewport, &point, &mut out, ClientInstant(250_000)),
        Err(Error::Input(fr_client::input::Error::Stopped(
            StopReason::ViewStale
        )))
    );
}

#[test]
fn mismatched_grant_or_viewport_never_consumes_an_input_identity() {
    let desktop = bounds(0, 0, 100, 100);
    let original = client_with(credentials(), desktop);
    let mut viewport = original.viewport();
    let layout = viewport
        .configure(desktop, SurfaceRect::new(0, 0, 100, 100).unwrap())
        .unwrap();
    viewport.confirm_layout(&layout).unwrap();
    for variant in 0..4 {
        let mut creds = credentials();
        let mut selected = desktop;
        match variant {
            0 => creds.session = RemoteSessionId::from_raw(99),
            1 => creds.lease = InputLeaseId::from_raw(99),
            2 => creds.view.geometry = DisplayGeometryGeneration::from_raw(99),
            _ => selected = bounds(0, 0, 101, 100),
        }
        let mut input = client_with(creds, selected);
        ready(&mut input, creds);
        let mut out = [0; MAX_INPUT_RECORD_BYTES];
        assert_eq!(
            input.pointer_on(
                &viewport,
                &layout.at(LocalPoint::pixels(50, 50)),
                &mut out,
                ClientInstant(1)
            ),
            Err(Error::WrongInput)
        );
        assert_eq!(
            input
                .pointer(DesktopPoint { x: 50, y: 50 }, &mut out, ClientInstant(1))
                .unwrap()
                .sequence,
            0
        );
    }
}

#[derive(Default)]
struct Sink(Vec<Operation>);
impl InputSink for Sink {
    fn prepare(&mut self, _: Operation) -> Result<(), PlatformError> {
        Ok(())
    }
    fn submit(&mut self, op: Operation) -> Submission {
        self.0.push(op);
        Submission::Submitted
    }
}

fn host_session(creds: InputCredentials, desktop: InputBounds) -> InputSession {
    let mut authority = SessionAuthority::new(creds.session, AuthorityPolicy::plan_defaults());
    authority.mark_capabilities_checked().unwrap();
    authority
        .authorize_observation(HostInstant::ORIGIN)
        .unwrap();
    authority.mark_view_ready(HostInstant::ORIGIN).unwrap();
    authority
        .grant_lease(creds.lease, HostInstant::ORIGIN)
        .unwrap();
    authority
        .issue_input_ticket(creds.lease, creds.ticket, HostInstant::ORIGIN)
        .unwrap();
    InputSession::new(authority, creds, desktop, caps(), HostInstant::ORIGIN).unwrap()
}
fn submit<'a>(
    host: &mut InputSession,
    sink: &mut Sink,
    bytes: &'a [u8],
    delivery: InputDelivery,
) -> InputEvent<'a> {
    let record = decode_input(
        bytes,
        &ProtocolLimits::ABSOLUTE,
        7,
        InputDirection::ViewerToHost,
        delivery,
    )
    .unwrap();
    assert!(matches!(
        host.dispatch(record, sink, || HostInstant::ORIGIN).unwrap(),
        Dispatch::Completed(receipt)
            if receipt.outcome == fr_core::input_sequence::InputOutcome::SubmittedToOs
    ));
    record.event
}
#[test]
fn transformed_pointer_button_and_scroll_keep_wire_barriers_and_native_coordinates() {
    let creds = credentials();
    let desktop = bounds(-1920, 100, 1920, 1080);
    let mut input = client_with(creds, desktop);
    ready(&mut input, creds);
    let mut viewport = input.viewport();
    let layout = viewport
        .configure(desktop, SurfaceRect::new(30, 20, 960, 640).unwrap())
        .unwrap();
    viewport.confirm_layout(&layout).unwrap();
    let mut host = host_session(creds, desktop);
    let mut sink = Sink::default();
    let mut out = [0; MAX_INPUT_RECORD_BYTES];
    let pointer = input
        .pointer_on(
            &viewport,
            &layout.at(LocalPoint::pixels(130, 170)),
            &mut out,
            ClientInstant(1),
        )
        .unwrap();
    assert_eq!(
        submit(
            &mut host,
            &mut sink,
            &out[..pointer.bytes],
            InputDelivery::Datagram
        ),
        InputEvent::Pointer {
            position: DesktopPoint { x: -1720, y: 300 }
        }
    );
    assert_eq!(
        input.action_on(
            &viewport,
            &layout.at(LocalPoint::pixels(0, 0)),
            PositionedAction::Button {
                button: PointerButton::Primary,
                pressed: true
            },
            &mut out,
            ClientInstant(1)
        ),
        Err(Error::OutsideImage)
    );
    let press = input
        .action_on(
            &viewport,
            &layout.at(LocalPoint::pixels(510, 340)),
            PositionedAction::Button {
                button: PointerButton::Primary,
                pressed: true,
            },
            &mut out,
            ClientInstant(2),
        )
        .unwrap();
    assert_eq!(press.sequence, 0);
    assert_eq!(
        submit(
            &mut host,
            &mut sink,
            &out[..press.bytes],
            InputDelivery::Reliable
        ),
        InputEvent::Button {
            button: PointerButton::Primary,
            pressed: true,
            position: DesktopPoint { x: -960, y: 640 },
            barrier: 1
        }
    );
    assert_scroll(&mut input, &viewport, &layout, &mut host, &mut sink);
}
fn assert_scroll(
    input: &mut InputClient,
    viewport: &Viewport,
    layout: &Layout,
    host: &mut InputSession,
    sink: &mut Sink,
) {
    let mut out = [0; MAX_INPUT_RECORD_BYTES];
    let scroll = input
        .action_on(
            viewport,
            &layout.at(LocalPoint::pixels(510, 340)),
            PositionedAction::Scroll {
                x: 0,
                y: -3,
                unit: ScrollUnit::Lines,
            },
            &mut out,
            ClientInstant(3),
        )
        .unwrap();
    assert_eq!(scroll.sequence, 1);
    assert_eq!(
        submit(host, sink, &out[..scroll.bytes], InputDelivery::Reliable),
        InputEvent::Scroll {
            position: DesktopPoint { x: -960, y: 640 },
            barrier: 2,
            x: 0,
            y: -3,
            unit: ScrollUnit::Lines
        }
    );
    assert!(
        sink.0
            .contains(&Operation::Absolute(DesktopPoint { x: -960, y: 640 }))
    );
}

#[test]
fn equal_numeric_regrant_cannot_reuse_the_previous_input_owners_layout() {
    let desktop = bounds(0, 0, 100, 100);
    let old = client_with(credentials(), desktop);
    let mut viewport = old.viewport();
    let layout = viewport
        .configure(desktop, SurfaceRect::new(0, 0, 100, 100).unwrap())
        .unwrap();
    viewport.confirm_layout(&layout).unwrap();
    let mut replacement = client_with(credentials(), desktop);
    ready(&mut replacement, credentials());
    let mut out = [0; MAX_INPUT_RECORD_BYTES];
    assert_eq!(
        replacement.pointer_on(
            &viewport,
            &layout.at(LocalPoint::pixels(50, 50)),
            &mut out,
            ClientInstant(1)
        ),
        Err(Error::WrongInput)
    );
    assert_eq!(
        replacement
            .pointer(DesktopPoint { x: 50, y: 50 }, &mut out, ClientInstant(1))
            .unwrap()
            .sequence,
        0
    );
}
