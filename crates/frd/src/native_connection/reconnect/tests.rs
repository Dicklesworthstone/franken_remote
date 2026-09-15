//! Retry policy and public connector failure paths. No fabricated successful dial.
use super::*;
use crate::native_connection::tests::{offer, roots};
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
fn control_role_and_invalid_policy_refuse_before_discovery_or_callbacks() {
    let rt = network::runtime();
    rt.block_on(async {
        for control in [false, true] {
            let cx = rt.request_cx_with_budget(Budget::INFINITE);
            let mut app = App::new();
            let mut selected = offer();
            let mut policy = quick();
            if control {
                selected.role = Role::RequestControl;
            } else {
                policy.max_attempts = 0;
            }
            assert_eq!(
                client()
                    .run_observing(
                        cx.clone(),
                        rt.handle(),
                        PeerSelector::StableId("absent"),
                        Configuration::default(),
                        selected,
                        policy,
                        &mut app
                    )
                    .await,
                Err(if control {
                    Failure::ObservationOnly
                } else {
                    Failure::InvalidPolicy
                })
            );
            assert_eq!(app.events.len(), 0);
            assert_eq!(app.cleaned, 0);
            assert!(cx.checkpoint().is_err());
        }
    });
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
