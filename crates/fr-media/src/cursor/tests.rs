use super::*;
use crate::pipeline::{CursorPipelineTracker, CursorRenderingOwner, PipelineError};
use fr_core::ids::DisplayGeometryGeneration;

fn snapshot(serial: u32, size: u16, x: i32, y: i32, rgba: &[u8]) -> Snapshot<'_> {
    Snapshot {
        native_serial: serial,
        x,
        y,
        width: size,
        height: size,
        hotspot_x: 0,
        hotspot_y: 0,
        rgba,
    }
}
fn wire(id: u32, size: u16, rgba: &[u8]) -> CursorShape<'_> {
    CursorShape {
        shape_id: id,
        width: size,
        height: size,
        hotspot_x: 0,
        hotspot_y: 0,
        scale_1000: 1000,
        flags: SHAPE_FLAG_VISIBLE,
        rgba,
    }
}
fn position(shape_id: u32, sequence: u64, geometry: u64) -> CursorPosition {
    CursorPosition {
        shape_id,
        x: 40,
        y: 30,
        geometry_generation: geometry,
        sequence,
        flags: POSITION_FLAG_VISIBLE,
    }
}
fn tracker() -> CursorPipelineTracker {
    CursorPipelineTracker::new(
        CursorRenderingOwner::ClientRendered,
        DisplayGeometryGeneration::INITIAL,
    )
}

#[test]
fn hostile_shapes_are_refused_before_any_allocation() {
    let mut t = tracker();
    let valid = [9_u8; 4 * 4 * 4];
    let cases: [(CursorShape<'_>, Refusal); 6] = [
        (
            CursorShape {
                width: 0,
                height: 0,
                rgba: &[],
                ..wire(1, 4, &valid)
            },
            Refusal::ZeroSize,
        ),
        (
            CursorShape {
                width: 257,
                height: 1,
                rgba: &[],
                ..wire(1, 4, &valid)
            },
            Refusal::Oversized,
        ),
        (
            CursorShape {
                rgba: &valid[..60],
                ..wire(1, 4, &valid)
            },
            Refusal::LengthMismatch,
        ),
        (
            CursorShape {
                hotspot_x: 4,
                ..wire(1, 4, &valid)
            },
            Refusal::HotspotOutside,
        ),
        (
            CursorShape {
                hotspot_y: u16::MAX,
                ..wire(1, 4, &valid)
            },
            Refusal::HotspotOutside,
        ),
        (wire(FALLBACK_SHAPE_ID, 4, &valid), Refusal::ReservedShapeId),
    ];
    for (shape, refusal) in cases {
        assert_eq!(
            t.store_shape(&shape),
            Err(PipelineError::Cursor(refusal)),
            "{shape:?}"
        );
        assert_eq!(t.usage(), (0, 0), "nothing retained for {refusal:?}");
    }
    assert!(t.shape(1).is_none());
    // The host-side map applies the same geometry gate before its copy.
    let mut host = HostCursor::new();
    let hostile = Snapshot {
        hotspot_x: 9,
        ..snapshot(1, 4, 0, 0, &valid)
    };
    assert_eq!(
        host.observe(&Observation::Inside(hostile)),
        Err(Refusal::HotspotOutside)
    );
    assert_eq!(host.usage(), (0, 0));
}

#[test]
fn unknown_shape_position_uses_the_fallback_without_allocating_then_reresolves() {
    let mut t = tracker();
    let effective = t.process_position(&position(77, 1, 0)).unwrap().unwrap();
    assert!(effective.visible && effective.is_fallback_shape);
    assert_eq!(effective.shape_id, FALLBACK_SHAPE_ID);
    assert_eq!(t.usage(), (0, 0), "an unknown id never allocates");
    let fallback = t.shape(FALLBACK_SHAPE_ID).unwrap();
    assert_eq!((fallback.width, fallback.height), (7, 7));
    // The reliable shape arrives after its position: the same confirmed
    // position now resolves to it without another datagram.
    let rgba = [3_u8; 2 * 2 * 4];
    t.store_shape(&wire(77, 2, &rgba)).unwrap();
    let now = t.current().unwrap();
    assert!(now.visible && !now.is_fallback_shape);
    assert_eq!((now.shape_id, now.x, now.y), (77, 40, 30));
    assert_eq!(t.shape(77).unwrap().rgba(), rgba);
}

#[test]
fn stale_geometry_and_sequences_are_fenced_without_advancing() {
    let mut t = tracker();
    let geometry = DisplayGeometryGeneration::INITIAL.as_raw();
    assert!(matches!(
        t.process_position(&position(1, 50, geometry + 1)),
        Err(PipelineError::MismatchedGeometry { .. })
    ));
    // The fenced record did not consume sequence space.
    assert!(t.process_position(&position(1, 2, geometry)).is_ok());
    assert!(matches!(
        t.process_position(&position(1, 2, geometry)),
        Err(PipelineError::StaleSequence { .. })
    ));
    assert!(matches!(
        t.process_position(&position(1, 0, geometry)),
        Err(PipelineError::Wire(_))
    ));
    // A new geometry drops the old confirmed position instead of remapping it.
    t.set_geometry(DisplayGeometryGeneration::from_raw(geometry + 1));
    assert_eq!(t.current(), None);
}

#[test]
fn caches_hold_count_and_byte_bounds_with_identical_host_mirror_eviction() {
    let small = [1_u8; 4 * 4 * 4];
    let mut t = tracker();
    let mut mirror = ShapeSet::default();
    let mut evictions = 0;
    for id in 1..=40_u32 {
        let admission = t.store_shape(&wire(id, 4, &small)).unwrap();
        evictions += admission.evicted;
        assert_eq!(mirror.admit(id, small.len()), Ok(admission.evicted));
        assert!(t.usage().0 <= MAX_SHAPES);
    }
    assert_eq!(evictions, 40 - MAX_SHAPES);
    assert_eq!(t.usage(), (mirror.len(), mirror.bytes()));
    for id in 1..=40 {
        assert_eq!(t.shape(id).is_some(), mirror.contains(id), "id {id}");
    }
    // Byte bound: 256x256 images (256 KiB each) evict long before the count.
    let large = vec![2_u8; 256 * 256 * 4];
    for id in 100..110_u32 {
        let admission = t.store_shape(&wire(id, 256, &large)).unwrap();
        assert_eq!(mirror.admit(id, large.len()), Ok(admission.evicted));
        assert!(t.usage().1 <= MAX_SHAPE_BYTES);
    }
    assert_eq!(t.usage(), (4, 4 * large.len()));
    assert_eq!(t.usage(), (mirror.len(), mirror.bytes()));
    // Host serial map: count and bytes stay bounded; identities never reuse.
    let mut host = HostCursor::new();
    let mut ids = Vec::new();
    for serial in 0..40_u32 {
        let pixels = vec![u8::try_from(serial).unwrap(); 256 * 256 * 4];
        let target = host
            .observe(&Observation::Inside(snapshot(serial, 256, 1, 1, &pixels)))
            .unwrap()
            .unwrap();
        assert!(!ids.contains(&target.shape));
        ids.push(target.shape);
        let (count, bytes) = host.usage();
        assert!(count <= MAX_SHAPES && bytes <= MAX_SHAPE_BYTES);
    }
    assert_eq!(host.usage(), (4, 4 * 256 * 256 * 4));
}

#[test]
fn host_map_reuses_ids_for_the_same_image_and_refuses_exhaustion_typed() {
    let a = [5_u8; 2 * 2 * 4];
    let b = [6_u8; 2 * 2 * 4];
    let mut host = HostCursor::new();
    let first = host
        .observe(&Observation::Inside(snapshot(7, 2, 3, 4, &a)))
        .unwrap()
        .unwrap();
    let moved = host
        .observe(&Observation::Inside(snapshot(7, 2, 9, 9, &a)))
        .unwrap()
        .unwrap();
    assert_eq!(first.shape, moved.shape);
    assert_eq!((moved.x, moved.y, moved.visible), (9, 9, true));
    assert_eq!(host.shape(first.shape).unwrap().wire().rgba, a);
    // Same native serial with different pixels is a different identity.
    let changed = host
        .observe(&Observation::Inside(snapshot(7, 2, 9, 9, &b)))
        .unwrap()
        .unwrap();
    assert_ne!(changed.shape, first.shape);
    assert!(host.shape(first.shape).is_none());
    // Moving keeps confirmed state; outside hides without new coordinates.
    assert_eq!(host.observe(&Observation::Moving), Ok(Some(changed)));
    let hidden = host.observe(&Observation::Outside).unwrap().unwrap();
    assert_eq!((hidden.x, hidden.y, hidden.visible), (9, 9, false));
    host.exhaust_for_test();
    assert_eq!(
        host.observe(&Observation::Inside(snapshot(8, 2, 0, 0, &a))),
        Err(Refusal::IdsExhausted)
    );
    assert_eq!(host.state(), SourceState::Exhausted);
    assert_eq!(
        host.observe(&Observation::Inside(snapshot(7, 2, 0, 0, &b))),
        Ok(None)
    );
    let mut unsupported = HostCursor::new();
    assert_eq!(unsupported.observe(&Observation::Unsupported), Ok(None));
    assert_eq!(unsupported.state(), SourceState::Unsupported);
    assert_eq!(
        unsupported.observe(&Observation::Inside(snapshot(1, 2, 0, 0, &a))),
        Ok(None),
        "unsupported is terminal typed absence, not a polling loop"
    );
}

#[test]
fn a_late_joining_viewer_gets_the_shape_before_any_position_referencing_it() {
    let rgba = [8_u8; 2 * 2 * 4];
    let mut host = HostCursor::new();
    let target = host
        .observe(&Observation::Inside(snapshot(3, 2, 11, 12, &rgba)))
        .unwrap();
    let mut early = ViewerLane::default();
    early.observe(target);
    early
        .shape_delivered(target.unwrap().shape, rgba.len())
        .unwrap();
    let mut late = ViewerLane::default();
    late.observe(target);
    let shape = target.unwrap().shape;
    assert_eq!(late.next(0), Next::Shape(shape));
    // Nothing is positioned before the shape is on its reliable lane.
    assert_eq!(late.next(5_000_000), Next::Shape(shape));
    late.shape_delivered(shape, rgba.len()).unwrap();
    assert_eq!(late.next(0), Next::Position);
    let p = late.position(4).unwrap().unwrap();
    assert_eq!(
        (p.shape_id, p.x, p.y, p.geometry_generation),
        (shape, 11, 12, 4)
    );
    assert_eq!(p.flags, POSITION_FLAG_VISIBLE);
    assert_eq!(early.next(0), Next::Position);
    // A hidden target needs no shape and clears visibility.
    let mut hidden = ViewerLane::default();
    hidden.observe(Some(Target {
        visible: false,
        ..target.unwrap()
    }));
    assert_eq!(hidden.next(0), Next::Position);
    assert_eq!(hidden.position(0).unwrap().unwrap().flags, 0);
}

#[test]
fn replaceable_positions_never_accumulate_and_refresh_is_bounded() {
    let mut lane = ViewerLane::default();
    lane.shape_delivered(1, 16).unwrap();
    for x in 0..1000 {
        lane.observe(Some(Target {
            shape: 1,
            x,
            y: 2,
            visible: true,
        }));
    }
    assert_eq!(lane.next(10), Next::Position);
    let p = lane.position(0).unwrap().unwrap();
    assert_eq!(
        (p.x, p.sequence),
        (999, 1),
        "only the latest target is sent"
    );
    lane.position_sent(&p, 10);
    assert_eq!(lane.next(11), Next::Idle);
    assert_eq!(lane.next(10 + REFRESH_US - 1), Next::Idle);
    assert_eq!(lane.next(10 + REFRESH_US), Next::Position);
    // A backpressured attempt leaves a gap; the next send is still newer.
    let skipped = lane.position(0).unwrap().unwrap();
    let next = lane.position(0).unwrap().unwrap();
    assert!(next.sequence > skipped.sequence);
    lane.exhaust_sequence_for_test();
    assert_eq!(lane.position(0), Err(Refusal::SequenceExhausted));
    assert_eq!(lane.next(u64::MAX), Next::Idle);
}

#[test]
fn an_undeliverable_shape_is_positioned_explicitly_as_the_fallback() {
    let mut lane = ViewerLane::default();
    lane.observe(Some(Target {
        shape: 9,
        x: 1,
        y: 1,
        visible: true,
    }));
    assert_eq!(lane.next(0), Next::Shape(9));
    lane.shape_undeliverable(9);
    assert_eq!(lane.next(0), Next::Position);
    assert_eq!(
        lane.position(0).unwrap().unwrap().shape_id,
        FALLBACK_SHAPE_ID
    );
    // The mirror refuses the reserved id and single images beyond a cache.
    let mut set = ShapeSet::default();
    assert_eq!(
        set.admit(FALLBACK_SHAPE_ID, 4),
        Err(Refusal::ReservedShapeId)
    );
    assert_eq!(
        set.admit(2, MAX_SHAPE_BYTES + 1),
        Err(Refusal::ShapeExceedsCache)
    );
    assert!(set.is_empty());
}
