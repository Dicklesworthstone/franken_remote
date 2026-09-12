use fr_core::{ids::*, limits::ProtocolLimits};
use fr_wire::{
    WireError,
    decoder::Binding,
    input::{InputDelivery as D, InputDirection as I},
    negotiation::ControlBinding,
    receiver_metrics::*,
};
fn binding() -> Binding {
    Binding {
        parent: ControlBinding {
            id: 7,
            host_boot: HostBootId::from_raw(1),
            os_session: OsSessionId::from_raw(2),
            remote_session: RemoteSessionId::from_raw(3),
        },
        display: 4,
        geometry: DisplayGeometryGeneration::INITIAL,
        configuration: CodecConfigurationGeneration::INITIAL,
        recovery: RecoveryGeneration::INITIAL,
        viewport: ViewportMappingGeneration::INITIAL,
    }
}
fn encode_message(m: Message, i: I) -> Vec<u8> {
    let mut b = vec![0; REPLY_BYTES];
    let n = encode(
        m,
        binding(),
        &ProtocolLimits::ABSOLUTE,
        &mut b,
        i,
        D::Reliable,
    )
    .unwrap();
    b.truncate(n);
    b
}
fn load() -> Load {
    Load {
        retained_bytes: 1000,
        retained_pictures: 2,
        decoding: true,
        work_us: Some(75_000),
    }
}
#[test]
fn queries_and_actual_stage_samples_have_exact_bounded_roundtrips() {
    for (m, i, n) in [
        (Message::Query { sequence: 1 }, I::HostToViewer, QUERY_BYTES),
        (
            Message::Reply {
                sequence: 2,
                load: load(),
            },
            I::ViewerToHost,
            REPLY_BYTES,
        ),
        (
            Message::Reply {
                sequence: 3,
                load: Load {
                    retained_bytes: 0,
                    retained_pictures: 0,
                    decoding: false,
                    work_us: None,
                },
            },
            I::ViewerToHost,
            REPLY_BYTES,
        ),
    ] {
        let b = encode_message(m, i);
        assert_eq!(b.len(), n);
        assert_eq!(
            decode(&b, binding(), &ProtocolLimits::ABSOLUTE, i, D::Reliable).unwrap(),
            m
        );
        for end in 0..b.len() {
            assert!(
                decode(
                    &b[..end],
                    binding(),
                    &ProtocolLimits::ABSOLUTE,
                    i,
                    D::Reliable
                )
                .is_err()
            );
        }
    }
}
#[test]
fn metrics_cannot_use_wrong_direction_datagrams_or_a_different_view_generation() {
    let b = encode_message(
        Message::Reply {
            sequence: 1,
            load: load(),
        },
        I::ViewerToHost,
    );
    assert_eq!(
        decode(
            &b,
            binding(),
            &ProtocolLimits::ABSOLUTE,
            I::HostToViewer,
            D::Reliable
        ),
        Err(WireError::WrongRole)
    );
    assert!(
        decode(
            &b,
            binding(),
            &ProtocolLimits::ABSOLUTE,
            I::ViewerToHost,
            D::Datagram
        )
        .is_err()
    );
    for i in 0..8 {
        let mut v = binding();
        match i {
            0 => v.parent.id += 1,
            1 => v.parent.host_boot = HostBootId::from_raw(5),
            2 => v.parent.os_session = OsSessionId::from_raw(5),
            3 => v.parent.remote_session = RemoteSessionId::from_raw(5),
            4 => v.display += 1,
            5 => v.geometry = v.geometry.next().unwrap(),
            6 => v.recovery = v.recovery.next().unwrap(),
            _ => v.configuration = v.configuration.next().unwrap(),
        }
        assert_eq!(
            decode(
                &b,
                v,
                &ProtocolLimits::ABSOLUTE,
                I::ViewerToHost,
                D::Reliable
            ),
            Err(WireError::InvalidBinding)
        );
    }
}
#[test]
fn unknown_values_are_not_zero_cost_measurements_and_invalid_flags_refuse() {
    let empty = Load {
        retained_bytes: 0,
        retained_pictures: 0,
        decoding: false,
        work_us: None,
    };
    let mut b = encode_message(
        Message::Reply {
            sequence: 1,
            load: empty,
        },
        I::ViewerToHost,
    );
    *b.last_mut().unwrap() = 1;
    assert!(
        decode(
            &b,
            binding(),
            &ProtocolLimits::ABSOLUTE,
            I::ViewerToHost,
            D::Reliable
        )
        .is_err()
    );
    for index in [QUERY_BYTES + 12, QUERY_BYTES + 13] {
        let mut b = encode_message(
            Message::Reply {
                sequence: 1,
                load: load(),
            },
            I::ViewerToHost,
        );
        b[index] = 2;
        assert!(
            decode(
                &b,
                binding(),
                &ProtocolLimits::ABSOLUTE,
                I::ViewerToHost,
                D::Reliable
            )
            .is_err()
        );
    }
    for invalid in [
        Load {
            decoding: true,
            ..empty
        },
        Load {
            retained_bytes: 1,
            ..empty
        },
        Load {
            work_us: Some(MAX_WORK_US + 1),
            ..empty
        },
        Load {
            retained_pictures: 99,
            ..load()
        },
    ] {
        assert!(invalid.validate(&ProtocolLimits::ABSOLUTE).is_err());
    }
}
#[test]
fn extensions_and_trailing_data_cannot_smuggle_extra_samples() {
    let mut b = encode_message(Message::Query { sequence: 1 }, I::HostToViewer);
    b.push(0);
    assert!(
        decode(
            &b,
            binding(),
            &ProtocolLimits::ABSOLUTE,
            I::HostToViewer,
            D::Reliable
        )
        .is_err()
    );
    let mut b = encode_message(Message::Query { sequence: 1 }, I::HostToViewer);
    b[120] = 2;
    assert_eq!(
        decode(
            &b,
            binding(),
            &ProtocolLimits::ABSOLUTE,
            I::HostToViewer,
            D::Reliable
        ),
        Err(WireError::UnsupportedVersion)
    );
}
