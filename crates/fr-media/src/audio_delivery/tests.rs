use super::*;

fn stream(generation: u64) -> SourceStream {
    SourceStream {
        generation: AudioGeneration::from_raw(generation),
        channels: AudioChannels::Stereo,
        frame_duration_ms: 20,
        max_packet_bytes: 1000,
        jitter_target_ms: 20,
    }
}
fn unit(generation: u64, sequence: u64, len: usize) -> AudioAccessUnit {
    AudioAccessUnit::new(
        AudioDirection::Downlink,
        AudioGeneration::from_raw(generation),
        sequence,
        sequence * 960,
        960,
        false,
        &vec![7; len],
    )
    .unwrap()
}
fn ack(epoch: u64) -> AudioConfigured {
    AudioConfigured {
        direction: AudioDirection::Downlink,
        generation: AudioGeneration::from_raw(epoch),
        accepted: true,
        actual_channels: AudioChannels::Stereo,
        actual_sample_rate: 48_000,
        actual_frame_duration_ms: 20,
    }
}
fn stop(epoch: u64, reason: AudioStopReason) -> AudioStop {
    AudioStop {
        direction: AudioDirection::Downlink,
        generation: AudioGeneration::from_raw(epoch),
        reason,
    }
}
fn packet(sequence: u64, epoch: u64) -> LaneAction {
    LaneAction::Packet {
        sequence,
        generation: AudioGeneration::from_raw(epoch),
    }
}
/// The configuration the lane announces: the source profile under the
/// lane's own epoch.
fn configuration(source: u64, epoch: u64) -> LaneAction {
    LaneAction::Configure(AudioConfiguration {
        generation: AudioGeneration::from_raw(epoch),
        ..stream(source).configuration()
    })
}
/// Drive one lane to Active on its next epoch at time `now`.
fn active(ring: &AudioRing, lane: &mut AudioLane, source: u64, epoch: u64, now: u64) {
    assert_eq!(lane.next(ring, now), configuration(source, epoch));
    lane.configuration_sent(now).unwrap();
    lane.configured(ack(epoch), ring, now).unwrap();
}

#[test]
fn no_packet_leaves_before_the_matching_acknowledgement() {
    let mut ring = AudioRing::new();
    let mut lane = AudioLane::new();
    // Idle source: nothing, not even a configuration.
    assert_eq!(lane.next(&ring, 0), LaneAction::Nothing);
    ring.start(stream(1)).unwrap();
    for s in 0..3 {
        ring.push(unit(1, s, 100), 10).unwrap();
    }
    // Only the configuration, repeatedly, until the transport admits it.
    for _ in 0..2 {
        assert_eq!(lane.next(&ring, 10), configuration(1, 1));
    }
    // No packet may be sent before acknowledgement.
    assert_eq!(lane.packet_sent(0), Err(Error::WrongState));
    lane.configuration_sent(10).unwrap();
    for s in 3..6 {
        ring.push(unit(1, s, 100), 20).unwrap();
        assert_eq!(lane.next(&ring, 20), LaneAction::Nothing);
    }
    lane.configured(ack(1), &ring, 20).unwrap();
    // Everything captured before the acknowledgement is skipped, not replayed.
    assert_eq!(lane.next(&ring, 30), LaneAction::Nothing);
    assert_eq!(lane.counters().skipped_before_ack, 6);
    ring.push(unit(1, 6, 100), 30).unwrap();
    assert_eq!(lane.next(&ring, 30), packet(6, 1));
    lane.packet_sent(6).unwrap();
    assert_eq!(lane.next(&ring, 30), LaneAction::Nothing);
    assert_eq!(lane.counters().sent, 1);
    // A duplicate acknowledgement never re-arms or rewinds the lane.
    assert_eq!(lane.configured(ack(1), &ring, 30), Err(Error::WrongState));
}

#[test]
fn audio_stop_fences_every_queued_packet() {
    let mut ring = AudioRing::new();
    ring.start(stream(1)).unwrap();
    let mut viewer_stops = AudioLane::new();
    let mut source_ends = AudioLane::new();
    active(&ring, &mut viewer_stops, 1, 1, 0);
    active(&ring, &mut source_ends, 1, 1, 0);
    for s in 0..4 {
        ring.push(unit(1, s, 100), 5).unwrap();
    }
    assert_eq!(viewer_stops.next(&ring, 5), packet(0, 1));
    // The viewer's stop fences the lane: none of the queued packets follow.
    assert_eq!(
        viewer_stops.viewer_stopped(stop(9, AudioStopReason::UserMute)),
        Err(Error::StaleGeneration)
    );
    viewer_stops
        .viewer_stopped(stop(1, AudioStopReason::UserMute))
        .unwrap();
    assert_eq!(viewer_stops.next(&ring, 5), LaneAction::Nothing);
    assert_eq!(viewer_stops.packet_sent(0), Err(Error::WrongState));
    assert!(!viewer_stops.wants_audio());
    // Ending the source discards the ring at once; the lane sends AudioStop
    // before anything else and then nothing.
    ring.end(AudioStopReason::DeviceChanged);
    assert!(ring.is_empty() && ring.bytes() == 0);
    assert_eq!(
        source_ends.next(&ring, 5),
        LaneAction::Stop(stop(1, AudioStopReason::DeviceChanged))
    );
    assert_eq!(ring.push(unit(1, 4, 100), 5), Err(Error::WrongState));
    source_ends.stop_sent().unwrap();
    assert_eq!(source_ends.next(&ring, 5), LaneAction::Nothing);
    assert!(source_ends.is_stopped());
    assert_eq!(source_ends.counters().stops, 1);
}

#[test]
fn bounded_ring_and_lanes_drop_obsolete_audio_with_counts() {
    let mut ring = AudioRing::new();
    ring.start(stream(1)).unwrap();
    let mut slow = AudioLane::new();
    active(&ring, &mut slow, 1, 1, 0);
    // The count bound: twelve packets leave the newest eight.
    for s in 0..12 {
        ring.push(unit(1, s, 10), 1_000).unwrap();
    }
    assert_eq!(ring.len(), RING_PACKETS);
    assert_eq!(ring.counters().evicted, 4);
    // The lane resumes at the oldest retained packet and counts what it lost.
    assert_eq!(slow.next(&ring, 1_000), packet(4, 1));
    assert_eq!(slow.counters().dropped_evicted, 4);
    slow.packet_sent(4).unwrap();
    // Everything older than the age bound is skipped and counted, not sent.
    ring.push(unit(1, 12, 10), 1_000 + MAX_PACKET_AGE_US + 5)
        .unwrap();
    assert_eq!(
        slow.next(&ring, 1_000 + MAX_PACKET_AGE_US + 5),
        packet(12, 1)
    );
    assert_eq!(slow.counters().dropped_obsolete, 7);
    // The byte bound applies independently of the count bound.
    let mut bytes = AudioRing::new();
    bytes.start(stream(1)).unwrap();
    for s in 0..6 {
        bytes.push(unit(1, s, 1000), 0).unwrap();
    }
    assert!(bytes.bytes() <= RING_BYTES);
    assert_eq!(bytes.len(), RING_BYTES / 1000);
    assert_eq!(bytes.counters().evicted, 6 - (RING_BYTES / 1000) as u64);
    // Oversized, reordered or wrong-duration packets are refused and counted.
    assert_eq!(bytes.push(unit(1, 6, 1001), 0), Err(Error::Invalid));
    assert_eq!(bytes.push(unit(1, 5, 10), 0), Err(Error::Invalid));
    let short = AudioAccessUnit::new(
        AudioDirection::Downlink,
        AudioGeneration::from_raw(1),
        9,
        0,
        480,
        false,
        &[1],
    )
    .unwrap();
    assert_eq!(bytes.push(short, 0), Err(Error::Invalid));
    assert_eq!(bytes.counters().refused, 3);
}

#[test]
fn stale_generations_are_refused_at_the_ring_and_the_lane() {
    let mut ring = AudioRing::new();
    ring.start(stream(2)).unwrap();
    // A source generation that is not strictly newer never starts.
    ring.idle();
    assert_eq!(ring.start(stream(2)), Err(Error::StaleGeneration));
    assert_eq!(ring.start(stream(1)), Err(Error::StaleGeneration));
    ring.start(stream(3)).unwrap();
    // Packets of another source generation are refused.
    assert_eq!(ring.push(unit(2, 0, 10), 0), Err(Error::StaleGeneration));
    let mut lane = AudioLane::new();
    assert_eq!(lane.next(&ring, 0), configuration(3, 1));
    lane.configuration_sent(0).unwrap();
    // An acknowledgement naming another epoch changes nothing.
    assert_eq!(
        lane.configured(ack(2), &ring, 0),
        Err(Error::StaleGeneration)
    );
    assert_eq!(lane.counters().stale_refused, 1);
    ring.push(unit(3, 0, 10), 0).unwrap();
    assert_eq!(lane.next(&ring, 0), LaneAction::Nothing);
    lane.configured(ack(1), &ring, 0).unwrap();
    // A restarted source: the old epoch is stopped first, then a NEW epoch is
    // configured, and the lane waits for its new acknowledgement.
    ring.end(AudioStopReason::DeviceChanged);
    ring.idle();
    ring.start(stream(4)).unwrap();
    ring.push(unit(4, 0, 10), 0).unwrap();
    assert_eq!(
        lane.next(&ring, 0),
        LaneAction::Stop(stop(1, AudioStopReason::DeviceChanged))
    );
    lane.stop_sent().unwrap();
    assert_eq!(lane.next(&ring, 0), configuration(4, 2));
    lane.configuration_sent(0).unwrap();
    // The old epoch's acknowledgement or stop can never address the new one.
    assert_eq!(
        lane.configured(ack(1), &ring, 0),
        Err(Error::StaleGeneration)
    );
    assert_eq!(
        lane.viewer_stopped(stop(1, AudioStopReason::UserMute)),
        Err(Error::StaleGeneration)
    );
    assert_eq!(lane.next(&ring, 0), LaneAction::Nothing);
    lane.configured(ack(2), &ring, 0).unwrap();
    ring.push(unit(4, 1, 10), 0).unwrap();
    assert_eq!(lane.next(&ring, 0), packet(1, 2));
}

#[test]
fn a_viewer_output_reset_gets_bounded_fresh_epochs() {
    let mut ring = AudioRing::new();
    ring.start(stream(1)).unwrap();
    let mut lane = AudioLane::new();
    let mut epoch = 1;
    active(&ring, &mut lane, 1, epoch, 0);
    for _ in 0..MAX_LANE_RESTARTS {
        lane.viewer_stopped(stop(epoch, AudioStopReason::DeviceChanged))
            .unwrap();
        // Nothing of the reset epoch follows; the next epoch is strictly newer.
        ring.push(unit(1, epoch, 10), 0).unwrap();
        epoch += 1;
        active(&ring, &mut lane, 1, epoch, 0);
    }
    assert_eq!(lane.counters().restarts, MAX_LANE_RESTARTS);
    // The bound is terminal: another reset ends the lane.
    lane.viewer_stopped(stop(epoch, AudioStopReason::DeviceChanged))
        .unwrap();
    assert!(lane.is_stopped());
    assert_eq!(lane.next(&ring, 0), LaneAction::Nothing);
}

#[test]
fn a_generation_that_played_for_a_while_earns_the_restart_budget_back() {
    let mut ring = AudioRing::new();
    ring.start(stream(1)).unwrap();
    let mut lane = AudioLane::new();
    let mut epoch = 1;
    let mut sequence = 0;
    active(&ring, &mut lane, 1, epoch, 0);
    for reset in 0..3 * MAX_LANE_RESTARTS {
        // Every eighth generation plays long enough to prove the output works.
        let played = if reset % MAX_LANE_RESTARTS == 0 {
            HEALTHY_EPOCH_PACKETS
        } else {
            1
        };
        for _ in 0..played {
            ring.push(unit(1, sequence, 10), 0).unwrap();
            assert_eq!(lane.next(&ring, 0), packet(sequence, epoch));
            lane.packet_sent(sequence).unwrap();
            sequence += 1;
        }
        lane.viewer_stopped(stop(epoch, AudioStopReason::DeviceChanged))
            .unwrap();
        assert!(!lane.is_stopped(), "reset {reset} ended a working lane");
        epoch += 1;
        active(&ring, &mut lane, 1, epoch, 0);
    }
    assert_eq!(lane.counters().restarts, 3 * MAX_LANE_RESTARTS);
}

#[test]
fn an_acknowledgement_before_the_next_send_turn_cannot_bypass_expiry() {
    for extra_us in [0, 1] {
        let mut ring = AudioRing::new();
        ring.start(stream(1)).unwrap();
        let mut lane = AudioLane::new();
        assert_eq!(lane.next(&ring, 100), configuration(1, 1));
        lane.configuration_sent(100).unwrap();
        let now = 100 + CONFIGURE_TIMEOUT_US + extra_us;

        // Receive dispatch may run before the sender gets another turn. The
        // original deadline must still win without an intervening next().
        assert_eq!(lane.configured(ack(1), &ring, now), Err(Error::WrongState));
        assert_eq!(
            lane.next(&ring, now),
            LaneAction::Stop(stop(1, AudioStopReason::SessionEnded))
        );
        assert!(!lane.is_active());
        assert!(!lane.wants_audio());
        assert_eq!(lane.counters().sent, 0);
        lane.stop_sent().unwrap();
        assert!(lane.is_stopped());
        assert_eq!(lane.configured(ack(1), &ring, now), Err(Error::WrongState));
        assert!(lane.is_stopped());
    }
}

#[test]
fn an_acknowledgement_just_before_expiry_admits_only_future_audio() {
    let mut ring = AudioRing::new();
    ring.start(stream(1)).unwrap();
    let mut lane = AudioLane::new();
    assert_eq!(lane.next(&ring, 100), configuration(1, 1));
    lane.configuration_sent(100).unwrap();
    let now = 100 + CONFIGURE_TIMEOUT_US - 1;
    ring.push(unit(1, 0, 10), now).unwrap();

    lane.configured(ack(1), &ring, now).unwrap();
    assert!(lane.is_active());
    assert_eq!(lane.counters().skipped_before_ack, 1);
    assert_eq!(lane.next(&ring, now), LaneAction::Nothing);
    ring.push(unit(1, 1, 10), now + 1).unwrap();
    assert_eq!(lane.next(&ring, now + 1), packet(1, 1));
}

#[test]
fn an_old_acknowledgement_does_not_expire_a_newer_configuration() {
    let mut ring = AudioRing::new();
    ring.start(stream(1)).unwrap();
    let mut lane = AudioLane::new();
    assert_eq!(lane.next(&ring, 100), configuration(1, 1));
    lane.configuration_sent(100).unwrap();
    lane.viewer_stopped(stop(1, AudioStopReason::DeviceChanged))
        .unwrap();
    let now = 100 + CONFIGURE_TIMEOUT_US;
    assert_eq!(lane.next(&ring, now), configuration(1, 2));
    lane.configuration_sent(now).unwrap();

    assert_eq!(
        lane.configured(ack(1), &ring, now + 1),
        Err(Error::StaleGeneration)
    );
    assert_eq!(lane.next(&ring, now + 1), LaneAction::Nothing);
    assert!(lane.wants_audio());
    lane.configured(ack(2), &ring, now + 2).unwrap();
    assert!(lane.is_active());
}

#[test]
fn expired_acknowledgement_stops_only_its_lane_not_the_shared_source() {
    let mut ring = AudioRing::new();
    ring.start(stream(1)).unwrap();
    let mut late = AudioLane::new();
    let mut healthy = AudioLane::new();
    assert_eq!(late.next(&ring, 100), configuration(1, 1));
    late.configuration_sent(100).unwrap();
    active(&ring, &mut healthy, 1, 1, 100);
    let now = 100 + CONFIGURE_TIMEOUT_US;

    assert_eq!(late.configured(ack(1), &ring, now), Err(Error::WrongState));
    ring.push(unit(1, 0, 10), now).unwrap();
    assert_eq!(
        late.next(&ring, now),
        LaneAction::Stop(stop(1, AudioStopReason::SessionEnded))
    );
    assert_eq!(healthy.next(&ring, now), packet(0, 1));
    assert_eq!(ring.state(), SourceState::Live(stream(1)));
    assert_eq!(ring.len(), 1);
    assert!(healthy.is_active());
}

#[test]
fn unacknowledged_configuration_expires_and_failed_sources_are_a_typed_stop() {
    let mut ring = AudioRing::new();
    ring.start(stream(1)).unwrap();
    let mut lane = AudioLane::new();
    assert!(matches!(lane.next(&ring, 0), LaneAction::Configure(_)));
    lane.configuration_sent(100).unwrap();
    assert_eq!(
        lane.next(&ring, 100 + CONFIGURE_TIMEOUT_US - 1),
        LaneAction::Nothing
    );
    assert_eq!(
        lane.next(&ring, 100 + CONFIGURE_TIMEOUT_US),
        LaneAction::Stop(stop(1, AudioStopReason::SessionEnded))
    );
    lane.stop_sent().unwrap();
    // A late acknowledgement cannot resurrect it.
    assert!(
        lane.configured(ack(1), &ring, 100 + CONFIGURE_TIMEOUT_US)
            .is_err()
    );
    assert_eq!(lane.next(&ring, u64::MAX), LaneAction::Nothing);
    // A source that never became live (missing monitor) is a typed stop for
    // every waiting viewer, and its generation is consumed.
    let mut failed = AudioRing::new();
    failed.fail(stream(5), AudioStopReason::HostDisabled);
    assert_eq!(failed.start(stream(5)), Err(Error::StaleGeneration));
    let mut after_failure = AudioLane::new();
    assert_eq!(
        after_failure.next(&failed, 0),
        LaneAction::Stop(stop(1, AudioStopReason::HostDisabled))
    );
    after_failure.stop_sent().unwrap();
    assert!(after_failure.is_stopped());
    // An explicit viewer refusal ends the lane without packets.
    let mut refused = AudioLane::new();
    assert!(matches!(refused.next(&ring, 0), LaneAction::Configure(_)));
    refused.configuration_sent(0).unwrap();
    refused
        .configured(
            AudioConfigured {
                accepted: false,
                ..ack(1)
            },
            &ring,
            0,
        )
        .unwrap();
    assert!(refused.is_stopped());
}
