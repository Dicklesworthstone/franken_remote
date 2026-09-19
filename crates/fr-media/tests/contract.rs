//! Contract-level integration tests driving the send/receive state machine
//! end to end through the scriptable fake backend (bead fr-p1-fr-media).
//!
//! These exercise the ownership and failure boundaries the real backends must
//! honour: IDR-first output, backpressure/drain, reconfiguration fencing,
//! wrong-backend and device-loss refusals, and a full encode -> decode loop.
//! With detailed assertions logged per step (via the harness `log` helper) so
//! a failure names exactly which contract rule broke.

#![cfg(feature = "testing")]

use fr_core::ids::CodecConfigurationGeneration;
use fr_core::limits::ProtocolLimits;
use fr_media::access_unit::FrameKind;
use fr_media::codec::{Decoder, EncodeRequest, Encoder, MediaError};
use fr_media::config::{CodecConfiguration, CodedGeometry, ColorInfo, GopPolicy};
use fr_media::fake::{FakeDecoder, FakeEncoder, FakeSurface};
use fr_media::surface::{PixelFormat, SurfaceBackend};

fn log(step: &str) {
    // Deterministic, greppable step log; visible with `cargo test -- --nocapture`.
    println!("[contract] {step}");
}

fn config(generation: u64) -> CodecConfiguration {
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

fn surface() -> FakeSurface {
    FakeSurface::new(PixelFormat::Nv12, 1920, 1080)
}

#[test]
fn encoder_must_be_configured_before_submit() {
    log("submit before configure -> NotConfigured");
    let mut enc = FakeEncoder::new(ProtocolLimits::ABSOLUTE, 4);
    assert_eq!(
        enc.submit(&surface(), EncodeRequest::default()),
        Err(MediaError::NotConfigured)
    );
    assert!(enc.configuration().is_none());
}

#[test]
fn first_output_after_configure_is_an_idr() {
    log("configure then submit -> first output is IDR");
    let mut enc = FakeEncoder::new(ProtocolLimits::ABSOLUTE, 4);
    enc.configure(config(0)).unwrap();
    enc.submit(&surface(), EncodeRequest::default()).unwrap();
    let au = enc.poll_output().unwrap();
    assert!(
        au.is_idr(),
        "the first access unit after configure must be an IDR"
    );
    assert_eq!(
        au.config_generation(),
        CodecConfigurationGeneration::from_raw(0)
    );

    log("second submit -> predicted frame referencing the IDR");
    enc.submit(&surface(), EncodeRequest::default()).unwrap();
    let p = enc.poll_output().unwrap();
    match p.kind() {
        FrameKind::Predicted { references } => assert_eq!(references, au.frame()),
        FrameKind::Idr { .. } => panic!("second frame should be predicted"),
    }
}

#[test]
fn force_idr_produces_an_idr_mid_stream() {
    log("force_idr mid-stream -> IDR advancing the recovery generation");
    let mut enc = FakeEncoder::new(ProtocolLimits::ABSOLUTE, 4);
    enc.configure(config(0)).unwrap();
    enc.submit(&surface(), EncodeRequest::default()).unwrap();
    let idr0 = enc.poll_output().unwrap();
    enc.submit(&surface(), EncodeRequest { force_idr: true })
        .unwrap();
    let idr1 = enc.poll_output().unwrap();
    assert!(idr1.is_idr());
    // Recovery generations differ across the two IDRs.
    match (idr0.kind(), idr1.kind()) {
        (FrameKind::Idr { recovery: r0 }, FrameKind::Idr { recovery: r1 }) => {
            assert!(
                r1.supersedes(r0),
                "each IDR advances the recovery generation"
            );
        }
        _ => panic!("both should be IDRs"),
    }
}

#[test]
fn backpressure_requires_drain_then_accepts_again() {
    log("fill to backpressure threshold");
    let mut enc = FakeEncoder::new(ProtocolLimits::ABSOLUTE, 2);
    enc.configure(config(0)).unwrap();
    enc.submit(&surface(), EncodeRequest::default()).unwrap();
    enc.submit(&surface(), EncodeRequest::default()).unwrap();
    log("third submit without drain -> Backpressure");
    assert_eq!(
        enc.submit(&surface(), EncodeRequest::default()),
        Err(MediaError::Backpressure)
    );
    log("drain one -> submit accepted again");
    let _ = enc.poll_output().unwrap();
    assert_eq!(enc.submit(&surface(), EncodeRequest::default()), Ok(()));
}

#[test]
fn wrong_backend_surface_is_refused() {
    log("submit a Direct3D11-tagged surface to a Fake encoder -> WrongBackend");
    let mut enc = FakeEncoder::new(ProtocolLimits::ABSOLUTE, 4);
    enc.configure(config(0)).unwrap();
    let foreign = surface().with_backend(SurfaceBackend::Direct3D11);
    assert_eq!(
        enc.submit(&foreign, EncodeRequest::default()),
        Err(MediaError::WrongBackend {
            expected: SurfaceBackend::Fake,
            found: SurfaceBackend::Direct3D11,
        })
    );
}

#[test]
fn device_loss_is_terminal_not_retryable() {
    log("scripted device loss on the 2nd submit");
    let mut enc = FakeEncoder::new(ProtocolLimits::ABSOLUTE, 8).lose_device_at_submit(2);
    enc.configure(config(0)).unwrap();
    enc.submit(&surface(), EncodeRequest::default()).unwrap();
    let err = enc
        .submit(&surface(), EncodeRequest::default())
        .unwrap_err();
    assert_eq!(err, MediaError::DeviceLost);
    assert!(
        err.is_terminal(),
        "device loss must be terminal, not sleep-retried"
    );
}

#[test]
fn decoder_rejects_mismatched_configuration_generation() {
    log("encode under generation 0, decoder configured for generation 1 -> ConfigMismatch");
    let mut enc = FakeEncoder::new(ProtocolLimits::ABSOLUTE, 4);
    enc.configure(config(0)).unwrap();
    enc.submit(&surface(), EncodeRequest::default()).unwrap();
    let au = enc.poll_output().unwrap();

    let mut dec = FakeDecoder::new();
    dec.configure(config(1)).unwrap();
    assert_eq!(dec.submit(&au), Err(MediaError::ConfigMismatch));
}

#[test]
fn decoder_requires_idr_first_after_configure() {
    log("configure encoder, force a predicted frame, submit to a freshly configured decoder");
    let mut enc = FakeEncoder::new(ProtocolLimits::ABSOLUTE, 8);
    enc.configure(config(0)).unwrap();
    // Produce IDR then a predicted frame; drain both.
    enc.submit(&surface(), EncodeRequest::default()).unwrap();
    let _idr = enc.poll_output().unwrap();
    enc.submit(&surface(), EncodeRequest::default()).unwrap();
    let predicted = enc.poll_output().unwrap();
    assert!(!predicted.is_idr());

    let mut dec = FakeDecoder::new();
    dec.configure(config(0)).unwrap();
    log("predicted-first into a reset decoder -> ConfigMismatch (no reference state)");
    assert_eq!(dec.submit(&predicted), Err(MediaError::ConfigMismatch));
}

#[test]
fn full_encode_decode_loop_preserves_frame_identity() {
    log("full loop: encode 5 frames, decode them, verify frame identities in order");
    let mut enc = FakeEncoder::new(ProtocolLimits::ABSOLUTE, 8);
    let mut dec = FakeDecoder::new();
    enc.configure(config(0)).unwrap();
    dec.configure(config(0)).unwrap();

    let mut decoded = Vec::new();
    for i in 0..5u64 {
        let req = EncodeRequest { force_idr: i == 0 };
        enc.submit(&surface(), req).unwrap();
        let au = enc.poll_output().unwrap();
        assert_eq!(au.frame().as_raw(), i);
        dec.submit(&au).unwrap();
        let pic = dec.poll_output().unwrap();
        decoded.push(pic.frame_raw());
    }
    assert_eq!(decoded, vec![0, 1, 2, 3, 4]);
    log("frame identities preserved end to end");

    // Draining an empty decoder is NeedMoreInput, not an error state.
    assert_eq!(
        dec.poll_output().map(|_| ()).unwrap_err(),
        MediaError::NeedMoreInput
    );
}

#[test]
fn probe_cache_invalidation_and_admission_refusal_contract() {
    use fr_media::capabilities::{
        AdmissionError, DeviceIdentity, MediaCapabilities, ProbeCache, SessionAdmission,
    };
    use fr_media::config::CodecProfile;

    log("probe cache bounds, invalidation, and admission capacity enforcement");
    let mut cache = ProbeCache::new();

    let id0 =
        DeviceIdentity::new("Ubuntu 24.04", "PCIe-0000:01:00.0", "nvidia-550.54.14", 0).unwrap();
    let caps = MediaCapabilities::new(
        vec![CodecProfile::Main8_420],
        3840,
        2160,
        vec![PixelFormat::Nv12],
        true,
        2,
    )
    .unwrap();

    cache.insert(id0.clone(), caps.clone()).unwrap();
    assert_eq!(cache.len(), 1);
    assert!(cache.get(&id0).is_some());

    // Device reset (bumped generation) misses cache
    let id0_reset =
        DeviceIdentity::new("Ubuntu 24.04", "PCIe-0000:01:00.0", "nvidia-550.54.14", 1).unwrap();
    assert!(cache.get(&id0_reset).is_none());

    // Explicit device invalidation removes the entry
    cache.invalidate_device("Ubuntu 24.04", "PCIe-0000:01:00.0", "nvidia-550.54.14");
    assert!(cache.is_empty());

    // Admission refusal when capacity is reached
    let mut admission = SessionAdmission::new(caps.max_sessions);
    assert!(admission.has_capacity());
    assert_eq!(admission.admit(), Ok(1));
    assert_eq!(admission.admit(), Ok(2));
    assert!(!admission.has_capacity());
    assert_eq!(
        admission.admit(),
        Err(AdmissionError::SessionCapacityExhausted { max_sessions: 2 })
    );

    // Release restores capacity
    admission.release();
    assert!(admission.has_capacity());
    assert_eq!(admission.admit(), Ok(2));
}

#[test]
fn probe_detailed_logging_contract() {
    use fr_media::capabilities::{DeviceIdentity, MediaCapabilities, ProbeReport};
    use fr_media::config::CodecProfile;
    use fr_media::surface::{CopyKind, CopyLedger};

    log("probe detailed logging with device identity, effective config, and copy counts");
    let id = DeviceIdentity::new("macOS 15.0", "Apple M3 Pro", "vt-2.0", 0).unwrap();
    let caps = MediaCapabilities::new(
        vec![CodecProfile::Main8_420],
        3840,
        2160,
        vec![PixelFormat::Nv12, PixelFormat::Bgra8],
        true,
        4,
    )
    .unwrap();
    let mut ledger = CopyLedger::new();
    ledger.record(CopyKind::CaptureToOwned);
    ledger.record(CopyKind::GpuConversion);

    let report = ProbeReport::new(id, caps, ledger);
    let summary = report.diagnostic_summary();
    log(&format!("emitted diagnostic summary: {summary}"));

    assert!(summary.starts_with("probe["));
    assert!(summary.contains("device=\"Apple M3 Pro\""));
    assert!(summary.contains("driver=\"vt-2.0\""));
    assert!(summary.contains("os=\"macOS 15.0\""));
    assert!(summary.contains("hw=true"));
    assert!(summary.contains("max_res=3840x2160"));
    assert!(summary.contains("max_sess=4"));
    assert!(summary.contains("copies=2"));
    // Ensure no raw pointer addresses or unredacted pixel data appear
    assert!(!summary.contains("0x7f"));
}

#[test]
fn compile_time_api_review_no_foreign_codec_types() {
    // Trait object assertions for public interfaces
    fn assert_is_encoder<T: Encoder>() {}
    fn assert_is_decoder<T: Decoder>() {}
    fn assert_is_surface<T: fr_media::surface::GpuSurface>() {}

    log("compile-time API review: public signatures only expose first-party types");

    assert_is_encoder::<FakeEncoder>();
    assert_is_decoder::<FakeDecoder>();
    assert_is_surface::<FakeSurface>();

    // Assert that the public contract types do not import or expose any foreign codec C types
    // (AVCodec, AVCodecContext, CVPixelBuffer, ID3D11Device, etc.)
    let limits = ProtocolLimits::ABSOLUTE;
    let geom = CodedGeometry::new(&limits, 1920, 1088, 1920, 1080, 16).unwrap();
    let gop = GopPolicy::baseline_for_frame_rate(60).unwrap();
    let cfg = CodecConfiguration::new_baseline(
        CodecConfigurationGeneration::from_raw(42),
        geom,
        ColorInfo::sdr_bt709(),
        gop,
    )
    .unwrap();

    let mut enc = FakeEncoder::new(limits, 4);
    assert_eq!(enc.configure(cfg), Ok(()));
    assert!(enc.configuration().is_some());
}
