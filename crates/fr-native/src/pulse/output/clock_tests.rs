//! Production native metadata gate; no server, hardware or audible-output claim.
use super::*;
use fr_core::audio::{AudioChannels, AudioGeneration, AudioStreamConfig};

fn frame(count: usize) -> (AudioStreamConfig, AudioPcmFrame, AudioSubmission) {
    let config = AudioStreamConfig::new(
        AudioDirection::Downlink,
        AudioGeneration::INITIAL,
        AudioChannels::Stereo,
        10,
        20,
    )
    .unwrap();
    let pcm = AudioPcmFrame::from_interleaved(
        config.generation(),
        config.channels(),
        900_000,
        &vec![1; count * 2],
    )
    .unwrap();
    let audio = AudioSubmission {
        direction: config.direction(),
        generation: config.generation(),
        sequence: 5,
        source_samples: 900_000,
        output_samples: 10_000,
        output_valid_before: 10_000 + u64::try_from(count).unwrap(),
        valid_until: ClientInstant(100_000),
        concealed: false,
    };
    (config, pcm, audio)
}
#[test]
fn device_accepts_only_bounded_adjustments_and_the_exact_contiguous_span() {
    for count in [479, 480, 481] {
        let (config, pcm, audio) = frame(count);
        assert_eq!(
            validate_pcm(config, &pcm, audio, Some((5, 10_000))),
            Ok(u64::try_from(count).unwrap())
        );
        for (seq, at) in [(4, 10_000), (6, 10_000), (5, 9999), (5, 10_001)] {
            assert_eq!(
                validate_pcm(config, &pcm, audio, Some((seq, at))),
                Err(Error::Metadata)
            );
        }
        let mut wrong = audio;
        wrong.output_valid_before += 1;
        assert_eq!(
            validate_pcm(config, &pcm, wrong, None),
            Err(Error::Metadata)
        );
    }
    for count in [478, 482, 960] {
        let (config, pcm, audio) = frame(count);
        assert_eq!(
            validate_pcm(config, &pcm, audio, None),
            Err(Error::Metadata)
        );
    }
}
#[test]
fn rate_correction_cannot_change_permission_epoch_source_or_channel_layout() {
    let (config, pcm, audio) = frame(481);
    let mut wrong = audio;
    wrong.direction = AudioDirection::Uplink;
    assert_eq!(
        validate_pcm(config, &pcm, wrong, None),
        Err(Error::Metadata)
    );
    wrong = audio;
    wrong.generation = audio.generation.next().unwrap();
    assert_eq!(
        validate_pcm(config, &pcm, wrong, None),
        Err(Error::Metadata)
    );
    wrong = audio;
    wrong.source_samples += 1;
    assert_eq!(
        validate_pcm(config, &pcm, wrong, None),
        Err(Error::Metadata)
    );
    let mono = AudioPcmFrame::from_interleaved(
        config.generation(),
        AudioChannels::Mono,
        audio.source_samples,
        &[1; 481],
    )
    .unwrap();
    assert_eq!(
        validate_pcm(config, &mono, audio, None),
        Err(Error::Metadata)
    );
    wrong = audio;
    wrong.output_samples = u64::MAX;
    assert_eq!(
        validate_pcm(config, &pcm, wrong, None),
        Err(Error::Metadata)
    );
}
#[test]
fn original_age_device_slot_and_native_extent_bounds_still_dominate_corrected_pcm() {
    for count in [479, 481] {
        let (_, _, audio) = frame(count);
        let end = audio.output_valid_before + 480;
        let good = PlayoutClock {
            now: ClientInstant(50_000),
            output_samples: audio.output_samples,
        };
        assert_eq!(validate_submission_clock(good, audio, end), Ok(()));
        assert_eq!(
            validate_submission_clock(
                PlayoutClock {
                    now: audio.valid_until,
                    ..good
                },
                audio,
                end
            ),
            Err(Error::Expired)
        );
        assert_eq!(
            validate_submission_clock(
                PlayoutClock {
                    output_samples: audio.output_valid_before,
                    ..good
                },
                audio,
                end
            ),
            Err(Error::Clock)
        );
        let bytes = u32::try_from((end - audio.output_samples) * 4).unwrap();
        assert_eq!(
            validate_native_extent(40_000, 0, 10_000, end, 2, bytes),
            Ok(())
        );
        assert_eq!(
            validate_native_extent(40_000, 0, 10_000, end, 2, bytes - 1),
            Err(Error::Backpressure)
        );
    }
}

#[test]
fn maximum_device_lead_reserves_rounding_inside_the_existing_queue_ceiling() {
    for nominal in [480, 960] {
        let lead = output_lead(15_000, nominal).unwrap();
        let largest = u64::from(nominal + nominal.div_ceil(1000));
        assert!(
            lead >= 15 * 48,
            "actual configured device latency is covered"
        );
        assert!(lead + largest <= u64::from(MAX_DEVICE_BUFFER_MS) * 48);
        let audio = AudioSubmission {
            output_samples: 100,
            output_valid_before: 100 + largest,
            ..frame(480).2
        };
        let clock = PlayoutClock {
            now: ClientInstant(0),
            output_samples: 100,
        };
        assert_eq!(
            validate_submission_clock(clock, audio, audio.output_valid_before + lead),
            Ok(())
        );
    }
    assert_eq!(output_lead(15_001, 960), Err(Error::BufferLimit));
    assert_eq!(output_lead(u64::MAX, 960), Err(Error::BufferLimit));
    assert_eq!(output_lead(0, u32::MAX), Err(Error::BufferLimit));
}
