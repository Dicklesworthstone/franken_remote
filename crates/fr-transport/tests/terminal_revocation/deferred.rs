//! Actual UDP/TLS with independent session/cleanup contexts. Fencing itself is
//! an explicit local atomic fixture, not an OS-input qualification claim.
use super::*;
use std::{future::Future, pin::pin, task::Poll};

fn arm(pair: &mut Pair, cleanup: &Cx) -> (RevocationReport, Arc<AtomicBool>) {
    let reporting = RevocationReport::default();
    let fenced = Arc::new(AtomicBool::new(false));
    let signal = fenced.clone();
    pair.local
        .arm_revocation_report(
            cleanup,
            &pair.local.binding(),
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

pub(super) async fn delivered(pair: &mut Pair, cleanup: &Cx, reporting: RevocationReport) {
    assert!(pair.local.is_closed());
    let (result, messages) = terminal::collect(
        cleanup,
        pair,
        reporting.finish(),
        terminal::decode_revocation,
    )
    .await;
    assert_eq!(result, Some(Ok(())));
    assert_eq!(messages, [report()]);
    assert_eq!(pair.local.tick(cleanup, || true), Err(Error::Closed));
}

#[test]
fn authorization_failure_fences_then_reports_without_flushing_queued_media() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx).await;
        let (reporting, fenced) = arm(&mut p, &cx);
        p.local
            .send(
                &cx,
                Route::Stream(p.bulk),
                &bulk(),
                clock(&cx) + 1_000_000,
                || true,
            )
            .unwrap();
        assert_eq!(
            p.local
                .receive(&cx, || false, |_, _| panic!("no post-fence dispatch")),
            Err(Error::Unauthorized)
        );
        assert!(fenced.load(Ordering::Acquire));
        assert_eq!(p.local.usage().retained_send_records, 0);
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
            let mut operation = pin!(p.local.drive(&session, Duration::from_millis(100), || true));
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

/// A dropped I/O turn transfers the same terminal custody as a returned
/// cancellation: the lease is fenced at the drop, the revocation is still
/// reported once, and the connection is never driven again (fr-izv6).
#[test]
fn abandoned_pending_io_still_reports_the_revocation_once() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx).await;
        let (reporting, fenced) = arm(&mut p, &cx);
        let mut exercised = false;
        for _ in 0..16 {
            let mut operation = pin!(p.local.drive(&cx, Duration::from_millis(100), || true));
            if std::future::poll_fn(|task| Poll::Ready(operation.as_mut().poll(task).is_pending()))
                .await
            {
                exercised = true;
                break; // drop, not returned cancellation
            }
        }
        assert!(exercised);
        assert!(fenced.load(Ordering::Acquire));
        assert_eq!(p.local.tick(&cx, || true), Err(Error::Closed));
        delivered(&mut p, &cx, reporting).await;
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
            b.local.arm_revocation_report(
                &cx,
                &a.local.binding(),
                b.control,
                binding(),
                report().lease,
                reporting.registration(),
                || panic!("foreign fence"),
            ),
            Err(Error::WrongRoute)
        );
        assert!(!b.local.is_closed());
        assert_eq!(
            b.local.arm_revocation_report(
                &cx,
                &b.local.binding(),
                b.control,
                binding(),
                report().lease,
                reporting.registration(),
                || panic!("duplicate fence"),
            ),
            Err(Error::InvalidPolicy)
        );
        assert!(!b.local.is_closed());
        a.local.close();
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
        p.local
            .retain_lifetime_check(&cx, Arc::new(move || guard.load(Ordering::Acquire)))
            .unwrap();
        let (reporting, fenced) = arm(&mut p, &cx);
        p.local.close();
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
        p.local
            .send(
                &cx,
                Route::Stream(p.bulk),
                &bulk(),
                clock(&cx) + 1_000_000,
                || true,
            )
            .unwrap();
        p.local.drive(&cx, Duration::ZERO, || true).await.unwrap();
        p.local.close();
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
        p.local.close();
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
        p.local.close();
        assert!(fenced.load(Ordering::Acquire));
        assert!(p.local.is_closed());
        let mut p = pair(&cx).await;
        let (reporting, fenced) = arm(&mut p, &cx);
        drop(p.local);
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
        p.local
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
        assert_eq!(p.local.tick(&cx, || true), Err(Error::Unauthorized));
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
            p.local
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
            p.local.close();
            security.store(false, Ordering::Release);
            assert!(fenced.load(Ordering::Acquire));
            assert_eq!(reporting.finish().await, Some(Err(Error::Unauthorized)));
            assert!(p.local.is_closed());
        });
    }
}

#[test]
fn both_lifetime_guards_are_frozen_before_traffic_and_cannot_be_retrofitted() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx).await;
        p.local
            .retain_lifetime_check(&cx, Arc::new(|| true))
            .unwrap();
        assert_eq!(
            p.local
                .retain_lifetime_checks(&cx, Arc::new(|| true), Arc::new(|| true)),
            Err(Error::InvalidPolicy)
        );
        let mut other = pair(&cx).await;
        assert_eq!(
            other
                .local
                .retain_lifetime_checks(&cx, Arc::new(|| true), Arc::new(|| false)),
            Err(Error::Unauthorized)
        );
        assert!(other.local.is_closed());
    });
}

/// Send one small record on the bulk lane: every byte leaves in one packet and
/// stays retained until the peer acknowledges it. Nothing is left unsent.
fn sent_unacknowledged(p: &mut Pair, cx: &Cx) -> Vec<u8> {
    let small = crate::terminal::recovery_bytes(100);
    p.local
        .send(
            cx,
            Route::Stream(p.bulk),
            &small,
            clock(cx) + 1_000_000,
            || true,
        )
        .unwrap();
    small
}

/// Bytes sent before the fence and waiting only for the peer's ACK are not
/// backlog: the registered report waits for that ACK, acknowledging and
/// receiving only, and is delivered as the record instead of the close packet
/// (fr-3d8i: about 90% of the close-packet fallbacks in namespace suites).
#[test]
fn sent_but_unacknowledged_bytes_settle_into_the_delivered_report() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx).await;
        let (reporting, fenced) = arm(&mut p, &cx);
        let small = sent_unacknowledged(&mut p, &cx);
        p.local.drive(&cx, Duration::ZERO, || true).await.unwrap();
        assert!(
            p.local.usage().retained_send_records > 0,
            "sent, not yet acknowledged"
        );
        p.local.close();
        assert!(fenced.load(Ordering::Acquire));
        let done = Cell::new(false);
        let mut bulk = Vec::new();
        let mut reports = Vec::new();
        let control = StreamRoute {
            outbound: false,
            ..p.control
        };
        let incoming_bulk = StreamRoute {
            outbound: false,
            ..p.bulk
        };
        let (result, ()) = Box::pin(both(
            async {
                let result = reporting.finish().await;
                done.set(true);
                result
            },
            async {
                while !done.get() {
                    p.peer
                        .drive(&cx, Duration::from_millis(1), || true)
                        .await
                        .unwrap();
                    p.peer
                        .receive(
                            &cx,
                            || true,
                            |route, bytes| {
                                if route == Route::Stream(incoming_bulk) {
                                    bulk.push(bytes.to_vec());
                                } else {
                                    assert_eq!(route, Route::Stream(control));
                                    reports.push(terminal::decode_revocation(bytes));
                                }
                                Ok(Disposition::Consumed)
                            },
                        )
                        .unwrap();
                }
            },
        ))
        .await;
        assert_eq!(result, Some(Ok(())), "the record, not the close packet");
        assert_eq!(bulk, [small]);
        assert_eq!(reports, [report()]);
    });
}

/// The same bytes with a peer that never acknowledges: the report waits half
/// its frozen 250 ms budget, then takes the close packet exactly as before.
#[test]
fn bytes_still_unacknowledged_at_half_the_budget_fall_back_to_the_close_packet() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut p = pair(&cx).await;
        let (reporting, fenced) = arm(&mut p, &cx);
        sent_unacknowledged(&mut p, &cx);
        p.local.drive(&cx, Duration::ZERO, || true).await.unwrap();
        assert!(p.local.usage().retained_send_records > 0);
        let closed_at = clock(&cx);
        p.local.close();
        assert!(fenced.load(Ordering::Acquire));
        assert_eq!(reporting.finish().await, Some(Err(Error::Backpressure)));
        let waited = clock(&cx) - closed_at;
        assert!(
            (120_000..250_000).contains(&waited),
            "waited {waited} us, not about half the 250 ms budget"
        );
    });
}
