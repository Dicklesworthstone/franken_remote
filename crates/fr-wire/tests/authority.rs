use fr_core::{
    ids::{InputLeaseId, RemoteSessionId},
    limits::{LimitOverrides, ProtocolLimits},
};
use fr_wire::{
    WireError,
    authority::{self, Binding, Message, Scope},
    input::{InputDelivery as Delivery, InputDirection as Direction},
};
const LIMITS: ProtocolLimits = ProtocolLimits::ABSOLUTE;
fn binding() -> Binding {
    Binding {
        channel: 0x0102_0304,
        session: RemoteSessionId::from_raw(0x1122_3344_5566_7788_99aa_bbcc_ddee_ff00),
    }
}
fn fixture(control: bool, response: bool) -> Vec<u8> {
    let mut bytes = vec![
        0x46,
        0x52,
        0x44,
        0x30,
        0,
        0,
        0,
        if response { 0x16 } else { 0x15 },
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        1,
        2,
        3,
        4,
        0,
        0,
        0,
        0,
    ];
    bytes.extend_from_slice(&[
        0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0,
    ]);
    bytes.extend_from_slice(if control { &[1, 1] } else { &[0, 0] });
    if control {
        bytes.extend_from_slice(&[0x55; 16]);
    }
    bytes.extend_from_slice(&[0x77; 16]);
    if !response {
        bytes.extend_from_slice(&[0, 0, 0, 0, 0, 0x0f, 0x42, 0x40]);
    }
    bytes[15] = u8::try_from(bytes.len() - 24).unwrap();
    bytes
}
fn message(control: bool, response: bool) -> Message {
    let scope = if control {
        Scope::Control(InputLeaseId::from_raw(u128::from_be_bytes([0x55; 16])))
    } else {
        Scope::Observation
    };
    let nonce = u128::from_be_bytes([0x77; 16]);
    if response {
        Message::Response { scope, nonce }
    } else {
        Message::Challenge {
            scope,
            nonce,
            deadline_micros: 1_000_000,
        }
    }
}
#[test]
fn four_independent_goldens_keep_full_width_scope_and_deadline() {
    for control in [false, true] {
        for response in [false, true] {
            let direction = if response {
                Direction::ViewerToHost
            } else {
                Direction::HostToViewer
            };
            let expected = fixture(control, response);
            let mut out = [0; authority::MAX_AUTHORITY_BYTES];
            let n = authority::encode(
                message(control, response),
                binding(),
                &LIMITS,
                &mut out,
                direction,
                Delivery::Reliable,
            )
            .unwrap();
            assert_eq!(&out[..n], expected);
            assert_eq!(
                authority::decode(&expected, binding(), &LIMITS, direction, Delivery::Reliable),
                Ok(message(control, response))
            );
            for end in 0..expected.len() {
                assert!(
                    authority::decode(
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
}
#[test]
fn role_delivery_and_application_bindings_cannot_be_substituted() {
    let bytes = fixture(false, false);
    assert_eq!(
        authority::decode(
            &bytes,
            binding(),
            &LIMITS,
            Direction::ViewerToHost,
            Delivery::Reliable
        ),
        Err(WireError::WrongRole)
    );
    assert_eq!(
        authority::decode(
            &bytes,
            binding(),
            &LIMITS,
            Direction::HostToViewer,
            Delivery::Datagram
        ),
        Err(WireError::WrongChannel)
    );
    for b in [
        Binding {
            channel: 0,
            ..binding()
        },
        Binding {
            channel: 3,
            ..binding()
        },
        Binding {
            session: RemoteSessionId::from_raw(2),
            ..binding()
        },
    ] {
        assert_eq!(
            authority::decode(
                &bytes,
                b,
                &LIMITS,
                Direction::HostToViewer,
                Delivery::Reliable
            ),
            Err(WireError::InvalidBinding)
        );
    }
    let mut zero_bound = bytes;
    zero_bound[16..20].fill(0);
    assert!(
        authority::decode(
            &zero_bound,
            Binding {
                channel: 0,
                ..binding()
            },
            &LIMITS,
            Direction::HostToViewer,
            Delivery::Reliable
        )
        .is_err()
    );
}
#[test]
fn contradictory_scope_zero_nonce_lease_deadline_and_trailing_bytes_refuse() {
    for (control, ranges) in [
        (false, vec![40..41, 41..42, 42..58, 58..66]),
        (true, vec![40..41, 41..42, 42..58, 58..74, 74..82]),
    ] {
        for range in ranges {
            let mut bytes = fixture(control, false);
            bytes[range.clone()].fill(if range.start < 42 && !control { 2 } else { 0 });
            assert!(
                authority::decode(
                    &bytes,
                    binding(),
                    &LIMITS,
                    Direction::HostToViewer,
                    Delivery::Reliable
                )
                .is_err(),
                "{control} {range:?}"
            );
        }
    }
    let mut bytes = fixture(false, true);
    bytes.push(0);
    assert_eq!(
        authority::decode(
            &bytes,
            binding(),
            &LIMITS,
            Direction::ViewerToHost,
            Delivery::Reliable
        ),
        Err(WireError::TrailingBytes)
    );
    let mut bytes = fixture(false, true);
    bytes[6..8].copy_from_slice(&0x48u16.to_be_bytes());
    assert_eq!(
        authority::decode(
            &bytes,
            binding(),
            &LIMITS,
            Direction::ViewerToHost,
            Delivery::Reliable
        ),
        Err(WireError::UnsupportedKind)
    );
}
#[test]
fn extensions_bounds_and_output_capacity_are_checked() {
    let mut bytes = fixture(false, false);
    bytes.extend_from_slice(&[0x70, 0, 0, 0, 0, 0, 0, 0]);
    bytes[15] += 8;
    bytes[23] = 8;
    assert_eq!(
        authority::decode(
            &bytes,
            binding(),
            &LIMITS,
            Direction::HostToViewer,
            Delivery::Reliable
        ),
        Ok(message(false, false))
    );
    let flag = bytes.len() - 6;
    bytes[flag] = 0x80;
    assert!(
        authority::decode(
            &bytes,
            binding(),
            &LIMITS,
            Direction::HostToViewer,
            Delivery::Reliable
        )
        .is_err()
    );
    let limits = ProtocolLimits::with_overrides(LimitOverrides {
        max_control_message_bytes: Some(64),
        ..LimitOverrides::default()
    })
    .unwrap();
    assert_eq!(
        authority::decode(
            &fixture(false, false),
            binding(),
            &limits,
            Direction::HostToViewer,
            Delivery::Reliable
        ),
        Err(WireError::ResourceLimit)
    );
    let mut out = [0xbb; 65];
    assert_eq!(
        authority::encode(
            message(false, false),
            binding(),
            &LIMITS,
            &mut out,
            Direction::HostToViewer,
            Delivery::Reliable
        ),
        Err(WireError::BufferTooSmall)
    );
    assert_eq!(out, [0xbb; 65]);
}
#[test]
fn diagnostics_exclude_nonce_session_and_lease() {
    assert_eq!(format!("{:?}", binding()), "AuthorityBinding");
    assert_eq!(format!("{:?}", message(true, false)), "Challenge");
    assert_eq!(format!("{:?}", message(true, true)), "ChallengeResponse");
}
