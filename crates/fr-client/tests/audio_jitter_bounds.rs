//! Adversarial packet metadata, not codec or hardware qualification.
use fr_client::audio::{AudioJitterBuffer, JitterDrainResult};
use fr_core::audio::{AudioDirection, AudioGeneration, MAX_JITTER_CEILING_MS};
use fr_media::audio::AudioAccessUnit;

fn packet(generation: AudioGeneration, sequence: u64, at: u64, samples: u16) -> AudioAccessUnit {
    AudioAccessUnit::new(
        AudioDirection::Downlink,
        generation,
        sequence,
        at,
        samples,
        false,
        &[0x42],
    )
    .unwrap()
}

#[test]
fn one_packet_cannot_exceed_the_entire_jitter_ceiling() {
    let generation = AudioGeneration::INITIAL;
    let mut jitter = AudioJitterBuffer::new(generation, 10);
    jitter.push_packet(packet(generation, 0, 0, 5760));
    assert!(jitter.current_depth_ms() <= MAX_JITTER_CEILING_MS);
}

#[test]
fn terminal_sequence_is_refused_before_cursor_overflow() {
    let generation = AudioGeneration::INITIAL;
    let mut jitter = AudioJitterBuffer::new(generation, 10);
    assert!(!jitter.push_packet(packet(generation, u64::MAX, 0, 480)));
    assert_eq!(jitter.drain(), JitterDrainResult::Underrun);
}

#[test]
fn old_generation_cannot_be_resurrected_by_reset() {
    let old = AudioGeneration::INITIAL;
    let new = old.next().unwrap();
    let mut jitter = AudioJitterBuffer::new(new, 10);
    jitter.reset_generation(old);
    assert!(!jitter.push_packet(packet(old, 0, 0, 480)));
    assert_eq!(jitter.drain(), JitterDrainResult::Underrun);
}

#[test]
fn enormous_sequence_holes_do_not_start_unbounded_concealment() {
    let generation = AudioGeneration::INITIAL;
    let mut jitter = AudioJitterBuffer::new(generation, 10);
    assert!(jitter.push_packet(packet(generation, 0, 0, 480)));
    assert!(matches!(jitter.drain(), JitterDrainResult::Packet(_)));
    jitter.push_packet(packet(generation, 1_000_000, 480_000_000, 480));
    let concealed = (0..12)
        .filter(|_| matches!(jitter.drain(), JitterDrainResult::Plc { .. }))
        .count();
    assert!(concealed <= 10);
}

use fr_client::audio::jitter::{JITTER_BUFFER_CAPACITY, JitterError};
use fr_core::audio::{AudioChannels, AudioStreamConfig};

fn configured(
    duration: u16,
    target: u16,
    direction: AudioDirection,
) -> Result<AudioJitterBuffer, JitterError> {
    AudioJitterBuffer::with_config(
        AudioStreamConfig::new(
            direction,
            AudioGeneration::INITIAL,
            AudioChannels::Mono,
            duration,
            target,
        )
        .unwrap(),
    )
}

#[test]
fn negotiated_duration_and_direction_reach_concealment_without_guessing() {
    let generation = AudioGeneration::INITIAL;
    for duration in [5, 10, 20, 40, 60, 80, 100] {
        for direction in [AudioDirection::Downlink, AudioDirection::Uplink] {
            let mut jitter = configured(duration, duration, direction).unwrap();
            let samples = duration * 48;
            let first =
                AudioAccessUnit::new(direction, generation, 9, 123, samples, false, &[0x42])
                    .unwrap();
            assert!(jitter.try_push_packet(first).unwrap());
            assert_eq!(
                jitter.drain_checked().unwrap(),
                JitterDrainResult::Packet(first)
            );
            let mut concealed = 0;
            while concealed + duration <= MAX_JITTER_CEILING_MS {
                assert_eq!(
                    jitter.next_sample_timestamp(),
                    Some(123 + u64::from(samples) + u64::from(concealed) * 48)
                );
                assert_eq!(
                    jitter.drain_for_playout().unwrap(),
                    JitterDrainResult::Plc {
                        missing_sequence: 10 + u64::from(concealed / duration),
                        duration_samples: samples,
                    }
                );
                concealed += duration;
            }
            assert_eq!(
                jitter.drain_for_playout(),
                Err(JitterError::ConcealmentExhausted)
            );
            assert_eq!(jitter.error(), Some(JitterError::ConcealmentExhausted));
            assert_eq!(jitter.queued_packet_count(), 0);
            assert_eq!(
                jitter.drain_for_playout(),
                Err(JitterError::ConcealmentExhausted)
            );
        }
    }
}

#[test]
fn unreachable_prebuffer_targets_and_overlong_packets_are_refused() {
    for (duration, target) in [(120, 10), (5, 100), (60, 100), (40, 90), (7, 20)] {
        assert!(matches!(
            configured(duration, target, AudioDirection::Downlink),
            Err(JitterError::InvalidConfiguration)
        ));
    }
    assert!(configured(5, 80, AudioDirection::Downlink).is_ok());
    assert!(configured(60, 60, AudioDirection::Downlink).is_ok());
}

#[test]
fn wrong_direction_duration_and_timestamp_do_not_poison_valid_queue() {
    let generation = AudioGeneration::INITIAL;
    let mut jitter = AudioJitterBuffer::new(generation, 10);
    let first = packet(generation, 0, 0, 480);
    assert!(jitter.try_push_packet(first).unwrap());
    let wrong_direction = AudioAccessUnit::new(
        AudioDirection::Uplink,
        generation,
        1,
        480,
        480,
        false,
        &[0x42],
    )
    .unwrap();
    assert_eq!(
        jitter.try_push_packet(wrong_direction),
        Err(JitterError::WrongDirection)
    );
    assert_eq!(
        jitter.try_push_packet(packet(generation, 1, 480, 960)),
        Err(JitterError::WrongDuration)
    );
    assert_eq!(
        jitter.try_push_packet(packet(generation, 1, 479, 480)),
        Err(JitterError::Timeline)
    );
    assert_eq!(
        jitter.try_push_packet(packet(generation, 1, 481, 480)),
        Err(JitterError::Timeline)
    );
    assert_eq!(jitter.error(), None);
    assert_eq!(
        jitter.drain_checked().unwrap(),
        JitterDrainResult::Packet(first)
    );
    assert!(
        jitter
            .try_push_packet(packet(generation, 1, 480, 480))
            .unwrap()
    );
    assert_eq!(jitter.metrics().packets_refused, 4);
}

#[test]
fn startup_eviction_keeps_newest_but_never_discards_newer_for_late_packets() {
    let generation = AudioGeneration::INITIAL;
    let mut jitter = AudioJitterBuffer::new(generation, 10);
    for seq in 100..130 {
        assert!(
            jitter
                .try_push_packet(packet(generation, seq, seq * 480, 480))
                .unwrap()
        );
        assert!(jitter.current_depth_ms() <= 100);
    }
    assert_eq!(jitter.queued_packet_count(), 10);
    assert!(
        !jitter
            .try_push_packet(packet(generation, 99, 99 * 480, 480))
            .unwrap()
    );
    assert_eq!(jitter.queued_packet_count(), 10);
    for seq in 120..130 {
        assert_eq!(
            jitter.drain_checked().unwrap(),
            JitterDrainResult::Packet(packet(generation, seq, seq * 480, 480))
        );
    }
}

#[test]
fn overflow_after_decode_fences_instead_of_skipping_decoder_history() {
    let generation = AudioGeneration::INITIAL;
    let mut jitter = AudioJitterBuffer::new(generation, 10);
    assert!(jitter.push_packet(packet(generation, 0, 0, 480)));
    let _ = jitter.drain();
    for seq in 1..=10 {
        assert!(jitter.push_packet(packet(generation, seq, seq * 480, 480)));
    }
    assert_eq!(
        jitter.try_push_packet(packet(generation, 11, 5280, 480)),
        Err(JitterError::WindowExceeded)
    );
    assert_eq!(jitter.queued_packet_count(), 0);
    assert_eq!(jitter.metrics().discontinuities, 1);
    assert_eq!(jitter.drain_checked(), Err(JitterError::WindowExceeded));
    assert_eq!(
        jitter.try_push_packet(packet(generation, 1, 480, 480)),
        Err(JitterError::WindowExceeded)
    );
    let fresh = generation.next().unwrap();
    jitter.reset_generation(fresh);
    assert!(jitter.push_packet(packet(fresh, 0, 0, 480)));
}

#[test]
fn sparse_packets_count_holes_in_the_window_not_just_payload_bytes() {
    let generation = AudioGeneration::INITIAL;
    let mut jitter = AudioJitterBuffer::new(generation, 10);
    assert!(jitter.push_packet(packet(generation, 0, 0, 480)));
    let _ = jitter.drain();
    assert_eq!(
        jitter.try_push_packet(packet(generation, 11, 5280, 480)),
        Err(JitterError::WindowExceeded)
    );
    assert_eq!(jitter.metrics().plc_concealment_events, 0);
}

#[test]
fn late_concealed_packets_do_not_replay_or_reset_the_loss_budget() {
    let generation = AudioGeneration::INITIAL;
    let mut jitter = AudioJitterBuffer::new(generation, 10);
    assert!(jitter.push_packet(packet(generation, 0, 0, 480)));
    let _ = jitter.drain();
    for seq in 1..=10 {
        assert!(matches!(
            jitter.drain_for_playout(),
            Ok(JitterDrainResult::Plc { .. })
        ));
        assert!(!jitter.push_packet(packet(generation, seq, seq * 480, 480)));
    }
    assert_eq!(
        jitter.drain_for_playout(),
        Err(JitterError::ConcealmentExhausted)
    );
}

#[test]
fn exact_counter_boundaries_never_wrap_even_during_trailing_loss() {
    let generation = AudioGeneration::INITIAL;
    for (seq, at) in [(u64::MAX - 1, 0), (0, u64::MAX - 480)] {
        let mut jitter = AudioJitterBuffer::new(generation, 10);
        assert!(
            jitter
                .try_push_packet(packet(generation, seq, at, 480))
                .unwrap()
        );
        assert!(matches!(
            jitter.drain_checked(),
            Ok(JitterDrainResult::Packet(_))
        ));
        assert_eq!(
            jitter.drain_for_playout(),
            Err(JitterError::CounterOverflow)
        );
    }
    let mut jitter = AudioJitterBuffer::new(generation, 10);
    assert_eq!(
        jitter.try_push_packet(packet(generation, 0, u64::MAX - 479, 480)),
        Err(JitterError::CounterOverflow)
    );
}

#[test]
fn reused_generation_and_stop_clear_buffers_without_lowering_epoch_floor() {
    let generation = AudioGeneration::from_raw(9);
    let mut jitter = AudioJitterBuffer::new(generation, 10);
    assert!(jitter.push_packet(packet(generation, 0, 0, 480)));
    jitter.reset_generation(generation);
    assert_eq!(jitter.error(), Some(JitterError::GenerationNotAdvanced));
    assert_eq!(jitter.generation(), generation);
    assert_eq!(jitter.queued_packet_count(), 0);
    jitter.reset_generation(AudioGeneration::from_raw(8));
    assert_eq!(jitter.generation(), generation);
    let fresh = generation.next().unwrap();
    jitter.reset_generation(fresh);
    assert!(jitter.push_packet(packet(fresh, 0, 0, 480)));
    jitter.stop();
    assert_eq!(jitter.error(), Some(JitterError::Stopped));
    assert_eq!(jitter.queued_packet_count(), 0);
}

#[test]
fn every_eight_packet_loss_pattern_is_bounded_and_sequence_exact() {
    let generation = AudioGeneration::INITIAL;
    for loss_mask in 0_u16..256 {
        let mut jitter = AudioJitterBuffer::new(generation, 10);
        assert!(jitter.push_packet(packet(generation, 0, 0, 480)));
        let _ = jitter.drain_checked().unwrap();
        // Reverse network order; the last packet closes every interior hole.
        for seq in (1..=9_u64).rev() {
            if seq == 9 || loss_mask & (1 << (seq - 1)) == 0 {
                assert!(
                    jitter
                        .try_push_packet(packet(generation, seq, seq * 480, 480))
                        .unwrap()
                );
            }
        }
        for seq in 1..=9_u64 {
            let expected = if seq != 9 && loss_mask & (1 << (seq - 1)) != 0 {
                JitterDrainResult::Plc {
                    missing_sequence: seq,
                    duration_samples: 480,
                }
            } else {
                JitterDrainResult::Packet(packet(generation, seq, seq * 480, 480))
            };
            assert_eq!(
                jitter.drain_checked().unwrap(),
                expected,
                "mask={loss_mask}, seq={seq}"
            );
            assert!(jitter.current_depth_ms() <= MAX_JITTER_CEILING_MS);
            assert!(jitter.queued_packet_count() <= JITTER_BUFFER_CAPACITY);
        }
        assert_eq!(jitter.drain_checked().unwrap(), JitterDrainResult::Underrun);
    }
}
