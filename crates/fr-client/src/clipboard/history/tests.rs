use super::*;

fn finished(submitted: u64) -> Snapshot {
    Snapshot {
        requested: true,
        configured: true,
        native_started: true,
        received_submitted: submitted,
        cleanup: Cleanup::Finished,
        collection: Collection::Finished,
        ..Snapshot::default()
    }
}

fn not_requested() -> Snapshot {
    Snapshot {
        collection: Collection::Finished,
        ..Snapshot::default()
    }
}

#[test]
fn every_original_owner_is_counted_including_unrequested_and_declined_lanes() {
    let mut history = History::default();
    history.retire(0, not_requested());
    history.retire(
        1,
        Snapshot {
            requested: true,
            ..not_requested()
        },
    );
    let completion = history.completion(2, finished(3));
    assert!(completion.known(3));
    assert!(!completion.covers(0));
    assert!(!completion.covers(2));
    assert!(!completion.covers(4));
    assert_eq!(completion.history.owners, 3);
    assert!(completion.snapshot.requested);
    assert!(completion.snapshot.configured);
    assert!(completion.snapshot.native_started);
    assert_eq!(completion.snapshot.received_submitted, 3);
    assert_eq!(completion.snapshot.cleanup, Cleanup::Finished);
}

#[test]
fn each_replacement_and_current_owner_contributes_its_actual_receipts() {
    let mut history = History::default();
    let mut first = finished(2);
    first.received_refused = 1;
    history.retire(0, first);
    history.retire(1, finished(3));
    let mut current = finished(5);
    current.received_refused = 4;
    let completion = history.completion(2, current);
    assert!(completion.known(3));
    assert_eq!(completion.snapshot.received_submitted, 10);
    assert_eq!(completion.snapshot.received_refused, 5);
    assert_eq!(completion.snapshot.received_unknown, 0);
}

#[test]
fn unknown_publication_in_any_owner_survives_later_clean_completion() {
    for unknown_owner in 0..3 {
        let mut history = History::default();
        for owner in 0..2 {
            let mut snapshot = finished(owner + 1);
            snapshot.received_unknown = u64::from(owner == unknown_owner);
            history.retire(owner, snapshot);
        }
        let mut current = finished(3);
        current.received_unknown = u64::from(unknown_owner == 2);
        let completion = history.completion(2, current);
        assert!(completion.covers(3), "{unknown_owner}");
        assert!(!completion.known(3), "{unknown_owner}");
        assert_eq!(completion.snapshot.received_unknown, 1, "{unknown_owner}");
        assert_eq!(completion.snapshot.received_submitted, 6);
    }
}

#[test]
fn every_unconfirmed_cleanup_remains_visible_after_a_clean_successor() {
    for cleanup in [Cleanup::Unknown, Cleanup::Pending, Cleanup::Failed] {
        for failed_owner in 0..3 {
            let mut history = History::default();
            for owner in 0..2 {
                let mut snapshot = finished(1);
                if owner == failed_owner {
                    snapshot.cleanup = cleanup;
                }
                history.retire(owner, snapshot);
            }
            let mut current = finished(1);
            if failed_owner == 2 {
                current.cleanup = cleanup;
            }
            let completion = history.completion(2, current);
            assert!(!completion.known(3), "{cleanup:?}, {failed_owner}");
            assert_eq!(completion.snapshot.cleanup, cleanup);
            assert_eq!(completion.snapshot.received_submitted, 3);
        }
    }
}

#[test]
fn failed_cleanup_is_not_repaired_by_unknown_pending_or_not_started_owners() {
    let mut history = History::default();
    let mut failed = finished(1);
    failed.cleanup = Cleanup::Failed;
    history.retire(0, failed);
    for (owner, cleanup) in [(1, Cleanup::Unknown), (2, Cleanup::Pending)] {
        let mut snapshot = finished(1);
        snapshot.cleanup = cleanup;
        history.retire(owner, snapshot);
    }
    let completion = history.completion(3, not_requested());
    assert_eq!(completion.snapshot.cleanup, Cleanup::Failed);
    assert!(!completion.known(4));
}

#[test]
fn final_snapshots_do_not_duplicate_the_current_owner_or_change_retired_evidence() {
    let mut history = History::default();
    history.retire(0, finished(2));
    let retained = history;
    let current = finished(3);
    let first = history.completion(1, current);
    for _ in 0..32 {
        assert_eq!(history.completion(1, current), first);
        assert_eq!(history, retained);
    }
    assert_eq!(first.snapshot.received_submitted, 5);
    // Only a real later replacement records that owner into retained history.
    history.retire(1, current);
    let last = history.completion(2, finished(7));
    assert!(last.known(3));
    assert_eq!(last.snapshot.received_submitted, 12);
}

#[test]
fn finished_cleanup_without_final_collection_cannot_certify_a_retired_owner() {
    let mut history = History::default();
    let mut original = finished(2);
    original.collection = Collection::Unconfirmed;
    history.retire(0, original);
    history.retire(1, finished(3));
    let completion = history.completion(2, finished(5));
    assert_eq!(completion.snapshot.cleanup, Cleanup::Finished);
    assert_eq!(completion.snapshot.collection, Collection::Unconfirmed);
    assert_eq!(completion.snapshot.received_submitted, 10);
    assert!(!completion.history.complete);
    assert!(!completion.known(3));
}

#[test]
fn current_owner_needs_its_positive_collection_before_a_final_receipt_can_be_known() {
    let mut history = History::default();
    history.retire(0, finished(2));
    let retained = history;
    let mut current = finished(3);
    current.collection = Collection::Unconfirmed;
    let incomplete = history.completion(1, current);
    assert_eq!(incomplete.snapshot.received_submitted, 5);
    assert!(!incomplete.covers(2));
    assert_eq!(history.completion(1, current), incomplete);
    assert_eq!(history, retained);
    // The same still-current owner can later supply positive final collection.
    // Reading its earlier snapshot never poisons the retired owners' history.
    current.collection = Collection::Finished;
    current.received_submitted = 4;
    let collected = history.completion(1, current);
    assert!(collected.known(2));
    assert_eq!(collected.snapshot.received_submitted, 6);
    // A late final unknown receipt is retained instead of certifying the earlier
    // finished-cleanup snapshot, which did not yet include this publication.
    current.received_unknown = 1;
    let unknown = history.completion(1, current);
    assert!(unknown.covers(2));
    assert!(!unknown.known(2));
    assert_eq!(unknown.snapshot.received_unknown, 1);
    assert_eq!(history, retained);
}

#[test]
fn failed_replacement_keeps_the_same_current_owner_and_coverage() {
    let mut history = History::default();
    history.retire(0, finished(1));
    let before = history.completion(1, finished(4));
    // A failed start, stale opening or cancelled native query calls no retire.
    assert_eq!(history.completion(1, finished(4)), before);
    history.retire(1, finished(4));
    let after = history.completion(2, finished(2));
    assert!(after.known(3));
    assert_eq!(after.snapshot.received_submitted, 7);
}

#[test]
fn duplicate_or_skipped_owners_invalidate_coverage_without_double_counting() {
    for invalid_owner in [0, 2, u64::MAX] {
        let mut history = History::default();
        history.retire(0, finished(2));
        history.retire(invalid_owner, finished(100));
        let completion = history.completion(1, finished(3));
        assert!(!completion.history.complete, "{invalid_owner}");
        assert!(!completion.known(2), "{invalid_owner}");
        assert_eq!(completion.history.owners, 2);
        assert_eq!(completion.snapshot.received_submitted, 5);
    }
}

#[test]
fn publication_counter_saturation_is_explicit_and_cannot_certify_completion() {
    for which in 0..3 {
        let mut history = History::default();
        let mut first = finished(0);
        let mut second = finished(0);
        match which {
            0 => {
                first.received_submitted = u64::MAX - 1;
                second.received_submitted = 2;
            }
            1 => {
                first.received_unknown = u64::MAX - 1;
                second.received_unknown = 2;
            }
            _ => {
                first.received_refused = u64::MAX - 1;
                second.received_refused = 2;
            }
        }
        history.retire(0, first);
        let completion = history.completion(1, second);
        assert!(completion.history.overflowed, "{which}");
        assert!(!completion.history.complete, "{which}");
        assert!(!completion.covers(2), "{which}");
        let counts = [
            completion.snapshot.received_submitted,
            completion.snapshot.received_unknown,
            completion.snapshot.received_refused,
        ];
        assert_eq!(counts[which], u64::MAX);
    }
    // Original per-owner counters also saturate. Their ceiling is no proof
    // that precisely that many effects occurred before the final snapshot.
    let saturated = History::default().completion(0, finished(u64::MAX));
    assert!(saturated.history.overflowed);
    assert!(!saturated.known(1));
    let mut exact_ceiling = History::default();
    exact_ceiling.retire(0, finished(u64::MAX - 1));
    assert!(exact_ceiling.completion(1, finished(1)).history.overflowed);
}

#[test]
fn owner_count_cannot_wrap_into_plausible_empty_history() {
    let mut history = History::default();
    history.0.history.owners = u64::MAX;
    let completion = history.completion(u64::MAX, not_requested());
    assert_eq!(completion.history.owners, u64::MAX);
    assert!(completion.history.overflowed);
    assert!(!completion.covers(u64::MAX));
    assert!(!completion.covers(0));
}

#[test]
fn inconsistent_original_evidence_cannot_be_hidden_by_later_aggregate_flags() {
    for first in [
        Snapshot {
            configured: true,
            ..not_requested()
        },
        Snapshot {
            requested: true,
            received_submitted: 1,
            ..not_requested()
        },
        Snapshot {
            requested: true,
            configured: true,
            native_started: true,
            ..not_requested()
        },
    ] {
        let mut history = History::default();
        history.retire(0, first);
        let completion = history.completion(1, finished(2));
        assert!(!completion.history.complete);
        assert!(!completion.known(2));
    }
}

#[test]
fn document_round_trip_requires_typed_complete_supported_coverage() {
    let completion = History::default().completion(0, finished(2));
    let mut document = serde_json::to_value(completion).unwrap();
    // Existing platform reason metadata contains no clipboard payload and
    // does not replace typed coverage or cleanup.
    document["reason"] = "Controller(Stopped)".into();
    assert_eq!(Completion::from_document(&document).unwrap(), completion);
    for value in [
        serde_json::Value::Null,
        true.into(),
        "1".into(),
        serde_json::json!([]),
    ] {
        document["history"] = value;
        assert!(Completion::from_document(&document).is_err());
    }
    let mut document = serde_json::to_value(completion).unwrap();
    document["history"]["schema_version"] = 2.into();
    assert!(!Completion::from_document(&document).unwrap().known(1));
    document["history"]
        .as_object_mut()
        .unwrap()
        .remove("complete");
    assert!(Completion::from_document(&document).is_err());
    let mut document = serde_json::to_value(completion).unwrap();
    for collection in [
        serde_json::Value::Null,
        true.into(),
        "finished_later".into(),
    ] {
        document["collection"] = collection;
        assert!(Completion::from_document(&document).is_err());
    }
    document.as_object_mut().unwrap().remove("collection");
    assert!(Completion::from_document(&document).is_err());
}

#[test]
fn long_history_has_fixed_storage_and_retains_every_reported_receipt() {
    let mut history = History::default();
    for owner in 0..10_000 {
        history.retire(owner, finished(1));
    }
    let completion = history.completion(10_000, finished(1));
    assert!(completion.known(10_001));
    assert_eq!(completion.snapshot.received_submitted, 10_001);
    assert!(std::mem::size_of::<History>() <= 128);
    assert!(serde_json::to_string(&completion).unwrap().len() < 512);
}
