use super::*;

fn job(ranges: &[RepairRange]) -> RepairJob {
    let mut all = [RepairRange { start: 0, end: 0 }; 64];
    all[..ranges.len()].copy_from_slice(ranges);
    RepairJob {
        ranges: all,
        count: ranges.len(),
        range: 0,
        fragment: ranges[0].start,
    }
}
fn ranges(mask: u16) -> Vec<RepairRange> {
    let mut out = Vec::new();
    let mut index = 0;
    while index < 8 {
        if mask & (1 << index) == 0 {
            index += 1;
            continue;
        }
        let start = index;
        while index < 8 && mask & (1 << index) != 0 {
            index += 1;
        }
        out.push(RepairRange { start, end: index });
    }
    out
}
fn mask(job: &RepairJob) -> u16 {
    job.remaining().fold(0, |mask, r| {
        (r.start..r.end).fold(mask, |mask, bit| mask | (1 << bit))
    })
}

#[test]
fn every_small_missing_set_union_is_exact_and_duplicates_are_not_new_rounds() {
    for original in 1..=255_u16 {
        let pending = job(&ranges(original));
        for requested in 1..=255_u16 {
            let merged = pending.coalesce(&ranges(requested), 64);
            if requested & !original == 0 {
                assert!(matches!(merged, Err(SendError::RepairBusy)));
            } else {
                let merged = merged.unwrap();
                assert_eq!(mask(&merged), original | requested);
                assert!(merged.count <= 4);
                assert_eq!(merged.range, 0);
            }
            assert_eq!(mask(&pending), original);
        }
    }
}

#[test]
fn emitted_ranges_and_partial_prefixes_are_not_implicitly_reintroduced() {
    let mut pending = job(&[
        RepairRange { start: 0, end: 2 },
        RepairRange { start: 4, end: 7 },
    ]);
    pending.range = 1;
    pending.fragment = 5;
    let merged = pending
        .coalesce(&[RepairRange { start: 6, end: 8 }], 64)
        .unwrap();
    assert_eq!(
        merged.remaining().collect::<Vec<_>>(),
        [RepairRange { start: 5, end: 8 }]
    );
    assert_eq!((pending.range, pending.fragment), (1, 5));
    // Retransmitting an emitted prefix requires an explicit new request.
    let again = pending
        .coalesce(&[RepairRange { start: 0, end: 1 }], 64)
        .unwrap();
    assert_eq!(
        again.remaining().collect::<Vec<_>>(),
        [
            RepairRange { start: 0, end: 1 },
            RepairRange { start: 5, end: 7 },
        ]
    );
}

#[test]
fn adjacent_normalization_alone_is_still_a_duplicate() {
    let pending = job(&[
        RepairRange { start: 0, end: 1 },
        RepairRange { start: 1, end: 2 },
    ]);
    assert!(matches!(
        pending.coalesce(&[RepairRange { start: 0, end: 2 }], 64),
        Err(SendError::RepairBusy)
    ));
}

#[test]
fn range_capacity_preserves_new_losses_with_the_smallest_possible_bridge() {
    let pending = job(&[
        RepairRange { start: 0, end: 1 },
        RepairRange { start: 4, end: 5 },
    ]);
    let compacted = pending
        .coalesce(&[RepairRange { start: 7, end: 8 }], 2)
        .unwrap();
    assert_eq!(
        compacted.remaining().collect::<Vec<_>>(),
        [
            RepairRange { start: 0, end: 1 },
            RepairRange { start: 4, end: 8 }
        ]
    );
    assert_eq!(mask(&pending), 0b0001_0001);
    let merged = pending
        .coalesce(&[RepairRange { start: 1, end: 4 }], 2)
        .unwrap();
    assert_eq!(
        merged.remaining().collect::<Vec<_>>(),
        [RepairRange { start: 0, end: 5 }]
    );
}

#[test]
fn union_never_allocates_beyond_the_fixed_sixty_four_ranges() {
    let original: Vec<_> = (0..64)
        .map(|i| RepairRange {
            start: 4 * i,
            end: 4 * i + 1,
        })
        .collect();
    let pending = job(&original);
    let compacted = pending
        .coalesce(
            &[RepairRange {
                start: 300,
                end: 301,
            }],
            64,
        )
        .unwrap();
    assert_eq!(compacted.count, 64);
    assert_eq!(compacted.ranges[0], RepairRange { start: 0, end: 5 });
    assert_eq!(
        compacted.ranges[63],
        RepairRange {
            start: 300,
            end: 301
        }
    );
    let merged = pending
        .coalesce(&[RepairRange { start: 1, end: 4 }], 64)
        .unwrap();
    assert_eq!(merged.count, 63);
    assert_eq!(pending.count, 64);
}

#[test]
fn maximum_fragment_coordinates_cannot_wrap_union_or_cardinality() {
    let pending = job(&[RepairRange {
        start: 0,
        end: u32::MAX - 1,
    }]);
    let merged = pending
        .coalesce(
            &[RepairRange {
                start: u32::MAX - 1,
                end: u32::MAX,
            }],
            1,
        )
        .unwrap();
    assert_eq!(
        merged.remaining().collect::<Vec<_>>(),
        [RepairRange {
            start: 0,
            end: u32::MAX
        }]
    );
    assert!(matches!(
        merged.coalesce(&[RepairRange { start: 2, end: 3 }], 1),
        Err(SendError::RepairBusy)
    ));
}

#[test]
fn every_bounded_small_union_keeps_all_losses_with_optimal_duplicate_cost() {
    // Independent brute-force oracle: all 256 possible output sets, not the
    // implementation's gap-merging rule. Inputs obey the negotiated range cap.
    for maximum in 1..=4 {
        let mut best = [u32::MAX; 256];
        for union in 1..=255_u16 {
            for candidate in union..=255_u16 {
                if candidate & union == union && ranges(candidate).len() <= maximum {
                    best[usize::from(union)] = best[usize::from(union)].min(candidate.count_ones());
                }
            }
        }
        for original in 1..=255_u16 {
            let before = ranges(original);
            if before.len() > maximum {
                continue;
            }
            let pending = job(&before);
            for requested in 1..=255_u16 {
                let incoming = ranges(requested);
                if incoming.len() > maximum {
                    continue;
                }
                let merged = pending.coalesce(&incoming, maximum);
                if requested & !original == 0 {
                    assert!(matches!(merged, Err(SendError::RepairBusy)));
                } else {
                    let merged = merged.unwrap();
                    let coverage = mask(&merged);
                    let union = original | requested;
                    assert_eq!(coverage & union, union);
                    assert_eq!(
                        coverage.count_ones(),
                        best[usize::from(union)],
                        "original={original:08b} requested={requested:08b} max={maximum}"
                    );
                    assert!(merged.count <= maximum);
                    assert_eq!(merged.range, 0);
                }
                assert_eq!(mask(&pending), original);
            }
        }
    }
}

#[test]
fn exhausted_range_cursor_and_a_lost_emitted_repair_keep_only_bounded_future_work() {
    let mut pending = job(&[
        RepairRange { start: 0, end: 2 },
        RepairRange { start: 4, end: 8 },
    ]);
    pending.range = 1;
    pending.fragment = 6;
    let merged = pending
        .coalesce(
            &[
                RepairRange { start: 1, end: 2 },
                RepairRange { start: 9, end: 10 },
            ],
            2,
        )
        .unwrap();
    assert_eq!(
        merged.remaining().collect::<Vec<_>>(),
        [
            RepairRange { start: 1, end: 2 },
            RepairRange { start: 6, end: 10 },
        ]
    );
    // Previously emitted 0 and 4..6 never become an implicit prefix replay.
    assert_eq!(mask(&pending), 0b1100_0000);
    assert!(matches!(
        merged.coalesce(
            &[
                RepairRange { start: 1, end: 2 },
                RepairRange { start: 7, end: 8 },
            ],
            2
        ),
        Err(SendError::RepairBusy)
    ));
}

#[test]
fn compaction_at_u32_boundary_is_bounded_and_zero_capacity_refuses() {
    let pending = job(&[
        RepairRange { start: 1, end: 2 },
        RepairRange {
            start: u32::MAX - 3,
            end: u32::MAX - 2,
        },
    ]);
    let merged = pending
        .coalesce(
            &[RepairRange {
                start: u32::MAX - 1,
                end: u32::MAX,
            }],
            2,
        )
        .unwrap();
    assert_eq!(
        merged.remaining().collect::<Vec<_>>(),
        [
            RepairRange { start: 1, end: 2 },
            RepairRange {
                start: u32::MAX - 3,
                end: u32::MAX
            },
        ]
    );
    assert!(matches!(
        pending.coalesce(&[RepairRange { start: 3, end: 4 }], 0),
        Err(SendError::RepairBusy)
    ));
}
