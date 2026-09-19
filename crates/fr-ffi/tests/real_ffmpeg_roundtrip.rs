use fr_core::ids::CodecConfigurationGeneration;
use fr_core::limits::ProtocolLimits;
use fr_ffi::{FfiEncoderBackend, FfiSurface, FfmpegDecoder, FfmpegEncoder};
use fr_media::codec::{Decoder, EncodeRequest, Encoder, MediaError};
use fr_media::config::{CodecConfiguration, CodedGeometry, ColorInfo, GopPolicy};
use fr_media::surface::{PixelFormat, SurfaceBackend};

fn test_config() -> CodecConfiguration {
    let limits = ProtocolLimits::ABSOLUTE;
    let geom = CodedGeometry::from_visible(&limits, 64, 64, 16).unwrap();
    let gop = GopPolicy::baseline_for_frame_rate(30).unwrap();
    CodecConfiguration::new_baseline(
        CodecConfigurationGeneration::INITIAL,
        geom,
        ColorInfo::sdr_bt709(),
        gop,
    )
    .unwrap()
}

#[test]
fn real_hevc_software_roundtrip_if_available() {
    let config = test_config();

    let mut enc = FfmpegEncoder::new(
        FfiEncoderBackend::SoftwareExplicit,
        SurfaceBackend::Fake,
        30,
        500_000,
    );

    // If libx265 is not available on this platform/build, configure returns Fatal/Unavailable
    if let Err(e) = enc.configure(config) {
        assert_eq!(e, MediaError::Fatal);
        return;
    }

    let mut dec = FfmpegDecoder::new(SurfaceBackend::Fake);
    if let Err(e) = dec.configure(config) {
        assert_eq!(e, MediaError::Fatal);
        return;
    }

    // Submit 3 frames of solid black pixels
    let black_pixels = vec![0u8; 64 * 64 * 4];
    let surface = FfiSurface::new(
        SurfaceBackend::Fake,
        PixelFormat::Bgra8,
        64,
        64,
        black_pixels,
    );

    // Frame 1: IDR
    enc.submit(&surface, EncodeRequest { force_idr: true })
        .unwrap();
    let au1 = enc.poll_output().unwrap();
    assert!(au1.is_idr());

    dec.submit(&au1).unwrap();
    let pic1 = dec.poll_output().unwrap();
    assert_eq!(pic1.frame_raw(), 1);
    assert_eq!(pic1.surface().width(), 64);
    assert_eq!(pic1.surface().height(), 64);
}
