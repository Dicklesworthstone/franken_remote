//! Real local-owner state transitions with bounded, authenticated receipt types.
//! These tests never construct a desktop or claim that an OS action occurred.
use super::*;
use crate::robot_input::Operation;
use fr_client::{input::ResultEvent, robot::InputDisposition};
use fr_core::{
    ids::{
        CodecConfigurationGeneration, DisplayGeometryGeneration, InputLeaseId, RecoveryGeneration,
        RemoteSessionId, ViewportMappingGeneration,
    },
    input_sequence::InputOutcome,
};
use fr_wire::{
    SourceObservation,
    input_result::{InputResult, ResultBinding, SequenceSpace, Stage},
    presented::{Sample, Stamp},
};
use std::{future::Future, pin::Pin, task::Context};

fn view() -> InputView {
    InputView {
        geometry: DisplayGeometryGeneration::INITIAL,
        viewport: ViewportMappingGeneration::INITIAL,
        configuration: CodecConfigurationGeneration::INITIAL,
        recovery: RecoveryGeneration::INITIAL,
    }
}
fn candidate(age_upper_us: u64) -> Candidate {
    Candidate {
        view: view(),
        at: Instant::now(),
        sample: Sample {
            stamp: Stamp {
                frame: 7,
                captured_us: 10,
                observed_us: 10,
                source: SourceObservation::Captured,
            },
            age_upper_us,
        },
    }
}
fn fresh() -> Shared {
    let shared = Shared::new([1; 16], [2; 16]).unwrap();
    shared.0.lock().unwrap().live = Some(candidate(0));
    shared
}
fn snapshot(shared: &Shared) -> Snapshot {
    let Response::Status { snapshot } = shared.status() else {
        panic!("status must remain available");
    };
    assert_eq!(validate_snapshot(&snapshot), Ok(()));
    snapshot
}
fn request(snapshot: &Snapshot, request_id: u64) -> Request {
    assert!(snapshot.ready);
    Request {
        lease: crate::robot_input::id(snapshot.lease.as_deref().unwrap()).unwrap(),
        request_id,
        observation_id: snapshot.observation_id.unwrap(),
        max_observation_age_ms: 1000,
        operation: Operation::Click {
            button: 1,
            x: -100,
            y: 20,
        },
    }
}
fn binding() -> ResultBinding {
    ResultBinding {
        channel: 17,
        session: RemoteSessionId::from_raw(23),
        lease: InputLeaseId::from_raw(31),
    }
}
fn receipt(sequence: u64) -> InputResult {
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
fn retained(shared: &Shared, request: Request) -> native::Report {
    let reply = shared
        .0
        .lock()
        .unwrap()
        .lookup(State::key(request).unwrap())
        .unwrap()
        .expect("terminal report must be retained");
    report(&reply)
}
fn report(reply: &Response) -> native::Report {
    let Response::Result { report } = reply else {
        panic!("expected actual retained request evidence");
    };
    assert_eq!(report.validate(), Ok(()));
    *report
}
fn ready(shared: &Shared, id: u64) -> bool {
    let mut state = shared.0.lock().unwrap();
    let now = state.now(Instant::now()).unwrap();
    state.ledger.ready(id, now).unwrap()
}
/// Represent the original encoder's synchronous successful return. Only this
/// helper records an encoded action; submissions, status and retries cannot.
fn encoded(shared: &Shared, id: u64, sequence: u64) {
    assert!(ready(shared, id));
    let mut state = shared.0.lock().unwrap();
    state.ledger.encoded(id, binding(), sequence).unwrap();
    state.pending.as_mut().unwrap().encoded += 1;
}
fn poll(future: Pin<&mut impl Future<Output = Response>>) -> Poll<Response> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

#[test]
fn unknown_and_expired_observations_consume_the_original_request_without_encoding() {
    for expired in [false, true] {
        let shared = fresh();
        let first = snapshot(&shared);
        let mut request = request(&first, 1);
        if expired {
            let mut state = shared.0.lock().unwrap();
            state.pinned.as_mut().unwrap().1.at =
                Instant::now().checked_sub(Duration::from_secs(2)).unwrap();
            // A newer receipt for the same view cannot refresh the saved pin.
            state.live = Some(candidate(0));
        } else {
            request.observation_id += 1;
        }
        let expected = if expired {
            Refusal::ObservationExpired
        } else {
            Refusal::ObservationUnknown
        };
        assert_eq!(shared.0.lock().unwrap().submit(request), Err(expected));
        let refused = retained(&shared, request);
        assert_eq!(refused.disposition, InputDisposition::Refused);
        assert_eq!(refused.actions_encoded, 0);
        assert_eq!(refused.stage, None);
        // Even a new status cannot transform this same ID into a new action.
        let current = snapshot(&shared);
        assert_eq!(current.next_request_id, Some(2));
        assert_eq!(
            report(&shared.0.lock().unwrap().submit(request).unwrap().unwrap()),
            refused
        );
        let changed = Request {
            observation_id: current.observation_id.unwrap(),
            ..request
        };
        if changed.observation_id != request.observation_id {
            assert_eq!(
                shared.0.lock().unwrap().submit(changed),
                Err(Refusal::RequestChanged)
            );
        }
    }
}

#[test]
fn every_changed_view_generation_fences_the_pinned_request_before_encoding() {
    let original = view();
    for changed in [
        InputView {
            geometry: DisplayGeometryGeneration::from_raw(2),
            ..original
        },
        InputView {
            viewport: ViewportMappingGeneration::from_raw(2),
            ..original
        },
        InputView {
            configuration: CodecConfigurationGeneration::from_raw(2),
            ..original
        },
        InputView {
            recovery: RecoveryGeneration::from_raw(2),
            ..original
        },
    ] {
        let shared = fresh();
        let request = request(&snapshot(&shared), 1);
        shared.0.lock().unwrap().live.as_mut().unwrap().view = changed;
        assert_eq!(
            shared.0.lock().unwrap().submit(request),
            Err(Refusal::ViewChanged)
        );
        let refused = retained(&shared, request);
        assert_eq!(refused.actions_encoded, 0);
        assert_eq!(refused.disposition, InputDisposition::Refused);
        assert_eq!(snapshot(&shared).next_request_id, Some(2));
    }
}

#[test]
fn duplicate_pending_requests_keep_their_original_observation_and_cannot_change_payload() {
    let shared = fresh();
    let first = snapshot(&shared);
    let request = request(&first, 1);
    assert_eq!(shared.0.lock().unwrap().submit(request), Ok(None));
    encoded(&shared, 1, 0);
    let later = snapshot(&shared);
    assert_ne!(later.observation_id, first.observation_id);
    assert_eq!(shared.0.lock().unwrap().submit(request), Ok(None));
    assert!(!ready(&shared, 1));
    for changed in [
        Request {
            observation_id: later.observation_id.unwrap(),
            ..request
        },
        Request {
            max_observation_age_ms: 999,
            ..request
        },
        Request {
            operation: Operation::KeyTap { usage: 4 },
            ..request
        },
    ] {
        assert_eq!(
            shared.0.lock().unwrap().submit(changed),
            Err(Refusal::RequestChanged)
        );
    }
    // A refreshed status changes the next pin, never the source of live work.
    let state = shared.0.lock().unwrap();
    let pending = state.pending.as_ref().unwrap();
    assert_eq!(
        pending.request.observation_id,
        first.observation_id.unwrap()
    );
    assert_eq!(pending.encoded, 1);
    assert_eq!(pending.original.sample.stamp.frame, first.frame.unwrap());
}

#[test]
fn a_new_status_invalidates_an_unsubmitted_old_pin_without_refreshing_its_id() {
    let shared = fresh();
    let old = request(&snapshot(&shared), 1);
    let latest = snapshot(&shared);
    assert_ne!(old.observation_id, latest.observation_id.unwrap());
    assert_eq!(
        shared.0.lock().unwrap().submit(old),
        Err(Refusal::ObservationUnknown)
    );
    assert_eq!(retained(&shared, old).actions_encoded, 0);
    let refreshed = Request {
        observation_id: latest.observation_id.unwrap(),
        ..old
    };
    assert_eq!(
        shared.0.lock().unwrap().submit(refreshed),
        Err(Refusal::RequestChanged)
    );
}

#[test]
fn busy_requests_are_retained_and_cannot_execute_after_the_original_finishes() {
    let shared = fresh();
    let first = Request {
        operation: Operation::Scroll {
            x: 0,
            y: 0,
            dx: 0,
            dy: 1,
        },
        ..request(&snapshot(&shared), 1)
    };
    assert_eq!(shared.0.lock().unwrap().submit(first), Ok(None));
    let busy = Request {
        request_id: 2,
        ..first
    };
    let refusal = report(&shared.0.lock().unwrap().submit(busy).unwrap().unwrap());
    assert_eq!(refusal.end, native::End::Busy);
    assert_eq!(refusal.actions_encoded, 0);
    assert_eq!(refusal.disposition, InputDisposition::Refused);
    assert!(ready(&shared, 1));
    encoded(&shared, 1, 0);
    shared.result(ResultEvent::Completed(receipt(0)));
    assert_eq!(
        retained(&shared, first).disposition,
        InputDisposition::Committed
    );
    assert_eq!(
        report(&shared.0.lock().unwrap().submit(busy).unwrap().unwrap()),
        refusal
    );
    assert_eq!(snapshot(&shared).next_request_id, Some(3));
    let changed = Request {
        operation: Operation::KeyTap { usage: 4 },
        ..busy
    };
    assert_eq!(
        shared.0.lock().unwrap().submit(changed),
        Err(Refusal::RequestChanged)
    );
}

#[test]
fn only_original_reliable_receipts_advance_a_two_action_request() {
    let shared = fresh();
    let request = request(&snapshot(&shared), 1);
    assert_eq!(shared.0.lock().unwrap().submit(request), Ok(None));
    encoded(&shared, 1, 0);
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
        shared.result(ResultEvent::Completed(InputResult {
            binding: foreign,
            ..receipt(0)
        }));
        assert!(!ready(&shared, 1));
    }
    for event in [
        ResultEvent::Completed(receipt(1)),
        ResultEvent::Pointer(InputResult {
            space: SequenceSpace::Pointer,
            ..receipt(0)
        }),
        ResultEvent::Completed(InputResult {
            space: SequenceSpace::Pointer,
            ..receipt(0)
        }),
        ResultEvent::Unretained,
    ] {
        shared.result(event);
        assert!(!ready(&shared, 1));
    }
    shared.result(ResultEvent::Completed(receipt(0)));
    assert!(ready(&shared, 1));
    assert_eq!(
        shared
            .0
            .lock()
            .unwrap()
            .lookup(State::key(request).unwrap()),
        Ok(None)
    );
    encoded(&shared, 1, 1);
    shared.result(ResultEvent::Duplicate(receipt(0)));
    assert!(!ready(&shared, 1));
    shared.result(ResultEvent::Completed(receipt(1)));
    let complete = retained(&shared, request);
    assert_eq!(complete.disposition, InputDisposition::Committed);
    assert_eq!(complete.actions_encoded, 2);
    assert_eq!(complete.actions_submitted, 2);
    assert_eq!(complete.submitted_operations, 4);
    assert_eq!(
        report(&shared.0.lock().unwrap().submit(request).unwrap().unwrap()),
        complete
    );
    assert!(shared.0.lock().unwrap().pending.is_none());
}

#[test]
fn cancelled_waiters_fence_the_original_owner_and_keep_truthful_effects() {
    for progress in 0..=2 {
        let shared = fresh();
        let request = request(&snapshot(&shared), 1);
        let mut waiting = Box::pin(shared.submit(request));
        assert!(poll(waiting.as_mut()).is_pending());
        if progress != 0 {
            encoded(&shared, 1, 0);
        }
        if progress == 2 {
            shared.result(ResultEvent::Completed(receipt(0)));
        }
        drop(waiting);
        let actual = retained(&shared, request);
        assert_eq!(
            actual.disposition,
            match progress {
                0 => InputDisposition::Refused,
                1 => InputDisposition::UnknownEffect,
                _ => InputDisposition::Partial,
            }
        );
        assert_eq!(
            actual.submitted_operations,
            if progress == 2 { 2 } else { 0 }
        );
        assert!(!snapshot(&shared).ready);
        assert_eq!(
            report(&shared.0.lock().unwrap().submit(request).unwrap().unwrap()),
            actual
        );
        let next = Request {
            request_id: 2,
            ..request
        };
        assert_eq!(shared.0.lock().unwrap().submit(next), Err(Refusal::Stopped));
    }
}

#[test]
fn dropping_a_completed_waiter_does_not_revoke_the_original_owner() {
    let shared = fresh();
    let request = Request {
        operation: Operation::Scroll {
            x: 0,
            y: 0,
            dx: 0,
            dy: 1,
        },
        ..request(&snapshot(&shared), 1)
    };
    let mut waiting = Box::pin(shared.submit(request));
    assert!(poll(waiting.as_mut()).is_pending());
    encoded(&shared, 1, 0);
    shared.result(ResultEvent::Completed(receipt(0)));
    let Poll::Ready(reply) = poll(waiting.as_mut()) else {
        panic!("matching receipt must complete the original waiter");
    };
    assert_eq!(report(&reply).disposition, InputDisposition::Committed);
    drop(waiting);
    let fresh = snapshot(&shared);
    assert!(fresh.ready);
    let next = Request {
        request_id: 2,
        observation_id: fresh.observation_id.unwrap(),
        ..request
    };
    assert_eq!(shared.0.lock().unwrap().submit(next), Ok(None));
}

#[test]
fn late_receipts_refine_cancelled_effects_without_reopening_authority() {
    let shared = fresh();
    let request = Request {
        operation: Operation::Scroll {
            x: 0,
            y: 0,
            dx: 0,
            dy: 1,
        },
        ..request(&snapshot(&shared), 1)
    };
    let mut waiting = Box::pin(shared.submit(request));
    assert!(poll(waiting.as_mut()).is_pending());
    encoded(&shared, 1, 0);
    drop(waiting);
    assert_eq!(
        retained(&shared, request).disposition,
        InputDisposition::UnknownEffect
    );
    shared.result(ResultEvent::Completed(receipt(0)));
    let late = retained(&shared, request);
    assert_eq!(late.disposition, InputDisposition::Committed);
    assert_eq!(late.submitted_operations, 2);
    assert!(!snapshot(&shared).ready);
    assert_eq!(
        shared.0.lock().unwrap().submit(Request {
            request_id: 2,
            ..request
        }),
        Err(Refusal::Stopped)
    );
}

#[test]
fn fractional_age_rounds_up_and_expired_status_never_exposes_an_action_pin() {
    let initial = candidate(123);
    assert_eq!(initial.age(initial.at), Some(123));
    assert_eq!(initial.age(initial.at + Duration::from_nanos(1)), Some(124));
    assert_eq!(
        initial.age(initial.at + Duration::from_nanos(1001)),
        Some(125)
    );
    assert_eq!(
        initial.age(initial.at.checked_sub(Duration::from_nanos(1)).unwrap()),
        None
    );
    let shared = fresh();
    // This sample is below one second in microseconds but reaches the exclusive
    // one-second ceiling when conservatively represented in milliseconds.
    shared.0.lock().unwrap().live = Some(candidate(999_001));
    let expired = snapshot(&shared);
    assert!(!expired.ready);
    assert!(expired.lease.is_none() && expired.observation_id.is_none());
    assert!(expired.frame.is_none() && expired.source_age_upper_ms.is_none());
}

#[test]
fn snapshot_validation_refuses_incoherent_ready_claims_and_physical_visibility() {
    let shared = fresh();
    let actual = snapshot(&shared);
    for changed in [
        Snapshot {
            lease: Some("00000000000000000000000000000000".into()),
            ..actual.clone()
        },
        Snapshot {
            observation_id: None,
            ..actual.clone()
        },
        Snapshot {
            next_request_id: Some(0),
            ..actual.clone()
        },
        Snapshot {
            next_request_id: None,
            ..actual.clone()
        },
        Snapshot {
            recovery_generation: None,
            ..actual.clone()
        },
        Snapshot {
            frame: None,
            ..actual.clone()
        },
        Snapshot {
            source_age_upper_ms: Some(1000),
            ..actual.clone()
        },
        Snapshot {
            physical_visibility_proven: true,
            ..actual.clone()
        },
        Snapshot {
            ready: false,
            ..actual.clone()
        },
    ] {
        assert_eq!(validate_snapshot(&changed), Err(Error::Protocol));
    }
    let mut cleared = actual;
    clear_snapshot(&mut cleared);
    assert_eq!(validate_snapshot(&cleared), Ok(()));
    assert!(!cleared.ready);
    assert!(cleared.lease.is_none() && cleared.observation_id.is_none());
    assert!(cleared.geometry_generation.is_none() && cleared.viewport_generation.is_none());
}

#[test]
fn wrong_lease_cannot_consume_ids_or_reach_the_original_request() {
    let shared = fresh();
    let request = request(&snapshot(&shared), 1);
    let foreign = Request {
        lease: [3; 16],
        ..request
    };
    assert_eq!(
        shared.0.lock().unwrap().submit(foreign),
        Err(Refusal::LeaseMismatch)
    );
    assert_eq!(snapshot(&shared).next_request_id, Some(1));
    // The legitimate request needs the actual current observation, not a pin
    // silently refreshed by the rejected foreign lease.
    let latest = snapshot(&shared);
    let valid = Request {
        observation_id: latest.observation_id.unwrap(),
        ..request
    };
    assert_eq!(shared.0.lock().unwrap().submit(valid), Ok(None));
}

#[test]
fn initial_zero_generations_and_frame_zero_are_valid_observation_evidence() {
    let shared = fresh();
    shared
        .0
        .lock()
        .unwrap()
        .live
        .as_mut()
        .unwrap()
        .sample
        .stamp
        .frame = 0;
    let first = snapshot(&shared);
    assert!(first.ready);
    assert_eq!(first.geometry_generation, Some(0));
    assert_eq!(first.viewport_generation, Some(0));
    assert_eq!(first.configuration_generation, Some(0));
    assert_eq!(first.recovery_generation, Some(0));
    assert_eq!(first.frame, Some(0));
    assert!(!first.physical_visibility_proven);
    let request = request(&first, 1);
    assert_eq!(shared.0.lock().unwrap().submit(request), Ok(None));
    assert!(ready(&shared, 1));
}

#[test]
fn coherent_single_action_receipt_cannot_complete_a_click_or_different_request() {
    let shared = fresh();
    let click = request(&snapshot(&shared), 1);
    let scroll = Request {
        operation: Operation::Scroll {
            x: 0,
            y: 0,
            dx: 0,
            dy: 1,
        },
        ..click
    };
    assert_eq!(shared.0.lock().unwrap().submit(scroll), Ok(None));
    encoded(&shared, 1, 0);
    shared.result(ResultEvent::Completed(receipt(0)));
    let result = retained(&shared, scroll);
    assert_eq!(result.validate(), Ok(()));
    assert_eq!(validate_report(result, scroll), Ok(()));
    assert_eq!(validate_report(result, click), Err(Error::Protocol));
    assert_eq!(
        validate_report(
            result,
            Request {
                request_id: 2,
                ..scroll
            }
        ),
        Err(Error::Protocol)
    );
}

#[test]
fn a_complete_chord_keeps_the_same_owner_and_retains_all_key_receipts() {
    let shared = fresh();
    let request = Request {
        operation: Operation::KeyChord {
            modifiers: 0b0000_1111,
            usage: 19,
        },
        ..request(&snapshot(&shared), 1)
    };
    assert_eq!(shared.0.lock().unwrap().submit(request), Ok(None));
    for sequence in 0..u64::from(request.operation.count()) {
        encoded(&shared, 1, sequence);
        assert!(!ready(&shared, 1));
        shared.result(ResultEvent::Completed(InputResult {
            submitted_operations: 1,
            ..receipt(sequence)
        }));
    }
    let complete = retained(&shared, request);
    assert_eq!(complete.disposition, InputDisposition::Committed);
    assert_eq!(complete.actions_total, 10);
    assert_eq!(complete.actions_submitted, 10);
    assert_eq!(complete.submitted_operations, 10);
    let state = shared.0.lock().unwrap();
    assert!(state.pending.is_none());
    assert!(!state.stopped && !state.ledger.is_closed());
    drop(state);
    assert_eq!(
        report(&shared.0.lock().unwrap().submit(request).unwrap().unwrap()),
        complete
    );
    assert!(snapshot(&shared).ready);
}

#[test]
fn partial_or_unknown_chords_retire_the_original_owner_without_resuming_releases() {
    for prefix in [1_u8, 4, 5, 6, 9] {
        for missing_receipt in [false, true] {
            let shared = fresh();
            let request = Request {
                operation: Operation::KeyChord {
                    modifiers: 0b0000_1111,
                    usage: 19,
                },
                ..request(&snapshot(&shared), 1)
            };
            let mut waiting = Box::pin(shared.submit(request));
            assert!(poll(waiting.as_mut()).is_pending());
            for sequence in 0..u64::from(prefix) {
                encoded(&shared, 1, sequence);
                shared.result(ResultEvent::Completed(InputResult {
                    submitted_operations: 1,
                    ..receipt(sequence)
                }));
            }
            encoded(&shared, 1, u64::from(prefix));
            if !missing_receipt {
                shared.result(ResultEvent::Completed(InputResult {
                    stage: Stage::Admitted,
                    outcome: InputOutcome::CancelledBeforeSubmission,
                    submitted_operations: 0,
                    reason: Some(fr_wire::input_result::Reason::Revoked),
                    ..receipt(u64::from(prefix))
                }));
            }
            // Losing the local requester is a terminal fence, including when
            // the previous modifier/key transition has no host receipt.
            drop(waiting);
            let actual = retained(&shared, request);
            assert_eq!(
                actual.disposition,
                if missing_receipt {
                    InputDisposition::UnknownEffect
                } else {
                    InputDisposition::Partial
                }
            );
            assert_eq!(actual.actions_encoded, prefix + 1);
            assert_eq!(actual.actions_submitted, prefix);
            assert_eq!(actual.submitted_operations, u32::from(prefix));
            assert_eq!(actual.unacknowledged_actions, u8::from(missing_receipt));
            let state = shared.0.lock().unwrap();
            assert!(state.stopped && state.ledger.is_closed());
            assert!(state.pending.is_none() && state.live.is_none() && state.pinned.is_none());
            drop(state);
            assert!(!snapshot(&shared).ready);
            assert_eq!(
                report(&shared.0.lock().unwrap().submit(request).unwrap().unwrap()),
                actual
            );
            assert_eq!(
                shared.0.lock().unwrap().submit(Request {
                    request_id: 2,
                    ..request
                }),
                Err(Refusal::Stopped)
            );
            if missing_receipt {
                shared.result(ResultEvent::Completed(InputResult {
                    submitted_operations: 1,
                    ..receipt(u64::from(prefix))
                }));
                let late = retained(&shared, request);
                assert_eq!(late.unacknowledged_actions, 0);
                assert_eq!(late.actions_submitted, prefix + 1);
                assert!(!snapshot(&shared).ready);
                assert_eq!(
                    shared.0.lock().unwrap().submit(Request {
                        request_id: 2,
                        ..request
                    }),
                    Err(Refusal::Stopped)
                );
            }
        }
    }
}
