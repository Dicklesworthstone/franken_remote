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
fn range_capacity_is_atomic_and_a_bridging_range_can_reduce_metadata() {
    let pending = job(&[
        RepairRange { start: 0, end: 1 },
        RepairRange { start: 4, end: 5 },
    ]);
    assert!(matches!(
        pending.coalesce(&[RepairRange { start: 7, end: 8 }], 2),
        Err(SendError::RepairBusy)
    ));
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
    assert!(matches!(
        pending.coalesce(
            &[RepairRange {
                start: 300,
                end: 301
            }],
            64
        ),
        Err(SendError::RepairBusy)
    ));
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
