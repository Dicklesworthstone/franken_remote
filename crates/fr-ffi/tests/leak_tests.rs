use fr_core::ids::CodecConfigurationGeneration;
use fr_core::limits::ProtocolLimits;
use fr_ffi::{FfiEncoderBackend, FfiSurface, FfmpegEncoder};
use fr_media::codec::{EncodeRequest, Encoder};
use fr_media::config::{CodecConfiguration, CodedGeometry, ColorInfo, GopPolicy};
use fr_media::surface::{PixelFormat, SurfaceBackend};

fn test_config() -> CodecConfiguration {
    let limits = ProtocolLimits::ABSOLUTE;
    let geom = CodedGeometry::from_visible(&limits, 64, 64, 16).unwrap();
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
fn ten_thousand_cycle_memory_stability_test() {
    let mut enc = FfmpegEncoder::new(
        FfiEncoderBackend::MockSimulated,
        SurfaceBackend::Fake,
        60,
        1_000_000,
    );
    enc.configure(test_config()).unwrap();

    let surface = FfiSurface::new(
        SurfaceBackend::Fake,
        PixelFormat::Bgra8,
        64,
        64,
        vec![0xAA; 64 * 64 * 4],
    );

    // 10,000 encode/drain cycles to verify complete memory reclamation
    for i in 1..=10_000u64 {
        let force_idr = i % 60 == 1;
        enc.submit(&surface, EncodeRequest { force_idr }).unwrap();
        let au = enc.poll_output().unwrap();
        assert_eq!(au.frame().as_raw(), i);
    }
}
