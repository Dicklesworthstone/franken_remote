//! In-place converter checked against an independent, out-of-place integer oracle.
//! Signals are test data, not measured audio-device quality or codec qualification.
use fr_core::audio::{AudioChannels, AudioGeneration, MAX_DECODED_SAMPLES};
use fr_media::audio::{AudioMediaError, AudioPcmFrame, clock::valid_sample_count};

fn oracle(input: &[i16], channels: usize, frames: usize) -> Vec<i16> {
    let original = input.len() / channels;
    (0..frames * channels)
        .map(|i| {
            let p = (i / channels) * (original - 1);
            let den = frames - 1;
            let a = p / den;
            let b = (a + 1).min(original - 1);
            let fraction = p % den;
            let value = i64::from(input[a * channels + i % channels])
                * i64::try_from(den - fraction).unwrap()
                + i64::from(input[b * channels + i % channels]) * i64::try_from(fraction).unwrap();
            i16::try_from(value / i64::try_from(den).unwrap()).unwrap()
        })
        .collect()
}

#[test]
fn each_negotiated_duration_channel_and_bounded_adjustment_matches_the_oracle() {
    for layout in [AudioChannels::Mono, AudioChannels::Stereo] {
        let channels = usize::from(layout.count());
        for count in [240_usize, 480, 960, 1920, 2880, 3840, 4800, 5760] {
            let input: Vec<_> = (0..count * channels)
                .map(|i| {
                    // Alternating extremes and independent channels catch an
                    // overwritten source sample or incorrect traversal direction.
                    match i % 4 {
                        0 => i16::MIN,
                        1 => i16::MAX,
                        2 => i16::try_from(i).unwrap(),
                        _ => -i16::try_from(i).unwrap(),
                    }
                })
                .collect();
            for out in count - count.div_ceil(1000)..=count + count.div_ceil(1000) {
                let mut frame = AudioPcmFrame::from_interleaved(
                    AudioGeneration::INITIAL,
                    layout,
                    u64::MAX - 10_000,
                    &input,
                )
                .unwrap();
                let original = frame;
                if out > MAX_DECODED_SAMPLES as usize {
                    assert!(frame.resample_for_clock(out).is_err());
                    assert_eq!(frame.samples(), original.samples());
                    continue;
                }
                frame.resample_for_clock(out).unwrap();
                assert_eq!(frame.samples(), oracle(&input, channels, out));
                assert_eq!(frame.samples_per_channel(), out);
                assert_eq!(frame.channels(), layout);
                assert_eq!(frame.generation(), original.generation());
                assert_eq!(frame.timestamp_samples(), original.timestamp_samples());
                assert_eq!(&frame.samples()[..channels], &input[..channels]);
                assert_eq!(
                    &frame.samples()[(out - 1) * channels..],
                    &input[(count - 1) * channels..]
                );
            }
        }
    }
}

#[test]
fn identity_is_bit_exact_and_dc_levels_do_not_change_under_clock_correction() {
    for level in [i16::MIN, -1, 0, 1, i16::MAX] {
        for out in [479, 480, 481] {
            let mut frame = AudioPcmFrame::from_interleaved(
                AudioGeneration::INITIAL,
                AudioChannels::Stereo,
                123,
                &[level; 960],
            )
            .unwrap();
            frame.resample_for_clock(out).unwrap();
            assert!(frame.samples().iter().all(|&x| x == level));
        }
    }
}

#[test]
fn malformed_or_excessive_correction_is_atomic_and_never_a_general_resampler() {
    let mut frame = AudioPcmFrame::from_interleaved(
        AudioGeneration::INITIAL,
        AudioChannels::Mono,
        4321,
        &[42; 480],
    )
    .unwrap();
    for count in [0, 1, 2, 478, 482, 5760, usize::MAX] {
        assert!(!valid_sample_count(480, count));
        assert_eq!(
            frame.resample_for_clock(count),
            Err(AudioMediaError::InvalidPayload)
        );
        assert_eq!(frame.samples(), &[42; 480]);
        assert_eq!(frame.timestamp_samples(), 4321);
    }
    for count in [0, 1, usize::MAX] {
        assert!(!valid_sample_count(count, 480));
    }
}
