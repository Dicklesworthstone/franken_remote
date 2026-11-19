//! Native ACK reconciliation is transport evidence, not application permission.
use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

async fn overdue_pair(cx: &Cx, acknowledge: bool, stage: bool) -> support::Pair {
    let mut p = pair(cx, Policy::default()).await;
    let route = Route::Stream(p.host_routes[1]);
    measure_path(cx, &mut p, route).await;
    let packet = recovery_packet(2, 100);
    let admitted = clock(cx);
    p.server
        .send(cx, route, &packet, admitted + 50_000, || true)
        .unwrap();
    if stage {
        p.server.drive(cx, Duration::ZERO, || true).await.unwrap();
    }
    let mut received = 0;
    if acknowledge {
        while clock(cx) < admitted + 60_000 {
            p.client
                .drive(cx, Duration::from_millis(1), || true)
                .await
                .unwrap();
            p.client
                .receive(
                    cx,
                    || true,
                    |_, bytes| {
                        assert_eq!(bytes, packet);
                        received += 1;
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
        }
        assert_eq!(received, 1);
    }
    assert_eq!(p.server.usage().retained_send_records, 1);
    asupersync::time::sleep(
        cx.now(),
        Duration::from_micros(MAX_DELIVERY_ALLOWANCE_US + 50_000),
    )
    .await;
    assert!(clock(cx) > admitted + MAX_DELIVERY_ALLOWANCE_US);
    p
}

#[test]
fn reconciliation_without_overdue_sends_leaves_maintenance_permissions_untouched() {
    run_test!(cx, {
        let mut p = pair(&cx, Policy::default()).await;
        let before = p.server.usage();
        p.server
            .reconcile_acknowledgements(&cx, || panic!("not an I/O turn"))
            .await
            .unwrap();
        assert_eq!(p.server.usage(), before);
        assert!(!p.server.is_closed());
        // The no-op is not a replacement for ordinary authorization.
        assert_eq!(p.server.tick(&cx, || false), Err(Error::Unauthorized));
    });
}

#[test]
fn overdue_send_without_a_peer_ack_keeps_its_original_terminal_deadline() {
    run_test!(cx, {
        let mut p = overdue_pair(&cx, false, true).await;
        assert_eq!(
            p.server.reconcile_acknowledgements(&cx, || true).await,
            Err(Error::Expired)
        );
        assert!(p.server.is_closed());
        assert_eq!(p.server.usage().retained_send_records, 0);
    });
}

#[test]
fn overdue_unsent_record_never_enters_native_io_to_find_an_ack() {
    run_test!(cx, {
        let mut p = overdue_pair(&cx, false, false).await;
        assert_eq!(
            p.server.reconcile_acknowledgements(&cx, || true).await,
            Err(Error::Expired)
        );
        assert!(p.server.is_closed());
        p.client
            .drive(&cx, Duration::from_millis(1), || true)
            .await
            .unwrap();
        assert_eq!(
            p.client
                .receive(&cx, || true, |_, _| panic!("unsent expired data escaped")),
            Ok(0)
        );
    });
}

#[test]
fn overdue_partial_record_cannot_finish_during_reconciliation() {
    run_test!(cx, {
        let mut p = pair(&cx, Policy::default()).await;
        let route = Route::Stream(p.host_routes[1]);
        measure_path(&cx, &mut p, route).await;
        // Larger than the bounded prefixes in one native turn. Only the first
        // part can leave before the sender stops being scheduled.
        p.server
            .send(
                &cx,
                route,
                &recovery_packet(2, 20_000),
                clock(&cx) + 50_000,
                || true,
            )
            .unwrap();
        p.server.drive(&cx, Duration::ZERO, || true).await.unwrap();
        asupersync::time::sleep(
            cx.now(),
            Duration::from_micros(MAX_DELIVERY_ALLOWANCE_US + 50_000),
        )
        .await;
        assert_eq!(
            p.server.reconcile_acknowledgements(&cx, || true).await,
            Err(Error::Expired)
        );
        assert!(p.server.is_closed());
        for _ in 0..8 {
            p.client
                .drive(&cx, Duration::from_millis(1), || true)
                .await
                .unwrap();
            assert_eq!(
                p.client
                    .receive(&cx, || true, |_, _| panic!("expired tail escaped")),
                Ok(0)
            );
        }
    });
}

#[test]
fn an_ack_queued_for_an_overdue_record_cannot_override_authority_loss() {
    run_test!(cx, {
        let mut p = overdue_pair(&cx, true, true).await;
        assert_eq!(
            p.server.reconcile_acknowledgements(&cx, || false).await,
            Err(Error::Unauthorized)
        );
        assert!(p.server.is_closed());
    });
}

#[test]
fn synchronous_tick_still_enforces_expiry_without_reading_queued_acks() {
    run_test!(cx, {
        let mut p = overdue_pair(&cx, true, true).await;
        assert_eq!(p.server.tick(&cx, || true), Err(Error::Expired));
        assert!(p.server.is_closed());
    });
}

#[test]
fn acknowledgement_reconciliation_never_flushes_other_queued_records() {
    run_test!(cx, {
        let mut p = pair(&cx, Policy::default()).await;
        let route = Route::Stream(p.host_routes[1]);
        measure_path(&cx, &mut p, route).await;
        let first = recovery_packet(2, 100);
        let queued = recovery_packet(2, 200);
        let admitted = clock(&cx);
        p.server
            .send(&cx, route, &first, admitted + 50_000, || true)
            .unwrap();
        p.server.drive(&cx, Duration::ZERO, || true).await.unwrap();
        let mut received = 0;
        while clock(&cx) < admitted + 60_000 {
            p.client
                .drive(&cx, Duration::from_millis(1), || true)
                .await
                .unwrap();
            p.client
                .receive(
                    &cx,
                    || true,
                    |_, bytes| {
                        assert_eq!(bytes, first);
                        received += 1;
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
        }
        assert_eq!(received, 1);
        p.server
            .send(&cx, route, &queued, admitted + 10_000_000, || true)
            .unwrap();
        asupersync::time::sleep(
            cx.now(),
            Duration::from_micros(MAX_DELIVERY_ALLOWANCE_US + 50_000),
        )
        .await;
        p.server
            .reconcile_acknowledgements(&cx, || true)
            .await
            .unwrap();
        assert_eq!(p.server.usage().retained_send_records, 1);
        // A queued fresh record does not authorize any ordinary flush as a
        // side effect of learning the older record's acknowledgement.
        p.client
            .drive(&cx, Duration::from_millis(1), || true)
            .await
            .unwrap();
        assert_eq!(
            p.client
                .receive(&cx, || true, |_, _| panic!("reconciliation sent data")),
            Ok(0)
        );
        for _ in 0..500 {
            drive(&cx, &mut p).await;
            p.client
                .receive(
                    &cx,
                    || true,
                    |_, bytes| {
                        assert_eq!(bytes, queued);
                        received += 1;
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
            if received == 2 && p.server.usage().retained_send_records == 0 {
                break;
            }
        }
        assert_eq!(received, 2);
        assert_eq!(p.server.usage().retained_send_records, 0);
    });
}

#[test]
fn original_connection_and_operation_guards_fence_reconciliation() {
    for operation in [false, true] {
        run_test!(cx, {
            let mut p = pair(&cx, Policy::default()).await;
            let live = Arc::new(AtomicBool::new(true));
            let gate = live.clone();
            if operation {
                p.server
                    .retain_operation_check(&cx, Arc::new(move || gate.load(Ordering::Acquire)))
                    .unwrap();
            } else {
                p.server
                    .retain_lifetime_check(&cx, Arc::new(move || gate.load(Ordering::Acquire)))
                    .unwrap();
            }
            let route = Route::Stream(p.host_routes[1]);
            p.server
                .send(
                    &cx,
                    route,
                    &recovery_packet(2, 100),
                    clock(&cx) + 50_000,
                    || true,
                )
                .unwrap();
            p.server.drive(&cx, Duration::ZERO, || true).await.unwrap();
            asupersync::time::sleep(
                cx.now(),
                Duration::from_micros(MAX_DELIVERY_ALLOWANCE_US + 50_000),
            )
            .await;
            live.store(false, Ordering::Release);
            assert_eq!(
                p.server.reconcile_acknowledgements(&cx, || true).await,
                Err(Error::Unauthorized)
            );
            assert!(p.server.is_closed());
        });
    }
}

#[test]
fn cancelled_context_wins_over_a_queued_acknowledgement() {
    run_test!(cx, {
        let mut p = overdue_pair(&cx, true, true).await;
        cx.cancel_fast(CancelKind::User);
        assert_eq!(
            p.server.reconcile_acknowledgements(&cx, || true).await,
            Err(Error::Cancelled)
        );
        assert!(p.server.is_closed());
    });
}
