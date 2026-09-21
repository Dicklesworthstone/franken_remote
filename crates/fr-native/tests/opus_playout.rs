#![cfg(all(target_os = "linux", feature = "linux-audio-playout"))]
//! Real libopus composition tests, not physical-device or session qualification.
//! The reference uses an independently driven native decoder to verify ordering
//! and exact PLC history; it is not an independent codec implementation.
use fr_client::{
    audio::playout::{PlayoutClock, PlayoutError, PlayoutResult},
    input::ClientInstant,
};
use fr_core::audio::{
    AudioChannels, AudioDirection, AudioGeneration, AudioStopReason, AudioStreamConfig,
};
use fr_media::audio::{
    AudioAccessUnit, AudioDecoder, AudioEncoder, AudioMediaError, AudioPcmFrame,
};
use fr_native::opus::{
    Decoder, Encoder,
    playout::{Error, OpusPlayout, ReceiveResult},
};
use fr_wire::{
    WireError,
    audio::{self, AudioConfiguration, AudioPacket, AudioStop},
};

const BINDING: u32 = 73;
fn clock(ms: u64) -> PlayoutClock {
    PlayoutClock {
        now: ClientInstant(1_000_000 + ms * 1000),
        output_samples: 30_000 + ms * 48,
    }
}
fn offer(
    direction: AudioDirection,
    channels: AudioChannels,
    duration: u16,
    target: u16,
) -> AudioConfiguration {
    AudioConfiguration {
        direction,
        generation: AudioGeneration::from_raw(7),
        channels,
        sample_rate: 48_000,
        frame_duration_ms: duration,
        max_packet_bytes: 1275,
        max_decoded_samples: u32::from(duration) * 48,
        jitter_target_ms: target,
    }
}
fn config(offer: AudioConfiguration) -> AudioStreamConfig {
    AudioStreamConfig::new(
        offer.direction,
        offer.generation,
        offer.channels,
        offer.frame_duration_ms,
        offer.jitter_target_ms,
    )
    .unwrap()
}
fn packets(offer: AudioConfiguration, count: u16) -> Vec<AudioAccessUnit> {
    let config = config(offer);
    let samples = usize::try_from(config.expected_samples_per_frame()).unwrap();
    let mut encoder = Encoder::new();
    encoder.configure(config).unwrap();
    (0..count)
        .map(|sequence| {
            let mut data = Vec::with_capacity(samples * usize::from(offer.channels.count()));
            for i in 0..samples {
                let phase = (i + usize::from(sequence) * samples) % 97;
                let left = (i16::try_from(phase).unwrap() - 48) * 250;
                data.push(left);
                if offer.channels == AudioChannels::Stereo {
                    data.push(-left);
                }
            }
            let at = 17_000_000 + u64::from(sequence) * u64::try_from(samples).unwrap();
            let frame =
                AudioPcmFrame::from_interleaved(offer.generation, offer.channels, at, &data)
                    .unwrap();
            encoder.submit_pcm(&frame).unwrap();
            encoder.poll_packet().unwrap().unwrap()
        })
        .collect()
}
fn wire(packet: &AudioAccessUnit, binding: u32) -> Vec<u8> {
    let packet = AudioPacket {
        direction: packet.direction(),
        generation: packet.generation(),
        sequence: packet.sequence(),
        timestamp_samples: packet.timestamp_samples(),
        duration_samples: packet.duration_samples(),
        payload: packet.payload(),
    };
    let mut bytes = vec![0; audio::AUDIO_PACKET_OVERHEAD + 1275];
    let n = audio::encode_packet(&packet, binding, &mut bytes).unwrap();
    bytes.truncate(n);
    bytes
}
fn stop_bytes(stop: AudioStop, binding: u32) -> Vec<u8> {
    let mut bytes = vec![0; audio::AUDIO_STOP_RECORD_BYTES];
    audio::encode_stop(&stop, binding, &mut bytes).unwrap();
    bytes
}

#[test]
fn every_native_profile_reaches_clock_paced_pcm_through_real_wire_records() {
    for duration in [5, 10, 20, 40, 60] {
        for direction in [AudioDirection::Downlink, AudioDirection::Uplink] {
            for channels in [AudioChannels::Mono, AudioChannels::Stereo] {
                let offer = offer(direction, channels, duration, duration.min(20));
                let packets = packets(offer, 12);
                let mut receiver = OpusPlayout::new(BINDING, offer, clock(0)).unwrap();
                let mut reference = Decoder::new();
                reference.configure(config(offer)).unwrap();
                let mut nonzero = false;
                for packet in packets {
                    let time = packet.sequence() * u64::from(duration);
                    let record = wire(&packet, BINDING);
                    assert_eq!(
                        receiver.receive_record(&record, clock(time)).unwrap(),
                        ReceiveResult::Queued
                    );
                    reference.submit_packet(&packet).unwrap();
                    let expected = reference.poll_pcm().unwrap().unwrap();
                    let due = clock(time + u64::from(offer.jitter_target_ms));
                    let result = receiver
                        .render(
                            || Ok(due),
                            |pcm, receipt| {
                                assert!(
                                    pcm.samples() == expected.samples(),
                                    "ordered decode differs"
                                );
                                assert_eq!(pcm.timestamp_samples(), packet.timestamp_samples());
                                assert_eq!(pcm.generation(), offer.generation);
                                assert_eq!(pcm.channels(), channels);
                                assert_eq!(receipt.direction, direction);
                                assert!(!receipt.concealed);
                                assert_eq!(receipt.output_samples, due.output_samples);
                                assert_eq!(
                                    receipt.output_valid_before,
                                    due.output_samples + u64::from(duration) * 48
                                );
                                assert_eq!(
                                    receipt.valid_until,
                                    ClientInstant(clock(time).now.0 + 100_000)
                                );
                                nonzero |= pcm.samples().iter().any(|&s| s != 0);
                                Ok(())
                            },
                        )
                        .unwrap();
                    assert!(matches!(result, PlayoutResult::Submitted(_)));
                    assert_eq!(
                        receiver
                            .render(|| Ok(due), |_, _| panic!("repeat output"))
                            .unwrap(),
                        PlayoutResult::Waiting
                    );
                }
                assert!(nonzero, "actual libopus output must contain decoded signal");
            }
        }
    }
}

#[test]
fn reordered_dropped_duplicated_packets_use_real_plc_with_exact_native_history() {
    let offer = offer(AudioDirection::Downlink, AudioChannels::Stereo, 10, 20);
    let packets = packets(offer, 9);
    let mut receiver = OpusPlayout::new(BINDING, offer, clock(0)).unwrap();
    // Network delivers 2 before 0; 1 and 5 are lost entirely.
    for seq in [2, 0] {
        assert_eq!(
            receiver
                .receive_record(&wire(&packets[seq], BINDING), clock(0))
                .unwrap(),
            ReceiveResult::Queued
        );
    }
    let mut reference = Decoder::new();
    reference.configure(config(offer)).unwrap();
    for seq in 0..packets.len() {
        let due = clock(20 + u64::try_from(seq).unwrap() * 10);
        let ahead = seq + 2;
        if ahead < packets.len() && ahead != 5 {
            let _ = receiver
                .receive_record(&wire(&packets[ahead], BINDING), due)
                .unwrap();
        }
        let lost = matches!(seq, 1 | 5);
        let expected = if lost {
            reference.decode_plc(480).unwrap()
        } else {
            reference.submit_packet(&packets[seq]).unwrap();
            reference.poll_pcm().unwrap().unwrap()
        };
        receiver
            .render(
                || Ok(due),
                |pcm, receipt| {
                    assert_eq!(receipt.sequence, u64::try_from(seq).unwrap());
                    assert_eq!(receipt.concealed, lost);
                    assert_eq!(pcm.timestamp_samples(), expected.timestamp_samples());
                    assert!(
                        pcm.samples() == expected.samples(),
                        "concealment/order changed codec history"
                    );
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(
            receiver
                .receive_record(&wire(&packets[seq], BINDING), due)
                .unwrap(),
            ReceiveResult::Ignored
        );
        for _ in 0..8 {
            assert_eq!(
                receiver
                    .render(|| Ok(due), |_, _| panic!("polling generated audio"))
                    .unwrap(),
                PlayoutResult::Waiting
            );
        }
    }
}

#[test]
fn bound_matching_stop_retires_real_decoder_and_remote_config_cannot_reopen_it() {
    let offer = offer(AudioDirection::Downlink, AudioChannels::Mono, 10, 20);
    let packets = packets(offer, 2);
    let mut receiver = OpusPlayout::new(BINDING, offer, clock(0)).unwrap();
    for packet in &packets {
        receiver
            .receive_record(&wire(packet, BINDING), clock(0))
            .unwrap();
    }
    let stop = AudioStop {
        direction: offer.direction,
        generation: offer.generation,
        reason: AudioStopReason::UserMute,
    };
    assert_eq!(
        receiver.receive_record(&stop_bytes(stop, BINDING + 1), clock(0)),
        Err(Error::Wire(WireError::InvalidBinding))
    );
    for other in [
        AudioStop {
            direction: AudioDirection::Uplink,
            ..stop
        },
        AudioStop {
            generation: offer.generation.next().unwrap(),
            ..stop
        },
    ] {
        assert_eq!(
            receiver
                .receive_record(&stop_bytes(other, BINDING), clock(0))
                .unwrap(),
            ReceiveResult::Ignored
        );
    }
    assert_eq!(receiver.queued_packets(), 2);
    for _ in 0..2 {
        assert_eq!(
            receiver
                .receive_record(&stop_bytes(stop, BINDING), clock(0))
                .unwrap(),
            ReceiveResult::Stopped(AudioStopReason::UserMute)
        );
    }
    assert_eq!(receiver.queued_packets(), 0);
    assert_eq!(
        receiver.render(|| Ok(clock(20)), |_, _| panic!("stopped sound")),
        Err(Error::Playout(PlayoutError::Stopped))
    );
    let fresh = AudioConfiguration {
        generation: offer.generation.next().unwrap(),
        ..offer
    };
    let mut bytes = vec![0; audio::AUDIO_CONFIGURATION_RECORD_BYTES];
    audio::encode_configuration(&fresh, BINDING, &mut bytes).unwrap();
    assert!(receiver.receive_record(&bytes, clock(20)).is_err());
    assert_eq!(receiver.error(), Some(PlayoutError::Stopped));
}

#[test]
fn negotiated_resource_limits_refuse_before_queue_copy_or_native_output() {
    let offer = offer(AudioDirection::Downlink, AudioChannels::Mono, 10, 10);
    let packet = packets(offer, 1)[0];
    assert!(packet.payload().len() > 1);
    let limited = AudioConfiguration {
        max_packet_bytes: 1,
        ..offer
    };
    let mut receiver = OpusPlayout::new(BINDING, limited, clock(0)).unwrap();
    assert_eq!(
        receiver.receive_record(&wire(&packet, BINDING), clock(0)),
        Err(Error::Wire(WireError::ResourceLimit))
    );
    assert_eq!(receiver.queued_packets(), 0);
    assert!(matches!(
        OpusPlayout::new(
            BINDING,
            AudioConfiguration {
                max_decoded_samples: 479,
                ..offer
            },
            clock(0)
        ),
        Err(Error::Configuration)
    ));
    assert!(matches!(
        OpusPlayout::new(0, offer, clock(0)),
        Err(Error::Binding)
    ));
    assert_eq!(
        receiver.render(|| Ok(clock(10)), |_, _| panic!("unadmitted packet")),
        Ok(PlayoutResult::Waiting)
    );
}

#[test]
fn malformed_real_opus_and_revocation_during_native_decode_never_submit_pcm() {
    let offer = offer(AudioDirection::Downlink, AudioChannels::Mono, 10, 10);
    let malformed =
        AudioAccessUnit::new(offer.direction, offer.generation, 0, 0, 480, false, &[0xff]).unwrap();
    let mut receiver = OpusPlayout::new(BINDING, offer, clock(0)).unwrap();
    assert_eq!(
        receiver
            .receive_record(&wire(&malformed, BINDING), clock(0))
            .unwrap(),
        ReceiveResult::Queued
    );
    assert_eq!(
        receiver.render(|| Ok(clock(10)), |_, _| panic!("malformed codec output")),
        Err(Error::Playout(PlayoutError::Codec(
            AudioMediaError::InvalidPayload
        )))
    );
    assert_eq!(receiver.queued_packets(), 0);
    let packet = packets(offer, 1)[0];
    let mut receiver = OpusPlayout::new(BINDING, offer, clock(0)).unwrap();
    receiver
        .receive_record(&wire(&packet, BINDING), clock(0))
        .unwrap();
    let mut calls = 0;
    assert_eq!(
        receiver.render(
            || {
                calls += 1;
                if calls == 1 {
                    Ok(clock(10))
                } else {
                    Err(PlayoutError::Denied)
                }
            },
            |_, _| panic!("revoked native output")
        ),
        Err(Error::Playout(PlayoutError::Denied))
    );
    assert_eq!(calls, 2);
    assert_eq!(receiver.queued_packets(), 0);
}

#[test]
fn new_epoch_has_fresh_native_history_and_old_wire_packets_do_not_enter_it() {
    let offer = offer(AudioDirection::Downlink, AudioChannels::Mono, 10, 10);
    let old = packets(offer, 2);
    let mut receiver = OpusPlayout::new(BINDING, offer, clock(0)).unwrap();
    receiver
        .receive_record(&wire(&old[0], BINDING), clock(0))
        .unwrap();
    receiver.render(|| Ok(clock(10)), |_, _| Ok(())).unwrap();
    receiver
        .receive_record(&wire(&old[1], BINDING), clock(10))
        .unwrap();
    receiver.stop();
    drop(receiver);
    let fresh = AudioConfiguration {
        generation: offer.generation.next().unwrap(),
        channels: AudioChannels::Stereo,
        ..offer
    };
    let packet = packets(fresh, 1)[0];
    let mut receiver = OpusPlayout::new(BINDING, fresh, clock(0)).unwrap();
    assert_eq!(
        receiver
            .receive_record(&wire(&old[1], BINDING), clock(0))
            .unwrap(),
        ReceiveResult::Ignored
    );
    receiver
        .receive_record(&wire(&packet, BINDING), clock(0))
        .unwrap();
    let mut reference = Decoder::new();
    reference.configure(config(fresh)).unwrap();
    reference.submit_packet(&packet).unwrap();
    let expected = reference.poll_pcm().unwrap().unwrap();
    receiver
        .render(
            || Ok(clock(10)),
            |pcm, receipt| {
                assert_eq!(receipt.generation, fresh.generation);
                assert_eq!(pcm.channels(), fresh.channels);
                assert!(
                    pcm.samples() == expected.samples(),
                    "new epoch retained prior native state"
                );
                Ok(())
            },
        )
        .unwrap();
}
