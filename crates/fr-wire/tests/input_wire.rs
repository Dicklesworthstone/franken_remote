use fr_core::{
    ids::*,
    input::*,
    limits::{LimitOverrides, ProtocolLimits},
};
use fr_wire::{WireError, input::*};
fn credentials() -> InputCredentials {
    InputCredentials {
        session: RemoteSessionId::from_raw(1),
        lease: InputLeaseId::from_raw(2),
        ticket: InputTicketId::from_raw(3),
        view: InputView {
            geometry: DisplayGeometryGeneration::from_raw(4),
            viewport: ViewportMappingGeneration::from_raw(5),
            configuration: CodecConfigurationGeneration::from_raw(6),
            recovery: RecoveryGeneration::from_raw(7),
        },
    }
}
fn request(event: InputEvent<'_>) -> InputRequest<'_> {
    InputRequest {
        credentials: credentials(),
        sequence: 8,
        event,
    }
}
fn hex(s: &str) -> Vec<u8> {
    s.trim()
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| u8::from_str_radix(core::str::from_utf8(b).unwrap(), 16).unwrap())
        .collect()
}
fn fixtures() -> Vec<(InputEvent<'static>, Vec<u8>)> {
    let position = DesktopPoint { x: -120, y: 45 };
    vec![
        (
            InputEvent::Key {
                key: PhysicalKey::new(4).unwrap(),
                transition: KeyTransition::Press,
            },
            hex(include_str!("fixtures/input/key_page_usage.hex")),
        ),
        (
            InputEvent::Button {
                button: PointerButton::Secondary,
                pressed: true,
                position,
                barrier: 99,
            },
            hex(include_str!("fixtures/input/button.hex")),
        ),
        (
            InputEvent::Pointer { position },
            hex(include_str!("fixtures/input/pointer.hex")),
        ),
        (
            InputEvent::Relative {
                mode_epoch: 11,
                cumulative_x: -1000,
                cumulative_y: 2000,
            },
            hex(include_str!("fixtures/input/relative.hex")),
        ),
        (
            InputEvent::Scroll {
                position,
                barrier: 99,
                x: -1,
                y: 2,
                unit: ScrollUnit::Lines,
            },
            hex(include_str!("fixtures/input/scroll.hex")),
        ),
        (
            InputEvent::Text("hé🙂"),
            hex(include_str!("fixtures/input/text.hex")),
        ),
        (
            InputEvent::Mode {
                mode: PointerMode::Relative,
                epoch: 11,
            },
            hex(include_str!("fixtures/input/mode.hex")),
        ),
    ]
}
fn decode(bytes: &[u8]) -> Result<InputRequest<'_>, WireError> {
    decode_input(
        bytes,
        &ProtocolLimits::ABSOLUTE,
        9,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
}
#[test]
fn independent_golden_records_are_exact_and_text_is_borrowed() {
    for (event, expected) in fixtures() {
        let mut out = [0; MAX_INPUT_RECORD_BYTES];
        let n = encode_input(
            request(event),
            &mut out,
            &ProtocolLimits::ABSOLUTE,
            9,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )
        .unwrap();
        assert_eq!(&out[..n], expected);
        assert_eq!(decode(&expected).unwrap(), request(event));
        if let InputEvent::Text(text) = decode(&expected).unwrap().event {
            assert_eq!(text.as_ptr(), expected[116..].as_ptr());
        }
    }
}
#[test]
fn every_truncation_and_trailing_byte_refuses() {
    for (_, mut b) in fixtures() {
        for n in 0..b.len() {
            assert!(decode(&b[..n]).is_err(), "length={n}");
        }
        b.push(0);
        assert_eq!(decode(&b), Err(WireError::TrailingBytes));
    }
}

#[test]
fn physical_keys_require_the_explicit_keyboard_page() {
    let mut bytes = hex(include_str!("fixtures/input/key_page_usage.hex"));
    assert_eq!(bytes.len(), 117);
    assert_eq!(&bytes[12..16], &93_u32.to_be_bytes());
    assert_eq!(&bytes[112..], &[0, 7, 0, 4, 1]);
    // The page is a full big-endian u16, not a truncated byte or an inferred
    // default. Every other page must refuse even when the usage is admitted.
    for page in 0..=u16::MAX {
        bytes[112..114].copy_from_slice(&page.to_be_bytes());
        if page == 7 {
            assert_eq!(decode(&bytes).unwrap(), request(fixtures()[0].0));
        } else {
            assert_eq!(decode(&bytes), Err(WireError::InvalidValue));
        }
    }
}

#[test]
fn legacy_implicit_page_records_remain_rejected_evidence() {
    // Original d9f8ea7 bytes are intentionally retained unchanged, not
    // regenerated. No admitted legacy key may be misread as a new record.
    let mut legacy = hex(include_str!("fixtures/input/key.hex"));
    assert_eq!(legacy.len(), 115);
    assert_eq!(&legacy[112..], &[0, 4, 1]);
    assert_eq!(decode(&legacy), Err(WireError::InvalidValue));
    for usage in (4_u16..=0xa4).chain(0xe0..=0xe7) {
        legacy[112..114].copy_from_slice(&usage.to_be_bytes());
        for transition in 0..=2 {
            legacy[114] = transition;
            assert!(decode(&legacy).is_err());
        }
    }
}

#[test]
fn page_usage_pairs_preserve_every_admitted_key_and_transition() {
    for usage in (4..=0xa4).chain(0xe0..=0xe7) {
        for transition in [
            KeyTransition::Release,
            KeyTransition::Press,
            KeyTransition::Repeat,
        ] {
            let request = request(InputEvent::Key {
                key: PhysicalKey::new(usage).unwrap(),
                transition,
            });
            let mut bytes = [0; 117];
            assert_eq!(
                encode_input(
                    request,
                    &mut bytes,
                    &ProtocolLimits::ABSOLUTE,
                    9,
                    InputDirection::ViewerToHost,
                    InputDelivery::Reliable,
                ),
                Ok(117)
            );
            assert_eq!(&bytes[112..114], &[0, 7]);
            assert_eq!(&bytes[114..116], &usage.to_be_bytes());
            assert_eq!(decode(&bytes), Ok(request));
        }
    }
    for usage in [0_u16, 3, 0xa5, 0xdf, 0xe8, u16::MAX] {
        let mut bytes = hex(include_str!("fixtures/input/key_page_usage.hex"));
        bytes[114..116].copy_from_slice(&usage.to_be_bytes());
        assert_eq!(decode(&bytes), Err(WireError::InvalidValue));
    }
}

#[test]
fn key_page_bytes_count_toward_record_and_destination_limits() {
    let event = fixtures()[0].0;
    let expected = hex(include_str!("fixtures/input/key_page_usage.hex"));
    let mut bytes = [42; 117];
    for maximum in [115, 116, 117] {
        let limits = ProtocolLimits::with_overrides(LimitOverrides {
            max_control_message_bytes: Some(maximum),
            ..LimitOverrides::default()
        })
        .unwrap();
        let result = encode_input(
            request(event),
            &mut bytes,
            &limits,
            9,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        );
        let parsed = decode_input(
            &expected,
            &limits,
            9,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        );
        if maximum == 117 {
            assert_eq!(result, Ok(117));
            assert_eq!(bytes.as_slice(), expected);
            assert_eq!(parsed, Ok(request(event)));
        } else {
            assert_eq!(result, Err(WireError::ResourceLimit));
            assert_eq!(bytes, [42; 117]);
            assert_eq!(parsed, Err(WireError::ResourceLimit));
        }
    }
    for size in [115, 116] {
        let mut out = [42; 117];
        assert_eq!(
            encode_input(
                request(event),
                &mut out[..size],
                &ProtocolLimits::ABSOLUTE,
                9,
                InputDirection::ViewerToHost,
                InputDelivery::Reliable,
            ),
            Err(WireError::BufferTooSmall)
        );
        assert_eq!(out, [42; 117]);
    }
}
#[test]
fn host_commands_and_unreliable_actions_are_refused() {
    for (event, bytes) in fixtures() {
        assert_eq!(
            decode_input(
                &bytes,
                &ProtocolLimits::ABSOLUTE,
                9,
                InputDirection::HostToViewer,
                InputDelivery::Reliable
            ),
            Err(WireError::WrongRole)
        );
        let got = decode_input(
            &bytes,
            &ProtocolLimits::ABSOLUTE,
            9,
            InputDirection::ViewerToHost,
            InputDelivery::Datagram,
        );
        assert_eq!(got.is_ok(), event.is_pointer());
        let mut out = [0; MAX_INPUT_RECORD_BYTES];
        assert_eq!(
            encode_input(
                request(event),
                &mut out,
                &ProtocolLimits::ABSOLUTE,
                9,
                InputDirection::HostToViewer,
                InputDelivery::Reliable
            ),
            Err(WireError::WrongRole)
        );
    }
}
#[test]
fn malformed_values_utf8_lengths_and_credentials_never_allocate() {
    let mut key = fixtures().remove(0).1;
    key[116] = 3;
    assert_eq!(decode(&key), Err(WireError::InvalidValue));
    key[116] = 1;
    key[115] = 0;
    assert_eq!(decode(&key), Err(WireError::InvalidValue));
    let mut button = fixtures().remove(1).1;
    button[113] = 2;
    assert_eq!(decode(&button), Err(WireError::InvalidValue));
    let mut text = fixtures().remove(5).1;
    text[116] = 0xff;
    assert_eq!(decode(&text), Err(WireError::InvalidValue));
    text[112..116].copy_from_slice(&u32::MAX.to_be_bytes());
    assert_eq!(decode(&text), Err(WireError::Truncated));
    assert_eq!(
        decode_input(
            &key,
            &ProtocolLimits::ABSOLUTE,
            10,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable
        ),
        Err(WireError::InvalidBinding)
    );
}
#[test]
fn exact_text_and_destination_limits_are_enforced() {
    let text = "x".repeat(MAX_TEXT_BYTES);
    let mut out = [0; MAX_INPUT_RECORD_BYTES];
    let n = encode_input(
        request(InputEvent::Text(&text)),
        &mut out,
        &ProtocolLimits::ABSOLUTE,
        9,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .unwrap();
    assert_eq!(n, out.len());
    assert_eq!(decode(&out).unwrap().event, InputEvent::Text(&text));
    let mut small = [42; 114];
    assert_eq!(
        encode_input(
            request(InputEvent::Text("x")),
            &mut small,
            &ProtocolLimits::ABSOLUTE,
            9,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable
        ),
        Err(WireError::BufferTooSmall)
    );
    assert_eq!(small, [42; 114]);
    for text in [String::new(), "x".repeat(MAX_TEXT_BYTES + 1)] {
        assert_eq!(
            encode_input(
                request(InputEvent::Text(&text)),
                &mut out,
                &ProtocolLimits::ABSOLUTE,
                9,
                InputDirection::ViewerToHost,
                InputDelivery::Reliable
            ),
            Err(WireError::ResourceLimit)
        );
    }
    let limits = ProtocolLimits::with_overrides(LimitOverrides {
        max_control_message_bytes: Some(256),
        ..LimitOverrides::default()
    })
    .unwrap();
    assert_eq!(
        decode_input(
            &out,
            &limits,
            9,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable
        ),
        Err(WireError::ResourceLimit)
    );
}
#[test]
fn diagnostics_do_not_depend_on_input_content_or_credentials() {
    let a = request(InputEvent::Text("PRIVATE"));
    let mut b = request(InputEvent::Text("DIFFERENT"));
    b.credentials.ticket = InputTicketId::from_raw(u128::MAX);
    assert_eq!(format!("{a:?}"), format!("{b:?}"));
    let a = request(InputEvent::Key {
        key: PhysicalKey::new(4).unwrap(),
        transition: KeyTransition::Press,
    });
    let b = request(InputEvent::Key {
        key: PhysicalKey::new(9).unwrap(),
        transition: KeyTransition::Release,
    });
    assert_eq!(format!("{a:?}"), format!("{b:?}"));
}
#[test]
fn signed_display_bounds_refuse_edges_without_clamping() {
    let b = InputBounds::new(DesktopPoint { x: -1920, y: -20 }, 1920, 1080).unwrap();
    assert!(b.contains(DesktopPoint { x: -1920, y: -20 }));
    assert!(b.contains(DesktopPoint { x: -1, y: 1059 }));
    assert!(!b.contains(DesktopPoint { x: 0, y: 0 }));
    assert!(!b.contains(DesktopPoint { x: -1921, y: 0 }));
    assert!(InputBounds::new(DesktopPoint { x: i32::MAX, y: 0 }, 2, 1).is_none());
    assert!(InputBounds::new(DesktopPoint { x: 0, y: 0 }, 0, 1).is_none());
}
