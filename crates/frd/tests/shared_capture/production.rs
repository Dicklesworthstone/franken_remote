//! Actual native IPC is gated by the physical reservation; no codec qualification.
use super::*;
use fr_core::limits::LimitOverrides;
use frd::worker::State;
use std::{
    future::{Future, poll_fn},
    pin::pin,
    task::Poll,
};

async fn capture_reserved(
    source: &mut CaptureSource,
    control: &ObservationControl,
    pool: &SharedFramePool,
    force_idr: bool,
) -> Result<SharedCaptureUpdate, Error> {
    source
        .prepare_shared_capture(control, pool)?
        .capture_if_changed(force_idr)
        .await
}

fn physical(pictures: usize) -> SharedFramePool {
    SharedFramePool::new(ProtocolLimits::ABSOLUTE, 2 * 1024 * 1024, pictures).unwrap()
}
async fn delayed(c: &ObservationControl, mode: &str) -> CaptureSource {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let file = std::env::temp_dir().join(format!(
        "fr-shared-production-{}-{}.py",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(
        &file,
        include_str!("../support/shared_production_fixture.py").replace("@MODE@", mode),
    )
    .unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o700)).unwrap();
    CaptureSource::start(
        c,
        Launch::new(&file, ":0", None, Role::Capture, 77).unwrap(),
        Configuration {
            width: 320,
            height: 240,
            fps: 30,
            backend: Backend::SoftwareExplicit,
            bitrate: 2_000_000,
            max_access_unit_bytes: 1024 * 1024,
            generation: epoch().configuration,
        },
    )
    .await
    .unwrap()
}
async fn pending(
    future: std::pin::Pin<&mut impl Future<Output = Result<SharedCaptureUpdate, Error>>>,
) {
    let mut future = future;
    poll_fn(|task| match future.as_mut().poll(task) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(_) => panic!("real child exchange must be pending"),
    })
    .await;
}

#[test]
fn reservation_precedes_ipc_and_prevents_a_second_native_producer_from_overbooking() {
    run_shared!(rt, cx, {
        let (owner, _) = gate(&rt, 1);
        let mut a = delayed(&owner, "delay").await;
        let mut b = source(&owner, true).await;
        let pool = physical(1);
        let clone = pool.clone();
        let first = {
            let mut capture = pin!(capture_reserved(&mut a, &owner, &pool, false));
            pending(capture.as_mut()).await;
            let reserved = pool.usage();
            assert_eq!(reserved.pictures, 1);
            assert!(reserved.bytes > 1024 * 1024 + fr_media::worker::UNIT_PREFIX_BYTES);
            assert_eq!(
                capture_reserved(&mut b, &owner, &clone, false)
                    .await
                    .unwrap_err(),
                Error::Backpressure
            );
            assert_eq!(pool.usage(), reserved);
            capture.await.unwrap()
        };
        assert_eq!(pool.usage().pictures, 1);
        assert!(pool.usage().bytes < 8192);
        assert_eq!(first.frame().as_raw(), 0);
        drop(first);
        assert_eq!(pool.usage(), BudgetUsage::default());
        let second = capture_reserved(&mut b, &owner, &clone, false)
            .await
            .unwrap();
        // Refusal sent no capture, so both the protocol and codec chain begin here.
        assert_eq!(second.frame().as_raw(), 0);
        assert!(matches!(
            second.encoded().unwrap().kind(),
            fr_media::access_unit::FrameKind::Idr { .. }
        ));
        drop(second);
        assert_eq!(pool.usage(), BudgetUsage::default());
        stop(&mut a, &cx).await;
        stop(&mut b, &cx).await;
    });
}

#[test]
fn impossible_profiles_refuse_before_capture_even_when_the_child_would_return_small_output() {
    run_shared!(rt, cx, {
        let (owner, _) = gate(&rt, 1);
        let mut s = source(&owner, true).await;
        let small_bytes = SharedFramePool::new(ProtocolLimits::ABSOLUTE, 8192, 2).unwrap();
        let small_payload = SharedFramePool::new(
            ProtocolLimits::with_overrides(LimitOverrides {
                max_encoded_access_unit_bytes: Some(4096),
                ..LimitOverrides::default()
            })
            .unwrap(),
            2 * 1024 * 1024,
            2,
        )
        .unwrap();
        for pool in [&small_bytes, &small_payload] {
            assert_eq!(
                capture_reserved(&mut s, &owner, pool, false)
                    .await
                    .unwrap_err(),
                Error::InvalidFrame
            );
            assert_eq!(pool.usage(), BudgetUsage::default());
            owner.check().unwrap();
        }
        let pool = physical(1);
        let first = capture_reserved(&mut s, &owner, &pool, false)
            .await
            .unwrap();
        assert_eq!(first.frame().as_raw(), 0);
        drop(first);
        stop(&mut s, &cx).await;
    });
}

#[test]
fn polls_keep_one_reservation_and_published_allocation_includes_the_removed_prefix() {
    run_shared!(rt, cx, {
        let (owner, _) = gate(&rt, 1);
        let mut s = delayed(&owner, "poll").await;
        let pool = physical(1);
        let metadata = {
            let credit = pool.reserve_capacity(1).unwrap();
            credit.charged_bytes() - 1
        };
        let frame = {
            let mut capture = pin!(capture_reserved(&mut s, &owner, &pool, false));
            pending(capture.as_mut()).await;
            assert_eq!(pool.usage().pictures, 1);
            capture.await.unwrap()
        };
        assert_eq!(frame.frame().as_raw(), 0);
        let encoded = frame.encoded().unwrap();
        assert_eq!(encoded.bytes().len(), 3200);
        assert_eq!(
            pool.usage().bytes,
            3200 + fr_media::worker::UNIT_PREFIX_BYTES + metadata
        );
        assert_eq!(encoded.allocation_charge(), pool.usage().bytes);
        let alias = frame.clone();
        assert!(encoded.shares_storage_with(alias.encoded().unwrap()));
        drop(frame);
        assert_eq!(pool.usage().pictures, 1);
        drop(alias);
        assert_eq!(pool.usage(), BudgetUsage::default());
        stop(&mut s, &cx).await;
    });
}

#[test]
fn cancelled_pending_capture_releases_host_credit_and_retains_poisoned_child_for_reaping() {
    run_shared!(rt, cx, {
        let (owner, _) = gate(&rt, 1);
        let mut s = delayed(&owner, "delay").await;
        let pool = physical(1);
        let id = s.worker_id();
        {
            let mut capture = pin!(capture_reserved(&mut s, &owner, &pool, false));
            pending(capture.as_mut()).await;
            assert_eq!(pool.usage().pictures, 1);
        }
        assert_eq!(pool.usage(), BudgetUsage::default());
        assert_eq!(s.worker_id(), id);
        let w = s.worker_mut();
        assert_eq!(w.state(), State::Poisoned);
        w.reap(&cx, Deadline::after(&cx, Duration::from_secs(1)).unwrap())
            .await
            .unwrap();
        assert_eq!(w.state(), State::Reaped);
    });
}

#[test]
fn reserved_static_capture_refunds_only_its_slot_without_unpinning_other_viewers() {
    run_shared!(rt, cx, {
        let (owner, _) = gate(&rt, 1);
        let mut s = source(&owner, false).await;
        let pool = physical(2);
        let first = capture_reserved(&mut s, &owner, &pool, false)
            .await
            .unwrap();
        let used = pool.usage();
        let idle = capture_reserved(&mut s, &owner, &pool, false)
            .await
            .unwrap();
        assert!(idle.is_unchanged());
        assert_eq!(idle.frame(), first.frame());
        assert_eq!(pool.usage(), used);
        drop(first);
        assert_eq!(pool.usage(), BudgetUsage::default());
        assert!(idle.encoded().is_none());
        stop(&mut s, &cx).await;
    });
}

#[test]
fn reserved_capture_distributes_one_allocation_to_real_independent_delivery_pipelines() {
    run_shared!(rt, cx, {
        let (owner, _) = gate(&rt, 1);
        let (ac, _) = gate(&rt, 2);
        let (bc, _) = gate(&rt, 3);
        let mut s = source(&owner, true).await;
        let pid = s.worker_id();
        let pool = physical(2);
        let mut a = egress(&ac, limits(1150), bindings(1), SendPolicy::default());
        let mut b = egress(&bc, limits(700), bindings(11), SendPolicy::default());
        let mut ar = receiver(&cx, limits(1150), bindings(1));
        let mut br = receiver(&cx, limits(700), bindings(11));
        for frame in 0..2 {
            let result = capture_reserved(&mut s, &owner, &pool, false)
                .await
                .unwrap();
            let usage = pool.usage();
            assert_eq!(
                result.distribute(&mut [&mut a, &mut b]).unwrap().results(),
                &[Ok(()), Ok(())]
            );
            assert_eq!(pool.usage(), usage);
            drop(result);
            pump(&cx, &mut a, &mut ar);
            pump(&cx, &mut b, &mut br);
            assert_eq!(decoded(&cx, &mut ar).descriptor().frame, frame);
            assert_eq!(decoded(&cx, &mut br).descriptor().frame, frame);
        }
        a.close();
        assert_eq!(pool.usage().pictures, 2);
        b.close();
        assert_eq!(pool.usage(), BudgetUsage::default());
        assert_eq!(s.worker_id(), pid);
        stop(&mut s, &cx).await;
    });
}

#[test]
fn physical_backpressure_preserves_the_queued_recovery_and_original_failure_deadline() {
    run_shared!(rt, cx, {
        let (owner, _) = gate(&rt, 1);
        let (viewer, input) = gate(&rt, 2);
        let mut s = source(&owner, false).await;
        let pid = s.worker_id();
        let pool = physical(1);
        let mut sub = Subscription::new(
            viewer.clone(),
            limits(1150),
            bindings(1),
            epoch(),
            SendPolicy::default(),
        )
        .unwrap();
        let initial = capture_reserved(&mut s, &owner, &pool, false)
            .await
            .unwrap();
        sub.enqueue_shared_capture(&initial).unwrap();
        drop(initial);
        let binding = Binding {
            parent: ControlBinding {
                id: 10,
                host_boot: HostBootId::from_raw(1),
                os_session: OsSessionId::from_raw(2),
                remote_session: RemoteSessionId::from_raw(2),
            },
            display: 4,
            geometry: DisplayGeometryGeneration::INITIAL,
            configuration: epoch().configuration,
            recovery: epoch().recovery,
            viewport: ViewportMappingGeneration::INITIAL,
        };
        let mut bytes = [0; recovery_request::REQUEST_BYTES];
        recovery_request::encode(
            Request {
                reason: Reason::ReferenceExpired,
                last_useful_frame: Some(0),
            },
            binding,
            &ProtocolLimits::ABSOLUTE,
            &mut bytes,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )
        .unwrap();
        assert!(sub.request_recovery(&mut s, &bytes, binding).unwrap());
        let failed_until = sub.next_deadline();
        let queued = s.next_recovery_deadline();
        assert!(queued.is_some());
        assert_eq!(pool.usage(), BudgetUsage::default());
        let blocker = pool.reserve_capacity(1).unwrap();
        for _ in 0..32 {
            assert_eq!(
                capture_reserved(&mut s, &owner, &pool, false)
                    .await
                    .unwrap_err(),
                Error::Backpressure
            );
            assert_eq!(s.next_recovery_deadline(), queued);
            assert_eq!(sub.next_deadline(), failed_until);
        }
        drop(blocker);
        // No explicit force flag: the unchanged native source consumes the original demand.
        let recovered = capture_reserved(&mut s, &owner, &pool, false)
            .await
            .unwrap();
        assert!(matches!(
            recovered.encoded().unwrap().kind(),
            fr_media::access_unit::FrameKind::Idr { .. }
        ));
        assert_eq!(recovered.frame().as_raw(), 1);
        assert_eq!(s.next_recovery_deadline(), None);
        assert_eq!(sub.next_deadline(), failed_until);
        assert!(!input_live(&cx, &input));
        sub.recover(
            MediaEpoch {
                recovery: epoch().recovery.next().unwrap(),
                ..epoch()
            },
            bindings(21),
        )
        .unwrap();
        sub.enqueue_shared_capture(&recovered).unwrap();
        assert_eq!(s.worker_id(), pid);
        drop(recovered);
        drop(sub);
        assert_eq!(pool.usage(), BudgetUsage::default());
        stop(&mut s, &cx).await;
    });
}

#[test]
fn revoked_capture_permission_does_not_borrow_credit_or_mutate_a_healthy_source() {
    run_shared!(rt, cx, {
        let (owner, _) = gate(&rt, 1);
        let (denied, _) = gate(&rt, 2);
        let mut s = source(&owner, true).await;
        let pool = physical(1);
        denied.revoke();
        assert!(
            capture_reserved(&mut s, &denied, &pool, false)
                .await
                .is_err()
        );
        assert_eq!(pool.usage(), BudgetUsage::default());
        let first = capture_reserved(&mut s, &owner, &pool, false)
            .await
            .unwrap();
        assert_eq!(first.frame().as_raw(), 0);
        stop(&mut s, &cx).await;
    });
}
