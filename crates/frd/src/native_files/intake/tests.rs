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
