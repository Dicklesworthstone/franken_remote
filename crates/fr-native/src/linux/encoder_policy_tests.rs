//! Real libx265 cadence and long-range motion regressions for the software profile.
use super::*;
use fr_core::ids::CodecConfigurationGeneration;
use fr_media::config::{CodedGeometry, GopPolicy};

const LIMITS: ProtocolLimits = ProtocolLimits::ABSOLUTE;

fn configuration(width: u32, height: u32, policy_fps: u32) -> CodecConfiguration {
    CodecConfiguration::new_baseline(
        CodecConfigurationGeneration::INITIAL,
        CodedGeometry::new(&LIMITS, width, height, width, height, 2).unwrap(),
        ColorInfo::sdr_bt709(),
        GopPolicy::baseline_for_frame_rate(policy_fps).unwrap(),
    )
    .unwrap()
}
fn open(config: CodecConfiguration, bitrate: u32) -> Result<HevcEncoder, NativeError> {
    HevcEncoder::new(config, LIMITS, EncodeBackend::SoftwareExplicit, 30, bitrate)
}
fn changing(index: u64) -> BgraFrame {
    let shade = u8::try_from(index % 200).unwrap();
    BgraFrame::new(64, 64, [shade, 50, 80, 255].repeat(64 * 64), &LIMITS).unwrap()
}
fn submit(
    encoder: &mut HevcEncoder,
    frame: u64,
    pixels: &BgraFrame,
    force_idr: bool,
) -> EncodedAccessUnit {
    encoder
        .submit(pixels, FrameId::from_raw(frame), frame * 33_334, force_idr)
        .unwrap();
    encoder.poll_output().unwrap()
}

#[test]
fn active_encoding_has_no_natural_idr_early_and_forced_idrs_are_immediate() {
    let mut encoder = open(configuration(64, 64, 30), 2_000_000).unwrap();
    for frame in 0..300 {
        let unit = submit(&mut encoder, frame, &changing(frame), false);
        assert_eq!(unit.is_idr(), frame == 0, "picture {frame}");
    }
    assert!(submit(&mut encoder, 300, &changing(300), true).is_idr());
    assert!(!submit(&mut encoder, 301, &changing(301), false).is_idr());
}

#[test]
fn the_policy_bound_still_produces_a_natural_idr() {
    // A 1 fps policy gives a 60-picture bound while this encoder runs at 30 fps.
    let mut encoder = open(configuration(64, 64, 1), 2_000_000).unwrap();
    for frame in 0..=61 {
        let unit = submit(&mut encoder, frame, &changing(frame), false);
        assert_eq!(unit.is_idr(), frame == 0 || frame == 60, "picture {frame}");
    }
}

#[test]
fn a_bound_past_every_admitted_frame_rate_is_refused() {
    assert!(open(configuration(64, 64, 240), 2_000_000).is_ok());
    assert!(matches!(
        open(configuration(64, 64, 241), 2_000_000),
        Err(NativeError::InvalidConfiguration)
    ));
}

const WIDTH: usize = 1280;
const HEIGHT: usize = 720;
const SCROLL: usize = 64;
const PICTURES: usize = 30;
const SETTLED_PICTURES: f64 = 10.0;

/// A page of deterministic dark glyph cells on white, taller than the whole scroll.
fn page() -> Vec<u8> {
    let rows = HEIGHT + SCROLL * PICTURES;
    let mut pixels = [250, 250, 250, 255].repeat(WIDTH * rows);
    let mut state = 0x9E37_79B9_7F4A_7C15_u64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for line in 0..rows / 18 {
        for cell in 0..(WIDTH - 16) / 8 {
            let glyph = next();
            if glyph.is_multiple_of(5) {
                continue;
            }
            for (bit, (y, x)) in (0..11)
                .flat_map(|y| (0..6).map(move |x| (y, x)))
                .enumerate()
            {
                if (glyph >> (bit % 61 + 3)) & 1 == 1 {
                    let offset = ((line * 18 + y + 3) * WIDTH + cell * 8 + x + 8) * 4;
                    pixels[offset..offset + 3].copy_from_slice(&[28, 28, 28]);
                }
            }
        }
    }
    pixels
}
#[allow(clippy::cast_precision_loss)] // Sums and sizes stay far below 2^52.
fn green_psnr(decoded: &[u8], source: &[u8]) -> f64 {
    let squared: u64 = decoded
        .as_chunks::<4>()
        .0
        .iter()
        .zip(source.as_chunks::<4>().0)
        .map(|(a, b)| u64::from(a[1].abs_diff(b[1])).pow(2))
        .sum();
    let mean = squared as f64 / (WIDTH * HEIGHT) as f64;
    10.0 * (255.0_f64 * 255.0 / mean).log10()
}

/// Ultrafast's diamond search loses this motion and spends the whole budget on
/// intra blocks. Removing `me=umh` from the bridge fails this test.
#[test]
fn fast_scrolled_text_keeps_its_quality_at_the_target_rate() {
    let (width, height) = (
        u32::try_from(WIDTH).unwrap(),
        u32::try_from(HEIGHT).unwrap(),
    );
    let config = configuration(width, height, 30);
    let mut encoder = open(config, 8_000_000).unwrap();
    let page = page();
    let picture = |index: usize| {
        let start = index * SCROLL * WIDTH * 4;
        let bytes = page[start..start + WIDTH * HEIGHT * 4].to_vec();
        BgraFrame::new(width, height, bytes, &LIMITS).unwrap()
    };
    let first = submit(&mut encoder, 0, &picture(0), false);
    let record = encoder.admission.decoder_record().unwrap().bytes().to_vec();
    let mut decoder = HevcDecoder::new(config, LIMITS, &record).unwrap();
    decoder.submit(&first).unwrap();
    decoder.poll_output().unwrap();
    let (mut bytes, mut quality) = (0, Vec::new());
    for index in 1..PICTURES {
        let source = picture(index);
        let unit = submit(&mut encoder, index as u64, &source, false);
        assert!(!unit.is_idr());
        bytes += unit.bytes().len();
        decoder.submit(&unit).unwrap();
        let (_, output) = decoder.poll_output().unwrap();
        quality.push(green_psnr(output.pixels(), source.pixels()));
    }
    let psnr = quality[quality.len() - 10..].iter().sum::<f64>() / SETTLED_PICTURES;
    eprintln!("fast scroll: {bytes} predicted bytes, last-10 mean {psnr:.2} dB");
    assert!(psnr >= MOTION_QUALITY_FLOOR_DB, "{psnr:.2} dB");
}
/// Measured on x86-64 with libx265: 17.4 dB with `me=umh`, 11.8 dB with ultrafast's
/// diamond search. The glyphs are random bit noise, so only the separation matters.
const MOTION_QUALITY_FLOOR_DB: f64 = 15.0;
