//! Real X11 capture, followed by the existing viewport action and wire codec.
//! Initial authority/view and clocks are explicit fixtures, not live-tailnet proof.
use super::*;
use fr_core::input_submission::scroll::{LINE, LineScroll, WheelDirection};
use fr_wire::input::{InputDelivery, InputDirection, MAX_INPUT_RECORD_BYTES, decode_input};

#[test]
fn captured_wheel_notches_are_whole_protocol_lines_on_both_axes() {
    let _serial = SERIAL.lock().unwrap();
    let Some((mut peer, display)) = Peer::new() else {
        return;
    };
    let (probe, task) = start(display, peer.window, capabilities());
    peer.command("motion 40 50");
    for button in 4..=7 {
        peer.command(&format!("button {button} 1"));
        peer.command(&format!("button {button} 0"));
    }
    // An ordered key pair is a barrier: all preceding wheel releases must have
    // been processed before the final key release becomes visible to this test.
    peer.command("key 38 1");
    peer.command("key 38 0");
    wait(|| {
        probe.events.lock().unwrap().iter().any(|(event, _)| {
            matches!(
                event,
                Event::Key {
                    transition: KeyTransition::Release,
                    ..
                }
            )
        }) || task.is_finished()
    });
    assert!(!task.is_finished());
    probe.stopped.store(true, Ordering::Release);
    assert_eq!(task.join().unwrap(), Err(StopReason::Closed));
    let events = probe.events.lock().unwrap();
    let scrolls: Vec<_> = events
        .iter()
        .filter_map(|(event, _)| match event {
            Event::Positioned {
                action: PositionedAction::Scroll { x, y, unit },
                ..
            } => Some((*x, *y, *unit)),
            _ => None,
        })
        .collect();
    assert_eq!(LINE, 65_536);
    let expected = [
        (0, -LINE, WheelDirection::Up),
        (0, LINE, WheelDirection::Down),
        (-LINE, 0, WheelDirection::Left),
        (LINE, 0, WheelDirection::Right),
    ];
    assert_eq!(
        scrolls.len(),
        expected.len(),
        "release edges are not extra scrolls"
    );
    let mut client = wheel_client();
    for ((x, y, unit), (expected_x, expected_y, direction)) in scrolls.into_iter().zip(expected) {
        assert_eq!((x, y, unit), (expected_x, expected_y, ScrollUnit::Lines));
        let expansion = LineScroll::new(x, y).expect("one captured notch must be one whole line");
        assert_eq!(expansion.steps().collect::<Vec<_>>(), [direction]);
        assert_eq!(
            expansion.native_operations(),
            3,
            "position + wheel press + release"
        );
        let mut bytes = [0; MAX_INPUT_RECORD_BYTES];
        let encoded = client
            .action(
                PositionedAction::Scroll { x, y, unit }.at(DesktopPoint { x: 40, y: 50 }),
                &mut bytes,
                ClientInstant(0),
            )
            .unwrap();
        let decoded = decode_input(
            &bytes[..encoded.bytes],
            &ProtocolLimits::ABSOLUTE,
            7,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )
        .unwrap();
        assert!(
            matches!(decoded.event, InputEvent::Scroll { x: dx, y: dy, unit: ScrollUnit::Lines, .. }
            if dx == expected_x && dy == expected_y)
        );
    }
}

fn wheel_client() -> InputClient {
    let credentials = InputCredentials {
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
        credentials,
        7,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
        capabilities(),
        ProtocolLimits::ABSOLUTE,
        Policy::default(),
        ClientInstant(0),
    )
    .unwrap();
    client
        .confirm_mapping(credentials.session, credentials.view, ClientInstant(0))
        .unwrap();
    client
        .presented(
            fr_client::input::PresentedObservation {
                session: credentials.session,
                serial: 0,
                view: credentials.view,
                received_at: ClientInstant(0),
                source_age_upper_us: 0,
            },
            ClientInstant(0),
        )
        .unwrap();
    client
}

#[test]
fn unnegotiated_wheel_is_ignored_without_consuming_key_actions() {
    let _serial = SERIAL.lock().unwrap();
    let Some((mut peer, display)) = Peer::new() else {
        return;
    };
    let caps = Capabilities::default()
        .with(Capability::Keys)
        .with(Capability::Repeat)
        .with(Capability::Absolute)
        .with(Capability::Buttons);
    let (probe, task) = start(display, peer.window, caps);
    for button in 4..=7 {
        peer.command(&format!("button {button} 1"));
        peer.command(&format!("button {button} 0"));
    }
    peer.command("key 38 1");
    peer.command("key 38 0");
    wait(|| {
        probe.events.lock().unwrap().iter().any(|(event, _)| {
            matches!(
                event,
                Event::Key {
                    transition: KeyTransition::Release,
                    ..
                }
            )
        }) || task.is_finished()
    });
    assert!(!task.is_finished());
    probe.stopped.store(true, Ordering::Release);
    assert_eq!(task.join().unwrap(), Err(StopReason::Closed));
    let events = probe.events.lock().unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|(e, _)| matches!(e, Event::Key { .. }))
            .count(),
        2
    );
    assert!(!events.iter().any(|(e, _)| matches!(
        e,
        Event::Positioned {
            action: PositionedAction::Scroll { .. },
            ..
        }
    )));
}
