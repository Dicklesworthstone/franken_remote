use fr_core::ids::{CodecConfigurationGeneration, RecoveryGeneration};
use fr_core::limits::ProtocolLimits;
use fr_ffi::FfmpegDecoder;
use fr_media::access_unit::{EncodedAccessUnit, FrameId, FrameKind};
use fr_media::codec::{Decoder, MediaError};
use fr_media::config::{CodecConfiguration, CodedGeometry, ColorInfo, GopPolicy};
use fr_media::surface::SurfaceBackend;

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
#[allow(clippy::cast_possible_truncation)]
fn decoder_safely_handles_corrupted_and_fuzzed_bitstreams() {
    let mut dec = FfmpegDecoder::new(SurfaceBackend::Fake);
    dec.configure(test_config()).unwrap();

    let limits = ProtocolLimits::ABSOLUTE;

    // Deterministic pseudo-random bytes sequence for hostile fuzzing
    let mut seed: u64 = 0xDEAD_BEEF_CAFE_1234;
    let mut rand_byte = || {
        seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        (seed >> 33) as u8
    };

    // Test a variety of adversarial slice patterns
    for frame_idx in 1..=100 {
        let payload_len = (rand_byte() as usize % 512) + 1;
        let mut hostile_payload = Vec::with_capacity(payload_len);
        for _ in 0..payload_len {
            hostile_payload.push(rand_byte());
        }

        let frame_id = FrameId::from_raw(frame_idx);
        let kind = if frame_idx == 1 {
            FrameKind::Idr {
                recovery: RecoveryGeneration::INITIAL,
            }
        } else {
            FrameKind::Predicted {
                references: FrameId::from_raw(frame_idx - 1),
            }
        };

        if let Ok(au) = EncodedAccessUnit::new(
            &limits,
            frame_id,
            kind,
            CodecConfigurationGeneration::INITIAL,
            16666,
            hostile_payload,
        ) {
            // Must either accept (and handle inside FFmpeg's error resilient decode)
            // or return a typed MediaError without segfaulting or leaking.
            let res = dec.submit(&au);
            assert!(
                res.is_ok()
                    || res == Err(MediaError::Fatal)
                    || res == Err(MediaError::Backpressure)
                    || res == Err(MediaError::ConfigMismatch)
            );
        }
    }
}
