//! Independent protocol fixtures, never regenerated from the encoder.
use fr_core::{
    ids::{
        CodecConfigurationGeneration, DisplayGeometryGeneration, HostBootId, OsSessionId,
        RecoveryGeneration, RemoteSessionId, ViewportMappingGeneration,
    },
    limits::{LimitOverrides, ProtocolLimits},
};
use fr_wire::{
    WireError,
    decoder::Binding,
    input::{InputDelivery as Delivery, InputDirection as Direction},
    negotiation::ControlBinding,
    quality_decision::{self, Decision, Mode, Reason},
};

const LIMITS: ProtocolLimits = ProtocolLimits::ABSOLUTE;

fn binding() -> Binding {
    Binding {
        parent: ControlBinding {
            id: 0x0102_0304,
            host_boot: HostBootId::from_raw(u128::from_be_bytes([0x11; 16])),
            os_session: OsSessionId::from_raw(u128::from_be_bytes([0x22; 16])),
            remote_session: RemoteSessionId::from_raw(u128::from_be_bytes([0x33; 16])),
        },
        display: u128::from_be_bytes([0x44; 16]),
        geometry: DisplayGeometryGeneration::from_raw(1),
        configuration: CodecConfigurationGeneration::from_raw(2),
        recovery: RecoveryGeneration::from_raw(3),
        viewport: ViewportMappingGeneration::from_raw(4),
    }
}

fn decision(pending: bool) -> Decision {
    Decision {
        sequence: 0x0102_0304_0506_0708,
        capture_interval_us: if pending { 40_000 } else { 200_000 },
        mode: if pending { Mode::Active } else { Mode::Idle },
        reason: if pending {
            Reason::SendAdmission
        } else {
            Reason::VerifiedIdle
        },
        applied_bitrate_bps: pending.then_some(8_000_000),
        pending_bitrate_bps: pending.then_some(6_000_000),
    }
}

fn fixture(pending: bool) -> Vec<u8> {
    // Header: FRD0, version 0, kind 0x0083, no flags/extensions, 126-byte
    // payload, binding 0x01020304. Full view tuple, version 1, sequence,
    // u64-microsecond interval, mode, reason, then two presence/u32 slots.
    let fixed = "
        46 52 44 30 00 00 00 83 00 00 00 00 00 00 00 7e 01 02 03 04 00 00 00 00
        11 11 11 11 11 11 11 11 11 11 11 11 11 11 11 11
        22 22 22 22 22 22 22 22 22 22 22 22 22 22 22 22
        33 33 33 33 33 33 33 33 33 33 33 33 33 33 33 33
        44 44 44 44 44 44 44 44 44 44 44 44 44 44 44 44
        00 00 00 00 00 00 00 01 00 00 00 00 00 00 00 02
        00 00 00 00 00 00 00 03 00 00 00 00 00 00 00 04
        00 01 01 02 03 04 05 06 07 08";
    let state = if pending {
        "00 00 00 00 00 00 9c 40 00 02 01 00 7a 12 00 01 00 5b 8d 80"
    } else {
        "00 00 00 00 00 03 0d 40 01 07 00 00 00 00 00 00 00 00 00 00"
    };
    fixed
        .split_whitespace()
        .chain(state.split_whitespace())
        .map(|byte| u8::from_str_radix(byte, 16).unwrap())
        .collect()
}

fn decode(bytes: &[u8]) -> Result<Decision, WireError> {
    quality_decision::decode(
        bytes,
        binding(),
        &LIMITS,
        Direction::HostToViewer,
        Delivery::Reliable,
    )
}

fn encode(decision: Decision) -> Result<[u8; quality_decision::RECORD_BYTES], WireError> {
    let mut bytes = [0; quality_decision::RECORD_BYTES];
    let written = quality_decision::encode(
        decision,
        binding(),
        &LIMITS,
        &mut bytes,
        Direction::HostToViewer,
        Delivery::Reliable,
    )?;
    assert_eq!(written, bytes.len());
    Ok(bytes)
}

#[test]
fn independent_goldens_preserve_applied_pending_and_unobserved_bitrates() {
    assert_eq!(quality_decision::RECORD_BYTES, 150);
    for pending in [false, true] {
        let expected = fixture(pending);
        assert_eq!(encode(decision(pending)).unwrap().as_slice(), expected);
        assert_eq!(decode(&expected), Ok(decision(pending)));
        for end in 0..expected.len() {
            assert!(decode(&expected[..end]).is_err(), "truncated at {end}");
        }
    }
    let applied = Decision {
        pending_bitrate_bps: None,
        ..decision(true)
    };
    assert_eq!(decode(&encode(applied).unwrap()), Ok(applied));
}

#[test]
fn every_view_identity_and_generation_is_checked() {
    let bytes = fixture(true);
    for index in 0..9 {
        let mut other = binding();
        match index {
            0 => other.parent.id += 1,
            1 => other.parent.host_boot = HostBootId::from_raw(2),
            2 => other.parent.os_session = OsSessionId::from_raw(2),
            3 => other.parent.remote_session = RemoteSessionId::from_raw(2),
            4 => other.display += 1,
            5 => other.geometry = other.geometry.next().unwrap(),
            6 => other.configuration = other.configuration.next().unwrap(),
            7 => other.recovery = other.recovery.next().unwrap(),
            _ => other.viewport = other.viewport.next().unwrap(),
        }
        assert_eq!(
            quality_decision::decode(
                &bytes,
                other,
                &LIMITS,
                Direction::HostToViewer,
                Delivery::Reliable,
            ),
            Err(WireError::InvalidBinding),
            "binding member {index}",
        );
    }
}

#[test]
fn only_the_host_can_report_on_the_reliable_channel() {
    for (direction, delivery, expected) in [
        (
            Direction::ViewerToHost,
            Delivery::Reliable,
            WireError::WrongRole,
        ),
        (
            Direction::HostToViewer,
            Delivery::Datagram,
            WireError::WrongChannel,
        ),
    ] {
        assert_eq!(
            quality_decision::decode(&fixture(true), binding(), &LIMITS, direction, delivery),
            Err(expected),
        );
        let mut out = [0x99; quality_decision::RECORD_BYTES];
        assert_eq!(
            quality_decision::encode(
                decision(true),
                binding(),
                &LIMITS,
                &mut out,
                direction,
                delivery,
            ),
            Err(expected),
        );
        assert!(out.iter().all(|byte| *byte == 0x99));
    }
}

#[test]
fn zero_sequence_and_out_of_range_duration_refuse_before_downcast() {
    let mut bytes = fixture(true);
    bytes[122..130].fill(0);
    assert_eq!(decode(&bytes), Err(WireError::InvalidValue));
    assert_eq!(
        encode(Decision {
            sequence: 0,
            ..decision(true)
        }),
        Err(WireError::InvalidValue)
    );
    for interval in [0_u64, 999, 1_000_001, u64::MAX] {
        let mut bytes = fixture(true);
        bytes[130..138].copy_from_slice(&interval.to_be_bytes());
        assert_eq!(decode(&bytes), Err(WireError::InvalidValue));
    }
    for interval in [1_000, 1_000_000] {
        let allowed = Decision {
            capture_interval_us: interval,
            ..decision(true)
        };
        assert_eq!(decode(&encode(allowed).unwrap()), Ok(allowed));
    }
    for interval in [0, 999, 1_000_001, u32::MAX] {
        assert_eq!(
            encode(Decision {
                capture_interval_us: interval,
                ..decision(true)
            }),
            Err(WireError::InvalidValue),
        );
    }
}

#[test]
fn bitrate_options_have_canonical_flags_values_and_original_applied_state() {
    for flag in [140, 145] {
        let mut bytes = fixture(true);
        bytes[flag] = 2;
        assert_eq!(decode(&bytes), Err(WireError::InvalidValue));
        bytes[flag] = 0;
        assert_eq!(decode(&bytes), Err(WireError::InvalidValue));
    }
    let mut bytes = fixture(true);
    bytes[140..145].fill(0);
    assert_eq!(decode(&bytes), Err(WireError::InvalidValue));
    assert_eq!(
        encode(Decision {
            applied_bitrate_bps: None,
            ..decision(true)
        }),
        Err(WireError::InvalidValue),
    );
    for bitrate in [0_u32, 9_999, 200_000_001, u32::MAX] {
        for range in [141..145, 146..150] {
            let mut bytes = fixture(true);
            bytes[range].copy_from_slice(&bitrate.to_be_bytes());
            assert_eq!(decode(&bytes), Err(WireError::InvalidValue));
        }
        for invalid in [
            Decision {
                applied_bitrate_bps: Some(bitrate),
                ..decision(true)
            },
            Decision {
                pending_bitrate_bps: Some(bitrate),
                ..decision(true)
            },
        ] {
            assert_eq!(encode(invalid), Err(WireError::InvalidValue));
        }
    }
    for bitrate in [10_000, 200_000_000] {
        let allowed = Decision {
            applied_bitrate_bps: Some(bitrate),
            pending_bitrate_bps: Some(bitrate),
            ..decision(true)
        };
        assert_eq!(decode(&encode(allowed).unwrap()), Ok(allowed));
    }
}

#[test]
fn exact_enum_assignments_and_unknown_values_refuse() {
    for (mode, mode_byte) in [(Mode::Active, 0), (Mode::Idle, 1)] {
        for (reason, reason_byte) in [
            (Reason::Hold, 0),
            (Reason::SourceWork, 1),
            (Reason::SendAdmission, 2),
            (Reason::CaptureCredit, 3),
            (Reason::ReceiverBacklog, 4),
            (Reason::DecoderWork, 5),
            (Reason::HeadroomProbe, 6),
            (Reason::VerifiedIdle, 7),
            (Reason::ChangedAfterIdle, 8),
            (Reason::EvidenceGap, 9),
        ] {
            let report = Decision {
                mode,
                reason,
                ..decision(true)
            };
            let bytes = encode(report).unwrap();
            assert_eq!((bytes[138], bytes[139]), (mode_byte, reason_byte));
            assert_eq!(decode(&bytes), Ok(report));
        }
    }
    for (index, value) in [(121, 2), (138, 2), (139, 10)] {
        let mut bytes = fixture(true);
        bytes[index] = value;
        assert_eq!(
            decode(&bytes),
            Err(if index == 121 {
                WireError::UnsupportedVersion
            } else {
                WireError::InvalidValue
            })
        );
    }
}

#[test]
fn output_and_negotiated_record_budgets_remain_binding() {
    let small = ProtocolLimits::with_overrides(LimitOverrides {
        max_control_message_bytes: Some(149),
        ..LimitOverrides::default()
    })
    .unwrap();
    let mut out = [0xaa; quality_decision::RECORD_BYTES];
    assert_eq!(
        quality_decision::encode(
            decision(true),
            binding(),
            &small,
            &mut out,
            Direction::HostToViewer,
            Delivery::Reliable
        ),
        Err(WireError::ResourceLimit),
    );
    assert!(out.iter().all(|byte| *byte == 0xaa));
    assert_eq!(
        quality_decision::decode(
            &fixture(true),
            binding(),
            &small,
            Direction::HostToViewer,
            Delivery::Reliable
        ),
        Err(WireError::ResourceLimit),
    );
    assert_eq!(
        quality_decision::encode(
            decision(true),
            binding(),
            &LIMITS,
            &mut out[..149],
            Direction::HostToViewer,
            Delivery::Reliable
        ),
        Err(WireError::BufferTooSmall),
    );
    assert!(out.iter().all(|byte| *byte == 0xaa));
}

#[test]
fn framing_extensions_and_extra_fixed_payload_never_override_state() {
    let mut bytes = fixture(true);
    bytes.push(0);
    assert_eq!(decode(&bytes), Err(WireError::TrailingBytes));
    bytes[15] += 1;
    assert_eq!(decode(&bytes), Err(WireError::TrailingBytes));
    let mut bytes = fixture(true);
    bytes.extend_from_slice(&[0x70, 0, 0, 0, 0, 0, 0, 0]);
    bytes[15] += 8;
    bytes[23] = 8;
    assert_eq!(decode(&bytes), Ok(decision(true)));
    bytes[153] = 1;
    assert_eq!(decode(&bytes), Err(WireError::RequiredExtension));
    for index in [8, 10] {
        let mut bytes = fixture(true);
        bytes[index] = 1;
        assert_eq!(decode(&bytes), Err(WireError::InvalidFlags));
    }
}
