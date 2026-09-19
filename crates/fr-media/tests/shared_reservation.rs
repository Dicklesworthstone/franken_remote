//! Real pool ownership and output allocations; no codec qualification claim.
use fr_core::{
    ids::*,
    limits::{LimitOverrides, ProtocolLimits},
};
use fr_media::{
    access_unit::{EncodedAccessUnit, FrameId, FrameKind},
    delivery::{BudgetUsage, DeliveryError, SharedFramePool},
};
use std::sync::{
    Arc, Barrier,
    atomic::{AtomicUsize, Ordering},
};

fn unit(capacity: usize, length: usize) -> EncodedAccessUnit {
    let mut bytes = Vec::with_capacity(capacity);
    bytes.resize(length, 7);
    EncodedAccessUnit::new(
        &ProtocolLimits::ABSOLUTE,
        FrameId::from_raw(9),
        FrameKind::Idr {
            recovery: RecoveryGeneration::INITIAL,
        },
        CodecConfigurationGeneration::INITIAL,
        42,
        bytes,
    )
    .unwrap()
}
fn pool(bytes: usize, pictures: usize) -> SharedFramePool {
    SharedFramePool::new(ProtocolLimits::ABSOLUTE, bytes, pictures).unwrap()
}

#[test]
fn reserved_output_transfers_without_reacquiring_a_full_slot_or_copying_bytes() {
    let pool = pool(8192, 1);
    let permit = pool.reserve_capacity(4096).unwrap();
    assert_eq!(pool.usage().pictures, 1);
    assert_eq!(pool.usage().bytes, permit.charged_bytes());
    assert_eq!(permit.maximum_capacity(), 4096);
    assert!(pool.reserve_capacity(1).is_err());
    assert!(pool.share(unit(1, 1)).is_err());
    let original = unit(100, 20);
    let pointer = original.bytes().as_ptr();
    let frame = permit.share(original).unwrap();
    assert_eq!(pointer, frame.bytes().as_ptr());
    assert_eq!(frame.frame(), FrameId::from_raw(9));
    assert_eq!(frame.capture_micros(), 42);
    assert!(frame.kind().is_idr());
    assert_eq!(pool.usage().bytes, frame.allocation_charge());
    assert!(pool.usage().bytes < 4096);
    let retained = frame.clone();
    drop(frame);
    assert_eq!(pool.usage().pictures, 1);
    drop(retained);
    assert_eq!(pool.usage(), BudgetUsage::default());
}

#[test]
fn oversized_capacity_refuses_even_when_length_fits_and_returns_all_credit() {
    let pool = pool(8192, 2);
    let permit = pool.reserve_capacity(64).unwrap();
    assert_eq!(
        permit.share(unit(128, 1)).unwrap_err(),
        DeliveryError::ResourceLimit
    );
    assert_eq!(pool.usage(), BudgetUsage::default());
    let permit = pool.reserve_capacity(128).unwrap();
    assert!(permit.share(unit(128, 128)).is_ok());
    assert_eq!(pool.usage(), BudgetUsage::default());
}

#[test]
fn pool_limits_still_govern_outputs_created_under_a_wider_codec_profile() {
    let limits = ProtocolLimits::with_overrides(LimitOverrides {
        max_encoded_access_unit_bytes: Some(64),
        ..LimitOverrides::default()
    })
    .unwrap();
    let pool = SharedFramePool::new(limits, 8192, 2).unwrap();
    let permit = pool.reserve_capacity(256).unwrap();
    assert_eq!(
        permit.share(unit(128, 128)).unwrap_err(),
        DeliveryError::ResourceLimit
    );
    assert_eq!(pool.usage(), BudgetUsage::default());
}

#[test]
fn abandoned_reservations_and_impossible_requests_do_not_leak_or_invent_credit() {
    let pool = pool(8192, 2);
    for capacity in [0, usize::MAX, pool.maximum_capacity() + 1] {
        assert_eq!(
            pool.reserve_capacity(capacity).unwrap_err(),
            DeliveryError::ResourceLimit
        );
        assert_eq!(pool.usage(), BudgetUsage::default());
    }
    let maximum = pool.maximum_capacity();
    let permit = pool.reserve_capacity(maximum).unwrap();
    assert_eq!(pool.usage().bytes, 8192);
    assert!(pool.reserve_capacity(1).is_err());
    drop(permit);
    assert_eq!(pool.usage(), BudgetUsage::default());
    let clone = pool.clone();
    let retained = clone.reserve_capacity(maximum).unwrap();
    drop(clone);
    assert_eq!(pool.usage().bytes, 8192);
    drop(retained);
    assert_eq!(pool.usage(), BudgetUsage::default());
}

#[test]
fn shrinking_to_actual_capacity_returns_bytes_but_never_an_occupied_frame_slot() {
    let pool = pool(4096, 2);
    let first = pool.reserve_capacity(3000).unwrap();
    let second = pool.reserve_capacity(256).unwrap();
    let before = pool.usage();
    let frame = first.share(unit(100, 10)).unwrap();
    assert_eq!(pool.usage().bytes, before.bytes - 2900);
    assert_eq!(pool.usage().pictures, 2);
    assert!(pool.reserve_capacity(1).is_err());
    drop(second);
    let next = pool.reserve_capacity(3000).unwrap();
    assert_eq!(pool.usage().pictures, 2);
    drop(next);
    drop(frame);
    assert_eq!(pool.usage(), BudgetUsage::default());
}

#[test]
fn concurrent_clones_cannot_admit_the_same_picture_credit_twice() {
    let pool = pool(8192, 1);
    let begin = Arc::new(Barrier::new(8));
    let admitted = Arc::new(Barrier::new(8));
    let release = Arc::new(Barrier::new(8));
    let winners = Arc::new(AtomicUsize::new(0));
    std::thread::scope(|scope| {
        for _ in 0..8 {
            let pool = pool.clone();
            let (begin, admitted, release, winners) = (
                begin.clone(),
                admitted.clone(),
                release.clone(),
                winners.clone(),
            );
            scope.spawn(move || {
                begin.wait();
                let result = pool.reserve_capacity(512);
                if result.is_ok() {
                    winners.fetch_add(1, Ordering::SeqCst);
                }
                admitted.wait();
                assert_eq!(winners.load(Ordering::SeqCst), 1);
                assert_eq!(pool.usage().pictures, 1);
                release.wait();
                drop(result);
            });
        }
    });
    assert_eq!(pool.usage(), BudgetUsage::default());
}
