//! Preserve authenticated host startup refusals and terminal control reports.
//! No Debug-string classification, remote text reflection, fallback role or retry.
use super::{Failure, failure};
use fr_wire::{
    negotiation,
    refusal::{Reason, Refused},
};
use frd::{
    native_connection,
    session_startup::{self, ControlledViewerError, ObserverError, StreamingViewerError},
};

fn message(message: Refused) -> Option<Failure> {
    // This projection describes startup only. Operation/effect-bearing refusals
    // need their own receipts; they must not become a claim of no external effect.
    if message.operation.is_some() || message.stage.is_some() {
        return None;
    }
    let (code, next) = match message.reason {
        Reason::InvalidMessage => (
            "host_invalid_message",
            "The host rejected the protocol request. Check compatible client and host builds; no fallback was selected.",
        ),
        Reason::UnsupportedVersion => (
            "host_unsupported_version",
            "The host rejected the protocol version. Use compatible client and host builds.",
        ),
        Reason::UnsupportedProfile => (
            "host_unsupported_profile",
            "The host rejected the transport profile. Check mutually implemented profiles; do not bypass transport qualification.",
        ),
        Reason::RequiredCapability => (
            "host_required_capability_missing",
            "The host and client lack a mutually required capability. Check compatible builds; admission and capability checks were not bypassed.",
        ),
        Reason::InvalidLimits => (
            "host_invalid_limits",
            "The host rejected the negotiated resource limits. Check client and host configuration rather than widening limits remotely.",
        ),
        Reason::InvalidSelection => (
            "host_invalid_selection",
            "The host rejected the selected configuration. Check compatible client and host capabilities.",
        ),
        Reason::PermissionDenied => (
            "host_permission_denied",
            "The host denied permission. Check its locally configured sharing scope and OS permissions; this client cannot grant access.",
        ),
        Reason::LocalApprovalDenied => (
            "host_local_approval_denied",
            "Local approval on the host was denied. A new session requires a new local approval; no approval or actions were replayed.",
        ),
        Reason::ApprovalExpired => (
            "host_approval_expired",
            "The host's approval request expired. Arrange local approval before starting a new session.",
        ),
        Reason::ControlUnavailable => (
            "host_control_unavailable",
            "The host refused control on this path. Use an explicitly view-only session or an input-capable host; observation does not grant control.",
        ),
        Reason::ResourceLimit => (
            "host_resource_limit",
            "The host refused the request because of a resource limit. Check its active sessions and local capacity.",
        ),
        Reason::InvalidState => (
            "host_invalid_state",
            "The host rejected the request in its current session state. Do not replay old session, input or attachment identifiers.",
        ),
        Reason::Expired => (
            "host_session_expired",
            "The host reported an expired request or authority. Expired sessions cannot be revived; check host policy before starting a new session.",
        ),
        Reason::HostUnavailable => (
            "host_unavailable",
            "The host reported that it is unavailable. Check its interactive session, local permissions and service status.",
        ),
        Reason::TailnetMembershipUnverifiable => (
            "host_tailnet_membership_unverifiable",
            "The host could not verify tailnet membership. Check installed Tailscale metadata and local sharing scope; do not bypass identity checks.",
        ),
    };
    Some(failure(code, next))
}

pub(super) fn session(error: session_startup::Error) -> Option<Failure> {
    match error {
        session_startup::Error::Protocol(negotiation::Error::Refused(value))
        | session_startup::Error::ClientStartup(fr_client::startup::Error::Protocol(
            negotiation::Error::Refused(value),
        )) => message(value),
        _ => None,
    }
}
pub(super) fn connection(error: native_connection::Error) -> Option<Failure> {
    match error {
        native_connection::Error::Session(error) => session(error),
        _ => None,
    }
}
pub(super) fn observation(error: ObserverError) -> Option<Failure> {
    match error {
        ObserverError::Session(error)
        | ObserverError::Streaming(StreamingViewerError::Session(error)) => session(error),
        ObserverError::Streaming(StreamingViewerError::Control(
            ControlledViewerError::LeaseRevoked(report),
        )) => Some(crate::terminal::failure(report)),
        _ => None,
    }
}
#[cfg(any(test, feature = "linux-desktop"))]
pub(super) fn reconnect(error: native_connection::reconnect::Failure) -> Option<Failure> {
    match error {
        native_connection::reconnect::Failure::Connection(error) => connection(error),
        native_connection::reconnect::Failure::Observation(error) => observation(error),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fr_wire::input_result::Stage;

    #[test]
    fn each_host_refusal_has_a_distinct_stable_code_through_every_startup_wrapper() {
        let cases = [
            (Reason::InvalidMessage, "host_invalid_message"),
            (Reason::UnsupportedVersion, "host_unsupported_version"),
            (Reason::UnsupportedProfile, "host_unsupported_profile"),
            (
                Reason::RequiredCapability,
                "host_required_capability_missing",
            ),
            (Reason::InvalidLimits, "host_invalid_limits"),
            (Reason::InvalidSelection, "host_invalid_selection"),
            (Reason::PermissionDenied, "host_permission_denied"),
            (Reason::LocalApprovalDenied, "host_local_approval_denied"),
            (Reason::ApprovalExpired, "host_approval_expired"),
            (Reason::ControlUnavailable, "host_control_unavailable"),
            (Reason::ResourceLimit, "host_resource_limit"),
            (Reason::InvalidState, "host_invalid_state"),
            (Reason::Expired, "host_session_expired"),
            (Reason::HostUnavailable, "host_unavailable"),
            (
                Reason::TailnetMembershipUnverifiable,
                "host_tailnet_membership_unverifiable",
            ),
        ];
        let mut codes = std::collections::BTreeSet::new();
        for (reason, expected) in cases {
            assert!(codes.insert(expected));
            let protocol = negotiation::Error::Refused(Refused::connection(reason));
            for error in [
                session_startup::Error::Protocol(protocol),
                session_startup::Error::ClientStartup(fr_client::startup::Error::Protocol(
                    protocol,
                )),
            ] {
                for failure in [
                    native_connection::reconnect::Failure::Connection(
                        native_connection::Error::Session(error),
                    ),
                    native_connection::reconnect::Failure::Observation(ObserverError::Session(
                        error,
                    )),
                    native_connection::reconnect::Failure::Observation(ObserverError::Streaming(
                        StreamingViewerError::Session(error),
                    )),
                ] {
                    assert!(!native_connection::reconnect::retryable(failure));
                }
                for result in [
                    session(error),
                    connection(native_connection::Error::Session(error)),
                    observation(ObserverError::Session(error)),
                    observation(ObserverError::Streaming(StreamingViewerError::Session(
                        error,
                    ))),
                    reconnect(native_connection::reconnect::Failure::Observation(
                        ObserverError::Session(error),
                    )),
                ] {
                    let failure = result.unwrap();
                    assert_eq!(failure.code, expected);
                    assert_eq!(failure.exit, 1);
                    assert_ne!(failure.next, "");
                }
            }
        }
    }

    #[test]
    fn local_timeouts_cancellation_and_cleanup_failures_are_never_host_refusals() {
        for error in [
            session_startup::Error::Cancelled,
            session_startup::Error::Expired,
            session_startup::Error::Closed,
            session_startup::Error::Protocol(negotiation::Error::Invalid),
        ] {
            assert_eq!(session(error), None);
        }
        assert_eq!(
            connection(native_connection::Error::Tailnet(
                fr_tailnet::Error::ScopeDenied
            )),
            None
        );
        assert_eq!(observation(ObserverError::Expired), None);
        for error in [
            native_connection::reconnect::Failure::Cleanup,
            native_connection::reconnect::Failure::CleanupExpired,
            native_connection::reconnect::Failure::Cancelled,
        ] {
            assert_eq!(reconnect(error), None);
        }
    }

    #[test]
    fn operation_and_effect_receipts_are_not_relabelled_as_startup_refusals() {
        for stage in [
            None,
            Some(Stage::Admitted),
            Some(Stage::SubmittedToOs),
            Some(Stage::Observed),
        ] {
            assert_eq!(
                message(Refused {
                    reason: Reason::Expired,
                    operation: Some(17),
                    stage
                }),
                None
            );
        }
    }
}

#[cfg(test)]
mod terminal_tests {
    use super::*;
    use fr_core::ids::InputLeaseId;
    use fr_wire::lease_revoked::{CleanupStage, EffectStage, Reason as EndReason, Revoked};

    #[test]
    fn authenticated_revocation_survives_the_actual_reconnect_error_wrappers() {
        let report = Revoked {
            lease: InputLeaseId::from_raw(19),
            reason: EndReason::LocalRevoke,
            cleanup: CleanupStage::Fenced,
            effects: EffectStage::Unknown,
        };
        let error = ObserverError::Streaming(StreamingViewerError::Control(
            ControlledViewerError::LeaseRevoked(report),
        ));
        let wrapped = native_connection::reconnect::Failure::Observation(error);
        let expected = crate::terminal::failure(report);
        assert_eq!(observation(error), Some(expected));
        assert_eq!(reconnect(wrapped), Some(expected));
        assert!(!native_connection::reconnect::retryable(wrapped));
    }

    #[test]
    fn local_closure_and_cleanup_errors_are_not_inferred_host_reports() {
        assert_eq!(
            observation(ObserverError::Streaming(StreamingViewerError::Control(
                ControlledViewerError::Closed,
            ))),
            None
        );
        for error in [
            native_connection::reconnect::Failure::Cleanup,
            native_connection::reconnect::Failure::CleanupExpired,
            native_connection::reconnect::Failure::Cancelled,
        ] {
            assert_eq!(reconnect(error), None);
        }
    }
}
