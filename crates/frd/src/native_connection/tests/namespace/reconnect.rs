//! Public reconnect supervisor against an actual unavailable UDP endpoint, then
//! real TLS/session startup. Metadata and CA are fixtures; no successful dial,
//! native desktop, input grant or live-tailnet qualification is fabricated.
use super::*;
use crate::native_connection::reconnect::{self, Application, CallbackError, Failure, Status};
use crate::session_startup::ObserverError;
use crate::worker::Deadline;
use asupersync::types::CancelKind;
use std::net::UdpSocket;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AfterTimeout {
    StartHost,
    StayUnavailable,
    FailCleanup,
    Cancel,
    ChangeIdentity,
}
struct RetryApp {
    mode: AfterTimeout,
    supervisor: Cx,
    changed: Arc<AtomicBool>,
    restart: Arc<AtomicBool>,
    listening: Arc<AtomicBool>,
    opened: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    events: Vec<Status>,
    cleanups: u8,
    viewers: u8,
}
impl RetryApp {
    fn new(mode: AfterTimeout, supervisor: Cx, changed: Arc<AtomicBool>) -> Self {
        Self {
            mode,
            supervisor,
            changed,
            restart: Arc::new(AtomicBool::new(false)),
            listening: Arc::new(AtomicBool::new(false)),
            opened: Arc::new(AtomicBool::new(false)),
            done: Arc::new(AtomicBool::new(false)),
            events: vec![],
            cleanups: 0,
            viewers: 0,
        }
    }
}
impl Application for RetryApp {
    type Output = ();
    async fn run(&mut self, attempt: u8, mut viewer: Viewer) -> Result<(), ObserverError> {
        assert_eq!(self.mode, AfterTimeout::StartHost);
        assert_eq!(attempt, 2, "the timed-out handshake exposed a Viewer");
        assert_eq!(self.cleanups, 1);
        self.viewers += 1;
        while !viewer.is_complete() {
            viewer
                .drive(Duration::from_millis(5))
                .await
                .map_err(ObserverError::Session)?;
        }
        let mut running = viewer.finish().map_err(ObserverError::Session)?;
        // Deliver BindingAccepted and observe the real host's startup completion
        // before ending. A TLS connection alone is not a SessionOpened result.
        while !self.opened.load(Ordering::Acquire) {
            running
                .drive(Duration::from_millis(5), |_, _| Err(()))
                .await
                .map_err(ObserverError::Session)?;
        }
        running.check().map_err(ObserverError::Session)?;
        self.done.store(true, Ordering::Release);
        Ok(())
    }
    async fn cleanup(&mut self, cx: &Cx, deadline: Deadline) -> Result<(), CallbackError> {
        assert!(
            cx.checkpoint().is_ok(),
            "cleanup needs its independent context"
        );
        self.cleanups += 1;
        if self.mode == AfterTimeout::FailCleanup {
            return Err(CallbackError);
        }
        if self.mode == AfterTimeout::StartHost && self.cleanups == 1 {
            // First retire the blackhole socket and its queued old Initials.
            // Confirm the replacement listener before the supervisor's backoff
            // ends, so this test cannot accidentally race the second connect.
            self.restart.store(true, Ordering::Release);
            while !self.listening.load(Ordering::Acquire) {
                cx.checkpoint().map_err(|_| CallbackError)?;
                if cx.now() >= deadline.time() {
                    return Err(CallbackError);
                }
                sleep(cx.now(), Duration::from_millis(1)).await;
            }
        }
        Ok(())
    }
    fn status(&mut self, status: Status) -> Result<(), CallbackError> {
        if let Status::Connecting { attempt } = status {
            assert_eq!(self.cleanups + 1, attempt, "retry preceded cleanup");
        }
        if let Status::Reconnecting {
            attempt, failure, ..
        } = status
        {
            assert_eq!(attempt, 2);
            assert_eq!(failure, handshake_timeout());
            assert_eq!(self.cleanups, 1);
            match self.mode {
                AfterTimeout::Cancel => self.supervisor.cancel_fast(CancelKind::User),
                AfterTimeout::ChangeIdentity => self.changed.store(true, Ordering::Release),
                _ => {}
            }
        }
        self.events.push(status);
        Ok(())
    }
}
fn handshake_timeout() -> Failure {
    Failure::Connection(Error::Tailnet(fr_tailnet::Error::HandshakeTimeout))
}
fn retry_policy() -> reconnect::Policy {
    reconnect::Policy {
        max_attempts: 2,
        initial_backoff: Duration::from_millis(20),
        maximum_backoff: Duration::from_millis(20),
        cleanup_timeout: Duration::from_secs(2),
    }
}
fn blackhole() -> UdpSocket {
    // A bound but unread socket produces silence, not an ICMP/local-I/O error.
    UdpSocket::bind("100.64.0.2:0").unwrap()
}
async fn opened_host(cx: &Cx, endpoint: QuicUdpEndpoint, opened: &AtomicBool, done: &AtomicBool) {
    let native = accept(cx, endpoint).await;
    let mut host = crate::session_startup::connection_test_host(
        cx.clone(),
        native,
        offer(),
        Policy::default(),
    );
    let mut approved = false;
    while !host.is_complete() {
        host.drive(Duration::from_millis(5)).await.unwrap();
        if !approved && let Some(approval) = host.approval() {
            approval.decide(true).unwrap();
            approved = true;
        }
    }
    assert!(approved);
    let mut running = host.finish().unwrap().into_running().unwrap();
    opened.store(true, Ordering::Release);
    let mut nonce = 0;
    while !done.load(Ordering::Acquire) {
        let result = running
            .drive(
                Duration::from_millis(5),
                || {
                    nonce += 1;
                    Ok(nonce)
                },
                |_, _| Err(()),
            )
            .await;
        if result.is_err() {
            assert!(done.load(Ordering::Acquire));
            break;
        }
    }
    running.close();
}

#[test]
#[ignore = "explicit isolated namespace: scripts/verify-native-session.sh"]
fn timed_out_native_handshake_retries_then_opens_the_same_verified_peer() {
    let api = Api::new();
    let runtime = network::runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let hc = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let silent = blackhole();
        let address = silent.local_addr().unwrap();
        let mut client = Client::new(api.client.clone(), roots(), Duration::from_secs(4)).unwrap();
        let mut app = RetryApp::new(AfterTimeout::StartHost, cx.clone(), api.changed.clone());
        let (restart, listening, opened, done) = (
            app.restart.clone(),
            app.listening.clone(),
            app.opened.clone(),
            app.done.clone(),
        );
        let client_run = async {
            let result = client
                .run_observing(
                    cx.clone(),
                    runtime.handle(),
                    PeerSelector::StableId("n-peer"),
                    Configuration {
                        port: address.port(),
                        ..Configuration::default()
                    },
                    offer(),
                    retry_policy(),
                    &mut app,
                )
                .await;
            done.store(true, Ordering::Release);
            result
        };
        let delayed_host = async {
            while !restart.load(Ordering::Acquire) {
                assert!(!done.load(Ordering::Acquire), "no first cleanup");
                sleep(hc.now(), Duration::from_millis(1)).await;
            }
            drop(silent);
            let endpoint = QuicUdpEndpoint::bind(
                &hc,
                address,
                QuicUdpEndpointConfig {
                    max_packet_size: 1200,
                    max_batch_size: 16,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            listening.store(true, Ordering::Release);
            let mut serving = pin!(opened_host(&hc, endpoint, &opened, &done));
            poll_fn(|task| {
                if done.load(Ordering::Acquire) {
                    return Poll::Ready(());
                }
                serving.as_mut().poll(task)
            })
            .await;
        };
        let (result, ()) = Box::pin(timeout(
            hc.now(),
            Duration::from_secs(16),
            network::both(client_run, delayed_host),
        ))
        .await
        .unwrap();
        assert_eq!(result, Ok(()));
        assert!(opened.load(Ordering::Acquire));
        assert_eq!(app.cleanups, 2);
        assert_eq!(app.viewers, 1);
        assert_eq!(
            app.events,
            [
                Status::Connecting { attempt: 1 },
                Status::Cleaning {
                    attempt: 1,
                    failure: Some(handshake_timeout())
                },
                Status::Reconnecting {
                    attempt: 2,
                    delay: Duration::from_millis(20),
                    failure: handshake_timeout(),
                },
                Status::Connecting { attempt: 2 },
                Status::Authenticated { attempt: 2 },
                Status::Cleaning {
                    attempt: 2,
                    failure: None
                },
                Status::Completed { attempt: 2 },
            ]
        );
        assert!(cx.checkpoint().is_err());
    });
}

async fn unavailable(
    runtime: &asupersync::runtime::Runtime,
    mode: AfterTimeout,
    control_capable: bool,
) {
    let api = Api::new();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let timeout_cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let silent = blackhole();
    let mut client = Client::new(api.client.clone(), roots(), Duration::from_millis(500)).unwrap();
    let mut app = RetryApp::new(mode, cx.clone(), api.changed.clone());
    let cfg = Configuration {
        port: silent.local_addr().unwrap().port(),
        ..Configuration::default()
    };
    let mut selection = offer();
    if control_capable {
        selection.role = Role::RequestControl;
    }
    let run = async {
        if control_capable {
            client
                .run_control_capable(
                    cx.clone(),
                    runtime.handle(),
                    PeerSelector::StableId("n-peer"),
                    cfg,
                    selection,
                    retry_policy(),
                    &mut app,
                )
                .await
        } else {
            client
                .run_observing(
                    cx.clone(),
                    runtime.handle(),
                    PeerSelector::StableId("n-peer"),
                    cfg,
                    selection,
                    retry_policy(),
                    &mut app,
                )
                .await
        }
    };
    let result = Box::pin(timeout(timeout_cx.now(), Duration::from_secs(5), run))
        .await
        .unwrap();
    let (expected, attempts) = match mode {
        AfterTimeout::StayUnavailable => (handshake_timeout(), 2),
        AfterTimeout::FailCleanup => (Failure::Cleanup, 1),
        AfterTimeout::Cancel => (Failure::Cancelled, 1),
        AfterTimeout::ChangeIdentity => (
            Failure::Connection(Error::Tailnet(fr_tailnet::Error::IdentityChanged)),
            2,
        ),
        AfterTimeout::StartHost => panic!("unavailable fixture cannot open a host"),
    };
    assert_eq!(result, Err(expected), "{mode:?}");
    assert_eq!(app.cleanups, attempts, "{mode:?}");
    assert_eq!(app.viewers, 0);
    assert_eq!(
        app.events
            .iter()
            .filter(|e| matches!(e, Status::Connecting { .. }))
            .count(),
        usize::from(attempts)
    );
    assert!(
        !app.events
            .iter()
            .any(|e| matches!(e, Status::Authenticated { .. }))
    );
    assert_eq!(
        app.events[1],
        Status::Cleaning {
            attempt: 1,
            failure: Some(handshake_timeout())
        }
    );
    assert_eq!(
        app.events.last(),
        Some(&Status::Stopped {
            attempt: attempts,
            failure: expected
        })
    );
    assert!(cx.checkpoint().is_err());
    assert!(api.calls.load(Ordering::Acquire) > 1);
}

#[test]
#[ignore = "explicit isolated namespace: scripts/verify-native-session.sh"]
fn unavailable_native_host_exhausts_the_original_attempt_cap_without_exposing_a_viewer() {
    let runtime = network::runtime();
    runtime.block_on(async {
        unavailable(&runtime, AfterTimeout::StayUnavailable, false).await;
        unavailable(&runtime, AfterTimeout::StayUnavailable, true).await;
    });
}
#[test]
#[ignore = "explicit isolated namespace: scripts/verify-native-session.sh"]
fn native_timeout_retry_stops_on_failed_cleanup_cancellation_or_changed_identity() {
    let runtime = network::runtime();
    runtime.block_on(async {
        for mode in [
            AfterTimeout::FailCleanup,
            AfterTimeout::Cancel,
            AfterTimeout::ChangeIdentity,
        ] {
            unavailable(&runtime, mode, false).await;
        }
    });
}
