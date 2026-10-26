//! Actual attached routes and UDP/TLS ownership with synthetic record payloads.
//! These exercise admission isolation, not a congestion-throughput guarantee.
use super::*;

async fn recovery_link(cx: &Cx, policy: Policy) -> Link {
    let mut l = link(cx, policy, policy).await;
    l.selection.capabilities.push(Capability {
        name: attachment::DELIVERY_CAPABILITY.into(),
        version: attachment::DELIVERY_VERSION,
        required: true,
    });
    l.selection.capabilities.sort_by(|a, b| a.name.cmp(&b.name));
    l
}

fn bulk_usage(q: &QuicRecords) -> (usize, usize) {
    let usage = q.usage();
    (
        usage.retained_send_upper_bound - usage.critical_send_bytes,
        usage.retained_send_records - usage.critical_send_records,
    )
}

async fn drain_setup(l: &mut Link, cx: &Cx) {
    // Promotion proves staging, not acknowledgement of the final Attached
    // record. Reclaim setup through actual peer ACKs before filling the pool.
    let until = clock(cx) + 1_000_000;
    while bulk_usage(&l.h) != (0, 0) || bulk_usage(&l.c) != (0, 0) {
        assert!(clock(cx) < until, "attachment send credit did not drain");
        l.drive(cx).await;
    }
}

fn send(q: &mut QuicRecords, cx: &Cx, route: StreamRoute, kind: u16, len: usize) {
    q.send(
        cx,
        Route::Stream(route),
        &record(kind, route.binding, len),
        clock(cx) + 1_000_000,
        || true,
    )
    .unwrap();
}

#[test]
fn concurrent_file_and_clipboard_records_leave_a_complete_recovery_record() {
    run_test!(cx, {
        let policy = Policy::default();
        let mut l = recovery_link(&cx, policy).await;
        let _input = attach(&mut l, &cx, 8, MediaRole::Input).await;
        let (_, _, clipboard, _) = attach(&mut l, &cx, 9, MediaRole::Clipboard).await;
        let (_, _, files, _) = attach(&mut l, &cx, 10, MediaRole::Files).await;
        let (_host, _viewer, recovery, _) = attach(&mut l, &cx, 11, MediaRole::Recovery).await;
        drain_setup(&mut l, &cx).await;
        assert_eq!(recovery.outbound.maximum, 8192);
        assert_eq!(bulk_usage(&l.h), (0, 0));
        send(&mut l.h, &cx, files.outbound, 0x72, 65_536);
        for _ in 0..3 {
            send(&mut l.h, &cx, clipboard.outbound, 0x51, 16_384);
        }
        send(&mut l.h, &cx, clipboard.outbound, 0x51, 8192);
        assert_eq!(bulk_usage(&l.h), (policy.retained_send_bytes - 8192, 5));
        let before = l.h.usage();
        let prepared = Cell::new(0);
        let original_send_by = clock(&cx) + 1_000_000;
        assert_eq!(
            l.h.send_prepared(
                &cx,
                Route::Stream(files.outbound),
                &record(0x72, 10, 24),
                original_send_by,
                || true,
                || {
                    prepared.set(prepared.get() + 1);
                    Ok::<(), ()>(())
                },
            ),
            Err(Error::Backpressure)
        );
        assert_eq!(prepared.get(), 0);
        assert_eq!(l.h.usage(), before);
        send(&mut l.h, &cx, recovery.outbound, 0x32, 8192);
        assert_eq!(bulk_usage(&l.h), (policy.retained_send_bytes, 6));
        assert_eq!(
            l.h.send(
                &cx,
                Route::Stream(recovery.outbound),
                &record(0x32, 11, 24),
                original_send_by,
                || true,
            ),
            Err(Error::Backpressure)
        );
        // A native prefix flush cannot reclaim either class of retained data.
        l.h.drive(&cx, Duration::ZERO, || true).await.unwrap();
        assert_eq!(bulk_usage(&l.h), (policy.retained_send_bytes, 6));
        assert!(!l.h.is_closed());
    });
}

#[test]
fn last_original_bulk_slot_remains_available_to_recovery() {
    run_test!(cx, {
        let policy = Policy {
            retained_send_records: 3,
            ..Policy::default()
        };
        let mut l = recovery_link(&cx, policy).await;
        let _input = attach(&mut l, &cx, 8, MediaRole::Input).await;
        let (_, _, files, _) = attach(&mut l, &cx, 9, MediaRole::Files).await;
        let (_host, _viewer, recovery, _) = attach(&mut l, &cx, 10, MediaRole::Recovery).await;
        drain_setup(&mut l, &cx).await;
        for _ in 0..2 {
            send(&mut l.h, &cx, files.outbound, 0x72, 24);
        }
        assert_eq!(
            l.h.send(
                &cx,
                Route::Stream(files.outbound),
                &record(0x72, 9, 24),
                clock(&cx) + 1_000_000,
                || true,
            ),
            Err(Error::Backpressure)
        );
        assert_eq!(bulk_usage(&l.h), (48, 2));
        send(&mut l.h, &cx, recovery.outbound, 0x32, 24);
        assert_eq!(bulk_usage(&l.h), (72, policy.retained_send_records));
    });
}

#[test]
fn retired_recovery_tombstone_releases_reservation_without_clearing_other_data() {
    run_test!(cx, {
        let policy = Policy {
            retained_send_records: 3,
            ..Policy::default()
        };
        let mut l = recovery_link(&cx, policy).await;
        let _input = attach(&mut l, &cx, 8, MediaRole::Input).await;
        let (_, _, files, _) = attach(&mut l, &cx, 9, MediaRole::Files).await;
        let (mut host, _viewer, recovery, _) = attach(&mut l, &cx, 10, MediaRole::Recovery).await;
        drain_setup(&mut l, &cx).await;
        for _ in 0..2 {
            send(&mut l.h, &cx, files.outbound, 0x72, 24);
        }
        let bytes = record(0x72, 9, 24);
        let original_send_by = clock(&cx) + 1_000_000;
        assert_eq!(
            l.h.send(
                &cx,
                Route::Stream(files.outbound),
                &bytes,
                original_send_by,
                || true
            ),
            Err(Error::Backpressure)
        );
        host.retire_media(&mut l.h, &cx).unwrap();
        assert!(!l.h.has_route(Route::Stream(recovery.outbound)));
        assert_eq!(bulk_usage(&l.h), (48, 2));
        l.h.send(
            &cx,
            Route::Stream(files.outbound),
            &bytes,
            original_send_by,
            || true,
        )
        .unwrap();
        assert_eq!(bulk_usage(&l.h), (72, 3));
        assert!(!l.h.is_closed());
    });
}

#[test]
fn absent_or_inbound_only_recovery_does_not_reduce_bulk_capacity() {
    run_test!(cx, {
        for inbound_only in [false, true] {
            let policy = Policy {
                retained_send_records: 3,
                ..Policy::default()
            };
            let mut l = recovery_link(&cx, policy).await;
            let _input = attach(&mut l, &cx, 8, MediaRole::Input).await;
            let (_, _, host, viewer) = attach(&mut l, &cx, 9, MediaRole::Files).await;
            let _recovery = if inbound_only {
                Some(attach(&mut l, &cx, 10, MediaRole::Recovery).await)
            } else {
                None
            };
            drain_setup(&mut l, &cx).await;
            let (q, route) = if inbound_only {
                (&mut l.c, viewer.outbound)
            } else {
                (&mut l.h, host.outbound)
            };
            for _ in 0..policy.retained_send_records {
                send(q, &cx, route, 0x72, 24);
            }
            assert_eq!(bulk_usage(q), (72, 3));
            assert_eq!(
                q.send(
                    &cx,
                    Route::Stream(route),
                    &record(0x72, 9, 24),
                    clock(&cx) + 1_000_000,
                    || true,
                ),
                Err(Error::Backpressure)
            );
        }
    });
}

#[test]
fn tiny_profile_borrows_only_one_empty_flight_and_keeps_caller_expiry() {
    run_test!(cx, {
        let capacity = attachment::GRANT_RECORD_BYTES;
        let policy = Policy {
            retained_send_bytes: capacity,
            ..Policy::default()
        };
        let mut l = recovery_link(&cx, policy).await;
        let _input = attach(&mut l, &cx, 8, MediaRole::Input).await;
        let (_, _, files, _) = attach(&mut l, &cx, 9, MediaRole::Files).await;
        let (_host, _viewer, recovery, _) = attach(&mut l, &cx, 10, MediaRole::Recovery).await;
        drain_setup(&mut l, &cx).await;
        assert_eq!(files.outbound.maximum, capacity);
        assert_eq!(recovery.outbound.maximum, capacity);
        send(&mut l.h, &cx, files.outbound, 0x72, capacity);
        assert_eq!(bulk_usage(&l.h), (capacity, 1));
        for (route, kind) in [(files.outbound, 0x72), (recovery.outbound, 0x32)] {
            assert_eq!(
                l.h.send(
                    &cx,
                    Route::Stream(route),
                    &record(kind, route.binding, 24),
                    clock(&cx) + 1_000_000,
                    || true,
                ),
                Err(Error::Backpressure)
            );
            let expired = clock(&cx);
            assert_eq!(
                l.h.send(
                    &cx,
                    Route::Stream(route),
                    &record(kind, route.binding, 24),
                    expired,
                    || true,
                ),
                Err(Error::Expired)
            );
        }
        assert_eq!(bulk_usage(&l.h), (capacity, 1));
        assert!(!l.h.is_closed());
    });
}
