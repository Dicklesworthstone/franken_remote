//! Contract conformance and integration test suite for the macOS host adapter (bead fr-p1-host-macos-vif).
//!
//! Acceptance criteria:
//! - "the fr-media fake-backend contract-conformance suite runs against this adapter"
//!
//! Exercises:
//! 1. `VideoToolboxEncoder` Encoder contract conformance (IDR-first, backpressure, force IDR,
//!    wrong-backend rejection, device loss terminality).
//! 2. `ScreenCaptureKit` capture borrowed-buffer discipline and copy ledger accounting.
//! 3. Damage metadata tracking (dirty rects).
//! 4. Protected content detection reported as typed capability event, never as network stall.
//! 5. Permission revocation reported as typed refusal.
//! 6. Thread/queue affinity for `ScreenCaptureKit` callbacks and `VideoToolbox` completions.

use fr_core::{ids::CodecConfigurationGeneration, limits::ProtocolLimits};
use fr_media::{
    access_unit::FrameKind,
    codec::{EncodeRequest, Encoder, MediaError},
    config::{CodecConfiguration, CodedGeometry, ColorInfo, GopPolicy},
    surface::{CopyKind, PixelFormat, SurfaceBackend},
};
use fr_native::macos::{
    CursorCaptureMode, DamageRect, MacOsCaptureError, MacOsCaptureEvent, SckCapture,
    SckCaptureConfig, VideoToolboxEncoder, VideoToolboxSurface,
};

fn test_config(generation: u64) -> CodecConfiguration {
    let limits = ProtocolLimits::ABSOLUTE;
    let geom = CodedGeometry::new(&limits, 1920, 1088, 1920, 1080, 16).unwrap();
    let gop = GopPolicy::baseline_for_frame_rate(60).unwrap();
    CodecConfiguration::new_baseline(
        CodecConfigurationGeneration::from_raw(generation),
        geom,
        ColorInfo::sdr_bt709(),
        gop,
    )
    .unwrap()
}

fn vt_surface() -> VideoToolboxSurface {
    VideoToolboxSurface::new(PixelFormat::Nv12, 1920, 1080, 0x1234_5678)
}

// ---------------------------------------------------------------------------
// Encoder Contract Conformance Tests
// ---------------------------------------------------------------------------

#[test]
fn vt_encoder_must_be_configured_before_submit() {
    let mut enc = VideoToolboxEncoder::new(ProtocolLimits::ABSOLUTE, 4);
    assert_eq!(
        enc.submit(&vt_surface(), EncodeRequest::default()),
        Err(MediaError::NotConfigured)
    );
    assert!(enc.configuration().is_none());
}

#[test]
fn vt_encoder_first_output_after_configure_is_an_idr() {
    let mut enc = VideoToolboxEncoder::new(ProtocolLimits::ABSOLUTE, 4);
    enc.configure(test_config(0)).unwrap();
    enc.submit(&vt_surface(), EncodeRequest::default()).unwrap();
    let au = enc.poll_output().unwrap();
    assert!(
        au.is_idr(),
        "the first access unit after configure must be an IDR"
    );
    assert_eq!(
        au.config_generation(),
        CodecConfigurationGeneration::from_raw(0)
    );

    // Second submit -> predicted frame referencing prior frame
    enc.submit(&vt_surface(), EncodeRequest::default()).unwrap();
    let p = enc.poll_output().unwrap();
    assert!(
        matches!(p.kind(), FrameKind::Predicted { references } if references == au.frame()),
        "second frame should be predicted referencing prior frame"
    );
}

#[test]
fn vt_encoder_force_idr_advances_recovery_generation() {
    let mut enc = VideoToolboxEncoder::new(ProtocolLimits::ABSOLUTE, 4);
    enc.configure(test_config(0)).unwrap();
    enc.submit(&vt_surface(), EncodeRequest::default()).unwrap();
    let idr0 = enc.poll_output().unwrap();

    enc.submit(&vt_surface(), EncodeRequest { force_idr: true })
        .unwrap();
    let idr1 = enc.poll_output().unwrap();
    assert!(idr1.is_idr());

    if let (FrameKind::Idr { recovery: r0 }, FrameKind::Idr { recovery: r1 }) =
        (idr0.kind(), idr1.kind())
    {
        assert!(r1.supersedes(r0), "each IDR advances recovery generation");
    } else {
        assert!(idr0.is_idr() && idr1.is_idr());
    }
}

#[test]
fn vt_encoder_backpressure_requires_drain() {
    let mut enc = VideoToolboxEncoder::new(ProtocolLimits::ABSOLUTE, 2);
    enc.configure(test_config(0)).unwrap();
    enc.submit(&vt_surface(), EncodeRequest::default()).unwrap();
    enc.submit(&vt_surface(), EncodeRequest::default()).unwrap();

    // 3rd submit without drain triggers Backpressure
    assert_eq!(
        enc.submit(&vt_surface(), EncodeRequest::default()),
        Err(MediaError::Backpressure)
    );

    // Drain one -> accepted again
    let _ = enc.poll_output().unwrap();
    assert_eq!(enc.submit(&vt_surface(), EncodeRequest::default()), Ok(()));
}

#[test]
fn vt_encoder_refuses_wrong_backend_surface() {
    let mut enc = VideoToolboxEncoder::new(ProtocolLimits::ABSOLUTE, 4);
    enc.configure(test_config(0)).unwrap();
    let foreign_surface = vt_surface().with_backend(SurfaceBackend::Direct3D11);
    assert_eq!(
        enc.submit(&foreign_surface, EncodeRequest::default()),
        Err(MediaError::WrongBackend {
            expected: SurfaceBackend::VideoToolbox,
            found: SurfaceBackend::Direct3D11,
        })
    );
}

#[test]
fn vt_encoder_device_loss_is_terminal() {
    let mut enc = VideoToolboxEncoder::new(ProtocolLimits::ABSOLUTE, 4);
    enc.configure(test_config(0)).unwrap();
    enc.simulate_device_lost();

    let err = enc
        .submit(&vt_surface(), EncodeRequest::default())
        .unwrap_err();
    assert_eq!(err, MediaError::DeviceLost);
    assert!(err.is_terminal(), "device loss must be terminal");
}

// ---------------------------------------------------------------------------
// ScreenCaptureKit & VideoToolbox Integration Tests
// ---------------------------------------------------------------------------

#[test]
fn sck_borrowed_buffer_discipline_and_copy_accounting() {
    let config = SckCaptureConfig::new_display(1, 1920, 1080, 60);
    let mut capture = SckCapture::new(config, true);
    capture.start().expect("start capture");

    let dirty_rects = [DamageRect {
        x: 0,
        y: 0,
        width: 100,
        height: 100,
    }];

    // Process borrowed sample buffer: takes owned copy, records in ledger
    let surface = capture
        .process_borrowed_sample_buffer(0xcafe_babe, &dirty_rects, false)
        .expect("process sample buffer");

    assert_eq!(surface.backend(), SurfaceBackend::VideoToolbox);
    assert_eq!(surface.format(), PixelFormat::Nv12);
    assert_eq!(surface.width(), 1920);
    assert_eq!(surface.height(), 1080);

    let stats = capture.stats();
    assert_eq!(stats.captured_frames, 1);
    assert_eq!(stats.damage_rects_observed, 1);
    assert_eq!(stats.copy_ledger.count(CopyKind::CaptureToOwned), 1);
    assert!(
        stats.copy_ledger.is_readback_free(),
        "GPU surface copy must be readback free (pixels stay on GPU)"
    );
}

#[test]
fn sck_protected_content_reported_as_typed_event_never_stalls() {
    let config = SckCaptureConfig::new_display(1, 1920, 1080, 60);
    let mut capture = SckCapture::new(config, true);
    capture.start().unwrap();

    // Frame with protected content (DRM / FairPlay)
    let _ = capture
        .process_borrowed_sample_buffer(0x1000, &[], true)
        .unwrap();

    assert!(capture.is_protected_content_active());
    let events = capture.poll_events();
    assert_eq!(events, vec![MacOsCaptureEvent::ProtectedContentDetected]);

    // Subsequent regular frame clears protected content flag
    let _ = capture
        .process_borrowed_sample_buffer(0x2000, &[], false)
        .unwrap();
    assert!(!capture.is_protected_content_active());
}

#[test]
fn sck_permission_loss_reported_as_typed_refusal() {
    let config = SckCaptureConfig::new_display(1, 1920, 1080, 60);
    let mut capture = SckCapture::new(config.clone(), false); // No permission

    // Cannot start without permission
    assert_eq!(capture.start(), Err(MacOsCaptureError::PermissionDenied));

    // Permission revoked mid-stream
    let mut active_capture = SckCapture::new(config, true);
    active_capture.start().unwrap();
    active_capture.on_permission_revoked();

    assert!(!active_capture.is_capturing());
    let events = active_capture.poll_events();
    assert_eq!(events, vec![MacOsCaptureEvent::PermissionLoss]);

    // Subsequent sample buffer processing returns PermissionDenied
    let err = active_capture
        .process_borrowed_sample_buffer(0x3000, &[], false)
        .unwrap_err();
    assert_eq!(err, MacOsCaptureError::PermissionDenied);
}

#[test]
fn sck_and_vt_thread_affinity_queues_are_named_and_distinct() {
    let config = SckCaptureConfig::new_display(1, 1920, 1080, 60);
    let capture = SckCapture::new(config, true);
    let encoder = VideoToolboxEncoder::new(ProtocolLimits::ABSOLUTE, 4);

    assert_eq!(
        capture.dispatch_queue_name(),
        SckCapture::SCK_DISPATCH_QUEUE
    );
    assert_eq!(
        encoder.completion_queue_name(),
        VideoToolboxEncoder::VT_COMPLETION_QUEUE
    );
    assert_ne!(
        capture.dispatch_queue_name(),
        encoder.completion_queue_name(),
        "SCK callbacks and VT completions must have separate thread affinity queues"
    );
}

#[test]
fn cursor_capture_mode_and_damage_metadata() {
    let mut config = SckCaptureConfig::new_display(1, 1920, 1080, 60);
    config.cursor_mode = CursorCaptureMode::Separate;
    let mut capture = SckCapture::new(config, true);
    capture.start().unwrap();

    let rects = [
        DamageRect {
            x: 10,
            y: 10,
            width: 50,
            height: 50,
        },
        DamageRect {
            x: 100,
            y: 100,
            width: 200,
            height: 150,
        },
    ];

    let _ = capture
        .process_borrowed_sample_buffer(0x4000, &rects, false)
        .unwrap();

    assert_eq!(capture.stats().damage_rects_observed, 2);
}
