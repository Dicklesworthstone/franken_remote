//! Record-construction regressions; no network, codec or device evidence.
use super::*;
use asupersync::net::quic_native::StreamId;
use fr_core::audio::AudioDirection;
use fr_media::{
    audio::AudioAccessUnit,
    audio_delivery::{MAX_PACKET_AGE_US, control::CONTROL_SEND_US},
};
use fr_transport::quic::{DatagramRoute, Messages, Priority, StreamRoute};

fn lanes() -> AudioLanes {
    let stream = |id, outbound, messages| StreamRoute {
        stream: StreamId(id),
        binding: 9,
        messages,
        priority: Priority::Critical,
        outbound,
        maximum: 1150,
    };
    AudioLanes {
        control: stream(7, true, Messages::AudioControl),
        replies: stream(6, false, Messages::AudioReplies),
        packets: DatagramRoute {
            binding: 9,
            kind: 0x0062,
            outbound: true,
        },
        binding: 9,
        packet_maximum: 1150,
    }
}
fn ring(captured: u64) -> AudioRing {
    let mut ring = AudioRing::new();
    ring.start(SourceStream {
        generation: AudioGeneration::from_raw(7),
        channels: AudioChannels::Stereo,
        frame_duration_ms: 20,
        max_packet_bytes: MAX_PACKET_BYTES,
        jitter_target_ms: 20,
    })
    .unwrap();
    ring.push(
        AudioAccessUnit::new(
            AudioDirection::Downlink,
            AudioGeneration::from_raw(7),
            42,
            960,
            960,
            false,
            &[0x42; 20],
        )
        .unwrap(),
        captured,
    )
    .unwrap();
    ring
}
fn action() -> LaneAction {
    LaneAction::Packet {
        sequence: 42,
        generation: AudioGeneration::from_raw(99),
    }
}

#[test]
fn egress_keeps_capture_deadline_and_maps_only_the_viewer_epoch() {
    let ring = ring(100);
    let until = 100 + MAX_PACKET_AGE_US;
    let lanes = lanes();
    let first = audio_packet(action(), &ring, lanes, 100).unwrap().unwrap();
    let later = audio_packet(action(), &ring, lanes, until - 1)
        .unwrap()
        .unwrap();
    assert_eq!(first, later);
    assert_eq!(later.0, Route::Datagram(lanes.packets));
    assert_eq!(later.2, until);
    let packet = wire::decode_packet(&later.1, lanes.binding).unwrap();
    assert_eq!(packet.generation, AudioGeneration::from_raw(99));
    assert_eq!(packet.sequence, 42);
    assert_eq!(packet.timestamp_samples, 960);
    assert_eq!(packet.payload, &[0x42; 20]);
}

#[test]
fn expired_prepared_actions_never_get_a_second_send_budget() {
    let ring = ring(100);
    for now in [100 + MAX_PACKET_AGE_US, 101 + MAX_PACKET_AGE_US, u64::MAX] {
        assert!(
            audio_packet(action(), &ring, lanes(), now)
                .unwrap()
                .is_none()
        );
    }
    assert_eq!(ring.len(), 1); // Read-only validation, no hidden queue or mutation.
}

#[test]
fn future_or_overflowed_capture_clocks_produce_no_record() {
    assert!(
        audio_packet(action(), &ring(100), lanes(), 99)
            .unwrap()
            .is_none()
    );
    assert!(
        audio_packet(action(), &ring(u64::MAX - 1), lanes(), u64::MAX)
            .unwrap()
            .is_none()
    );
}

#[test]
fn source_stop_is_not_expired_under_a_packet_deadline() {
    let ring = ring(0);
    let stop = wire::AudioStop {
        direction: AudioDirection::Downlink,
        generation: AudioGeneration::from_raw(99),
        reason: AudioStopReason::SessionEnded,
    };
    let now = 10 * MAX_PACKET_AGE_US;
    assert!(
        audio_packet(action(), &ring, lanes(), now)
            .unwrap()
            .is_none()
    );
    let offer = ControlOffer::prepare(LaneAction::Stop(stop), lanes().binding, now).unwrap();
    assert_eq!(offer.deadline_micros(), now + CONTROL_SEND_US);
    assert_eq!(
        wire::decode_stop(offer.bytes(), lanes().binding).unwrap(),
        stop
    );
}

#[test]
fn original_entry_retains_configuration_and_stop_deadlines_through_backpressure() {
    let ring = ring(0);
    let mut entry = EntryAudio::default();
    let configure = entry.lane.next(&ring, 0);
    let binding = lanes().binding;
    entry.prepare_control(configure, binding, 0).unwrap();
    let bytes = entry.control_offer.as_ref().unwrap().bytes().to_vec();
    for now in [1, CONTROL_SEND_US / 2, CONTROL_SEND_US - 1] {
        let retry = entry.lane.next(&ring, now);
        entry.prepare_control(retry, binding, now).unwrap();
        let offer = entry.control_offer.as_ref().unwrap();
        assert_eq!(offer.deadline_micros(), CONTROL_SEND_US);
        assert_eq!(offer.bytes(), bytes);
        assert_eq!(entry.lane.counters().configurations, 0);
    }
    assert!(
        entry
            .prepare_control(configure, binding, CONTROL_SEND_US)
            .is_err()
    );
    entry.close();
    assert!(entry.control_offer.is_none());
    assert!(!entry.lane.wants_audio());

    let mut entry = EntryAudio::default();
    let mut ring = ring;
    entry.lane.next(&ring, 0);
    entry.lane.configuration_sent(0).unwrap();
    ring.end(AudioStopReason::DeviceChanged);
    let stop = entry.lane.next(&ring, 1);
    assert!(matches!(stop, LaneAction::Stop(_)));
    entry.prepare_control(stop, binding, 1).unwrap();
    entry
        .prepare_control(stop, binding, CONTROL_SEND_US)
        .unwrap();
    assert_eq!(
        entry.control_offer.as_ref().unwrap().deadline_micros(),
        1 + CONTROL_SEND_US
    );
    assert!(
        entry
            .prepare_control(stop, binding, 1 + CONTROL_SEND_US)
            .is_err()
    );
    assert_eq!(entry.lane.counters().stops, 0);
}

#[test]
fn actual_lane_supersession_replaces_only_unadmitted_control_bytes() {
    let mut ring = ring(0);
    let mut entry = EntryAudio::default();
    let configure = entry.lane.next(&ring, 0);
    let binding = lanes().binding;
    entry.prepare_control(configure, binding, 0).unwrap();
    ring.end(AudioStopReason::HostDisabled);
    let stop = entry.lane.next(&ring, 10);
    entry.prepare_control(stop, binding, 10).unwrap();
    let offer = entry.control_offer.as_ref().unwrap();
    assert_eq!(offer.deadline_micros(), 10 + CONTROL_SEND_US);
    assert_eq!(
        wire::decode_stop(offer.bytes(), binding).unwrap().reason,
        AudioStopReason::HostDisabled
    );
    assert_eq!(entry.lane.counters().configurations, 0);
    entry.close();
    assert!(entry.control_offer.is_none());
    assert_eq!(entry.lane.next(&ring, 11), LaneAction::Nothing);
}
