#![forbid(unsafe_code)]

use fr_core::dos::{
    AdmissionAccountant, CodecProbeTracker, DosRefusal, FloodFairQueue, IdleSessionTracker,
    MetadataFragmentValidator, PreAllocValidator, QueueLane, RateLimiterRegistry, TokenBucket,
};
use fr_core::ids::RemoteSessionId;
use fr_core::limits::{LimitField, LimitOverrides, LimitsError, ProtocolLimits};
use fr_core::time::{HostDuration, HostInstant};

fn ms(millis: u64) -> HostDuration {
    HostDuration::from_millis_checked(millis).expect("valid duration")
}

#[allow(clippy::large_types_passed_by_value)]
fn assert_above_ceiling(ov: LimitOverrides, expected_field: LimitField) {
    match ProtocolLimits::with_overrides(ov) {
        Err(LimitsError::AboveCeiling { field, .. }) => assert_eq!(field, expected_field),
        other => panic!("expected AboveCeiling({expected_field:?}), got {other:?}"),
    }
}

#[allow(clippy::large_types_passed_by_value)]
fn assert_below_floor(ov: LimitOverrides, expected_field: LimitField, expected_floor: u64) {
    match ProtocolLimits::with_overrides(ov) {
        Err(LimitsError::BelowFloor { field, floor, .. }) => {
            assert_eq!(field, expected_field);
            assert_eq!(floor, expected_floor);
        }
        other => panic!("expected BelowFloor({expected_field:?}), got {other:?}"),
    }
}

fn assert_err_field<T: std::fmt::Debug>(res: Result<T, LimitsError>, expected: LimitField) {
    match res {
        Err(LimitsError::AboveCeiling { field, .. }) => assert_eq!(field, expected),
        other => panic!("expected AboveCeiling({expected:?}), got {other:?}"),
    }
}

fn assert_capacity<T: std::fmt::Debug>(res: Result<T, DosRefusal>, expected: LimitField) {
    match res {
        Err(DosRefusal::CapacityExceeded { field, .. }) => assert_eq!(field, expected),
        other => panic!("expected CapacityExceeded({expected:?}), got {other:?}"),
    }
}

fn assert_ratelimit<T: std::fmt::Debug>(res: Result<T, DosRefusal>, expected: LimitField) {
    match res {
        Err(DosRefusal::RateLimitExceeded { field, .. }) => assert_eq!(field, expected),
        other => panic!("expected RateLimitExceeded({expected:?}), got {other:?}"),
    }
}

#[test]
fn protocol_limits_rate_overrides_above_ceiling_are_rejected() {
    let a = ProtocolLimits::ABSOLUTE;
    assert_above_ceiling(
        LimitOverrides {
            max_handshake_duration_ms: Some(a.max_handshake_duration_ms() + 1),
            ..Default::default()
        },
        LimitField::HandshakeDurationMs,
    );
    assert_above_ceiling(
        LimitOverrides {
            max_preadmission_rate_per_sec: Some(a.max_preadmission_rate_per_sec() + 1),
            ..Default::default()
        },
        LimitField::PreadmissionRatePerSec,
    );
    assert_above_ceiling(
        LimitOverrides {
            max_half_attached_channels: Some(a.max_half_attached_channels() + 1),
            ..Default::default()
        },
        LimitField::HalfAttachedChannels,
    );
    assert_above_ceiling(
        LimitOverrides {
            max_pending_approvals: Some(a.max_pending_approvals() + 1),
            ..Default::default()
        },
        LimitField::PendingApprovals,
    );
}

#[test]
fn protocol_limits_payload_overrides_above_ceiling_are_rejected() {
    let a = ProtocolLimits::ABSOLUTE;
    assert_above_ceiling(
        LimitOverrides {
            max_cursor_dimension_pixels: Some(a.max_cursor_dimension_pixels() + 1),
            ..Default::default()
        },
        LimitField::CursorDimensionPixels,
    );
    assert_above_ceiling(
        LimitOverrides {
            max_cursor_shape_bytes: Some(a.max_cursor_shape_bytes() + 1),
            ..Default::default()
        },
        LimitField::CursorShapeBytes,
    );
    assert_above_ceiling(
        LimitOverrides {
            max_name_bytes: Some(a.max_name_bytes() + 1),
            ..Default::default()
        },
        LimitField::NameBytes,
    );
    assert_above_ceiling(
        LimitOverrides {
            max_parameter_set_bytes: Some(a.max_parameter_set_bytes() + 1),
            ..Default::default()
        },
        LimitField::ParameterSetBytes,
    );
    assert_above_ceiling(
        LimitOverrides {
            max_fragments_per_access_unit: Some(a.max_fragments_per_access_unit() + 1),
            ..Default::default()
        },
        LimitField::FragmentsPerAccessUnit,
    );
}

#[test]
fn protocol_limits_resource_overrides_above_ceiling_are_rejected() {
    let a = ProtocolLimits::ABSOLUTE;
    assert_above_ceiling(
        LimitOverrides {
            max_retained_receipts: Some(a.max_retained_receipts() + 1),
            ..Default::default()
        },
        LimitField::RetainedReceipts,
    );
    assert_above_ceiling(
        LimitOverrides {
            max_encoder_sessions: Some(a.max_encoder_sessions() + 1),
            ..Default::default()
        },
        LimitField::EncoderSessions,
    );
    assert_above_ceiling(
        LimitOverrides {
            max_gpu_surfaces: Some(a.max_gpu_surfaces() + 1),
            ..Default::default()
        },
        LimitField::GpuSurfaces,
    );
    assert_above_ceiling(
        LimitOverrides {
            max_bandwidth_bps: Some(a.max_bandwidth_bps() + 1),
            ..Default::default()
        },
        LimitField::BandwidthBps,
    );
    assert_above_ceiling(
        LimitOverrides {
            max_viewers: Some(a.max_viewers() + 1),
            ..Default::default()
        },
        LimitField::Viewers,
    );
}

#[test]
fn protocol_limits_rate_overrides_below_floor_are_rejected() {
    assert_below_floor(
        LimitOverrides {
            max_handshake_duration_ms: Some(999),
            ..Default::default()
        },
        LimitField::HandshakeDurationMs,
        1000,
    );
    assert_below_floor(
        LimitOverrides {
            idle_session_timeout_seconds: Some(9),
            ..Default::default()
        },
        LimitField::IdleSessionTimeoutSecs,
        10,
    );
}

#[test]
fn protocol_limits_resource_overrides_below_floor_are_rejected() {
    assert_below_floor(
        LimitOverrides {
            max_cursor_dimension_pixels: Some(15),
            ..Default::default()
        },
        LimitField::CursorDimensionPixels,
        16,
    );
    assert_below_floor(
        LimitOverrides {
            max_cursor_shape_bytes: Some(1023),
            ..Default::default()
        },
        LimitField::CursorShapeBytes,
        1024,
    );
    assert_below_floor(
        LimitOverrides {
            max_parameter_set_bytes: Some(31),
            ..Default::default()
        },
        LimitField::ParameterSetBytes,
        32,
    );
    assert_below_floor(
        LimitOverrides {
            max_retained_receipts: Some(15),
            ..Default::default()
        },
        LimitField::RetainedReceipts,
        16,
    );
    assert_below_floor(
        LimitOverrides {
            max_gpu_surfaces: Some(1),
            ..Default::default()
        },
        LimitField::GpuSurfaces,
        2,
    );
    assert_below_floor(
        LimitOverrides {
            max_bandwidth_bps: Some(999_999),
            ..Default::default()
        },
        LimitField::BandwidthBps,
        1_000_000,
    );
}

#[test]
fn protocol_limits_payload_validations_reject_violations() {
    let limits = ProtocolLimits::ABSOLUTE;

    // Cursor dimension: 0 is rejected
    assert!(matches!(
        limits.validate_cursor_dimensions(0, 64),
        Err(LimitsError::ZeroDimension)
    ));
    assert!(matches!(
        limits.validate_cursor_dimensions(64, 0),
        Err(LimitsError::ZeroDimension)
    ));

    // Exceeding ceiling rejected
    assert_err_field(
        limits.validate_cursor_dimensions(limits.max_cursor_dimension_pixels() + 1, 64),
        LimitField::CursorDimensionPixels,
    );
    assert_err_field(
        limits.validate_cursor_shape_len(limits.max_cursor_shape_bytes() as usize + 1),
        LimitField::CursorShapeBytes,
    );
    assert_err_field(
        limits.validate_name_len(limits.max_name_bytes() + 1),
        LimitField::NameBytes,
    );
    assert_err_field(
        limits.validate_parameter_set_len(limits.max_parameter_set_bytes() as usize + 1),
        LimitField::ParameterSetBytes,
    );
    assert_err_field(
        limits.validate_fragment_count(limits.max_fragments_per_access_unit() + 1),
        LimitField::FragmentsPerAccessUnit,
    );
}

#[test]
fn protocol_limits_concurrency_validations_reject_violations() {
    let limits = ProtocolLimits::ABSOLUTE;

    assert_err_field(
        limits.validate_retained_receipts(limits.max_retained_receipts() + 1),
        LimitField::RetainedReceipts,
    );
    assert_err_field(
        limits.validate_handshake_concurrency(limits.max_concurrent_handshakes()),
        LimitField::ConcurrentHandshakes,
    );
    assert_err_field(
        limits.validate_pending_approvals(limits.max_pending_approvals()),
        LimitField::PendingApprovals,
    );
    assert_err_field(
        limits.validate_half_attached_channels(limits.max_half_attached_channels()),
        LimitField::HalfAttachedChannels,
    );
    assert_err_field(
        limits.validate_encoder_sessions(limits.max_encoder_sessions()),
        LimitField::EncoderSessions,
    );
    assert_err_field(
        limits.validate_gpu_surfaces(limits.max_gpu_surfaces()),
        LimitField::GpuSurfaces,
    );
    assert_err_field(
        limits.validate_viewers(limits.max_viewers()),
        LimitField::Viewers,
    );
}

#[test]
fn admission_accountant_bounds_global_resources() {
    let limits = ProtocolLimits::ABSOLUTE;
    let mut accountant = AdmissionAccountant::new();

    // Viewers ceiling enforcement
    let session = RemoteSessionId::from_raw(1);
    for i in 0..limits.max_viewers() {
        let sid = RemoteSessionId::from_raw(u128::from(i + 1));
        assert!(accountant.acquire_viewer(sid, &limits).is_ok());
    }
    match accountant.acquire_viewer(RemoteSessionId::from_raw(999), &limits) {
        Err(DosRefusal::CapacityExceeded {
            field,
            current,
            limit,
        }) => {
            assert_eq!(field, LimitField::Viewers);
            assert_eq!(current, u64::from(limits.max_viewers()));
            assert_eq!(limit, u64::from(limits.max_viewers()));
        }
        other => panic!("expected CapacityExceeded, got {other:?}"),
    }

    // Release viewer and re-acquire
    accountant.release_viewer(session);
    assert!(accountant.acquire_viewer(session, &limits).is_ok());

    // Encoder sessions ceiling enforcement
    for _ in 0..limits.max_encoder_sessions() {
        assert!(accountant.acquire_encoder_session(&limits).is_ok());
    }
    assert_capacity(
        accountant.acquire_encoder_session(&limits),
        LimitField::EncoderSessions,
    );

    // GPU surfaces ceiling enforcement
    assert!(
        accountant
            .acquire_gpu_surfaces(limits.max_gpu_surfaces(), &limits)
            .is_ok()
    );
    assert_capacity(
        accountant.acquire_gpu_surfaces(1, &limits),
        LimitField::GpuSurfaces,
    );

    // Half-attached channels ceiling enforcement
    for _ in 0..limits.max_half_attached_channels() {
        assert!(accountant.acquire_half_attached(&limits).is_ok());
    }
    assert_capacity(
        accountant.acquire_half_attached(&limits),
        LimitField::HalfAttachedChannels,
    );

    // Pending approvals ceiling enforcement
    for _ in 0..limits.max_pending_approvals() {
        assert!(accountant.acquire_pending_approval(&limits).is_ok());
    }
    assert_capacity(
        accountant.acquire_pending_approval(&limits),
        LimitField::PendingApprovals,
    );

    // Bandwidth allocation
    let remaining = limits.max_bandwidth_bps();
    assert!(accountant.allocate_bandwidth(remaining, &limits).is_ok());
    assert_capacity(
        accountant.allocate_bandwidth(1, &limits),
        LimitField::BandwidthBps,
    );

    // Viewer memory allocation
    let viewer_limit = limits.per_viewer_compressed_bytes();
    assert!(
        accountant
            .allocate_viewer_memory(session, viewer_limit, &limits)
            .is_ok()
    );
    assert_capacity(
        accountant.allocate_viewer_memory(session, 1, &limits),
        LimitField::PerViewerCompressedBytes,
    );
}

#[test]
fn rate_limiter_registry_rejects_floods() {
    let limits = ProtocolLimits::ABSOLUTE;
    let t0 = HostInstant::ORIGIN;
    let mut registry = RateLimiterRegistry::from_limits(&limits, t0);

    // Codec probes: capped at min(max_codec_probes, 5)
    for _ in 0..5 {
        assert!(registry.check_codec_probe(t0).is_ok());
    }
    match registry.check_codec_probe(t0) {
        Err(DosRefusal::RateLimitExceeded { field, retry_after }) => {
            assert_eq!(field, LimitField::CodecProbesPerMin);
            assert!(retry_after.as_micros() > 0);
        }
        other => panic!("expected RateLimitExceeded, got {other:?}"),
    }

    // Diagnostic exports: capacity is 3
    for _ in 0..3 {
        assert!(registry.check_diagnostic_export(t0).is_ok());
    }
    assert_ratelimit(
        registry.check_diagnostic_export(t0),
        LimitField::DiagnosticExportsPerMin,
    );

    // Decoder reconfigurations: capped at 5
    for _ in 0..5 {
        assert!(registry.check_decoder_reconfig(t0).is_ok());
    }
    assert_ratelimit(
        registry.check_decoder_reconfig(t0),
        LimitField::DecoderReconfigurationsPerMin,
    );

    // Worker restarts: capped at 3
    for _ in 0..3 {
        assert!(registry.check_worker_restart(t0).is_ok());
    }
    assert_ratelimit(
        registry.check_worker_restart(t0),
        LimitField::WorkerRestartsPerMin,
    );
}

#[test]
fn token_bucket_refills_monotonically() {
    let t0 = HostInstant::ORIGIN;
    let mut tb = TokenBucket::new(10, 10, t0);

    // Drain all 10 tokens
    for _ in 0..10 {
        assert!(
            tb.try_consume(t0, 1, LimitField::ControlRequestsPerSec)
                .is_ok()
        );
    }
    assert!(
        tb.try_consume(t0, 1, LimitField::ControlRequestsPerSec)
            .is_err()
    );

    // Advance 500ms -> should replenish 5 tokens (10 per second)
    let t1 = t0.checked_add(ms(500)).unwrap();
    for _ in 0..5 {
        assert!(
            tb.try_consume(t1, 1, LimitField::ControlRequestsPerSec)
                .is_ok()
        );
    }
    assert!(
        tb.try_consume(t1, 1, LimitField::ControlRequestsPerSec)
            .is_err()
    );

    // Advance another 1000ms -> capped at burst capacity 10
    let t2 = t1.checked_add(ms(1000)).unwrap();
    for _ in 0..10 {
        assert!(
            tb.try_consume(t2, 1, LimitField::ControlRequestsPerSec)
                .is_ok()
        );
    }
    assert!(
        tb.try_consume(t2, 1, LimitField::ControlRequestsPerSec)
            .is_err()
    );
}

#[test]
fn idle_session_tracker_unauthenticated_traffic_cannot_extend_timeout() {
    let limits = ProtocolLimits::ABSOLUTE;
    let t0 = HostInstant::ORIGIN;
    let mut tracker = IdleSessionTracker::new(t0);

    // Initial timeout check is healthy
    assert!(
        tracker
            .check_idle_timeout(t0, limits.idle_session_timeout_seconds())
            .is_ok()
    );

    // Unauthenticated packet arrives
    assert!(tracker.record_unauthenticated_garbage(t0).is_ok());

    // Advance time past idle timeout (301 seconds, default timeout is 300s)
    let t1 = t0.checked_add(ms(301_000)).unwrap();

    // The session MUST be expired because unauthenticated traffic did NOT extend timeout
    match tracker.check_idle_timeout(t1, limits.idle_session_timeout_seconds()) {
        Err(DosRefusal::SessionIdleExpired {
            idle_duration,
            timeout,
        }) => {
            assert!(idle_duration > timeout);
        }
        other => panic!("expected SessionIdleExpired, got {other:?}"),
    }

    // Unauthenticated flood triggers denial
    let mut flood_refused = false;
    for _ in 0..15 {
        if tracker.record_unauthenticated_garbage(t0).is_err() {
            flood_refused = true;
            break;
        }
    }
    assert!(flood_refused);
}

#[test]
fn codec_probe_tracker_cancels_orphaned_probes() {
    let limits = ProtocolLimits::ABSOLUTE;
    let mut tracker = CodecProbeTracker::new();
    let session1 = RemoteSessionId::from_raw(101);
    let session2 = RemoteSessionId::from_raw(102);

    assert!(tracker.register_probe(session1, 1, &limits).is_ok());
    assert!(tracker.register_probe(session1, 2, &limits).is_ok());
    assert!(tracker.register_probe(session2, 3, &limits).is_ok());
    assert_eq!(tracker.active_probe_count(), 3);

    // Session 1 closes: its 2 probes must be canceled
    let canceled = tracker.on_session_closed(session1);
    assert_eq!(canceled.len(), 2);
    assert_eq!(tracker.active_probe_count(), 1);

    // Probe 3 for session 2 completes normally
    assert!(tracker.complete_probe(session2, 3));
    assert_eq!(tracker.active_probe_count(), 0);
}

#[test]
fn prealloc_validator_rejects_excessive_allocations() {
    let limits = ProtocolLimits::ABSOLUTE;

    // Width exceeding max coded pixels
    assert!(matches!(
        PreAllocValidator::validate_surface_allocation(9000, 1080, 8, 4, &limits),
        Err(DosRefusal::CursorTooLarge { .. })
    ));

    // Dimensions within max coded pixels (3840x2160 = 8.29M <= 8.84M) but exceeding per-viewer memory budget
    // 3840 * 2160 * 1.5 * 18 = ~224 MiB >> 32 MiB
    assert!(matches!(
        PreAllocValidator::validate_surface_allocation(3840, 2160, 8, 16, &limits),
        Err(DosRefusal::DecoderAllocationExceeded { .. })
    ));

    // Modest dimensions fit well within budget
    // 1920 * 1080 with 4 DPB surfaces = 1920*1080*1.5 * 6 = ~18.6 MiB < 64 MiB
    assert!(PreAllocValidator::validate_surface_allocation(1920, 1080, 8, 4, &limits).is_ok());
}

#[test]
fn metadata_fragment_validator_rejects_exhaustion_attacks() {
    let limits = ProtocolLimits::ABSOLUTE;

    // Zero-byte fragment exhaustion attack (multiple fragments with 0 total payload bytes)
    match MetadataFragmentValidator::validate(100, 0, &limits) {
        Err(DosRefusal::ExcessiveMetadataFragments { count, max }) => {
            assert_eq!(count, 100);
            assert_eq!(max, 1);
        }
        other => panic!("expected ExcessiveMetadataFragments, got {other:?}"),
    }

    // Exceeding fragments per access unit ceiling
    let too_many = limits.max_fragments_per_access_unit() + 1;
    match MetadataFragmentValidator::validate(too_many, 1000, &limits) {
        Err(DosRefusal::ExcessiveMetadataFragments { count, max }) => {
            assert_eq!(count, too_many);
            assert_eq!(max, limits.max_fragments_per_access_unit());
        }
        other => panic!("expected ExcessiveMetadataFragments, got {other:?}"),
    }

    // Valid metadata passes
    assert!(MetadataFragmentValidator::validate(4, 1024, &limits).is_ok());
}

#[test]
fn flood_fair_queue_bounds_count_and_bytes() {
    let mut queue = FloodFairQueue::<u32>::new(2, 100, 2, 100, 2, 100);

    // Fill high lane count
    assert!(queue.push(QueueLane::High, 1, 10).is_ok());
    assert!(queue.push(QueueLane::High, 2, 10).is_ok());
    match queue.push(QueueLane::High, 3, 10) {
        Err(DosRefusal::QueueFull {
            lane,
            current_count,
            max_count,
        }) => {
            assert_eq!(lane, QueueLane::High);
            assert_eq!(current_count, 2);
            assert_eq!(max_count, 2);
        }
        other => panic!("expected QueueFull, got {other:?}"),
    }

    // Fill bulk lane bytes
    assert!(queue.push(QueueLane::Bulk, 10, 80).is_ok());
    match queue.push(QueueLane::Bulk, 11, 30) {
        Err(DosRefusal::ByteLimitExceeded {
            lane,
            current_bytes,
            max_bytes,
        }) => {
            assert_eq!(lane, QueueLane::Bulk);
            assert_eq!(current_bytes, 80);
            assert_eq!(max_bytes, 100);
        }
        other => panic!("expected ByteLimitExceeded, got {other:?}"),
    }

    // Drain high lane
    assert_eq!(queue.pop(), Some((QueueLane::High, 1)));
    assert_eq!(queue.pop(), Some((QueueLane::High, 2)));

    // Now High can accept again
    assert!(queue.push(QueueLane::High, 4, 10).is_ok());
}
