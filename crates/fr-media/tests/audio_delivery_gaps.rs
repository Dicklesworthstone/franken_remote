#![forbid(unsafe_code)]
//! Production ring/lane regressions. No codec, network or device qualification.
use fr_core::audio::{AudioChannels, AudioDirection, AudioGeneration, AudioStopReason};
use fr_media::{
    audio::AudioAccessUnit,
    audio_delivery::{
        AudioLane, AudioRing, Error, LaneAction, MAX_PACKET_AGE_US, RING_BYTES, RING_PACKETS,
        SourceStream,
    },
};
use fr_wire::audio::AudioConfigured;

fn source() -> SourceStream {
    SourceStream {
        generation: AudioGeneration::from_raw(7),
        channels: AudioChannels::Stereo,
        frame_duration_ms: 20,
        max_packet_bytes: 1000,
        jitter_target_ms: 20,
    }
}
fn ring() -> AudioRing {
    let mut ring = AudioRing::new();
    ring.start(source()).unwrap();
    ring
}
fn packet(sequence: u64, size: usize) -> AudioAccessUnit {
    // Sequence arithmetic is independent of sample time. A fixed valid sample
    // timestamp lets the exhaustion tests exercise the entire u64 sequence space.
    AudioAccessUnit::new(
        AudioDirection::Downlink,
        source().generation,
        sequence,
        0,
        960,
        false,
        &vec![0x42; size],
    )
    .unwrap()
}
fn active(ring: &AudioRing) -> AudioLane {
    let mut lane = AudioLane::new();
    let LaneAction::Configure(offer) = lane.next(ring, 0) else {
        panic!("expected configuration, never implicit activation");
    };
    lane.configuration_sent(0).unwrap();
    lane.configured(
        AudioConfigured {
            direction: offer.direction,
            generation: offer.generation,
            accepted: true,
            actual_channels: offer.channels,
            actual_sample_rate: offer.sample_rate,
            actual_frame_duration_ms: offer.frame_duration_ms,
        },
        ring,
    )
    .unwrap();
    lane
}
fn expected(sequence: u64) -> LaneAction {
    LaneAction::Packet {
        sequence,
        generation: AudioGeneration::from_raw(1),
    }
}
fn assert_terminal_stop(lane: &mut AudioLane, ring: &AudioRing, now: u64) {
    let LaneAction::Stop(stop) = lane.next(ring, now) else {
        panic!("sequence exhaustion must stop, never wrap");
    };
    assert_eq!(stop.generation, AudioGeneration::from_raw(1));
    assert_eq!(stop.reason, AudioStopReason::SessionEnded);
    lane.stop_sent().unwrap();
    assert!(lane.is_stopped());
    assert_eq!(lane.next(ring, now), LaneAction::Nothing);
    assert_eq!(lane.packet_sent(0), Err(Error::WrongState));
}

#[test]
fn sparse_ring_lookup_uses_identity_not_sequence_distance() {
    let mut ring = ring();
    for sequence in [0, 2, 100, u64::MAX] {
        ring.push(packet(sequence, 10), 5).unwrap();
    }
    for sequence in [0, 2, 100, u64::MAX] {
        assert_eq!(ring.get(sequence).unwrap().unit.sequence(), sequence);
    }
    for missing in [1, 3, 99, u64::MAX - 1] {
        assert!(ring.get(missing).is_none());
    }
}

#[test]
fn a_capture_gap_does_not_stall_an_active_viewer() {
    let mut ring = ring();
    let mut lane = active(&ring);
    ring.push(packet(0, 10), 10).unwrap();
    ring.push(packet(2, 10), 10).unwrap();
    assert_eq!(lane.next(&ring, 10), expected(0));
    lane.packet_sent(0).unwrap();
    assert_eq!(lane.next(&ring, 10), expected(2));
    assert_eq!(lane.counters().dropped_missing, 1);
    // Transport backpressure retries the SAME offer without counting the gap twice.
    assert_eq!(lane.next(&ring, 11), expected(2));
    assert_eq!(lane.counters().dropped_missing, 1);
    lane.packet_sent(2).unwrap();
    assert_eq!(lane.next(&ring, 11), LaneAction::Nothing);
    assert_eq!(lane.counters().sent, 2);
}

#[test]
fn huge_gaps_skip_in_bounded_work_and_keep_source_sequences() {
    let mut ring = ring();
    let mut lane = active(&ring);
    let distant = 1_u64 << 60;
    ring.push(packet(0, 10), 10).unwrap();
    ring.push(packet(distant, 10), 10).unwrap();
    lane.packet_sent(0).unwrap();
    assert_eq!(lane.next(&ring, 10), expected(distant));
    assert_eq!(lane.counters().dropped_missing, distant - 1);
    assert_eq!(ring.len(), 2);
    assert_eq!(ring.bytes(), 20);
}

#[test]
fn a_new_viewer_never_replays_pre_ack_sparse_packets() {
    let mut ring = ring();
    ring.push(packet(0, 10), 1).unwrap();
    ring.push(packet(100, 10), 1).unwrap();
    let mut lane = active(&ring);
    assert_eq!(lane.next(&ring, 1), LaneAction::Nothing);
    assert_eq!(lane.counters().skipped_before_ack, 2);
    ring.push(packet(105, 10), 5).unwrap();
    assert_eq!(lane.next(&ring, 5), expected(105));
    assert_eq!(lane.counters().dropped_missing, 4);
}

#[test]
fn holes_and_expired_packets_do_not_hold_fresh_audio_behind_them() {
    let mut ring = ring();
    let mut lane = active(&ring);
    let now = MAX_PACKET_AGE_US + 1;
    ring.push(packet(0, 10), 0).unwrap();
    ring.push(packet(3, 10), 0).unwrap();
    ring.push(packet(9, 10), now).unwrap();
    assert_eq!(lane.next(&ring, now), expected(9));
    assert_eq!(lane.counters().dropped_obsolete, 2);
    assert_eq!(lane.counters().dropped_missing, 7);
    assert_eq!(lane.counters().sent, 0);
}

#[test]
fn sparse_lookup_survives_ring_wrap_and_independent_byte_eviction() {
    for size in [10, 1000] {
        let mut ring = ring();
        let mut lane = active(&ring);
        for sequence in (0..32).map(|n| n * 3) {
            ring.push(packet(sequence, size), 1).unwrap();
        }
        assert!(ring.len() <= RING_PACKETS);
        assert!(ring.bytes() <= RING_BYTES);
        let first = (32 - u64::try_from(ring.len()).unwrap()) * 3;
        for sequence in (first..96).step_by(3) {
            assert_eq!(ring.get(sequence).unwrap().unit.sequence(), sequence);
            assert_eq!(lane.next(&ring, 1), expected(sequence));
            lane.packet_sent(sequence).unwrap();
        }
        assert_eq!(lane.counters().dropped_evicted, first);
        assert_eq!(lane.next(&ring, 1), LaneAction::Nothing);
    }
}

#[test]
fn slow_and_fast_viewers_advance_independently_across_gaps() {
    let mut ring = ring();
    let mut fast = active(&ring);
    let mut slow = active(&ring);
    ring.push(packet(0, 10), 0).unwrap();
    assert_eq!(fast.next(&ring, 0), expected(0));
    fast.packet_sent(0).unwrap();
    let now = MAX_PACKET_AGE_US + 1;
    ring.push(packet(4, 10), now).unwrap();
    assert_eq!(fast.next(&ring, now), expected(4));
    assert_eq!(slow.next(&ring, now), expected(4));
    assert_eq!(fast.counters().dropped_obsolete, 0);
    assert_eq!(slow.counters().dropped_obsolete, 1);
    assert_eq!(fast.counters().dropped_missing, 3);
    assert_eq!(slow.counters().dropped_missing, 3);
}

#[test]
fn final_sequence_is_sent_once_then_stops_without_wrapping() {
    let mut ring = ring();
    let mut lane = active(&ring);
    ring.push(packet(u64::MAX, 10), 1).unwrap();
    assert_eq!(lane.next(&ring, 1), expected(u64::MAX));
    lane.packet_sent(u64::MAX).unwrap();
    assert_eq!(lane.counters().sent, 1);
    assert_terminal_stop(&mut lane, &ring, 1);
}

#[test]
fn obsolete_final_sequence_stops_without_wrapping_or_sending() {
    let mut ring = ring();
    let mut lane = active(&ring);
    ring.push(packet(u64::MAX, 10), 0).unwrap();
    assert_terminal_stop(&mut lane, &ring, MAX_PACKET_AGE_US + 1);
    assert_eq!(lane.counters().sent, 0);
    assert_eq!(lane.counters().dropped_obsolete, 1);
}

#[test]
fn final_sequence_before_ack_cannot_reopen_at_zero() {
    let mut ring = ring();
    ring.push(packet(u64::MAX, 10), 0).unwrap();
    let mut lane = active(&ring);
    assert_terminal_stop(&mut lane, &ring, 1);
    assert_eq!(lane.counters().skipped_before_ack, 1);
    assert_eq!(lane.counters().sent, 0);
}
