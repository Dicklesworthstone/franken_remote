//! Receipt policy fixtures; native lifecycle evidence is tested by viewer_window.
use super::*;
use crate::{viewer_input::StopReason as InputStop, viewer_window::StopReason as WindowStop};

fn summary() -> CleanupSummary {
    CleanupSummary {
        disconnect: None,
        control_disconnect: None,
        input_stop: Some(InputStop::FocusLost),
        failure: None,
    }
}
fn retired() -> Cleanup {
    let mut cleanup = report();
    cleanup.media = Ok(Some(asupersync::process::ExitStatus::from_parts(
        Some(0),
        None,
    )));
    cleanup.input = CaptureCleanup::Complete;
    cleanup.window = WindowCleanup::Complete(WindowStop::SessionEnded);
    cleanup
}

#[test]
fn focus_recovery_requires_every_original_native_owner_to_finish() {
    assert!(focus_loss_cleanup_allows_retry(summary(), Some(&retired())));
    assert!(!focus_loss_cleanup_allows_retry(summary(), None));
    for state in [CaptureCleanup::NotStarted, CaptureCleanup::Pending] {
        let mut cleanup = retired();
        cleanup.input = state;
        assert!(!focus_loss_cleanup_allows_retry(summary(), Some(&cleanup)));
    }
    for state in [
        WindowCleanup::NotStarted,
        WindowCleanup::Pending,
        WindowCleanup::Complete(WindowStop::User),
        WindowCleanup::Complete(WindowStop::Hidden),
        WindowCleanup::Complete(WindowStop::WindowLost),
        WindowCleanup::Complete(WindowStop::GeometryChanged),
        WindowCleanup::Complete(WindowStop::NativeFailure),
    ] {
        let mut cleanup = retired();
        cleanup.window = state;
        assert!(!focus_loss_cleanup_allows_retry(summary(), Some(&cleanup)));
    }
    let mut cleanup = retired();
    cleanup.media = Ok(None);
    assert!(!focus_loss_cleanup_allows_retry(summary(), Some(&cleanup)));
    let mut cleanup = retired();
    cleanup.picker = PickerCleanup::Pending;
    assert!(!focus_loss_cleanup_allows_retry(summary(), Some(&cleanup)));
    let mut cleanup = retired();
    cleanup.clipboard = Ok(frd::native_clipboard::Cleanup::Pending);
    assert!(!focus_loss_cleanup_allows_retry(summary(), Some(&cleanup)));
    let mut cleanup = retired();
    cleanup.files = Err(frd::session_startup::FileSendError::Cancelled);
    cleanup.file_result = Some(frd::session_startup::FileSendReceipt {
        id: 17,
        outcome: frd::session_startup::FileSendOutcome::PublicationUnknown,
    });
    assert!(!focus_loss_cleanup_allows_retry(summary(), Some(&cleanup)));
    assert_eq!(
        cleanup.file_result.unwrap().id,
        17,
        "original effects are not discarded"
    );
}

#[test]
fn all_host_reports_and_other_native_stops_veto_focus_recovery() {
    for stop in [
        None,
        Some(InputStop::Local),
        Some(InputStop::Closed),
        Some(InputStop::LocalEscape),
        Some(InputStop::WindowChanged),
        Some(InputStop::WindowResized),
        Some(InputStop::NativeFailure),
        Some(InputStop::Expired),
        Some(InputStop::SyntheticInput),
    ] {
        let value = CleanupSummary {
            input_stop: stop,
            ..summary()
        };
        assert!(!value.focus_loss_only());
        assert!(!focus_loss_cleanup_allows_retry(value, Some(&retired())));
    }
    for reason in [
        fr_wire::closure::ClosedReason::ClientRequested,
        fr_wire::closure::ClosedReason::PermissionLost,
        fr_wire::closure::ClosedReason::AuthorityExpired,
        fr_wire::closure::ClosedReason::HostFailure,
        fr_wire::closure::ClosedReason::GeometryChanged,
    ] {
        let value = CleanupSummary {
            disconnect: Some(ViewerCloseOutcome {
                request_acknowledged: false,
                report: Some(fr_wire::closure::Closed {
                    reason,
                    cleanup: fr_wire::closure::Cleanup::Unconfirmed,
                    effects: fr_wire::closure::OutstandingEffects::Known {
                        pending: 2,
                        uncertain: 1,
                    },
                }),
                transport: Ok(()),
            }),
            ..summary()
        };
        assert!(!focus_loss_cleanup_allows_retry(value, Some(&retired())));
        assert_eq!(value.disconnect.unwrap().report.unwrap().reason, reason);
    }
    let value = CleanupSummary {
        failure: Some(CleanupFailure::Files),
        ..summary()
    };
    assert!(!focus_loss_cleanup_allows_retry(value, Some(&retired())));
}

#[test]
fn opt_in_or_focus_metadata_without_original_window_evidence_never_retries() {
    let mut session = Session::new(
        Configuration::new(Path::new("/usr/bin/false"), ":0", None, 71).unwrap(),
        ObserverPolicy::default(),
        Mode::ControlCapable(ControlPolicy {
            clock: ClockPolicy::default(),
            input: input::Policy::default(),
            sequence: 1,
            capabilities: Capabilities::default(),
        }),
        SummaryUi::default(),
    )
    .with_focus_loss_recovery();
    session.current_attempt = 1;
    session.last_attempt = 1;
    session.input_stop = Some(InputStop::FocusLost);
    session.cleanup = Some(retired());
    assert_eq!(
        Application::retry_viewing_after_local_stop(&mut session, Failure::Cancelled),
        Ok(false)
    );
    assert!(session.focus_loss_window.is_none());
    assert!(focus_loss_local_error(None));
    for error in [
        Error::NotViewing,
        Error::Window(crate::viewer_window::Error::NotReady),
        Error::Window(crate::viewer_window::Error::SessionEnded),
        Error::Window(crate::viewer_window::Error::NativeStopped(
            WindowStop::SessionEnded,
        )),
    ] {
        assert!(focus_loss_local_error(Some(error)), "{error:?}");
    }
    for error in [
        Error::Observer(ObserverError::Application),
        Error::Observer(ObserverError::Streaming(
            frd::session_startup::StreamingViewerError::Application,
        )),
        Error::Window(crate::viewer_window::Error::NativeStopped(
            WindowStop::Hidden,
        )),
        Error::Window(crate::viewer_window::Error::NativeStopped(
            WindowStop::NativeFailure,
        )),
        Error::Window(crate::viewer_window::Error::Launch(
            frd::worker::Error::PipeFailed,
        )),
        Error::LayoutMismatch,
    ] {
        assert!(!focus_loss_local_error(Some(error)), "{error:?}");
    }
}

#[test]
fn a_late_lease_report_vetoes_focus_recovery_without_losing_its_effect_receipts() {
    use fr_wire::lease_revoked::{CleanupStage, EffectStage, Reason, Revoked};
    for reason in [
        Reason::LocalRevoke,
        Reason::LeaseExpired,
        Reason::ClientRequested,
        Reason::SessionEnded,
    ] {
        let revocation = Revoked {
            lease: fr_core::ids::InputLeaseId::from_raw(17),
            reason,
            cleanup: CleanupStage::Fenced,
            effects: EffectStage::ReceiptsPending,
        };
        let value = CleanupSummary {
            control_disconnect: Some(frd::session_startup::ControlCloseOutcome {
                exchange: ViewerCloseOutcome {
                    request_acknowledged: false,
                    report: None,
                    transport: Ok(()),
                },
                revocation: Some(revocation),
            }),
            ..summary()
        };
        assert!(!focus_loss_cleanup_allows_retry(value, Some(&retired())));
        assert_eq!(
            value.control_disconnect.unwrap().revocation,
            Some(revocation)
        );
    }
}
