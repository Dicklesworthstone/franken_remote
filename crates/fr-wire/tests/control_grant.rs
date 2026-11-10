use fr_core::{
    ids::*,
    input::*,
    input_submission::{Capabilities, Capability},
    limits::{LimitOverrides, ProtocolLimits},
};
use fr_wire::{
    WireError,
    control::*,
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
        sequence: 0,
        target: Target {
            display_binding: 8,
            view: InputView {
                geometry: DisplayGeometryGeneration::from_raw(4),
                viewport: ViewportMappingGeneration::from_raw(5),
                configuration: CodecConfigurationGeneration::from_raw(6),
                recovery: RecoveryGeneration::from_raw(7),
            },
            bounds: InputBounds::new(DesktopPoint { x: -100, y: 20 }, 1920, 1080).unwrap(),
            capabilities: Capabilities::default()
                .with(Capability::Keys)
                .with(Capability::Absolute)
                .with(Capability::Buttons),
        },
    }
}
fn grant() -> Granted {
    Granted {
        request: request(),
        input_channel: 9,
        lease: InputLeaseId::from_raw(10),
        ticket: InputTicketId::from_raw(11),
        issued_at_us: 0,
        lease_until_us: 3_000_000,
        ticket_until_us: 1_000_000,
        first_action: 0,
        first_pointer: 0,
    }
}
fn request_bytes() -> Vec<u8> {
    let mut b = vec![0; REQUEST_BYTES];
    assert_eq!(
        encode_request(request(), &mut b, &L, D::ViewerToHost, T::Reliable),
        Ok(REQUEST_BYTES)
    );
    b
}
fn grant_bytes(g: Granted) -> Vec<u8> {
    let mut b = vec![0; GRANTED_BYTES];
    assert_eq!(
        encode_granted(g, &mut b, &L, D::HostToViewer, T::Reliable),
        Ok(GRANTED_BYTES)
    );
    b
}
fn read(b: &[u8]) -> Result<Granted, WireError> {
    decode_granted(b, request().parent, &L, D::HostToViewer, T::Reliable)
}
#[test]
fn independent_golden_request_and_grant_layout() {
    let mut b = vec![
        0x46, 0x52, 0x44, 0x30, 0, 0, 0, 0x12, 0, 0, 0, 0, 0, 0, 0, 109, 0, 0, 0, 7, 0, 0, 0, 0,
    ];
    for n in [1_u128, 2, 3] {
        b.extend_from_slice(&n.to_be_bytes());
    }
    b.extend_from_slice(&0_u64.to_be_bytes());
    b.extend_from_slice(&8_u32.to_be_bytes());
    for n in [4_u64, 5, 6, 7] {
        b.extend_from_slice(&n.to_be_bytes());
    }
    b.extend_from_slice(&(-100_i32).to_be_bytes());
    b.extend_from_slice(&20_i32.to_be_bytes());
    b.extend_from_slice(&1920_u32.to_be_bytes());
    b.extend_from_slice(&1080_u32.to_be_bytes());
    b.push(13);
    assert_eq!(b.len(), 133);
    assert_eq!(request_bytes(), b);
    assert_eq!(
        decode_request(&b, request().parent, &L, D::ViewerToHost, T::Reliable),
        Ok(request())
    );
    b[7] = 0x13;
    b[15] = 185;
    b.extend_from_slice(&9_u32.to_be_bytes());
    for n in [10_u128, 11] {
        b.extend_from_slice(&n.to_be_bytes());
    }
    for n in [0_u64, 3_000_000, 1_000_000, 0, 0] {
        b.extend_from_slice(&n.to_be_bytes());
    }
    assert_eq!(b.len(), 209);
    assert_eq!(grant_bytes(grant()), b);
    assert_eq!(read(&b), Ok(grant()));
}
#[test]
fn every_truncation_and_trailing_byte_refuse_without_partial_grant() {
    let request = request_bytes();
    let grant = grant_bytes(grant());
    for end in 0..request.len() {
        assert!(
            decode_request(
                &request[..end],
                self::request().parent,
                &L,
                D::ViewerToHost,
                T::Reliable
            )
            .is_err()
        );
    }
    for end in 0..grant.len() {
        assert!(read(&grant[..end]).is_err());
    }
    let mut extra = grant;
    extra.push(0);
    assert!(read(&extra).is_err());
}
#[test]
fn full_parent_role_channel_and_limits_are_not_advisory() {
    let b = grant_bytes(grant());
    for offset in [24, 40, 56] {
        let mut altered = b.clone();
        altered[offset + 15] ^= 1;
        assert!(read(&altered).is_err());
    }
    assert_eq!(
        decode_granted(&b, request().parent, &L, D::ViewerToHost, T::Reliable),
        Err(WireError::WrongRole)
    );
    assert_eq!(
        decode_granted(&b, request().parent, &L, D::HostToViewer, T::Datagram),
        Err(WireError::WrongChannel)
    );
    let small = ProtocolLimits::with_overrides(LimitOverrides {
        max_control_message_bytes: Some(208),
        ..LimitOverrides::default()
    })
    .unwrap();
    assert!(decode_granted(&b, request().parent, &small, D::HostToViewer, T::Reliable).is_err());
    let mut ext = b;
    ext[20..24].copy_from_slice(&1_u32.to_be_bytes());
    assert!(read(&ext).is_err());
}
#[test]
fn bounds_capability_dependencies_and_channel_aliases_refuse() {
    let mut b = request_bytes();
    b[132] = 2;
    assert_eq!(
        decode_request(&b, request().parent, &L, D::ViewerToHost, T::Reliable),
        Err(WireError::InvalidValue)
    );
    for caps in [0, 8, 32, 64] {
        b[132] = caps;
        assert!(decode_request(&b, request().parent, &L, D::ViewerToHost, T::Reliable).is_err());
    }
    for channel in [0, 7, 8] {
        let mut g = grant();
        g.input_channel = channel;
        assert_eq!(
            encode_granted(g, &mut [0; GRANTED_BYTES], &L, D::HostToViewer, T::Reliable),
            Err(WireError::InvalidBinding)
        );
    }
    let mut b = request_bytes();
    b[116..120].copy_from_slice(&i32::MAX.to_be_bytes());
    assert!(decode_request(&b, request().parent, &L, D::ViewerToHost, T::Reliable).is_err());
}
#[test]
fn lease_ticket_deadlines_and_fresh_replay_positions_are_checked() {
    for g in [
        Granted {
            ticket_until_us: 0,
            ..grant()
        },
        Granted {
            ticket_until_us: 1_500_001,
            ..grant()
        },
        Granted {
            lease_until_us: 3_000_001,
            ..grant()
        },
        Granted {
            lease_until_us: 999_999,
            ..grant()
        },
        Granted {
            issued_at_us: u64::MAX,
            ..grant()
        },
        Granted {
            first_action: 1,
            ..grant()
        },
        Granted {
            first_pointer: u64::MAX,
            ..grant()
        },
    ] {
        assert_eq!(
            encode_granted(g, &mut [0; GRANTED_BYTES], &L, D::HostToViewer, T::Reliable),
            Err(WireError::InvalidValue)
        );
    }
    assert_eq!(grant().initial_ticket().sequence, 0);
    assert_eq!(grant().initial_ticket().credentials, grant().credentials());
}
#[test]
fn diagnostics_do_not_include_secrets_or_target_coordinates() {
    let text = format!("{:?} {:?} {:?}", request(), grant(), request().target);
    assert!(!text.contains("1920"));
    assert!(!text.contains("-100"));
    assert!(!text.contains("InputLeaseId"));
    let mut g = grant();
    g.lease = InputLeaseId::from_raw(u128::MAX);
    assert_eq!(format!("{g:?}"), format!("{:?}", grant()));
}

#[test]
fn line_scroll_is_requestable_only_when_both_peers_selected_it() {
    use fr_wire::negotiation::Capability as Negotiated;
    let cap = |name: &str, version| Negotiated {
        name: name.into(),
        version,
        required: false,
    };
    let core = Capabilities::default()
        .with(Capability::Keys)
        .with(Capability::Repeat)
        .with(Capability::Absolute)
        .with(Capability::Buttons);
    let wanted = core.with(Capability::LineScroll);
    let grant = [cap(GRANT_CAPABILITY, 1)];
    let without = requestable(&grant);
    assert!(!without.contains(Capability::LineScroll));
    assert_eq!(wanted.meet(without), core);
    // Another version, or a similar name, is not this capability.
    for other in [
        cap(LINE_SCROLL_CAPABILITY, LINE_SCROLL_VERSION + 1),
        cap("native-input-line-scrolls", LINE_SCROLL_VERSION),
    ] {
        assert!(!requestable(&[other]).contains(Capability::LineScroll));
    }
    let with = requestable(&[cap(GRANT_CAPABILITY, 1), cap(LINE_SCROLL_CAPABILITY, 1)]);
    assert_eq!(wanted.meet(with), wanted);
    assert!(!without.contains(Capability::PixelScroll));
    assert!(!with.contains(Capability::PixelScroll));
    // Selection removes unselected scroll/text modes; other operations stay
    // whatever the controller wanted (the host still decides the grant).
    for c in [
        Capability::Keys,
        Capability::Repeat,
        Capability::Absolute,
        Capability::Buttons,
        Capability::Relative,
    ] {
        assert!(without.contains(c) && with.contains(c), "{c:?}");
    }
}

#[test]
fn committed_text_is_optional_and_never_inferred_from_keyboard_or_clipboard() {
    use fr_wire::negotiation::{
        Capability as Negotiated, NATIVE_PROFILE, Offer, PROFILE_VERSION, Role,
    };
    let capability = |name: &str, version| Negotiated {
        name: name.into(),
        version,
        required: false,
    };
    for selection in [
        vec![],
        vec![capability(GRANT_CAPABILITY, 1)],
        vec![capability(TEXT_CAPABILITY, 2)],
        vec![capability("clipboard", 1)],
    ] {
        assert!(!requestable(&selection).contains(Capability::Text));
        assert!(requestable(&selection).contains(Capability::Keys));
    }
    let base = Offer {
        versions: vec![0],
        profile: NATIVE_PROFILE,
        profile_version: PROFILE_VERSION,
        role: Role::RequestControl,
        limits: L,
        capabilities: vec![capability(GRANT_CAPABILITY, 1)],
    };
    let text = with_committed_text(base.clone()).unwrap();
    assert!(requestable(&text.capabilities).contains(Capability::Text));
    assert_eq!(with_committed_text(text.clone()), Ok(text.clone()));
    assert!(!requestable(&text.intersect(&base).unwrap().capabilities).contains(Capability::Text));
    assert!(requestable(&text.intersect(&text).unwrap().capabilities).contains(Capability::Text));
    let both = with_pixel_scroll(text).unwrap();
    assert!(requestable(&both.capabilities).contains(Capability::Text));
    assert!(requestable(&both.capabilities).contains(Capability::PixelScroll));
}

#[test]
fn pixel_scroll_requires_its_exact_optional_selection_independently_of_lines() {
    use fr_wire::negotiation::Capability as Negotiated;
    let cap = |name: &str, version| Negotiated {
        name: name.into(),
        version,
        required: false,
    };
    let wanted = Capabilities::default()
        .with(Capability::Absolute)
        .with(Capability::LineScroll)
        .with(Capability::PixelScroll);
    assert_eq!(
        wanted.meet(requestable(&[])),
        Capabilities::default().with(Capability::Absolute)
    );
    for (name, version) in [
        (PIXEL_SCROLL_CAPABILITY, 2),
        ("native-input-pixel-scrolls", 1),
        (LINE_SCROLL_CAPABILITY, 1),
    ] {
        assert!(!requestable(&[cap(name, version)]).contains(Capability::PixelScroll));
    }
    let pixels = requestable(&[cap(PIXEL_SCROLL_CAPABILITY, PIXEL_SCROLL_VERSION)]);
    assert!(pixels.contains(Capability::PixelScroll));
    assert!(!pixels.contains(Capability::LineScroll));
    let both = requestable(&[
        cap(PIXEL_SCROLL_CAPABILITY, 1),
        cap(LINE_SCROLL_CAPABILITY, 1),
    ]);
    assert_eq!(wanted.meet(both), wanted);
}

#[test]
fn explicit_pixel_offer_preserves_required_boundaries_and_capability_ceiling() {
    use fr_wire::negotiation::{
        Capability as Negotiated, Error, MAX_CAPABILITIES, NATIVE_PROFILE, Offer, PROFILE_VERSION,
        Role,
    };
    let cap = |name: &str, required| Negotiated {
        name: name.into(),
        version: 1,
        required,
    };
    let base = Offer {
        versions: vec![0],
        profile: NATIVE_PROFILE,
        profile_version: PROFILE_VERSION,
        role: Role::RequestControl,
        limits: L,
        capabilities: vec![cap(GRANT_CAPABILITY, true)],
    };
    let explicit = with_pixel_scroll(base.clone()).unwrap();
    assert_eq!(explicit.capabilities.len(), 2);
    assert!(
        explicit
            .capabilities
            .iter()
            .any(|c| c.name == PIXEL_SCROLL_CAPABILITY && !c.required)
    );
    assert_eq!(with_pixel_scroll(explicit.clone()), Ok(explicit));
    let mut fullest = base.clone();
    for index in 0..MAX_CAPABILITIES - 2 {
        fullest
            .capabilities
            .push(cap(&format!("required-{index:02}"), true));
    }
    fullest
        .capabilities
        .push(cap(fr_wire::quality_decision::CAPABILITY, false));
    fullest.capabilities.sort_by(|a, b| a.name.cmp(&b.name));
    let bounded = with_pixel_scroll(fullest.clone()).unwrap();
    assert_eq!(bounded.capabilities.len(), MAX_CAPABILITIES);
    assert_eq!(bounded.role, fullest.role);
    assert_eq!(bounded.limits, fullest.limits);
    for required in fullest.capabilities.iter().filter(|c| c.required) {
        assert!(bounded.capabilities.contains(required));
    }
    fullest
        .capabilities
        .iter_mut()
        .find(|c| c.name == fr_wire::quality_decision::CAPABILITY)
        .unwrap()
        .required = true;
    assert_eq!(with_pixel_scroll(fullest), Err(Error::Invalid));
    let mut wrong = base.clone();
    wrong.capabilities.push(Negotiated {
        name: PIXEL_SCROLL_CAPABILITY.into(),
        version: 2,
        required: false,
    });
    wrong.capabilities.sort_by(|a, b| a.name.cmp(&b.name));
    assert_eq!(with_pixel_scroll(wrong), Err(Error::RequiredCapability));
    let mut observing = base;
    observing.capabilities.clear();
    assert_eq!(with_pixel_scroll(observing), Err(Error::RequiredCapability));
}
