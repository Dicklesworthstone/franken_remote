#![forbid(unsafe_code)]
//! Deterministic property tests and acceptance verification for the media pipeline policy:
//! - Planted violations for every queue bound (count AND bytes) across all 12 stages;
//! - Skip presentation vs discard reference state;
//! - Driver surface ownership protection;
//! - Full surface reconstruction from partial damage and clearing on resize/device loss;
//! - Cursor tracker with single rendering owner, geometry fencing, and safe unknown-shape fallback;
//! - Static screen idle state machine proving near-zero encode work;
//! - Two ages tracking proving capture stall detection despite healthy heartbeats.

use fr_core::ids::DisplayGeometryGeneration;
use fr_media::pipeline::{
    CursorPipelineTracker, CursorRenderingOwner, DamageRect, DamageRegion,
    DamageSurfaceReconstructor, IdleAction, IdleController, IdleState, PipelineError,
    PipelineQueueLedger, PipelineQueuePolicy, StageKind, StageLimits, TwoAgesTracker,
};
use fr_wire::{
    CursorPosition, CursorShape, POSITION_FLAG_LOCKED, POSITION_FLAG_VISIBLE, SHAPE_FLAG_VISIBLE,
    SourceObservation,
};

#[test]
fn planted_bounds_violations_all_twelve_stages() {
    let all_stages = [
        StageKind::CaptureAdmission,
        StageKind::EncoderSubmission,
        StageKind::CompressedOutbound,
        StageKind::ReassemblyWindow,
        StageKind::DecoderSubmission,
        StageKind::Presentation,
        StageKind::AudioJitter,
        StageKind::CodecInternalSurfaces,
        StageKind::PacketCache,
        StageKind::TransportSendBuffer,
        StageKind::RendererHeldFrames,
        StageKind::SharedViewerRetention,
    ];

    for stage in all_stages {
        // Test 1: Planted count limit violation
        let mut policy = PipelineQueuePolicy::new();
        let max_count = 3;
        let max_bytes = 10_000;
        policy.set_limit(stage, StageLimits::new(max_count, max_bytes));

        let mut ledger = PipelineQueueLedger::new(policy.clone());

        // Fill up to max_count
        for _ in 0..max_count {
            assert!(
                ledger.admit(stage, 100).is_ok(),
                "should admit within count limit on {stage}"
            );
        }

        // Attempt one more -> must fail with CountLimitExceeded
        let count_err = ledger.admit(stage, 100);
        assert_eq!(
            count_err,
            Err(PipelineError::CountLimitExceeded {
                stage,
                current: max_count + 1,
                limit: max_count,
            }),
            "planted count violation must be caught on {stage}"
        );

        // Test 2: Planted byte limit violation
        let mut byte_ledger = PipelineQueueLedger::new(policy);
        // Admitting an item exceeding max_bytes
        let byte_err = byte_ledger.admit(stage, max_bytes + 1);
        assert_eq!(
            byte_err,
            Err(PipelineError::ByteLimitExceeded {
                stage,
                current: max_bytes + 1,
                limit: max_bytes,
            }),
            "planted byte violation must be caught on {stage}"
        );

        // Test 3: High-water mark tracking
        let mut hwm_policy = PipelineQueuePolicy::default();
        hwm_policy.set_limit(stage, StageLimits::new(5, 100_000));
        let mut hwm_ledger = PipelineQueueLedger::new(hwm_policy);
        hwm_ledger.admit(stage, 500).unwrap();
        hwm_ledger.admit(stage, 700).unwrap();
        assert_eq!(hwm_ledger.usage(stage).high_water_count, 2);
        assert_eq!(hwm_ledger.usage(stage).high_water_bytes, 1200);

        hwm_ledger.release(stage, 500);
        assert_eq!(hwm_ledger.usage(stage).current_count, 1);
        assert_eq!(hwm_ledger.usage(stage).current_bytes, 700);
        // High-water mark must remain retained
        assert_eq!(hwm_ledger.usage(stage).high_water_count, 2);
        assert_eq!(hwm_ledger.usage(stage).high_water_bytes, 1200);
    }
}

#[test]
fn skip_presentation_retains_reference_dependency() {
    let mut ledger = PipelineQueueLedger::new(PipelineQueuePolicy::default());

    // Admit Frame 1 to DecoderSubmission and Presentation
    let frame_1_size = 100_000;
    let display_surface_size = 8_000_000;
    ledger
        .admit(StageKind::DecoderSubmission, frame_1_size)
        .unwrap();
    ledger
        .admit(StageKind::Presentation, display_surface_size)
        .unwrap();

    assert_eq!(ledger.usage(StageKind::Presentation).current_count, 1);

    // Frame 2 arrives. To catch up, presenter skips presentation of Frame 1.
    // However, Frame 2 depends on Frame 1 as reference!
    ledger.skip_presentation_retain_reference(1, display_surface_size, frame_1_size);

    // Presentation surface is released
    assert_eq!(ledger.usage(StageKind::Presentation).current_count, 0);
    assert_eq!(ledger.usage(StageKind::Presentation).current_bytes, 0);

    // Reference remains held in DecoderSubmission
    assert_eq!(ledger.usage(StageKind::DecoderSubmission).current_count, 1);
    assert_eq!(
        ledger.usage(StageKind::DecoderSubmission).current_bytes,
        frame_1_size
    );

    // Later, once Frame 2 decoding completes and no longer depends on Frame 1:
    ledger.release_reference(1);
    assert_eq!(ledger.usage(StageKind::DecoderSubmission).current_count, 0);
    assert_eq!(ledger.usage(StageKind::DecoderSubmission).current_bytes, 0);
}

#[test]
fn driver_owned_surfaces_cannot_be_freed_early() {
    let mut ledger = PipelineQueueLedger::new(PipelineQueuePolicy::default());
    let surface_id = 101;

    ledger.mark_driver_owned(surface_id);

    // Attempting to free a surface the GPU driver still owns must refuse
    let res = ledger.try_free_surface(surface_id);
    assert_eq!(
        res,
        Err(PipelineError::DriverSurfaceOwnershipViolation { surface_id })
    );

    // Once driver confirms completion and releases ownership:
    ledger.release_driver_owned(surface_id);
    assert!(ledger.try_free_surface(surface_id).is_ok());
}

#[test]
fn damage_surface_reconstruction_and_clear_on_resize() {
    let width = 100;
    let height = 100;
    let mut reconstructor = DamageSurfaceReconstructor::new(width, height);

    // Initial state: not valid for encode
    assert_eq!(
        reconstructor.verify_valid_for_encode(),
        Err(PipelineError::SurfaceNotInitialized)
    );

    // Partial damage blit (10x10)
    let partial_rect = DamageRect::new(10, 10, 10, 10);
    let partial_pixels = vec![0xAA; 10 * 10 * 4];
    assert!(
        reconstructor
            .apply_damage_rect(partial_rect, &partial_pixels)
            .is_ok()
    );

    // Still not fully initialized
    assert_eq!(
        reconstructor.verify_valid_for_encode(),
        Err(PipelineError::SurfaceNotInitialized)
    );

    // Out of bounds damage rect must return OutOfBoundsDamage error
    let oob_rect = DamageRect::new(95, 95, 10, 10);
    let oob_pixels = vec![0xBB; 10 * 10 * 4];
    assert!(matches!(
        reconstructor.apply_damage_rect(oob_rect, &oob_pixels),
        Err(PipelineError::OutOfBoundsDamage { .. })
    ));

    // Full frame initialization
    let full_pixels = vec![0xCC; (width as usize) * (height as usize) * 4];
    assert!(reconstructor.apply_full_frame(&full_pixels).is_ok());
    assert!(reconstructor.verify_valid_for_encode().is_ok());

    // Subsequent partial rect on top of valid surface
    assert!(
        reconstructor
            .apply_damage_rect(partial_rect, &partial_pixels)
            .is_ok()
    );
    assert!(reconstructor.verify_valid_for_encode().is_ok());

    // On resize or device loss, surface is cleared and reset to uninitialized
    reconstructor.clear_and_resize(200, 200);
    assert_eq!(
        reconstructor.verify_valid_for_encode(),
        Err(PipelineError::SurfaceNotInitialized)
    );
    assert_eq!(reconstructor.surface_bytes().len(), 200 * 200 * 4);
}

#[test]
fn damage_region_bounding_box_and_area() {
    let mut region = DamageRegion::new();
    assert!(region.is_empty());
    assert_eq!(region.bounding_box(), None);

    region.add_rect(DamageRect::new(10, 20, 30, 40));
    region.add_rect(DamageRect::new(50, 60, 20, 20));

    let bbox = region.bounding_box().unwrap();
    assert_eq!(bbox.x, 10);
    assert_eq!(bbox.y, 20);
    assert_eq!(bbox.width, 60); // 70 - 10
    assert_eq!(bbox.height, 60); // 80 - 20

    assert_eq!(region.total_area(), (30 * 40) + (20 * 20));
}

#[test]
fn cursor_pipeline_tracking_and_single_rendering_owner() {
    let geom = DisplayGeometryGeneration::INITIAL;
    let mut tracker = CursorPipelineTracker::new(CursorRenderingOwner::ClientRendered, geom);

    let rgba = [0xFF, 0x00, 0x00, 0xFF];
    let shape = CursorShape {
        shape_id: 42,
        width: 1,
        height: 1,
        hotspot_x: 0,
        hotspot_y: 0,
        scale_1000: 1000,
        flags: SHAPE_FLAG_VISIBLE,
        rgba: &rgba,
    };
    tracker.store_shape(&shape).unwrap();

    // 1. Valid position referencing cached shape
    let pos1 = CursorPosition {
        shape_id: 42,
        x: 100,
        y: 200,
        geometry_generation: geom.as_raw(),
        sequence: 1,
        flags: POSITION_FLAG_VISIBLE,
    };
    let eff1 = tracker.process_position(&pos1).unwrap().unwrap();
    assert_eq!(eff1.shape_id, 42);
    assert_eq!(eff1.x, 100);
    assert_eq!(eff1.y, 200);
    assert!(eff1.visible);
    assert!(!eff1.is_fallback_shape);

    // 2. Stale sequence must be rejected
    let pos_stale = CursorPosition {
        shape_id: 42,
        x: 105,
        y: 205,
        geometry_generation: geom.as_raw(),
        sequence: 1, // duplicate sequence
        flags: POSITION_FLAG_VISIBLE,
    };
    assert!(matches!(
        tracker.process_position(&pos_stale),
        Err(PipelineError::StaleSequence { .. })
    ));

    // 3. Unknown shape ID must safely fall back without error
    let pos_unknown = CursorPosition {
        shape_id: 999, // not yet cached
        x: 110,
        y: 210,
        geometry_generation: geom.as_raw(),
        sequence: 2,
        flags: POSITION_FLAG_VISIBLE,
    };
    let eff_unknown = tracker.process_position(&pos_unknown).unwrap().unwrap();
    assert_eq!(
        eff_unknown.shape_id,
        CursorPipelineTracker::FALLBACK_SHAPE_ID
    );
    assert!(eff_unknown.is_fallback_shape);
    assert!(eff_unknown.visible);

    // 4. Mismatched geometry generation must be rejected
    let pos_wrong_geom = CursorPosition {
        shape_id: 42,
        x: 120,
        y: 220,
        geometry_generation: 9999,
        sequence: 3,
        flags: POSITION_FLAG_VISIBLE,
    };
    assert!(matches!(
        tracker.process_position(&pos_wrong_geom),
        Err(PipelineError::MismatchedGeometry { .. })
    ));

    // 5. HostComposited owner suppresses client rendering (no double cursor)
    tracker.set_rendering_owner(CursorRenderingOwner::HostComposited);
    let pos_host_comp = CursorPosition {
        shape_id: 42,
        x: 130,
        y: 230,
        geometry_generation: geom.as_raw(),
        sequence: 4,
        flags: POSITION_FLAG_VISIBLE,
    };
    let eff_host = tracker.process_position(&pos_host_comp).unwrap().unwrap();
    assert!(
        !eff_host.visible,
        "must not render client cursor when host composites"
    );

    // 6. Pointer lock mode suppresses client cursor rendering
    tracker.set_rendering_owner(CursorRenderingOwner::ClientRendered);
    tracker.set_pointer_locked(true);
    let pos_locked = CursorPosition {
        shape_id: 42,
        x: 140,
        y: 240,
        geometry_generation: geom.as_raw(),
        sequence: 5,
        flags: POSITION_FLAG_VISIBLE | POSITION_FLAG_LOCKED,
    };
    let eff_locked = tracker.process_position(&pos_locked).unwrap().unwrap();
    assert!(
        !eff_locked.visible,
        "pointer lock must suppress local cursor"
    );
    assert!(eff_locked.locked);
}

#[test]
fn static_screen_idle_settle_to_sharp_and_near_zero_encode() {
    let mut controller = IdleController::new(1_000_000);

    // Frame 1 with damage -> standard encode
    let a1 = controller.on_frame_event(true, 1_016_000);
    assert_eq!(a1, IdleAction::EncodeStandardFrame);
    assert_eq!(controller.state(), IdleState::Active);

    // Screen stops moving: frame 2 stationary -> transitions to Settling
    let a2 = controller.on_frame_event(false, 1_033_000);
    assert_eq!(a2, IdleAction::SkipVideoTransmission);
    assert!(matches!(controller.state(), IdleState::Settling { .. }));

    // Still stationary for 300 ms -> still settling, no video transmission
    let a3 = controller.on_frame_event(false, 1_333_000);
    assert_eq!(a3, IdleAction::SkipVideoTransmission);

    // Motion settled after 400 ms -> triggers settle-to-sharp refinement encode!
    let a4 = controller.on_frame_event(false, 1_450_000);
    assert_eq!(a4, IdleAction::EncodeSettleToSharpRefinement);
    assert_eq!(controller.state(), IdleState::SettleToSharpPending);

    // Next frame -> enters Idle
    let a5 = controller.on_frame_event(false, 1_466_000);
    assert_eq!(a5, IdleAction::SkipVideoTransmission);
    assert!(matches!(controller.state(), IdleState::Idle { .. }));

    // 100 consecutive stationary frames in Idle -> skip transmission except periodic source verification
    let mut last_verified_us = 1_466_000;
    for i in 1..=100 {
        let now = 1_466_000 + i * 16_000;
        let action = controller.on_frame_event(false, now);
        if now.saturating_sub(last_verified_us) >= IdleController::DEFAULT_VERIFICATION_INTERVAL_US
        {
            // Source verification probe triggered
            assert_eq!(action, IdleAction::EmitSourceVerificationProbe);
            controller.on_source_verification(now);
            last_verified_us = now;
        } else {
            assert_eq!(action, IdleAction::SkipVideoTransmission);
        }
    }

    // First damage event promptly exits Idle back to Active!
    let a_damage = controller.on_frame_event(true, 3_500_000);
    assert_eq!(a_damage, IdleAction::EncodeStandardFrame);
    assert_eq!(controller.state(), IdleState::Active);
}

#[test]
fn two_ages_distinguishes_idle_from_hung_capture() {
    let start_us = 1_000_000;
    let mut tracker = TwoAgesTracker::new(start_us);

    // Initial state: 0 age
    assert_eq!(tracker.pixel_age_us(start_us), 0);
    assert_eq!(tracker.source_observation_age_us(start_us), 0);
    assert!(!tracker.is_view_stale(start_us));

    // Case 1: Static desktop for 60 seconds.
    // Pixel update was at t = 1,000,000 us.
    // Source verification probes arrive every 1 second.
    let now_us = start_us + 60_000_000; // 60s later
    tracker.record_source_observation(now_us, SourceObservation::QualifiedUnchanged);

    // Pixel age is 60 seconds (old)
    assert_eq!(tracker.pixel_age_us(now_us), 60_000_000);
    // Source observation age is 0 us (fresh!)
    assert_eq!(tracker.source_observation_age_us(now_us), 0);
    // The view is NOT stale because source verification is fresh!
    assert!(!tracker.is_view_stale(now_us));

    // Case 2: Capture pipeline hangs!
    // Time advances by 2.0s without any new source observation.
    // Meanwhile, connection/transport heartbeats continue arriving every 50ms.
    let hung_now_us = now_us + 2_000_000;
    tracker.record_heartbeat(hung_now_us - 10_000);

    // Network heartbeat is super fresh (10ms ago)
    assert_eq!(tracker.heartbeat_age_us(hung_now_us), 10_000);
    // Source observation is 2.0s old (exceeds 1.5s threshold)
    assert_eq!(tracker.source_observation_age_us(hung_now_us), 2_000_000);

    // View is marked STALE despite healthy connection heartbeat! (Plan §11.3)
    assert!(tracker.is_view_stale(hung_now_us));
}
