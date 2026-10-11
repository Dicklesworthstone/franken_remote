//! Deterministic reservation tests; no allocator or native decoder timing claims.
use super::{BudgetUsage, DeliveryError, Ledger, MediaBudget};
use std::sync::{Arc, Mutex};

fn budget(bytes: usize, pictures: usize) -> MediaBudget {
    MediaBudget(Arc::new(Mutex::new(Ledger {
        used: BudgetUsage::default(),
        max_bytes: bytes,
        max_pictures: pictures,
    })))
}

#[test]
fn individually_impossible_picture_is_not_recoverable_pressure() {
    let budget = budget(100, 2);
    assert!(matches!(
        budget.reserve(101),
        Err(DeliveryError::ResourceLimit)
    ));
    assert_eq!(budget.usage(), BudgetUsage::default());
    let held = budget.reserve(100).unwrap();
    assert!(matches!(
        budget.reserve(101),
        Err(DeliveryError::ResourceLimit)
    ));
    assert!(matches!(
        budget.reserve(1),
        Err(DeliveryError::ReferenceCapacityExceeded)
    ));
    assert_eq!(
        budget.usage(),
        BudgetUsage {
            bytes: 100,
            pictures: 1
        }
    );
    drop(held);
    assert_eq!(budget.usage(), BudgetUsage::default());
}

#[test]
fn aggregate_bytes_are_shared_and_refusal_does_not_allocate_or_refund() {
    let budget = budget(100, 4);
    let replacement = budget.clone();
    let old = budget.reserve(60).unwrap();
    let current = replacement.reserve(30).unwrap();
    assert!(matches!(
        replacement.reserve(20),
        Err(DeliveryError::ReferenceCapacityExceeded)
    ));
    assert_eq!(
        budget.usage(),
        BudgetUsage {
            bytes: 90,
            pictures: 2
        }
    );
    drop(current);
    assert_eq!(
        replacement.usage(),
        BudgetUsage {
            bytes: 60,
            pictures: 1
        }
    );
    let next = replacement.reserve(40).unwrap();
    assert_eq!(
        budget.usage(),
        BudgetUsage {
            bytes: 100,
            pictures: 2
        }
    );
    drop(old);
    assert_eq!(
        replacement.usage(),
        BudgetUsage {
            bytes: 40,
            pictures: 1
        }
    );
    drop(next);
    assert_eq!(budget.usage(), BudgetUsage::default());
}

#[test]
fn picture_count_is_independent_of_byte_headroom() {
    let budget = budget(1_000, 2);
    let first = budget.reserve(1).unwrap();
    let second = budget.reserve(1).unwrap();
    assert!(matches!(
        budget.clone().reserve(1),
        Err(DeliveryError::ReferenceCapacityExceeded)
    ));
    assert_eq!(
        budget.usage(),
        BudgetUsage {
            bytes: 2,
            pictures: 2
        }
    );
    drop(first);
    let third = budget.reserve(1).unwrap();
    drop(second);
    drop(third);
    assert_eq!(budget.usage(), BudgetUsage::default());
}

#[test]
fn refused_growth_keeps_both_reservations_and_exact_original_charges() {
    let budget = budget(100, 3);
    let old = budget.reserve(50).unwrap();
    let mut growing = budget.reserve(40).unwrap();
    assert_eq!(
        growing.grow(20),
        Err(DeliveryError::ReferenceCapacityExceeded)
    );
    assert_eq!(growing.bytes, 40);
    assert_eq!(
        budget.usage(),
        BudgetUsage {
            bytes: 90,
            pictures: 2
        }
    );
    assert_eq!(growing.grow(61), Err(DeliveryError::ResourceLimit));
    assert_eq!(growing.bytes, 40);
    assert_eq!(
        budget.usage(),
        BudgetUsage {
            bytes: 90,
            pictures: 2
        }
    );
    drop(old);
    growing.grow(60).unwrap();
    growing.grow(0).unwrap();
    assert_eq!(
        budget.usage(),
        BudgetUsage {
            bytes: 100,
            pictures: 1
        }
    );
    drop(growing);
    assert_eq!(budget.usage(), BudgetUsage::default());
}

#[test]
fn overflow_and_invalid_allocation_sizes_do_not_become_recovery_requests() {
    let budget = budget(100, 2);
    assert!(matches!(
        budget.allocate(usize::MAX, 1),
        Err(DeliveryError::ResourceLimit)
    ));
    let mut held = budget.reserve(50).unwrap();
    assert_eq!(held.grow(usize::MAX), Err(DeliveryError::ResourceLimit));
    assert_eq!(held.bytes, 50);
    assert_eq!(
        budget.usage(),
        BudgetUsage {
            bytes: 50,
            pictures: 1
        }
    );
    drop(held);
    assert_eq!(budget.usage(), BudgetUsage::default());
}

#[test]
fn one_viewers_pressure_does_not_spend_another_viewers_capacity() {
    let pressured = budget(100, 2);
    let healthy = budget(100, 2);
    let held = pressured.reserve(100).unwrap();
    assert!(matches!(
        pressured.reserve(1),
        Err(DeliveryError::ReferenceCapacityExceeded)
    ));
    let independent = healthy.reserve(100).unwrap();
    assert_eq!(
        healthy.usage(),
        BudgetUsage {
            bytes: 100,
            pictures: 1
        }
    );
    drop(held);
    assert_eq!(
        healthy.usage(),
        BudgetUsage {
            bytes: 100,
            pictures: 1
        }
    );
    drop(independent);
    assert_eq!(healthy.usage(), BudgetUsage::default());
}
