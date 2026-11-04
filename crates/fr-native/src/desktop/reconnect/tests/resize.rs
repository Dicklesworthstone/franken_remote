//! Policy fixtures only: native resize/retirement are exercised by viewer_window.
use super::*;
use crate::viewer_input::StopReason as InputStop;
use crate::viewer_window::StopReason as WindowStop;
use std::os::unix::process::ExitStatusExt;

fn summary() -> CleanupSummary {
    CleanupSummary {
        disconnect: None,
        control_disconnect: None,
        input_stop: None,
        failure: None,
    }
}
fn retired() -> Cleanup {
    let mut cleanup = report();
    cleanup.media = Ok(Some(std::process::ExitStatus::from_raw(0)));
    cleanup.input = CaptureCleanup::Complete;
    cleanup.window = WindowCleanup::Complete(WindowStop::GeometryChanged);
    cleanup
}

#[test]
fn local_resize_requires_original_window_and_every_native_cleanup_receipt() {
    for input in [None, Some(InputStop::Local), Some(InputStop::Closed), Some(InputStop::WindowResized)] {
        let mut summary = summary();
        summary.input_stop = input;
        assert!(resize_cleanup_allows_reopen(summary, Some(&retired())));
        assert!(!resize_cleanup_allows_reopen(summary, None));
    }
    for state in [
        WindowCleanup::NotStarted,
        WindowCleanup::Pending,
        WindowCleanup::Complete(WindowStop::NativeFailure),
        WindowCleanup::Complete(WindowStop::Hidden),
        WindowCleanup::Complete(WindowStop::User),
        WindowCleanup::Complete(WindowStop::SessionEnded),
    ] {
        let mut cleanup = retired();
        cleanup.window = state;
        assert!(!resize_cleanup_allows_reopen(summary(), Some(&cleanup)));
    }
    let mut cleanup = retired();
    cleanup.media = Ok(None);
    assert!(!resize_cleanup_allows_reopen(summary(), Some(&cleanup)));
    cleanup = retired();
    cleanup.input = CaptureCleanup::Pending;
    assert!(!resize_cleanup_allows_reopen(summary(), Some(&cleanup)));
    cleanup = retired();
    cleanup.picker = PickerCleanup::Pending;
    assert!(!resize_cleanup_allows_reopen(summary(), Some(&cleanup)));
    cleanup = retired();
    cleanup.clipboard = Ok(frd::native_clipboard::Cleanup::Pending);
    assert!(!resize_cleanup_allows_reopen(summary(), Some(&cleanup)));
    cleanup = retired();
    cleanup.files = Err(frd::session_startup::FileSendError::Cancelled);
    assert!(!resize_cleanup_allows_reopen(summary(), Some(&cleanup)));
}

#[test]
fn original_input_stops_host_reports_and_callback_failures_veto_resize() {
    for reason in [
        InputStop::FocusLost, InputStop::WindowChanged, InputStop::NativeFailure,
        InputStop::LocalEscape, InputStop::SyntheticInput, InputStop::Expired,
    ] {
        let mut value = summary();
        value.input_stop = Some(reason);
        assert!(!resize_cleanup_allows_reopen(value, Some(&retired())));
    }
    for reason in [
        fr_wire::closure::ClosedReason::PermissionLost,
        fr_wire::closure::ClosedReason::HostFailure,
        fr_wire::closure::ClosedReason::ClientRequested,
        fr_wire::closure::ClosedReason::GeometryChanged,
    ] {
        let mut value = summary();
        value.disconnect = Some(ViewerCloseOutcome {
            request_acknowledged: false,
            report: Some(fr_wire::closure::Closed {
                reason,
                cleanup: fr_wire::closure::Cleanup::Unconfirmed,
                effects: fr_wire::closure::OutstandingEffects::Unknown,
            }),
            transport: Ok(()),
        });
        let original = value;
        assert!(!resize_cleanup_allows_reopen(value, Some(&retired())));
        assert_eq!(value, original);
    }
    let mut value = summary();
    value.failure = Some(CleanupFailure::Media);
    assert!(!resize_cleanup_allows_reopen(value, Some(&retired())));
    assert!(!resize_local_error(Some(Error::Observer(ObserverError::Application))));
    assert!(!resize_local_error(Some(Error::Observer(ObserverError::Streaming(
        frd::session_startup::StreamingViewerError::Application
    )))));
    assert!(!resize_local_error(Some(Error::LayoutMismatch)));
    assert!(!resize_local_error(Some(Error::Window(crate::viewer_window::Error::NativeStopped(
        WindowStop::Hidden
    )))));
    assert!(resize_local_error(None));
    assert!(resize_local_error(Some(Error::NotViewing)));
    assert!(resize_local_error(Some(Error::Window(crate::viewer_window::Error::NativeStopped(
        WindowStop::GeometryChanged
    )))));
}

#[test]
fn resizing_retains_only_fit_preference_and_creates_fresh_native_owners() {
    let mut original = session(71).with_window_resizing();
    original.last_attempt = 1;
    original.configuration.fit_window = Some((960, 540));
    let fresh = original.configuration(2).unwrap();
    assert_eq!(fresh.worker_epoch, 72);
    assert_eq!(fresh.fit_window, Some((960, 540)));
    assert!(!fresh.display_picker, "resize alone does not choose a different display");
    let desktop = Desktop::new(fresh);
    assert!(desktop.window().is_none());
    assert!(desktop.input().is_none());
    assert!(desktop.picker().is_none());
    assert!(desktop.stop.is_none());
    original.configuration.display_picker = true;
    assert!(original.configuration(3).unwrap().display_picker);
}

#[test]
fn opting_in_without_an_established_native_receipt_cannot_reopen() {
    let mut value = Session::new(
        Configuration::new(Path::new("/usr/bin/false"), ":0", None, 71).unwrap(),
        ObserverPolicy::default(),
        Mode::Observe,
        SummaryUi::default(),
    ).with_window_resizing();
    for failure in [Failure::Cancelled, Failure::Observation(ObserverError::Application)] {
        assert_eq!(Application::local_layout_change(&mut value, failure), Ok(None));
    }
    value.prepare_attempt(1).unwrap();
    assert!(value.resize_window.is_none());
    assert_eq!(Application::local_layout_change(&mut value, Failure::Cancelled), Ok(None));
    assert!(value.desktop.is_some(), "unreaped original remains owned");
}
