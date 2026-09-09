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
