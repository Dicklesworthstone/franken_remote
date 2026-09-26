use fr_core::{ids::RemoteSessionId, limits::ProtocolLimits};
use fr_wire::{
    WireError,
    authority::Binding,
    closure::{self, Cleanup, Closed, ClosedReason as Reason, OutstandingEffects as Effects},
    input::{InputDelivery as T, InputDirection as D},
    stream::RecordStream,
};

// Written by hand from the field contract: session 13, compact binding 7,
// HostStopping, Unconfirmed cleanup, UNKNOWN effects. Not an encoder dump.
const UNKNOWN: &[u8] = b"FRD0\x00\x00\x00\x1e\x00\x00\x00\x00\x00\x00\x00\x1c\x00\x00\x00\x07\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x0d\x00\x02\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00";
// Same framing; ClientRequested, Complete cleanup, known 2 pending/3 uncertain.
// Complete cleanup does not undo external effects or fabricate complete receipts.
const KNOWN: &[u8] = b"FRD0\x00\x00\x00\x1e\x00\x00\x00\x00\x00\x00\x00\x1c\x00\x00\x00\x07\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x0d\x00\x01\x02\x01\x00\x00\x00\x02\x00\x00\x00\x03";
fn binding() -> Binding {
    Binding {
        channel: 7,
        session: RemoteSessionId::from_raw(13),
    }
}
fn parse(bytes: &[u8]) -> Result<Closed, WireError> {
    closure::decode_closed(
        bytes,
        binding(),
        &ProtocolLimits::ABSOLUTE,
        D::HostToViewer,
        T::Reliable,
    )
}
fn unknown() -> Closed {
    Closed {
        reason: Reason::HostStopping,
        cleanup: Cleanup::Unconfirmed,
        effects: Effects::Unknown,
    }
}
fn encode(report: Closed, out: &mut [u8]) -> Result<usize, WireError> {
    closure::encode_closed(
        report,
        binding(),
        &ProtocolLimits::ABSOLUTE,
        out,
        D::HostToViewer,
        T::Reliable,
    )
}
#[test]
fn independent_fixtures_survive_all_splits_and_refuse_every_truncation() {
    for (bytes, expected) in [
        (UNKNOWN, unknown()),
        (
            KNOWN,
            Closed {
                reason: Reason::ClientRequested,
                cleanup: Cleanup::Complete,
                effects: Effects::Known {
                    pending: 2,
                    uncertain: 3,
                },
            },
        ),
    ] {
        assert_eq!(bytes.len(), closure::CLOSED_BYTES);
        assert_eq!(parse(bytes), Ok(expected));
        let mut actual = [0; closure::CLOSED_BYTES];
        assert_eq!(encode(expected, &mut actual), Ok(bytes.len()));
        assert_eq!(actual, bytes);
        for split in 0..=bytes.len() {
            let mut stream = RecordStream::new(128, 7, 100).unwrap();
            assert_eq!(stream.push(&bytes[..split], 0).unwrap(), split);
            assert_eq!(
                stream.push(&bytes[split..], 1).unwrap(),
                bytes.len() - split
            );
            assert_eq!(parse(stream.frame(1).unwrap().unwrap()), Ok(expected));
            stream.consume(1).unwrap();
            stream.finish(1).unwrap();
        }
        for end in 0..bytes.len() {
            assert!(parse(&bytes[..end]).is_err());
        }
    }
}
#[test]
fn reason_cleanup_and_effect_accounting_are_independent_and_lossless() {
    for reason in [
        Reason::ClientRequested,
        Reason::HostStopping,
        Reason::AuthorityExpired,
        Reason::PermissionLost,
        Reason::ViewInvalidated,
        Reason::ProtocolError,
        Reason::HostFailure,
        Reason::SessionReplaced,
    ] {
        assert_ne!(reason.code(), "");
        for cleanup in [Cleanup::Unconfirmed, Cleanup::Complete, Cleanup::Incomplete] {
            for effects in [
                Effects::Unknown,
                Effects::Known {
                    pending: 0,
                    uncertain: 0,
                },
                Effects::Known {
                    pending: u32::MAX,
                    uncertain: u32::MAX,
                },
            ] {
                let report = Closed {
                    reason,
                    cleanup,
                    effects,
                };
                let mut bytes = [0; closure::CLOSED_BYTES];
                encode(report, &mut bytes).unwrap();
                assert_eq!(parse(&bytes), Ok(report));
            }
        }
    }
    let mut known = UNKNOWN.to_vec();
    known[43] = 1;
    assert_ne!(parse(UNKNOWN), parse(&known));
}
#[test]
fn wrong_owner_role_transport_or_preadmission_never_accepts_a_report() {
    for b in [
        Binding {
            channel: 0,
            ..binding()
        },
        Binding {
            channel: 8,
            ..binding()
        },
        Binding {
            session: RemoteSessionId::from_raw(0),
            ..binding()
        },
        Binding {
            session: RemoteSessionId::from_raw(14),
            ..binding()
        },
    ] {
        assert_eq!(
            closure::decode_closed(
                UNKNOWN,
                b,
                &ProtocolLimits::ABSOLUTE,
                D::HostToViewer,
                T::Reliable
            ),
            Err(WireError::InvalidBinding)
        );
    }
    for (direction, delivery, error) in [
        (D::ViewerToHost, T::Reliable, WireError::WrongRole),
        (D::HostToViewer, T::Datagram, WireError::WrongChannel),
    ] {
        assert_eq!(
            closure::decode_closed(
                UNKNOWN,
                binding(),
                &ProtocolLimits::ABSOLUTE,
                direction,
                delivery
            ),
            Err(error)
        );
        let mut out = [0x55; closure::CLOSED_BYTES];
        assert_eq!(
            closure::encode_closed(
                unknown(),
                binding(),
                &ProtocolLimits::ABSOLUTE,
                &mut out,
                direction,
                delivery
            ),
            Err(error)
        );
        assert_eq!(out, [0x55; closure::CLOSED_BYTES]);
    }
    let mut early = RecordStream::negotiation(128, 100).unwrap();
    assert!(early.push(UNKNOWN, 0).is_err());
    assert_eq!(early.allocated_bytes(), 0);
}
#[test]
fn malformed_unknown_summaries_reserved_values_and_trailing_bytes_refuse() {
    for (offset, value) in [
        (41, 0),
        (41, 9),
        (42, 0),
        (42, 4),
        (43, 2),
        (47, 1),
        (51, 1),
    ] {
        let mut bytes = UNKNOWN.to_vec();
        bytes[offset] = value;
        assert_eq!(parse(&bytes), Err(WireError::InvalidValue));
    }
    let mut bytes = UNKNOWN.to_vec();
    bytes[7] = 0x1d;
    assert_eq!(parse(&bytes), Err(WireError::UnsupportedKind));
    let mut bytes = UNKNOWN.to_vec();
    bytes.push(0);
    assert_eq!(parse(&bytes), Err(WireError::TrailingBytes));
    bytes[15] += 1;
    assert_eq!(parse(&bytes), Err(WireError::TrailingBytes));
    let mut short = [0x55; closure::CLOSED_BYTES - 1];
    assert_eq!(
        encode(unknown(), &mut short),
        Err(WireError::BufferTooSmall)
    );
    assert_eq!(short, [0x55; closure::CLOSED_BYTES - 1]);
}
#[test]
fn extensions_and_advertised_lengths_use_the_existing_bounded_framer() {
    let mut bytes = UNKNOWN.to_vec();
    bytes.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0, 1, 99]);
    bytes[15] += 9;
    bytes[23] = 9;
    assert_eq!(parse(&bytes), Ok(unknown()));
    bytes[closure::CLOSED_BYTES + 3] = 1;
    assert_eq!(parse(&bytes), Err(WireError::RequiredExtension));
    let mut too_small = RecordStream::new(closure::CLOSED_BYTES - 1, 7, 100).unwrap();
    assert!(too_small.push(UNKNOWN, 0).is_err());
    assert_eq!(too_small.allocated_bytes(), 0);
    let mut huge = UNKNOWN.to_vec();
    huge[12..16].copy_from_slice(&u32::MAX.to_be_bytes());
    let mut stream = RecordStream::new(128, 7, 100).unwrap();
    assert!(stream.push(&huge, 0).is_err());
    assert_eq!(stream.allocated_bytes(), 0);
}
