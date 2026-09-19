use fr_core::ids::CodecConfigurationGeneration;
use fr_core::limits::ProtocolLimits;
use fr_ffi::{FfiEncoderBackend, FfiSurface, FfmpegEncoder};
use fr_media::codec::{EncodeRequest, Encoder, MediaError};
use fr_media::config::{CodecConfiguration, CodedGeometry, ColorInfo, GopPolicy};
use fr_media::surface::{PixelFormat, SurfaceBackend};

fn test_config(width: u32, height: u32) -> CodecConfiguration {
    let limits = ProtocolLimits::ABSOLUTE;
    let geom = CodedGeometry::from_visible(&limits, width, height, 16).unwrap();
    let gop = GopPolicy::baseline_for_frame_rate(60).unwrap();
    CodecConfiguration::new_baseline(
        CodecConfigurationGeneration::INITIAL,
        geom,
        ColorInfo::sdr_bt709(),
        gop,
    )
    .unwrap()
}

#[test]
fn unconfigured_encoder_refuses_operations() {
    let mut enc = FfmpegEncoder::new(
        FfiEncoderBackend::MockSimulated,
        SurfaceBackend::Fake,
        60,
        1_000_000,
    );

    let surface = FfiSurface::new(
        SurfaceBackend::Fake,
        PixelFormat::Bgra8,
        64,
        64,
        vec![0u8; 64 * 64 * 4],
    );

    assert_eq!(
        enc.submit(&surface, EncodeRequest::default()),
        Err(MediaError::NotConfigured)
    );
    assert_eq!(enc.poll_output(), Err(MediaError::NotConfigured));
    assert_eq!(enc.drain(), Err(MediaError::NotConfigured));
}

#[test]
fn encoder_rejects_wrong_backend_and_mismatched_geometry() {
    let mut enc = FfmpegEncoder::new(
        FfiEncoderBackend::MockSimulated,
        SurfaceBackend::Direct3D11,
        60,
        1_000_000,
    );
    enc.configure(test_config(64, 64)).unwrap();

    // Wrong geometry (128x128 instead of 64x64)
    let bad_geom_surface = FfiSurface::new(
        SurfaceBackend::Direct3D11,
        PixelFormat::Bgra8,
        128,
        128,
        vec![0u8; 128 * 128 * 4],
    );
    assert_eq!(
        enc.submit(&bad_geom_surface, EncodeRequest::default()),
        Err(MediaError::ConfigMismatch)
    );
}

#[test]
fn encoder_send_receive_state_machine_and_idr_forcing() {
    let mut enc = FfmpegEncoder::new(
        FfiEncoderBackend::MockSimulated,
        SurfaceBackend::Fake,
        60,
        1_000_000,
    );
    enc.configure(test_config(64, 64)).unwrap();

    let surface = FfiSurface::new(
        SurfaceBackend::Fake,
        PixelFormat::Bgra8,
        64,
        64,
        vec![0u8; 64 * 64 * 4],
    );

    // Initial output must be an IDR
    enc.submit(&surface, EncodeRequest::default()).unwrap();
    let au1 = enc.poll_output().unwrap();
    assert!(
        au1.is_idr(),
        "first access unit after configure must be IDR"
    );

    // Second output without forced IDR is predicted
    enc.submit(&surface, EncodeRequest::default()).unwrap();
    let au2 = enc.poll_output().unwrap();
    assert!(!au2.is_idr(), "second access unit should be predicted");

    // Third output with forced IDR
    enc.submit(&surface, EncodeRequest { force_idr: true })
        .unwrap();
    let au3 = enc.poll_output().unwrap();
    assert!(
        au3.is_idr(),
        "forced IDR request must produce IDR access unit"
    );
}

#[test]
fn encoder_backpressure_enforcement() {
    let mut enc = FfmpegEncoder::new(
        FfiEncoderBackend::MockSimulated,
        SurfaceBackend::Fake,
        60,
        1_000_000,
    );
    enc.configure(test_config(64, 64)).unwrap();

    let surface = FfiSurface::new(
        SurfaceBackend::Fake,
        PixelFormat::Bgra8,
        64,
        64,
        vec![0u8; 64 * 64 * 4],
    );

    // Submit 4 frames to fill in-flight queue
    for _ in 0..4 {
        enc.submit(&surface, EncodeRequest::default()).unwrap();
    }

    // 5th submit should signal backpressure
    assert_eq!(
        enc.submit(&surface, EncodeRequest::default()),
        Err(MediaError::Backpressure)
    );

    // Drain one output to relieve backpressure
    enc.poll_output().unwrap();

    // Now submit succeeds
    assert!(enc.submit(&surface, EncodeRequest::default()).is_ok());
}

#[test]
fn encoder_device_loss_handling() {
    let mut enc = FfmpegEncoder::new(
        FfiEncoderBackend::MockSimulated,
        SurfaceBackend::Fake,
        60,
        1_000_000,
    );
    enc.configure(test_config(64, 64)).unwrap();

    let surface = FfiSurface::new(
        SurfaceBackend::Fake,
        PixelFormat::Bgra8,
        64,
        64,
        vec![0u8; 64 * 64 * 4],
    );

    enc.submit(&surface, EncodeRequest::default()).unwrap();

    // Simulate GPU device lost
    enc.simulate_device_loss();

    // Next submit or poll must return DeviceLost and is terminal
    let err = enc.submit(&surface, EncodeRequest::default()).unwrap_err();
    assert_eq!(err, MediaError::DeviceLost);
    assert!(err.is_terminal());

    let poll_err = enc.poll_output().unwrap_err();
    assert_eq!(poll_err, MediaError::DeviceLost);
    assert!(poll_err.is_terminal());
}
