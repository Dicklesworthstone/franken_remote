use fr_core::{
    ids::{InputLeaseId, RemoteSessionId},
    limits::{LimitOverrides, ProtocolLimits},
};
use fr_wire::{WireError, files::*};
fn context(sender: Role) -> Context {
    Context {
        session: RemoteSessionId::from_raw(1),
        lease: InputLeaseId::from_raw(2),
        handle: 3,
        channel: if sender == Role::Host { 10 } else { 9 },
        sender,
        direction: Direction::ToHost,
        lane: Lane::Files,
    }
}
fn limits() -> Limits {
    Limits::new(&ProtocolLimits::ABSOLUTE, 4096).unwrap()
}
fn message(body: Body<'_>) -> Message<'_> {
    Message { id: 4, body }
}
fn fixture(kind: u8, channel: u8, suffix: &[u8]) -> Vec<u8> {
    let mut bytes = vec![0_u8; COMMON_BYTES];
    bytes[..4].copy_from_slice(b"FRD0");
    bytes[7] = kind;
    bytes[12..16].copy_from_slice(
        &u32::try_from(COMMON_BYTES - 24 + suffix.len())
            .unwrap()
            .to_be_bytes(),
    );
    bytes[19] = channel;
    bytes[39] = 1;
    bytes[55] = 2;
    bytes[71] = 3;
    bytes[79] = 4;
    bytes.extend_from_slice(suffix);
    bytes
}
#[test]
fn five_message_kinds_match_independently_constructed_byte_fixtures() {
    assert_eq!((ATP_PORTABLE_FULL, ATP_PORTABLE_DIRECTORY_FULL), (1, 2));
    for profile in [1_u8, 2] {
        let atp = &[1, 2, 3];
        let mut accepted = vec![0, profile];
        accepted.extend_from_slice(&3_u64.to_be_bytes());
        accepted.extend_from_slice(&1000_u32.to_be_bytes());
        accepted.extend_from_slice(&500_u32.to_be_bytes());
        accepted.extend_from_slice(&[0, 1, 0, 0, 0, 3, 1, 2, 3]);
        let mut completed = vec![1, 0, 0];
        completed.extend_from_slice(&3_u64.to_be_bytes());
        completed.extend_from_slice(&[0, 0, 0, 3, 1, 2, 3]);
        for (body, role, expected) in [
            (
                Body::Offer {
                    profile: u16::from(profile),
                    atp,
                },
                Role::Controller,
                fixture(0x70, 9, &[0, profile, 0, 0, 0, 3, 1, 2, 3]),
            ),
            (
                Body::Accept {
                    profile: u16::from(profile),
                    size: 3,
                    bytes_per_second: 1000,
                    chunk_bytes: 500,
                    concurrent_transfers: 1,
                    atp,
                },
                Role::Host,
                fixture(0x71, 10, &accepted),
            ),
            (
                Body::Chunk { atp },
                Role::Controller,
                fixture(0x72, 9, &[0, 0, 0, 3, 1, 2, 3]),
            ),
            (
                Body::Complete {
                    disposition: Disposition::PublishedDurable,
                    reason: Reason::None,
                    published_bytes: 3,
                    atp,
                },
                Role::Host,
                fixture(0x73, 10, &completed),
            ),
            (
                Body::Cancel(Reason::User),
                Role::Controller,
                fixture(0x74, 9, &[0, 1]),
            ),
        ] {
            let mut output = [0; 4096];
            let size = encode(message(body), context(role), limits(), &mut output).unwrap();
            assert_eq!(&output[..size], expected);
            assert_eq!(
                decode(&expected, context(role), limits()),
                Ok(message(body))
            );
            for end in 0..expected.len() {
                assert!(decode(&expected[..end], context(role), limits()).is_err());
            }
            let mut extra = expected;
            extra.push(0);
            assert!(decode(&extra, context(role), limits()).is_err());
        }
    }
}
#[test]
fn authenticated_direction_binding_handle_and_actual_lane_are_mandatory() {
    let frame = fixture(0x70, 9, &[0, 1, 0, 0, 0, 1, 7]);
    let original = context(Role::Controller);
    for changed in [
        Context {
            session: RemoteSessionId::from_raw(9),
            ..original
        },
        Context {
            lease: InputLeaseId::from_raw(9),
            ..original
        },
        Context {
            handle: 9,
            ..original
        },
        Context {
            channel: 8,
            ..original
        },
        Context {
            lane: Lane::Other,
            ..original
        },
        Context {
            sender: Role::Observer,
            ..original
        },
        Context {
            sender: Role::Host,
            ..original
        },
        Context {
            direction: Direction::ToController,
            ..original
        },
    ] {
        assert!(decode(&frame, changed, limits()).is_err());
    }
    for bad in [
        Context {
            handle: 0,
            ..original
        },
        Context {
            channel: 0,
            ..original
        },
        Context {
            session: RemoteSessionId::from_raw(0),
            ..original
        },
        Context {
            lease: InputLeaseId::from_raw(0),
            ..original
        },
    ] {
        assert_eq!(bad.validate(), Err(WireError::InvalidBinding));
    }
    assert_eq!(
        encode(
            message(Body::Cancel(Reason::User)),
            Context {
                sender: Role::Observer,
                ..original
            },
            limits(),
            &mut [0; 4096]
        ),
        Err(WireError::WrongRole)
    );
}
#[test]
fn contradictory_publication_results_and_forged_accept_limits_are_refused() {
    for (disposition, reason, bytes, atp) in [
        (
            Disposition::PublishedDurable,
            Reason::Integrity,
            3,
            &[1][..],
        ),
        (
            Disposition::PublishedDurabilityUnknown,
            Reason::None,
            3,
            &[][..],
        ),
        (Disposition::Refused, Reason::None, 0, &[][..]),
        (Disposition::Refused, Reason::Integrity, 1, &[][..]),
        (Disposition::Refused, Reason::Conflict, 0, &[1][..]),
        (Disposition::UnknownEffect, Reason::None, 0, &[][..]),
        (
            Disposition::UnknownEffect,
            Reason::UnknownEffect,
            1,
            &[][..],
        ),
    ] {
        assert_eq!(
            encode(
                message(Body::Complete {
                    disposition,
                    reason,
                    published_bytes: bytes,
                    atp
                }),
                context(Role::Host),
                limits(),
                &mut [0; 4096]
            ),
            Err(WireError::InvalidValue)
        );
    }
    for (rate, chunk, count) in [(0, 1, 1), (1, 0, 1), (1, 4096, 1), (1, 1, 0), (1, 1, 2)] {
        assert_eq!(
            encode(
                message(Body::Accept {
                    profile: 1,
                    size: 0,
                    bytes_per_second: rate,
                    chunk_bytes: chunk,
                    concurrent_transfers: count,
                    atp: &[1]
                }),
                context(Role::Host),
                limits(),
                &mut [0; 4096]
            ),
            Err(WireError::InvalidLimits)
        );
    }
}
#[test]
fn publication_uncertainty_and_refusal_have_distinct_roundtrip_dispositions() {
    for (disposition, reason, bytes, atp) in [
        (Disposition::PublishedDurable, Reason::None, 3, &[1][..]),
        (
            Disposition::PublishedDurabilityUnknown,
            Reason::None,
            3,
            &[1][..],
        ),
        (Disposition::Refused, Reason::Integrity, 0, &[][..]),
        (
            Disposition::UnknownEffect,
            Reason::UnknownEffect,
            0,
            &[][..],
        ),
    ] {
        let value = message(Body::Complete {
            disposition,
            reason,
            published_bytes: bytes,
            atp,
        });
        let mut buffer = [0; 4096];
        let size = encode(value, context(Role::Host), limits(), &mut buffer).unwrap();
        assert_eq!(
            decode(&buffer[..size], context(Role::Host), limits()),
            Ok(value)
        );
    }
}
#[test]
fn entire_envelope_is_charged_to_selected_f_and_never_exceeds_c() {
    let protocol = ProtocolLimits::with_overrides(LimitOverrides {
        max_control_message_bytes: Some(4096),
        ..LimitOverrides::default()
    })
    .unwrap();
    assert!(Limits::new(&protocol, 4097).is_err());
    let selected = Limits::new(&protocol, 2048).unwrap();
    let payload = vec![7; selected.atp_bytes()];
    let mut output = vec![0; 4096];
    let body = Body::Accept {
        profile: 1,
        size: 0,
        bytes_per_second: 1,
        chunk_bytes: 1,
        concurrent_transfers: 1,
        atp: &payload,
    };
    assert_eq!(
        encode(message(body), context(Role::Host), selected, &mut output),
        Ok(2048)
    );
    let too_large = vec![7; selected.atp_bytes() + 1];
    assert_eq!(
        encode(
            message(Body::Chunk { atp: &too_large }),
            context(Role::Controller),
            selected,
            &mut output
        ),
        Err(WireError::ResourceLimit)
    );
    assert_eq!(
        decode(&output[..2049], context(Role::Host), selected),
        Err(WireError::ResourceLimit)
    );
    assert_eq!(
        encode(
            message(Body::Chunk { atp: &[1] }),
            context(Role::Controller),
            selected,
            &mut []
        ),
        Err(WireError::BufferTooSmall)
    );
}
#[test]
fn profiles_and_peer_lengths_cannot_bypass_the_payload_limit() {
    let mut body = fixture(0x70, 9, &[0, 1, 0, 0, 0, 1, 7]);
    body[81] = 3; // Profiles 1 and 2 are explicitly implemented; 3 is not.
    assert_eq!(
        decode(&body, context(Role::Controller), limits()),
        Err(WireError::UnsupportedVersion)
    );
    body[81] = 1;
    body[82..86].copy_from_slice(&u32::MAX.to_be_bytes());
    assert!(decode(&body, context(Role::Controller), limits()).is_err());
    let debug = format!(
        "{:?}",
        message(Body::Chunk {
            atp: b"confidential-payload"
        })
    );
    assert!(!debug.contains("confidential"));
}
