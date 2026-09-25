//! Independent, hand-written bytes from PROTOCOL_LEASE_REVOKED.md, not an
//! encoder-generated golden. These are codec tests, not live-wire evidence.
use fr_core::{
    ids::{InputLeaseId, RemoteSessionId},
    limits::ProtocolLimits,
};
use fr_wire::{
    WireError,
    authority::Binding,
    input::{InputDelivery, InputDirection},
    lease_revoked::{self, CleanupStage, EffectStage, REVOKED_BYTES, Reason, Revoked},
};

fn fixture() -> Vec<u8> {
    "46 52 44 30 00 00 00 14 00 00 00 00 00 00 00 24 00 00 00 07 00 00 00 00
     00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 01
     00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 02
     00 01 01 00"
        .split_whitespace()
        .map(|s| u8::from_str_radix(s, 16).expect("hand-written hex"))
        .collect()
}
fn binding() -> Binding {
    Binding {
        channel: 7,
        session: RemoteSessionId::from_raw(1),
    }
}
fn message() -> Revoked {
    Revoked {
        lease: InputLeaseId::from_raw(2),
        reason: Reason::LocalRevoke,
        cleanup: CleanupStage::Fenced,
        effects: EffectStage::Unknown,
    }
}
fn decode(bytes: &[u8]) -> Result<Revoked, WireError> {
    lease_revoked::decode(
        bytes,
        binding(),
        message().lease,
        &ProtocolLimits::ABSOLUTE,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
}
fn encode(value: Revoked, out: &mut [u8]) -> Result<usize, WireError> {
    lease_revoked::encode(
        value,
        binding(),
        &ProtocolLimits::ABSOLUTE,
        out,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
}
#[test]
fn independent_fixture_in_both_directions() {
    let expected = fixture();
    assert_eq!(expected.len(), REVOKED_BYTES);
    assert_eq!(decode(&expected), Ok(message()));
    let mut out = [0; REVOKED_BYTES];
    assert_eq!(encode(message(), &mut out), Ok(REVOKED_BYTES));
    assert_eq!(out.as_slice(), expected);
}
#[test]
fn every_truncation_and_short_output_refuse() {
    let bytes = fixture();
    for end in 0..bytes.len() {
        assert!(decode(&bytes[..end]).is_err(), "prefix {end}");
        let mut out = vec![0; end];
        assert_eq!(encode(message(), &mut out), Err(WireError::BufferTooSmall));
    }
}
#[test]
fn wrong_direction_and_delivery_refuse() {
    for (direction, delivery, error) in [
        (
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
            WireError::WrongRole,
        ),
        (
            InputDirection::HostToViewer,
            InputDelivery::Datagram,
            WireError::WrongChannel,
        ),
    ] {
        let mut out = [0; REVOKED_BYTES];
        assert_eq!(
            lease_revoked::encode(
                message(),
                binding(),
                &ProtocolLimits::ABSOLUTE,
                &mut out,
                direction,
                delivery,
            ),
            Err(error)
        );
        assert_eq!(
            lease_revoked::decode(
                &fixture(),
                binding(),
                message().lease,
                &ProtocolLimits::ABSOLUTE,
                direction,
                delivery,
            ),
            Err(error)
        );
    }
}
#[test]
fn stale_session_lease_and_channel_never_match() {
    for offset in [19, 39, 55] {
        let mut bytes = fixture();
        bytes[offset] ^= 1;
        assert_eq!(decode(&bytes), Err(WireError::InvalidBinding));
    }
    assert_eq!(
        lease_revoked::decode(
            &fixture(),
            binding(),
            InputLeaseId::from_raw(3),
            &ProtocolLimits::ABSOLUTE,
            InputDirection::HostToViewer,
            InputDelivery::Reliable,
        ),
        Err(WireError::InvalidBinding)
    );
}
#[test]
fn invalid_reason_cleanup_and_effects_refuse() {
    for (offset, value) in [(57, 0), (57, 10), (58, 0), (58, 4), (59, 3)] {
        let mut bytes = fixture();
        bytes[offset] = value;
        assert_eq!(decode(&bytes), Err(WireError::InvalidValue));
    }
}
#[test]
fn every_defined_reason_and_stage_round_trips_without_losing_information() {
    for reason in [
        Reason::LocalRevoke,
        Reason::LeaseExpired,
        Reason::ObservationEnded,
        Reason::ViewInvalidated,
        Reason::SessionEnded,
        Reason::PermissionLost,
        Reason::HostFailure,
        Reason::ClientRequested,
        Reason::Suspended,
    ] {
        for cleanup in [
            CleanupStage::Fenced,
            CleanupStage::Released,
            CleanupStage::Failed,
        ] {
            for effects in [
                EffectStage::Unknown,
                EffectStage::ReceiptsPending,
                EffectStage::ReceiptsComplete,
            ] {
                let value = Revoked {
                    reason,
                    cleanup,
                    effects,
                    ..message()
                };
                let mut out = [0; REVOKED_BYTES];
                encode(value, &mut out).expect("valid stage");
                assert_eq!(decode(&out), Ok(value));
                assert!(!reason.code().is_empty());
            }
        }
    }
}
#[test]
fn zero_bindings_are_not_wildcards() {
    for (binding, lease) in [
        (
            Binding {
                channel: 0,
                ..binding()
            },
            message().lease,
        ),
        (
            Binding {
                session: RemoteSessionId::from_raw(0),
                ..binding()
            },
            message().lease,
        ),
        (binding(), InputLeaseId::from_raw(0)),
    ] {
        assert_eq!(
            lease_revoked::decode(
                &fixture(),
                binding,
                lease,
                &ProtocolLimits::ABSOLUTE,
                InputDirection::HostToViewer,
                InputDelivery::Reliable,
            ),
            Err(WireError::InvalidBinding)
        );
    }
}
#[test]
fn trailing_bytes_and_oversize_records_refuse() {
    let mut bytes = fixture();
    bytes.push(0);
    assert_eq!(decode(&bytes), Err(WireError::TrailingBytes));
    bytes.resize(
        ProtocolLimits::ABSOLUTE.max_control_message_bytes() as usize + 1,
        0,
    );
    assert_eq!(decode(&bytes), Err(WireError::ResourceLimit));
}
#[test]
fn optional_extensions_are_bounded_and_required_extensions_refuse() {
    let mut bytes = fixture();
    bytes[15] = 44;
    bytes[23] = 8;
    bytes.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0, 0]);
    assert_eq!(decode(&bytes), Ok(message()));
    bytes[63] = 1;
    assert_eq!(decode(&bytes), Err(WireError::RequiredExtension));
    bytes[63] = 0;
    bytes[67] = 1;
    assert_eq!(decode(&bytes), Err(WireError::Truncated));
}
#[test]
fn diagnostic_output_does_not_expose_identifiers() {
    let value = Revoked {
        lease: InputLeaseId::from_raw(u128::MAX),
        ..message()
    };
    let debug = format!("{value:?}");
    assert!(!debug.contains(&u128::MAX.to_string()));
    assert!(!debug.contains("lease:"));
    assert!(debug.contains("Fenced"));
    assert!(debug.contains("Unknown"));
}
