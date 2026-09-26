//! Actual UDP/TLS with independent session/cleanup contexts. Fencing itself is
//! an explicit local atomic fixture, not an OS-input qualification claim.
use super::*;
use std::{future::Future, pin::pin, task::Poll};

fn arm(pair: &mut Pair, cleanup: &Cx) -> (RevocationReport, Arc<AtomicBool>) {
    let reporting = RevocationReport::default();
    let fenced = Arc::new(AtomicBool::new(false));
    let signal = fenced.clone();
    pair.server
        .arm_revocation_report(
            cleanup,
            &pair.server.binding(),
            pair.control,
            binding(),
            report().lease,
            reporting.registration(),
            move || {
                signal.store(true, Ordering::Release);
                Reason::LocalRevoke
            },
        )
        .unwrap();
    (reporting, fenced)
}

async fn delivered(pair: &mut Pair, cleanup: &Cx, reporting: RevocationReport) {
    assert!(pair.server.is_closed());
    let done = Cell::new(false);
    let mut messages = Vec::new();
    let (result, ()) = Box::pin(both(
        async {
            let result = reporting.finish().await;
            done.set(true);
            result
        },
        async {
            while !done.get() {
                pair.client
                    .drive(cleanup, Duration::from_millis(1), || true)
                    .await
                    .unwrap();
                pair.client
                    .receive(
                        cleanup,
                        || true,
                        |route, bytes| {
                            assert_eq!(
                                route,
                                Route::Stream(StreamRoute {
                                    outbound: false,
                                    ..pair.control
                                })
                            );
                            messages.push(
                                lease_revoked::decode(
                                    bytes,
                                    binding(),
                                    report().lease,
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
    assert_eq!(messages, [report()]);
    assert_eq!(pair.server.tick(cleanup, || true), Err(Error::Closed));
}

#[test]
fn authorization_failure_fences_then_reports_without_flushing_queued_media() {
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
        assert_eq!(
            p.server
                .receive(&cx, || false, |_, _| panic!("no post-fence dispatch")),
            Err(Error::Unauthorized)
        );
        assert!(fenced.load(Ordering::Acquire));
        assert_eq!(p.server.usage().retained_send_records, 0);
        delivered(&mut p, &cx, reporting).await;
    });
}

#[test]
fn returned_pending_io_cancellation_transfers_only_terminal_custody() {
    let rt = runtime();
    let session = rt.request_cx_with_budget(asupersync::types::Budget::INFINITE);
    let cleanup = rt.request_cx_with_budget(asupersync::types::Budget::INFINITE);
    rt.block_on(async {
        let mut p = pair(&session).await;
        let (reporting, fenced) = arm(&mut p, &cleanup);
        let mut exercised = false;
        for _ in 0..16 {
            let mut operation = pin!(
                p.server
                    .drive(&session, Duration::from_millis(100), || true)
            );
            let pending = std::future::poll_fn(|task| {
                Poll::Ready(operation.as_mut().poll(task).is_pending())
            })
            .await;
            if pending {
                session.cancel_fast(CancelKind::User);
                assert_eq!(operation.await, Err(Error::Cancelled));
                exercised = true;
                break;
            }
        }
        assert!(exercised, "must exercise genuinely pending native I/O");
        assert!(session.is_cancel_requested());
        assert!(!cleanup.is_cancel_requested());
        assert!(fenced.load(Ordering::Acquire));
        delivered(&mut p, &cleanup, reporting).await;
        assert!(
            session.is_cancel_requested(),
            "reporting must not un-cancel the session"
        );
    });
}

#[test]
fn abandoned_pending_io_is_not_recovered_as_a_reportable_socket() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx).await;
        let (reporting, fenced) = arm(&mut p, &cx);
        let mut exercised = false;
        for _ in 0..16 {
            let mut operation = pin!(p.server.drive(&cx, Duration::from_millis(100), || true));
            if std::future::poll_fn(|task| Poll::Ready(operation.as_mut().poll(task).is_pending()))
                .await
            {
                exercised = true;
                break; // drop, not returned cancellation
            }
        }
        assert!(exercised);
        p.server.close();
        assert!(fenced.load(Ordering::Acquire));
        assert_eq!(reporting.finish().await, Some(Err(Error::Closed)));
    });
}

#[test]
fn one_registration_cannot_arm_a_second_connection_or_use_foreign_identity() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut a = pair(&cx).await;
        let mut b = pair(&cx).await;
        let (reporting, _) = arm(&mut a, &cx);
        assert_eq!(
            b.server.arm_revocation_report(
                &cx,
                &a.server.binding(),
                b.control,
                binding(),
                report().lease,
                reporting.registration(),
                || panic!("foreign fence"),
            ),
            Err(Error::WrongRoute)
        );
        assert!(!b.server.is_closed());
        assert_eq!(
            b.server.arm_revocation_report(
                &cx,
                &b.server.binding(),
                b.control,
                binding(),
                report().lease,
                reporting.registration(),
                || panic!("duplicate fence"),
            ),
            Err(Error::InvalidPolicy)
        );
        assert!(!b.server.is_closed());
        a.server.close();
        delivered(&mut a, &cx, reporting).await;
    });
}

#[test]
fn immutable_ingress_guard_is_not_bypassed_by_deferred_reporting() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx).await;
        let alive = Arc::new(AtomicBool::new(true));
        let guard = alive.clone();
        p.server
            .retain_lifetime_check(&cx, Arc::new(move || guard.load(Ordering::Acquire)))
            .unwrap();
        let (reporting, fenced) = arm(&mut p, &cx);
        p.server.close();
        alive.store(false, Ordering::Release);
        assert_eq!(reporting.finish().await, Some(Err(Error::Unauthorized)));
        assert!(fenced.load(Ordering::Acquire));
    });
}

#[test]
fn native_payload_backlog_remains_a_terminal_refusal() {
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
        assert_eq!(reporting.finish().await, Some(Err(Error::Backpressure)));
    });
}

#[test]
fn report_deadline_starts_at_close_not_at_consumer_poll() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx).await;
        let (reporting, fenced) = arm(&mut p, &cx);
        p.server.close();
        assert!(fenced.load(Ordering::Acquire));
        asupersync::time::sleep(cx.now(), Duration::from_millis(260)).await;
        assert_eq!(reporting.finish().await, Some(Err(Error::Expired)));
    });
}

#[test]
fn abandoning_either_owner_never_fabricates_delivery_or_reopens_io() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        assert_eq!(RevocationReport::default().finish().await, None);
        let mut p = pair(&cx).await;
        let (reporting, fenced) = arm(&mut p, &cx);
        drop(reporting);
        p.server.close();
        assert!(fenced.load(Ordering::Acquire));
        assert!(p.server.is_closed());
        let mut p = pair(&cx).await;
        let (reporting, fenced) = arm(&mut p, &cx);
        drop(p.server);
        assert!(
            fenced.load(Ordering::Acquire),
            "drop must fence before destroying socket"
        );
        assert_eq!(reporting.finish().await, Some(Err(Error::Closed)));
    });
}

#[test]
fn paired_guards_keep_normal_cancellation_terminal_but_preserve_security_for_the_report() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx).await;
        let active = Arc::new(AtomicBool::new(true));
        let ordinary = active.clone();
        let security = Arc::new(AtomicBool::new(true));
        let retained = security.clone();
        let terminal = security.clone();
        p.server
            .retain_lifetime_checks(
                &cx,
                Arc::new(move || {
                    ordinary.load(Ordering::Acquire) && retained.load(Ordering::Acquire)
                }),
                Arc::new(move || terminal.load(Ordering::Acquire)),
            )
            .unwrap();
        let (reporting, fenced) = arm(&mut p, &cx);
        active.store(false, Ordering::Release);
        assert_eq!(p.server.tick(&cx, || true), Err(Error::Unauthorized));
        assert!(fenced.load(Ordering::Acquire));
        delivered(&mut p, &cx, reporting).await;
    });
}

#[test]
fn paired_guards_never_bypass_security_loss_during_terminal_preparation_or_drain() {
    for after_close in [false, true] {
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let mut p = pair(&cx).await;
            let security = Arc::new(AtomicBool::new(true));
            let ordinary = security.clone();
            let terminal = security.clone();
            p.server
                .retain_lifetime_checks(
                    &cx,
                    Arc::new(move || ordinary.load(Ordering::Acquire)),
                    Arc::new(move || terminal.load(Ordering::Acquire)),
                )
                .unwrap();
            let (reporting, fenced) = arm(&mut p, &cx);
            if !after_close {
                security.store(false, Ordering::Release);
            }
            p.server.close();
            security.store(false, Ordering::Release);
            assert!(fenced.load(Ordering::Acquire));
            assert_eq!(reporting.finish().await, Some(Err(Error::Unauthorized)));
            assert!(p.server.is_closed());
        });
    }
}

#[test]
fn both_lifetime_guards_are_frozen_before_traffic_and_cannot_be_retrofitted() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx).await;
        p.server
            .retain_lifetime_check(&cx, Arc::new(|| true))
            .unwrap();
        assert_eq!(
            p.server
                .retain_lifetime_checks(&cx, Arc::new(|| true), Arc::new(|| true)),
            Err(Error::InvalidPolicy)
        );
        let mut other = pair(&cx).await;
        assert_eq!(
            other
                .server
                .retain_lifetime_checks(&cx, Arc::new(|| true), Arc::new(|| false)),
            Err(Error::Unauthorized)
        );
        assert!(other.server.is_closed());
    });
}
