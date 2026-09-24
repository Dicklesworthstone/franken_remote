#![forbid(unsafe_code)]

use fr_core::dos::{
    AdmissionAccountant, CodecProbeTracker, DosRefusal, FloodFairQueue, IdleSessionTracker,
    MetadataFragmentValidator, PreAllocValidator, QueueLane, RateLimiterRegistry, TokenBucket,
};
use fr_core::ids::RemoteSessionId;
use fr_core::limits::{LimitField, LimitField as LF, LimitOverrides, LimitsError, ProtocolLimits};
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

macro_rules! check_ceiling {
    ($field:ident, $val:expr, $expected:expr) => {
        assert_above_ceiling(
            LimitOverrides {
                $field: Some($val),
                ..Default::default()
            },
            $expected,
        );
    };
}

macro_rules! check_floor {
    ($field:ident, $val:expr, $expected_field:expr, $floor:expr) => {
        assert_below_floor(
            LimitOverrides {
                $field: Some($val),
                ..Default::default()
            },
            $expected_field,
            $floor,
        );
    };
}

#[test]
fn protocol_limits_rate_overrides_above_ceiling_are_rejected() {
    let a = ProtocolLimits::ABSOLUTE;
    check_ceiling!(
        max_handshake_duration_ms,
        a.max_handshake_duration_ms() + 1,
        LF::HandshakeDurationMs
    );
    check_ceiling!(
        max_preadmission_rate_per_sec,
        a.max_preadmission_rate_per_sec() + 1,
        LF::PreadmissionRatePerSec
    );
    check_ceiling!(
        max_half_attached_channels,
        a.max_half_attached_channels() + 1,
        LF::HalfAttachedChannels
    );
    check_ceiling!(
        max_pending_approvals,
        a.max_pending_approvals() + 1,
        LF::PendingApprovals
    );
}

#[test]
fn protocol_limits_payload_overrides_above_ceiling_are_rejected() {
    let a = ProtocolLimits::ABSOLUTE;
    check_ceiling!(
        max_cursor_dimension_pixels,
        a.max_cursor_dimension_pixels() + 1,
        LF::CursorDimensionPixels
    );
    check_ceiling!(
        max_cursor_shape_bytes,
        a.max_cursor_shape_bytes() + 1,
        LF::CursorShapeBytes
    );
    check_ceiling!(max_name_bytes, a.max_name_bytes() + 1, LF::NameBytes);
    check_ceiling!(
        max_parameter_set_bytes,
        a.max_parameter_set_bytes() + 1,
        LF::ParameterSetBytes
    );
    check_ceiling!(
        max_fragments_per_access_unit,
        a.max_fragments_per_access_unit() + 1,
        LF::FragmentsPerAccessUnit
    );
}

#[test]
fn protocol_limits_resource_overrides_above_ceiling_are_rejected() {
    let a = ProtocolLimits::ABSOLUTE;
    check_ceiling!(
        max_retained_receipts,
        a.max_retained_receipts() + 1,
        LF::RetainedReceipts
    );
    check_ceiling!(
        max_encoder_sessions,
        a.max_encoder_sessions() + 1,
        LF::EncoderSessions
    );
    check_ceiling!(max_gpu_surfaces, a.max_gpu_surfaces() + 1, LF::GpuSurfaces);
    check_ceiling!(
        max_bandwidth_bps,
        a.max_bandwidth_bps() + 1,
        LF::BandwidthBps
    );
    check_ceiling!(max_viewers, a.max_viewers() + 1, LF::Viewers);
}

#[test]
fn protocol_limits_rate_overrides_below_floor_are_rejected() {
    check_floor!(
        max_handshake_duration_ms,
        999,
        LF::HandshakeDurationMs,
        1000
    );
    check_floor!(
        idle_session_timeout_seconds,
        9,
        LF::IdleSessionTimeoutSecs,
        10
    );
}

#[test]
fn protocol_limits_resource_overrides_below_floor_are_rejected() {
    check_floor!(
        max_cursor_dimension_pixels,
        15,
        LF::CursorDimensionPixels,
        16
    );
    check_floor!(max_cursor_shape_bytes, 1023, LF::CursorShapeBytes, 1024);
    check_floor!(max_parameter_set_bytes, 31, LF::ParameterSetBytes, 32);
    check_floor!(max_retained_receipts, 15, LF::RetainedReceipts, 16);
    check_floor!(max_gpu_surfaces, 1, LF::GpuSurfaces, 2);
    check_floor!(max_bandwidth_bps, 999_999, LF::BandwidthBps, 1_000_000);
}

#[test]
fn protocol_limits_payload_validations_reject_violations() {
    let limits = ProtocolLimits::ABSOLUTE;
    assert!(matches!(
        limits.validate_cursor_dimensions(0, 64),
        Err(LimitsError::ZeroDimension)
    ));
    assert!(matches!(
        limits.validate_cursor_dimensions(64, 0),
        Err(LimitsError::ZeroDimension)
    ));
    assert_err_field(
        limits.validate_cursor_dimensions(limits.max_cursor_dimension_pixels() + 1, 64),
        LF::CursorDimensionPixels,
    );
    assert_err_field(
        limits.validate_cursor_shape_len(limits.max_cursor_shape_bytes() as usize + 1),
        LF::CursorShapeBytes,
    );
    assert_err_field(
        limits.validate_name_len(limits.max_name_bytes() + 1),
        LF::NameBytes,
    );
    assert_err_field(
        limits.validate_parameter_set_len(limits.max_parameter_set_bytes() as usize + 1),
        LF::ParameterSetBytes,
    );
    assert_err_field(
        limits.validate_fragment_count(limits.max_fragments_per_access_unit() + 1),
        LF::FragmentsPerAccessUnit,
    );
}

#[test]
fn protocol_limits_concurrency_validations_reject_violations() {
    let limits = ProtocolLimits::ABSOLUTE;
    assert_err_field(
        limits.validate_retained_receipts(limits.max_retained_receipts() + 1),
        LF::RetainedReceipts,
    );
    assert_err_field(
        limits.validate_handshake_concurrency(limits.max_concurrent_handshakes()),
        LF::ConcurrentHandshakes,
    );
    assert_err_field(
        limits.validate_pending_approvals(limits.max_pending_approvals()),
        LF::PendingApprovals,
    );
    assert_err_field(
        limits.validate_half_attached_channels(limits.max_half_attached_channels()),
        LF::HalfAttachedChannels,
    );
    assert_err_field(
        limits.validate_encoder_sessions(limits.max_encoder_sessions()),
        LF::EncoderSessions,
    );
    assert_err_field(
        limits.validate_gpu_surfaces(limits.max_gpu_surfaces()),
        LF::GpuSurfaces,
    );
    assert_err_field(limits.validate_viewers(limits.max_viewers()), LF::Viewers);
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
    assert_eq!(
        MetadataFragmentValidator::validate(100, 0, &limits),
        Err(DosRefusal::ExcessiveMetadataFragments { count: 100, max: 1 })
    );

    // Exceeding fragments per access unit ceiling
    let too_many = limits.max_fragments_per_access_unit() + 1;
    assert_eq!(
        MetadataFragmentValidator::validate(too_many, 1000, &limits),
        Err(DosRefusal::ExcessiveMetadataFragments {
            count: too_many,
            max: limits.max_fragments_per_access_unit()
        })
    );

    // Valid metadata passes
    assert!(MetadataFragmentValidator::validate(4, 1024, &limits).is_ok());
}

#[test]
fn flood_fair_queue_bounds_count_and_bytes() {
    let mut queue = FloodFairQueue::<u32>::new(2, 100, 2, 100, 2, 100);

    // Fill high lane count
    assert!(queue.push(QueueLane::High, 1, 10).is_ok());
    assert!(queue.push(QueueLane::High, 2, 10).is_ok());
    assert_eq!(
        queue.push(QueueLane::High, 3, 10),
        Err(DosRefusal::QueueFull {
            lane: QueueLane::High,
            current_count: 2,
            max_count: 2
        })
    );

    // Fill bulk lane bytes
    assert!(queue.push(QueueLane::Bulk, 10, 80).is_ok());
    assert_eq!(
        queue.push(QueueLane::Bulk, 11, 30),
        Err(DosRefusal::ByteLimitExceeded {
            lane: QueueLane::Bulk,
            current_bytes: 80,
            max_bytes: 100
        })
    );

    // Drain high lane
    assert_eq!(queue.pop(), Some((QueueLane::High, 1)));
    assert_eq!(queue.pop(), Some((QueueLane::High, 2)));

    // Now High can accept again
    assert!(queue.push(QueueLane::High, 4, 10).is_ok());
}
