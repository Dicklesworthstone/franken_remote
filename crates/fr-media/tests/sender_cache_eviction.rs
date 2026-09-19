//! Sender-cache byte budget, picture capacity, and time-based eviction unit tests.
use fr_core::{
    ids::{CodecConfigurationGeneration, RecoveryGeneration},
    limits::ProtocolLimits,
};
use fr_media::delivery::{
    DeliveryMode, MediaBindings, MediaEpoch, SendCache, SendError, SendPolicy,
};
use fr_wire::{FrameDescriptor, MediaLimits, PipelineState, Progress, SourceObservation};

fn make_sender(policy: SendPolicy) -> (SendCache, MediaLimits) {
    let limits = MediaLimits::new(ProtocolLimits::ABSOLUTE, 1_150, 16_384, 64).unwrap();
    (
        SendCache::new(
            limits,
            MediaBindings::new(1, 2, 3, 4).unwrap(),
            MediaEpoch {
                configuration: CodecConfigurationGeneration::INITIAL,
                recovery: RecoveryGeneration::INITIAL,
            },
            policy,
        )
        .unwrap(),
        limits,
    )
}

fn picture(limits: &MediaLimits, frame: u64, capture: u64, bytes_len: u32) -> Progress {
    Progress {
        descriptor: FrameDescriptor {
            frame,
            reference: frame.checked_sub(1),
            total_bytes: bytes_len,
            stride: limits.fragment_stride(),
            capture_micros: capture,
        },
        observed_micros: capture,
        observation: SourceObservation::Captured,
        pipeline: PipelineState::Running,
    }
}

fn drain_all(tx: &mut SendCache, now: u64) {
    let mut buffer = [0_u8; 1_150];
    while let Ok(Some(packet)) = tx.next_packet(now, &mut buffer) {
        tx.authorize_write(&packet, now).unwrap();
    }
}

#[test]
fn sender_cache_enforces_byte_budget_including_metadata() {
    let custom_policy = SendPolicy {
        max_cached_pictures: 10,
        max_cached_bytes: 4_096,
        reference_horizon_micros: 250_000,
        recovery_horizon_micros: 2_000_000,
        minimum_repair_interval_micros: 10_000,
        max_repair_rounds: 3,
        repair_bytes_per_window: 2_048,
        repair_window_micros: 250_000,
    };
    let (mut tx, limits) = make_sender(custom_policy);

    assert_eq!(tx.cached_bytes(), 0);
    assert_eq!(tx.cached_pictures(), 0);

    // Initial IDR
    let idr_prog = picture(&limits, 0, 0, 100);
    tx.push(idr_prog, vec![0xAA; 100], DeliveryMode::Recovery, 0)
        .unwrap();
    drain_all(&mut tx, 0);

    let bytes_after_first = tx.cached_bytes();
    assert!(
        bytes_after_first > 100,
        "cached_bytes must include CachedPicture struct overhead"
    );
    assert_eq!(tx.cached_pictures(), 1);

    // Push second picture
    let p1 = picture(&limits, 1, 10_000, 100);
    tx.push(p1, vec![0xBB; 100], DeliveryMode::Datagrams, 10_000)
        .unwrap();
    drain_all(&mut tx, 10_000);

    assert_eq!(tx.cached_pictures(), 2);
    assert_eq!(tx.cached_bytes(), bytes_after_first * 2);

    // Attempt to push picture that exceeds remaining byte budget (4096 bytes total)
    let huge_p = picture(&limits, 2, 20_000, 3_800);
    assert_eq!(
        tx.push(huge_p, vec![0xCC; 3_800], DeliveryMode::Datagrams, 20_000),
        Err(SendError::CacheFull)
    );

    // Cache state must remain clean and uncorrupted after rejection
    assert_eq!(tx.cached_pictures(), 2);
    assert_eq!(tx.cached_bytes(), bytes_after_first * 2);
}

#[test]
fn sender_cache_enforces_picture_count_limit() {
    let custom_policy = SendPolicy {
        max_cached_pictures: 3,
        max_cached_bytes: 1_000_000,
        reference_horizon_micros: 250_000,
        recovery_horizon_micros: 2_000_000,
        minimum_repair_interval_micros: 10_000,
        max_repair_rounds: 3,
        repair_bytes_per_window: 256_000,
        repair_window_micros: 250_000,
    };
    let (mut tx, limits) = make_sender(custom_policy);

    // Frame 0 (IDR)
    tx.push(
        picture(&limits, 0, 0, 50),
        vec![1; 50],
        DeliveryMode::Recovery,
        0,
    )
    .unwrap();
    drain_all(&mut tx, 0);

    // Frame 1
    tx.push(
        picture(&limits, 1, 1_000, 50),
        vec![2; 50],
        DeliveryMode::Datagrams,
        1_000,
    )
    .unwrap();
    drain_all(&mut tx, 1_000);

    // Frame 2
    tx.push(
        picture(&limits, 2, 2_000, 50),
        vec![3; 50],
        DeliveryMode::Datagrams,
        2_000,
    )
    .unwrap();
    drain_all(&mut tx, 2_000);

    assert_eq!(tx.cached_pictures(), 3);

    // Frame 3 exceeds max_cached_pictures (3)
    let p3 = picture(&limits, 3, 3_000, 50);
    assert_eq!(
        tx.push(p3, vec![4; 50], DeliveryMode::Datagrams, 3_000),
        Err(SendError::CacheFull)
    );
    assert_eq!(tx.cached_pictures(), 3);
}

#[test]
fn time_eviction_reclaims_completed_pictures() {
    let horizon = 100_000;
    let custom_policy = SendPolicy {
        max_cached_pictures: 5,
        max_cached_bytes: 100_000,
        reference_horizon_micros: horizon,
        recovery_horizon_micros: horizon,
        minimum_repair_interval_micros: 10_000,
        max_repair_rounds: 3,
        repair_bytes_per_window: 50_000,
        repair_window_micros: 100_000,
    };
    let (mut tx, limits) = make_sender(custom_policy);

    // Push frame 0 at t=0, deadline = t=100_000
    tx.push(
        picture(&limits, 0, 0, 64),
        vec![1; 64],
        DeliveryMode::Recovery,
        0,
    )
    .unwrap();
    drain_all(&mut tx, 0);

    // Push frame 1 at t=50_000, deadline = t=150_000
    tx.push(
        picture(&limits, 1, 50_000, 64),
        vec![2; 64],
        DeliveryMode::Datagrams,
        50_000,
    )
    .unwrap();
    drain_all(&mut tx, 50_000);

    assert_eq!(tx.cached_pictures(), 2);
    let bytes_two = tx.cached_bytes();

    // At t=99_999, neither is expired
    tx.tick(99_999).unwrap();
    assert_eq!(tx.cached_pictures(), 2);
    assert_eq!(tx.cached_bytes(), bytes_two);

    // At t=100_000, frame 0 expires and is evicted
    tx.tick(100_000).unwrap();
    assert_eq!(tx.cached_pictures(), 1);
    assert!(tx.cached_bytes() < bytes_two);

    // At t=150_000, frame 1 expires and is evicted
    tx.tick(150_000).unwrap();
    assert_eq!(tx.cached_pictures(), 0);
    assert_eq!(tx.cached_bytes(), 0);
}

#[test]
fn unsent_picture_expiration_fences_dependents_and_forces_recovery() {
    let horizon = 50_000;
    let custom_policy = SendPolicy {
        max_cached_pictures: 5,
        max_cached_bytes: 100_000,
        reference_horizon_micros: horizon,
        recovery_horizon_micros: horizon,
        minimum_repair_interval_micros: 10_000,
        max_repair_rounds: 3,
        repair_bytes_per_window: 50_000,
        repair_window_micros: 100_000,
    };
    let (mut tx, limits) = make_sender(custom_policy);

    // Push frame 0 at t=0, but DO NOT drain it (unsent!)
    tx.push(
        picture(&limits, 0, 0, 64),
        vec![1; 64],
        DeliveryMode::Recovery,
        0,
    )
    .unwrap();
    assert!(!tx.needs_recovery());

    // Advance clock past deadline without transmitting
    assert_eq!(tx.tick(50_000), Err(SendError::OriginalExpired));
    assert!(tx.needs_recovery());
    assert_eq!(tx.cached_pictures(), 0);
    assert_eq!(tx.cached_bytes(), 0);

    // While recovery is needed, further pushes are refused
    let p1 = picture(&limits, 1, 50_001, 64);
    assert_eq!(
        tx.push(p1, vec![2; 64], DeliveryMode::Datagrams, 50_001),
        Err(SendError::NeedsRecovery)
    );

    // Calling replace with a newer epoch clears needs_recovery
    let next_epoch = MediaEpoch {
        configuration: CodecConfigurationGeneration::INITIAL,
        recovery: RecoveryGeneration::INITIAL.next().expect("next generation"),
    };
    let next_bindings = MediaBindings::new(5, 6, 7, 8).unwrap();
    tx.replace(next_epoch, next_bindings, 50_002).unwrap();
    assert!(!tx.needs_recovery());

    // Fresh IDR can now be pushed
    let fresh_idr = picture(&limits, 0, 50_003, 64);
    tx.push(fresh_idr, vec![3; 64], DeliveryMode::Recovery, 50_003)
        .unwrap();
    assert_eq!(tx.cached_pictures(), 1);
}

#[test]
fn observation_metadata_eviction_without_breaking_codec_state() {
    let (mut tx, limits) = make_sender(SendPolicy::default());

    // Frame 0 (IDR)
    tx.push(
        picture(&limits, 0, 0, 64),
        vec![1; 64],
        DeliveryMode::Recovery,
        0,
    )
    .unwrap();
    drain_all(&mut tx, 0);

    // Record unchanged observation at t=1,000
    tx.observe_unchanged(0, 1_000, 1_000).unwrap();
    assert!(tx.originals_pending());

    // Drain observation packet
    drain_all(&mut tx, 1_000);
    assert!(!tx.originals_pending());

    // Second observation, but allowed to expire (250_000 us horizon)
    tx.observe_unchanged(0, 2_000, 2_000).unwrap();
    tx.tick(252_000).unwrap();

    // Expired observation is evicted on tick, needs_recovery is NOT set
    assert!(!tx.needs_recovery());
}

#[test]
fn clock_monotonicity_enforced_in_tick_and_push() {
    let (mut tx, limits) = make_sender(SendPolicy::default());

    tx.push(
        picture(&limits, 0, 100, 64),
        vec![1; 64],
        DeliveryMode::Recovery,
        100,
    )
    .unwrap();

    // Advancing time works
    tx.tick(200).unwrap();

    // Backward time in tick() returns ClockRegression error
    assert_eq!(
        tx.tick(150),
        Err(SendError::Delivery(
            fr_media::delivery::DeliveryError::ClockRegression
        ))
    );

    // Backward time in push() returns ClockRegression error
    let p1 = picture(&limits, 1, 180, 64);
    assert_eq!(
        tx.push(p1, vec![2; 64], DeliveryMode::Datagrams, 180),
        Err(SendError::Delivery(
            fr_media::delivery::DeliveryError::ClockRegression
        ))
    );
}
