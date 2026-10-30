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
fn ambiguous_recovery_closure_and_native_failures_do_not_trigger_new_sessions() {
    for error in [
        recovery::Error::Closed,
        recovery::Error::WrongBinding,
        recovery::Error::NotNegotiated,
        recovery::Error::Delivery(DeliveryError::DecodeFailed),
        // These wrappers do not establish that decoder replacement exhausted
        // its own handshake. Do not guess their cause from an inner horizon.
        recovery::Error::Delivery(DeliveryError::RecoveryExpired),
        recovery::Error::Delivery(DeliveryError::ReferenceExpired),
    ] {
        assert!(!retryable(observation(StreamingViewerError::Recovery(
            error
        ))));
    }
    for error in [
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
fn only_an_established_worker_deadline_permits_fresh_viewing_after_cleanup() {
    use crate::{media, worker};
    use fr_media::worker::Error as Protocol;

    let timed_out = media::Error::Worker(worker::Error::Deadline);
    let failure = observation(StreamingViewerError::Media(timed_out));
    assert!(retryable(failure));
    assert_eq!(lapse(failure), Some(Lapse::VideoWorkerLate));

    // Initial startup and worker/IPC failures during replacement do not gain
    // retry eligibility from the replacement handshake's separate expiry case.
    for failure in [
        Failure::Observation(ObserverError::Decoder(
            media::decoder_startup::Error::Media(timed_out),
        )),
        observation(StreamingViewerError::Startup(
            media::decoder_startup::Error::Media(timed_out),
        )),
        Failure::Observation(ObserverError::Decoder(
            media::decoder_startup::Error::Expired,
        )),
        Failure::Cleanup,
        Failure::CleanupExpired,
        Failure::Cancelled,
    ] {
        assert!(!retryable(failure), "{failure:?}");
    }

    // Codec/IPC failures are not evidence of scheduling pressure. Even a
    // successfully reaped child cannot turn them into automatic reconnects.
    for error in [
        worker::Error::Cancelled,
        worker::Error::ClockRegression,
        worker::Error::PeerClosed,
        worker::Error::PipeFailed,
        worker::Error::Unavailable,
        worker::Error::ReapPending,
        worker::Error::Protocol(Protocol::Malformed),
        worker::Error::WorkerRefused(Protocol::Malformed),
    ] {
        assert!(!retryable(observation(StreamingViewerError::Media(
            media::Error::Worker(error),
        ))));
    }
    for error in [media::Error::InvalidFrame, media::Error::Poisoned] {
        assert!(!retryable(observation(StreamingViewerError::Media(error))));
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

#[test]
fn an_expired_established_decoder_replacement_can_restart_but_initial_startup_cannot() {
    use crate::media::decoder_startup::Error as Startup;

    // This is the error emitted by resume_stream, after the original observer
    // has already established its first decoder and entered continuous service.
    let failure = observation(StreamingViewerError::Startup(Startup::Expired));
    assert!(retryable(failure));
    assert_eq!(lapse(failure), Some(Lapse::VideoStartupExpired));
    assert_eq!(peer_close(failure), None);
    // Keep the exact cause for attempts_ended and the final exhausted attempt.
    assert_eq!(
        failure,
        Failure::Observation(ObserverError::Streaming(StreamingViewerError::Startup(
            Startup::Expired
        )))
    );
    for initial in [
        Failure::Observation(ObserverError::Decoder(Startup::Expired)),
        Failure::Observation(ObserverError::Expired),
        Failure::Connection(ConnectionError::Session(crate::session_startup::Error::Expired)),
    ] {
        assert!(!retryable(initial), "initial admission was retried: {initial:?}");
    }
}

#[test]
fn replacement_timeout_does_not_make_configuration_protocol_or_native_faults_retryable() {
    use crate::media::decoder_startup::Error as Startup;

    for error in [
        Startup::UnsupportedConfiguration,
        Startup::InvalidRoutes,
        Startup::ForeignConnection,
        Startup::WrongState,
        Startup::Closed,
        Startup::Allocation,
        Startup::Media(crate::media::Error::InvalidFrame),
        Startup::Media(crate::media::Error::Worker(crate::worker::Error::Deadline)),
        Startup::Media(crate::media::Error::Worker(crate::worker::Error::Cancelled)),
        Startup::Media(crate::media::Error::Worker(crate::worker::Error::ReapPending)),
        Startup::Transport(Transport::Closed),
        Startup::Transport(Transport::Unauthorized),
        Startup::Transport(Transport::Malformed),
        Startup::Transport(Transport::Cancelled),
        Startup::Transport(Transport::Handler),
    ] {
        assert!(!retryable(observation(StreamingViewerError::Startup(error))), "{error:?}");
    }
}

#[test]
fn replacement_expiry_never_reclassifies_input_expiry_or_uncertain_cleanup() {
    use crate::session_startup::ControlledViewerError as Control;
    use fr_client::input::presentation::SendLimit;

    for error in [
        observation(StreamingViewerError::Control(Control::Expired)),
        observation(StreamingViewerError::Control(Control::Unsent(SendLimit::View))),
        observation(StreamingViewerError::Control(Control::Unsent(SendLimit::Ticket))),
        observation(StreamingViewerError::Control(Control::Unsent(SendLimit::Receipt))),
        Failure::Cleanup,
        Failure::CleanupExpired,
        Failure::Cancelled,
        Failure::Notification,
    ] {
        assert!(!retryable(error), "{error:?}");
    }
}

#[test]
fn retained_host_evidence_vetoes_replacement_expiry_without_losing_known_effects() {
    use fr_wire::closure::{Cleanup, Closed, ClosedReason, OutstandingEffects};

    // Explicit evidence fixture for the same selector called AFTER production
    // cleanup. This test does not manufacture a successful native session or
    // claim to authenticate a wire report; the original owner performs that.
    struct Evidence(ObserverError);
    impl Application for Evidence {
        type Output = ();
        fn run(&mut self, _: u8, _: Viewer) -> impl Future<Output = Result<(), ObserverError>> {
            std::future::ready(Err(ObserverError::Application))
        }
        fn cleanup(&mut self, _: &Cx, _: Deadline) -> impl Future<Output = Result<(), CallbackError>> {
            std::future::ready(Ok(()))
        }
        fn status(&mut self, _: Status) -> Result<(), CallbackError> {
            Ok(())
        }
        fn terminal_failure(&self) -> Option<ObserverError> {
            Some(self.0)
        }
    }
    let expired = observation(StreamingViewerError::Startup(
        crate::media::decoder_startup::Error::Expired,
    ));
    assert!(retryable(expired));
    for reason in [ClosedReason::PermissionLost, ClosedReason::HostFailure] {
        let report = Closed {
            reason,
            cleanup: Cleanup::Unconfirmed,
            effects: OutstandingEffects::Known { pending: 3, uncertain: 1 },
        };
        let terminal = ObserverError::Streaming(StreamingViewerError::Session(
            crate::session_startup::Error::RemoteClosed(report),
        ));
        let app = Evidence(terminal);
        let retained = failed_attempt_terminal(&app, &Err(expired)).unwrap();
        assert_eq!(retained, Failure::Observation(terminal));
        assert!(!retryable(retained));
        assert_eq!(failed_attempt_terminal(&app, &Ok(())), None);
    }
}
