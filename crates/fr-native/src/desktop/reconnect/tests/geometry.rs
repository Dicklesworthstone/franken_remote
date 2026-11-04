//! Fresh viewing after a checked resize retains all original terminal evidence.
use super::*;
use fr_wire::{closure, lease_revoked};
use frd::session_startup::{ControlCloseOutcome, ControlledViewerError, StreamingViewerError};

fn summary() -> CleanupSummary {
    CleanupSummary {
        disconnect: None,
        control_disconnect: Some(ControlCloseOutcome {
            exchange: ViewerCloseOutcome {
                request_acknowledged: false,
                report: Some(closure::Closed {
                    reason: closure::ClosedReason::GeometryChanged,
                    cleanup: closure::Cleanup::Unconfirmed,
                    effects: closure::OutstandingEffects::Known {
                        pending: 2,
                        uncertain: 1,
                    },
                }),
                transport: Err(fr_transport::quic::Error::Expired),
            },
            revocation: Some(lease_revoked::Revoked {
                lease: fr_core::ids::InputLeaseId::from_raw(17),
                reason: lease_revoked::Reason::GeometryChanged,
                cleanup: lease_revoked::CleanupStage::Fenced,
                effects: lease_revoked::EffectStage::ReceiptsPending,
            }),
        }),
        input_stop: Some(crate::viewer_input::StopReason::Closed),
        failure: None,
    }
}

#[test]
fn geometry_allows_only_fresh_viewing_and_preserves_unknown_cleanup_and_effects() {
    let original = summary();
    assert!(!original.prevents_viewing_retry());
    assert_eq!(original.retry_failure(), None);
    assert!(
        retained_failure(original, true).is_some(),
        "keep the terminal reason for retry status"
    );
    assert!(
        !requested_close_allows_reopen(original),
        "resize is not a successful user display choice"
    );
    let mut session = Session::new(
        Configuration::new(Path::new("/usr/bin/false"), ":0", None, 71).unwrap(),
        ObserverPolicy::default(),
        Mode::Observe,
        OfferUi::default(),
    );
    session.current_attempt = 1;
    session.last_attempt = 1;
    session.control_disconnect = original.control_disconnect;
    assert!(Application::terminal_failure(&session).is_some());
    Application::status(&mut session, Status::Connecting { attempt: 2 }).unwrap();
    assert_eq!(Application::terminal_failure(&session), None);
    assert_eq!(
        session.control_disconnect_outcome(),
        original.control_disconnect
    );
    let configuration = session.configuration(2).unwrap();
    assert_eq!(configuration.worker_epoch, 72);
    let fresh = Desktop::new(configuration);
    assert!(fresh.input().is_none());
    assert!(fresh.window().is_none());
    assert!(fresh.picker().is_none());
}

#[test]
fn permission_protocol_and_failed_cleanup_cannot_hide_behind_geometry() {
    let original = summary();
    for reason in [
        closure::ClosedReason::PermissionLost,
        closure::ClosedReason::ProtocolError,
        closure::ClosedReason::AuthorityExpired,
        closure::ClosedReason::HostStopping,
    ] {
        let mut value = original;
        let mut report = value.control_disconnect.unwrap().exchange;
        report.report.as_mut().unwrap().reason = reason;
        // Two retained exchanges must both be considered, even when the
        // lease report and first exchange are an otherwise retryable resize.
        value.disconnect = Some(report);
        assert!(value.prevents_viewing_retry());
        assert_eq!(
            value.retry_failure(),
            Some(ObserverError::Session(
                frd::session_startup::Error::RemoteClosed(report.report.unwrap()),
            ))
        );
    }
    let mut value = original;
    let revoked = value
        .control_disconnect
        .as_mut()
        .unwrap()
        .revocation
        .as_mut()
        .unwrap();
    revoked.cleanup = lease_revoked::CleanupStage::Failed;
    let expected = *revoked;
    assert!(value.prevents_viewing_retry());
    assert_eq!(
        value.retry_failure(),
        Some(ObserverError::Streaming(StreamingViewerError::Control(
            ControlledViewerError::LeaseRevoked(expected)
        ),))
    );
    let mut value = original;
    value
        .control_disconnect
        .as_mut()
        .unwrap()
        .exchange
        .report
        .as_mut()
        .unwrap()
        .cleanup = closure::Cleanup::Incomplete;
    assert!(value.prevents_viewing_retry());
    let mut value = original;
    value
        .control_disconnect
        .as_mut()
        .unwrap()
        .exchange
        .transport = Err(fr_transport::quic::Error::Malformed);
    assert!(value.prevents_viewing_retry());
    assert_eq!(
        value.retry_failure(),
        Some(ObserverError::Streaming(StreamingViewerError::Transport(
            fr_transport::quic::Error::Malformed
        ),))
    );
    for reason in [
        crate::viewer_input::StopReason::FocusLost,
        crate::viewer_input::StopReason::LocalEscape,
        crate::viewer_input::StopReason::WindowChanged,
    ] {
        let mut value = original;
        value.input_stop = Some(reason);
        assert!(value.prevents_viewing_retry());
    }
    let mut value = original;
    value.failure = Some(CleanupFailure::Media);
    assert!(value.prevents_viewing_retry());
    assert_eq!(
        original,
        summary(),
        "classification cannot change retained evidence"
    );
}
