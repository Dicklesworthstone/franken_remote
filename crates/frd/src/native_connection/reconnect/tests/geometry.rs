//! Checked host resize is a new viewing attempt, bounded by existing cleanup.
use super::*;
use crate::session_startup::{ControlledViewerError as Control, Error as SessionError};
use fr_wire::{closure, lease_revoked};

fn closed(cleanup: closure::Cleanup) -> closure::Closed {
    closure::Closed {
        reason: closure::ClosedReason::GeometryChanged,
        cleanup,
        effects: closure::OutstandingEffects::Known {
            pending: 2,
            uncertain: 1,
        },
    }
}

#[test]
fn checked_geometry_reports_allow_fresh_viewing_but_failed_cleanup_stops() {
    for cleanup in [
        closure::Cleanup::Unconfirmed,
        closure::Cleanup::Complete,
        closure::Cleanup::Incomplete,
    ] {
        let error = SessionError::RemoteClosed(closed(cleanup));
        for failure in [
            Failure::Connection(ConnectionError::Session(error)),
            Failure::Observation(ObserverError::Session(error)),
            Failure::Observation(ObserverError::Streaming(StreamingViewerError::Session(
                error,
            ))),
            Failure::Observation(ObserverError::Streaming(StreamingViewerError::Control(
                Control::Session(error),
            ))),
        ] {
            assert_eq!(retryable(failure), cleanup != closure::Cleanup::Incomplete);
            assert_eq!(geometry_changed(failure), retryable(failure));
            assert_eq!(lapse(failure), None, "resize is not a media-loss diagnosis");
        }
    }
    for cleanup in [
        lease_revoked::CleanupStage::Fenced,
        lease_revoked::CleanupStage::Released,
        lease_revoked::CleanupStage::Failed,
    ] {
        let report = lease_revoked::Revoked {
            lease: fr_core::ids::InputLeaseId::from_raw(17),
            reason: lease_revoked::Reason::GeometryChanged,
            cleanup,
            effects: lease_revoked::EffectStage::Unknown,
        };
        let failure = Failure::Observation(ObserverError::Streaming(
            StreamingViewerError::Control(Control::LeaseRevoked(report)),
        ));
        assert_eq!(
            retryable(failure),
            cleanup != lease_revoked::CleanupStage::Failed
        );
    }
}

#[test]
fn only_exact_geometry_close_codes_permit_new_viewing_without_a_cleanup_report() {
    for reason in [
        PeerClose::Session(closure::ClosedReason::GeometryChanged),
        PeerClose::Lease(lease_revoked::Reason::GeometryChanged),
        PeerClose::Session(closure::ClosedReason::ViewInvalidated),
        PeerClose::Session(closure::ClosedReason::PermissionLost),
        PeerClose::Session(closure::ClosedReason::HostFailure),
        PeerClose::Lease(lease_revoked::Reason::LocalRevoke),
        PeerClose::Lease(lease_revoked::Reason::ViewInvalidated),
        PeerClose::Lease(lease_revoked::Reason::PermissionLost),
    ] {
        let error = fr_transport::quic::Error::PeerClosed(reason);
        let permitted = matches!(
            reason,
            PeerClose::Session(closure::ClosedReason::GeometryChanged)
                | PeerClose::Lease(lease_revoked::Reason::GeometryChanged)
        );
        for failure in [
            Failure::Observation(ObserverError::Transport(error)),
            Failure::Observation(ObserverError::Streaming(StreamingViewerError::Session(
                SessionError::Transport(error),
            ))),
            Failure::Observation(ObserverError::Streaming(StreamingViewerError::Control(
                Control::Input(crate::input_quic::Error::Transport(error)),
            ))),
        ] {
            assert_eq!(peer_close(failure), Some(reason));
            assert_eq!(retryable(failure), permitted);
        }
    }
    // A local worker value is not an authenticated host resize report.
    for error in [
        crate::worker::Error::Protocol(fr_media::worker::Error::GeometryChanged),
        crate::worker::Error::WorkerRefused(fr_media::worker::Error::GeometryChanged),
    ] {
        assert!(!retryable(Failure::Observation(ObserverError::Streaming(
            StreamingViewerError::Media(crate::media::Error::Worker(error)),
        ))));
    }
}

#[test]
fn a_retained_geometry_report_spends_the_same_retry_budget_after_each_cleanup() {
    let rt = network::runtime();
    rt.block_on(async {
        let mut app = App::new();
        let terminal = ObserverError::Session(SessionError::RemoteClosed(closed(
            closure::Cleanup::Unconfirmed,
        )));
        app.terminal = Some(terminal);
        // Exercise real supervisor ordering using the existing absent LocalAPI
        // path. The retained report is a fixture, not a claim of a live resize.
        let result = client()
            .run_observing(
                rt.request_cx_with_budget(Budget::INFINITE),
                rt.handle(),
                PeerSelector::StableId("absent"),
                Configuration::default(),
                offer(),
                quick(),
                &mut app,
            )
            .await;
        assert_eq!(result, Err(Failure::Observation(terminal)));
        assert_eq!(app.cleaned, 3);
        assert_eq!(app.terminal_reads.get(), 3);
        let retries: Vec<_> = app
            .events
            .iter()
            .filter_map(|status| match status {
                Status::Reconnecting {
                    attempt,
                    delay,
                    failure,
                } => Some((*attempt, *delay, *failure)),
                _ => None,
            })
            .collect();
        assert_eq!(
            retries,
            vec![
                (2, Duration::from_millis(10), Failure::Observation(terminal)),
                (3, Duration::from_millis(20), Failure::Observation(terminal)),
            ]
        );
        assert_eq!(
            app.terminal,
            Some(terminal),
            "effects remain available unchanged"
        );
    });
}

#[test]
fn geometry_cannot_override_uncertain_local_cleanup_or_a_local_stop() {
    let rt = network::runtime();
    rt.block_on(async {
        for (fail_cleanup, fail_cleaning_status, expected) in [
            (true, false, Failure::Cleanup),
            (false, true, Failure::Notification),
        ] {
            let mut app = App::new();
            app.terminal = Some(ObserverError::Session(SessionError::RemoteClosed(closed(
                closure::Cleanup::Complete,
            ))));
            app.fail_cleanup = fail_cleanup;
            app.fail_cleaning_status = fail_cleaning_status;
            assert_eq!(
                client()
                    .run_observing(
                        rt.request_cx_with_budget(Budget::INFINITE),
                        rt.handle(),
                        PeerSelector::StableId("absent"),
                        Configuration::default(),
                        offer(),
                        quick(),
                        &mut app,
                    )
                    .await,
                Err(expected)
            );
            assert_eq!(app.cleaned, 1);
            assert!(
                !app.events
                    .iter()
                    .any(|status| matches!(status, Status::Reconnecting { .. }))
            );
        }
    });
}

#[test]
fn a_geometry_report_cannot_relabel_an_original_protocol_or_native_failure() {
    let mut app = App::new();
    app.cleaned = 1;
    app.terminal = Some(ObserverError::Session(SessionError::RemoteClosed(closed(
        closure::Cleanup::Complete,
    ))));
    for failure in [
        Failure::Observation(ObserverError::Application),
        Failure::Observation(ObserverError::Transport(
            fr_transport::quic::Error::Malformed,
        )),
        Failure::Observation(ObserverError::Streaming(StreamingViewerError::Media(
            crate::media::Error::Worker(crate::worker::Error::PipeFailed),
        ))),
        Failure::Cancelled,
    ] {
        assert_eq!(failed_attempt_terminal(&app, &Err(failure)), None);
        assert!(!retryable(failure));
    }
    let timeout = Err(Failure::Observation(ObserverError::Streaming(
        StreamingViewerError::Transport(fr_transport::quic::Error::Expired),
    )));
    assert_eq!(
        failed_attempt_terminal(&app, &timeout),
        app.terminal.map(Failure::Observation)
    );
}
