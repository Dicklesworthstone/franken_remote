//! Real software stop/reopen and original decoder/configuration regressions.
use super::*;
use fr_core::ids::CodecConfigurationGeneration;
use fr_media::config::{CodedGeometry, GopPolicy};

fn configuration(width: u32, height: u32) -> CodecConfiguration {
    CodecConfiguration::new_baseline(
        CodecConfigurationGeneration::INITIAL,
        CodedGeometry::new(&ProtocolLimits::ABSOLUTE, width, height, width, height, 2).unwrap(),
        ColorInfo::sdr_bt709(),
        GopPolicy::baseline_for_frame_rate(30).unwrap(),
    )
    .unwrap()
}
fn encoder(config: CodecConfiguration, bitrate: u32) -> HevcEncoder {
    HevcEncoder::new(
        config,
        ProtocolLimits::ABSOLUTE,
        EncodeBackend::SoftwareExplicit,
        30,
        bitrate,
    )
    .unwrap()
}
fn flat() -> BgraFrame {
    BgraFrame::new(
        64,
        64,
        [30, 50, 80, 255].repeat(64 * 64),
        &ProtocolLimits::ABSOLUTE,
    )
    .unwrap()
}
fn submit(encoder: &mut HevcEncoder, frame: u64, pixels: &BgraFrame) -> EncodedAccessUnit {
    encoder
        .submit(pixels, FrameId::from_raw(frame), frame * 33_334, false)
        .unwrap();
    encoder.poll_output().unwrap()
}
#[test]
fn software_rates_reuse_exact_original_decoder_configuration_and_frame_identity() {
    let config = configuration(64, 64);
    let mut encoder = encoder(config, 8_000_000);
    let pixels = flat();
    let first = submit(&mut encoder, 0, &pixels);
    let original = encoder.admission.decoder_record().unwrap().bytes().to_vec();
    let level = encoder.level_idc;
    let mut decoder = HevcDecoder::new(config, ProtocolLimits::ABSOLUTE, &original).unwrap();
    decoder.submit(&first).unwrap();
    assert_eq!(decoder.poll_output().unwrap().0, first.frame());
    for (frame, target) in [(1, 4_000_000), (2, 2_000_000), (3, 8_000_000)] {
        let previous = encoder.bitrate();
        assert_eq!(encoder.reopen_bitrate(target), Ok(true));
        assert_eq!(encoder.bitrate(), previous, "open is not validated output");
        let unit = submit(&mut encoder, frame, &pixels);
        assert!(unit.is_idr());
        assert_eq!(unit.frame().as_raw(), frame);
        assert_eq!(unit.capture_micros(), frame * 33_334);
        assert_eq!(unit.config_generation(), config.generation());
        assert_eq!(encoder.bitrate(), target);
        assert_eq!(encoder.initial_bitrate(), 8_000_000);
        assert_eq!(encoder.level_idc, level);
        assert_eq!(
            encoder.admission.decoder_record().unwrap().bytes(),
            original
        );
        decoder.submit(&unit).unwrap();
        assert_eq!(decoder.poll_output().unwrap().0, unit.frame());
    }
    let predicted = submit(&mut encoder, 4, &pixels);
    assert!(
        matches!(predicted.kind(), FrameKind::Predicted { references } if references.as_raw()==3)
    );
    decoder.submit(&predicted).unwrap();
    assert_eq!(decoder.poll_output().unwrap().0, predicted.frame());
}
#[test]
fn invalid_rates_and_pending_output_cannot_replace_the_live_encoder() {
    let mut encoder = encoder(configuration(64, 64), 8_000_000);
    for invalid in [0, 9_999, 499_999, 8_000_001, u32::MAX] {
        assert_eq!(
            encoder.reopen_bitrate(invalid),
            Err(NativeError::InvalidConfiguration)
        );
        assert!(encoder.raw.is_some());
        assert!(!encoder.closed);
    }
    assert_eq!(
        encoder.reopen_bitrate(4_000_000),
        Err(NativeError::NeedInput)
    );
    let pixels = flat();
    encoder.submit(&pixels, FrameId::FIRST, 0, false).unwrap();
    assert_eq!(
        encoder.reopen_bitrate(4_000_000),
        Err(NativeError::NeedDrain)
    );
    assert_eq!(encoder.bitrate(), 8_000_000);
    let unit = encoder.poll_output().unwrap();
    assert_eq!(unit.frame(), FrameId::FIRST);
    assert_eq!(encoder.reopen_bitrate(4_000_000), Ok(true));
    assert_eq!(
        encoder.reopen_bitrate(2_000_000),
        Err(NativeError::NeedDrain)
    );
    assert!(submit(&mut encoder, 1, &pixels).is_idr());
}
#[test]
fn a_changed_parameter_set_never_releases_the_candidate_or_commits_its_rate() {
    let mut encoder = encoder(configuration(64, 64), 1_000_000);
    let pixels = flat();
    let first = submit(&mut encoder, 0, &pixels);
    // Plant the unsupported-header condition AFTER original real admission.
    // Production obtains the level only from that admission's PictureInfo.
    encoder.level_idc = Some(120);
    assert_eq!(encoder.reopen_bitrate(500_000), Ok(true));
    encoder
        .submit(&pixels, FrameId::from_raw(1), 33_334, false)
        .unwrap();
    assert_eq!(
        encoder.poll_output().unwrap_err(),
        NativeError::UnsupportedBitstream
    );
    assert_eq!(encoder.bitrate(), 1_000_000);
    assert_eq!(encoder.last_output, Some(first.frame()));
    assert_eq!(
        encoder.submit(&pixels, FrameId::from_raw(2), 66_668, false),
        Err(NativeError::Closed)
    );
}
#[test]
fn a_failed_stop_reopen_is_terminal_and_owns_no_stale_native_pointer() {
    let mut encoder = encoder(configuration(64, 64), 1_000_000);
    let pixels = flat();
    submit(&mut encoder, 0, &pixels);
    // Native boundary refuses this impossible level after the old context stops.
    encoder.level_idc = Some(255);
    assert_eq!(
        encoder.reopen_bitrate(500_000),
        Err(NativeError::InvalidConfiguration)
    );
    assert!(encoder.raw.is_none());
    assert_eq!(encoder.bitrate(), 1_000_000);
    assert_eq!(
        encoder.submit(&pixels, FrameId::from_raw(1), 33_334, false),
        Err(NativeError::Closed)
    );
}

fn busy_desktop(frame: u64) -> BgraFrame {
    let mut bytes = Vec::with_capacity(640 * 480 * 4);
    let mut random = frame.wrapping_add(1);
    for y in 0..480_usize {
        for x in 0..640_usize {
            random = random
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            let rgb = if x >= 400 && (120..420).contains(&y) {
                let parts = random.to_be_bytes();
                [parts[0], parts[1], parts[2]]
            } else {
                let shift = usize::try_from(frame % 10).unwrap();
                let ink = (y / 2 + shift) % 10 < 7
                    && x / 2 % 7 < 5
                    && ((x / 14 + y / 20) & (1 << (x / 2 % 5))) != 0;
                [if ink { 230 } else { 20 }; 3]
            };
            bytes.extend_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
        }
    }
    BgraFrame::new(640, 480, bytes, &ProtocolLimits::ABSOLUTE).unwrap()
}
fn offered_bytes(lower: bool) -> (usize, usize) {
    let config = configuration(640, 480);
    let mut encoder = encoder(config, 8_000_000);
    let mut decoder = None;
    let mut bytes = 0;
    let mut idr_bytes = 0;
    for frame in 0..90_u64 {
        if frame == 30 && lower {
            assert_eq!(encoder.reopen_bitrate(2_000_000), Ok(true));
        }
        let source = busy_desktop(frame);
        encoder
            .submit(
                &source,
                FrameId::from_raw(frame),
                frame * 33_334,
                frame == 30,
            )
            .unwrap();
        let unit = encoder.poll_output().unwrap();
        assert_eq!(unit.is_idr(), frame == 0 || frame == 30);
        assert_eq!(unit.frame().as_raw(), frame);
        assert_eq!(unit.capture_micros(), frame * 33_334);
        assert_eq!(unit.config_generation(), config.generation());
        if decoder.is_none() {
            decoder = Some(
                HevcDecoder::new(
                    config,
                    ProtocolLimits::ABSOLUTE,
                    encoder.admission.decoder_record().unwrap().bytes(),
                )
                .unwrap(),
            );
        }
        let decoder = decoder.as_mut().unwrap();
        decoder.submit(&unit).unwrap();
        assert_eq!(decoder.poll_output().unwrap().0, unit.frame());
        if frame >= 30 {
            bytes += unit.bytes().len();
        }
        if frame == 30 {
            idr_bytes = unit.bytes().len();
        }
    }
    (idr_bytes, bytes)
}
#[test]
fn real_software_rate_reduction_cuts_offered_bytes_including_its_idr_cost() {
    let baseline = offered_bytes(false);
    let lowered = offered_bytes(true);
    eprintln!(
        "640x480 mixed desktop8M->2M: IDR {} -> {} bytes;60 nominal30fps pictures {} -> {} bytes",
        baseline.0, lowered.0, baseline.1, lowered.1
    );
    assert!(
        lowered.0 < baseline.0 * 3 / 4,
        "reopen failed to reduce the real IDR cost"
    );
    assert!(
        lowered.1 < baseline.1 * 3 / 5,
        "IDR overhead erased the sustained offered-load reduction"
    );
}
