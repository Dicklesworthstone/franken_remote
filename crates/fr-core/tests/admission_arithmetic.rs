#![forbid(unsafe_code)]
#![allow(clippy::too_many_lines)]

//! Unit tests for admission arithmetic (sessions, surfaces, per-viewer bytes) with planted over-budget cases
//! (plan §§2.2, 7.1, 15.5; bead fr-p2-viewer-admission-e62).
//!
//! Acceptance criteria verified:
//! 1. Sessions admission arithmetic (viewers, encoder sessions, half-attached channels, pending approvals)
//!    with planted over-budget capacity enforcement.
//! 2. GPU surfaces and DPB memory arithmetic (`PreAllocValidator`) with planted over-budget cases.
//! 3. Per-viewer compressed bytes and bandwidth arithmetic with planted over-budget cases,
//!    multi-viewer independence, and saturating underflow protection.
//! 4. Metadata fragment count bounds (`MetadataFragmentValidator`) preventing `DoS` amplification.
//! 5. Multi-viewer shared pipeline admission matrix (1 controller + 2 observers) modeling
//!    shared encoder ownership, independent degradation, and seat reuse after disconnect.
//! 6. Admission arithmetic under custom administrator downward-overridden `ProtocolLimits`.

use fr_core::dos::{AdmissionAccountant, DosRefusal, MetadataFragmentValidator, PreAllocValidator};
use fr_core::ids::RemoteSessionId;
use fr_core::limits::{LimitField, LimitOverrides, ProtocolLimits};

#[test]
fn test_sessions_admission_arithmetic_planted_over_budget() {
    let limits = ProtocolLimits::ABSOLUTE;
    let mut accountant = AdmissionAccountant::new();

    // -----------------------------------------------------------------------
    // 1. Viewer Seats (plan §15.5: 1 controller + 2 observers = 3 viewers)
    // -----------------------------------------------------------------------
    let max_viewers = limits.max_viewers();
    assert_eq!(max_viewers, 3);
    assert_eq!(accountant.active_viewers(), 0);

    for i in 0..max_viewers {
        let vid = RemoteSessionId::from_raw(u128::from(i + 100));
        assert!(accountant.acquire_viewer(vid, &limits).is_ok());
    }
    assert_eq!(accountant.active_viewers(), max_viewers);

    // Planted over-budget case: 4th viewer attempt must be refused with CapacityExceeded
    let v_over = RemoteSessionId::from_raw(999);
    assert_eq!(
        accountant.acquire_viewer(v_over, &limits),
        Err(DosRefusal::CapacityExceeded {
            field: LimitField::Viewers,
            current: u64::from(max_viewers),
            limit: u64::from(max_viewers),
        })
    );
    assert_eq!(accountant.active_viewers(), max_viewers);

    // Releasing viewer 101 drops active viewers by 1
    let v_first = RemoteSessionId::from_raw(100);
    accountant.release_viewer(v_first);
    assert_eq!(accountant.active_viewers(), max_viewers - 1);

    // Now viewer v_over can be admitted into the vacated seat
    assert!(accountant.acquire_viewer(v_over, &limits).is_ok());
    assert_eq!(accountant.active_viewers(), max_viewers);

    // Releasing an unknown viewer session ID is a safe no-op
    let unknown = RemoteSessionId::from_raw(8888);
    accountant.release_viewer(unknown);
    assert_eq!(accountant.active_viewers(), max_viewers);

    // -----------------------------------------------------------------------
    // 2. Encoder Sessions
    // -----------------------------------------------------------------------
    let max_encoders = limits.max_encoder_sessions();
    assert_eq!(accountant.active_encoder_sessions(), 0);

    for _ in 0..max_encoders {
        assert!(accountant.acquire_encoder_session(&limits).is_ok());
    }
    assert_eq!(accountant.active_encoder_sessions(), max_encoders);

    // Planted over-budget case: (max_encoders + 1) encoder session must be refused
    assert_eq!(
        accountant.acquire_encoder_session(&limits),
        Err(DosRefusal::CapacityExceeded {
            field: LimitField::EncoderSessions,
            current: u64::from(max_encoders),
            limit: u64::from(max_encoders),
        })
    );
    assert_eq!(accountant.active_encoder_sessions(), max_encoders);

    // Release and re-acquire
    accountant.release_encoder_session();
    assert_eq!(accountant.active_encoder_sessions(), max_encoders - 1);
    assert!(accountant.acquire_encoder_session(&limits).is_ok());
    assert_eq!(accountant.active_encoder_sessions(), max_encoders);

    // Release all, verify saturating at 0 without underflow
    for _ in 0..max_encoders + 5 {
        accountant.release_encoder_session();
    }
    assert_eq!(accountant.active_encoder_sessions(), 0);

    // -----------------------------------------------------------------------
    // 3. Half-Attached Channels
    // -----------------------------------------------------------------------
    let max_half = limits.max_half_attached_channels();
    assert_eq!(accountant.active_half_attached(), 0);

    for _ in 0..max_half {
        assert!(accountant.acquire_half_attached(&limits).is_ok());
    }
    assert_eq!(accountant.active_half_attached(), max_half);

    // Planted over-budget case: (max_half + 1) half-attached channel must be refused
    assert_eq!(
        accountant.acquire_half_attached(&limits),
        Err(DosRefusal::CapacityExceeded {
            field: LimitField::HalfAttachedChannels,
            current: u64::from(max_half),
            limit: u64::from(max_half),
        })
    );
    assert_eq!(accountant.active_half_attached(), max_half);

    accountant.release_half_attached();
    assert_eq!(accountant.active_half_attached(), max_half - 1);

    // -----------------------------------------------------------------------
    // 4. Pending Approvals
    // -----------------------------------------------------------------------
    let max_approvals = limits.max_pending_approvals();
    assert_eq!(accountant.active_pending_approvals(), 0);

    for _ in 0..max_approvals {
        assert!(accountant.acquire_pending_approval(&limits).is_ok());
    }
    assert_eq!(accountant.active_pending_approvals(), max_approvals);

    // Planted over-budget case: (max_approvals + 1) pending approval must be refused
    assert_eq!(
        accountant.acquire_pending_approval(&limits),
        Err(DosRefusal::CapacityExceeded {
            field: LimitField::PendingApprovals,
            current: u64::from(max_approvals),
            limit: u64::from(max_approvals),
        })
    );
    assert_eq!(accountant.active_pending_approvals(), max_approvals);

    accountant.release_pending_approval();
    assert_eq!(accountant.active_pending_approvals(), max_approvals - 1);
}

#[test]
fn test_surfaces_and_dpb_memory_admission_arithmetic_planted_over_budget() {
    let limits = ProtocolLimits::ABSOLUTE;
    let mut accountant = AdmissionAccountant::new();

    // -----------------------------------------------------------------------
    // 1. GPU Surfaces
    // -----------------------------------------------------------------------
    let max_surfaces = limits.max_gpu_surfaces();
    assert_eq!(accountant.active_gpu_surfaces(), 0);

    let half = max_surfaces / 2;
    assert!(accountant.acquire_gpu_surfaces(half, &limits).is_ok());
    assert_eq!(accountant.active_gpu_surfaces(), half);

    assert!(
        accountant
            .acquire_gpu_surfaces(max_surfaces - half, &limits)
            .is_ok()
    );
    assert_eq!(accountant.active_gpu_surfaces(), max_surfaces);

    // Planted over-budget case: acquiring even 1 additional GPU surface is refused
    assert_eq!(
        accountant.acquire_gpu_surfaces(1, &limits),
        Err(DosRefusal::CapacityExceeded {
            field: LimitField::GpuSurfaces,
            current: u64::from(max_surfaces),
            limit: u64::from(max_surfaces),
        })
    );
    assert_eq!(accountant.active_gpu_surfaces(), max_surfaces);

    // Planted over-budget case: huge count saturating u32 does not wrap or panic
    assert!(matches!(
        accountant.acquire_gpu_surfaces(u32::MAX, &limits),
        Err(DosRefusal::CapacityExceeded {
            field: LimitField::GpuSurfaces,
            ..
        })
    ));
    assert_eq!(accountant.active_gpu_surfaces(), max_surfaces);

    // Release 6 surfaces
    accountant.release_gpu_surfaces(6);
    assert_eq!(accountant.active_gpu_surfaces(), max_surfaces - 6);

    // Re-acquire 6 surfaces
    assert!(accountant.acquire_gpu_surfaces(6, &limits).is_ok());
    assert_eq!(accountant.active_gpu_surfaces(), max_surfaces);

    // Release more than allocated saturates at 0 without underflow
    accountant.release_gpu_surfaces(100);
    assert_eq!(accountant.active_gpu_surfaces(), 0);

    // -----------------------------------------------------------------------
    // 2. PreAllocValidator: Decoded Picture Buffer (DPB) Memory Arithmetic
    // Formula: width * height * 1.5 * (bit_depth / 8) * (reference_surfaces + 2)
    // -----------------------------------------------------------------------

    // Case A: 1280x720, 8-bit, 1 reference surface
    // frame_bytes = 1280 * 720 * 3 / 2 = 1,382,400 bytes
    // total_buffers = 1 ref + 2 working = 3
    // total_required = 1,382,400 * 3 = 4,147,200 bytes (~3.95 MiB)
    let res_720p = PreAllocValidator::validate_surface_allocation(1280, 720, 8, 1, &limits);
    assert_eq!(res_720p, Ok(4_147_200));

    // Case B: 1280x720, 10-bit, 1 reference surface
    // bpp_numerator = 4 -> frame_bytes = 1280 * 720 * 4 / 2 = 1,843,200 bytes
    // total_buffers = 3 -> total_required = 1,843,200 * 3 = 5,529,600 bytes (~5.27 MiB)
    let res_720p_10bit = PreAllocValidator::validate_surface_allocation(1280, 720, 10, 1, &limits);
    assert_eq!(res_720p_10bit, Ok(5_529_600));

    // Case C: 1920x1080, 8-bit, 2 reference surfaces
    // frame_bytes = 1920 * 1080 * 3 / 2 = 3,110,400 bytes
    // total_buffers = 2 ref + 2 working = 4
    // total_required = 3,110,400 * 4 = 12,441,600 bytes (~11.86 MiB)
    // Fits within ABSOLUTE per_viewer_compressed_bytes (33,554,432 bytes)
    let res_1080p = PreAllocValidator::validate_surface_allocation(1920, 1080, 8, 2, &limits);
    assert_eq!(res_1080p, Ok(12_441_600));

    // Planted over-budget case D: 4K (3840x2160), 10-bit, 4 reference surfaces
    // frame_bytes = 3840 * 2160 * 4 / 2 = 16,588,800 bytes
    // total_buffers = 4 ref + 2 working = 6
    // total_required = 16,588,800 * 6 = 99,532,800 bytes (~94.92 MiB)
    // Exceeds 32 MiB ABSOLUTE limit (33,554,432) -> must be refused!
    assert_eq!(
        PreAllocValidator::validate_surface_allocation(3840, 2160, 10, 4, &limits),
        Err(DosRefusal::DecoderAllocationExceeded {
            requested_bytes: 99_532_800,
            max_bytes: limits.per_viewer_compressed_bytes(),
        })
    );

    // Planted over-budget case E: Excessive dimension exceeds max_dimension_pixels
    let excessive_dim = limits.max_dimension_pixels() + 1;
    assert!(matches!(
        PreAllocValidator::validate_surface_allocation(excessive_dim, 1080, 8, 1, &limits),
        Err(DosRefusal::CursorTooLarge { width, .. }) if width == excessive_dim
    ));
}

#[test]
fn test_per_viewer_compressed_bytes_and_bandwidth_admission_arithmetic_planted_over_budget() {
    let limits = ProtocolLimits::ABSOLUTE;
    let mut accountant = AdmissionAccountant::new();

    let v1 = RemoteSessionId::from_raw(201);
    let v2 = RemoteSessionId::from_raw(202);

    // -----------------------------------------------------------------------
    // 1. Memory allocation to unadmitted session must be refused
    // -----------------------------------------------------------------------
    assert!(matches!(
        accountant.allocate_viewer_memory(v1, 1024, &limits),
        Err(DosRefusal::CapacityExceeded {
            field: LimitField::Viewers,
            ..
        })
    ));

    // Admit both viewers
    assert!(accountant.acquire_viewer(v1, &limits).is_ok());
    assert!(accountant.acquire_viewer(v2, &limits).is_ok());

    // -----------------------------------------------------------------------
    // 2. Per-Viewer Compressed Memory Budget
    // -----------------------------------------------------------------------
    let max_viewer_bytes = limits.per_viewer_compressed_bytes();

    // Allocate half budget to Viewer 1
    let half_budget = max_viewer_bytes / 2;
    assert!(
        accountant
            .allocate_viewer_memory(v1, half_budget, &limits)
            .is_ok()
    );

    // Allocate almost the remainder (leaving 100 bytes)
    let remainder = max_viewer_bytes - half_budget;
    assert!(
        accountant
            .allocate_viewer_memory(v1, remainder - 100, &limits)
            .is_ok()
    );

    // Planted over-budget case: allocate 101 bytes (exceeds budget by 1 byte)
    assert_eq!(
        accountant.allocate_viewer_memory(v1, 101, &limits),
        Err(DosRefusal::CapacityExceeded {
            field: LimitField::PerViewerCompressedBytes,
            current: max_viewer_bytes - 100,
            limit: max_viewer_bytes,
        })
    );

    // Allocate exact remaining 100 bytes (hits exactly 100% capacity)
    assert!(accountant.allocate_viewer_memory(v1, 100, &limits).is_ok());

    // Planted over-budget case: now even 1 additional byte is refused
    assert_eq!(
        accountant.allocate_viewer_memory(v1, 1, &limits),
        Err(DosRefusal::CapacityExceeded {
            field: LimitField::PerViewerCompressedBytes,
            current: max_viewer_bytes,
            limit: max_viewer_bytes,
        })
    );

    // -----------------------------------------------------------------------
    // 3. Multi-Viewer Independence
    // Viewer 1 reaching its budget MUST NOT starve Viewer 2's budget
    // -----------------------------------------------------------------------
    // Allocate full budget to Viewer 2
    assert!(
        accountant
            .allocate_viewer_memory(v2, max_viewer_bytes, &limits)
            .is_ok()
    );

    // Planted over-budget case on Viewer 2
    assert!(accountant.allocate_viewer_memory(v2, 1, &limits).is_err());

    // Release 1,000,000 bytes from Viewer 1
    accountant.release_viewer_memory(v1, 1_000_000);
    // Can now allocate 500,000 bytes to Viewer 1
    assert!(
        accountant
            .allocate_viewer_memory(v1, 500_000, &limits)
            .is_ok()
    );

    // Release more bytes than allocated saturates at 0 without underflow
    accountant.release_viewer_memory(v1, u64::MAX);
    // Can now allocate full budget to Viewer 1 again
    assert!(
        accountant
            .allocate_viewer_memory(v1, max_viewer_bytes, &limits)
            .is_ok()
    );

    // -----------------------------------------------------------------------
    // 4. Bandwidth Allocation
    // -----------------------------------------------------------------------
    let max_bw = limits.max_bandwidth_bps();
    assert_eq!(accountant.allocated_bandwidth_bps(), 0);

    let half_bw = max_bw / 2;
    assert!(accountant.allocate_bandwidth(half_bw, &limits).is_ok());
    assert_eq!(accountant.allocated_bandwidth_bps(), half_bw);

    let remaining_bw = max_bw - half_bw;
    assert!(accountant.allocate_bandwidth(remaining_bw, &limits).is_ok());
    assert_eq!(accountant.allocated_bandwidth_bps(), max_bw);

    // Planted over-budget case: allocate 1 bps over budget
    assert_eq!(
        accountant.allocate_bandwidth(1, &limits),
        Err(DosRefusal::CapacityExceeded {
            field: LimitField::BandwidthBps,
            current: max_bw,
            limit: max_bw,
        })
    );
    assert_eq!(accountant.allocated_bandwidth_bps(), max_bw);

    // Release half
    accountant.release_bandwidth(half_bw);
    assert_eq!(accountant.allocated_bandwidth_bps(), max_bw - half_bw);

    // Release more than allocated saturates at 0 without underflow
    accountant.release_bandwidth(u64::MAX);
    assert_eq!(accountant.allocated_bandwidth_bps(), 0);
}

#[test]
fn test_metadata_fragment_admission_arithmetic_planted_over_budget() {
    let limits = ProtocolLimits::ABSOLUTE;
    let max_frags = limits.max_fragments_per_access_unit();

    // Valid fragment configurations
    assert!(MetadataFragmentValidator::validate(1, 1000, &limits).is_ok());
    assert!(MetadataFragmentValidator::validate(max_frags / 2, 50_000, &limits).is_ok());
    assert!(MetadataFragmentValidator::validate(max_frags, 100_000, &limits).is_ok());

    // Planted over-budget case 1: (max_frags + 1) exceeds limit
    assert_eq!(
        MetadataFragmentValidator::validate(max_frags + 1, 100_000, &limits),
        Err(DosRefusal::ExcessiveMetadataFragments {
            count: max_frags + 1,
            max: max_frags,
        })
    );

    // Planted over-budget case 2: Multiple fragments declared with zero payload bytes (anti-amplification DoS)
    assert_eq!(
        MetadataFragmentValidator::validate(2, 0, &limits),
        Err(DosRefusal::ExcessiveMetadataFragments { count: 2, max: 1 })
    );

    // Single zero-byte fragment is permitted (e.g. empty keepalive / EOS)
    assert!(MetadataFragmentValidator::validate(1, 0, &limits).is_ok());
}

#[test]
fn test_multi_viewer_shared_pipeline_and_handoff_arithmetic_matrix() {
    let limits = ProtocolLimits::ABSOLUTE;
    let mut accountant = AdmissionAccountant::new();

    // -----------------------------------------------------------------------
    // Topology (plan §15.5): 1 Controller (c1) + 2 Read-Only Observers (o2, o3)
    // Shared encoder: 1 encoder session serves all 3 viewers
    // -----------------------------------------------------------------------
    let c1 = RemoteSessionId::from_raw(301);
    let o2 = RemoteSessionId::from_raw(302);
    let o3 = RemoteSessionId::from_raw(303);

    // 1. Controller joins
    assert!(accountant.acquire_viewer(c1, &limits).is_ok());
    assert!(accountant.acquire_encoder_session(&limits).is_ok());
    assert!(accountant.acquire_gpu_surfaces(4, &limits).is_ok());
    assert!(accountant.allocate_bandwidth(15_000_000, &limits).is_ok());
    assert!(
        accountant
            .allocate_viewer_memory(c1, 3_000_000, &limits)
            .is_ok()
    );

    // 2. Observer 2 joins shared pipeline
    // Reuses the existing encoder session (0 extra encoder sessions allocated)
    assert!(accountant.acquire_viewer(o2, &limits).is_ok());
    assert!(accountant.allocate_bandwidth(10_000_000, &limits).is_ok());
    assert!(
        accountant
            .allocate_viewer_memory(o2, 2_000_000, &limits)
            .is_ok()
    );

    // 3. Observer 3 joins shared pipeline
    assert!(accountant.acquire_viewer(o3, &limits).is_ok());
    assert!(accountant.allocate_bandwidth(10_000_000, &limits).is_ok());
    assert!(
        accountant
            .allocate_viewer_memory(o3, 2_000_000, &limits)
            .is_ok()
    );

    // Current state check
    assert_eq!(accountant.active_viewers(), 3);
    assert_eq!(accountant.active_encoder_sessions(), 1);
    assert_eq!(accountant.active_gpu_surfaces(), 4);
    assert_eq!(accountant.allocated_bandwidth_bps(), 35_000_000);

    // Planted over-budget case: 4th viewer attempt rejected (seats full)
    let o4 = RemoteSessionId::from_raw(304);
    assert_eq!(
        accountant.acquire_viewer(o4, &limits),
        Err(DosRefusal::CapacityExceeded {
            field: LimitField::Viewers,
            current: 3,
            limit: 3,
        })
    );

    // Planted over-budget case: allocating more than remaining bandwidth
    let remaining_bw = limits.max_bandwidth_bps() - accountant.allocated_bandwidth_bps();
    assert_eq!(
        accountant.allocate_bandwidth(remaining_bw + 1, &limits),
        Err(DosRefusal::CapacityExceeded {
            field: LimitField::BandwidthBps,
            current: 35_000_000,
            limit: limits.max_bandwidth_bps(),
        })
    );

    // 4. Observer o3 disconnects / degrades independently
    accountant.release_viewer(o3);
    accountant.release_bandwidth(10_000_000);
    // Note: encoder session and surfaces are NOT released because c1 and o2 still reference them
    assert_eq!(accountant.active_viewers(), 2);
    assert_eq!(accountant.active_encoder_sessions(), 1);
    assert_eq!(accountant.allocated_bandwidth_bps(), 25_000_000);

    // Now viewer o4 can be admitted into the freed seat
    assert!(accountant.acquire_viewer(o4, &limits).is_ok());
    assert!(accountant.allocate_bandwidth(10_000_000, &limits).is_ok());
    assert!(
        accountant
            .allocate_viewer_memory(o4, 2_000_000, &limits)
            .is_ok()
    );

    assert_eq!(accountant.active_viewers(), 3);
    assert_eq!(accountant.allocated_bandwidth_bps(), 35_000_000);

    // 5. Teardown all viewers: last unsubscribe stops encoder and frees surfaces
    accountant.release_viewer(o4);
    accountant.release_bandwidth(10_000_000);

    accountant.release_viewer(o2);
    accountant.release_bandwidth(10_000_000);

    accountant.release_viewer(c1);
    accountant.release_bandwidth(15_000_000);

    // Last subscriber unsubscribed: release shared pipeline resources
    accountant.release_encoder_session();
    accountant.release_gpu_surfaces(4);

    assert_eq!(accountant.active_viewers(), 0);
    assert_eq!(accountant.active_encoder_sessions(), 0);
    assert_eq!(accountant.active_gpu_surfaces(), 0);
    assert_eq!(accountant.allocated_bandwidth_bps(), 0);
}

#[test]
fn test_admission_arithmetic_under_custom_downward_limits() {
    // Test that administrator downward overrides are strictly respected by AdmissionAccountant
    let overrides = LimitOverrides {
        max_viewers: Some(2),
        max_encoder_sessions: Some(1),
        max_gpu_surfaces: Some(8),
        max_encoded_access_unit_bytes: Some(2 * 1024 * 1024), // 2 MiB
        per_viewer_compressed_bytes: Some(4 * 1024 * 1024), // 4 MiB (>= max_encoded_access_unit_bytes)
        max_bandwidth_bps: Some(25_000_000),                // 25 Mbps
        ..Default::default()
    };
    let custom_limits = ProtocolLimits::with_overrides(overrides).expect("valid overrides");

    let mut accountant = AdmissionAccountant::new();

    let v1 = RemoteSessionId::from_raw(401);
    let v2 = RemoteSessionId::from_raw(402);
    let v3 = RemoteSessionId::from_raw(403);

    // 1. Viewer capacity clamped to 2
    assert!(accountant.acquire_viewer(v1, &custom_limits).is_ok());
    assert!(accountant.acquire_viewer(v2, &custom_limits).is_ok());
    // Planted over-budget case: 3rd viewer refused under max_viewers = 2
    assert_eq!(
        accountant.acquire_viewer(v3, &custom_limits),
        Err(DosRefusal::CapacityExceeded {
            field: LimitField::Viewers,
            current: 2,
            limit: 2,
        })
    );

    // 2. Encoder session capacity clamped to 1
    assert!(accountant.acquire_encoder_session(&custom_limits).is_ok());
    // Planted over-budget case: 2nd encoder session refused under max_encoder_sessions = 1
    assert_eq!(
        accountant.acquire_encoder_session(&custom_limits),
        Err(DosRefusal::CapacityExceeded {
            field: LimitField::EncoderSessions,
            current: 1,
            limit: 1,
        })
    );

    // 3. GPU surfaces clamped to 8
    assert!(accountant.acquire_gpu_surfaces(8, &custom_limits).is_ok());
    assert_eq!(
        accountant.acquire_gpu_surfaces(1, &custom_limits),
        Err(DosRefusal::CapacityExceeded {
            field: LimitField::GpuSurfaces,
            current: 8,
            limit: 8,
        })
    );

    // 4. Per-viewer compressed memory clamped to 4 MiB (4,194,304 bytes)
    assert!(
        accountant
            .allocate_viewer_memory(v1, 4_194_304, &custom_limits)
            .is_ok()
    );
    assert_eq!(
        accountant.allocate_viewer_memory(v1, 1, &custom_limits),
        Err(DosRefusal::CapacityExceeded {
            field: LimitField::PerViewerCompressedBytes,
            current: 4_194_304,
            limit: 4_194_304,
        })
    );

    // 5. Bandwidth clamped to 25 Mbps (25,000,000 bps)
    assert!(
        accountant
            .allocate_bandwidth(25_000_000, &custom_limits)
            .is_ok()
    );
    assert_eq!(
        accountant.allocate_bandwidth(1, &custom_limits),
        Err(DosRefusal::CapacityExceeded {
            field: LimitField::BandwidthBps,
            current: 25_000_000,
            limit: 25_000_000,
        })
    );
}
