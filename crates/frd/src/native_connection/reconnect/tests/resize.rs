//! Resize eligibility never grants a retry without the application's native receipt.
use super::*;

#[test]
fn only_precise_resize_interruptions_reach_the_local_receipt_hook() {
    use crate::session_startup::{ControlledViewerError as C, Error as S};
    for failure in [
        Failure::Cancelled,
        Failure::Connection(ConnectionError::Cancelled),
        Failure::Observation(ObserverError::Application),
        Failure::Observation(ObserverError::Session(S::Cancelled)),
        Failure::Observation(ObserverError::Session(S::Transport(
            fr_transport::quic::Error::Cancelled,
        ))),
        Failure::Observation(ObserverError::Streaming(StreamingViewerError::Session(
            S::Transport(fr_transport::quic::Error::Cancelled),
        ))),
        Failure::Observation(ObserverError::Streaming(StreamingViewerError::Control(
            C::Session(S::Transport(fr_transport::quic::Error::Cancelled)),
        ))),
        Failure::Observation(ObserverError::Streaming(StreamingViewerError::Closed)),
        Failure::Observation(ObserverError::Streaming(StreamingViewerError::Control(
            C::Closed,
        ))),
        Failure::Observation(ObserverError::Streaming(StreamingViewerError::Control(
            C::Capture(crate::session_startup::viewer_events::Error::Closed),
        ))),
        Failure::Observation(ObserverError::Streaming(StreamingViewerError::Media(
            crate::media::Error::Worker(crate::worker::Error::WorkerRefused(
                fr_media::worker::Error::GeometryChanged,
            )),
        ))),
    ] {
        assert!(resize_interrupted(failure), "{failure:?}");
        assert!(
            !retryable(failure),
            "an interruption alone cannot retry: {failure:?}"
        );
    }
    for failure in [
        Failure::Notification,
        Failure::Cleanup,
        Failure::CleanupExpired,
        Failure::Clock,
        Failure::AttemptLimit,
        Failure::Connection(ConnectionError::Tailnet(fr_tailnet::Error::Revoked)),
        Failure::Observation(ObserverError::Expired),
        Failure::Observation(ObserverError::Order),
        Failure::Observation(ObserverError::Streaming(StreamingViewerError::Transport(
            fr_transport::quic::Error::Malformed,
        ))),
        Failure::Observation(ObserverError::Streaming(StreamingViewerError::Transport(
            fr_transport::quic::Error::Unauthorized,
        ))),
        Failure::Observation(ObserverError::Streaming(StreamingViewerError::Media(
            crate::media::Error::Worker(crate::worker::Error::PipeFailed),
        ))),
        Failure::Observation(ObserverError::Streaming(StreamingViewerError::Media(
            crate::media::Error::Worker(crate::worker::Error::WorkerRefused(
                fr_media::worker::Error::NativeFailure,
            )),
        ))),
    ] {
        assert!(!resize_interrupted(failure), "{failure:?}");
    }
}

#[test]
fn a_host_geometry_report_is_not_a_local_resize_receipt() {
    let report = fr_wire::closure::Closed {
        reason: fr_wire::closure::ClosedReason::GeometryChanged,
        cleanup: fr_wire::closure::Cleanup::Complete,
        effects: fr_wire::closure::OutstandingEffects::Known {
            pending: 0,
            uncertain: 0,
        },
    };
    let failure = Failure::Observation(ObserverError::Session(
        crate::session_startup::Error::RemoteClosed(report),
    ));
    assert!(retryable(failure));
    assert!(!resize_interrupted(failure));
}
