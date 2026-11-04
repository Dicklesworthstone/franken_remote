//! Original-session bridge checks. Actual transfer/publication uses the real
//! controller/ATP fixture in `viewer::controlled::tests::files::download`.
use super::*;
use crate::native_files::{HostDownload, HostSelection, SendPhase};
use asupersync::{
    runtime::RuntimeBuilder,
    types::{Budget, CancelKind},
};
use fr_files::sender::batch::Selection;
use std::{fs::File, time::Duration};

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
fn interactive_configuration_binds_once_and_never_queues_before_its_original_owner() {
    let rt = RuntimeBuilder::current_thread().build().unwrap();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let (selection, control) = HostSelection::interactive();
    let clone = selection.clone();
    assert!(selection.is_available());
    assert!(!control.ready() && !control.is_closed());
    assert!(matches!(control.submit(request()), Err(Error::Busy)));
    let HostDownload::Interactive(intake) = selection.take(cx.clone()).unwrap() else {
        panic!("interactive source, never an empty batch");
    };
    assert!(!selection.is_available() && !clone.is_available());
    assert!(matches!(clone.take(cx.clone()), Err(Absence::Unavailable)));
    assert!(matches!(control.bind(cx), Err(Absence::Unavailable)));
    assert!(intake.take().is_none());
    assert!(!control.ready());
    intake.set_ready(true);
    assert!(control.ready());
}

#[test]
fn bridge_preserves_one_pending_slot_deadline_and_independent_batch_controls() {
    let rt = RuntimeBuilder::current_thread().build().unwrap();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let bridge = HostSendIntake::new();
    let intake = bridge.bind(cx.clone()).unwrap();
    intake.set_ready(true);
    let before = cx.timer_driver().unwrap().now().as_nanos() / 1000;
    let first = bridge.submit(request()).unwrap();
    let after = cx.timer_driver().unwrap().now().as_nanos() / 1000;
    assert!(!bridge.ready());
    assert!(matches!(bridge.submit(request()), Err(Error::Busy)));
    let pending = intake.take().unwrap();
    assert!((before + 5_000_000..=after + 5_000_000).contains(&pending.until_us));
    assert_eq!(pending.control.snapshot(), first.snapshot());
    // A completed old UI handle cannot cancel the next explicit selection.
    first.update(|s| s.phase = SendPhase::Finished);
    intake.set_ready(true);
    let second = bridge.submit(request()).unwrap();
    assert!(!first.cancel());
    assert!(!second.snapshot().cancel_requested);
    bridge.close();
    assert_eq!(
        second.status().absence,
        Some(Absence::Refused(Error::Cancelled))
    );
    assert_eq!(second.snapshot().cleanup, Some(Ok(())));
    assert_eq!(first.status().phase, SendPhase::Finished);
    assert!(intake.take().is_none());
}

#[test]
fn terminal_close_fences_a_late_selector_and_cancels_only_the_active_batch() {
    let rt = RuntimeBuilder::current_thread().build().unwrap();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let (selection, bridge) = HostSelection::interactive();
    let HostDownload::Interactive(intake) = selection.take(cx.clone()).unwrap() else {
        panic!("interactive owner");
    };
    intake.set_ready(true);
    let active = bridge.submit(request()).unwrap();
    let _original = intake.take().unwrap();
    let late_selection = request();
    bridge.clone().close();
    assert!(bridge.is_closed() && intake.is_closed());
    assert!(!bridge.ready());
    assert!(active.snapshot().cancel_requested);
    assert_eq!(
        active.snapshot().cleanup,
        None,
        "stop is not proof of cleanup"
    );
    assert!(matches!(bridge.submit(late_selection), Err(Error::Closed)));
    assert!(matches!(bridge.bind(cx), Err(Absence::Unavailable)));
    intake.set_ready(true);
    assert!(!bridge.ready());
}

#[test]
fn closing_before_admission_and_cancelling_original_context_never_rebind() {
    let rt = RuntimeBuilder::current_thread().build().unwrap();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let (selection, bridge) = HostSelection::interactive();
    bridge.close();
    assert!(matches!(
        selection.take(cx.clone()),
        Err(Absence::Unavailable)
    ));
    assert!(!selection.is_available());
    assert!(matches!(bridge.submit(request()), Err(Error::Closed)));

    let (selection, bridge) = HostSelection::interactive();
    let HostDownload::Interactive(intake) = selection.take(cx.clone()).unwrap() else {
        panic!("interactive owner");
    };
    intake.set_ready(true);
    cx.cancel_fast(CancelKind::User);
    assert!(bridge.is_closed() && !bridge.ready());
    assert!(matches!(bridge.submit(request()), Err(Error::Closed)));
    let replacement = rt.request_cx_with_budget(Budget::INFINITE);
    assert!(matches!(
        bridge.bind(replacement),
        Err(Absence::Unavailable)
    ));
}
