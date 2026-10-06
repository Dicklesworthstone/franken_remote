//! Production ring/lane admission with explicit clocks; no native-code claim.
use super::*;
use fr_core::audio::{AudioChannels, AudioGeneration};
use fr_media::{
    audio::AudioAccessUnit,
    audio_delivery::{AudioLane, LaneAction},
    worker::audio::CaptureCounters,
};
use fr_wire::audio::AudioConfigured;

fn setup() -> (Admission, AudioRing, AudioLane) {
    let stream = SourceStream {
        generation: AudioGeneration::from_raw(1),
        channels: AudioChannels::Stereo,
        frame_duration_ms: 20,
        max_packet_bytes: 1000,
        jitter_target_ms: 20,
    };
    let mut ring = AudioRing::new();
    ring.start(stream).unwrap();
    let mut lane = AudioLane::new();
    let LaneAction::Configure(config) = lane.next(&ring, 0) else {
        panic!("configuration")
    };
    lane.configuration_sent(0).unwrap();
    lane.configured(
        AudioConfigured {
            direction: AudioDirection::Downlink,
            generation: config.generation,
            accepted: true,
            actual_channels: stream.channels,
            actual_sample_rate: OPUS_SAMPLE_RATE,
            actual_frame_duration_ms: 20,
        },
        &ring,
    )
    .unwrap();
    (Admission::new(stream), ring, lane)
}
fn packet(sequence: u64, timestamp: u64) -> AudioAccessUnit {
    AudioAccessUnit::new(
        AudioDirection::Downlink,
        AudioGeneration::from_raw(1),
        sequence,
        timestamp,
        960,
        false,
        &[1, 2],
    )
    .unwrap()
}
fn batch(packets: Vec<AudioAccessUnit>) -> Batch {
    Batch {
        packets,
        counters: CaptureCounters::default(),
    }
}

#[test]
fn reply_wait_consumes_the_original_admission_budget() {
    let (mut admission, mut ring, mut lane) = setup();
    admission
        .admit(batch(vec![packet(0, 0)]), &mut ring, 100_000, 139_999)
        .unwrap();
    assert_eq!(ring.get(0).unwrap().captured_us, 100_000);
    assert_eq!(ring.get(0).unwrap().send_deadline(139_999), Some(140_000));
    assert!(matches!(
        lane.next(&ring, 139_999),
        LaneAction::Packet { sequence: 0, .. }
    ));
    assert_eq!(lane.next(&ring, 140_000), LaneAction::Nothing);
}

#[test]
fn expired_reply_is_dropped_and_a_later_pull_continues_without_reopening() {
    for completed in [140_000, 500_000] {
        let (mut admission, mut ring, mut lane) = setup();
        admission
            .admit(batch(vec![packet(0, 0)]), &mut ring, 100_000, completed)
            .unwrap();
        assert!(ring.is_empty());
        assert_eq!(admission.obsolete, 1);
        assert_eq!(lane.next(&ring, completed), LaneAction::Nothing);
        admission
            .admit(batch(vec![packet(1, 960)]), &mut ring, 600_000, 601_000)
            .unwrap();
        assert!(matches!(
            lane.next(&ring, 601_000),
            LaneAction::Packet { sequence: 1, .. }
        ));
        assert_eq!(lane.counters().configurations, 1);
        assert_eq!(admission.admitted, 1);
    }
}

#[test]
fn sample_holes_are_not_compressed_into_adjacent_packet_slots() {
    let (mut admission, mut ring, _) = setup();
    admission
        .admit(
            batch(vec![packet(0, 0), packet(1, 4800), packet(2, 5760)]),
            &mut ring,
            200_000,
            200_001,
        )
        .unwrap();
    assert!(ring.get(0).is_none());
    assert_eq!(ring.get(1).unwrap().captured_us, 180_000);
    assert_eq!(ring.get(2).unwrap().captured_us, 200_000);
    assert_eq!((admission.admitted, admission.obsolete), (2, 1));
}

#[test]
fn obsolete_batches_still_consume_their_packet_identities() {
    let (mut admission, mut ring, _) = setup();
    admission
        .admit(batch(vec![packet(10, 9600)]), &mut ring, 100_000, 200_000)
        .unwrap();
    for unit in [packet(10, 9600), packet(9, 8640), packet(11, 9601)] {
        assert_eq!(
            admission.admit(batch(vec![unit]), &mut ring, 300_000, 300_001),
            Err(Error::Ordering)
        );
        assert!(ring.is_empty());
    }
    assert_eq!(admission.obsolete, 1);
}

#[test]
fn a_bad_tail_never_partially_admits_a_batch() {
    let (mut admission, mut ring, _) = setup();
    assert_eq!(
        admission.admit(
            batch(vec![packet(0, 0), packet(1, 959)]),
            &mut ring,
            100_000,
            100_001,
        ),
        Err(Error::Ordering)
    );
    assert!(ring.is_empty());
    assert_eq!(admission.last, None);
}

#[test]
fn underflow_drops_old_work_and_clock_overflow_or_regression_refuses() {
    let (mut admission, mut ring, _) = setup();
    admission
        .admit(
            batch(vec![packet(0, 0), packet(1, 960)]),
            &mut ring,
            10_000,
            10_001,
        )
        .unwrap();
    assert!(ring.get(0).is_none());
    assert_eq!(ring.get(1).unwrap().captured_us, 10_000);
    for (requested, completed) in [(100, 99), (u64::MAX, u64::MAX)] {
        assert_eq!(
            admission.admit(batch(vec![]), &mut ring, requested, completed),
            Err(Error::Clock)
        );
    }
}

#[test]
fn empty_pulls_never_refresh_an_existing_packet() {
    let (mut admission, mut ring, mut lane) = setup();
    admission
        .admit(batch(vec![packet(0, 0)]), &mut ring, 100_000, 100_001)
        .unwrap();
    admission
        .admit(batch(vec![]), &mut ring, 130_000, 130_001)
        .unwrap();
    assert_eq!(ring.get(0).unwrap().captured_us, 100_000);
    assert_eq!(lane.next(&ring, 140_000), LaneAction::Nothing);
}

#[test]
fn source_retirement_and_foreign_epochs_cannot_receive_a_batch() {
    let (mut admission, mut ring, _) = setup();
    ring.idle();
    assert_eq!(
        admission.admit(batch(vec![packet(0, 0)]), &mut ring, 100_000, 100_001),
        Err(Error::Ring)
    );
    ring.start(SourceStream {
        generation: AudioGeneration::from_raw(2),
        ..admission.stream
    })
    .unwrap();
    assert_eq!(
        admission.admit(batch(vec![packet(0, 0)]), &mut ring, 100_000, 100_001),
        Err(Error::Ring)
    );
    assert!(ring.is_empty());
}
