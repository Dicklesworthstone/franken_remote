//! The shared/exclusive transport adapter against the real bounded lane owner.
//! These are deterministic record/state tests, not codec or device evidence.
use super::super::{audio_record, wire, EntryAudio};
use super::*;
use asupersync::net::quic_native::StreamId;
use fr_core::audio::{AudioChannels, AudioDirection, AudioGeneration};
use fr_media::{
    audio::AudioAccessUnit,
    audio_delivery::{LaneAction, SourceStream},
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
fn ring() -> AudioRing {
    let mut ring = AudioRing::new();
    ring.start(SourceStream {
        generation: AudioGeneration::from_raw(27),
        channels: AudioChannels::Stereo,
        frame_duration_ms: 20,
        max_packet_bytes: 1000,
        jitter_target_ms: 20,
    })
    .unwrap();
    ring
}
fn endpoint(ring: &AudioRing) -> EntryAudio {
    let mut audio = EntryAudio {
        lanes: Some(lanes()),
        ..EntryAudio::default()
    };
    let LaneAction::Configure(offer) = audio.lane.next(ring, 0) else {
        panic!("configuration must precede packets");
    };
    assert_eq!(offer.generation, AudioGeneration::from_raw(1));
    audio.lane.configuration_sent(0).unwrap();
    audio
}
fn acknowledgement(generation: u64, binding: u32) -> Vec<u8> {
    let mut bytes = vec![0; wire::AUDIO_CONFIGURED_RECORD_BYTES];
    wire::encode_configured(
        &wire::AudioConfigured {
            direction: AudioDirection::Downlink,
            generation: AudioGeneration::from_raw(generation),
            accepted: true,
            actual_channels: AudioChannels::Stereo,
            actual_sample_rate: 48_000,
            actual_frame_duration_ms: 20,
        },
        binding,
        &mut bytes,
    )
    .unwrap();
    bytes
}
fn push(ring: &mut AudioRing, sequence: u64, at: u64) {
    let unit = AudioAccessUnit::new(
        AudioDirection::Downlink,
        AudioGeneration::from_raw(27),
        sequence,
        sequence * 960,
        960,
        false,
        &[0x42; 40],
    )
    .unwrap();
    ring.push(unit, at).unwrap();
}

#[test]
fn only_the_original_reply_route_and_epoch_activate_the_lane() {
    let ring = ring();
    let mut audio = endpoint(&ring);
    assert!(audio
        .receive(&ring, Route::Stream(lanes().control), &acknowledgement(1, 9))
        .is_none());
    assert!(!audio.lane.is_active());
    assert_eq!(
        audio.receive(&ring, Route::Stream(lanes().replies), &acknowledgement(2, 9)),
        Some(Disposition::Consumed)
    );
    assert!(!audio.lane.is_active());
    assert_eq!(audio.lane.counters().stale_refused, 1);
    audio.receive(&ring, Route::Stream(lanes().replies), &acknowledgement(1, 9));
    assert!(audio.lane.is_active());
}

#[test]
fn malformed_or_misbound_replies_fence_only_the_audio_lane() {
    let ring = ring();
    for record in [vec![0xff; 20], acknowledgement(1, 10)] {
        let mut audio = endpoint(&ring);
        assert_eq!(
            audio.receive(&ring, Route::Stream(lanes().replies), &record),
            Some(Disposition::Consumed)
        );
        assert!(audio.lane.is_stopped());
        audio.receive(&ring, Route::Stream(lanes().replies), &acknowledgement(1, 9));
        assert!(audio.lane.is_stopped());
        assert!(matches!(audio.lane.next(&ring, 10), LaneAction::Nothing));
    }
}

#[test]
fn acknowledgement_discards_pre_ack_sound_and_packets_use_the_viewer_epoch() {
    let mut ring = ring();
    let mut audio = endpoint(&ring);
    push(&mut ring, 0, 0);
    assert!(matches!(audio.lane.next(&ring, 1), LaneAction::Nothing));
    audio.receive(&ring, Route::Stream(lanes().replies), &acknowledgement(1, 9));
    assert!(matches!(audio.lane.next(&ring, 2), LaneAction::Nothing));
    push(&mut ring, 1, 20_000);
    let action = audio.lane.next(&ring, 20_000);
    let (route, record, deadline) = audio_record(action, &ring, lanes(), 20_000)
        .unwrap()
        .unwrap();
    assert_eq!(route, Route::Datagram(lanes().packets));
    assert_eq!(deadline, 60_000);
    let packet = wire::decode_packet(&record, 9).unwrap();
    assert_eq!(packet.generation, AudioGeneration::from_raw(1));
    assert_eq!(packet.sequence, 1);
    assert_eq!(packet.payload, &[0x42; 40]);
    assert_eq!(audio.lane.counters().skipped_before_ack, 1);
}

#[test]
fn stale_stop_does_not_mute_a_newer_lane_but_matching_stop_is_terminal() {
    let ring = ring();
    let mut audio = endpoint(&ring);
    audio.receive(&ring, Route::Stream(lanes().replies), &acknowledgement(1, 9));
    for generation in [2, 1] {
        let mut bytes = [0; wire::AUDIO_STOP_RECORD_BYTES];
        wire::encode_stop(
            &wire::AudioStop {
                direction: AudioDirection::Downlink,
                generation: AudioGeneration::from_raw(generation),
                reason: AudioStopReason::UserMute,
            },
            9,
            &mut bytes,
        )
        .unwrap();
        audio.receive(&ring, Route::Stream(lanes().replies), &bytes);
        assert_eq!(audio.lane.is_stopped(), generation == 1);
    }
    assert!(!audio.lane.wants_audio());
}

#[test]
fn source_failure_uses_the_reliable_stop_lane_not_a_video_error() {
    let mut ring = ring();
    let mut audio = endpoint(&ring);
    audio.receive(&ring, Route::Stream(lanes().replies), &acknowledgement(1, 9));
    push(&mut ring, 0, 1);
    ring.end(AudioStopReason::DeviceChanged);
    assert!(ring.is_empty());
    let action = audio.lane.next(&ring, 2);
    let (route, bytes, _) = audio_record(action, &ring, lanes(), 2).unwrap().unwrap();
    assert_eq!(route, Route::Stream(lanes().control));
    let stop = wire::decode_stop(&bytes, 9).unwrap();
    assert_eq!(stop.reason, AudioStopReason::DeviceChanged);
    audio.lane.stop_sent().unwrap();
    assert!(!audio.lane.wants_audio());
}
