//! Retry policy must retain the cause through recovery/attachment wrappers.
use super::*;
use crate::media_quic::{recovery, replacement};
use fr_media::delivery::DeliveryError;
use fr_transport::quic::Error as Transport;
use fr_wire::recovery_request::Reason;

fn observation(error: StreamingViewerError) -> Failure {
    Failure::Observation(ObserverError::Streaming(error))
}
#[test]
fn exhausted_roles_retry_only_for_an_original_reference_or_recovery_horizon() {
    for (cause, allowed) in [
        (Reason::ReferenceExpired, true),
        (Reason::RecoveryExpired, true),
        (Reason::DecodeFailed, false),
    ] {
        let failure = observation(StreamingViewerError::Recovery(
            recovery::Error::NamespaceExhausted(cause),
        ));
        assert_eq!(retryable(failure), allowed);
        // The final attempt's end is named exactly when it was a lost chain;
        // a decode failure is never relabelled as one.
        assert_eq!(
            lapse(failure),
            allowed.then_some(Lapse::VideoReferenceLost),
            "{cause:?}"
        );
    }
}
#[test]
fn recovery_and_replacement_keep_the_existing_strict_transport_retry_policy() {
    for error in [
        Transport::InvalidPolicy,
        Transport::NotEstablished,
        Transport::Alpn,
        Transport::WrongRoute,
        Transport::Malformed,
        Transport::TooLarge,
        Transport::Backpressure,
        Transport::Expired,
        Transport::Unauthorized,
        Transport::Cancelled,
        Transport::Closed,
        Transport::Native,
        Transport::Handler,
        Transport::Clock,
        Transport::Allocation,
    ] {
        let permitted = matches!(error, Transport::Native | Transport::Expired);
        for wrapped in [
            StreamingViewerError::Recovery(recovery::Error::Transport(error)),
            StreamingViewerError::Replacement(replacement::Error::Transport(error)),
        ] {
            assert_eq!(retryable(observation(wrapped)), permitted, "{wrapped:?}");
        }
    }
}
#[test]
fn ambiguous_recovery_closure_and_hung_native_work_do_not_trigger_new_sessions() {
    for error in [
        recovery::Error::Closed,
        recovery::Error::WrongBinding,
        recovery::Error::NotNegotiated,
        recovery::Error::Delivery(DeliveryError::DecodeFailed),
        // A handshake timeout may hide failed/hung native work. It is NOT the
        // original receive horizon carried by NamespaceExhausted.
        recovery::Error::Delivery(DeliveryError::RecoveryExpired),
        recovery::Error::Delivery(DeliveryError::ReferenceExpired),
    ] {
        assert!(!retryable(observation(StreamingViewerError::Recovery(
            error
        ))));
    }
    for error in [
        crate::worker::Error::Deadline,
        crate::worker::Error::Cancelled,
        crate::worker::Error::PeerClosed,
        crate::worker::Error::PipeFailed,
        crate::worker::Error::Unavailable,
        crate::worker::Error::ReapPending,
    ] {
        assert!(!retryable(observation(StreamingViewerError::Media(
            crate::media::Error::Worker(error)
        ))));
    }
    for error in [
        Failure::Cleanup,
        Failure::CleanupExpired,
        Failure::Cancelled,
    ] {
        assert!(!retryable(error));
    }
}

#[test]
fn a_typed_peer_close_is_extracted_from_every_transport_wrapper_and_never_retried() {
    use crate::session_startup::{ControlledViewerError as C, Error as S};
    use fr_transport::quic::PeerClose;
    use fr_wire::closure::ClosedReason;

    let peer = PeerClose::Session(ClosedReason::HostFailure);
    let closed = Transport::PeerClosed(peer);
    for failure in [
        Failure::Connection(ConnectionError::Session(S::Transport(closed))),
        Failure::Observation(ObserverError::Transport(closed)),
        Failure::Observation(ObserverError::Session(S::Transport(closed))),
        observation(StreamingViewerError::Session(S::Transport(closed))),
        observation(StreamingViewerError::Control(C::Session(S::Transport(
            closed,
        )))),
        observation(StreamingViewerError::Recovery(recovery::Error::Transport(
            closed,
        ))),
        observation(StreamingViewerError::Control(C::Input(
            crate::input_quic::Error::Transport(closed),
        ))),
    ] {
        assert_eq!(peer_close(failure), Some(peer), "{failure:?}");
        assert!(!retryable(failure), "{failure:?}");
        assert_eq!(lapse(failure), None, "{failure:?}");
    }
    // A bare closure carries no reason.
    assert_eq!(
        peer_close(observation(StreamingViewerError::Session(S::Transport(
            Transport::Closed
        )))),
        None
    );
}
