use fr_core::{ids::*, limits::ProtocolLimits};
use fr_wire::{
    HEADER_BYTES, Kind, WireError,
    decoder::{BINDING_BYTES, Binding},
    input::{InputDelivery as D, InputDirection as I},
    negotiation::ControlBinding,
    recovery_request::*,
};
fn binding() -> Binding {
    Binding {
        parent: ControlBinding {
            id: 9,
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
fn bytes(request: Request) -> Vec<u8> {
    let mut out = vec![0; REQUEST_BYTES];
    let n = encode(
        request,
        binding(),
        &ProtocolLimits::ABSOLUTE,
        &mut out,
        I::ViewerToHost,
        D::Reliable,
    )
    .unwrap();
    assert_eq!(n, REQUEST_BYTES);
    out
}
fn parse(bytes: &[u8]) -> Result<Request, WireError> {
    decode(
        bytes,
        binding(),
        &ProtocolLimits::ABSOLUTE,
        I::ViewerToHost,
        D::Reliable,
    )
}
#[test]
fn exact_record_roundtrips_all_reasons_and_distinguishes_unknown_from_frame_zero() {
    for reason in [
        Reason::ReferenceExpired,
        Reason::RecoveryExpired,
        Reason::DecodeFailed,
    ] {
        for frame in [None, Some(0), Some(42), Some(u64::MAX)] {
            let request = Request {
                reason,
                last_useful_frame: frame,
            };
            let wire = bytes(request);
            assert_eq!(&wire[..4], b"FRD0");
            assert_eq!(&wire[6..8], &(Kind::RecoveryRequest as u16).to_be_bytes());
            assert_eq!(Kind::RecoveryRequest as u16, 0x0036);
            assert_eq!(parse(&wire), Ok(request));
            for end in 0..wire.len() {
                assert!(parse(&wire[..end]).is_err());
            }
            let mut extra = wire.clone();
            extra.push(0);
            assert_eq!(parse(&extra), Err(WireError::TrailingBytes));
        }
    }
}
#[test]
fn every_binding_dimension_is_checked_not_just_recovery_number() {
    let wire = bytes(Request {
        reason: Reason::DecodeFailed,
        last_useful_frame: Some(5),
    });
    for index in 0..9 {
        let mut changed = binding();
        match index {
            0 => changed.parent.id += 1,
            1 => changed.parent.host_boot = HostBootId::from_raw(7),
            2 => changed.parent.os_session = OsSessionId::from_raw(7),
            3 => changed.parent.remote_session = RemoteSessionId::from_raw(7),
            4 => changed.display += 1,
            5 => changed.geometry = changed.geometry.next().unwrap(),
            6 => changed.configuration = changed.configuration.next().unwrap(),
            7 => changed.recovery = changed.recovery.next().unwrap(),
            _ => changed.viewport = changed.viewport.next().unwrap(),
        }
        assert_eq!(
            decode(
                &wire,
                changed,
                &ProtocolLimits::ABSOLUTE,
                I::ViewerToHost,
                D::Reliable
            ),
            Err(WireError::InvalidBinding)
        );
    }
}
#[test]
fn wrong_direction_transport_kind_version_and_noncanonical_values_refuse() {
    let request = Request {
        reason: Reason::ReferenceExpired,
        last_useful_frame: None,
    };
    let wire = bytes(request);
    for (direction, delivery, error) in [
        (I::HostToViewer, D::Reliable, WireError::WrongRole),
        (I::ViewerToHost, D::Datagram, WireError::WrongChannel),
    ] {
        assert_eq!(
            decode(
                &wire,
                binding(),
                &ProtocolLimits::ABSOLUTE,
                direction,
                delivery
            ),
            Err(error)
        );
        assert_eq!(
            encode(
                request,
                binding(),
                &ProtocolLimits::ABSOLUTE,
                &mut [0; REQUEST_BYTES],
                direction,
                delivery
            ),
            Err(error)
        );
    }
    let payload = HEADER_BYTES + BINDING_BYTES;
    for (index, value, error) in [
        (payload, 2, WireError::UnsupportedVersion),
        (payload + 1, 0, WireError::InvalidValue),
        (payload + 1, 4, WireError::InvalidValue),
        (payload + 2, 2, WireError::InvalidValue),
        (REQUEST_BYTES - 1, 1, WireError::InvalidValue),
        (9, 1, WireError::InvalidFlags),
    ] {
        let mut invalid = wire.clone();
        invalid[index] = value;
        assert_eq!(parse(&invalid), Err(error));
    }
    let mut invalid = wire;
    invalid[6..8].copy_from_slice(&(Kind::Repair as u16).to_be_bytes());
    assert_eq!(parse(&invalid), Err(WireError::WrongChannel));
    invalid[6..8].copy_from_slice(&(Kind::StageMetrics as u16).to_be_bytes());
    assert_eq!(parse(&invalid), Err(WireError::UnsupportedKind));
}
#[test]
fn small_output_and_zero_parent_binding_never_emit_a_partial_record() {
    let request = Request {
        reason: Reason::RecoveryExpired,
        last_useful_frame: None,
    };
    let mut short = [0x55; REQUEST_BYTES - 1];
    assert_eq!(
        encode(
            request,
            binding(),
            &ProtocolLimits::ABSOLUTE,
            &mut short,
            I::ViewerToHost,
            D::Reliable
        ),
        Err(WireError::BufferTooSmall)
    );
    assert_eq!(short, [0x55; REQUEST_BYTES - 1]);
    let mut invalid = binding();
    invalid.parent.id = 0;
    assert_eq!(
        encode(
            request,
            invalid,
            &ProtocolLimits::ABSOLUTE,
            &mut [0; REQUEST_BYTES],
            I::ViewerToHost,
            D::Reliable
        ),
        Err(WireError::InvalidBinding)
    );
}
