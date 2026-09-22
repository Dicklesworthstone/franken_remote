//! Actual source IPC and the original encoder admission clock; synthetic media.
#![cfg(target_os = "linux")]
#[path = "shared_startup/support.rs"]
#[allow(dead_code)]
mod support;
use asupersync::cx::Cx;
use fr_media::delivery::BudgetUsage;
use frd::media::host_now;
use support::*;

macro_rules! run_join {
    ($rt:ident, $cx:ident, $body:expr) => {{
        let $rt = runtime();
        $rt.block_on(async {
            let $cx = Cx::current().unwrap();
            $body
        });
    }};
}

#[test]
fn late_join_idrs_are_throttled_without_stalling_healthy_dependent_output() {
    run_join!(rt, cx, {
        let owner = gate(&rt, 1);
        let mut source = source_variant(&owner, true, true, false).await;
        let pool = pool();
        let until = host_now(&cx).unwrap().as_micros() + 2_000_000;
        let first = source
            .prepare_shared_capture(&owner, &pool)
            .unwrap()
            .capture_for_join(until)
            .await
            .unwrap();
        assert!(first.encoded().unwrap().kind().is_idr());
        let issued = first.observed_micros();
        let mut previous = first.frame();
        drop(first);
        // Repeated requests have no extra encoder allowance. Actual dependent
        // frames keep their original source identity/reference sequence.
        for _ in 0..4 {
            let next = source
                .prepare_shared_capture(&owner, &pool)
                .unwrap()
                .capture_for_join(until)
                .await
                .unwrap();
            assert!(next.observed_micros() < issued + 500_000);
            assert_eq!(
                next.encoded().unwrap().kind(),
                fr_media::access_unit::FrameKind::Predicted {
                    references: previous
                }
            );
            previous = next.frame();
        }
        asupersync::time::sleep_until(asupersync::types::Time::from_nanos(
            (issued + 510_000) * 1000,
        ))
        .await;
        let later = source
            .prepare_shared_capture(&owner, &pool)
            .unwrap()
            .capture_for_join(until)
            .await
            .unwrap();
        assert!(later.encoded().unwrap().kind().is_idr());
        assert_eq!(later.frame().as_raw(), previous.as_raw() + 1);
        drop(later);
        assert_eq!(pool.usage(), BudgetUsage::default());
        stop(&mut source, &cx).await;
    });
}

#[test]
fn expired_join_and_unused_preparation_cannot_consume_rate_or_poison_static_capture() {
    run_join!(rt, cx, {
        let owner = gate(&rt, 1);
        let mut source = source(&owner, true).await;
        let pool = pool();
        let first = source
            .prepare_shared_capture(&owner, &pool)
            .unwrap()
            .capture_if_changed(true)
            .await
            .unwrap();
        drop(first);
        let expired = source
            .prepare_shared_capture(&owner, &pool)
            .unwrap()
            .capture_for_join(0)
            .await
            .unwrap();
        assert!(expired.is_unchanged());
        let until = host_now(&cx).unwrap().as_micros() + 2_000_000;
        drop(
            source
                .prepare_shared_capture(&owner, &pool)
                .unwrap()
                .capture_for_join(until),
        );
        let fresh = source
            .prepare_shared_capture(&owner, &pool)
            .unwrap()
            .capture_for_join(until)
            .await
            .unwrap();
        assert!(fresh.encoded().unwrap().kind().is_idr());
        drop(fresh);
        let rate_limited = source
            .prepare_shared_capture(&owner, &pool)
            .unwrap()
            .capture_for_join(until)
            .await
            .unwrap();
        assert!(rate_limited.is_unchanged());
        assert_eq!(pool.usage(), BudgetUsage::default());
        stop(&mut source, &cx).await;
    });
}

#[test]
fn expiring_newcomer_cannot_abort_a_healthy_shared_native_capture() {
    run_join!(rt, cx, {
        let owner = gate(&rt, 1);
        let mut source = source_variant(&owner, true, true, true).await;
        let pool = pool();
        let until = host_now(&cx).unwrap().as_micros() + 10_000;
        // The original native fixture deliberately takes longer than the join
        // has left. Its output still belongs to the healthy source; the caller
        // must refuse this expired join, not discard/abort everyone's encoder.
        let frame = source
            .prepare_shared_capture(&owner, &pool)
            .unwrap()
            .capture_for_join(until)
            .await
            .unwrap();
        assert!(host_now(&cx).unwrap().as_micros() >= until);
        assert!(frame.encoded().unwrap().kind().is_idr());
        let reference = frame.frame();
        drop(frame);
        let next = source
            .prepare_shared_capture(&owner, &pool)
            .unwrap()
            .capture_if_changed(false)
            .await
            .unwrap();
        assert_eq!(
            next.encoded().unwrap().kind(),
            fr_media::access_unit::FrameKind::Predicted {
                references: reference
            }
        );
        drop(next);
        assert_eq!(pool.usage(), BudgetUsage::default());
        stop(&mut source, &cx).await;
    });
}
