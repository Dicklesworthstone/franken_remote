#![cfg(all(
    target_os = "linux",
    feature = "linux-opus-process",
    feature = "linux-audio-playout"
))]
//! Real Opus child + wire + production playout, using explicit synthetic device
//! clocks and a borrowed PCM sink. Not a speaker, network or live-session test.
use fr_client::{
    audio::playout::{PlayoutClock, PlayoutResult},
    input::ClientInstant,
};
use fr_core::audio::{AudioChannels, AudioDirection, AudioGeneration, AudioStreamConfig};
use fr_media::audio::{AudioAccessUnit, AudioDecoder, AudioEncoder, AudioPcmFrame};
use fr_native::opus::{
    CodecLimits, Decoder, Encoder,
    playout::{OpusPlayout, ReceiveResult},
    process::ProcessDecoder,
};
use fr_wire::audio::{self, AudioConfiguration, AudioPacket};
use std::{
    path::Path,
    time::{Duration, Instant},
};

const BINDING: u32 = 73;
fn clock(ms: u64) -> PlayoutClock {
    PlayoutClock {
        now: ClientInstant(ms * 1000),
        output_samples: ms * 48,
    }
}
fn record(packet: &AudioAccessUnit) -> Vec<u8> {
    let mut bytes = vec![0; audio::AUDIO_PACKET_OVERHEAD + 1275];
    let size = audio::encode_packet(
        &AudioPacket {
            direction: packet.direction(),
            generation: packet.generation(),
            sequence: packet.sequence(),
            timestamp_samples: packet.timestamp_samples(),
            duration_samples: packet.duration_samples(),
            payload: packet.payload(),
        },
        BINDING,
        &mut bytes,
    )
    .unwrap();
    bytes.truncate(size);
    bytes
}
fn wait(mut check: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(2);
    while !check() {
        assert!(Instant::now() < until, "native operation deadline");
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn signal(config: AudioStreamConfig) -> (Vec<AudioAccessUnit>, Vec<AudioPcmFrame>) {
    let generation = config.generation();
    let channels = config.channels();
    let mut encoder = Encoder::new();
    let mut reference = Decoder::new();
    encoder.configure(config).unwrap();
    reference.configure(config).unwrap();
    let mut packets = Vec::new();
    let mut expected = Vec::new();
    for sequence in 0..3_u64 {
        let count = usize::try_from(config.expected_samples_per_frame()).unwrap();
        let samples: Vec<_> = (0..count * usize::from(channels.count()))
            .map(|i| (i16::try_from(i % 97).unwrap() - 48) * 250)
            .collect();
        let pcm = AudioPcmFrame::from_interleaved(
            generation,
            channels,
            900_000 + sequence * u64::from(config.expected_samples_per_frame()),
            &samples,
        )
        .unwrap();
        encoder.submit_pcm(&pcm).unwrap();
        let packet = encoder.poll_packet().unwrap().unwrap();
        reference.submit_packet(&packet).unwrap();
        expected.push(reference.poll_pcm().unwrap().unwrap());
        packets.push(packet);
    }
    (packets, expected)
}

#[test]
fn the_real_opus_child_decodes_ahead_but_outputs_exact_pcm_only_in_its_original_slot() {
    for channels in [AudioChannels::Mono, AudioChannels::Stereo] {
        for duration in [10_u16, 20] {
            let generation = AudioGeneration::from_raw(7);
            let config = AudioStreamConfig::new(
                AudioDirection::Downlink,
                generation,
                channels,
                duration,
                20,
            )
            .unwrap();
            let offer = AudioConfiguration {
                direction: config.direction(),
                generation,
                channels,
                sample_rate: 48_000,
                frame_duration_ms: duration,
                max_packet_bytes: 1275,
                max_decoded_samples: config.expected_samples_per_frame(),
                jitter_target_ms: 20,
            };
            let (packets, expected) = signal(config);
            assert!(expected.iter().any(|p| p.samples().iter().any(|&s| s != 0)));
            let (decoder, retirement) = ProcessDecoder::new(
                Path::new(env!("CARGO_BIN_EXE_fr-opus-worker")),
                CodecLimits::ABSOLUTE,
            )
            .unwrap();
            let mut receiver =
                OpusPlayout::with_decoder(BINDING, offer, clock(0), decoder).unwrap();
            wait(|| receiver.poll_configured(clock(0)).unwrap());
            assert!(retirement.process_id().is_some());
            for packet in &packets {
                assert_eq!(
                    receiver.receive_record(&record(packet), clock(0)).unwrap(),
                    ReceiveResult::Queued
                );
            }
            for (sequence, expected) in expected.iter().enumerate() {
                let due_ms = 20 + u64::try_from(sequence).unwrap() * u64::from(duration);
                if sequence != 0 {
                    let before = receiver.queued_packets();
                    assert_eq!(
                        receiver
                            .render(|| Ok(clock(due_ms - 1)), |_, _| panic!("early PCM"))
                            .unwrap(),
                        PlayoutResult::Waiting
                    );
                    assert_eq!(
                        receiver.queued_packets(),
                        before - 1,
                        "real packet handed to the child early"
                    );
                    for _ in 0..20 {
                        assert_eq!(
                            receiver
                                .render(|| Ok(clock(due_ms - 1)), |_, _| panic!("early PCM"))
                                .unwrap(),
                            PlayoutResult::Waiting
                        );
                    }
                }
                wait(|| {
                    matches!(
                        receiver
                            .render(
                                || Ok(clock(due_ms)),
                                |pcm, receipt| {
                                    assert!(
                                        pcm.samples() == expected.samples(),
                                        "native PCM differs"
                                    );
                                    assert_eq!(
                                        pcm.timestamp_samples(),
                                        expected.timestamp_samples()
                                    );
                                    assert_eq!(receipt.sequence, u64::try_from(sequence).unwrap());
                                    assert_eq!(receipt.output_samples, due_ms * 48);
                                    assert_eq!(
                                        receipt.output_valid_before,
                                        (due_ms + u64::from(duration)) * 48
                                    );
                                    assert_eq!(receipt.valid_until, ClientInstant(100_000));
                                    assert!(!receipt.concealed);
                                    Ok(())
                                }
                            )
                            .unwrap(),
                        PlayoutResult::Submitted(_)
                    )
                });
            }
            receiver.stop();
            assert_eq!(receiver.queued_packets(), 0);
            wait(|| retirement.is_complete());
        }
    }
}
