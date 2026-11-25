//! Local focus loss is negative input authority, never a generic retry reason.
use super::{
    Application, CallbackError, ConnectionError, Failure, ObserverError, Role, StreamingViewerError,
};

/// Cancellation wrappers that an original native input fence can produce.
/// Expiry, permission loss, host reports, callback notifications and native
/// I/O/codec failures cannot obtain retry eligibility through a focus receipt.
pub fn interrupted(failure: Failure) -> bool {
    use crate::session_startup::{ControlledViewerError as C, Error as S};
    matches!(
        failure,
        Failure::Cancelled
            | Failure::Connection(ConnectionError::Cancelled)
            | Failure::Observation(
                ObserverError::Application
                    | ObserverError::Session(
                        S::Cancelled | S::Transport(fr_transport::quic::Error::Cancelled)
                    )
                    | ObserverError::Streaming(
                        StreamingViewerError::Closed
                            | StreamingViewerError::Session(
                                S::Cancelled | S::Transport(fr_transport::quic::Error::Cancelled)
                            )
                            | StreamingViewerError::Control(
                                C::Closed
                                    | C::Capture(
                                        crate::session_startup::viewer_events::Error::Closed
                                    )
                                    | C::Session(
                                        S::Cancelled
                                            | S::Transport(fr_transport::quic::Error::Cancelled)
                                    )
                            )
                            | StreamingViewerError::Transport(fr_transport::quic::Error::Cancelled)
                            | StreamingViewerError::Media(crate::media::Error::Worker(
                                crate::worker::Error::Cancelled
                            ))
                    )
            )
    )
}

/// None means the application is not even asked for local intent. This hook
/// follows the supervisor's original cleanup/terminal and retry-budget checks.
pub(super) fn requested(
    application: &mut impl Application,
    role: Role,
    started: bool,
    failure: Failure,
) -> Option<Result<bool, CallbackError>> {
    (role == Role::RequestControl && started && interrupted(failure))
        .then(|| application.retry_viewing_after_local_stop(failure))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{session_startup::Viewer, worker::Deadline};
    use asupersync::cx::Cx;

    struct Local {
        calls: usize,
        decision: Result<bool, CallbackError>,
    }
    #[allow(clippy::unused_async_trait_impl)]
    impl Application for Local {
        type Output = ();
        async fn run(&mut self, _: u8, _: Viewer) -> Result<(), ObserverError> {
            panic!("pure retry eligibility fixture never opens a viewer")
        }
        async fn cleanup(&mut self, _: &Cx, _: Deadline) -> Result<(), CallbackError> {
            panic!("native cleanup is required separately by the supervisor")
        }
        fn status(&mut self, _: super::super::Status) -> Result<(), CallbackError> {
            Ok(())
        }
        fn retry_viewing_after_local_stop(&mut self, _: Failure) -> Result<bool, CallbackError> {
            self.calls += 1;
            self.decision
        }
    }

    #[test]
    fn cancellation_alone_never_grants_a_retry_or_reaches_an_unstarted_viewer() {
        let mut app = Local {
            calls: 0,
            decision: Ok(true),
        };
        for (role, started) in [
            (Role::Observe, false),
            (Role::Observe, true),
            (Role::RequestControl, false),
        ] {
            assert_eq!(requested(&mut app, role, started, Failure::Cancelled), None);
        }
        assert_eq!(app.calls, 0);
        assert!(!super::super::retryable(Failure::Cancelled));
        for decision in [Ok(false), Ok(true), Err(CallbackError)] {
            app.decision = decision;
            assert_eq!(
                requested(&mut app, Role::RequestControl, true, Failure::Cancelled),
                Some(decision)
            );
        }
        assert_eq!(app.calls, 3);
    }

    #[test]
    fn original_authority_native_and_protocol_failures_cannot_claim_focus_recovery() {
        use crate::session_startup::Error as S;
        let mut app = Local {
            calls: 0,
            decision: Ok(true),
        };
        for failure in [
            Failure::Cleanup,
            Failure::CleanupExpired,
            Failure::Clock,
            Failure::Notification,
            Failure::AttemptLimit,
            Failure::Connection(ConnectionError::Tailnet(fr_tailnet::Error::Expired)),
            Failure::Connection(ConnectionError::Tailnet(fr_tailnet::Error::Revoked)),
            Failure::Connection(ConnectionError::Tailnet(fr_tailnet::Error::IdentityChanged)),
            Failure::Observation(ObserverError::Expired),
            Failure::Observation(ObserverError::Session(S::Authority)),
            Failure::Observation(ObserverError::Streaming(StreamingViewerError::Application)),
            Failure::Observation(ObserverError::Streaming(StreamingViewerError::Transport(
                fr_transport::quic::Error::Expired,
            ))),
            Failure::Observation(ObserverError::Streaming(StreamingViewerError::Transport(
                fr_transport::quic::Error::Unauthorized,
            ))),
            Failure::Observation(ObserverError::Streaming(StreamingViewerError::Transport(
                fr_transport::quic::Error::Malformed,
            ))),
            Failure::Observation(ObserverError::Streaming(StreamingViewerError::Media(
                crate::media::Error::Worker(crate::worker::Error::PipeFailed),
            ))),
        ] {
            assert!(!interrupted(failure), "{failure:?}");
            assert_eq!(
                requested(&mut app, Role::RequestControl, true, failure),
                None
            );
        }
        assert_eq!(app.calls, 0);
    }
}
