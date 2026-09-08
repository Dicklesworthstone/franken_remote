use fr_core::{
    authority::{AuthorityError, Phase},
    ids::{InputLeaseId, RemoteSessionId},
    input_sequence::{InputOutcome, InputSequenceError},
    input_submission::{PlatformError, Receipt, Refusal},
    limits::{LimitOverrides, ProtocolLimits},
};
use fr_wire::{WireError, input::*, input_result::*};

fn binding() -> ResultBinding {
    ResultBinding {
        channel: 9,
        session: RemoteSessionId::from_raw(1),
        lease: InputLeaseId::from_raw(2),
    }
}
fn hex(s: &str) -> Vec<u8> {
    let (pairs, tail) = s.trim().as_bytes().as_chunks::<2>();
    assert!(tail.is_empty());
    pairs
        .iter()
        .map(|p| u8::from_str_radix(core::str::from_utf8(p).unwrap(), 16).unwrap())
        .collect()
}
fn fixtures() -> Vec<(Vec<u8>, InputOutcome, u32, Option<Reason>)> {
    use InputOutcome as O;
    vec![
        (
            hex(include_str!("fixtures/input/result-submitted.hex")),
            O::SubmittedToOs,
            2,
            None,
        ),
        (
            hex(include_str!("fixtures/input/result-local.hex")),
            O::AppliedLocally,
            0,
            None,
        ),
        (
            hex(include_str!("fixtures/input/result-refused.hex")),
            O::RejectedBeforeSubmission,
            0,
            Some(Reason::Unsupported),
        ),
        (
            hex(include_str!("fixtures/input/result-expired.hex")),
            O::ExpiredBeforeSubmission,
            0,
            Some(Reason::TicketExpired),
        ),
        (
            hex(include_str!("fixtures/input/result-cancelled.hex")),
            O::CancelledBeforeSubmission,
            0,
            Some(Reason::Revoked),
        ),
        (
            hex(include_str!("fixtures/input/result-partial.hex")),
            O::PartiallySubmittedToOs,
            1,
            Some(Reason::TicketExpired),
        ),
        (
            hex(include_str!("fixtures/input/result-unknown.hex")),
            O::EffectUnknown,
            2,
            Some(Reason::UnknownEffect),
        ),
        (
            hex(include_str!("fixtures/input/result-unknown-first.hex")),
            O::EffectUnknown,
            0,
            Some(Reason::UnknownEffect),
        ),
        (
            hex(include_str!("fixtures/input/result-pointer.hex")),
            O::SubmittedToOs,
            1,
            None,
        ),
        (
            hex(include_str!("fixtures/input/result-observed.hex")),
            O::SubmittedToOs,
            2,
            None,
        ),
    ]
}
fn decode(bytes: &[u8]) -> Result<InputResult, WireError> {
    decode_input_result(
        bytes,
        &ProtocolLimits::ABSOLUTE,
        binding(),
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
}
fn encode(result: InputResult) -> Result<Vec<u8>, WireError> {
    let mut bytes = vec![0; INPUT_RESULT_BYTES];
    let n = encode_input_result(
        result,
        &mut bytes,
        &ProtocolLimits::ABSOLUTE,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )?;
    assert_eq!(n, bytes.len());
    Ok(bytes)
}

#[test]
fn independent_fixtures_pin_every_outcome_and_stage() {
    for (bytes, outcome, count, reason) in fixtures() {
        let result = decode(&bytes).unwrap();
        assert_eq!(result.binding, binding());
        assert_eq!(result.sequence, 8);
        assert_eq!(result.outcome, outcome);
        assert_eq!(result.submitted_operations, count);
        assert_eq!(result.reason, reason);
        assert_eq!(
            result.unknown_next_operation,
            outcome == InputOutcome::EffectUnknown
        );
        assert_eq!(encode(result).unwrap(), bytes);
    }
    assert_eq!(
        decode(&hex(include_str!("fixtures/input/result-observed.hex")))
            .unwrap()
            .stage,
        Stage::Observed
    );
    assert_eq!(
        decode(&hex(include_str!("fixtures/input/result-pointer.hex")))
            .unwrap()
            .space,
        SequenceSpace::Pointer
    );
}

#[test]
fn rejects_every_truncation_and_trailing_data() {
    for (bytes, ..) in fixtures() {
        for n in 0..bytes.len() {
            assert!(decode(&bytes[..n]).is_err(), "truncation {n}");
        }
        let mut extra = bytes;
        extra.push(0);
        assert_eq!(decode(&extra), Err(WireError::TrailingBytes));
        // Even an extra byte included in the declared fixed payload is invalid.
        extra[15] += 1;
        assert_eq!(decode(&extra), Err(WireError::TrailingBytes));
    }
}

#[test]
fn authenticated_direction_delivery_and_epoch_binding_are_required() {
    let bytes = &fixtures()[0].0;
    let result = decode(bytes).unwrap();
    for (direction, delivery, expected) in [
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
        assert_eq!(
            decode_input_result(
                bytes,
                &ProtocolLimits::ABSOLUTE,
                binding(),
                direction,
                delivery
            ),
            Err(expected)
        );
        let mut out = [0xaa; INPUT_RESULT_BYTES];
        assert_eq!(
            encode_input_result(
                result,
                &mut out,
                &ProtocolLimits::ABSOLUTE,
                direction,
                delivery
            ),
            Err(expected)
        );
        assert_eq!(out, [0xaa; INPUT_RESULT_BYTES]);
    }
    for expected in [
        ResultBinding {
            channel: 0,
            ..binding()
        },
        ResultBinding {
            channel: 10,
            ..binding()
        },
        ResultBinding {
            session: RemoteSessionId::from_raw(3),
            ..binding()
        },
        ResultBinding {
            lease: InputLeaseId::from_raw(3),
            ..binding()
        },
    ] {
        assert_eq!(
            decode_input_result(
                bytes,
                &ProtocolLimits::ABSOLUTE,
                expected,
                InputDirection::HostToViewer,
                InputDelivery::Reliable
            ),
            Err(WireError::InvalidBinding)
        );
    }
    assert!(
        decode_input(
            bytes,
            &ProtocolLimits::ABSOLUTE,
            9,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable
        )
        .is_err()
    );
    assert!(decode(&hex(include_str!("fixtures/input/key.hex"))).is_err());
}

#[test]
fn optional_extensions_are_bounded_and_required_extensions_refuse() {
    let mut bytes = fixtures()[0].0.clone();
    bytes[12..16].copy_from_slice(&61_u32.to_be_bytes());
    bytes[20..24].copy_from_slice(&11_u32.to_be_bytes());
    bytes.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0, 3, 9, 8, 7]);
    assert_eq!(decode(&bytes).unwrap(), decode(&fixtures()[0].0).unwrap());
    bytes[77] = 1;
    assert_eq!(decode(&bytes), Err(WireError::RequiredExtension));
    bytes[77] = 2;
    assert_eq!(decode(&bytes), Err(WireError::InvalidExtension));
    bytes[77] = 0;
    bytes[81] = 4;
    assert_eq!(decode(&bytes), Err(WireError::Truncated));
    bytes[20..24].copy_from_slice(&u32::MAX.to_be_bytes());
    assert_eq!(decode(&bytes), Err(WireError::InvalidExtension));
}

#[test]
fn malformed_envelope_and_scalar_values_fail_closed() {
    for (offset, value, expected) in [
        (0, b'X', WireError::BadMagic),
        (5, 1, WireError::UnsupportedVersion),
        (7, 0xff, WireError::UnsupportedKind),
        (9, 1, WireError::InvalidFlags),
        (11, 1, WireError::InvalidFlags),
        (64, 2, WireError::InvalidValue),
        (65, 3, WireError::InvalidValue),
        (66, 7, WireError::InvalidValue),
        (71, 2, WireError::InvalidValue),
        (73, 34, WireError::InvalidValue),
    ] {
        let mut bytes = fixtures()[0].0.clone();
        bytes[offset] = value;
        assert_eq!(decode(&bytes), Err(expected), "offset {offset}");
    }
}

#[test]
fn negotiated_byte_caps_and_output_storage_are_enforced_before_writes() {
    let limits = ProtocolLimits::with_overrides(LimitOverrides {
        max_control_message_bytes: Some(1024),
        ..LimitOverrides::default()
    })
    .unwrap();
    let mut bytes = fixtures()[0].0.clone();
    bytes.resize(1025, 0);
    assert_eq!(
        decode_input_result(
            &bytes,
            &limits,
            binding(),
            InputDirection::HostToViewer,
            InputDelivery::Reliable
        ),
        Err(WireError::ResourceLimit)
    );
    let result = decode(&fixtures()[0].0).unwrap();
    for n in 0..INPUT_RESULT_BYTES {
        let mut out = [0xaa; INPUT_RESULT_BYTES];
        assert_eq!(
            encode_input_result(
                result,
                &mut out[..n],
                &limits,
                InputDirection::HostToViewer,
                InputDelivery::Reliable
            ),
            Err(WireError::BufferTooSmall)
        );
        assert_eq!(out, [0xaa; INPUT_RESULT_BYTES]);
    }
}

#[test]
fn inconsistent_receipts_cannot_erase_effects_or_invent_observation() {
    let submitted = decode(&fixtures()[0].0).unwrap();
    let partial = decode(&fixtures()[5].0).unwrap();
    let unknown = decode(&fixtures()[6].0).unwrap();
    for result in [
        InputResult {
            submitted_operations: 0,
            ..submitted
        },
        InputResult {
            stage: Stage::Admitted,
            ..submitted
        },
        InputResult {
            reason: Some(Reason::PermissionMissing),
            ..submitted
        },
        InputResult {
            outcome: InputOutcome::ExpiredBeforeSubmission,
            ..partial
        },
        InputResult {
            reason: None,
            ..partial
        },
        InputResult {
            stage: Stage::Observed,
            ..partial
        },
        InputResult {
            reason: Some(Reason::UnknownEffect),
            ..partial
        },
        InputResult {
            unknown_next_operation: false,
            ..unknown
        },
        InputResult {
            stage: Stage::Observed,
            ..unknown
        },
        InputResult {
            reason: None,
            ..unknown
        },
        InputResult {
            space: SequenceSpace::Pointer,
            ..partial
        },
        InputResult {
            space: SequenceSpace::Pointer,
            ..unknown
        },
    ] {
        assert_eq!(encode(result), Err(WireError::InvalidValue));
    }
    for count in [4097, u32::MAX] {
        assert_eq!(
            encode(InputResult {
                submitted_operations: count,
                ..submitted
            }),
            Err(WireError::ResourceLimit)
        );
    }
    assert!(
        encode(InputResult {
            submitted_operations: 4096,
            ..submitted
        })
        .is_ok()
    );
    assert_eq!(
        encode(InputResult {
            submitted_operations: 4096,
            ..unknown
        }),
        Err(WireError::ResourceLimit)
    );
    assert_eq!(
        encode(InputResult {
            submitted_operations: 4096,
            ..partial
        }),
        Err(WireError::ResourceLimit)
    );
    let mut bytes = fixtures()[0].0.clone();
    bytes[67..71].copy_from_slice(&u32::MAX.to_be_bytes());
    assert_eq!(decode(&bytes), Err(WireError::ResourceLimit));
    bytes[67..71].copy_from_slice(&0_u32.to_be_bytes());
    assert_eq!(decode(&bytes), Err(WireError::InvalidValue));
}

#[test]
fn conversion_keeps_actual_prefix_and_never_promotes_to_observed() {
    let receipt = Receipt {
        sequence: u64::MAX,
        outcome: InputOutcome::EffectUnknown,
        submitted_operations: 2,
        refusal: Some(Refusal::UnknownEffect),
    };
    let result = InputResult::from_receipt(binding(), SequenceSpace::Action, receipt).unwrap();
    let received = decode(&encode(result).unwrap()).unwrap();
    assert_eq!(received.sequence, u64::MAX);
    assert_eq!(received.submitted_operations, 2);
    assert_eq!(received.stage, Stage::SubmittedToOs);
    assert!(received.unknown_next_operation);
    assert_eq!(format!("{:?}", binding()), "ResultBinding");
    assert!(!format!("{received:?}").contains("lease:"));
}

#[test]
fn stable_reason_codes_cover_authority_sequence_and_platform_refusals() {
    use AuthorityError as A;
    use InputSequenceError as S;
    use Refusal as R;
    let cases = [
        (
            R::Authority(A::InvalidState {
                phase: Phase::Closed,
            }),
            1,
        ),
        (R::Authority(A::ObservationExpired), 2),
        (R::Authority(A::NoLease), 3),
        (R::Authority(A::StaleLease), 4),
        (R::Authority(A::LeaseExpired), 5),
        (R::Authority(A::ViewUnready), 6),
        (R::Authority(A::ControllerBusy), 7),
        (R::Authority(A::ControllerCleanupRequired), 8),
        (R::Authority(A::TicketInvalid), 9),
        (R::Authority(A::TicketExpired), 10),
        (R::Authority(A::ChallengeMismatch), 11),
        (R::Authority(A::ChallengePending), 12),
        (R::Authority(A::ChallengeExpired), 13),
        (R::Authority(A::ClockRegression), 14),
        (R::Authority(A::DeadlineOverflow), 15),
        (
            R::Sequence(S::InvalidCapacity {
                requested: usize::MAX,
            }),
            16,
        ),
        (R::Sequence(S::Fenced), 17),
        (
            R::Sequence(S::PreviousActionPending { sequence: u64::MAX }),
            18,
        ),
        (
            R::Sequence(S::SequenceGap {
                expected: 1,
                received: u64::MAX,
            }),
            19,
        ),
        (R::Sequence(S::NotPending), 20),
        (R::StaleSession, 21),
        (R::StaleView, 22),
        (R::OutOfBounds, 23),
        (R::Unsupported, 24),
        (R::InvalidTransition, 25),
        (R::RelativeOverflow, 26),
        (R::ModeMismatch, 27),
        (R::Revoked, 28),
        (R::Platform(PlatformError::Permission), 29),
        (R::Platform(PlatformError::GeometryChanged), 30),
        (R::Platform(PlatformError::Unavailable), 31),
        (R::UnknownEffect, 32),
        (R::AuthorityUnavailable, 33),
        (R::StaleLease, 4),
        (R::Sequence(S::StaleLease), 4),
        (R::Platform(PlatformError::Unsupported), 24),
    ];
    for (refusal, code) in cases {
        let reason = Reason::try_from(refusal).unwrap();
        assert_eq!(reason as u16, code);
        let (outcome, submitted_operations) = if code == 32 {
            (InputOutcome::EffectUnknown, 2)
        } else {
            (InputOutcome::PartiallySubmittedToOs, 1)
        };
        let result = InputResult::from_receipt(
            binding(),
            SequenceSpace::Action,
            Receipt {
                sequence: 8,
                outcome,
                submitted_operations,
                refusal: Some(refusal),
            },
        )
        .unwrap();
        let bytes = encode(result).unwrap();
        assert_eq!(&bytes[72..74], &code.to_be_bytes());
        assert_eq!(decode(&bytes).unwrap().reason, Some(reason));
    }
}

#[test]
fn every_single_byte_mutation_is_refused_or_remains_canonical() {
    // Bounded adversarial sweep of the independently defined corpus. This is
    // not a coverage-guided fuzz campaign or independent-peer interoperability.
    for (bytes, ..) in fixtures() {
        for offset in 0..bytes.len() {
            for value in 0..=u8::MAX {
                let mut candidate = bytes.clone();
                candidate[offset] = value;
                if let Ok(result) = decode(&candidate) {
                    assert_eq!(
                        encode(result).unwrap(),
                        candidate,
                        "offset {offset}, value {value}"
                    );
                }
            }
        }
    }
}
