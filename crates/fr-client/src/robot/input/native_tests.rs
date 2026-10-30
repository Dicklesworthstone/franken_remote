use super::*;
use fr_core::ids::{InputLeaseId, RemoteSessionId};
use fr_wire::input_result::Reason;

fn scope() -> Scope {
    Scope {
        connection: [1; 16],
        attempt: 1,
        lease_generation: 1,
    }
}
fn key(id: u64) -> RequestKey {
    let mut fingerprint = [0; 32];
    fingerprint[..8].copy_from_slice(&id.to_be_bytes());
    RequestKey { id, fingerprint }
}
fn binding() -> ResultBinding {
    ResultBinding {
        channel: 17,
        session: RemoteSessionId::from_raw(23),
        lease: InputLeaseId::from_raw(31),
    }
}
fn submitted(sequence: u64) -> InputResult {
    InputResult {
        binding: binding(),
        sequence,
        space: SequenceSpace::Action,
        stage: Stage::SubmittedToOs,
        outcome: InputOutcome::SubmittedToOs,
        submitted_operations: 2,
        unknown_next_operation: false,
        reason: None,
    }
}
fn start(ledger: &mut Ledger, id: u64, now: u64, count: u8) {
    assert_eq!(
        ledger.begin(scope(), key(id), now, now + 100, count),
        Ok(Admission::Started)
    );
}
fn report(ledger: &Ledger, id: u64) -> Report {
    let Some(Admission::Retained(report)) = ledger.lookup(scope(), key(id)).unwrap() else {
        panic!("expected retained terminal report");
    };
    report
}

#[test]
fn original_reliable_receipt_is_required_and_duplicate_never_adds_effects() {
    let mut ledger = Ledger::new(scope()).unwrap();
    start(&mut ledger, 1, 10, 1);
    assert!(ledger.ready(1, 10).unwrap());
    ledger.encoded(1, binding(), 8).unwrap();
    assert!(!ledger.ready(1, 11).unwrap());
    for foreign in [
        ResultBinding {
            channel: 18,
            ..binding()
        },
        ResultBinding {
            session: RemoteSessionId::from_raw(24),
            ..binding()
        },
        ResultBinding {
            lease: InputLeaseId::from_raw(32),
            ..binding()
        },
    ] {
        assert_eq!(
            ledger.result(ResultEvent::Completed(InputResult {
                binding: foreign,
                ..submitted(8)
            })),
            Ok(None)
        );
    }
    assert_eq!(
        ledger.result(ResultEvent::Completed(submitted(7))),
        Ok(None)
    );
    assert_eq!(ledger.result(ResultEvent::Pointer(submitted(8))), Ok(None));
    assert_eq!(ledger.result(ResultEvent::Unretained), Ok(None));
    let actual = ledger
        .result(ResultEvent::Completed(submitted(8)))
        .unwrap()
        .unwrap();
    assert_eq!(actual.disposition, InputDisposition::Committed);
    assert_eq!(actual.submitted_operations, 2);
    assert_eq!(actual.actions_submitted, 1);
    assert_eq!(actual.acknowledged_actions, 1);
    assert_eq!(actual.unacknowledged_actions, 0);
    assert_eq!(actual.stage, Some(AcknowledgementStage::SubmittedToOs));
    assert_eq!(
        ledger.result(ResultEvent::Duplicate(submitted(8))),
        Ok(None)
    );
    assert_eq!(
        ledger.begin(scope(), key(1), 12, 999, 1),
        Ok(Admission::Retained(actual))
    );
    assert!(!ledger.is_closed());
}

#[test]
fn receipt_eviction_keeps_consumed_floor_and_changed_ids_cannot_be_rebound() {
    let mut ledger = Ledger::new(scope()).unwrap();
    for id in 1..=u64::try_from(RETAINED_REPORTS + 1).unwrap() {
        start(&mut ledger, id, id * 1000, 1);
        ledger.encoded(id, binding(), id).unwrap();
        ledger
            .result(ResultEvent::Completed(submitted(id)))
            .unwrap()
            .unwrap();
    }
    assert_eq!(ledger.lookup(scope(), key(1)), Err(Error::Consumed));
    assert_eq!(
        ledger.begin(scope(), key(1), 20_000, 20_100, 1),
        Err(Error::Consumed)
    );
    let id = ledger.consumed_through();
    let changed = RequestKey {
        fingerprint: [77; 32],
        ..key(id)
    };
    assert_eq!(ledger.lookup(scope(), changed), Err(Error::RequestChanged));
    let other = Scope {
        lease_generation: 2,
        ..scope()
    };
    assert_eq!(ledger.lookup(other, key(id)), Err(Error::WrongScope));
}

#[test]
fn pending_duplicate_keeps_deadline_and_busy_refusal_does_not_displace_owner() {
    let mut ledger = Ledger::new(scope()).unwrap();
    start(&mut ledger, 1, 10, 1);
    assert_eq!(
        ledger.begin(scope(), key(1), 11, 1000, 1),
        Ok(Admission::Pending)
    );
    assert_eq!(
        ledger.begin(
            scope(),
            RequestKey {
                fingerprint: [2; 32],
                ..key(1)
            },
            12,
            1000,
            1
        ),
        Err(Error::RequestChanged)
    );
    let Admission::Retained(busy) = ledger.begin(scope(), key(2), 13, 1000, 1).unwrap() else {
        panic!("busy refusal required");
    };
    assert_eq!(busy.end, End::Busy);
    assert_eq!(busy.disposition, InputDisposition::Refused);
    ledger.encoded(1, binding(), 3).unwrap();
    ledger
        .result(ResultEvent::Completed(submitted(3)))
        .unwrap()
        .unwrap();
    assert_eq!(
        ledger.lookup(scope(), key(2)),
        Ok(Some(Admission::Retained(busy)))
    );
    assert_eq!(ledger.consumed_through(), 2);
}

#[test]
fn timeout_before_encoding_is_definite_and_cannot_be_retimed_by_retry() {
    let mut ledger = Ledger::new(scope()).unwrap();
    start(&mut ledger, 1, 10, 1);
    let Admission::Retained(expired) = ledger.begin(scope(), key(1), 110, 1000, 1).unwrap() else {
        panic!("original deadline expired");
    };
    assert_eq!(expired.end, End::Expired);
    assert_eq!(expired.disposition, InputDisposition::Refused);
    assert_eq!(expired.actions_encoded, 0);
    assert_eq!(expired.stage, None);
    assert_eq!(ledger.encoded(1, binding(), 1), Err(Error::NoPending));
    start(&mut ledger, 2, 111, 1);
    assert!(ledger.ready(2, 112).unwrap());
}

#[test]
fn timeout_after_encoding_is_unknown_and_late_receipt_never_reopens_authority() {
    let mut ledger = Ledger::new(scope()).unwrap();
    start(&mut ledger, 1, 10, 1);
    ledger.encoded(1, binding(), 7).unwrap();
    assert_eq!(ledger.expire(109), Ok(None));
    let expired = ledger.expire(110).unwrap().unwrap();
    assert_eq!(expired.disposition, InputDisposition::UnknownEffect);
    assert_eq!(expired.actions_encoded, 1);
    assert_eq!(expired.submitted_operations, 0);
    assert_eq!(expired.unacknowledged_actions, 1);
    assert!(!expired.unknown_next_operation);
    assert!(ledger.is_closed());
    let late = ledger
        .result(ResultEvent::Completed(submitted(7)))
        .unwrap()
        .unwrap();
    assert_eq!(late.disposition, InputDisposition::Committed);
    assert_eq!(late.submitted_operations, 2);
    assert_eq!(late.end, End::Expired);
    assert_eq!(late.unacknowledged_actions, 0);
    assert!(!late.unknown_next_operation);
    assert_eq!(
        ledger.begin(scope(), key(2), 111, 200, 1),
        Err(Error::Closed)
    );
}

#[test]
fn host_partial_unknown_expired_and_cancelled_keep_the_actual_stage_and_prefix() {
    for (outcome, count, reason, expected) in [
        (
            InputOutcome::PartiallySubmittedToOs,
            1,
            Reason::PlatformUnavailable,
            InputDisposition::Partial,
        ),
        (
            InputOutcome::EffectUnknown,
            1,
            Reason::UnknownEffect,
            InputDisposition::UnknownEffect,
        ),
        (
            InputOutcome::ExpiredBeforeSubmission,
            0,
            Reason::TicketExpired,
            InputDisposition::Refused,
        ),
        (
            InputOutcome::CancelledBeforeSubmission,
            0,
            Reason::Revoked,
            InputDisposition::Refused,
        ),
    ] {
        let mut ledger = Ledger::new(scope()).unwrap();
        start(&mut ledger, 1, 0, 1);
        ledger.encoded(1, binding(), 9).unwrap();
        let receipt = InputResult {
            outcome,
            submitted_operations: count,
            unknown_next_operation: outcome == InputOutcome::EffectUnknown,
            reason: Some(reason),
            stage: if count == 0 {
                Stage::Admitted
            } else {
                Stage::SubmittedToOs
            },
            ..submitted(9)
        };
        let actual = ledger
            .result(ResultEvent::Completed(receipt))
            .unwrap()
            .unwrap();
        assert_eq!(actual.disposition, expected);
        assert_eq!(actual.submitted_operations, count);
        assert_eq!(actual.host_outcome, Some(outcome.into()));
        assert_eq!(actual.host_reason, Some(reason as u16));
        assert_eq!(
            actual.unknown_next_operation,
            outcome == InputOutcome::EffectUnknown
        );
        assert!(ledger.is_closed());
    }
}

#[test]
fn multi_action_prefix_is_never_rolled_back_or_confused_with_complete_submission() {
    let mut ledger = Ledger::new(scope()).unwrap();
    start(&mut ledger, 1, 0, 2);
    ledger.encoded(1, binding(), 1).unwrap();
    assert_eq!(
        ledger.result(ResultEvent::Completed(submitted(1))),
        Ok(None)
    );
    assert!(ledger.ready(1, 1).unwrap());
    let expired = ledger.expire(100).unwrap().unwrap();
    assert_eq!(expired.disposition, InputDisposition::Partial);
    assert_eq!(expired.actions_submitted, 1);
    assert_eq!(expired.actions_encoded, 1);
    assert_eq!(expired.submitted_operations, 2);
    assert!(!expired.unknown_next_operation);
    assert!(ledger.is_closed());
}

#[test]
fn second_action_uses_same_binding_new_sequence_and_cannot_overclaim_observed_stage() {
    let mut ledger = Ledger::new(scope()).unwrap();
    start(&mut ledger, 1, 0, 2);
    ledger.encoded(1, binding(), 3).unwrap();
    ledger
        .result(ResultEvent::Completed(InputResult {
            stage: Stage::Observed,
            ..submitted(3)
        }))
        .unwrap();
    ledger.encoded(1, binding(), 4).unwrap();
    let complete = ledger
        .result(ResultEvent::Completed(submitted(4)))
        .unwrap()
        .unwrap();
    assert_eq!(complete.disposition, InputDisposition::Committed);
    assert_eq!(complete.submitted_operations, 4);
    assert_eq!(complete.stage, Some(AcknowledgementStage::SubmittedToOs));
}

#[test]
fn local_rejection_and_retirement_preserve_whether_sending_was_possible() {
    for encoded in [false, true] {
        for retired in [false, true] {
            let mut ledger = Ledger::new(scope()).unwrap();
            start(&mut ledger, 1, 0, 1);
            if encoded {
                ledger.encoded(1, binding(), 5).unwrap();
            }
            let outcome = if retired {
                ledger.retire().unwrap()
            } else {
                ledger.reject(1).unwrap()
            };
            assert_eq!(
                outcome.disposition,
                if encoded {
                    InputDisposition::UnknownEffect
                } else {
                    InputDisposition::Refused
                }
            );
            assert_eq!(ledger.is_closed(), encoded || retired);
            assert_eq!(report(&ledger, 1), outcome);
        }
    }
}

#[test]
fn invalid_receipt_and_conflicting_duplicate_fence_new_work_without_rewriting_evidence() {
    let mut ledger = Ledger::new(scope()).unwrap();
    start(&mut ledger, 1, 0, 1);
    ledger.encoded(1, binding(), 1).unwrap();
    let confirmed = ledger
        .result(ResultEvent::Completed(submitted(1)))
        .unwrap()
        .unwrap();
    start(&mut ledger, 2, 1, 1);
    ledger.encoded(2, binding(), 2).unwrap();
    assert_eq!(
        ledger.result(ResultEvent::Duplicate(InputResult {
            submitted_operations: 1,
            ..submitted(1)
        })),
        Err(Error::ConflictingReceipt)
    );
    assert_eq!(report(&ledger, 1), confirmed);
    assert_eq!(
        report(&ledger, 2).disposition,
        InputDisposition::UnknownEffect
    );
    assert!(ledger.is_closed());

    let mut ledger = Ledger::new(scope()).unwrap();
    start(&mut ledger, 1, 0, 1);
    ledger.encoded(1, binding(), 1).unwrap();
    for invalid in [
        InputResult {
            submitted_operations: 0,
            ..submitted(1)
        },
        InputResult {
            reason: Some(Reason::Revoked),
            ..submitted(1)
        },
        InputResult {
            stage: Stage::Admitted,
            ..submitted(1)
        },
    ] {
        assert_eq!(
            ledger.result(ResultEvent::Completed(invalid)),
            Err(Error::InvalidReceipt)
        );
        assert_eq!(
            report(&ledger, 1).disposition,
            InputDisposition::UnknownEffect
        );
        assert!(ledger.is_closed());
    }
}

#[test]
fn clock_regression_retires_pending_effects_without_reopening_the_scope() {
    let mut ledger = Ledger::new(scope()).unwrap();
    start(&mut ledger, 1, 10, 1);
    ledger.encoded(1, binding(), 3).unwrap();
    assert_eq!(ledger.expire(9), Err(Error::ClockRegression));
    let actual = report(&ledger, 1);
    assert_eq!(actual.end, End::ClockRegression);
    assert_eq!(actual.disposition, InputDisposition::UnknownEffect);
    assert_eq!(actual.unacknowledged_actions, 1);
    assert_eq!(
        ledger.begin(scope(), key(2), 11, 100, 1),
        Err(Error::Closed)
    );
}

#[test]
fn scope_bounds_and_request_exhaustion_fail_closed_without_wrapping() {
    for invalid in [
        Scope {
            connection: [0; 16],
            ..scope()
        },
        Scope {
            attempt: 0,
            ..scope()
        },
        Scope {
            attempt: 33,
            ..scope()
        },
        Scope {
            lease_generation: 0,
            ..scope()
        },
    ] {
        assert!(matches!(Ledger::new(invalid), Err(Error::InvalidScope)));
    }
    let mut ledger = Ledger::new(scope()).unwrap();
    for (id, count, until) in [
        (0, 1, 10),
        (1, 0, 10),
        (1, MAX_ACTIONS + 1, 10),
        (1, 1, MAX_REQUEST_MICROS + 1),
    ] {
        assert_eq!(
            ledger.begin(scope(), key(id), 0, until, count),
            Err(Error::InvalidRequest)
        );
        assert_eq!(ledger.consumed_through(), 0);
    }
    start(&mut ledger, u64::MAX, 0, 1);
    let refused = ledger.reject(u64::MAX).unwrap();
    assert_eq!(refused.disposition, InputDisposition::Refused);
    assert_eq!(ledger.consumed_through(), u64::MAX);
    assert_eq!(
        ledger.begin(scope(), key(1), 1, 100, 1),
        Err(Error::Consumed)
    );
    assert_eq!(
        ledger.lookup(scope(), key(u64::MAX)),
        Ok(Some(Admission::Retained(refused)))
    );
}

#[test]
fn original_sequence_and_actual_authority_cannot_be_rebound_for_later_requests() {
    let mut ledger = Ledger::new(scope()).unwrap();
    start(&mut ledger, 1, 0, 1);
    ledger.encoded(1, binding(), 0).unwrap();
    ledger.result(ResultEvent::Completed(submitted(0))).unwrap();
    start(&mut ledger, 2, 1, 1);
    ledger.encoded(2, binding(), 1).unwrap();
    assert_eq!(
        ledger.result(ResultEvent::Duplicate(submitted(0))),
        Ok(None)
    );
    assert_eq!(ledger.lookup(scope(), key(2)), Ok(Some(Admission::Pending)));
    let actual = ledger
        .result(ResultEvent::Completed(submitted(1)))
        .unwrap()
        .unwrap();
    assert_eq!(actual.disposition, InputDisposition::Committed);
    assert_eq!(actual.submitted_operations, 2);
}

#[test]
fn terminal_reports_roundtrip_without_exposing_authority_or_action_content() {
    let mut ledger = Ledger::new(scope()).unwrap();
    start(&mut ledger, 1, 0, 1);
    ledger.encoded(1, binding(), 3).unwrap();
    let report = ledger
        .result(ResultEvent::Completed(submitted(3)))
        .unwrap()
        .unwrap();
    let json = serde_json::to_string(&report).unwrap();
    let decoded: Report = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded, report);
    assert_eq!(decoded.validate(), Ok(()));
    for forbidden in [
        "binding",
        "lease",
        "ticket",
        "fingerprint",
        "connection",
        "coordinates",
        "text",
    ] {
        assert!(!json.contains(forbidden));
    }
    let mut value = serde_json::to_value(report).unwrap();
    value["unexpected_authority"] = serde_json::Value::Bool(true);
    assert!(serde_json::from_value::<Report>(value).is_err());
    assert!(!format!("{:?}", scope()).contains("connection"));
    assert!(!format!("{:?}", key(1)).contains("fingerprint"));
}

#[test]
fn malformed_ipc_reports_cannot_turn_incomplete_work_into_success() {
    let mut ledger = Ledger::new(scope()).unwrap();
    start(&mut ledger, 1, 0, 1);
    ledger.encoded(1, binding(), 3).unwrap();
    let complete = ledger
        .result(ResultEvent::Completed(submitted(3)))
        .unwrap()
        .unwrap();
    for invalid in [
        Report {
            request_id: 0,
            ..complete
        },
        Report {
            actions_total: 0,
            ..complete
        },
        Report {
            actions_total: MAX_ACTIONS + 1,
            ..complete
        },
        Report {
            actions_encoded: 0,
            ..complete
        },
        Report {
            acknowledged_actions: 0,
            ..complete
        },
        Report {
            acknowledged_actions: u8::MAX,
            ..complete
        },
        Report {
            unacknowledged_actions: 1,
            ..complete
        },
        Report {
            actions_submitted: 0,
            ..complete
        },
        Report {
            submitted_operations: 0,
            ..complete
        },
        Report {
            submitted_operations: u32::MAX,
            ..complete
        },
        Report {
            stage: None,
            ..complete
        },
        Report {
            stage: Some(AcknowledgementStage::Admitted),
            ..complete
        },
        Report {
            host_outcome: None,
            ..complete
        },
        Report {
            host_reason: Some(Reason::Revoked as u16),
            ..complete
        },
        Report {
            unknown_next_operation: true,
            ..complete
        },
        Report {
            end: End::Busy,
            ..complete
        },
        Report {
            disposition: InputDisposition::Refused,
            ..complete
        },
        Report {
            actions_total: 2,
            disposition: InputDisposition::Partial,
            ..complete
        },
    ] {
        let json = serde_json::to_string(&invalid).unwrap();
        let decoded: Report = serde_json::from_str(&json).unwrap();
        assert_eq!(
            decoded.validate(),
            Err(Error::InvalidReport),
            "accepted inconsistent report: {invalid:?}"
        );
    }
}

#[test]
fn report_validation_preserves_partial_prefixes_and_wholly_missing_receipts() {
    for encoded_second in [false, true] {
        let mut ledger = Ledger::new(scope()).unwrap();
        start(&mut ledger, 1, 0, 2);
        ledger.encoded(1, binding(), 4).unwrap();
        ledger
            .result(ResultEvent::Completed(InputResult {
                stage: Stage::Observed,
                ..submitted(4)
            }))
            .unwrap();
        if encoded_second {
            ledger.encoded(1, binding(), 5).unwrap();
        }
        let actual = ledger.expire(100).unwrap().unwrap();
        assert_eq!(actual.validate(), Ok(()));
        assert_eq!(actual.submitted_operations, 2);
        assert_eq!(actual.stage, Some(AcknowledgementStage::Observed));
        assert_eq!(actual.unacknowledged_actions, u8::from(encoded_second));
        assert!(!actual.unknown_next_operation);
        assert_eq!(
            actual.disposition,
            if encoded_second {
                InputDisposition::UnknownEffect
            } else {
                InputDisposition::Partial
            }
        );
        assert_eq!(
            Report {
                disposition: InputDisposition::Committed,
                ..actual
            }
            .validate(),
            Err(Error::InvalidReport)
        );
    }
    for outcome in [
        InputOutcome::AppliedLocally,
        InputOutcome::RejectedBeforeSubmission,
        InputOutcome::PartiallySubmittedToOs,
        InputOutcome::EffectUnknown,
    ] {
        let mut ledger = Ledger::new(scope()).unwrap();
        start(&mut ledger, 1, 0, 1);
        ledger.encoded(1, binding(), 1).unwrap();
        let receipt = InputResult {
            outcome,
            stage: if outcome == InputOutcome::PartiallySubmittedToOs {
                Stage::SubmittedToOs
            } else {
                Stage::Admitted
            },
            submitted_operations: u32::from(outcome == InputOutcome::PartiallySubmittedToOs),
            unknown_next_operation: outcome == InputOutcome::EffectUnknown,
            reason: match outcome {
                InputOutcome::AppliedLocally => None,
                InputOutcome::EffectUnknown => Some(Reason::UnknownEffect),
                _ => Some(Reason::PlatformUnavailable),
            },
            ..submitted(1)
        };
        let actual = ledger
            .result(ResultEvent::Completed(receipt))
            .unwrap()
            .unwrap();
        assert_eq!(actual.validate(), Ok(()));
        let invalid_reason = if outcome == InputOutcome::AppliedLocally {
            Some(0)
        } else {
            None
        };
        assert_eq!(
            Report {
                host_reason: invalid_reason,
                ..actual
            }
            .validate(),
            Err(Error::InvalidReport)
        );
    }
}

#[test]
fn post_encoder_mismatches_preserve_uncertainty_instead_of_claiming_zero_effects() {
    for foreign in [
        (binding(), 0),
        (
            ResultBinding {
                channel: 0,
                ..binding()
            },
            1,
        ),
        (
            ResultBinding {
                lease: InputLeaseId::from_raw(99),
                ..binding()
            },
            1,
        ),
    ] {
        let mut ledger = Ledger::new(scope()).unwrap();
        start(&mut ledger, 1, 0, 1);
        ledger.encoded(1, binding(), 0).unwrap();
        ledger.result(ResultEvent::Completed(submitted(0))).unwrap();
        start(&mut ledger, 2, 1, 1);
        assert!(ledger.ready(2, 1).unwrap());
        assert_eq!(
            ledger.encoded(2, foreign.0, foreign.1),
            Err(Error::InvalidSequence)
        );
        let unknown = report(&ledger, 2);
        assert_eq!(unknown.actions_encoded, 1);
        assert_eq!(unknown.unacknowledged_actions, 1);
        assert_eq!(unknown.disposition, InputDisposition::UnknownEffect);
        assert_eq!(unknown.validate(), Ok(()));
        assert!(ledger.is_closed());
        assert_eq!(
            ledger.result(ResultEvent::Completed(InputResult {
                binding: foreign.0,
                ..submitted(foreign.1)
            })),
            Ok(None)
        );
        assert_eq!(report(&ledger, 2), unknown);
    }
    let mut ledger = Ledger::new(scope()).unwrap();
    start(&mut ledger, 1, 0, 2);
    ledger.encoded(1, binding(), 0).unwrap();
    assert_eq!(ledger.encoded(1, binding(), 1), Err(Error::ActionPending));
    let unknown = report(&ledger, 1);
    assert_eq!(unknown.actions_encoded, 2);
    assert_eq!(unknown.unacknowledged_actions, 2);
    assert_eq!(unknown.validate(), Ok(()));
    let prefix = ledger
        .result(ResultEvent::Completed(submitted(0)))
        .unwrap()
        .unwrap();
    assert_eq!(prefix.disposition, InputDisposition::UnknownEffect);
    assert_eq!(prefix.submitted_operations, 2);
    assert_eq!(prefix.unacknowledged_actions, 1);
}

#[test]
fn late_failed_receipt_cannot_erase_an_untracked_post_encoder_effect() {
    for outcome in [
        InputOutcome::RejectedBeforeSubmission,
        InputOutcome::PartiallySubmittedToOs,
        InputOutcome::EffectUnknown,
    ] {
        let mut ledger = Ledger::new(scope()).unwrap();
        start(&mut ledger, 1, 0, 2);
        ledger.encoded(1, binding(), 0).unwrap();
        assert_eq!(ledger.encoded(1, binding(), 1), Err(Error::ActionPending));
        let prefix = u32::from(outcome == InputOutcome::PartiallySubmittedToOs);
        let actual = ledger
            .result(ResultEvent::Completed(InputResult {
                outcome,
                submitted_operations: prefix,
                stage: if prefix == 0 {
                    Stage::Admitted
                } else {
                    Stage::SubmittedToOs
                },
                unknown_next_operation: outcome == InputOutcome::EffectUnknown,
                reason: Some(if outcome == InputOutcome::EffectUnknown {
                    Reason::UnknownEffect
                } else {
                    Reason::PlatformUnavailable
                }),
                ..submitted(0)
            }))
            .unwrap()
            .unwrap();
        assert_eq!(actual.validate(), Ok(()));
        assert_eq!(actual.acknowledged_actions, 1);
        assert_eq!(actual.unacknowledged_actions, 1);
        assert_eq!(actual.disposition, InputDisposition::UnknownEffect);
        assert_eq!(actual.submitted_operations, prefix);
        assert_eq!(actual.end, End::Stopped);
    }
}
