#![cfg(all(target_os = "linux", feature = "linux-opus"))]
mod opus_support;
use fr_core::audio::{
    AudioChannels, AudioDirection, AudioGeneration, AudioStreamConfig, MAX_OPUS_PAYLOAD_BYTES,
};
use fr_media::audio::{
    AudioAccessUnit, AudioDecoder, AudioEncoder, AudioMediaError, AudioPcmFrame,
};
use fr_native::opus::{Decoder, Encoder, MAX_CODEC_STATE_BYTES, MAX_CONCEALED_SAMPLES};
use fr_wire::audio::{AUDIO_PACKET_OVERHEAD, AudioPacket, decode_packet, encode_packet};
use opus_support::{Oracle, aggregate, config, tone};

fn packets(cfg: AudioStreamConfig, count: u64) -> Vec<AudioAccessUnit> {
    let mut encoder = Encoder::new();
    encoder.configure(cfg).unwrap();
    (0..count)
        .map(|n| {
            encoder
                .submit_pcm(&tone(
                    cfg,
                    10_000 + n * u64::from(cfg.expected_samples_per_frame()),
                ))
                .unwrap();
            encoder.poll_packet().unwrap().unwrap()
        })
        .collect()
}
fn wire(packet: &AudioAccessUnit) -> AudioAccessUnit {
    let view = AudioPacket {
        direction: packet.direction(),
        generation: packet.generation(),
        sequence: packet.sequence(),
        timestamp_samples: packet.timestamp_samples(),
        duration_samples: packet.duration_samples(),
        payload: packet.payload(),
    };
    let mut buffer = [0_u8; AUDIO_PACKET_OVERHEAD + MAX_OPUS_PAYLOAD_BYTES];
    let len = encode_packet(&view, 17, &mut buffer).unwrap();
    assert!(decode_packet(&buffer[..len], 18).is_err());
    let view = decode_packet(&buffer[..len], 17).unwrap();
    AudioAccessUnit::new(
        view.direction,
        view.generation,
        view.sequence,
        view.timestamp_samples,
        view.duration_samples,
        false,
        view.payload,
    )
    .unwrap()
}
fn take(decoder: &mut Decoder) -> AudioPcmFrame {
    decoder.poll_pcm().unwrap().unwrap()
}

#[test]
fn real_encoder_wire_decoder_path_matches_native_oracle_for_every_profile() {
    for channels in [AudioChannels::Mono, AudioChannels::Stereo] {
        for direction in [AudioDirection::Downlink, AudioDirection::Uplink] {
            for duration in [5, 10, 20, 40, 60] {
                let cfg = config(channels, direction, duration);
                let mut decoder = Decoder::new();
                decoder.configure(cfg).unwrap();
                assert!(
                    decoder.native_state_bytes() > 0
                        && decoder.native_state_bytes() <= MAX_CODEC_STATE_BYTES
                );
                assert_eq!(
                    decoder.pcm_capacity_bytes(),
                    usize::try_from(cfg.expected_samples_per_frame()).unwrap()
                        * usize::from(channels.count())
                        * 2
                );
                let mut oracle = Oracle::new(channels);
                for packet in packets(cfg, 8) {
                    let packet = wire(&packet);
                    decoder.submit_packet(&packet).unwrap();
                    let pcm = take(&mut decoder);
                    assert_eq!(pcm.timestamp_samples(), packet.timestamp_samples());
                    assert_eq!(pcm.generation(), packet.generation());
                    assert_eq!(pcm.channels(), channels);
                    assert_eq!(pcm.sample_rate(), 48_000);
                    assert_eq!(
                        pcm.samples(),
                        oracle.decode(packet.payload(), usize::from(packet.duration_samples()))
                    );
                    assert!(decoder.poll_pcm().unwrap().is_none());
                }
            }
        }
    }
}

#[test]
fn real_lost_packets_use_native_plc_on_the_original_sample_timeline_then_resume() {
    let cfg = config(AudioChannels::Stereo, AudioDirection::Downlink, 10);
    let packets = packets(cfg, 5);
    let mut decoder = Decoder::new();
    decoder.configure(cfg).unwrap();
    assert!(matches!(
        decoder.decode_plc(480),
        Err(AudioMediaError::NeedMoreInput)
    ));
    let mut oracle = Oracle::new(cfg.channels());
    decoder.submit_packet(&wire(&packets[0])).unwrap();
    assert!(matches!(
        decoder.decode_plc(480),
        Err(AudioMediaError::Backpressure)
    ));
    assert_eq!(
        take(&mut decoder).samples(),
        oracle.decode(packets[0].payload(), 480)
    );
    for packet in &packets[1..3] {
        let pcm = decoder.decode_plc(480).unwrap();
        assert_eq!(pcm.timestamp_samples(), packet.timestamp_samples());
        assert_eq!(pcm.generation(), cfg.generation());
        assert_eq!(pcm.samples(), oracle.decode(&[], 480));
        assert!(decoder.poll_pcm().unwrap().is_none());
    }
    for packet in &packets[3..] {
        decoder.submit_packet(&wire(packet)).unwrap();
        assert_eq!(
            take(&mut decoder).samples(),
            oracle.decode(packet.payload(), 480)
        );
    }
}

#[test]
fn forged_headers_replay_and_malformed_framing_do_not_mutate_live_codec_history() {
    let cfg = config(AudioChannels::Mono, AudioDirection::Downlink, 10);
    let packets = packets(cfg, 3);
    let mut decoder = Decoder::new();
    decoder.configure(cfg).unwrap();
    let mut clean = Decoder::new();
    clean.configure(cfg).unwrap();
    decoder.submit_packet(&packets[0]).unwrap();
    clean.submit_packet(&packets[0]).unwrap();
    assert_eq!(
        decoder.submit_packet(&packets[1]),
        Err(AudioMediaError::Backpressure)
    );
    assert_eq!(take(&mut decoder).samples(), take(&mut clean).samples());
    let p = packets[1];
    let make = |dir, generation, seq, time, duration, data: &[u8]| {
        AudioAccessUnit::new(dir, generation, seq, time, duration, false, data).unwrap()
    };
    for bad in malformed_variants(&p, cfg) {
        assert_eq!(
            decoder.submit_packet(&bad),
            Err(AudioMediaError::InvalidPayload)
        );
    }
    assert_eq!(
        decoder.submit_packet(&packets[0]),
        Err(AudioMediaError::InvalidPayload)
    );
    let wrong_gen = make(
        p.direction(),
        AudioGeneration::from_raw(8),
        1,
        p.timestamp_samples(),
        480,
        p.payload(),
    );
    assert!(matches!(
        decoder.submit_packet(&wrong_gen),
        Err(AudioMediaError::GenerationMismatch { .. })
    ));
    let overflow = make(
        p.direction(),
        p.generation(),
        1,
        u64::MAX - 479,
        480,
        p.payload(),
    );
    assert_eq!(
        decoder.submit_packet(&overflow),
        Err(AudioMediaError::BufferOverflow)
    );
    assert!(matches!(
        decoder.decode_plc(240),
        Err(AudioMediaError::InvalidPayload)
    ));
    decoder.submit_packet(&p).unwrap();
    clean.submit_packet(&p).unwrap();
    assert_eq!(take(&mut decoder).samples(), take(&mut clean).samples());
}
fn packets_for_twenty_ms(cfg: AudioStreamConfig) -> AudioAccessUnit {
    packets(config(cfg.channels(), cfg.direction(), 20), 1)[0]
}

#[test]
fn generation_reset_discards_pending_sound_and_native_history_without_reallocation() {
    let cfg = config(AudioChannels::Stereo, AudioDirection::Downlink, 10);
    let p = packets(cfg, 1)[0];
    let mut decoder = Decoder::new();
    decoder.configure(cfg).unwrap();
    decoder.submit_packet(&p).unwrap();
    let native_bytes = decoder.native_state_bytes();
    let pcm_bytes = decoder.pcm_capacity_bytes();
    let generation = AudioGeneration::from_raw(9);
    decoder.reset(generation);
    assert_eq!(decoder.native_state_bytes(), native_bytes);
    assert_eq!(decoder.pcm_capacity_bytes(), pcm_bytes);
    assert!(decoder.poll_pcm().unwrap().is_none());
    assert!(matches!(
        decoder.submit_packet(&p),
        Err(AudioMediaError::GenerationMismatch { .. })
    ));
    assert!(matches!(
        decoder.decode_plc(480),
        Err(AudioMediaError::NeedMoreInput)
    ));
    let next = AudioStreamConfig::new(cfg.direction(), generation, cfg.channels(), 10, 20).unwrap();
    let fresh = packets(next, 1)[0];
    decoder.submit_packet(&fresh).unwrap();
    let pcm = take(&mut decoder);
    assert_eq!(pcm.generation(), generation);
    assert_eq!(
        pcm.samples(),
        Oracle::new(cfg.channels()).decode(fresh.payload(), 480)
    );
    decoder.reset(generation); // same generation is not a permitted replay reset
    assert_eq!(decoder.configuration(), None);
    assert_eq!(decoder.native_state_bytes(), 0);
    assert_eq!(decoder.pcm_capacity_bytes(), 0);
    assert!(matches!(
        decoder.poll_pcm(),
        Err(AudioMediaError::NotConfigured)
    ));
    assert_eq!(
        decoder.configure(next),
        Err(AudioMediaError::InvalidPayload)
    );
    assert_eq!(decoder.configure(cfg), Err(AudioMediaError::InvalidPayload));
}

#[test]
fn concealment_is_bounded_and_only_real_packets_replenish_the_allowance() {
    let cfg = config(AudioChannels::Mono, AudioDirection::Uplink, 10);
    let packets = packets(cfg, 12);
    let mut decoder = Decoder::new();
    decoder.configure(cfg).unwrap();
    decoder.submit_packet(&packets[0]).unwrap();
    take(&mut decoder);
    assert_eq!(MAX_CONCEALED_SAMPLES, 4800);
    for n in 1..=10 {
        let pcm = decoder.decode_plc(480).unwrap();
        assert_eq!(pcm.timestamp_samples(), 10_000 + n * 480);
    }
    for _ in 0..3 {
        assert!(matches!(
            decoder.decode_plc(480),
            Err(AudioMediaError::NeedMoreInput)
        ));
    }
    // Refused attempts do not spend a sequence or extend the timeline.
    decoder.submit_packet(&packets[11]).unwrap();
    assert_eq!(
        take(&mut decoder).timestamp_samples(),
        packets[11].timestamp_samples()
    );
    let pcm = decoder.decode_plc(480).unwrap();
    assert_eq!(
        pcm.timestamp_samples(),
        packets[11].timestamp_samples() + 480
    );
}

#[test]
fn extended_native_packets_use_the_negotiated_maximum_and_no_larger_pcm_buffer() {
    let base = config(AudioChannels::Stereo, AudioDirection::Downlink, 20);
    let mut encoder = Encoder::with_settings(32_000, 0).unwrap();
    encoder.configure(base).unwrap();
    let mut packets = Vec::new();
    for n in 0..6 {
        let frame = AudioPcmFrame::from_interleaved(
            base.generation(),
            base.channels(),
            n * 960,
            &[0; 1920],
        )
        .unwrap();
        encoder.submit_pcm(&frame).unwrap();
        packets.push(encoder.poll_packet().unwrap().unwrap());
    }
    for duration in [80_u16, 100, 120] {
        let config = AudioStreamConfig::new(
            base.direction(),
            base.generation(),
            base.channels(),
            duration,
            20,
        )
        .unwrap();
        let payload = aggregate(&packets[..usize::from(duration / 20)]);
        let samples = duration * 48;
        let packet = AudioAccessUnit::new(
            base.direction(),
            base.generation(),
            0,
            0,
            samples,
            false,
            &payload,
        )
        .unwrap();
        let mut decoder = Decoder::new();
        decoder.configure(config).unwrap();
        decoder.submit_packet(&wire(&packet)).unwrap();
        let pcm = take(&mut decoder);
        assert_eq!(pcm.samples_per_channel(), usize::from(samples));
        assert_eq!(decoder.pcm_capacity_bytes(), usize::from(samples) * 4);
        assert_eq!(
            pcm.samples(),
            Oracle::new(base.channels()).decode(&payload, usize::from(samples))
        );
    }
}

#[test]
fn sequence_and_timestamp_exhaustion_never_wrap_or_call_the_decoder() {
    let cfg = config(AudioChannels::Mono, AudioDirection::Downlink, 10);
    let p = packets(cfg, 1)[0];
    let mut decoder = Decoder::new();
    decoder.configure(cfg).unwrap();
    let packet = |sequence, timestamp| {
        AudioAccessUnit::new(
            p.direction(),
            p.generation(),
            sequence,
            timestamp,
            480,
            false,
            p.payload(),
        )
        .unwrap()
    };
    assert_eq!(
        decoder.submit_packet(&packet(u64::MAX, 0)),
        Err(AudioMediaError::BufferOverflow)
    );
    assert_eq!(
        decoder.submit_packet(&packet(0, u64::MAX - 479)),
        Err(AudioMediaError::BufferOverflow)
    );
    decoder
        .submit_packet(&packet(u64::MAX - 1, u64::MAX - 480))
        .unwrap();
    assert_eq!(take(&mut decoder).timestamp_samples(), u64::MAX - 480);
    assert!(matches!(
        decoder.decode_plc(480),
        Err(AudioMediaError::BufferOverflow)
    ));
    assert_eq!(
        decoder.submit_packet(&packet(u64::MAX, u64::MAX)),
        Err(AudioMediaError::BufferOverflow)
    );
    assert!(decoder.poll_pcm().unwrap().is_none());
}

#[test]
fn configuration_failure_preserves_original_stream_and_repeated_epoch_changes_drop_old_audio() {
    let base = config(AudioChannels::Mono, AudioDirection::Downlink, 10);
    let mut decoder = Decoder::new();
    assert!(matches!(
        decoder.poll_pcm(),
        Err(AudioMediaError::NotConfigured)
    ));
    decoder.configure(base).unwrap();
    let invalid = AudioStreamConfig::new(
        base.direction(),
        AudioGeneration::from_raw(1),
        base.channels(),
        7,
        20,
    )
    .unwrap();
    assert_eq!(
        decoder.configure(invalid),
        Err(AudioMediaError::UnsupportedFormat)
    );
    assert_eq!(decoder.configuration(), Some(base));
    for generation in 1..=24 {
        let channels = if generation % 2 == 0 {
            AudioChannels::Stereo
        } else {
            AudioChannels::Mono
        };
        let cfg = AudioStreamConfig::new(
            base.direction(),
            AudioGeneration::from_raw(generation),
            channels,
            10,
            20,
        )
        .unwrap();
        decoder.configure(cfg).unwrap();
        let p = packets(cfg, 1)[0];
        decoder.submit_packet(&p).unwrap();
        assert_eq!(decoder.configure(base), Err(AudioMediaError::Backpressure));
        assert_eq!(take(&mut decoder).generation(), cfg.generation());
        decoder.close();
        assert_eq!(decoder.pcm_capacity_bytes(), 0);
        assert_eq!(decoder.native_state_bytes(), 0);
        assert_eq!(decoder.configure(cfg), Err(AudioMediaError::InvalidPayload));
    }
}

fn malformed_variants(p: &AudioAccessUnit, cfg: AudioStreamConfig) -> [AudioAccessUnit; 7] {
    let make = |dir, generation, seq, time, duration, data: &[u8]| {
        AudioAccessUnit::new(dir, generation, seq, time, duration, false, data).unwrap()
    };
    [
        make(
            AudioDirection::Uplink,
            p.generation(),
            1,
            p.timestamp_samples(),
            480,
            p.payload(),
        ),
        make(
            p.direction(),
            p.generation(),
            1,
            p.timestamp_samples(),
            960,
            p.payload(),
        ),
        make(p.direction(), p.generation(), 1, 0, 480, p.payload()),
        make(
            p.direction(),
            p.generation(),
            2,
            p.timestamp_samples(),
            480,
            p.payload(),
        ),
        // Code 3 requires a second byte; a duration-only header query is not enough.
        make(
            p.direction(),
            p.generation(),
            1,
            p.timestamp_samples(),
            480,
            &[0x83],
        ),
        make(
            p.direction(),
            p.generation(),
            1,
            p.timestamp_samples(),
            480,
            &[0x83, 0x3f],
        ),
        // Structurally legal 20 ms Opus, falsely declared to be 10 ms.
        make(
            p.direction(),
            p.generation(),
            1,
            p.timestamp_samples(),
            480,
            packets_for_twenty_ms(cfg).payload(),
        ),
    ]
}

#[test]
fn admitted_packet_and_sample_ceilings_apply_before_codec_work_or_allocation() {
    use fr_native::opus::CodecLimits;
    for (bytes, samples) in [(0, 480), (1276, 480), (96, 0), (96, 5761)] {
        assert_eq!(
            CodecLimits::new(bytes, samples),
            Err(AudioMediaError::BufferOverflow)
        );
    }
    let cfg = config(AudioChannels::Stereo, AudioDirection::Downlink, 10);
    let small = CodecLimits::new(96, 240).unwrap();
    let mut encoder = Encoder::with_limits(small, 64_000, 5).unwrap();
    let mut decoder = Decoder::with_limits(small);
    assert_eq!(encoder.configure(cfg), Err(AudioMediaError::BufferOverflow));
    assert_eq!(decoder.configure(cfg), Err(AudioMediaError::BufferOverflow));
    assert_eq!(encoder.native_state_bytes(), 0);
    assert_eq!(decoder.native_state_bytes(), 0);
    assert_eq!(decoder.pcm_capacity_bytes(), 0);
    let admitted = CodecLimits::new(96, 480).unwrap();
    let mut encoder = Encoder::with_limits(admitted, 64_000, 5).unwrap();
    let mut decoder = Decoder::with_limits(admitted);
    encoder.configure(cfg).unwrap();
    decoder.configure(cfg).unwrap();
    assert_eq!(decoder.pcm_capacity_bytes(), 1920);
    let mut oversized = Encoder::with_settings(192_000, 5).unwrap();
    oversized.configure(cfg).unwrap();
    oversized.submit_pcm(&tone(cfg, 0)).unwrap();
    let packet = oversized.poll_packet().unwrap().unwrap();
    assert!(packet.payload().len() > admitted.max_packet_bytes());
    assert_eq!(
        decoder.submit_packet(&packet),
        Err(AudioMediaError::BufferOverflow)
    );
    encoder.submit_pcm(&tone(cfg, 0)).unwrap();
    let packet = encoder.poll_packet().unwrap().unwrap();
    assert!(packet.payload().len() <= 96);
    decoder.submit_packet(&packet).unwrap();
    assert_eq!(
        take(&mut decoder).samples(),
        Oracle::new(cfg.channels()).decode(packet.payload(), 480)
    );
    // A requested bitrate which cannot fit the ceiling is refused, not lowered.
    let mut encoder = Encoder::with_limits(admitted, 192_000, 5).unwrap();
    assert_eq!(
        encoder.configure(cfg),
        Err(AudioMediaError::UnsupportedFormat)
    );
    assert_eq!(encoder.native_state_bytes(), 0);
}

#[test]
fn source_gaps_are_preserved_without_manufactured_plc_or_input_borrow_retention() {
    let cfg = config(AudioChannels::Stereo, AudioDirection::Downlink, 10);
    let mut encoder = Encoder::new();
    encoder.configure(cfg).unwrap();
    let mut decoder = Decoder::new();
    decoder.configure(cfg).unwrap();
    let mut oracle = Oracle::new(cfg.channels());
    for at in [0, 480, 10_000_000] {
        let mut input = tone(cfg, at);
        encoder.submit_pcm(&input).unwrap();
        input.samples_mut().fill(0); // Submission did not retain the PCM borrow.
        let expected;
        {
            let packet = encoder.poll_packet().unwrap().unwrap();
            expected = oracle.decode(packet.payload(), 480);
            decoder.submit_packet(&packet).unwrap();
        } // Nor does decoding retain the packet's borrowed allocation.
        let frame = take(&mut decoder);
        assert_eq!(frame.samples(), expected);
        assert_eq!(frame.timestamp_samples(), at);
    }
    assert!(!format!("{decoder:?}").contains("payload"));
}
