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

#[test]
fn protocol_limits_rate_overrides_above_ceiling_are_rejected() {
    let a = ProtocolLimits::ABSOLUTE;

    // Handshake duration
    let ov = LimitOverrides {
        max_handshake_duration_ms: Some(a.max_handshake_duration_ms() + 1),
        ..Default::default()
    };
    match ProtocolLimits::with_overrides(ov) {
        Err(LimitsError::AboveCeiling {
            field,
            value,
            ceiling,
        }) => {
            assert_eq!(field, LimitField::HandshakeDurationMs);
            assert_eq!(value, u64::from(a.max_handshake_duration_ms() + 1));
            assert_eq!(ceiling, u64::from(a.max_handshake_duration_ms()));
        }
        other => panic!("expected AboveCeiling, got {other:?}"),
    }

    // Preadmission rate
    let ov = LimitOverrides {
        max_preadmission_rate_per_sec: Some(a.max_preadmission_rate_per_sec() + 1),
        ..Default::default()
    };
    match ProtocolLimits::with_overrides(ov) {
        Err(LimitsError::AboveCeiling { field, .. }) => {
            assert_eq!(field, LimitField::PreadmissionRatePerSec);
        }
        other => panic!("expected AboveCeiling, got {other:?}"),
    }

    // Half-attached channels
    let ov = LimitOverrides {
        max_half_attached_channels: Some(a.max_half_attached_channels() + 1),
        ..Default::default()
    };
    match ProtocolLimits::with_overrides(ov) {
        Err(LimitsError::AboveCeiling { field, .. }) => {
            assert_eq!(field, LimitField::HalfAttachedChannels);
        }
        other => panic!("expected AboveCeiling, got {other:?}"),
    }

    // Pending approvals
    let ov = LimitOverrides {
        max_pending_approvals: Some(a.max_pending_approvals() + 1),
        ..Default::default()
    };
    match ProtocolLimits::with_overrides(ov) {
        Err(LimitsError::AboveCeiling { field, .. }) => {
            assert_eq!(field, LimitField::PendingApprovals);
        }
        other => panic!("expected AboveCeiling, got {other:?}"),
    }
}

#[test]
fn protocol_limits_payload_overrides_above_ceiling_are_rejected() {
    let a = ProtocolLimits::ABSOLUTE;

    // Cursor dimension pixels
    let ov = LimitOverrides {
        max_cursor_dimension_pixels: Some(a.max_cursor_dimension_pixels() + 1),
        ..Default::default()
    };
    match ProtocolLimits::with_overrides(ov) {
        Err(LimitsError::AboveCeiling { field, .. }) => {
            assert_eq!(field, LimitField::CursorDimensionPixels);
        }
        other => panic!("expected AboveCeiling, got {other:?}"),
    }

    // Cursor shape bytes
    let ov = LimitOverrides {
        max_cursor_shape_bytes: Some(a.max_cursor_shape_bytes() + 1),
        ..Default::default()
    };
    match ProtocolLimits::with_overrides(ov) {
        Err(LimitsError::AboveCeiling { field, .. }) => {
            assert_eq!(field, LimitField::CursorShapeBytes);
        }
        other => panic!("expected AboveCeiling, got {other:?}"),
    }

    // Name bytes
    let ov = LimitOverrides {
        max_name_bytes: Some(a.max_name_bytes() + 1),
        ..Default::default()
    };
    match ProtocolLimits::with_overrides(ov) {
        Err(LimitsError::AboveCeiling { field, .. }) => {
            assert_eq!(field, LimitField::NameBytes);
        }
        other => panic!("expected AboveCeiling, got {other:?}"),
    }

    // Parameter set bytes
    let ov = LimitOverrides {
        max_parameter_set_bytes: Some(a.max_parameter_set_bytes() + 1),
        ..Default::default()
    };
    match ProtocolLimits::with_overrides(ov) {
        Err(LimitsError::AboveCeiling { field, .. }) => {
            assert_eq!(field, LimitField::ParameterSetBytes);
        }
        other => panic!("expected AboveCeiling, got {other:?}"),
    }

    // Fragments per access unit
    let ov = LimitOverrides {
        max_fragments_per_access_unit: Some(a.max_fragments_per_access_unit() + 1),
        ..Default::default()
    };
    match ProtocolLimits::with_overrides(ov) {
        Err(LimitsError::AboveCeiling { field, .. }) => {
            assert_eq!(field, LimitField::FragmentsPerAccessUnit);
        }
        other => panic!("expected AboveCeiling, got {other:?}"),
    }
}

#[test]
fn protocol_limits_resource_overrides_above_ceiling_are_rejected() {
    let a = ProtocolLimits::ABSOLUTE;

    // Retained receipts
    let ov = LimitOverrides {
        max_retained_receipts: Some(a.max_retained_receipts() + 1),
        ..Default::default()
    };
    match ProtocolLimits::with_overrides(ov) {
        Err(LimitsError::AboveCeiling { field, .. }) => {
            assert_eq!(field, LimitField::RetainedReceipts);
        }
        other => panic!("expected AboveCeiling, got {other:?}"),
    }

    // Encoder sessions
    let ov = LimitOverrides {
        max_encoder_sessions: Some(a.max_encoder_sessions() + 1),
        ..Default::default()
    };
    match ProtocolLimits::with_overrides(ov) {
        Err(LimitsError::AboveCeiling { field, .. }) => {
            assert_eq!(field, LimitField::EncoderSessions);
        }
        other => panic!("expected AboveCeiling, got {other:?}"),
    }

    // GPU surfaces
    let ov = LimitOverrides {
        max_gpu_surfaces: Some(a.max_gpu_surfaces() + 1),
        ..Default::default()
    };
    match ProtocolLimits::with_overrides(ov) {
        Err(LimitsError::AboveCeiling { field, .. }) => {
            assert_eq!(field, LimitField::GpuSurfaces);
        }
        other => panic!("expected AboveCeiling, got {other:?}"),
    }

    // Bandwidth bps
    let ov = LimitOverrides {
        max_bandwidth_bps: Some(a.max_bandwidth_bps() + 1),
        ..Default::default()
    };
    match ProtocolLimits::with_overrides(ov) {
        Err(LimitsError::AboveCeiling { field, .. }) => {
            assert_eq!(field, LimitField::BandwidthBps);
        }
        other => panic!("expected AboveCeiling, got {other:?}"),
    }

    // Viewers
    let ov = LimitOverrides {
        max_viewers: Some(a.max_viewers() + 1),
        ..Default::default()
    };
    match ProtocolLimits::with_overrides(ov) {
        Err(LimitsError::AboveCeiling { field, .. }) => {
            assert_eq!(field, LimitField::Viewers);
        }
        other => panic!("expected AboveCeiling, got {other:?}"),
    }
}

#[test]
fn protocol_limits_rate_overrides_below_floor_are_rejected() {
    // Handshake duration floor is 1000ms
    let ov = LimitOverrides {
        max_handshake_duration_ms: Some(999),
        ..Default::default()
    };
    match ProtocolLimits::with_overrides(ov) {
        Err(LimitsError::BelowFloor {
            field,
            value,
            floor,
        }) => {
            assert_eq!(field, LimitField::HandshakeDurationMs);
            assert_eq!(value, 999);
            assert_eq!(floor, 1000);
        }
        other => panic!("expected BelowFloor, got {other:?}"),
    }

    // Idle session timeout floor is 10s
    let ov = LimitOverrides {
        idle_session_timeout_seconds: Some(9),
        ..Default::default()
    };
    match ProtocolLimits::with_overrides(ov) {
        Err(LimitsError::BelowFloor {
            field,
            value,
            floor,
        }) => {
            assert_eq!(field, LimitField::IdleSessionTimeoutSecs);
            assert_eq!(value, 9);
            assert_eq!(floor, 10);
        }
        other => panic!("expected BelowFloor, got {other:?}"),
    }
}

#[test]
fn protocol_limits_resource_overrides_below_floor_are_rejected() {
    // Cursor dimension floor is 16
    let ov = LimitOverrides {
        max_cursor_dimension_pixels: Some(15),
        ..Default::default()
    };
    match ProtocolLimits::with_overrides(ov) {
        Err(LimitsError::BelowFloor {
            field,
            value,
            floor,
        }) => {
            assert_eq!(field, LimitField::CursorDimensionPixels);
            assert_eq!(value, 15);
            assert_eq!(floor, 16);
        }
        other => panic!("expected BelowFloor, got {other:?}"),
    }

    // Cursor shape floor is 1024 bytes
    let ov = LimitOverrides {
        max_cursor_shape_bytes: Some(1023),
        ..Default::default()
    };
    match ProtocolLimits::with_overrides(ov) {
        Err(LimitsError::BelowFloor {
            field,
            value,
            floor,
        }) => {
            assert_eq!(field, LimitField::CursorShapeBytes);
            assert_eq!(value, 1023);
            assert_eq!(floor, 1024);
        }
        other => panic!("expected BelowFloor, got {other:?}"),
    }

    // Parameter set floor is 32 bytes
    let ov = LimitOverrides {
        max_parameter_set_bytes: Some(31),
        ..Default::default()
    };
    match ProtocolLimits::with_overrides(ov) {
        Err(LimitsError::BelowFloor {
            field,
            value,
            floor,
        }) => {
            assert_eq!(field, LimitField::ParameterSetBytes);
            assert_eq!(value, 31);
            assert_eq!(floor, 32);
        }
        other => panic!("expected BelowFloor, got {other:?}"),
    }

    // Retained receipts floor is 16
    let ov = LimitOverrides {
        max_retained_receipts: Some(15),
        ..Default::default()
    };
    match ProtocolLimits::with_overrides(ov) {
        Err(LimitsError::BelowFloor {
            field,
            value,
            floor,
        }) => {
            assert_eq!(field, LimitField::RetainedReceipts);
            assert_eq!(value, 15);
            assert_eq!(floor, 16);
        }
        other => panic!("expected BelowFloor, got {other:?}"),
    }

    // GPU surfaces floor is 2
    let ov = LimitOverrides {
        max_gpu_surfaces: Some(1),
        ..Default::default()
    };
    match ProtocolLimits::with_overrides(ov) {
        Err(LimitsError::BelowFloor {
            field,
            value,
            floor,
        }) => {
            assert_eq!(field, LimitField::GpuSurfaces);
            assert_eq!(value, 1);
            assert_eq!(floor, 2);
        }
        other => panic!("expected BelowFloor, got {other:?}"),
    }

    // Bandwidth floor is 1 Mbps
    let ov = LimitOverrides {
        max_bandwidth_bps: Some(999_999),
        ..Default::default()
    };
    match ProtocolLimits::with_overrides(ov) {
        Err(LimitsError::BelowFloor {
            field,
            value,
            floor,
        }) => {
            assert_eq!(field, LimitField::BandwidthBps);
            assert_eq!(value, 999_999);
            assert_eq!(floor, 1_000_000);
        }
        other => panic!("expected BelowFloor, got {other:?}"),
    }
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

    // Cursor dimension: exceeding ceiling rejected
    let too_wide = limits.max_cursor_dimension_pixels() + 1;
    assert!(matches!(
        limits.validate_cursor_dimensions(too_wide, 64),
        Err(LimitsError::AboveCeiling {
            field: LimitField::CursorDimensionPixels,
            ..
        })
    ));

    // Cursor shape length
    let too_large_cursor = limits.max_cursor_shape_bytes() as usize + 1;
    assert!(matches!(
        limits.validate_cursor_shape_len(too_large_cursor),
        Err(LimitsError::AboveCeiling {
            field: LimitField::CursorShapeBytes,
            ..
        })
    ));

    // Name length
    let too_long_name = limits.max_name_bytes() + 1;
    assert!(matches!(
        limits.validate_name_len(too_long_name),
        Err(LimitsError::AboveCeiling {
            field: LimitField::NameBytes,
            ..
        })
    ));

    // Parameter set length
    let too_large_ps = limits.max_parameter_set_bytes() as usize + 1;
    assert!(matches!(
        limits.validate_parameter_set_len(too_large_ps),
        Err(LimitsError::AboveCeiling {
            field: LimitField::ParameterSetBytes,
            ..
        })
    ));

    // Fragment count
    let too_many_fragments = limits.max_fragments_per_access_unit() + 1;
    assert!(matches!(
        limits.validate_fragment_count(too_many_fragments),
        Err(LimitsError::AboveCeiling {
            field: LimitField::FragmentsPerAccessUnit,
            ..
        })
    ));
}

#[test]
fn protocol_limits_concurrency_validations_reject_violations() {
    let limits = ProtocolLimits::ABSOLUTE;

    // Retained receipts
    let too_many_receipts = limits.max_retained_receipts() + 1;
    assert!(matches!(
        limits.validate_retained_receipts(too_many_receipts),
        Err(LimitsError::AboveCeiling {
            field: LimitField::RetainedReceipts,
            ..
        })
    ));

    // Handshake concurrency
    let too_many_handshakes = limits.max_concurrent_handshakes();
    assert!(matches!(
        limits.validate_handshake_concurrency(too_many_handshakes),
        Err(LimitsError::AboveCeiling {
            field: LimitField::ConcurrentHandshakes,
            ..
        })
    ));

    // Pending approvals
    let too_many_approvals = limits.max_pending_approvals();
    assert!(matches!(
        limits.validate_pending_approvals(too_many_approvals),
        Err(LimitsError::AboveCeiling {
            field: LimitField::PendingApprovals,
            ..
        })
    ));

    // Half-attached channels
    let too_many_channels = limits.max_half_attached_channels();
    assert!(matches!(
        limits.validate_half_attached_channels(too_many_channels),
        Err(LimitsError::AboveCeiling {
            field: LimitField::HalfAttachedChannels,
            ..
        })
    ));

    // Encoder sessions
    let too_many_encoders = limits.max_encoder_sessions();
    assert!(matches!(
        limits.validate_encoder_sessions(too_many_encoders),
        Err(LimitsError::AboveCeiling {
            field: LimitField::EncoderSessions,
            ..
        })
    ));

    // GPU surfaces
    let too_many_surfaces = limits.max_gpu_surfaces();
    assert!(matches!(
        limits.validate_gpu_surfaces(too_many_surfaces),
        Err(LimitsError::AboveCeiling {
            field: LimitField::GpuSurfaces,
            ..
        })
    ));

    // Viewers
    let too_many_viewers = limits.max_viewers();
    assert!(matches!(
        limits.validate_viewers(too_many_viewers),
        Err(LimitsError::AboveCeiling {
            field: LimitField::Viewers,
            ..
        })
    ));
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
    match accountant.acquire_encoder_session(&limits) {
        Err(DosRefusal::CapacityExceeded { field, .. }) => {
            assert_eq!(field, LimitField::EncoderSessions);
        }
        other => panic!("expected CapacityExceeded, got {other:?}"),
    }

    // GPU surfaces ceiling enforcement
    assert!(
        accountant
            .acquire_gpu_surfaces(limits.max_gpu_surfaces(), &limits)
            .is_ok()
    );
    match accountant.acquire_gpu_surfaces(1, &limits) {
        Err(DosRefusal::CapacityExceeded { field, .. }) => {
            assert_eq!(field, LimitField::GpuSurfaces);
        }
        other => panic!("expected CapacityExceeded, got {other:?}"),
    }

    // Half-attached channels ceiling enforcement
    for _ in 0..limits.max_half_attached_channels() {
        assert!(accountant.acquire_half_attached(&limits).is_ok());
    }
    match accountant.acquire_half_attached(&limits) {
        Err(DosRefusal::CapacityExceeded { field, .. }) => {
            assert_eq!(field, LimitField::HalfAttachedChannels);
        }
        other => panic!("expected CapacityExceeded, got {other:?}"),
    }

    // Pending approvals ceiling enforcement
    for _ in 0..limits.max_pending_approvals() {
        assert!(accountant.acquire_pending_approval(&limits).is_ok());
    }
    match accountant.acquire_pending_approval(&limits) {
        Err(DosRefusal::CapacityExceeded { field, .. }) => {
            assert_eq!(field, LimitField::PendingApprovals);
        }
        other => panic!("expected CapacityExceeded, got {other:?}"),
    }

    // Bandwidth allocation
    let remaining = limits.max_bandwidth_bps();
    assert!(accountant.allocate_bandwidth(remaining, &limits).is_ok());
    match accountant.allocate_bandwidth(1, &limits) {
        Err(DosRefusal::CapacityExceeded { field, .. }) => {
            assert_eq!(field, LimitField::BandwidthBps);
        }
        other => panic!("expected CapacityExceeded, got {other:?}"),
    }

    // Viewer memory allocation
    let viewer_limit = limits.per_viewer_compressed_bytes();
    assert!(
        accountant
            .allocate_viewer_memory(session, viewer_limit, &limits)
            .is_ok()
    );
    match accountant.allocate_viewer_memory(session, 1, &limits) {
        Err(DosRefusal::CapacityExceeded { field, .. }) => {
            assert_eq!(field, LimitField::PerViewerCompressedBytes);
        }
        other => panic!("expected CapacityExceeded, got {other:?}"),
    }
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
    match registry.check_diagnostic_export(t0) {
        Err(DosRefusal::RateLimitExceeded { field, .. }) => {
            assert_eq!(field, LimitField::DiagnosticExportsPerMin);
        }
        other => panic!("expected RateLimitExceeded, got {other:?}"),
    }

    // Decoder reconfigurations: capped at 5
    for _ in 0..5 {
        assert!(registry.check_decoder_reconfig(t0).is_ok());
    }
    match registry.check_decoder_reconfig(t0) {
        Err(DosRefusal::RateLimitExceeded { field, .. }) => {
            assert_eq!(field, LimitField::DecoderReconfigurationsPerMin);
        }
        other => panic!("expected RateLimitExceeded, got {other:?}"),
    }

    // Worker restarts: capped at 3
    for _ in 0..3 {
        assert!(registry.check_worker_restart(t0).is_ok());
    }
    match registry.check_worker_restart(t0) {
        Err(DosRefusal::RateLimitExceeded { field, .. }) => {
            assert_eq!(field, LimitField::WorkerRestartsPerMin);
        }
        other => panic!("expected RateLimitExceeded, got {other:?}"),
    }
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
