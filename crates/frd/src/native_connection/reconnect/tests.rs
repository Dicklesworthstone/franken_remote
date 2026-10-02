//! Retry policy and public connector failure paths. No fabricated successful dial.
use super::*;
use crate::native_connection::tests::{offer, roots};
use crate::session_startup::ObserverPolicy;
use crate::session_startup::test_network as network;
use std::{
    cell::Cell,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::Waker,
};

struct App {
    events: Vec<Status>,
    cleaned: usize,
    fail_cleanup: bool,
    stop: Option<Cx>,
}
impl App {
    fn new() -> Self {
        Self {
            events: vec![],
            cleaned: 0,
            fail_cleanup: false,
            stop: None,
        }
    }
}
#[allow(clippy::unused_async_trait_impl)]
impl Application for App {
    type Output = ();
    async fn run(&mut self, _: u8, _: Viewer) -> Result<(), ObserverError> {
        panic!("failed discovery exposed a Viewer")
    }
    async fn cleanup(&mut self, cx: &Cx, _: Deadline) -> Result<(), CallbackError> {
        assert!(cx.checkpoint().is_ok());
        self.cleaned += 1;
        if self.fail_cleanup {
            Err(CallbackError)
        } else {
            Ok(())
        }
    }
    fn status(&mut self, status: Status) -> Result<(), CallbackError> {
        self.events.push(status);
        if matches!(status, Status::Reconnecting { .. })
            && let Some(cx) = &self.stop
        {
            cx.cancel_fast(CancelKind::User);
        }
        Ok(())
    }
}
fn quick() -> Policy {
    Policy {
        max_attempts: 3,
        initial_backoff: Duration::from_millis(10),
        maximum_backoff: Duration::from_millis(20),
        cleanup_timeout: Duration::from_millis(50),
    }
}
fn client() -> Client {
    Client::new(
        fr_tailnet::LocalApi::new("/tmp/fr-reconnect-absent-local-api.sock").unwrap(),
        roots(),
        Duration::from_secs(1),
    )
    .unwrap()
}
fn control_offer() -> Offer {
    let mut offer = offer();
    offer.role = Role::RequestControl;
    offer
}
#[test]
fn backoff_is_bounded_and_attempt_counter_does_not_reset_or_wrap() {
    let p = Policy {
        max_attempts: 32,
        ..Policy::default()
    };
    p.validate().unwrap();
    let mut previous = Duration::ZERO;
    for attempt in 1..=p.max_attempts {
        let delay = p.delay(attempt);
        assert!(delay >= previous && delay <= p.maximum_backoff);
        previous = delay;
    }
    assert_eq!(p.delay(1), Duration::from_millis(250));
    assert_eq!(p.delay(4), Duration::from_secs(2));
    assert_eq!(p.delay(32), Duration::from_secs(4));
    assert_eq!(
        after(Time::from_nanos(u64::MAX), Duration::from_nanos(1)),
        Err(Failure::Clock)
    );
    for bad in [
        Policy {
            max_attempts: 0,
            ..p
        },
        Policy {
            max_attempts: 33,
            ..p
        },
        Policy {
            initial_backoff: Duration::ZERO,
            ..p
        },
        Policy {
            maximum_backoff: Duration::from_secs(31),
            ..p
        },
        Policy {
            cleanup_timeout: Duration::ZERO,
            ..p
        },
        Policy {
            cleanup_timeout: Duration::from_secs(6),
            ..p
        },
    ] {
        assert_eq!(bad.validate(), Err(Failure::InvalidPolicy));
    }
}
#[test]
fn denial_identity_protocol_codec_and_local_stop_never_retry() {
    use fr_tailnet::Error as T;
    for error in [
        T::SharedPeer,
        T::IdentityChanged,
        T::Revoked,
        T::KeyExpired,
        T::CertificateRejected,
        T::NativeHandshake,
        T::UntrustedLocalApi,
        T::LocalApiDenied,
        T::MalformedMetadata,
        T::ScopeDenied,
        T::Clock,
        T::Cancelled,
    ] {
        assert!(!retryable(Failure::Connection(ConnectionError::Tailnet(
            error
        ))));
    }
    for error in [
        fr_transport::quic::Error::Unauthorized,
        fr_transport::quic::Error::Closed,
        fr_transport::quic::Error::Cancelled,
        fr_transport::quic::Error::Malformed,
        fr_transport::quic::Error::Handler,
    ] {
        assert!(!retryable(Failure::Observation(ObserverError::Transport(
            error
        ))));
    }
    assert!(!retryable(Failure::Observation(ObserverError::Application)));
    assert!(!retryable(Failure::Observation(ObserverError::Expired)));
    assert!(retryable(Failure::Observation(ObserverError::Streaming(
        StreamingViewerError::Session(crate::session_startup::Error::Expired)
    ))));
    assert!(retryable(Failure::Observation(ObserverError::Streaming(
        StreamingViewerError::Transport(fr_transport::quic::Error::Native)
    ))));
    assert!(!retryable(Failure::Observation(ObserverError::Session(
        crate::session_startup::Error::Transport(fr_transport::quic::Error::Native)
    ))));
}
#[test]
fn only_exhausted_media_horizons_retry_after_streaming_has_started() {
    use fr_media::delivery::DeliveryError as D;
    for error in [D::ReferenceExpired, D::RecoveryExpired] {
        // The network and decoder completion paths wrap the same receiving
        // failure differently. Both require a new authenticated observation,
        // never continuation of a broken reference chain.
        for streaming in [
            StreamingViewerError::Delivery(error),
            StreamingViewerError::Media(crate::media::Error::Receiver(error)),
        ] {
            assert!(retryable(Failure::Observation(ObserverError::Streaming(
                streaming
            ))));
        }
        // A startup failure is not evidence that an established observation
        // lost a reference. Do not turn failed negotiation into a retry loop.
        assert!(!retryable(Failure::Observation(ObserverError::Streaming(
            StreamingViewerError::Startup(crate::media::decoder_startup::Error::Media(
                crate::media::Error::Receiver(error)
            ))
        ))));
    }
}

#[test]
fn media_faults_other_than_expired_horizons_remain_terminal() {
    use fr_media::delivery::DeliveryError as D;
    for error in [
        D::InvalidPolicy,
        D::WrongState,
        D::ResourceLimit,
        D::AllocationFailed,
        D::ConflictingPicture,
        D::NoncontiguousRecovery,
        D::ClockRegression,
        D::ClockOverflow,
        D::StaleGeneration,
        D::DecodeFailed,
        D::DecodeMismatch,
        D::Wire(fr_wire::WireError::Truncated),
    ] {
        for streaming in [
            StreamingViewerError::Delivery(error),
            StreamingViewerError::Media(crate::media::Error::Receiver(error)),
        ] {
            assert!(!retryable(Failure::Observation(ObserverError::Streaming(
                streaming
            ))));
        }
    }
}

#[test]
fn established_streaming_transport_failures_have_the_same_policy_in_every_owner() {
    use crate::session_startup::{ControlledViewerError as C, Error as S};
    use fr_transport::quic::Error as T;
    for error in [
        T::Native,
        T::Expired,
        T::Unauthorized,
        T::Closed,
        T::Cancelled,
        T::Malformed,
        T::Handler,
        T::WrongRoute,
        T::Backpressure,
        T::Clock,
        T::Allocation,
    ] {
        let expected = matches!(error, T::Native | T::Expired);
        // An active controller, decoder route, or advisory report must not
        // hide actual transport loss from the same fresh-session supervisor.
        // Conversely, wrapping a denial or local stop must never enable retry.
        for streaming in [
            StreamingViewerError::Transport(error),
            StreamingViewerError::Routes(crate::media_quic::Error::Transport(error)),
            StreamingViewerError::Session(S::Transport(error)),
            StreamingViewerError::Control(C::Session(S::Transport(error))),
            StreamingViewerError::Control(C::Media(crate::media_quic::Error::Transport(error))),
            StreamingViewerError::Control(C::Input(crate::input_quic::Error::Transport(error))),
            StreamingViewerError::Feedback(crate::media::ReceiverFeedbackError::Transport(error)),
            StreamingViewerError::PresentedState(crate::media::PresentedStateError::Transport(
                error,
            )),
        ] {
            assert_eq!(
                retryable(Failure::Observation(ObserverError::Streaming(streaming))),
                expected,
                "incorrect retry policy for {streaming:?}"
            );
        }
    }
}

#[test]
fn only_transport_deadlines_and_stale_views_are_named_lapses() {
    use crate::session_startup::{ControlledViewerError as C, Error as S};
    use fr_client::input::presentation::Error as P;
    use fr_media::freshness::Error as F;
    use fr_transport::quic::Error as T;
    let observed = |error| Failure::Observation(ObserverError::Streaming(error));
    for error in [T::Expired, T::Native, T::Closed, T::Unauthorized] {
        let expected = (error == T::Expired).then_some(Lapse::TransportDeadline);
        for failure in [
            Failure::Connection(ConnectionError::Session(S::Transport(error))),
            Failure::Observation(ObserverError::Transport(error)),
            Failure::Observation(ObserverError::Session(S::Transport(error))),
            observed(StreamingViewerError::Transport(error)),
            observed(StreamingViewerError::Session(S::Transport(error))),
            observed(StreamingViewerError::Control(C::Session(S::Transport(
                error,
            )))),
            observed(StreamingViewerError::Control(C::Input(
                crate::input_quic::Error::Transport(error),
            ))),
            observed(StreamingViewerError::Recovery(
                crate::media_quic::recovery::Error::Transport(error),
            )),
            observed(StreamingViewerError::PresentedState(
                crate::media::PresentedStateError::Transport(error),
            )),
            observed(StreamingViewerError::Control(C::Clock(
                crate::media::clock::Error::Transport(error),
            ))),
        ] {
            assert_eq!(lapse(failure), expected, "{failure:?}");
        }
    }
    for error in [
        F::SourceStale,
        F::SourceUnknown,
        F::QueueExpired,
        F::NotSubmitted,
        F::ClockExpired,
        F::StaleBinding,
    ] {
        let expected =
            matches!(error, F::SourceStale | F::SourceUnknown).then_some(Lapse::ViewStale);
        let failure = observed(StreamingViewerError::Control(C::View(P::Media(error))));
        assert_eq!(lapse(failure), expected, "{error:?}");
        // Naming the lapse never turns a stale view into a reconnect.
        assert!(!retryable(failure));
        // A view-only acquisition fault is not an input stop on a stale view.
        assert_eq!(
            lapse(observed(StreamingViewerError::Freshness(error))),
            None
        );
    }
    // A decoder start/restart handshake that ran out of time is named; its
    // other failures keep the generic path.
    {
        use crate::media::decoder_startup::Error as D;
        for error in [D::Expired, D::WrongState, D::Closed, D::InvalidRoutes] {
            let expected = (error == D::Expired).then_some(Lapse::VideoStartupExpired);
            assert_eq!(
                lapse(observed(StreamingViewerError::Startup(error))),
                expected,
                "{error:?}"
            );
        }
    }
    // The input client can meet the same stale view first and stop itself.
    for reason in [
        fr_client::input::StopReason::ViewStale,
        fr_client::input::StopReason::ViewChanged,
        fr_client::input::StopReason::ReceiptTimeout,
        fr_client::input::StopReason::FocusLost,
    ] {
        let failure = observed(StreamingViewerError::Control(C::View(P::Input(
            fr_client::input::Error::Stopped(reason),
        ))));
        let expected =
            (reason == fr_client::input::StopReason::ViewStale).then_some(Lapse::ViewStale);
        assert_eq!(lapse(failure), expected, "{reason:?}");
        assert!(!retryable(failure));
    }
    for error in [
        S::Expired,
        S::ClientRenewal(fr_client::authority::Error::Expired),
    ] {
        for failure in [
            Failure::Observation(ObserverError::Session(error)),
            observed(StreamingViewerError::Session(error)),
            observed(StreamingViewerError::Control(C::Session(error))),
        ] {
            assert_eq!(lapse(failure), Some(Lapse::HostNotHeard), "{failure:?}");
        }
        // Startup expiry (before any session was established) is not named.
        assert_eq!(
            lapse(Failure::Connection(ConnectionError::Session(error))),
            None
        );
    }
}

#[test]
fn a_lapsed_control_lease_is_named_but_capture_faults_are_not() {
    use crate::session_startup::ControlledViewerError as C;
    use fr_client::input::{Error as I, StopReason};
    use fr_client::{authority::Error as A, input::presentation::Error as P};
    let observed = |error| Failure::Observation(ObserverError::Streaming(error));
    // No capture fault is a named lapse: an aged event is dropped as obsolete
    // input, never an end (fr-1r40); the other faults keep the generic path.
    {
        use crate::session_startup::viewer_events::Error as E;
        for error in [E::Closed, E::Clock, E::Overflow, E::Unavailable] {
            let failure = observed(StreamingViewerError::Control(C::Capture(error)));
            assert_eq!(lapse(failure), None, "{error:?}");
            assert!(!retryable(failure));
        }
    }
    // A clock probe whose reply missed its deadline is that exchange's
    // delivery deadline; the exchange's other faults are not.
    for (error, expected) in [
        (
            fr_client::clock::Error::Expired,
            Some(Lapse::TransportDeadline),
        ),
        (fr_client::clock::Error::Stopped, None),
        (fr_client::clock::Error::UnexpectedReply, None),
    ] {
        let failure = observed(StreamingViewerError::Control(C::Clock(
            crate::media::clock::Error::Client(error),
        )));
        assert_eq!(lapse(failure), expected, "{error:?}");
        assert!(!retryable(failure));
    }
    // A lapsed control lease is named both as the expiry the client met first
    // and as the stop it reports afterwards; a malformed or stale challenge is
    // a protocol fault, not a lapse.
    for (error, expected) in [
        (
            I::Stopped(StopReason::InvalidControl),
            Some(Lapse::ControlRenewalFailed),
        ),
        (I::Control(A::Expired), Some(Lapse::ControlRenewalFailed)),
        (I::Control(A::StaleChallenge), None),
        (I::Control(A::WrongScope), None),
        (I::Stopped(StopReason::InvalidTicket), None),
    ] {
        let failure = observed(StreamingViewerError::Control(C::View(P::Input(error))));
        assert_eq!(lapse(failure), expected, "{error:?}");
        assert!(!retryable(failure));
    }
}

#[test]
fn a_lost_reference_is_named_but_decode_failure_and_local_ends_are_not() {
    use crate::session_startup::{ControlledViewerError as C, Error as S};
    let observed = |error| Failure::Observation(ObserverError::Streaming(error));
    for error in [
        fr_media::delivery::DeliveryError::ReferenceExpired,
        fr_media::delivery::DeliveryError::RecoveryExpired,
    ] {
        for failure in [
            observed(StreamingViewerError::Delivery(error)),
            observed(StreamingViewerError::Media(crate::media::Error::Receiver(
                error,
            ))),
        ] {
            assert_eq!(lapse(failure), Some(Lapse::VideoReferenceLost));
        }
    }
    // A decode failure is not a lost reference.
    assert_eq!(
        lapse(observed(StreamingViewerError::Delivery(
            fr_media::delivery::DeliveryError::DecodeFailed
        ))),
        None
    );
    // The controlled viewer's own Expired is a local send/response deadline.
    assert_eq!(
        lapse(observed(StreamingViewerError::Control(C::Expired))),
        Some(Lapse::TransportDeadline)
    );
    for failure in [
        observed(StreamingViewerError::Control(C::Session(S::Authority))),
        observed(StreamingViewerError::Control(C::Session(S::ClientRenewal(
            fr_client::authority::Error::Stopped,
        )))),
        Failure::Observation(ObserverError::Expired),
        Failure::Cancelled,
        Failure::Cleanup,
        Failure::CleanupExpired,
    ] {
        assert_eq!(lapse(failure), None, "{failure:?}");
    }
}

#[test]
fn observation_expiry_reconnects_but_input_expiry_and_revocation_do_not() {
    use crate::session_startup::{ControlledViewerError as C, Error as S};
    for error in [
        S::Expired,
        S::ClientRenewal(fr_client::authority::Error::Expired),
    ] {
        for streaming in [
            StreamingViewerError::Session(error),
            StreamingViewerError::Control(C::Session(error)),
        ] {
            assert!(retryable(Failure::Observation(ObserverError::Streaming(
                streaming
            ))));
        }
    }
    for error in [
        C::Expired,
        C::Closed,
        C::WrongBinding,
        C::ClockNotReady,
        C::ControlNotNegotiated,
        C::Backpressure,
        C::Session(S::Authority),
        C::Session(S::Denied),
        C::Session(S::Closed),
        C::Session(S::Cancelled),
        C::Session(S::ClientRenewal(fr_client::authority::Error::Stopped)),
        C::Input(crate::input_quic::Error::TicketExpired),
        C::Input(crate::input_quic::Error::WrongConnection),
        C::Input(crate::input_quic::Error::Closed),
    ] {
        assert!(
            !retryable(Failure::Observation(ObserverError::Streaming(
                StreamingViewerError::Control(error)
            ))),
            "input or authority failure retried: {error:?}"
        );
    }
}

#[test]
fn public_connector_retries_unavailable_daemon_only_after_cleanup_and_fixed_waits() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let start = cx.now();
        let mut app = App::new();
        let result = client()
            .run_observing(
                cx.clone(),
                rt.handle(),
                PeerSelector::StableId("absent"),
                Configuration::default(),
                offer(),
                quick(),
                &mut app,
            )
            .await;
        assert_eq!(
            result,
            Err(Failure::Connection(ConnectionError::Tailnet(
                fr_tailnet::Error::LocalApiUnavailable
            )))
        );
        assert!(cx.checkpoint().is_err());
        assert_eq!(app.cleaned, 3);
        assert_eq!(app.events.len(), 9);
        for (index, attempt) in [(0, 1), (3, 2), (6, 3)] {
            assert_eq!(app.events[index], Status::Connecting { attempt });
            assert!(
                matches!(app.events[index+1], Status::Cleaning { attempt: a, .. } if a == attempt)
            );
        }
        assert!(matches!(
            app.events.last(),
            Some(Status::Stopped { attempt: 3, .. })
        ));
        assert!(
            rt.handle()
                .request_cx_with_budget(Budget::INFINITE)
                .now()
                .as_nanos()
                >= start.as_nanos() + 30_000_000
        );
    });
}
#[test]
fn role_specific_supervisors_refuse_before_discovery_or_callbacks() {
    let rt = network::runtime();
    rt.block_on(async {
        let cx = rt.request_cx_with_budget(Budget::INFINITE);
        let mut app = App::new();
        assert_eq!(
            client()
                .run_observing(
                    cx.clone(),
                    rt.handle(),
                    PeerSelector::StableId("absent"),
                    Configuration::default(),
                    control_offer(),
                    quick(),
                    &mut app,
                )
                .await,
            Err(Failure::ObservationOnly)
        );
        assert!(app.events.is_empty() && app.cleaned == 0);
        assert!(cx.checkpoint().is_err());

        let cx = rt.request_cx_with_budget(Budget::INFINITE);
        let mut app = App::new();
        assert_eq!(
            client()
                .run_control_capable(
                    cx.clone(),
                    rt.handle(),
                    PeerSelector::StableId("absent"),
                    Configuration::default(),
                    offer(),
                    quick(),
                    &mut app,
                )
                .await,
            Err(Failure::ControlCapableOnly)
        );
        assert!(app.events.is_empty() && app.cleaned == 0);
        assert!(cx.checkpoint().is_err());

        let cx = rt.request_cx_with_budget(Budget::INFINITE);
        let mut app = App::new();
        let mut invalid = quick();
        invalid.max_attempts = 0;
        assert_eq!(
            client()
                .run_observing(
                    cx.clone(),
                    rt.handle(),
                    PeerSelector::StableId("absent"),
                    Configuration::default(),
                    offer(),
                    invalid,
                    &mut app,
                )
                .await,
            Err(Failure::InvalidPolicy)
        );
        assert!(app.events.is_empty() && app.cleaned == 0);
        assert!(cx.checkpoint().is_err());
    });
}
#[test]
fn control_capable_reconnect_uses_fresh_attempts_and_cleanup_without_replaying_control() {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut app = App::new();
        let result = client()
            .run_control_capable(
                cx.clone(),
                rt.handle(),
                PeerSelector::StableId("absent"),
                Configuration::default(),
                control_offer(),
                quick(),
                &mut app,
            )
            .await;
        assert_eq!(
            result,
            Err(Failure::Connection(ConnectionError::Tailnet(
                fr_tailnet::Error::LocalApiUnavailable
            )))
        );
        assert_eq!(app.cleaned, 3);
        assert_eq!(
            app.events
                .iter()
                .filter(|event| matches!(event, Status::Connecting { .. }))
                .count(),
            3
        );
        assert_eq!(
            app.events
                .iter()
                .filter(|event| matches!(event, Status::Authenticated { .. }))
                .count(),
            0,
            "unavailable identity never exposed a Viewer or control-capable session"
        );
        assert!(cx.checkpoint().is_err());
    });
}
#[test]
fn native_control_adapter_constructs_without_starting_a_session_or_request() {
    let _app = native_control_view(
        ObserverPolicy::default(),
        fr_media::freshness::ClockPolicy::default(),
        fr_client::input::Policy::default(),
        1,
        fr_core::input_submission::Capabilities::default(),
        |_| -> Result<crate::worker::Launch, CallbackError> { panic!("unpolled launch") },
        |_, _| Ok(None),
        |_, _| Ok(()),
        |_, _, _| Ok(()),
        |_, _| {},
        |_| Ok(()),
    );
}

#[test]
fn cleanup_failure_and_explicit_stop_are_terminal_even_for_transient_errors() {
    let rt = network::runtime();
    rt.block_on(async {
        for fail_cleanup in [true, false] {
            let cx = rt.request_cx_with_budget(Budget::INFINITE);
            let mut app = App::new();
            app.fail_cleanup = fail_cleanup;
            if !fail_cleanup {
                app.stop = Some(cx.clone());
            }
            let outcome = client()
                .run_observing(
                    cx,
                    rt.handle(),
                    PeerSelector::StableId("absent"),
                    Configuration::default(),
                    offer(),
                    quick(),
                    &mut app,
                )
                .await;
            assert_eq!(
                outcome,
                Err(if fail_cleanup {
                    Failure::Cleanup
                } else {
                    Failure::Cancelled
                })
            );
            assert_eq!(app.cleaned, 1);
            assert_eq!(
                app.events
                    .iter()
                    .filter(|s| matches!(s, Status::Connecting { .. }))
                    .count(),
                1
            );
        }
    });
}
#[test]
fn dropping_unpolled_supervisor_cancels_only_its_own_context() {
    let rt = network::runtime();
    let own = rt.request_cx_with_budget(Budget::INFINITE);
    let other = rt.request_cx_with_budget(Budget::INFINITE);
    let mut client = client();
    let mut app = App::new();
    drop(client.run_observing(
        own.clone(),
        rt.handle(),
        PeerSelector::StableId("absent"),
        Configuration::default(),
        offer(),
        quick(),
        &mut app,
    ));
    assert!(own.checkpoint().is_err());
    assert!(other.checkpoint().is_ok());
    assert_eq!(app.events.len(), 0);
}
struct Pending {
    cx: Cx,
    dropped: Arc<AtomicBool>,
}
impl Future for Pending {
    type Output = Result<(), Failure>;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Pending
    }
}
impl Drop for Pending {
    fn drop(&mut self) {
        assert!(self.cx.checkpoint().is_err());
        self.dropped.store(true, Ordering::Release);
    }
}
#[test]
fn supervisor_stop_fences_pending_attempt_before_dropping_its_work() {
    let rt = network::runtime();
    let outer = rt.request_cx_with_budget(Budget::INFINITE);
    let child = rt.request_cx_with_budget(Budget::INFINITE);
    let dropped = Arc::new(AtomicBool::new(false));
    rt.block_on(async {
        let mut clock = Clock::new(&outer).unwrap();
        let mut task = Box::pin(guarded(
            &outer,
            &mut clock,
            Owned {
                cx: child.clone(),
                inner: Box::pin(Pending {
                    cx: child.clone(),
                    dropped: dropped.clone(),
                }),
            },
        ));
        assert!(
            task.as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        outer.cancel_fast(CancelKind::User);
        assert_eq!(task.await, Err(Failure::Cancelled));
        assert!(dropped.load(Ordering::Acquire));
    });
}
struct StuckCleanup {
    seen: Cell<bool>,
    cx: Option<Cx>,
}
#[allow(clippy::unused_async_trait_impl)]
impl Application for StuckCleanup {
    type Output = ();
    async fn run(&mut self, _: u8, _: Viewer) -> Result<(), ObserverError> {
        unreachable!()
    }
    async fn cleanup(&mut self, cx: &Cx, _: Deadline) -> Result<(), CallbackError> {
        self.cx = Some(cx.clone());
        self.seen.set(true);
        std::future::pending().await
    }
    fn status(&mut self, _: Status) -> Result<(), CallbackError> {
        Ok(())
    }
}
#[test]
fn stalled_cleanup_has_exclusive_deadline_and_cannot_leave_a_live_context() {
    let rt = network::runtime();
    rt.block_on(async {
        let cx = rt.request_cx_with_budget(Budget::INFINITE);
        let mut app = StuckCleanup {
            seen: Cell::new(false),
            cx: None,
        };
        assert_eq!(
            cleanup(cx.clone(), Duration::from_millis(10), &mut app).await,
            Err(Failure::CleanupExpired)
        );
        assert!(app.seen.get());
        assert!(cx.checkpoint().is_err());
    });
}
