//! Pre-production credit through the actual child IPC path, not codec evidence.
use super::*;
use std::{
    future::{Future, poll_fn},
    task::Poll,
};

fn reserved_pool(pictures: usize) -> SharedFramePool {
    SharedFramePool::new(ProtocolLimits::ABSOLUTE, 4 * 1024 * 1024, pictures).unwrap()
}

#[test]
fn impossible_pool_and_profile_refuse_before_native_frame_identity_advances() {
    run_shared!(rt, cx, {
        let (owner, _) = gate(&rt, 1);
        let mut s = source(&owner, true).await;
        assert_eq!(
            s.prepare_shared_capture(&owner, &pool()).unwrap_err(),
            Error::InvalidFrame
        );
        let narrow = SharedFramePool::new(
            ProtocolLimits::with_overrides(fr_core::limits::LimitOverrides {
                max_encoded_access_unit_bytes: Some(512 * 1024),
                ..Default::default()
            })
            .unwrap(),
            4 * 1024 * 1024,
            4,
        )
        .unwrap();
        assert_eq!(
            s.prepare_shared_capture(&owner, &narrow).unwrap_err(),
            Error::InvalidFrame
        );
        let p = reserved_pool(1);
        let prepared = s.prepare_shared_capture(&owner, &p).unwrap();
        assert_eq!(p.usage().bytes, prepared.reserved_bytes());
        let update = prepared.capture_if_changed(true).await.unwrap();
        assert_eq!(update.frame().as_raw(), 0);
        assert_eq!(p.usage().pictures, 1);
        drop(update);
        assert_eq!(p.usage(), BudgetUsage::default());
        stop(&mut s, &cx).await;
    });
}

#[test]
fn full_pool_blocks_capture_without_skipping_the_next_reference() {
    run_shared!(rt, cx, {
        let (owner, _) = gate(&rt, 1);
        let mut s = source(&owner, true).await;
        let p = reserved_pool(1);
        let first = s
            .prepare_shared_capture(&owner, &p)
            .unwrap()
            .capture_if_changed(true)
            .await
            .unwrap();
        let charge = p.usage();
        assert_eq!(
            s.prepare_shared_capture(&owner, &p).unwrap_err(),
            Error::Backpressure
        );
        assert_eq!(p.usage(), charge);
        drop(first);
        let next = s
            .prepare_shared_capture(&owner, &p)
            .unwrap()
            .capture_if_changed(false)
            .await
            .unwrap();
        assert_eq!(next.frame().as_raw(), 1);
        assert_eq!(
            next.encoded().unwrap().kind(),
            fr_media::access_unit::FrameKind::Predicted {
                references: fr_media::access_unit::FrameId::FIRST,
            }
        );
        stop(&mut s, &cx).await;
    });
}

#[test]
fn unpolled_preparation_is_side_effect_free_and_native_cancellation_refunds_after_abort() {
    run_shared!(rt, cx, {
        let (owner, _) = gate(&rt, 1);
        let mut s = source(&owner, true).await;
        let p = reserved_pool(1);
        drop(s.prepare_shared_capture(&owner, &p).unwrap());
        assert_eq!(p.usage(), BudgetUsage::default());
        let mut capture = Box::pin(
            s.prepare_shared_capture(&owner, &p)
                .unwrap()
                .capture_if_changed(true),
        );
        poll_fn(|task| match capture.as_mut().poll(task) {
            Poll::Pending => Poll::Ready(()),
            Poll::Ready(_) => panic!("actual child exchange must yield before completion"),
        })
        .await;
        assert_eq!(p.usage().pictures, 1);
        assert!(p.reserve_capacity(1).is_err());
        drop(capture);
        assert_eq!(p.usage(), BudgetUsage::default());
        let w = s.worker_mut();
        assert_eq!(w.state(), frd::worker::State::Poisoned);
        w.reap(&cx, Deadline::after(&cx, Duration::from_secs(1)).unwrap())
            .await
            .unwrap();
    });
}

#[test]
fn pending_capture_retains_one_slot_then_shares_the_same_actual_output() {
    run_shared!(rt, cx, {
        let (owner, _) = gate(&rt, 1);
        let (ac, _) = gate(&rt, 2);
        let (bc, _) = gate(&rt, 3);
        let mut s = source(&owner, true).await;
        let p = reserved_pool(1);
        let mut capture = Box::pin(
            s.prepare_shared_capture(&owner, &p)
                .unwrap()
                .capture_if_changed(true),
        );
        poll_fn(|task| match capture.as_mut().poll(task) {
            Poll::Pending => Poll::Ready(()),
            Poll::Ready(_) => panic!("actual child exchange must yield before completion"),
        })
        .await;
        let reserved = p.usage();
        assert_eq!(reserved.pictures, 1);
        assert!(p.clone().reserve_capacity(1).is_err());
        let update = capture.await.unwrap();
        assert_eq!(update.frame().as_raw(), 0);
        assert!(p.usage().bytes < reserved.bytes);
        let mut a = egress(&ac, limits(1150), bindings(1), SendPolicy::default());
        let mut b = egress(&bc, limits(700), bindings(11), SendPolicy::default());
        update.distribute(&mut [&mut a, &mut b]).unwrap();
        let physical = p.usage();
        drop(update);
        assert_eq!(p.usage(), physical);
        a.close();
        assert_eq!(p.usage(), physical);
        b.close();
        assert_eq!(p.usage(), BudgetUsage::default());
        stop(&mut s, &cx).await;
    });
}

#[test]
fn unchanged_native_result_returns_unused_reservation_without_new_picture() {
    run_shared!(rt, cx, {
        let (owner, _) = gate(&rt, 1);
        let mut s = source(&owner, false).await;
        let p = reserved_pool(1);
        let first = s
            .prepare_shared_capture(&owner, &p)
            .unwrap()
            .capture_if_changed(true)
            .await
            .unwrap();
        let frame = first.frame();
        drop(first);
        let idle = s.prepare_shared_capture(&owner, &p).unwrap();
        assert_eq!(p.usage().pictures, 1);
        let idle = idle.capture_if_changed(false).await.unwrap();
        assert!(idle.is_unchanged());
        assert_eq!(idle.frame(), frame);
        assert_eq!(p.usage(), BudgetUsage::default());
        stop(&mut s, &cx).await;
    });
}

#[test]
fn recipient_preflight_uses_full_charge_and_never_backpressures_another_viewer() {
    run_shared!(rt, cx, {
        let (owner, _) = gate(&rt, 1);
        let (ac, _) = gate(&rt, 2);
        let (bc, bi) = gate(&rt, 3);
        let mut s = source(&owner, true).await;
        let p = reserved_pool(2);
        let first = s
            .prepare_shared_capture(&owner, &p)
            .unwrap()
            .capture_if_changed(true)
            .await
            .unwrap();
        let mut a = egress(&ac, limits(1150), bindings(1), SendPolicy::default());
        let mut b = egress(
            &bc,
            limits(700),
            bindings(11),
            SendPolicy {
                max_cached_bytes: 16 * 1024,
                repair_bytes_per_window: 4096,
                ..SendPolicy::default()
            },
        );
        first.distribute(&mut [&mut a, &mut b]).unwrap();
        let mut ar = receiver(&cx, limits(1150), bindings(1));
        let mut br = receiver(&cx, limits(700), bindings(11));
        pump(&cx, &mut a, &mut ar);
        pump(&cx, &mut b, &mut br);
        decoded(&cx, &mut ar);
        decoded(&cx, &mut br);
        let next = s.prepare_shared_capture(&owner, &p).unwrap();
        assert!(next.check_recipient(&mut a).unwrap());
        assert!(!next.check_recipient(&mut b).unwrap());
        // No picture has been lost by mere preflight: it does not revoke input.
        assert!(input_live(&cx, &bi));
        let update = next.capture_if_changed(false).await.unwrap();
        a.enqueue_shared_capture(&update).unwrap();
        pump(&cx, &mut a, &mut ar);
        assert_eq!(decoded(&cx, &mut ar).descriptor().frame, 1);
        stop(&mut s, &cx).await;
    });
}

#[test]
fn revoked_permission_between_reservation_and_poll_does_not_issue_native_capture() {
    run_shared!(rt, cx, {
        let (owner, _) = gate(&rt, 1);
        let mut s = source(&owner, true).await;
        let p = reserved_pool(1);
        let prepared = s.prepare_shared_capture(&owner, &p).unwrap();
        owner.revoke();
        assert!(prepared.capture_if_changed(true).await.is_err());
        assert_eq!(p.usage(), BudgetUsage::default());
        // The source was not borrowed by a native operation, so local cleanup
        // can stop the original still-running process without a poisoned retry.
        assert_eq!(s.worker_mut().state(), frd::worker::State::Running);
        stop(&mut s, &cx).await;
    });
}

#[test]
fn foreign_source_recipient_preflight_refuses_without_touching_live_input() {
    run_shared!(rt, cx, {
        let (owner, _) = gate(&rt, 1);
        let (viewer, input) = gate(&rt, 2);
        let mut s = source(&owner, true).await;
        let mut foreign = source(&owner, true).await;
        let p = reserved_pool(2);
        let update = foreign
            .prepare_shared_capture(&owner, &p)
            .unwrap()
            .capture_if_changed(true)
            .await
            .unwrap();
        let mut target = egress(&viewer, limits(1150), bindings(1), SendPolicy::default());
        target.enqueue_shared_capture(&update).unwrap();
        let charged = target.cache_usage();
        let prepared = s.prepare_shared_capture(&owner, &p).unwrap();
        assert_eq!(
            prepared.check_recipient(&mut target),
            Err(Error::InvalidFrame)
        );
        assert_eq!(target.cache_usage(), charged);
        assert!(input_live(&cx, &input));
        drop(prepared);
        stop(&mut s, &cx).await;
        stop(&mut foreign, &cx).await;
    });
}
