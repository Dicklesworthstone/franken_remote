//! Production TLS/UDP, socket custody and deadlines. Atomic fences and cleanup
//! summaries are explicit owner fixtures, not native-input or hardware evidence.
use super::*;
use fr_wire::closure::{self, Cleanup, Closed, ClosedReason, OutstandingEffects};
use std::{future::Future, pin::pin, task::Poll};

fn outcome() -> Closed {
    Closed {
        reason: ClosedReason::ClientRequested,
        cleanup: Cleanup::Incomplete,
        effects: OutstandingEffects::Known {
            pending: 2,
            uncertain: u32::MAX,
        },
    }
}
fn arm(p: &mut Pair, cleanup: &Cx) -> (ClosedReport, Arc<AtomicBool>) {
    let reporting = ClosedReport::default();
    let fenced = Arc::new(AtomicBool::new(false));
    let signal = fenced.clone();
    p.server
        .arm_closed_report(
            cleanup,
            &p.server.binding(),
            p.control,
            binding(),
            reporting.registration(),
            move || {
                assert!(
                    !signal.swap(true, Ordering::AcqRel),
                    "one synchronous fence"
                );
            },
        )
        .unwrap();
    (reporting, fenced)
}
async fn delivered(p: &mut Pair, cleanup: &Cx, reporting: ClosedReport) {
    let done = Cell::new(false);
    let mut messages = Vec::new();
    let (result, ()) = Box::pin(both(
        async {
            let result = reporting.finish(outcome()).await;
            done.set(true);
            result
        },
        async {
            while !done.get() {
                p.client
                    .drive(cleanup, Duration::from_millis(1), || true)
                    .await
                    .unwrap();
                p.client
                    .receive(
                        cleanup,
                        || true,
                        |route, bytes| {
                            assert_eq!(
                                route,
                                Route::Stream(StreamRoute {
                                    outbound: false,
                                    ..p.control
                                })
                            );
                            assert_eq!(bytes.len(), closure::CLOSED_BYTES);
                            messages.push(
                                closure::decode_closed(
                                    bytes,
                                    binding(),
                                    &ProtocolLimits::ABSOLUTE,
                                    InputDirection::HostToViewer,
                                    InputDelivery::Reliable,
                                )
                                .unwrap(),
                            );
                            Ok(Disposition::Consumed)
                        },
                    )
                    .unwrap();
            }
        },
    ))
    .await;
    assert_eq!(result, Some(Ok(())));
    assert_eq!(messages, [outcome()], "never emit the private placeholder");
    assert_eq!(p.server.tick(cleanup, || true), Err(Error::Closed));
}

#[test]
fn close_captures_only_custody_and_waits_for_the_original_owners_final_outcome() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx).await;
        let (reporting, fenced) = arm(&mut p, &cx);
        p.server
            .send(
                &cx,
                Route::Stream(p.bulk),
                &bulk(),
                clock(&cx) + 1_000_000,
                || true,
            )
            .unwrap();
        assert_eq!(p.server.tick(&cx, || false), Err(Error::Unauthorized));
        assert!(p.server.is_closed());
        assert!(fenced.load(Ordering::Acquire));
        assert_eq!(p.server.usage().retained_send_records, 0);
        // The cleanup result is not known at the fence. Neither a placeholder
        // nor previously queued media may be transmitted while the owner works.
        for _ in 0..3 {
            p.client
                .drive(&cx, Duration::from_millis(1), || true)
                .await
                .unwrap();
            assert_eq!(
                p.client
                    .receive(&cx, || true, |_, _| panic!("report before completion")),
                Ok(0)
            );
        }
        delivered(&mut p, &cx, reporting).await;
    });
}

#[test]
fn cooperative_io_cancellation_retains_a_closed_only_socket_on_the_cleanup_context() {
    let rt = runtime();
    let session = rt.request_cx_with_budget(asupersync::types::Budget::INFINITE);
    let cleanup = rt.request_cx_with_budget(asupersync::types::Budget::INFINITE);
    rt.block_on(async {
        let mut p = pair(&session).await;
        let (reporting, fenced) = arm(&mut p, &cleanup);
        let mut pending = false;
        for _ in 0..16 {
            let mut operation = pin!(
                p.server
                    .drive(&session, Duration::from_millis(100), || true)
            );
            if std::future::poll_fn(|task| Poll::Ready(operation.as_mut().poll(task).is_pending()))
                .await
            {
                session.cancel_fast(CancelKind::User);
                assert_eq!(operation.await, Err(Error::Cancelled));
                pending = true;
                break;
            }
        }
        assert!(pending, "exercise actually pending native I/O");
        assert!(fenced.load(Ordering::Acquire));
        assert!(session.is_cancel_requested());
        assert!(!cleanup.is_cancel_requested());
        delivered(&mut p, &cleanup, reporting).await;
        assert!(session.is_cancel_requested());
    });
}

#[test]
fn slow_cleanup_or_delayed_report_poll_cannot_restart_the_closure_deadline() {
    for before_finish in [false, true] {
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let mut p = pair(&cx).await;
            let (reporting, fenced) = arm(&mut p, &cx);
            p.server.close();
            assert!(fenced.load(Ordering::Acquire));
            if before_finish {
                asupersync::time::sleep(cx.now(), Duration::from_millis(260)).await;
            }
            let finish = reporting.finish(outcome());
            if !before_finish {
                asupersync::time::sleep(cx.now(), Duration::from_millis(260)).await;
            }
            assert_eq!(finish.await, Some(Err(Error::Expired)));
            assert!(p.server.is_closed());
        });
    }
}

#[test]
fn abandoned_io_or_either_report_owner_cannot_emit_or_reopen_a_connection() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        assert_eq!(ClosedReport::default().finish(outcome()).await, None);
        let mut p = pair(&cx).await;
        let (reporting, fenced) = arm(&mut p, &cx);
        let mut pending = false;
        for _ in 0..16 {
            let mut operation = pin!(p.server.drive(&cx, Duration::from_millis(100), || true));
            if std::future::poll_fn(|task| Poll::Ready(operation.as_mut().poll(task).is_pending()))
                .await
            {
                pending = true;
                break; // Abandon, rather than return a cancelled I/O turn.
            }
        }
        assert!(pending);
        p.server.close();
        assert!(fenced.load(Ordering::Acquire));
        assert_eq!(reporting.finish(outcome()).await, Some(Err(Error::Closed)));
        let mut p = pair(&cx).await;
        let (reporting, fenced) = arm(&mut p, &cx);
        drop(reporting);
        p.server.close();
        assert!(fenced.load(Ordering::Acquire));
        let mut p = pair(&cx).await;
        let (reporting, fenced) = arm(&mut p, &cx);
        drop(p.server);
        assert!(fenced.load(Ordering::Acquire));
        assert_eq!(reporting.finish(outcome()).await, Some(Err(Error::Closed)));
    });
}

#[test]
fn foreign_or_consumed_registrations_cannot_mutate_another_live_connection() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut a = pair(&cx).await;
        let mut b = pair(&cx).await;
        let (reporting, _) = arm(&mut a, &cx);
        let registration = reporting.registration();
        assert_eq!(
            b.server.arm_closed_report(
                &cx,
                &a.server.binding(),
                b.control,
                binding(),
                registration.clone(),
                || panic!("foreign fence"),
            ),
            Err(Error::WrongRoute)
        );
        assert_eq!(
            b.server.arm_closed_report(
                &cx,
                &b.server.binding(),
                b.control,
                binding(),
                registration,
                || panic!("duplicate fence"),
            ),
            Err(Error::InvalidPolicy)
        );
        assert_eq!(b.server.tick(&cx, || true), Ok(()));
        a.server.close();
        delivered(&mut a, &cx, reporting).await;
    });
}

#[test]
fn the_first_terminal_registration_cannot_be_replaced_by_the_other_kind() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut a = pair(&cx).await;
        let (reporting, fenced) = arm(&mut a, &cx);
        let other = RevocationReport::default();
        assert_eq!(
            a.server.arm_revocation_report(
                &cx,
                &a.server.binding(),
                a.control,
                binding(),
                report().lease,
                other.registration(),
                || panic!("replaced observation fence"),
            ),
            Err(Error::InvalidPolicy)
        );
        assert!(!fenced.load(Ordering::Acquire));
        assert_eq!(other.finish().await, None);
        a.server.close();
        delivered(&mut a, &cx, reporting).await;
        let mut a = pair(&cx).await;
        let revocation = RevocationReport::default();
        let stopped = Arc::new(AtomicBool::new(false));
        let flag = stopped.clone();
        a.server
            .arm_revocation_report(
                &cx,
                &a.server.binding(),
                a.control,
                binding(),
                report().lease,
                revocation.registration(),
                move || {
                    assert!(!flag.swap(true, Ordering::AcqRel));
                    Reason::LocalRevoke
                },
            )
            .unwrap();
        let other = ClosedReport::default();
        assert_eq!(
            a.server.arm_closed_report(
                &cx,
                &a.server.binding(),
                a.control,
                binding(),
                other.registration(),
                || panic!("replaced lease fence"),
            ),
            Err(Error::InvalidPolicy)
        );
        assert_eq!(other.finish(outcome()).await, None);
        a.server.close();
        assert!(stopped.load(Ordering::Acquire));
        // The unmodified revocation delivery suite verifies this original owner.
        // Abandoning its sole captured drain cannot run the rejected callback.
        drop(revocation.finish());
    });
}

#[test]
fn native_payload_or_lost_terminal_security_refuses_without_dispatching_after_the_fence() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx).await;
        let (reporting, fenced) = arm(&mut p, &cx);
        p.server
            .send(
                &cx,
                Route::Stream(p.bulk),
                &bulk(),
                clock(&cx) + 1_000_000,
                || true,
            )
            .unwrap();
        p.server.drive(&cx, Duration::ZERO, || true).await.unwrap();
        p.server.close();
        assert!(fenced.load(Ordering::Acquire));
        assert_eq!(
            reporting.finish(outcome()).await,
            Some(Err(Error::Backpressure))
        );
    });
    for after_close in [false, true] {
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let mut p = pair(&cx).await;
            let allowed = Arc::new(AtomicBool::new(true));
            let guard = allowed.clone();
            p.server
                .retain_lifetime_check(&cx, Arc::new(move || guard.load(Ordering::Acquire)))
                .unwrap();
            let (reporting, fenced) = arm(&mut p, &cx);
            if !after_close {
                allowed.store(false, Ordering::Release);
            }
            p.server.close();
            allowed.store(false, Ordering::Release);
            assert!(fenced.load(Ordering::Acquire));
            assert_eq!(
                reporting.finish(outcome()).await,
                Some(Err(Error::Unauthorized))
            );
        });
    }
}

#[test]
fn premature_completion_or_unpolled_finish_never_sends_a_guessed_closed_report() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx).await;
        let (reporting, fenced) = arm(&mut p, &cx);
        assert_eq!(reporting.finish(outcome()).await, Some(Err(Error::Closed)));
        assert!(!fenced.load(Ordering::Acquire));
        p.server.close();
        assert!(fenced.load(Ordering::Acquire));
        let mut p = pair(&cx).await;
        let (reporting, _) = arm(&mut p, &cx);
        p.server.close();
        drop(reporting.finish(outcome()));
        p.client
            .drive(&cx, Duration::from_millis(2), || true)
            .await
            .unwrap();
        assert_eq!(
            p.client
                .receive(&cx, || true, |_, _| panic!("abandoned report")),
            Ok(0)
        );
    });
}
