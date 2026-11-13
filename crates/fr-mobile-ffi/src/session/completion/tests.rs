use super::*;
use fr_core::ids::InputLeaseId;
use fr_wire::{closure, lease_revoked};

fn closed(reason: closure::ClosedReason, cleanup: closure::Cleanup) -> ObserverError {
    ObserverError::Streaming(StreamingViewerError::Session(Error::RemoteClosed(
        closure::Closed {
            reason,
            cleanup,
            effects: closure::OutstandingEffects::Known {
                pending: 7,
                uncertain: 3,
            },
        },
    )))
}

fn revoked(
    reason: lease_revoked::Reason,
    cleanup: lease_revoked::CleanupStage,
) -> ObserverError {
    ObserverError::Streaming(StreamingViewerError::Control(
        ControlledViewerError::LeaseRevoked(lease_revoked::Revoked {
            lease: InputLeaseId::new(19),
            reason,
            cleanup,
            effects: lease_revoked::EffectStage::ReceiptsPending,
        }),
    ))
}

fn adverse() -> Vec<ObserverError> {
    let mut errors = [
        closure::ClosedReason::HostStopping,
        closure::ClosedReason::AuthorityExpired,
        closure::ClosedReason::PermissionLost,
        closure::ClosedReason::ViewInvalidated,
        closure::ClosedReason::ProtocolError,
        closure::ClosedReason::HostFailure,
        closure::ClosedReason::SessionReplaced,
        closure::ClosedReason::GeometryChanged,
    ]
    .map(|reason| closed(reason, closure::Cleanup::Complete))
    .to_vec();
    errors.push(closed(
        closure::ClosedReason::ClientRequested,
        closure::Cleanup::Incomplete,
    ));
    errors.extend(
        [
            lease_revoked::Reason::LocalRevoke,
            lease_revoked::Reason::LeaseExpired,
            lease_revoked::Reason::ObservationEnded,
            lease_revoked::Reason::ViewInvalidated,
            lease_revoked::Reason::PermissionLost,
            lease_revoked::Reason::HostFailure,
            lease_revoked::Reason::Suspended,
            lease_revoked::Reason::GeometryChanged,
        ]
        .map(|reason| revoked(reason, lease_revoked::CleanupStage::Released)),
    );
    errors.push(revoked(
        lease_revoked::Reason::ClientRequested,
        lease_revoked::CleanupStage::Failed,
    ));
    errors
}

#[test]
fn only_successful_original_service_and_cleanup_permit_replacement() {
    assert!(orderly_closed(Ok(()), true, true, None, None));
    assert_eq!(ending(Ok(()), true, None, None), (end::OK, None));
    assert!(!orderly_closed(Ok(()), false, true, None, None));
    assert!(!orderly_closed(Ok(()), true, false, None, None));
    for stopped in [
        LocalStop::Hidden,
        LocalStop::InputRefused,
        LocalStop::PictureChanged,
    ] {
        assert_eq!(LocalStop::from_code(stopped as u32), Some(stopped));
        assert!(!orderly_closed(Ok(()), true, true, Some(stopped), None));
    }
    for code in [0, 4, u32::MAX] {
        assert_eq!(LocalStop::from_code(code), None);
    }
    for failure in [
        Failure::Cancelled,
        Failure::Notification,
        Failure::Connection(frd::native_connection::Error::Cancelled),
        Failure::Observation(ObserverError::Application),
        Failure::Cleanup,
        Failure::CleanupExpired,
        Failure::MissingRuntime,
    ] {
        assert!(!orderly_closed(Err(failure), true, true, None, None));
    }
}

#[test]
fn a_user_visible_cancel_is_not_proof_of_orderly_service_cleanup() {
    for failure in [
        Failure::Cancelled,
        Failure::Notification,
        Failure::Connection(frd::native_connection::Error::Cancelled),
    ] {
        assert_eq!(ending(Err(failure), true, None, None).0, end::OK);
        assert!(!orderly_closed(Err(failure), true, true, None, None));
    }
    // A locally requested close report alone does not turn failed service
    // into a successful original owner, either.
    let report = closed(
        closure::ClosedReason::ClientRequested,
        closure::Cleanup::Complete,
    );
    let failure = Failure::Observation(report);
    assert_eq!(ending(Err(failure), true, None, None).0, end::OK);
    assert!(!orderly_closed(Err(failure), true, true, None, None));
}

#[test]
fn adverse_reports_survive_close_and_local_stop_races_with_all_effect_fields() {
    for terminal in adverse() {
        let detail = Some(format!("{:?}", Failure::Observation(terminal)));
        for result in [
            Ok(()),
            Err(Failure::Cancelled),
            Err(Failure::Observation(ObserverError::Application)),
        ] {
            for local_close in [false, true] {
                for stopped in [None, Some(LocalStop::Hidden), Some(LocalStop::InputRefused)] {
                    assert_eq!(
                        ending(result, local_close, stopped, Some(terminal)),
                        (end::SESSION_FAILED, detail.clone()),
                    );
                    assert!(!orderly_closed(
                        result,
                        true,
                        local_close,
                        stopped,
                        Some(terminal)
                    ));
                }
            }
        }
    }
}

#[test]
fn an_original_report_in_the_result_does_not_need_a_duplicate_terminal_slot() {
    for terminal in adverse() {
        let failure = Failure::Observation(terminal);
        for local_close in [false, true] {
            for stopped in [None, Some(LocalStop::InputRefused)] {
                assert_eq!(
                    ending(Err(failure), local_close, stopped, None),
                    (end::SESSION_FAILED, Some(format!("{failure:?}"))),
                );
                assert!(!orderly_closed(
                    Err(failure),
                    true,
                    local_close,
                    stopped,
                    None
                ));
            }
        }
    }
}

#[test]
fn cleanup_failures_remain_primary_and_retain_the_original_report() {
    for failure in [Failure::Cleanup, Failure::CleanupExpired] {
        assert_eq!(
            ending(Err(failure), true, Some(LocalStop::Hidden), None),
            (end::INTERNAL, Some(format!("{failure:?}"))),
        );
        for terminal in adverse() {
            let detail = format!("{failure:?}; terminal: {:?}", Failure::Observation(terminal));
            assert_eq!(
                ending(Err(failure), true, Some(LocalStop::Hidden), Some(terminal)),
                (end::INTERNAL, Some(detail)),
            );
            assert!(!orderly_closed(
                Err(failure),
                true,
                true,
                None,
                Some(terminal)
            ));
        }
    }
}

#[test]
fn a_local_close_does_not_convert_unreported_observer_failure_into_success() {
    let failure = Failure::Observation(ObserverError::Application);
    assert_eq!(
        ending(Err(failure), true, None, None),
        (end::SESSION_FAILED, Some(format!("{failure:?}"))),
    );
    assert_eq!(
        ending(Ok(()), false, Some(LocalStop::Hidden), None),
        (end::CONTROL_STOPPED, Some("Hidden".into())),
    );
    assert_eq!(ending(Ok(()), false, None, None), (end::HOST_CLOSED, None));
}

#[test]
fn a_later_local_close_preserves_the_first_control_stop_reason() {
    for stopped in [
        LocalStop::Hidden,
        LocalStop::InputRefused,
        LocalStop::PictureChanged,
    ] {
        for result in [Ok(()), Err(Failure::Observation(ObserverError::Application))] {
            for local_close in [false, true] {
                assert_eq!(
                    ending(result, local_close, Some(stopped), None),
                    (end::CONTROL_STOPPED, Some(format!("{stopped:?}"))),
                );
                assert!(!orderly_closed(
                    result,
                    true,
                    local_close,
                    Some(stopped),
                    None
                ));
            }
        }
    }
    assert_eq!(ending(Ok(()), true, None, None), (end::OK, None));
    assert!(orderly_closed(Ok(()), true, true, None, None));
}

#[test]
fn requested_reports_are_expected_only_when_their_cleanup_did_not_fail() {
    for cleanup in [closure::Cleanup::Unconfirmed, closure::Cleanup::Complete] {
        assert!(requested_terminal(closed(
            closure::ClosedReason::ClientRequested,
            cleanup
        )));
    }
    for cleanup in [
        lease_revoked::CleanupStage::Fenced,
        lease_revoked::CleanupStage::Released,
    ] {
        for reason in [
            lease_revoked::Reason::ClientRequested,
            lease_revoked::Reason::SessionEnded,
        ] {
            assert!(requested_terminal(revoked(reason, cleanup)));
        }
    }
    for terminal in adverse() {
        assert!(!requested_terminal(terminal));
    }
}

#[test]
fn connection_refusals_keep_their_reason_even_after_a_local_close() {
    use fr_tailnet::Error as T;
    use frd::native_connection::Error as C;
    for (failure, expected) in [
        (
            Failure::Connection(C::Tailnet(T::NameUnresolved)),
            end::NAME_UNRESOLVED,
        ),
        (
            Failure::Connection(C::Tailnet(T::OutsideTailnet)),
            end::OUTSIDE_TAILNET,
        ),
        (
            Failure::Connection(C::Tailnet(T::IdentityChanged)),
            end::IDENTITY_CHANGED,
        ),
        (
            Failure::Connection(C::Tailnet(T::NativeHandshake)),
            end::TLS_REFUSED,
        ),
        (
            Failure::Connection(C::Tailnet(T::PeerCertificateRejected)),
            end::TLS_REFUSED,
        ),
        (
            Failure::Connection(C::Tailnet(T::HandshakeTimeout)),
            end::UNREACHABLE,
        ),
        (
            Failure::Connection(C::Tailnet(T::HostBusy)),
            end::HOST_BUSY,
        ),
        (
            Failure::Connection(C::Session(Error::Denied)),
            end::HOST_REFUSED,
        ),
    ] {
        for local_close in [false, true] {
            assert_eq!(
                ending(Err(failure), local_close, None, None),
                (expected, Some(format!("{failure:?}"))),
            );
        }
    }
    assert_eq!(classify(Failure::Cancelled, false), end::CANCELLED);
}
