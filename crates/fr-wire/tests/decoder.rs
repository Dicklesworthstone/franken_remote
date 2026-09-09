use fr_core::{
    ids::*,
    limits::{LimitOverrides, ProtocolLimits},
};
use fr_wire::{
    WireError,
    decoder::{self, Binding, Configuration, Message},
    input::{InputDelivery as T, InputDirection as D},
};
const L: ProtocolLimits = ProtocolLimits::ABSOLUTE;
fn binding() -> Binding {
    Binding {
        parent: fr_wire::negotiation::ControlBinding {
            id: 7,
            host_boot: HostBootId::from_raw(11),
            os_session: OsSessionId::from_raw(12),
            remote_session: RemoteSessionId::from_raw(13),
        },
        display: 14,
        geometry: DisplayGeometryGeneration::from_raw(15),
        configuration: CodecConfigurationGeneration::from_raw(16),
        recovery: RecoveryGeneration::from_raw(17),
        viewport: ViewportMappingGeneration::from_raw(18),
    }
}
fn config() -> Configuration<'static> {
    Configuration {
        coded_width: 320,
        coded_height: 240,
        crop_width: 318,
        crop_height: 238,
        fps: 30,
        primaries: 1,
        transfer: 1,
        matrix: 1,
        full_range: false,
        decoded_pictures: 4,
        codec: "hev1.1.6.L60.90",
        hvcc: &[1; 23],
    }
}
fn direction(m: Message<'_>) -> D {
    if matches!(m, Message::Configuration(_)) {
        D::HostToViewer
    } else {
        D::ViewerToHost
    }
}
fn bytes(m: Message<'_>) -> Vec<u8> {
    let mut out = vec![0; 20000];
    let n = decoder::encode(m, binding(), &L, &mut out, direction(m), T::Reliable).unwrap();
    out.truncate(n);
    out
}
fn golden_prefix(kind: u16, payload: u32) -> Vec<u8> {
    let mut b = b"FRD0".to_vec();
    b.extend_from_slice(&[0, 0]);
    b.extend_from_slice(&kind.to_be_bytes());
    b.extend_from_slice(&[0; 4]);
    b.extend_from_slice(&payload.to_be_bytes());
    b.extend_from_slice(&7u32.to_be_bytes());
    b.extend_from_slice(&[0; 4]);
    for n in [11u128, 12, 13, 14] {
        b.extend_from_slice(&n.to_be_bytes());
    }
    for n in [15u64, 16, 17, 18] {
        b.extend_from_slice(&n.to_be_bytes());
    }
    b
}
#[test]
fn independently_assembled_configuration_and_acknowledgements_are_exact() {
    let c = config();
    let mut b = golden_prefix(
        0x30,
        u32::try_from(127 + c.codec.len() + c.hvcc.len()).unwrap(),
    );
    for n in [320u32, 240, 318, 238] {
        b.extend_from_slice(&n.to_be_bytes());
    }
    b.extend_from_slice(&[0, 30, 1, 1, 1, 0, 4]);
    b.extend_from_slice(&u32::try_from(c.codec.len()).unwrap().to_be_bytes());
    b.extend_from_slice(c.codec.as_bytes());
    b.extend_from_slice(&23u32.to_be_bytes());
    b.extend_from_slice(&[1; 23]);
    assert_eq!(bytes(Message::Configuration(c)), b);
    assert_eq!(bytes(Message::Configured), golden_prefix(0x31, 96));
    let mut first = golden_prefix(0x33, 112);
    first.extend_from_slice(&0u64.to_be_bytes());
    first.extend_from_slice(&99u64.to_be_bytes());
    assert_eq!(
        bytes(Message::FirstDecoded {
            frame: 0,
            decoder_micros: 99
        }),
        first
    );
    let decoded = decoder::decode(&b, binding(), &L, D::HostToViewer, T::Reliable).unwrap();
    assert_eq!(decoded, Message::Configuration(c));
    let Message::Configuration(d) = decoded else {
        panic!()
    };
    assert!(d.hvcc.as_ptr() >= b.as_ptr() && d.hvcc.as_ptr() < b.as_ptr().wrapping_add(b.len()));
}
#[test]
fn every_truncation_and_trailing_byte_refuses() {
    for m in [
        Message::Configuration(config()),
        Message::Configured,
        Message::FirstDecoded {
            frame: 4,
            decoder_micros: 7,
        },
    ] {
        let b = bytes(m);
        for n in 0..b.len() {
            assert!(
                decoder::decode(&b[..n], binding(), &L, direction(m), T::Reliable).is_err(),
                "accepted truncation {n}"
            );
        }
        let mut extra = b;
        extra.push(0);
        assert!(decoder::decode(&extra, binding(), &L, direction(m), T::Reliable).is_err());
    }
}
#[test]
fn full_parent_display_and_each_generation_are_bound_even_when_channel_matches() {
    let b = bytes(Message::Configured);
    for offset in [24, 40, 56, 72, 88, 96, 104, 112] {
        let mut changed = b.clone();
        changed[offset] ^= 1;
        assert_eq!(
            decoder::decode(&changed, binding(), &L, D::ViewerToHost, T::Reliable),
            Err(WireError::InvalidBinding)
        );
    }
    let mut zero = binding();
    zero.parent.id = 0;
    assert_eq!(
        decoder::decode(&b, zero, &L, D::ViewerToHost, T::Reliable),
        Err(WireError::InvalidBinding)
    );
}
#[test]
fn wrong_role_datagrams_and_initial_binding_never_admit() {
    for m in [
        Message::Configuration(config()),
        Message::Configured,
        Message::FirstDecoded {
            frame: 0,
            decoder_micros: 0,
        },
    ] {
        let mut b = bytes(m);
        let opposite = if direction(m) == D::HostToViewer {
            D::ViewerToHost
        } else {
            D::HostToViewer
        };
        assert_eq!(
            decoder::decode(&b, binding(), &L, opposite, T::Reliable),
            Err(WireError::WrongRole)
        );
        assert_eq!(
            decoder::decode(&b, binding(), &L, direction(m), T::Datagram),
            Err(WireError::WrongChannel)
        );
        b[16..20].fill(0);
        assert!(decoder::decode(&b, binding(), &L, direction(m), T::Reliable).is_err());
    }
}
#[test]
fn malformed_lengths_and_resource_declarations_refuse_without_allocation() {
    let mut configs = vec![];
    let c = config();
    configs.push(Configuration { crop_width: 0, ..c });
    configs.push(Configuration {
        coded_width: u32::MAX,
        ..c
    });
    configs.push(Configuration {
        crop_width: 321,
        ..c
    });
    configs.push(Configuration { fps: 0, ..c });
    configs.push(Configuration { fps: 241, ..c });
    configs.push(Configuration {
        decoded_pictures: 13,
        ..c
    });
    configs.push(Configuration {
        decoded_pictures: 1,
        ..c
    });
    configs.push(Configuration {
        codec: "hvc1.1.6.L60.90",
        ..c
    });
    configs.push(Configuration {
        codec: "hev1.\n",
        ..c
    });
    configs.push(Configuration {
        hvcc: &[0; 22],
        ..c
    });
    configs.push(Configuration { transfer: 16, ..c });
    for c in configs {
        assert!(
            decoder::encode(
                Message::Configuration(c),
                binding(),
                &L,
                &mut [0; 1000],
                D::HostToViewer,
                T::Reliable
            )
            .is_err()
        );
    }
    let b = bytes(Message::Configuration(config()));
    for at in [143, 147 + config().codec.len()] {
        let mut changed = b.clone();
        changed[at..at + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(decoder::decode(&changed, binding(), &L, D::HostToViewer, T::Reliable).is_err());
    }
    let mut bad_bool = b;
    bad_bool[141] = 2;
    assert!(decoder::decode(&bad_bool, binding(), &L, D::HostToViewer, T::Reliable).is_err());
}
#[test]
fn exact_destination_and_control_caps_precede_any_write() {
    let m = Message::Configuration(config());
    let b = bytes(m);
    let mut out = vec![0xaa; b.len() - 1];
    assert_eq!(
        decoder::encode(m, binding(), &L, &mut out, D::HostToViewer, T::Reliable),
        Err(WireError::BufferTooSmall)
    );
    assert!(out.iter().all(|v| *v == 0xaa));
    let limits = ProtocolLimits::with_overrides(LimitOverrides {
        max_control_message_bytes: Some(u32::try_from(b.len() - 1).unwrap()),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        decoder::decode(&b, binding(), &limits, D::HostToViewer, T::Reliable),
        Err(WireError::ResourceLimit)
    );
}
#[test]
fn diagnostic_formatting_does_not_echo_parameter_bytes_or_peer_codec_text() {
    let first = format!("{:?}", config());
    let c = Configuration {
        codec: "hev1.sensitive",
        hvcc: &[99; 23],
        ..config()
    };
    assert_eq!(format!("{c:?}"), first);
    let mut other = binding();
    other.display = 999;
    assert_eq!(format!("{other:?}"), format!("{:?}", binding()));
}
