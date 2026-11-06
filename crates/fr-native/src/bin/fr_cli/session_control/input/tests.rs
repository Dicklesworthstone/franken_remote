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
        capabilities: fr_core::input_submission::Capabilities::default()
            .with(fr_core::input_submission::Capability::Keys)
            .with(fr_core::input_submission::Capability::Absolute)
            .with(fr_core::input_submission::Capability::Buttons)
            .with(fr_core::input_submission::Capability::LineScroll),
        bounds: fr_core::input::InputBounds::new(
            fr_core::input::DesktopPoint { x: -800, y: -200 },
            1600,
            800,
        )
        .unwrap(),
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
    assert!(expired.input_support.is_none());
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
    assert!(cleared.input_support.is_none());
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
fn a_large_scroll_waits_for_each_receipt_and_preserves_the_original_result() {
    let shared = fresh();
    let request = Request {
        operation: Operation::Scroll {
            x: -100,
            y: 20,
            dx: -120,
            dy: 120,
        },
        ..request(&snapshot(&shared), 1)
    };
    assert_eq!(request.operation.count(), 8);
    assert_eq!(shared.0.lock().unwrap().submit(request), Ok(None));
    for sequence in 0..8 {
        encoded(&shared, 1, sequence);
        assert!(!ready(&shared, 1), "never pipeline another wheel action");
        // A repeated command is the same pending request, not eight more actions.
        assert_eq!(shared.0.lock().unwrap().submit(request), Ok(None));
        shared.result(ResultEvent::Completed(InputResult {
            submitted_operations: if sequence == 7 { 33 } else { 65 },
            ..receipt(sequence)
        }));
    }
    let complete = retained(&shared, request);
    assert_eq!(complete.disposition, InputDisposition::Committed);
    assert_eq!(complete.actions_submitted, 8);
    assert_eq!(complete.submitted_operations, 488); // eight positions, 240 wheel pairs
    assert_eq!(complete.unacknowledged_actions, 0);
    assert_eq!(validate_report(complete, request), Ok(()));
    assert_eq!(
        report(&shared.0.lock().unwrap().submit(request).unwrap().unwrap()),
        complete
    );
    assert!(snapshot(&shared).ready);
}

#[test]
fn interrupted_large_scrolls_keep_the_prefix_and_never_replay_the_remainder() {
    for missing_receipt in [false, true] {
        let shared = fresh();
        let request = Request {
            operation: Operation::Scroll {
                x: -100,
                y: 20,
                dx: 120,
                dy: 120,
            },
            ..request(&snapshot(&shared), 1)
        };
        let mut waiting = Box::pin(shared.submit(request));
        assert!(poll(waiting.as_mut()).is_pending());
        for sequence in 0..3 {
            encoded(&shared, 1, sequence);
            shared.result(ResultEvent::Completed(InputResult {
                submitted_operations: 65,
                ..receipt(sequence)
            }));
        }
        encoded(&shared, 1, 3);
        if !missing_receipt {
            shared.result(ResultEvent::Completed(InputResult {
                stage: Stage::Admitted,
                outcome: InputOutcome::ExpiredBeforeSubmission,
                submitted_operations: 0,
                reason: Some(fr_wire::input_result::Reason::TicketExpired),
                ..receipt(3)
            }));
        }
        drop(waiting);
        let result = retained(&shared, request);
        assert_eq!(result.actions_total, 8);
        assert_eq!(result.actions_encoded, 4);
        assert_eq!(result.actions_submitted, 3);
        assert_eq!(result.submitted_operations, 195);
        assert_eq!(result.unacknowledged_actions, u8::from(missing_receipt));
        assert_eq!(
            result.disposition,
            if missing_receipt {
                InputDisposition::UnknownEffect
            } else {
                InputDisposition::Partial
            }
        );
        assert!(!snapshot(&shared).ready);
        assert_eq!(
            report(&shared.0.lock().unwrap().submit(request).unwrap().unwrap()),
            result
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
                submitted_operations: 65,
                ..receipt(3)
            }));
            let late = retained(&shared, request);
            assert_eq!(late.actions_submitted, 4);
            assert_eq!(late.submitted_operations, 260);
            assert_eq!(late.unacknowledged_actions, 0);
            assert_eq!(late.disposition, InputDisposition::Partial);
            assert!(
                !snapshot(&shared).ready,
                "a late receipt never resumes the scroll"
            );
        }
    }
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

fn drag_request(shared: &Shared) -> Request {
    Request {
        operation: Operation::Drag {
            button: 1,
            from_x: -800,
            from_y: -200,
            to_x: 799,
            to_y: 599,
        },
        ..request(&snapshot(shared), 1)
    }
}

#[test]
fn either_out_of_bounds_drag_endpoint_refuses_before_the_press_is_encoded() {
    // The original granted display is [-800,800) x [-200,600). Its signed
    // corners are valid; each exclusive edge is checked before any press.
    for (from_x, from_y, to_x, to_y) in [
        (-801, -200, 799, 599),
        (-800, -201, 799, 599),
        (800, -200, 799, 599),
        (-800, 600, 799, 599),
        (-800, -200, -801, 599),
        (-800, -200, 799, -201),
        (-800, -200, 800, 599),
        (-800, -200, 799, 600),
        (i32::MIN, -200, i32::MAX, 599),
    ] {
        let shared = fresh();
        let request = Request {
            operation: Operation::Drag {
                button: 1,
                from_x,
                from_y,
                to_x,
                to_y,
            },
            ..drag_request(&shared)
        };
        assert_eq!(
            shared.0.lock().unwrap().submit(request),
            Err(Refusal::OutOfBounds)
        );
        let refused = retained(&shared, request);
        assert_eq!(refused.actions_total, 2);
        assert_eq!(refused.actions_encoded, 0);
        assert_eq!(refused.submitted_operations, 0);
        assert_eq!(refused.disposition, InputDisposition::Refused);
        assert!(shared.0.lock().unwrap().pending.is_none());
        assert_eq!(snapshot(&shared).next_request_id, Some(2));
        assert_eq!(
            report(&shared.0.lock().unwrap().submit(request).unwrap().unwrap()),
            refused
        );
        // Fixing coordinates cannot turn the consumed request into another drag.
        let changed = Request {
            operation: Operation::Drag {
                button: 1,
                from_x: -799,
                from_y: -199,
                to_x: 798,
                to_y: 598,
            },
            ..request
        };
        assert_eq!(
            shared.0.lock().unwrap().submit(changed),
            Err(Refusal::RequestChanged)
        );
    }
}

#[test]
fn changed_granted_bounds_cannot_reinterpret_an_original_drag() {
    let shared = fresh();
    let request = drag_request(&shared);
    shared.0.lock().unwrap().live.as_mut().unwrap().bounds = fr_core::input::InputBounds::new(
        fr_core::input::DesktopPoint { x: -800, y: -200 },
        1599,
        800,
    )
    .unwrap();
    assert_eq!(
        shared.0.lock().unwrap().submit(request),
        Err(Refusal::ViewChanged)
    );
    assert_eq!(retained(&shared, request).actions_encoded, 0);
    assert!(shared.0.lock().unwrap().pending.is_none());
}

#[test]
fn a_drag_waits_for_both_original_receipts_and_fingerprints_each_endpoint() {
    let shared = fresh();
    let request = drag_request(&shared);
    assert_eq!(shared.0.lock().unwrap().submit(request), Ok(None));
    encoded(&shared, 1, 0);
    assert!(!ready(&shared, 1));
    for operation in [
        Operation::Drag {
            button: 2,
            from_x: -800,
            from_y: -200,
            to_x: 799,
            to_y: 599,
        },
        Operation::Drag {
            button: 1,
            from_x: -799,
            from_y: -200,
            to_x: 799,
            to_y: 599,
        },
        Operation::Drag {
            button: 1,
            from_x: -800,
            from_y: -199,
            to_x: 799,
            to_y: 599,
        },
        Operation::Drag {
            button: 1,
            from_x: -800,
            from_y: -200,
            to_x: 798,
            to_y: 599,
        },
        Operation::Drag {
            button: 1,
            from_x: -800,
            from_y: -200,
            to_x: 799,
            to_y: 598,
        },
    ] {
        assert_eq!(
            shared.0.lock().unwrap().submit(Request {
                operation,
                ..request
            }),
            Err(Refusal::RequestChanged)
        );
    }
    shared
        .0
        .lock()
        .unwrap()
        .live
        .as_mut()
        .unwrap()
        .sample
        .stamp
        .frame = 8;
    let newer = snapshot(&shared);
    assert_ne!(newer.observation_id, Some(request.observation_id));
    {
        let state = shared.0.lock().unwrap();
        let pending = state.pending.as_ref().unwrap();
        assert_eq!(pending.request.observation_id, request.observation_id);
        assert_eq!(pending.original.sample.stamp.frame, 7);
        assert_eq!(pending.encoded, 1);
    }
    shared.result(ResultEvent::Completed(receipt(0)));
    assert!(ready(&shared, 1));
    encoded(&shared, 1, 1);
    shared.result(ResultEvent::Duplicate(receipt(0)));
    assert!(!ready(&shared, 1));
    shared.result(ResultEvent::Completed(receipt(1)));
    let complete = retained(&shared, request);
    assert_eq!(complete.disposition, InputDisposition::Committed);
    assert_eq!(complete.actions_total, 2);
    assert_eq!(complete.actions_encoded, 2);
    assert_eq!(complete.actions_submitted, 2);
    assert_eq!(complete.submitted_operations, 4);
    assert_eq!(complete.unacknowledged_actions, 0);
    assert_eq!(
        report(&shared.0.lock().unwrap().submit(request).unwrap().unwrap()),
        complete
    );
    assert!(snapshot(&shared).ready);
    assert!(shared.0.lock().unwrap().pending.is_none());
}

#[test]
fn cancelled_or_expired_drags_preserve_effects_and_never_resume_the_release() {
    // Stop before the press receipt, after it, after encoding the release, or
    // on the original request deadline. No newer observation authorizes replay.
    for progress in 0..4 {
        let shared = fresh();
        let request = drag_request(&shared);
        let mut waiting = Box::pin(shared.submit(request));
        assert!(poll(waiting.as_mut()).is_pending());
        encoded(&shared, 1, 0);
        if progress != 0 {
            shared.result(ResultEvent::Completed(receipt(0)));
        }
        if progress == 2 {
            encoded(&shared, 1, 1);
        }
        if progress == 3 {
            let mut state = shared.0.lock().unwrap();
            let now = state.now(Instant::now()).unwrap();
            state.ledger.expire(now + 2_000_000).unwrap();
            state.finish_if_retained();
        }
        drop(waiting);
        let actual = retained(&shared, request);
        assert_eq!(actual.actions_encoded, if progress == 2 { 2 } else { 1 });
        assert_eq!(actual.actions_submitted, u8::from(progress != 0));
        assert_eq!(
            actual.submitted_operations,
            if progress == 0 { 0 } else { 2 }
        );
        assert_eq!(
            actual.unacknowledged_actions,
            u8::from(progress == 0 || progress == 2)
        );
        assert_eq!(
            actual.disposition,
            if progress == 0 || progress == 2 {
                InputDisposition::UnknownEffect
            } else {
                InputDisposition::Partial
            }
        );
        if progress == 3 {
            assert_eq!(actual.end, native::End::Expired);
        }
        {
            let state = shared.0.lock().unwrap();
            assert!(state.stopped && state.ledger.is_closed());
            assert!(state.pending.is_none() && state.live.is_none() && state.pinned.is_none());
        }
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
        if progress == 0 || progress == 2 {
            let sequence = u64::from(progress == 2);
            shared.result(ResultEvent::Completed(receipt(sequence)));
            assert_eq!(retained(&shared, request).unacknowledged_actions, 0);
            assert!(!snapshot(&shared).ready);
            assert!(shared.0.lock().unwrap().pending.is_none());
        }
    }
}

fn move_request(shared: &Shared, x: i32, y: i32) -> Request {
    Request {
        operation: Operation::Move { x, y },
        ..request(&snapshot(shared), 1)
    }
}

fn encoded_pointer(shared: &Shared, id: u64, sequence: u64) {
    assert!(ready(shared, id));
    let mut state = shared.0.lock().unwrap();
    state
        .ledger
        .encoded_in(id, binding(), SequenceSpace::Pointer, sequence)
        .unwrap();
    state.pending.as_mut().unwrap().encoded += 1;
}

fn pointer_receipt(sequence: u64) -> InputResult {
    InputResult {
        space: SequenceSpace::Pointer,
        submitted_operations: 1,
        ..receipt(sequence)
    }
}

#[test]
fn move_outside_the_original_display_consumes_the_request_before_encoding() {
    for (x, y) in [
        (-801, 0),
        (800, 0),
        (0, -201),
        (0, 600),
        (i32::MIN, i32::MAX),
    ] {
        let shared = fresh();
        let request = move_request(&shared, x, y);
        assert_eq!(
            shared.0.lock().unwrap().submit(request),
            Err(Refusal::OutOfBounds)
        );
        let refused = retained(&shared, request);
        assert_eq!(refused.actions_total, 1);
        assert_eq!(refused.actions_encoded, 0);
        assert_eq!(refused.disposition, InputDisposition::Refused);
        assert!(shared.0.lock().unwrap().pending.is_none());
        assert_eq!(snapshot(&shared).next_request_id, Some(2));
        assert_eq!(
            report(&shared.0.lock().unwrap().submit(request).unwrap().unwrap()),
            refused
        );
        assert_eq!(
            shared.0.lock().unwrap().submit(Request {
                operation: Operation::Move { x: 0, y: 0 },
                ..request
            }),
            Err(Refusal::RequestChanged)
        );
    }
}

#[test]
fn move_pins_its_coordinates_observation_and_original_pointer_receipt() {
    for (x, y) in [(-800, -200), (799, 599)] {
        let shared = fresh();
        let request = move_request(&shared, x, y);
        assert_eq!(shared.0.lock().unwrap().submit(request), Ok(None));
        encoded_pointer(&shared, 1, 0);
        for operation in [
            Operation::Move { x: x + 1, y },
            Operation::Move { x, y: y + 1 },
            Operation::Click { button: 1, x, y },
        ] {
            assert_eq!(
                shared.0.lock().unwrap().submit(Request {
                    operation,
                    ..request
                }),
                Err(Refusal::RequestChanged)
            );
        }
        shared
            .0
            .lock()
            .unwrap()
            .live
            .as_mut()
            .unwrap()
            .sample
            .stamp
            .frame = 8;
        let newer = snapshot(&shared);
        assert_ne!(newer.observation_id, Some(request.observation_id));
        {
            let state = shared.0.lock().unwrap();
            let pending = state.pending.as_ref().unwrap();
            assert_eq!(pending.original.sample.stamp.frame, 7);
            assert_eq!(pending.request.observation_id, request.observation_id);
            assert_eq!(pending.encoded, 1);
        }
        shared.result(ResultEvent::Completed(receipt(0)));
        assert!(!ready(&shared, 1));
        shared.result(ResultEvent::Pointer(pointer_receipt(0)));
        let complete = retained(&shared, request);
        assert_eq!(complete.disposition, InputDisposition::Committed);
        assert_eq!(complete.actions_total, 1);
        assert_eq!(complete.actions_submitted, 1);
        assert_eq!(complete.submitted_operations, 1);
        assert_eq!(
            report(&shared.0.lock().unwrap().submit(request).unwrap().unwrap()),
            complete
        );
        assert!(shared.0.lock().unwrap().pending.is_none());
    }
}

#[test]
fn move_keeps_original_observation_focus_and_display_preconditions() {
    for refusal in [
        Refusal::ObservationExpired,
        Refusal::InputUnavailable,
        Refusal::ViewChanged,
    ] {
        let shared = fresh();
        let request = move_request(&shared, 0, 0);
        {
            let mut state = shared.0.lock().unwrap();
            match refusal {
                Refusal::ObservationExpired => {
                    state.pinned.as_mut().unwrap().1.at =
                        Instant::now().checked_sub(Duration::from_secs(2)).unwrap();
                    state.live = Some(candidate(0));
                }
                Refusal::InputUnavailable => state.live = None,
                Refusal::ViewChanged => {
                    state.live.as_mut().unwrap().bounds = fr_core::input::InputBounds::new(
                        fr_core::input::DesktopPoint { x: -800, y: -200 },
                        1599,
                        800,
                    )
                    .unwrap();
                }
                _ => unreachable!(),
            }
            assert_eq!(state.submit(request), Err(refusal));
        }
        assert_eq!(retained(&shared, request).actions_encoded, 0);
    }
}

#[test]
fn cancelled_move_keeps_unknown_effect_and_a_late_receipt_cannot_restart_owner() {
    let shared = fresh();
    let request = move_request(&shared, 0, 0);
    assert_eq!(shared.0.lock().unwrap().submit(request), Ok(None));
    encoded_pointer(&shared, 1, 0);
    shared.0.lock().unwrap().retire();
    let cancelled = retained(&shared, request);
    assert_eq!(cancelled.disposition, InputDisposition::UnknownEffect);
    assert_eq!(cancelled.unacknowledged_actions, 1);
    assert!(!snapshot(&shared).ready);
    shared.result(ResultEvent::Pointer(pointer_receipt(0)));
    let late = retained(&shared, request);
    assert_eq!(late.end, cancelled.end);
    assert_eq!(late.disposition, InputDisposition::Committed);
    assert_eq!(late.submitted_operations, 1);
    assert!(!snapshot(&shared).ready);
    assert_eq!(
        shared.0.lock().unwrap().submit(Request {
            request_id: 2,
            ..request
        }),
        Err(Refusal::Stopped)
    );
}

fn modified_operation(drag: bool, modifiers: u8) -> Operation {
    if drag {
        Operation::ModifiedDrag {
            modifiers,
            button: 1,
            from_x: -800,
            from_y: -200,
            to_x: 799,
            to_y: 599,
        }
    } else {
        Operation::ModifiedClick {
            modifiers,
            button: 1,
            x: -800,
            y: -200,
        }
    }
}

fn modified_request(shared: &Shared, drag: bool) -> Request {
    Request {
        operation: modified_operation(drag, 0b0000_1111),
        ..request(&snapshot(shared), 1)
    }
}

#[test]
fn modified_gestures_preflight_every_coordinate_before_any_modifier() {
    for (x, y) in [
        (-801, 0),
        (800, 0),
        (0, -201),
        (0, 600),
        (i32::MIN, i32::MAX),
    ] {
        for operation in [
            Operation::ModifiedClick {
                modifiers: 15,
                button: 1,
                x,
                y,
            },
            Operation::ModifiedDrag {
                modifiers: 15,
                button: 1,
                from_x: x,
                from_y: y,
                to_x: 799,
                to_y: 599,
            },
            Operation::ModifiedDrag {
                modifiers: 15,
                button: 1,
                from_x: -800,
                from_y: -200,
                to_x: x,
                to_y: y,
            },
        ] {
            let shared = fresh();
            let request = Request {
                operation,
                ..request(&snapshot(&shared), 1)
            };
            assert_eq!(shared.locked().submit(request), Err(Refusal::OutOfBounds));
            let refused = retained(&shared, request);
            assert_eq!(refused.actions_total, 10);
            assert_eq!(refused.actions_encoded, 0);
            assert_eq!(refused.submitted_operations, 0);
            assert_eq!(refused.disposition, InputDisposition::Refused);
            assert!(shared.locked().pending.is_none());
            assert!(snapshot(&shared).ready);
            assert_eq!(
                report(&shared.locked().submit(request).unwrap().unwrap()),
                refused
            );
        }
    }
}

#[test]
fn modified_gestures_require_the_entire_original_capability_grant() {
    use fr_core::input_submission::{Capabilities, Capability};
    for drag in [false, true] {
        for mask in 0_u8..7 {
            let shared = fresh();
            let mut capabilities = Capabilities::default().with(Capability::LineScroll);
            for (bit, capability) in [
                (1, Capability::Keys),
                (2, Capability::Buttons),
                (4, Capability::Absolute),
            ] {
                if mask & bit != 0 {
                    capabilities = capabilities.with(capability);
                }
            }
            shared.locked().live.as_mut().unwrap().capabilities = capabilities;
            let request = modified_request(&shared, drag);
            assert_eq!(
                shared.locked().submit(request),
                Err(Refusal::InputUnavailable)
            );
            let refused = retained(&shared, request);
            assert_eq!(refused.actions_encoded, 0);
            assert_eq!(refused.submitted_operations, 0);
            assert_eq!(refused.disposition, InputDisposition::Refused);
            assert!(shared.locked().pending.is_none());
            assert!(!shared.locked().ledger.is_closed());
        }
        let shared = fresh();
        let request = modified_request(&shared, drag);
        shared.locked().live.as_mut().unwrap().capabilities = Capabilities::default();
        assert_eq!(shared.locked().submit(request), Err(Refusal::ViewChanged));
        assert_eq!(retained(&shared, request).actions_encoded, 0);
    }
}

fn modified_receipt(sequence: u64) -> InputResult {
    InputResult {
        // Four modifiers, two reliable position+button barriers, four releases.
        submitted_operations: if (4..=5).contains(&sequence) { 2 } else { 1 },
        ..receipt(sequence)
    }
}

#[test]
fn modified_gestures_keep_ten_action_receipts_and_twelve_native_operations() {
    for drag in [false, true] {
        let shared = fresh();
        let request = modified_request(&shared, drag);
        assert_eq!(shared.locked().submit(request), Ok(None));
        for sequence in 0..10 {
            encoded(&shared, 1, sequence);
            assert!(
                !ready(&shared, 1),
                "each modifier/button needs its own receipt"
            );
            assert_eq!(shared.locked().submit(request), Ok(None));
            assert_eq!(
                shared.locked().submit(Request {
                    operation: modified_operation(drag, 0b0000_0111),
                    ..request
                }),
                Err(Refusal::RequestChanged)
            );
            shared.result(ResultEvent::Completed(modified_receipt(sequence)));
        }
        let complete = retained(&shared, request);
        assert_eq!(complete.actions_total, 10);
        assert_eq!(complete.actions_encoded, 10);
        assert_eq!(complete.actions_submitted, 10);
        assert_eq!(complete.acknowledged_actions, 10);
        assert_eq!(complete.submitted_operations, 12);
        assert_eq!(complete.unacknowledged_actions, 0);
        assert_eq!(complete.disposition, InputDisposition::Committed);
        assert_eq!(validate_report(complete, request), Ok(()));
        assert_eq!(
            report(&shared.locked().submit(request).unwrap().unwrap()),
            complete
        );
        assert!(shared.locked().pending.is_none());
        assert!(snapshot(&shared).ready);
    }
}

#[test]
fn interrupted_modified_gestures_keep_the_exact_prefix_and_retire_the_owner() {
    const OPERATIONS: [u32; 11] = [0, 1, 2, 3, 4, 6, 8, 9, 10, 11, 12];
    for drag in [false, true] {
        for prefix in 1_u8..10 {
            for missing_receipt in [false, true] {
                let shared = fresh();
                let request = modified_request(&shared, drag);
                let mut waiting = Box::pin(shared.submit(request));
                assert!(poll(waiting.as_mut()).is_pending());
                for sequence in 0..u64::from(prefix) {
                    encoded(&shared, 1, sequence);
                    shared.result(ResultEvent::Completed(modified_receipt(sequence)));
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
                drop(waiting);
                let actual = retained(&shared, request);
                assert_eq!(actual.actions_total, 10);
                assert_eq!(actual.actions_encoded, prefix + 1);
                assert_eq!(actual.actions_submitted, prefix);
                assert_eq!(actual.submitted_operations, OPERATIONS[usize::from(prefix)]);
                assert_eq!(actual.unacknowledged_actions, u8::from(missing_receipt));
                assert_eq!(
                    actual.disposition,
                    if missing_receipt {
                        InputDisposition::UnknownEffect
                    } else {
                        InputDisposition::Partial
                    }
                );
                assert!(!snapshot(&shared).ready);
                assert!(shared.locked().pending.is_none());
                assert_eq!(
                    report(&shared.locked().submit(request).unwrap().unwrap()),
                    actual
                );
                assert_eq!(
                    shared.locked().submit(Request {
                        request_id: 2,
                        ..request
                    }),
                    Err(Refusal::Stopped)
                );
                if missing_receipt {
                    shared.result(ResultEvent::Completed(modified_receipt(u64::from(prefix))));
                    let late = retained(&shared, request);
                    assert_eq!(late.actions_submitted, prefix + 1);
                    assert_eq!(
                        late.submitted_operations,
                        OPERATIONS[usize::from(prefix + 1)]
                    );
                    assert_eq!(late.unacknowledged_actions, 0);
                    assert!(
                        !snapshot(&shared).ready,
                        "late evidence cannot resume input"
                    );
                    assert!(shared.locked().pending.is_none());
                }
            }
        }
    }
}

#[test]
fn modified_gesture_expiry_fences_held_modifiers_and_buttons_without_replaying() {
    for drag in [false, true] {
        for prefix in [1_u8, 5] {
            let shared = fresh();
            let request = modified_request(&shared, drag);
            let mut waiting = Box::pin(shared.submit(request));
            assert!(poll(waiting.as_mut()).is_pending());
            for sequence in 0..u64::from(prefix) {
                encoded(&shared, 1, sequence);
                shared.result(ResultEvent::Completed(modified_receipt(sequence)));
            }
            {
                let mut state = shared.locked();
                let now = state.now(Instant::now()).unwrap();
                state.ledger.expire(now + 2_000_000).unwrap();
                state.finish_if_retained();
                assert!(state.stopped && state.ledger.is_closed());
                assert!(state.pending.is_none() && state.live.is_none() && state.pinned.is_none());
            }
            drop(waiting);
            let expired = retained(&shared, request);
            assert_eq!(expired.end, native::End::Expired);
            assert_eq!(expired.disposition, InputDisposition::Partial);
            assert_eq!(expired.actions_encoded, prefix);
            assert_eq!(expired.actions_submitted, prefix);
            assert_eq!(
                expired.submitted_operations,
                if prefix == 1 { 1 } else { 6 }
            );
            assert_eq!(expired.unacknowledged_actions, 0);
            assert_eq!(
                report(&shared.locked().submit(request).unwrap().unwrap()),
                expired
            );
        }
    }
}

fn modified_scroll_request(shared: &Shared) -> Request {
    Request {
        operation: Operation::ModifiedScroll {
            modifiers: 1,
            x: -100,
            y: 20,
            dx: -120,
            dy: 120,
        },
        ..request(&snapshot(shared), 1)
    }
}

fn modified_scroll_receipt(sequence: u64) -> InputResult {
    InputResult {
        // Discrete-wheel fixture: one modifier, eight bounded wheel actions,
        // then release. Each wheel action includes its own position barrier.
        submitted_operations: match sequence {
            0 | 9 => 1,
            1..=7 => 65,
            8 => 33,
            _ => panic!("the bounded scroll has exactly ten actions"),
        },
        ..receipt(sequence)
    }
}

#[test]
fn modified_scroll_requires_keys_position_and_wheel_before_any_modifier() {
    use fr_core::input_submission::{Capabilities, Capability};

    for mask in [3_u8, 5, 6, 7] {
        let shared = fresh();
        let mut capabilities = Capabilities::default();
        for (bit, capability) in [
            (1, Capability::Keys),
            (2, Capability::Absolute),
            (4, Capability::LineScroll),
        ] {
            if mask & bit != 0 {
                capabilities = capabilities.with(capability);
            }
        }
        shared.locked().live.as_mut().unwrap().capabilities = capabilities;
        let request = modified_scroll_request(&shared);
        if mask == 7 {
            // No button capability is needed by a complete modified scroll.
            assert_eq!(shared.locked().submit(request), Ok(None));
            assert_eq!(shared.locked().pending.as_ref().unwrap().encoded, 0);
        } else {
            assert_eq!(shared.locked().submit(request), Err(Refusal::InputUnavailable));
            let refused = retained(&shared, request);
            assert_eq!(refused.actions_encoded, 0);
            assert_eq!(refused.submitted_operations, 0);
            assert_eq!(refused.disposition, InputDisposition::Refused);
            assert!(shared.locked().pending.is_none());
            assert!(!shared.locked().ledger.is_closed());
        }
    }
    let shared = fresh();
    let request = modified_scroll_request(&shared);
    shared.locked().live.as_mut().unwrap().capabilities = Capabilities::default();
    assert_eq!(shared.locked().submit(request), Err(Refusal::ViewChanged));
    assert_eq!(retained(&shared, request).actions_encoded, 0);
}

#[test]
fn modified_scroll_refuses_outside_the_original_display_before_pressing_keys() {
    for (x, y) in [(-801, 0), (800, 0), (0, -201), (0, 600), (i32::MIN, i32::MAX)] {
        let shared = fresh();
        let request = Request {
            operation: Operation::ModifiedScroll {
                modifiers: 1,
                x,
                y,
                dx: -120,
                dy: 120,
            },
            ..modified_scroll_request(&shared)
        };
        assert_eq!(shared.locked().submit(request), Err(Refusal::OutOfBounds));
        let refused = retained(&shared, request);
        assert_eq!(refused.actions_total, 10);
        assert_eq!(refused.actions_encoded, 0);
        assert_eq!(refused.submitted_operations, 0);
        assert_eq!(refused.disposition, InputDisposition::Refused);
        assert!(shared.locked().pending.is_none());
        assert!(snapshot(&shared).ready);
        assert_eq!(
            report(&shared.locked().submit(request).unwrap().unwrap()),
            refused
        );
    }
}

#[test]
fn over_budget_modified_scroll_never_enters_the_original_receipt_ledger() {
    let shared = fresh();
    let original = modified_scroll_request(&shared);
    let invalid = Request {
        operation: Operation::ModifiedScroll {
            modifiers: 15,
            x: -100,
            y: 20,
            dx: -120,
            dy: 120,
        },
        ..original
    };
    assert_eq!(invalid.operation.count(), 16);
    assert_eq!(shared.locked().submit(invalid), Err(Refusal::InvalidRequest));
    assert!(shared.locked().pending.is_none());
    assert_eq!(shared.locked().ledger.consumed_through(), 0);
    assert_eq!(shared.locked().submit(original), Ok(None));
}

#[test]
fn modified_scroll_retains_ten_receipts_and_all_490_native_operations() {
    let shared = fresh();
    let request = modified_scroll_request(&shared);
    assert_eq!(shared.locked().submit(request), Ok(None));
    for (modifiers, dx, dy) in [(2, -120, 120), (1, 120, 120), (1, -120, 119)] {
        assert_eq!(
            shared.locked().submit(Request {
                operation: Operation::ModifiedScroll {
                    modifiers,
                    x: -100,
                    y: 20,
                    dx,
                    dy,
                },
                ..request
            }),
            Err(Refusal::RequestChanged)
        );
    }
    for sequence in 0..10 {
        encoded(&shared, 1, sequence);
        assert!(!ready(&shared, 1), "each key or wheel action needs its own receipt");
        assert_eq!(shared.locked().submit(request), Ok(None));
        shared.result(ResultEvent::Completed(modified_scroll_receipt(sequence)));
    }
    let complete = retained(&shared, request);
    assert_eq!(complete.actions_total, 10);
    assert_eq!(complete.actions_encoded, 10);
    assert_eq!(complete.actions_submitted, 10);
    assert_eq!(complete.acknowledged_actions, 10);
    assert_eq!(complete.submitted_operations, 490);
    assert_eq!(complete.unacknowledged_actions, 0);
    assert_eq!(complete.disposition, InputDisposition::Committed);
    assert_eq!(validate_report(complete, request), Ok(()));
    assert_eq!(
        report(&shared.locked().submit(request).unwrap().unwrap()),
        complete
    );
    assert!(shared.locked().pending.is_none());
    assert!(snapshot(&shared).ready);
}

#[test]
fn interrupted_modified_scroll_preserves_key_and_wheel_prefixes_without_resuming() {
    const OPERATIONS: [u32; 11] = [0, 1, 66, 131, 196, 261, 326, 391, 456, 489, 490];

    // After the modifier, amid the wheel batches, and before its release.
    for prefix in [1_u8, 4, 9] {
        for missing_receipt in [false, true] {
            let shared = fresh();
            let request = modified_scroll_request(&shared);
            let mut waiting = Box::pin(shared.submit(request));
            assert!(poll(waiting.as_mut()).is_pending());
            for sequence in 0..u64::from(prefix) {
                encoded(&shared, 1, sequence);
                shared.result(ResultEvent::Completed(modified_scroll_receipt(sequence)));
            }
            encoded(&shared, 1, u64::from(prefix));
            if !missing_receipt {
                shared.result(ResultEvent::Completed(InputResult {
                    stage: Stage::Admitted,
                    outcome: InputOutcome::ExpiredBeforeSubmission,
                    submitted_operations: 0,
                    reason: Some(fr_wire::input_result::Reason::TicketExpired),
                    ..receipt(u64::from(prefix))
                }));
            }
            drop(waiting);
            let interrupted = retained(&shared, request);
            assert_eq!(interrupted.actions_total, 10);
            assert_eq!(interrupted.actions_encoded, prefix + 1);
            assert_eq!(interrupted.actions_submitted, prefix);
            assert_eq!(interrupted.submitted_operations, OPERATIONS[usize::from(prefix)]);
            assert_eq!(interrupted.unacknowledged_actions, u8::from(missing_receipt));
            assert_eq!(
                interrupted.disposition,
                if missing_receipt {
                    InputDisposition::UnknownEffect
                } else {
                    InputDisposition::Partial
                }
            );
            assert!(!snapshot(&shared).ready);
            assert!(shared.locked().pending.is_none());
            assert_eq!(
                report(&shared.locked().submit(request).unwrap().unwrap()),
                interrupted
            );
            assert_eq!(
                shared.locked().submit(Request { request_id: 2, ..request }),
                Err(Refusal::Stopped)
            );
            if missing_receipt {
                shared.result(ResultEvent::Completed(modified_scroll_receipt(u64::from(prefix))));
                let late = retained(&shared, request);
                assert_eq!(late.end, interrupted.end);
                assert_eq!(late.actions_submitted, prefix + 1);
                assert_eq!(late.submitted_operations, OPERATIONS[usize::from(prefix + 1)]);
                assert_eq!(late.unacknowledged_actions, 0);
                assert!(!snapshot(&shared).ready, "late evidence cannot resume the wheel sequence");
                assert!(shared.locked().pending.is_none());
                assert_eq!(
                    shared.locked().submit(Request { request_id: 2, ..request }),
                    Err(Refusal::Stopped)
                );
            }
        }
    }
}

#[test]
fn modified_scroll_expiry_fences_held_keys_and_keeps_confirmed_wheel_effects() {
    for (prefix, operations) in [(1_u8, 1), (4, 196), (9, 489)] {
        let shared = fresh();
        let request = modified_scroll_request(&shared);
        let mut waiting = Box::pin(shared.submit(request));
        assert!(poll(waiting.as_mut()).is_pending());
        for sequence in 0..u64::from(prefix) {
            encoded(&shared, 1, sequence);
            shared.result(ResultEvent::Completed(modified_scroll_receipt(sequence)));
        }
        {
            let mut state = shared.locked();
            let now = state.now(Instant::now()).unwrap();
            state.ledger.expire(now + 2_000_000).unwrap();
            state.finish_if_retained();
            assert!(state.stopped && state.ledger.is_closed());
            assert!(state.pending.is_none() && state.live.is_none() && state.pinned.is_none());
        }
        drop(waiting);
        let expired = retained(&shared, request);
        assert_eq!(expired.end, native::End::Expired);
        assert_eq!(expired.disposition, InputDisposition::Partial);
        assert_eq!(expired.actions_encoded, prefix);
        assert_eq!(expired.actions_submitted, prefix);
        assert_eq!(expired.submitted_operations, operations);
        assert_eq!(expired.unacknowledged_actions, 0);
        assert_eq!(
            report(&shared.locked().submit(request).unwrap().unwrap()),
            expired
        );
        assert_eq!(
            shared.locked().submit(Request { request_id: 2, ..request }),
            Err(Refusal::Stopped)
        );
    }
}

#[test]
fn status_discloses_only_the_original_grants_signed_bounds_and_capabilities() {
    use fr_core::input_submission::{Capabilities, Capability};
    for (capabilities, expected) in [
        (Capabilities::default(), vec![]),
        (
            Capabilities::default().with(Capability::Keys),
            vec!["physical_keys"],
        ),
        (
            Capabilities::default()
                .with(Capability::Buttons)
                .with(Capability::Absolute),
            vec!["absolute_pointer", "buttons"],
        ),
        (
            Capabilities::default()
                .with(Capability::LineScroll)
                .with(Capability::Buttons)
                .with(Capability::Absolute)
                .with(Capability::Keys),
            vec!["physical_keys", "absolute_pointer", "buttons", "line_scroll"],
        ),
        (
            Capabilities::default()
                .with(Capability::Repeat)
                .with(Capability::Relative)
                .with(Capability::PixelScroll)
                .with(Capability::Text),
            vec![],
        ),
    ] {
        let shared = fresh();
        shared.locked().live.as_mut().unwrap().capabilities = capabilities;
        let actual = snapshot(&shared);
        let support = actual.input_support.as_ref().expect("known original grant");
        assert!(support.valid());
        let value = serde_json::to_value(&actual).unwrap();
        assert_eq!(
            value["input_support"],
            serde_json::json!({
                "bounds": {"x": -800, "y": -200, "width": 1600, "height": 800},
                "capabilities": expected,
            })
        );
        assert_eq!(shared.locked().pinned.unwrap().1.support(), *support);
        let bytes = serde_json::to_vec(&Response::Status {
            snapshot: actual.clone(),
        })
        .unwrap();
        assert!(bytes.len() <= MAX_REPLY);
        let decoded: Snapshot = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, actual);
        assert_eq!(validate_snapshot(&decoded), Ok(()));
        shared.retire();
        assert!(snapshot(&shared).input_support.is_none());
    }
}

#[test]
fn legacy_ready_status_keeps_absent_support_unknown_and_clearing_drops_known_support() {
    let shared = fresh();
    let actual = snapshot(&shared);
    let mut legacy = serde_json::to_value(&actual).unwrap();
    legacy.as_object_mut().unwrap().remove("input_support");
    let mut decoded: Snapshot = serde_json::from_value(legacy).unwrap();
    assert!(decoded.ready);
    assert!(decoded.input_support.is_none());
    assert_eq!(validate_snapshot(&decoded), Ok(()));
    assert_eq!(decoded.lease, actual.lease);
    assert_eq!(decoded.observation_id, actual.observation_id);
    clear_snapshot(&mut decoded);
    assert_eq!(validate_snapshot(&decoded), Ok(()));
    assert!(decoded.input_support.is_none());

    let mut cleared = actual.clone();
    clear_snapshot(&mut cleared);
    assert_eq!(validate_snapshot(&cleared), Ok(()));
    assert!(cleared.input_support.is_none());
    cleared.input_support = actual.input_support;
    assert_eq!(validate_snapshot(&cleared), Err(Error::Protocol));
}

#[test]
fn status_refuses_invalid_bounds_duplicates_noncanonical_and_unknown_capabilities() {
    use crate::robot_input::{InputCapability, InputRectangle};
    let shared = fresh();
    let actual = snapshot(&shared);
    for (x, y, width, height, valid) in [
        (-800, -200, 1600, 800, true),
        (i32::MAX, i32::MAX, 1, 1, true),
        (i32::MIN, i32::MIN, u32::MAX, u32::MAX, true),
        (-800, -200, 0, 800, false),
        (-800, -200, 1600, 0, false),
        (i32::MAX, 0, 2, 1, false),
        (0, i32::MAX, 1, 2, false),
        (0, 0, u32::MAX, 1, false),
    ] {
        let mut changed = actual.clone();
        changed.input_support.as_mut().unwrap().bounds = InputRectangle {
            x,
            y,
            width,
            height,
        };
        assert_eq!(validate_snapshot(&changed).is_ok(), valid);
    }
    for capabilities in [
        vec![InputCapability::PhysicalKeys, InputCapability::PhysicalKeys],
        vec![InputCapability::Buttons, InputCapability::AbsolutePointer],
        vec![
            InputCapability::PhysicalKeys,
            InputCapability::AbsolutePointer,
            InputCapability::Buttons,
            InputCapability::LineScroll,
            InputCapability::PhysicalKeys,
        ],
    ] {
        let mut changed = actual.clone();
        changed.input_support.as_mut().unwrap().capabilities = capabilities;
        assert_eq!(validate_snapshot(&changed), Err(Error::Protocol));
    }
    for capabilities in [
        serde_json::json!(["text"]),
        serde_json::json!(["relative"]),
        serde_json::json!(["physical_keys", "unknown"]),
    ] {
        let mut value = serde_json::to_value(&actual).unwrap();
        value["input_support"]["capabilities"] = capabilities;
        assert!(serde_json::from_value::<Snapshot>(value).is_err());
    }
    let mut extra = serde_json::to_value(&actual).unwrap();
    extra["input_support"]["ticket"] = 1.into();
    assert!(serde_json::from_value::<Snapshot>(extra).is_err());
    let mut extra = serde_json::to_value(&actual).unwrap();
    extra["input_support"]["bounds"]["z"] = 1.into();
    assert!(serde_json::from_value::<Snapshot>(extra).is_err());
}
