#![cfg(all(target_os = "linux", feature = "linux-opus"))]
mod opus_support;
use fr_core::audio::{
    AudioChannels, AudioDirection, AudioGeneration, AudioStreamConfig, MAX_OPUS_PAYLOAD_BYTES,
};
use fr_media::audio::{AudioEncoder, AudioMediaError, AudioPcmFrame};
use fr_native::opus::{Encoder, MAX_CODEC_STATE_BYTES};
use opus_support::{Oracle, config, tone};

#[test]
fn genuine_packets_decode_for_both_directions_channels_and_every_admitted_duration() {
    for channels in [AudioChannels::Mono, AudioChannels::Stereo] {
        for direction in [AudioDirection::Downlink, AudioDirection::Uplink] {
            for millis in [5, 10, 20, 40, 60] {
                let cfg = config(channels, direction, millis);
                let mut encoder = Encoder::new();
                encoder.configure(cfg).unwrap();
                assert!(encoder.native_state_bytes() > 0);
                assert!(encoder.native_state_bytes() <= MAX_CODEC_STATE_BYTES);
                let bytes = encoder.native_state_bytes();
                let mut decoder = Oracle::new(channels);
                for sequence in 0..8_u64 {
                    let timestamp = sequence * u64::from(cfg.expected_samples_per_frame());
                    encoder.submit_pcm(&tone(cfg, timestamp)).unwrap();
                    let packet = encoder.poll_packet().unwrap().unwrap();
                    assert_eq!(packet.sequence(), sequence);
                    assert_eq!(packet.timestamp_samples(), timestamp);
                    assert_eq!(packet.direction(), direction);
                    assert_eq!(packet.generation(), cfg.generation());
                    assert_eq!(
                        u32::from(packet.duration_samples()),
                        cfg.expected_samples_per_frame()
                    );
                    assert_ne!(packet.payload().len(), 0);
                    assert!(packet.payload().len() <= MAX_OPUS_PAYLOAD_BYTES);
                    let pcm =
                        decoder.decode(packet.payload(), usize::from(packet.duration_samples()));
                    assert!(pcm.iter().any(|&sample| sample != 0));
                    assert_eq!(encoder.native_state_bytes(), bytes);
                    assert_eq!(encoder.poll_packet().unwrap(), None);
                }
            }
        }
    }
}

#[test]
fn decoded_stereo_waveforms_track_the_real_input_after_reported_codec_delay() {
    let cfg = config(AudioChannels::Stereo, AudioDirection::Downlink, 10);
    let mut encoder = Encoder::with_settings(128_000, 5).unwrap();
    encoder.configure(cfg).unwrap();
    let delay = usize::from(encoder.lookahead_samples().unwrap()) * 2;
    let mut decoder = Oracle::new(cfg.channels());
    let mut input = Vec::new();
    let mut output = Vec::new();
    for index in 0..40 {
        let frame = tone(cfg, index * 480);
        input.extend_from_slice(frame.samples());
        encoder.submit_pcm(&frame).unwrap();
        output.extend(decoder.decode(encoder.poll_packet().unwrap().unwrap().payload(), 480));
    }
    for channel in 0..2 {
        let mut dot = 0.0;
        let mut energy_in = 0.0;
        let mut energy_out = 0.0;
        for n in (1920 + channel..input.len() - delay).step_by(2) {
            let x = f64::from(input[n]);
            let y = f64::from(output[n + delay]);
            dot += x * y;
            energy_in += x * x;
            energy_out += y * y;
        }
        assert!(dot / (energy_in * energy_out).sqrt() > 0.95);
    }
}

#[test]
fn backpressure_and_bad_pcm_do_not_advance_native_history_or_sequences() {
    let cfg = config(AudioChannels::Mono, AudioDirection::Downlink, 10);
    let mut subject = Encoder::new();
    let mut clean = Encoder::new();
    subject.configure(cfg).unwrap();
    clean.configure(cfg).unwrap();
    let frame = tone(cfg, 0);
    subject.submit_pcm(&frame).unwrap();
    clean.submit_pcm(&frame).unwrap();
    assert_eq!(
        subject.submit_pcm(&tone(cfg, 480)),
        Err(AudioMediaError::Backpressure)
    );
    assert_eq!(subject.configure(cfg), Err(AudioMediaError::Backpressure));
    assert_eq!(subject.poll_packet().unwrap(), clean.poll_packet().unwrap());
    let wrong = config(AudioChannels::Stereo, cfg.direction(), 10);
    assert_eq!(
        subject.submit_pcm(&tone(wrong, 480)),
        Err(AudioMediaError::InvalidPayload)
    );
    let short = config(cfg.channels(), cfg.direction(), 5);
    assert_eq!(
        subject.submit_pcm(&tone(short, 480)),
        Err(AudioMediaError::InvalidPayload)
    );
    assert_eq!(
        subject.submit_pcm(&frame),
        Err(AudioMediaError::InvalidPayload)
    );
    let other = AudioStreamConfig::new(
        cfg.direction(),
        AudioGeneration::from_raw(7),
        cfg.channels(),
        10,
        20,
    )
    .unwrap();
    assert!(matches!(
        subject.submit_pcm(&tone(other, 480)),
        Err(AudioMediaError::GenerationMismatch { .. })
    ));
    subject.submit_pcm(&tone(cfg, 480)).unwrap();
    clean.submit_pcm(&tone(cfg, 480)).unwrap();
    assert_eq!(subject.poll_packet().unwrap(), clean.poll_packet().unwrap());
}

#[test]
fn timeline_overflow_refuses_before_codec_and_explicit_source_gaps_are_preserved() {
    let cfg = config(AudioChannels::Mono, AudioDirection::Uplink, 10);
    let mut encoder = Encoder::new();
    encoder.configure(cfg).unwrap();
    assert_eq!(
        encoder.submit_pcm(&tone(cfg, u64::MAX - 479)),
        Err(AudioMediaError::BufferOverflow)
    );
    for (sequence, timestamp) in [1000_u64, 50_000, u64::MAX - 480].into_iter().enumerate() {
        encoder.submit_pcm(&tone(cfg, timestamp)).unwrap();
        let packet = encoder.poll_packet().unwrap().unwrap();
        assert_eq!(packet.sequence(), u64::try_from(sequence).unwrap());
        assert_eq!(packet.timestamp_samples(), timestamp);
    }
    assert_eq!(
        encoder.submit_pcm(&tone(cfg, u64::MAX)),
        Err(AudioMediaError::BufferOverflow)
    );
}

#[test]
fn invalid_reconfiguration_preserves_live_stream_but_closed_generations_never_reopen() {
    let cfg = config(AudioChannels::Mono, AudioDirection::Downlink, 10);
    let mut encoder = Encoder::new();
    encoder.configure(cfg).unwrap();
    encoder.submit_pcm(&tone(cfg, 0)).unwrap();
    encoder.poll_packet().unwrap();
    let invalid = AudioStreamConfig::new(
        cfg.direction(),
        AudioGeneration::from_raw(1),
        cfg.channels(),
        7,
        20,
    )
    .unwrap();
    assert_eq!(
        encoder.configure(invalid),
        Err(AudioMediaError::UnsupportedFormat)
    );
    assert_eq!(encoder.configure(cfg), Err(AudioMediaError::InvalidPayload));
    assert_eq!(encoder.configuration(), Some(cfg));
    encoder.submit_pcm(&tone(cfg, 480)).unwrap();
    assert_eq!(encoder.poll_packet().unwrap().unwrap().sequence(), 1);
    encoder.close();
    assert_eq!(encoder.native_state_bytes(), 0);
    assert_eq!(encoder.poll_packet(), Err(AudioMediaError::NotConfigured));
    assert_eq!(encoder.configure(cfg), Err(AudioMediaError::InvalidPayload));
    let new = AudioStreamConfig::new(
        cfg.direction(),
        AudioGeneration::from_raw(2),
        AudioChannels::Stereo,
        20,
        20,
    )
    .unwrap();
    encoder.configure(new).unwrap();
    encoder.submit_pcm(&tone(new, 0)).unwrap();
    let packet = encoder.poll_packet().unwrap().unwrap();
    assert_eq!(packet.sequence(), 0);
    assert_eq!(packet.generation(), new.generation());
    Oracle::new(new.channels()).decode(packet.payload(), 960);
}

#[test]
fn silence_is_real_opus_and_quality_options_are_locally_bounded() {
    assert!(matches!(
        Encoder::with_settings(5999, 5),
        Err(AudioMediaError::UnsupportedFormat)
    ));
    assert!(matches!(
        Encoder::with_settings(192_001, 5),
        Err(AudioMediaError::UnsupportedFormat)
    ));
    assert!(matches!(
        Encoder::with_settings(64_000, 11),
        Err(AudioMediaError::UnsupportedFormat)
    ));
    let cfg = config(AudioChannels::Stereo, AudioDirection::Downlink, 10);
    let mut encoder = Encoder::new();
    assert_eq!(
        encoder.submit_pcm(&tone(cfg, 0)),
        Err(AudioMediaError::NotConfigured)
    );
    encoder.configure(cfg).unwrap();
    let silence =
        AudioPcmFrame::from_interleaved(cfg.generation(), cfg.channels(), 0, &[0; 960]).unwrap();
    encoder.submit_pcm(&silence).unwrap();
    let packet = encoder.poll_packet().unwrap().unwrap();
    assert!(packet.is_silence());
    let decoded = Oracle::new(cfg.channels()).decode(packet.payload(), 480);
    assert!(decoded.iter().all(|sample| sample.abs() <= 2));
    assert!(!format!("{encoder:?}").contains("payload"));
}

#[test]
fn repeated_configuration_release_and_quality_extremes_remain_bounded() {
    for bitrate in [6_000, 192_000] {
        for complexity in [0, 10] {
            let mut encoder = Encoder::with_settings(bitrate, complexity).unwrap();
            for generation in 0..32 {
                let cfg = AudioStreamConfig::new(
                    AudioDirection::Downlink,
                    AudioGeneration::from_raw(generation),
                    AudioChannels::Stereo,
                    10,
                    20,
                )
                .unwrap();
                encoder.configure(cfg).unwrap();
                encoder.submit_pcm(&tone(cfg, 0)).unwrap();
                let packet = encoder.poll_packet().unwrap().unwrap();
                assert!(packet.payload().len() <= MAX_OPUS_PAYLOAD_BYTES);
                Oracle::new(cfg.channels()).decode(packet.payload(), 480);
                encoder.close();
                assert_eq!(encoder.native_state_bytes(), 0);
            }
        }
    }
}
