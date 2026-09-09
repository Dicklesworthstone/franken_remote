use fr_core::{
    ids::{HostBootId, OsSessionId, RemoteSessionId},
    limits::{LimitOverrides, ProtocolLimits},
};
use fr_wire::{
    WireError,
    clock::{self, Message},
    input::{InputDelivery as Delivery, InputDirection as Direction},
    negotiation::ControlBinding,
};
const LIMITS: ProtocolLimits = ProtocolLimits::ABSOLUTE;
fn binding() -> ControlBinding {
    ControlBinding {
        id: 0x0102_0304,
        host_boot: HostBootId::from_raw(u128::from_be_bytes([0x11; 16])),
        os_session: OsSessionId::from_raw(u128::from_be_bytes([0x22; 16])),
        remote_session: RemoteSessionId::from_raw(u128::from_be_bytes([0x33; 16])),
    }
}
fn fixture(reply: bool) -> Vec<u8> {
    let mut bytes = vec![
        0x46,
        0x52,
        0x44,
        0x30,
        0,
        0,
        0,
        if reply { 0x85 } else { 0x84 },
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        if reply { 64 } else { 56 },
        1,
        2,
        3,
        4,
        0,
        0,
        0,
        0,
    ];
    bytes.extend_from_slice(&[0x11; 16]);
    bytes.extend_from_slice(&[0x22; 16]);
    bytes.extend_from_slice(&[0x33; 16]);
    bytes.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
    if reply {
        bytes.extend_from_slice(&[0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8]);
    }
    bytes
}
fn message(reply: bool) -> Message {
    if reply {
        Message::Reply {
            sequence: 0x0102_0304_0506_0708,
            host_sample_us: 0xf1f2_f3f4_f5f6_f7f8,
        }
    } else {
        Message::Probe {
            sequence: 0x0102_0304_0506_0708,
        }
    }
}
#[test]
fn independent_goldens_and_every_truncation() {
    for reply in [false, true] {
        let direction = if reply {
            Direction::HostToViewer
        } else {
            Direction::ViewerToHost
        };
        let expected = fixture(reply);
        let mut out = [0; clock::REPLY_BYTES];
        let n = clock::encode(
            message(reply),
            binding(),
            &LIMITS,
            &mut out,
            direction,
            Delivery::Reliable,
        )
        .unwrap();
        assert_eq!(&out[..n], expected);
        assert_eq!(
            clock::decode(&expected, binding(), &LIMITS, direction, Delivery::Reliable),
            Ok(message(reply))
        );
        for end in 0..expected.len() {
            assert!(
                clock::decode(
                    &expected[..end],
                    binding(),
                    &LIMITS,
                    direction,
                    Delivery::Reliable
                )
                .is_err()
            );
        }
    }
}
#[test]
fn exact_boot_os_session_and_channel_not_just_sequence() {
    let bytes = fixture(true);
    let b = binding();
    for wrong in [
        ControlBinding {
            host_boot: HostBootId::from_raw(2),
            ..b
        },
        ControlBinding {
            os_session: OsSessionId::from_raw(2),
            ..b
        },
        ControlBinding {
            remote_session: RemoteSessionId::from_raw(2),
            ..b
        },
        ControlBinding { id: 9, ..b },
        ControlBinding { id: 0, ..b },
    ] {
        assert!(
            clock::decode(
                &bytes,
                wrong,
                &LIMITS,
                Direction::HostToViewer,
                Delivery::Reliable
            )
            .is_err()
        );
    }
    assert_eq!(
        clock::decode(
            &bytes,
            b,
            &LIMITS,
            Direction::ViewerToHost,
            Delivery::Reliable
        ),
        Err(WireError::WrongRole)
    );
    assert_eq!(
        clock::decode(
            &bytes,
            b,
            &LIMITS,
            Direction::HostToViewer,
            Delivery::Datagram
        ),
        Err(WireError::WrongChannel)
    );
}
#[test]
fn zero_sequence_is_invalid_but_zero_clock_is_valid() {
    let mut bytes = fixture(true);
    bytes[72..80].fill(0);
    assert_eq!(
        clock::decode(
            &bytes,
            binding(),
            &LIMITS,
            Direction::HostToViewer,
            Delivery::Reliable
        ),
        Err(WireError::InvalidValue)
    );
    let mut out = [0; clock::REPLY_BYTES];
    let msg = Message::Reply {
        sequence: 1,
        host_sample_us: 0,
    };
    let n = clock::encode(
        msg,
        binding(),
        &LIMITS,
        &mut out,
        Direction::HostToViewer,
        Delivery::Reliable,
    )
    .unwrap();
    assert_eq!(
        clock::decode(
            &out[..n],
            binding(),
            &LIMITS,
            Direction::HostToViewer,
            Delivery::Reliable
        ),
        Ok(msg)
    );
    assert!(!format!("{msg:?}").contains("sequence"));
}
#[test]
fn bounds_extensions_and_trailing_data_stay_checked() {
    let small = ProtocolLimits::with_overrides(LimitOverrides {
        max_control_message_bytes: Some(80),
        ..LimitOverrides::default()
    })
    .unwrap();
    let mut out = [0xbb; clock::REPLY_BYTES - 1];
    assert!(
        clock::encode(
            message(true),
            binding(),
            &LIMITS,
            &mut out,
            Direction::HostToViewer,
            Delivery::Reliable
        )
        .is_err()
    );
    assert_eq!(
        clock::decode(
            &fixture(true),
            binding(),
            &small,
            Direction::HostToViewer,
            Delivery::Reliable
        ),
        Err(WireError::ResourceLimit)
    );
    let mut bytes = fixture(false);
    bytes.push(0);
    assert_eq!(
        clock::decode(
            &bytes,
            binding(),
            &LIMITS,
            Direction::ViewerToHost,
            Delivery::Reliable
        ),
        Err(WireError::TrailingBytes)
    );
    bytes = fixture(false);
    bytes.extend_from_slice(&[0x70, 0, 0, 0, 0, 0, 0, 0]);
    bytes[15] += 8;
    bytes[23] = 8;
    assert_eq!(
        clock::decode(
            &bytes,
            binding(),
            &LIMITS,
            Direction::ViewerToHost,
            Delivery::Reliable
        ),
        Ok(message(false))
    );
    let flag = bytes.len() - 5;
    bytes[flag] = 1;
    assert_eq!(
        clock::decode(
            &bytes,
            binding(),
            &LIMITS,
            Direction::ViewerToHost,
            Delivery::Reliable
        ),
        Err(WireError::RequiredExtension)
    );
}
