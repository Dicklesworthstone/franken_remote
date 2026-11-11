use fr_core::{
    ids::{HostBootId, OsSessionId, RemoteSessionId},
    limits::{LimitOverrides, ProtocolLimits},
};
use fr_wire::{
    HEADER_BYTES,
    negotiation::{self as n, *},
    stream::RecordStream,
};
fn offer() -> Offer {
    Offer {
        versions: vec![0, 7],
        profile: NATIVE_PROFILE,
        profile_version: PROFILE_VERSION,
        role: Role::Observe,
        limits: ProtocolLimits::ABSOLUTE,
        capabilities: vec![Capability {
            name: "media.hevc.main".into(),
            version: 1,
            required: true,
        }],
    }
}
fn encode(m: &Message) -> Vec<u8> {
    let mut out = vec![0; MAX_RECORD];
    let len = n::encode(m, MAX_RECORD, &mut out).unwrap();
    out.truncate(len);
    out
}
// Constructed independently of the production writer. Pins field order, widths,
// option bytes, primitive endianness and complete-record length.
fn hello_fixture() -> Vec<u8> {
    let mut b = b"FRD0\0\0\0\x01\0\0\0\0\0\0\0\x48\0\0\0\0\0\0\0\0".to_vec();
    b.extend_from_slice(&[0, 0, 0, 2, 0, 0, 0, 7, 1, 0, 0, 0]);
    b.extend_from_slice(&[0, 1, 0, 0, 0, 16, 0, 0, 1, 0, 0, 0, 0, 0, 32, 0]);
    b.extend_from_slice(&[0, 0, 0, 0, 1, 0, 0, 0, 12, 0, 0, 0, 0, 2, 0, 0, 0]);
    b.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 15]);
    b.extend_from_slice(b"media.hevc.main");
    b.extend_from_slice(&[0, 1, 1, 0]);
    b
}
#[test]
fn golden_hello_and_all_message_roundtrips() {
    let fixture = hello_fixture();
    assert_eq!(encode(&Message::ClientHello(offer())), fixture);
    let selection = offer().select().unwrap();
    for m in [
        Message::ClientHello(offer()),
        Message::HostCapabilities(offer()),
        Message::SelectedConfiguration(selection.clone()),
        Message::ApprovalRequired {
            request: RemoteSessionId::from_raw(9),
            deadline_us: 33,
            role: Role::Observe,
        },
        Message::SessionOpened {
            binding: ControlBinding {
                id: 1,
                host_boot: HostBootId::from_raw(1),
                os_session: OsSessionId::from_raw(2),
                remote_session: RemoteSessionId::from_raw(3),
            },
            selection,
            observation_until_us: 44,
        },
        Message::BindingAccepted { binding: 1 },
    ] {
        let bytes = encode(&m);
        assert_eq!(n::decode(&bytes, MAX_RECORD, m.binding()), Ok(m.clone()));
        for end in 0..bytes.len() {
            assert!(
                n::decode(&bytes[..end], MAX_RECORD, m.binding()).is_err(),
                "truncation {end}"
            );
        }
        let mut extra = bytes.clone();
        extra.push(0);
        assert!(n::decode(&extra, MAX_RECORD, m.binding()).is_err());
        let mut small = vec![0x55; bytes.len() - 1];
        assert!(n::encode(&m, MAX_RECORD, &mut small).is_err());
        assert!(small.iter().all(|&b| b == 0x55));
    }
}
#[test]
fn capability_intersection_preserves_required_semantics_and_downward_limits() {
    let client = offer();
    let mut host = offer();
    host.versions = vec![0];
    host.capabilities.push(Capability {
        name: "optional.future".into(),
        version: 1,
        required: false,
    });
    host.limits = ProtocolLimits::with_overrides(LimitOverrides {
        max_encoded_access_unit_bytes: Some(2048),
        per_viewer_compressed_bytes: Some(4096),
        ..Default::default()
    })
    .unwrap();
    let common = host.intersect(&client).unwrap();
    assert_eq!(common.versions, [0]);
    assert_eq!(common.capabilities, client.capabilities);
    assert_eq!(common.limits, host.limits);
    client.check_host(&common).unwrap();
    let mut selection = common.select().unwrap();
    selection.check_against(&common).unwrap();
    selection.limits = client.limits;
    assert_eq!(selection.check_against(&common), Err(Error::Selection));
    selection = common.select().unwrap();
    selection.capabilities.clear();
    assert_eq!(
        selection.check_against(&common),
        Err(Error::RequiredCapability)
    );
    host.capabilities[1].required = true;
    assert_eq!(host.intersect(&client), Err(Error::RequiredCapability));
    host = offer();
    host.capabilities[0].version = 2;
    assert_eq!(host.intersect(&client), Err(Error::RequiredCapability));
    host = offer();
    host.versions = vec![7];
    assert_eq!(host.intersect(&client), Err(Error::Version));
}
#[test]
fn malformed_collections_profiles_and_boundaries_refuse_before_unbounded_work() {
    let packet = encode(&Message::ClientHello(offer()));
    for count in [0, 9, u32::MAX] {
        let mut b = packet.clone();
        b[24..28].copy_from_slice(&count.to_be_bytes());
        assert!(n::decode(&b, MAX_RECORD, 0).is_err());
    }
    for profile in [0, 2, 3, 255] {
        let mut b = packet.clone();
        b[32] = profile;
        assert_eq!(n::decode(&b, MAX_RECORD, 0), Err(Error::Profile));
    }
    for (start, width) in [(36, 4), (60, 1), (65, 4), (69, 4)] {
        let mut b = packet.clone();
        b[start..start + width].fill(255);
        assert!(n::decode(&b, MAX_RECORD, 0).is_err());
    }
    let mut o = offer();
    o.versions = vec![0, 0];
    assert!(n::encode(&Message::ClientHello(o), MAX_RECORD, &mut [0; MAX_RECORD]).is_err());
    let mut o = offer();
    o.capabilities.push(o.capabilities[0].clone());
    assert!(o.validate().is_err());
    let mut o = offer();
    o.capabilities[0].name = "x".repeat(65);
    assert!(o.validate().is_err());
    assert!(n::decode(&packet, packet.len() - 1, 0).is_err());
}
#[test]
fn zero_binding_stream_is_explicit_bounded_and_never_accepts_input_or_media() {
    assert!(RecordStream::new(4096, 0, 100).is_err());
    let packet = encode(&Message::ClientHello(offer()));
    for split in 0..packet.len() {
        let mut stream = RecordStream::negotiation(4096, 100).unwrap();
        assert_eq!(stream.push(&packet[..split], 10).unwrap(), split);
        assert_eq!(
            stream.push(&packet[split..], 11).unwrap(),
            packet.len() - split
        );
        assert_eq!(stream.frame(12).unwrap().unwrap(), packet);
        assert!(stream.allocated_bytes() <= 4096);
    }
    for kind in [0x32u16, 0x34, 0x40, 0x48, 0x1c, 0xffff] {
        let mut b = packet.clone();
        b[6..8].copy_from_slice(&kind.to_be_bytes());
        let mut stream = RecordStream::negotiation(4096, 100).unwrap();
        assert!(stream.push(&b[..HEADER_BYTES], 1).is_err());
        assert_eq!(stream.allocated_bytes(), 0);
        assert!(n::decode(&b, MAX_RECORD, 0).is_err());
    }
    let mut stream = RecordStream::negotiation(4096, 100).unwrap();
    stream.push(&packet[..1], 10).unwrap();
    assert!(stream.push(&packet[1..], 110).is_err());
    let mut b = packet.clone();
    b[12..16].copy_from_slice(&u32::MAX.to_be_bytes());
    let mut stream = RecordStream::negotiation(4096, 100).unwrap();
    assert!(stream.push(&b[..HEADER_BYTES], 1).is_err());
    assert_eq!(stream.allocated_bytes(), 0);
}
#[test]
fn binding_ack_and_extensions_cannot_change_the_selected_session() {
    let ack = encode(&Message::BindingAccepted { binding: 1 });
    assert!(n::decode(&ack, MAX_RECORD, 2).is_err());
    let packet = encode(&Message::ClientHello(offer()));
    let mut extra = packet.clone();
    extra.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0, 0]);
    let payload = u32::try_from(extra.len() - 24).unwrap();
    extra[12..16].copy_from_slice(&payload.to_be_bytes());
    extra[20..24].copy_from_slice(&8u32.to_be_bytes());
    assert_eq!(
        n::decode(&extra, MAX_RECORD, 0),
        Ok(Message::ClientHello(offer()))
    );
    extra[packet.len() + 3] = 1;
    assert!(n::decode(&extra, MAX_RECORD, 0).is_err());
    assert!(!format!("{:?}", offer()).contains("media.hevc.main"));
}

#[test]
fn an_offer_holds_at_most_max_capabilities_and_a_larger_one_is_refused() {
    let caps = |count: usize| -> Vec<Capability> {
        (0..count)
            .map(|i| Capability {
                name: format!("cap.{i:02}"),
                version: 1,
                required: false,
            })
            .collect()
    };
    let mut full = offer();
    full.capabilities = caps(MAX_CAPABILITIES);
    assert!(full.validate().is_ok());
    let message = Message::ClientHello(full);
    let bytes = encode(&message);
    assert!(bytes.len() < MAX_RECORD / 2, "{}", bytes.len());
    assert_eq!(
        n::decode(&bytes, MAX_RECORD, message.binding()),
        Ok(message)
    );
    let mut over = offer();
    over.capabilities = caps(MAX_CAPABILITIES + 1);
    assert_eq!(over.validate(), Err(Error::Invalid));
}

fn scaled_picture() -> Capability {
    Capability {
        name: fr_wire::display::picture::CAPABILITY.into(),
        version: fr_wire::display::picture::VERSION,
        required: false,
    }
}
fn scaled_host() -> Offer {
    fr_wire::display::picture::with_capability(offer()).unwrap()
}
#[test]
fn a_stated_picture_travels_only_with_the_scaled_picture_capability() {
    // A host that offers scaled pictures: the viewer's box rides after the
    // capabilities, in both SelectedConfiguration and SessionOpened.
    let host = scaled_host();
    assert!(host.capabilities.contains(&scaled_picture()));
    let selection = host.select_picture(Some((1920, 1080))).unwrap();
    assert_eq!(selection.picture, Some((1920, 1080)));
    assert!(selection.capabilities.contains(&scaled_picture()));
    selection.check_against(&host).unwrap();
    let plain = host.select().unwrap();
    assert_eq!(plain.picture, None);
    assert!(!plain.capabilities.contains(&scaled_picture()));
    plain.check_against(&host).unwrap();
    let with = encode(&Message::SelectedConfiguration(selection.clone()));
    let without = encode(&Message::SelectedConfiguration(plain.clone()));
    // Eight bytes for the box plus the capability entry itself.
    let entry = 4 + fr_wire::display::picture::CAPABILITY.len() + 3;
    assert_eq!(with.len(), without.len() + entry + 8);
    assert_eq!(&with[with.len() - 8..], &[0, 0, 7, 128, 0, 0, 4, 56]);
    for m in [
        Message::SelectedConfiguration(selection.clone()),
        Message::SessionOpened {
            binding: ControlBinding {
                id: 1,
                host_boot: HostBootId::from_raw(1),
                os_session: OsSessionId::from_raw(2),
                remote_session: RemoteSessionId::from_raw(3),
            },
            selection: selection.clone(),
            observation_until_us: 44,
        },
    ] {
        let bytes = encode(&m);
        assert_eq!(n::decode(&bytes, MAX_RECORD, 0), Ok(m.clone()));
        for end in 0..bytes.len() {
            assert!(n::decode(&bytes[..end], MAX_RECORD, 0).is_err(), "{end}");
        }
    }
    // A host without the capability never receives the extra bytes: the
    // viewer's box is dropped, and the selection is byte-identical to today's.
    let legacy = offer().select_picture(Some((1920, 1080))).unwrap();
    assert_eq!(legacy, offer().select().unwrap());
    assert_eq!(legacy.picture, None);
    // A box larger than the protocol's dimension bound is clamped to it.
    let huge = host.select_picture(Some((20_000, 9_000))).unwrap();
    assert_eq!(huge.picture, Some((8192, 8192)));
}
#[test]
fn a_selection_box_must_match_its_capability_and_stay_in_bounds() {
    let host = scaled_host();
    let good = host.select_picture(Some((1920, 1080))).unwrap();
    // Capability without a box, box without the capability, and boxes outside
    // [64, max_dimension_pixels] are all invalid selections.
    let mut missing = good.clone();
    missing.picture = None;
    let mut stray = host.select().unwrap();
    stray.picture = Some((1920, 1080));
    let mut tiny = good.clone();
    tiny.picture = Some((63, 1080));
    let mut wide = good.clone();
    wide.picture = Some((1920, ProtocolLimits::ABSOLUTE.max_dimension_pixels() + 1));
    for bad in [missing, stray, tiny, wide] {
        assert_eq!(bad.validate(), Err(Error::Invalid));
        let mut out = vec![0; MAX_RECORD];
        assert!(n::encode(&Message::SelectedConfiguration(bad), MAX_RECORD, &mut out).is_err());
    }
    // A decoder that sees the capability requires the eight box bytes, and a
    // decoded box below the minimum is refused.
    let bytes = encode(&Message::SelectedConfiguration(good.clone()));
    let mut short = bytes[..bytes.len() - 8].to_vec();
    let length = u32::from_be_bytes(short[12..16].try_into().unwrap()) - 8;
    short[12..16].copy_from_slice(&length.to_be_bytes());
    assert!(n::decode(&short, MAX_RECORD, 0).is_err());
    let mut small = bytes.clone();
    let at = small.len() - 4;
    small[at..].copy_from_slice(&63_u32.to_be_bytes());
    assert_eq!(n::decode(&small, MAX_RECORD, 0), Err(Error::Invalid));
    // A viewer may not select the capability from a host that lacks it.
    assert!(good.check_against(&offer()).is_err());
}
#[test]
fn a_full_offer_yields_quality_diagnostics_before_skipping_scaled_pictures() {
    let mut full = offer();
    full.capabilities = (0..MAX_CAPABILITIES)
        .map(|i| Capability {
            name: format!("cap-{i:02}"),
            version: 1,
            required: false,
        })
        .collect();
    // Full of other optional capabilities: unchanged, pictures stay native.
    assert_eq!(
        fr_wire::display::picture::with_capability(full.clone()),
        Ok(full.clone())
    );
    full.capabilities[0] = Capability {
        name: fr_wire::quality_decision::CAPABILITY.into(),
        version: fr_wire::quality_decision::VERSION,
        required: false,
    };
    full.capabilities.sort_by(|a, b| a.name.cmp(&b.name));
    let added = fr_wire::display::picture::with_capability(full).unwrap();
    assert_eq!(added.capabilities.len(), MAX_CAPABILITIES);
    assert!(added.capabilities.contains(&scaled_picture()));
    assert!(
        !added
            .capabilities
            .iter()
            .any(|cap| cap.name == fr_wire::quality_decision::CAPABILITY)
    );
    // Adding it twice is idempotent; a required or other-version entry refuses.
    let host = scaled_host();
    assert_eq!(
        fr_wire::display::picture::with_capability(host.clone()),
        Ok(host)
    );
    let mut required = offer();
    required.capabilities.push(Capability {
        required: true,
        ..scaled_picture()
    });
    required.capabilities.sort_by(|a, b| a.name.cmp(&b.name));
    assert_eq!(
        fr_wire::display::picture::with_capability(required),
        Err(Error::RequiredCapability)
    );
}
