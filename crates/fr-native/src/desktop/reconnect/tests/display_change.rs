use super::*;
use fr_wire::{closure, lease_revoked};

fn summary() -> CleanupSummary {
    CleanupSummary {
        disconnect: Some(ViewerCloseOutcome {
            request_acknowledged: true,
            report: Some(closure::Closed {
                reason: closure::ClosedReason::ClientRequested,
                cleanup: closure::Cleanup::Unconfirmed,
                effects: closure::OutstandingEffects::Unknown,
            }),
            transport: Ok(()),
        }),
        control_disconnect: None,
        input_stop: None,
        failure: None,
    }
}

#[test]
fn selecting_another_display_forces_a_fresh_picker_and_checked_worker_generation() {
    let mut session = session(71).with_display_switching();
    assert!(!session.configuration(1).unwrap().display_picker);
    session.last_attempt = 1;
    session.select_fresh_display = true;
    let next = session.configuration(2).unwrap();
    assert!(next.display_picker);
    assert_eq!(next.worker_epoch, 72);
    assert!(session.configuration(32).unwrap().display_picker);
    assert!(matches!(
        session.configuration(1),
        Err(ObserverError::Order)
    ));
    let desktop = Desktop::new(next);
    assert!(desktop.window().is_none());
    assert!(desktop.input().is_none());
    assert!(desktop.picker().is_none());
}

#[test]
fn original_closing_uncertainty_is_preserved_without_inventing_release() {
    let mut summary = summary();
    assert!(requested_close_allows_reopen(summary));
    for transport in [
        Ok(()),
        Err(fr_transport::quic::Error::Expired),
        Err(fr_transport::quic::Error::Closed),
        Err(fr_transport::quic::Error::Backpressure),
    ] {
        let exchange = summary.disconnect.as_mut().unwrap();
        exchange.transport = transport;
        exchange.request_acknowledged = false;
        exchange.report = None;
        let original = summary;
        assert!(requested_close_allows_reopen(summary));
        assert_eq!(summary, original);
    }
    let exchange = summary.disconnect.take().unwrap();
    summary.control_disconnect = Some(frd::session_startup::ControlCloseOutcome {
        exchange,
        revocation: Some(lease_revoked::Revoked {
            lease: fr_core::ids::InputLeaseId::from_raw(17),
            reason: lease_revoked::Reason::SessionEnded,
            cleanup: lease_revoked::CleanupStage::Fenced,
            effects: lease_revoked::EffectStage::Unknown,
        }),
    });
    assert!(requested_close_allows_reopen(summary));
    assert_eq!(
        summary
            .control_disconnect
            .unwrap()
            .revocation
            .unwrap()
            .cleanup,
        lease_revoked::CleanupStage::Fenced
    );
}

#[test]
fn adverse_host_reports_and_real_local_failures_veto_display_reopening() {
    for reason in [
        closure::ClosedReason::PermissionLost,
        closure::ClosedReason::AuthorityExpired,
        closure::ClosedReason::HostStopping,
        closure::ClosedReason::ViewInvalidated,
        closure::ClosedReason::ProtocolError,
        closure::ClosedReason::HostFailure,
        closure::ClosedReason::SessionReplaced,
    ] {
        let mut value = summary();
        value
            .disconnect
            .as_mut()
            .unwrap()
            .report
            .as_mut()
            .unwrap()
            .reason = reason;
        assert!(!requested_close_allows_reopen(value));
    }
    let mut incomplete = summary();
    incomplete
        .disconnect
        .as_mut()
        .unwrap()
        .report
        .as_mut()
        .unwrap()
        .cleanup = closure::Cleanup::Incomplete;
    assert!(!requested_close_allows_reopen(incomplete));
    for reason in [
        crate::viewer_input::StopReason::LocalEscape,
        crate::viewer_input::StopReason::FocusLost,
        crate::viewer_input::StopReason::Expired,
        crate::viewer_input::StopReason::NativeFailure,
    ] {
        let mut value = summary();
        value.input_stop = Some(reason);
        assert!(!requested_close_allows_reopen(value));
    }
    for error in [
        fr_transport::quic::Error::Unauthorized,
        fr_transport::quic::Error::Cancelled,
        fr_transport::quic::Error::Malformed,
        fr_transport::quic::Error::Clock,
    ] {
        let mut value = summary();
        value.disconnect.as_mut().unwrap().transport = Err(error);
        assert!(!requested_close_allows_reopen(value));
    }
    let mut failed = summary();
    failed.failure = Some(CleanupFailure::Media);
    assert!(!requested_close_allows_reopen(failed));
    let mut absent = summary();
    absent.disconnect = None;
    assert!(!requested_close_allows_reopen(absent));
}

#[test]
fn expected_control_release_never_masks_an_adverse_original_lease_report() {
    for reason in [
        lease_revoked::Reason::LocalRevoke,
        lease_revoked::Reason::LeaseExpired,
        lease_revoked::Reason::Suspended,
        lease_revoked::Reason::PermissionLost,
        lease_revoked::Reason::ViewInvalidated,
    ] {
        let mut value = summary();
        value.control_disconnect = Some(frd::session_startup::ControlCloseOutcome {
            exchange: value.disconnect.unwrap(),
            revocation: Some(lease_revoked::Revoked {
                lease: fr_core::ids::InputLeaseId::from_raw(17),
                reason,
                cleanup: lease_revoked::CleanupStage::Released,
                effects: lease_revoked::EffectStage::ReceiptsComplete,
            }),
        });
        assert!(!requested_close_allows_reopen(value));
    }
    let mut value = summary();
    value.control_disconnect = Some(frd::session_startup::ControlCloseOutcome {
        exchange: value.disconnect.unwrap(),
        revocation: Some(lease_revoked::Revoked {
            lease: fr_core::ids::InputLeaseId::from_raw(17),
            reason: lease_revoked::Reason::ClientRequested,
            cleanup: lease_revoked::CleanupStage::Failed,
            effects: lease_revoked::EffectStage::Unknown,
        }),
    });
    assert!(!requested_close_allows_reopen(value));
}

#[test]
fn expected_own_close_cannot_rename_a_real_protocol_or_authorization_failure() {
    for error in [
        fr_transport::quic::Error::Malformed,
        fr_transport::quic::Error::Unauthorized,
        fr_transport::quic::Error::Clock,
        fr_transport::quic::Error::Cancelled,
    ] {
        let mut value = summary();
        value.disconnect.as_mut().unwrap().transport = Err(error);
        assert_eq!(
            closing_failure(value),
            Some(ObserverError::Streaming(
                frd::session_startup::StreamingViewerError::Transport(error)
            ))
        );
    }
}

#[test]
fn a_later_discovery_failure_cannot_inherit_the_previous_owners_terminal_report() {
    let mut session = Session::new(
        Configuration::new(Path::new("/usr/bin/false"), ":0", None, 71).unwrap(),
        ObserverPolicy::default(),
        Mode::Observe,
        OfferUi::default(),
    );
    let mut value = summary();
    value
        .disconnect
        .as_mut()
        .unwrap()
        .report
        .as_mut()
        .unwrap()
        .reason = closure::ClosedReason::PermissionLost;
    session.disconnect = value.disconnect;
    session.last_attempt = 1;
    session.current_attempt = 1;
    assert!(Application::terminal_failure(&session).is_some());
    Application::status(&mut session, Status::Connecting { attempt: 2 }).unwrap();
    assert_eq!(Application::terminal_failure(&session), None);
    assert_eq!(session.disconnect_outcome(), value.disconnect);
    assert_eq!(
        Application::completion_action(&mut session),
        Ok(CompletionAction::Finish)
    );
}

#[test]
fn expected_close_reasons_are_only_exempt_for_an_actual_successful_service_result() {
    let mut session = Session::new(
        Configuration::new(Path::new("/usr/bin/false"), ":0", None, 71).unwrap(),
        ObserverPolicy::default(),
        Mode::Observe,
        OfferUi::default(),
    );
    session.current_attempt = 1;
    session.last_attempt = 1;
    session.disconnect = summary().disconnect;
    assert!(Application::terminal_failure(&session).is_some());
    session.service_succeeded = true;
    assert_eq!(Application::terminal_failure(&session), None);
    session.service_succeeded = false;
    assert!(Application::terminal_failure(&session).is_some());
}
