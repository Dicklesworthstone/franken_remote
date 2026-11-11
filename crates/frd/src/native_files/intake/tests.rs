//! Local bounded admission tests. Real transfer/publication is covered by the
//! original-session repeated-selection test in controlled/files/owners.
use super::*;
use asupersync::{
    runtime::RuntimeBuilder,
    types::{Budget, CancelKind},
};
use fr_files::sender::batch::Selection;
use std::fs::File;

fn request() -> SendRequest {
    let mut selection = Selection::new();
    selection
        .push(
            File::open(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).unwrap(),
            "selected",
        )
        .unwrap();
    SendRequest {
        selection,
        policy: Policy::default(),
        lifetime: Duration::from_secs(5),
    }
}

#[test]
fn one_slot_refuses_overlap_and_keeps_the_original_submission_deadline() {
    let rt = RuntimeBuilder::current_thread().build().unwrap();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let intake = SendIntake::new(cx, Policy::default());
    assert!(!intake.ready());
    assert!(matches!(intake.submit(request()), Err(Error::Busy)));
    intake.set_ready(true);
    let before = intake.now().unwrap();
    let control = intake.submit(request()).unwrap();
    let after = intake.now().unwrap();
    assert!(!intake.ready());
    assert!(matches!(intake.submit(request()), Err(Error::Busy)));
    let deadline = intake.lock().pending.as_ref().unwrap().until_us;
    assert!((before + 5_000_000..=after + 5_000_000).contains(&deadline));
    for _ in 0..16 {
        assert!(!intake.ready());
    }
    let original = intake.take().unwrap();
    assert_eq!(original.until_us, deadline);
    assert_eq!(original.control.snapshot(), control.snapshot());
    assert_eq!(original.request.selection.len(), 1);
}

#[test]
fn another_selection_cannot_change_the_original_lane_policy() {
    let rt = RuntimeBuilder::current_thread().build().unwrap();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let intake = SendIntake::new(cx, Policy::default());
    intake.set_ready(true);
    for changed in 0..4 {
        let mut request = request();
        match changed {
            0 => request.policy.max_file_bytes += 1,
            1 => request.policy.bytes_per_second += 1,
            2 => request.policy.transfer_lifetime += Duration::from_secs(1),
            _ => request.policy.record_lifetime += Duration::from_secs(1),
        }
        assert!(matches!(intake.submit(request), Err(Error::Limits)));
        assert!(intake.ready());
    }
    for lifetime in [Duration::ZERO, Duration::from_secs(3601)] {
        let mut request = request();
        request.lifetime = lifetime;
        assert!(matches!(intake.submit(request), Err(Error::Limits)));
    }
    let mut empty = request();
    empty.selection = Selection::new();
    assert!(matches!(intake.submit(empty), Err(Error::Limits)));
    assert!(intake.ready());
}

#[test]
fn original_session_closure_discards_pending_descriptors_and_cannot_reopen() {
    let rt = RuntimeBuilder::current_thread().build().unwrap();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let intake = SendIntake::new(cx.clone(), Policy::default());
    intake.set_ready(true);
    let control = intake.submit(request()).unwrap();
    cx.cancel_fast(CancelKind::User);
    assert!(intake.is_closed());
    assert!(!intake.ready());
    intake.close();
    assert!(intake.take().is_none());
    assert_eq!(
        control.status().absence,
        Some(Absence::Refused(Error::Cancelled))
    );
    assert_eq!(control.snapshot().cleanup, Some(Ok(())));
    intake.set_ready(true);
    assert!(matches!(intake.submit(request()), Err(Error::Closed)));
    let other = SendIntake::new(
        rt.request_cx_with_budget(Budget::INFINITE),
        Policy::default(),
    );
    other.set_ready(true);
    let current = other.submit(request()).unwrap();
    assert!(!control.cancel());
    assert!(!current.cancel_requested());
}

// Hold the real state mutex across the first clock sample. An ordinary owner
// turn then advances its clock before the submitter can acquire that mutex.
// No sleep or scheduler speed determines the clock values or expected outcome.
fn submit_after_owner_turn(
    intake: &SendIntake,
    request: SendRequest,
    submitted: u64,
    owner_time: u64,
    acquired: u64,
) -> Result<SendControl, Error> {
    std::thread::scope(|scope| {
        let mut state = intake.lock();
        let (sampled, waiting) = std::sync::mpsc::sync_channel(1);
        let worker = scope.spawn(move || {
            let mut first = true;
            intake.submit_with_clock(request, || {
                if std::mem::take(&mut first) {
                    sampled.send(()).unwrap();
                    Ok(submitted)
                } else {
                    Ok(acquired)
                }
            })
        });
        let sample = waiting.recv_timeout(Duration::from_secs(5));
        state.last = owner_time;
        // Release even if the worker failed, before any assertion can unwind.
        drop(state);
        sample.unwrap();
        worker.join().unwrap()
    })
}

#[test]
fn contended_submission_uses_fresh_ordering_but_keeps_original_deadline() {
    let rt = RuntimeBuilder::current_thread().build().unwrap();
    let intake = SendIntake::new(
        rt.request_cx_with_budget(Budget::INFINITE),
        Policy::default(),
    );
    intake.set_ready(true);
    let mut selected = request();
    selected.lifetime = Duration::from_micros(10);
    let control = submit_after_owner_turn(&intake, selected, 100, 105, 109).unwrap();
    let state = intake.lock();
    assert_eq!(state.last, 109);
    let pending = state.pending.as_ref().unwrap();
    assert_eq!(
        pending.until_us, 110,
        "lock waiting cannot renew the selection"
    );
    assert_eq!(pending.control.snapshot(), control.snapshot());
    assert_eq!(pending.request.selection.len(), 1);
}

#[test]
fn contended_submission_expires_at_its_original_deadline_without_consuming_slot() {
    for acquired in [110, 111, 10_000] {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        let intake = SendIntake::new(
            rt.request_cx_with_budget(Budget::INFINITE),
            Policy::default(),
        );
        intake.set_ready(true);
        let mut selected = request();
        selected.lifetime = Duration::from_micros(10);
        assert!(matches!(
            submit_after_owner_turn(&intake, selected, 100, 105, acquired),
            Err(Error::Expired)
        ));
        assert!(
            intake.ready(),
            "a refused selection never owns the idle slot"
        );
        assert!(intake.take().is_none());
        assert_eq!(intake.lock().last, acquired);
    }
}

#[test]
fn contended_submission_still_refuses_real_clock_regression() {
    // The fresh sample must follow BOTH this submitter's original sample and
    // the latest sample serialized by the owner, independently.
    for (submitted, owner_time, acquired) in [(100, 90, 99), (100, 105, 104)] {
        let rt = RuntimeBuilder::current_thread().build().unwrap();
        let intake = SendIntake::new(
            rt.request_cx_with_budget(Budget::INFINITE),
            Policy::default(),
        );
        intake.set_ready(true);
        assert!(matches!(
            submit_after_owner_turn(&intake, request(), submitted, owner_time, acquired),
            Err(Error::Clock)
        ));
        assert_eq!(intake.lock().last, owner_time);
        assert!(intake.ready());
        assert!(intake.take().is_none());
    }
}

#[test]
fn terminal_maintenance_during_submission_preserves_capacity_and_old_receipt() {
    let rt = RuntimeBuilder::current_thread().build().unwrap();
    let intake = SendIntake::new(
        rt.request_cx_with_budget(Budget::INFINITE),
        Policy::default(),
    );
    intake.set_ready(true);
    let original = intake.submit_with_clock(request(), || Ok(100)).unwrap();
    let original_snapshot = original.snapshot();
    let original_until = intake.lock().pending.as_ref().unwrap().until_us;
    let mut first = true;
    let overlapping = intake.submit_with_clock(request(), || {
        if std::mem::take(&mut first) {
            // Service the pending selection after the second submission's
            // original sample, while it has yet to acquire the state mutex.
            assert!(intake.take_terminal_with_clock(|| Ok(200)).is_none());
            Ok(150)
        } else {
            Ok(250)
        }
    });
    assert!(matches!(overlapping, Err(Error::Busy)));
    assert_eq!(original.snapshot(), original_snapshot);
    assert_eq!(
        intake.lock().pending.as_ref().unwrap().until_us,
        original_until
    );
    assert_eq!(intake.lock().last, 250);

    // The owner's normal cancellation drain leaves the first exact handle
    // available for its own terminal receipt. A new selection cannot reuse it.
    assert!(original.cancel());
    let (pending, reason) = intake
        .take_terminal_with_clock(|| panic!("cancel needs no clock"))
        .unwrap();
    assert_eq!(reason, Error::Cancelled);
    pending.control.end(Absence::Refused(reason));
    pending.control.cleaned(Some(Ok(())));
    let terminal = original.snapshot();
    intake.set_ready(true);
    let next = intake.submit_with_clock(request(), || Ok(300)).unwrap();
    assert!(!original.cancel());
    assert!(!next.cancel_requested());
    assert_eq!(original.snapshot(), terminal);
    assert_eq!(
        terminal.status.absence,
        Some(Absence::Refused(Error::Cancelled))
    );
    assert_eq!(terminal.cleanup, Some(Ok(())));
}

#[test]
fn submission_after_concurrent_expiry_drain_keeps_its_original_deadline() {
    let rt = RuntimeBuilder::current_thread().build().unwrap();
    let intake = SendIntake::new(
        rt.request_cx_with_budget(Budget::INFINITE),
        Policy::default(),
    );
    intake.set_ready(true);
    let mut old_request = request();
    old_request.lifetime = Duration::from_micros(10);
    let previous = intake.submit_with_clock(old_request, || Ok(100)).unwrap();
    let mut first = true;
    let current = intake
        .submit_with_clock(request(), || {
            if std::mem::take(&mut first) {
                let (pending, error) = intake.take_terminal_with_clock(|| Ok(110)).unwrap();
                assert_eq!(error, Error::Expired);
                assert_eq!(pending.until_us, 110);
                pending.control.end(Absence::Refused(error));
                pending.control.cleaned(Some(Ok(())));
                intake.set_ready(true);
                Ok(105)
            } else {
                Ok(111)
            }
        })
        .unwrap();
    assert_eq!(intake.lock().last, 111);
    assert_eq!(intake.lock().pending.as_ref().unwrap().until_us, 5_000_105);
    assert_eq!(
        previous.status().absence,
        Some(Absence::Refused(Error::Expired))
    );
    assert_eq!(previous.snapshot().cleanup, Some(Ok(())));
    assert!(!previous.cancel());
    assert!(!current.cancel_requested());
}

#[test]
fn closure_between_submission_samples_never_queues_a_late_selection() {
    let rt = RuntimeBuilder::current_thread().build().unwrap();
    let intake = SendIntake::new(
        rt.request_cx_with_budget(Budget::INFINITE),
        Policy::default(),
    );
    intake.set_ready(true);
    assert!(matches!(
        intake.submit_with_clock(request(), || {
            intake.close();
            Ok(100)
        }),
        Err(Error::Closed)
    ));
    assert!(intake.is_closed());
    assert!(!intake.ready());
    assert!(intake.take().is_none());
}
