//! Actual granted owners, UDP/TLS, input ledger and independent host Driver.
//! Decoder/presentation and OS key effects are explicitly the existing fixtures.
use super::*;
use crate::input_watchdog::StopReason as HostStop;
use crate::session_startup::viewer::controlled::events::{CaptureCleanup, NativeCapture};
use fr_wire::closure::{self, CloseRequest};
use std::sync::atomic::{AtomicBool, Ordering};

struct Capture {
    control: ViewerControl,
    stopped: Arc<AtomicBool>,
    reaped: Arc<AtomicBool>,
}
impl NativeCapture for Capture {
    fn stop(&self) {
        assert!(self.control.is_stopped(), "fence before platform cleanup");
        self.stopped.store(true, Ordering::Release);
    }
    fn try_reap(&mut self) -> bool {
        self.reaped.load(Ordering::Acquire)
    }
}
fn capture(viewer: &mut ControlledViewer) -> (Arc<AtomicBool>, Arc<AtomicBool>) {
    // Lifecycle-only fixture, not a claim of X11 cleanup or real native events.
    let stopped = Arc::new(AtomicBool::new(false));
    let reaped = Arc::new(AtomicBool::new(false));
    viewer.native_capture = Some(Box::new(Capture {
        control: viewer.control(),
        stopped: stopped.clone(),
        reaped: reaped.clone(),
    }));
    (stopped, reaped)
}
async fn turn(f: &mut Fixture) {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(40_000);
    let seed = NEXT.fetch_add(100, Ordering::Relaxed);
    let mut nonces = u128::from(seed);
    let mut tickets = u128::from(seed + 50);
    let (host, viewer) = Box::pin(support::both(
        f.host.drive(
            Duration::from_millis(1),
            || nonce(&mut nonces),
            || {
                tickets += 1;
                Some(InputTicketId::from_raw(tickets))
            },
            block,
        ),
        f.viewer.drive(Duration::from_millis(1), |_| {}, block),
    ))
    .await;
    host.unwrap();
    viewer.unwrap();
}
async fn settle(f: &mut Fixture) {
    for _ in 0..64 {
        turn(f).await;
        if f.host.io().unwrap().0.usage().retained_send_records == 0
            && f.viewer.session.transport.usage().retained_send_records == 0
        {
            return;
        }
    }
    panic!("original record credit did not settle");
}

async fn verify_retained(
    viewer: &mut ControlledViewer,
    cleanup_cx: &Cx,
    outcome: fr_transport::ControlCloseOutcome,
    last_receipt: Option<ResultEvent>,
) {
    assert!(viewer.is_closed());
    assert!(!viewer.pending_send());
    assert_eq!(viewer.pending_actions(), 1);
    assert_eq!(viewer.last_result(), last_receipt);
    assert_eq!(viewer.action(key(true)), Err(Error::Closed));
    assert_eq!(viewer.input_capture_cleanup(), CaptureCleanup::Pending);
    assert_eq!(viewer.disconnect_outcome(), Some(outcome));
    assert_eq!(
        viewer
            .disconnect_with_cleanup(cleanup_cx, closure::Reason::Requested)
            .await,
        Err(Error::Closed)
    );
    assert_eq!(viewer.disconnect_outcome(), Some(outcome));
}

fn arm_report(f: &mut Fixture, cleanup_cx: &Cx) -> quic::RevocationReport {
    let bound = binding(&f.viewer);
    let lease = f.viewer.input.binding().lease;
    let reporting = quic::RevocationReport::default();
    let stopped = f.host.control();
    let (q, routes) = f.host.io().unwrap();
    let original = q.binding();
    q.arm_revocation_report(
        cleanup_cx,
        &original,
        routes.outbound,
        bound,
        lease,
        reporting.registration(),
        move || {
            stopped.stop(HostStop::ClientDisconnected);
            // Local reporting policy fixture. The stages are always the
            // production terminal owner's Fenced/Unknown, not a native claim.
            Reason::SessionEnded
        },
    )
    .unwrap();
    reporting
}

#[test]
fn close_request_fences_granted_input_and_retains_the_actual_hosts_lease_report() {
    run(|c, h| async move {
        let cleanup_cx = Cx::current().unwrap();
        let mut f = Box::pin(fixture(&c, &h)).await;
        let driver = f.driver.take().unwrap();
        let control = f.host.control();
        let (outcome, summary) = Box::pin(support::both(
            async {
                settle(&mut f).await;
                let _ = f.viewer.action(key(true)).unwrap();
                for _ in 0..64 {
                    turn(&mut f).await;
                    if f.viewer.pending_actions() == 0 {
                        break;
                    }
                }
                assert_eq!(f.viewer.pending_actions(), 0);
                assert_eq!(f.effects.lock().unwrap().keys, [true]);
                settle(&mut f).await;
                let last_receipt = f.viewer.last_result();
                assert!(last_receipt.is_some());
                // Retain an encoded but unsubmitted action. Close must never retry it,
                // or turn missing per-action evidence into NotSubmitted/success.
                let _ = f.viewer.action(key(false)).unwrap();
                let lease = f.viewer.input.binding().lease;
                let reporting = arm_report(&mut f, &cleanup_cx);
                let (capture_stopped, capture_reaped) = capture(&mut f.viewer);
                let ending = f
                    .viewer
                    .disconnect_with_cleanup(&cleanup_cx, closure::Reason::Requested);
                assert!(c.is_cancel_requested());
                assert!(capture_stopped.load(Ordering::Acquire));
                assert!(!capture_reaped.load(Ordering::Acquire));
                let (outcome, sent) = Box::pin(support::both(ending, async {
                    let mut nonces = 60_000;
                    let mut tickets = 70_000;
                    loop {
                        let result = f
                            .host
                            .drive(
                                Duration::from_millis(1),
                                || nonce(&mut nonces),
                                || {
                                    tickets += 1;
                                    Some(InputTicketId::from_raw(tickets))
                                },
                                |_, _| panic!("CloseRequest must not reach application dispatch"),
                            )
                            .await;
                        if result.is_err() {
                            break;
                        }
                    }
                    assert!(control.is_stopped());
                    reporting.finish().await
                }))
                .await;
                let outcome = outcome.unwrap();
                assert_eq!(outcome.exchange.transport, Ok(()));
                assert_eq!(sent, Some(Ok(())));
                assert_eq!(
                    outcome.revocation,
                    Some(Revoked {
                        lease,
                        reason: Reason::SessionEnded,
                        cleanup: CleanupStage::Fenced,
                        effects: EffectStage::Unknown,
                    })
                );
                assert_eq!(outcome.exchange.report, None);
                verify_retained(&mut f.viewer, &cleanup_cx, outcome, last_receipt).await;
                capture_reaped.store(true, Ordering::Release);
                assert_eq!(
                    f.viewer
                        .reap_input_capture(
                            &cleanup_cx,
                            crate::worker::Deadline::after(&cleanup_cx, Duration::from_secs(1))
                                .unwrap()
                        )
                        .await,
                    Ok(CaptureCleanup::Complete)
                );
                assert_eq!(f.viewer.disconnect_outcome(), Some(outcome));
                outcome
            },
            driver,
        ))
        .await;
        assert!(summary.handoff_safe());
        // Actual original Driver cleanup submits the fixture's release. The
        // unsubmitted viewer action did not acquire a fabricated receipt.
        assert_eq!(f.effects.lock().unwrap().keys, [true, false]);
        assert_eq!(f.viewer.pending_actions(), 1);
        assert_eq!(outcome.revocation.unwrap().cleanup, CleanupStage::Fenced);
        f.observation.revoke();
    });
}

#[test]
fn unpolled_controller_close_fences_capture_but_cannot_claim_a_completed_exchange() {
    run(|c, h| async move {
        let cleanup_cx = Cx::current().unwrap();
        let mut f = Box::pin(fixture(&c, &h)).await;
        let _ = f.viewer.action(key(true)).unwrap();
        let (stopped, reaped) = capture(&mut f.viewer);
        let ending = f
            .viewer
            .disconnect_with_cleanup(&cleanup_cx, closure::Reason::Requested);
        assert!(c.is_cancel_requested());
        assert!(stopped.load(Ordering::Acquire));
        assert!(!reaped.load(Ordering::Acquire));
        drop(ending);
        assert!(f.viewer.session.transport.is_closed());
        assert!(!f.viewer.pending_send());
        assert_eq!(f.viewer.pending_actions(), 1);
        assert_eq!(f.viewer.disconnect_outcome(), None);
        assert_eq!(f.viewer.input_capture_cleanup(), CaptureCleanup::Pending);
        assert_eq!(f.viewer.action(key(false)), Err(Error::Closed));
        cleanup(&mut f).await;
    });
}

#[test]
fn the_cancelled_application_context_is_not_reused_as_cleanup_authority() {
    run(|c, h| async move {
        let mut f = Box::pin(fixture(&c, &h)).await;
        let (stopped, _) = capture(&mut f.viewer);
        let outcome = f
            .viewer
            .disconnect_with_cleanup(&c, closure::Reason::Requested)
            .await
            .unwrap();
        assert!(stopped.load(Ordering::Acquire));
        assert!(c.is_cancel_requested());
        assert_eq!(outcome.exchange.transport, Err(quic::Error::Cancelled));
        assert_eq!(outcome.revocation, None);
        assert_eq!(outcome.exchange.report, None);
        assert!(f.viewer.is_closed());
        cleanup(&mut f).await;
    });
}

#[test]
fn delayed_poll_keeps_the_call_time_budget_and_native_backlog_remains_a_refusal() {
    for backlog in [false, true] {
        run(|c, h| async move {
            let cleanup_cx = Cx::current().unwrap();
            let mut f = Box::pin(fixture(&c, &h)).await;
            settle(&mut f).await;
            if backlog {
                let _ = f.viewer.action(key(true)).unwrap();
                f.viewer.drive(Duration::ZERO, |_| {}, block).await.unwrap();
                assert!(f.viewer.session.transport.usage().retained_send_records > 0);
            }
            let ending = f
                .viewer
                .disconnect_with_cleanup(&cleanup_cx, closure::Reason::Requested);
            assert!(c.is_cancel_requested());
            if !backlog {
                asupersync::time::sleep(cleanup_cx.now(), Duration::from_millis(270)).await;
            }
            let outcome = ending.await.unwrap();
            assert_eq!(
                outcome.exchange.transport,
                Err(if backlog {
                    quic::Error::Backpressure
                } else {
                    quic::Error::Expired
                })
            );
            assert_eq!(outcome.revocation, None);
            assert_eq!(outcome.exchange.report, None);
            assert_eq!(f.viewer.disconnect_outcome(), Some(outcome));
            cleanup(&mut f).await;
        });
    }
}

#[test]
fn a_session_report_survives_controller_teardown_without_becoming_lease_release() {
    run(|c, h| async move {
        let cleanup_cx = Cx::current().unwrap();
        let mut f = Box::pin(fixture(&c, &h)).await;
        settle(&mut f).await;
        let _ = f.viewer.action(key(true)).unwrap();
        let bound = binding(&f.viewer);
        let expected = closure::Closed {
            reason: closure::ClosedReason::ClientRequested,
            cleanup: closure::Cleanup::Incomplete,
            effects: closure::OutstandingEffects::Known {
                pending: 2,
                uncertain: 3,
            },
        };
        let ending = f
            .viewer
            .disconnect_with_cleanup(&cleanup_cx, closure::Reason::Requested);
        let (outcome, sent) = Box::pin(support::both(ending, async {
            // Explicit host-accounting fixture; input cleanup is still the
            // original independent Driver, never inferred from these fields.
            let mut seen = false;
            let (q, routes) = f.host.io().unwrap();
            while !seen {
                q.drive(&h, Duration::from_millis(1), || true)
                    .await
                    .unwrap();
                q.receive(
                    &h,
                    || true,
                    |route, bytes| {
                        assert_eq!(route, Route::Stream(routes.inbound));
                        assert_eq!(
                            closure::decode_request(
                                bytes,
                                bound,
                                &ProtocolLimits::ABSOLUTE,
                                InputDirection::ViewerToHost,
                                InputDelivery::Reliable
                            )
                            .unwrap(),
                            CloseRequest {
                                reason: closure::Reason::Requested
                            }
                        );
                        seen = true;
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
            }
            let original = q.binding();
            q.close_with_closed(&h, &original, routes.outbound, bound, expected)
                .await
        }))
        .await;
        let outcome = outcome.unwrap();
        assert_eq!(outcome.exchange.report, Some(expected));
        assert_eq!(outcome.revocation, None);
        assert_eq!(outcome.exchange.transport, Ok(()));
        assert_eq!(sent, Ok(()));
        assert_eq!(f.viewer.session.closed_report(), Some(expected));
        assert_eq!(f.viewer.pending_actions(), 1);
        assert_eq!(f.viewer.disconnect_outcome(), Some(outcome));
        cleanup(&mut f).await;
        assert_eq!(f.viewer.disconnect_outcome(), Some(outcome));
    });
}

#[test]
fn a_late_stop_callback_cannot_acquire_a_fresh_network_closure_budget() {
    struct LateStop(AtomicBool);
    impl NativeCapture for LateStop {
        fn stop(&self) {
            if !self.0.swap(true, Ordering::AcqRel) {
                // Deliberately misbehaving adapter fixture. Real adapters must
                // be nonblocking; returning late still cannot buy another budget.
                std::thread::sleep(Duration::from_millis(270));
            }
        }
        fn try_reap(&mut self) -> bool {
            false
        }
    }
    run(|c, h| async move {
        let cleanup_cx = Cx::current().unwrap();
        let mut f = Box::pin(fixture(&c, &h)).await;
        settle(&mut f).await;
        f.viewer.native_capture = Some(Box::new(LateStop(AtomicBool::new(false))));
        let outcome = f
            .viewer
            .disconnect_with_cleanup(&cleanup_cx, closure::Reason::Requested)
            .await
            .unwrap();
        assert_eq!(outcome.exchange.transport, Err(quic::Error::Expired));
        assert_eq!(outcome.revocation, None);
        assert_eq!(outcome.exchange.report, None);
        assert!(c.is_cancel_requested());
        assert_eq!(f.viewer.input_capture_cleanup(), CaptureCleanup::Pending);
        cleanup(&mut f).await;
    });
}

mod streaming;
