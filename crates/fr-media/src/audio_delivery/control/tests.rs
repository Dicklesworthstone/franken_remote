use super::*;
use crate::audio_delivery::{AudioLane, AudioRing, SourceStream};
use fr_core::audio::{AudioChannels, AudioDirection, AudioGeneration, AudioStopReason};

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

#[test]
fn backpressured_configuration_keeps_the_first_record_and_deadline() {
    let ring = ring();
    let mut lane = AudioLane::new();
    let action = lane.next(&ring, 100);
    let mut offer = ControlOffer::prepare(action, 9, 100).unwrap();
    let bytes = offer.bytes().to_vec();
    for now in [101, 10_000, 1_000_000, 100 + CONTROL_SEND_US - 1] {
        let retry = lane.next(&ring, now);
        assert!(offer.matches(retry, 9));
        offer.authorize(retry, 9, now).unwrap();
        assert_eq!(offer.deadline_micros(), 100 + CONTROL_SEND_US);
        assert_eq!(offer.bytes(), bytes);
        assert!(!lane.is_active());
        assert_eq!(lane.counters().configurations, 0);
    }
    assert_eq!(
        offer.authorize(action, 9, 100 + CONTROL_SEND_US),
        Err(Error::Expired)
    );
    assert_eq!(offer.authorize(action, 9, 101), Err(Error::Expired));
    // Expiry belongs to this optional lane, not its source or sibling viewers.
    lane.close();
    assert!(lane.is_stopped());
    let mut sibling = AudioLane::new();
    assert!(matches!(
        sibling.next(&ring, 2_000_101),
        LaneAction::Configure(_)
    ));
}

#[test]
fn backpressured_stop_is_bounded_and_cannot_reconfigure_after_timeout() {
    let mut ring = ring();
    let mut lane = AudioLane::new();
    lane.next(&ring, 0);
    lane.configuration_sent(0).unwrap();
    ring.end(AudioStopReason::DeviceChanged);
    let action = lane.next(&ring, 1);
    let mut offer = ControlOffer::prepare(action, 9, 1).unwrap();
    let stop = wire::decode_stop(offer.bytes(), 9).unwrap();
    assert_eq!(stop.reason, AudioStopReason::DeviceChanged);
    assert_eq!(stop.generation, AudioGeneration::from_raw(1));
    for now in [2, 1_000_000, CONTROL_SEND_US] {
        let retry = lane.next(&ring, now);
        offer.authorize(retry, 9, now).unwrap();
        assert_eq!(offer.deadline_micros(), 1 + CONTROL_SEND_US);
        assert_eq!(lane.counters().stops, 0);
    }
    assert_eq!(
        offer.authorize(action, 9, 1 + CONTROL_SEND_US),
        Err(Error::Expired)
    );
    lane.close();
    assert_eq!(lane.next(&ring, 2 + CONTROL_SEND_US), LaneAction::Nothing);
    assert!(!lane.wants_audio());
}

#[test]
fn offer_identity_includes_binding_epoch_configuration_and_stop_reason() {
    let ring = ring();
    let mut lane = AudioLane::new();
    let action = lane.next(&ring, 0);
    let LaneAction::Configure(configuration) = action else {
        panic!("configuration");
    };
    let mut offer = ControlOffer::prepare(action, 9, 0).unwrap();
    let mut newer = configuration;
    newer.generation = AudioGeneration::from_raw(2);
    let mut changed = configuration;
    changed.channels = AudioChannels::Mono;
    for (foreign, binding) in [
        (action, 10),
        (LaneAction::Configure(newer), 9),
        (LaneAction::Configure(changed), 9),
    ] {
        assert_eq!(offer.authorize(foreign, binding, 1), Err(Error::StaleOffer));
    }
    offer.authorize(action, 9, 1).unwrap();
    assert_eq!(
        wire::decode_configuration(offer.bytes(), 9).unwrap(),
        configuration
    );
    let stop = wire::AudioStop {
        direction: AudioDirection::Downlink,
        generation: AudioGeneration::from_raw(1),
        reason: AudioStopReason::SessionEnded,
    };
    let stop_offer = ControlOffer::prepare(LaneAction::Stop(stop), 9, 1).unwrap();
    assert!(!stop_offer.matches(
        LaneAction::Stop(wire::AudioStop {
            reason: AudioStopReason::DeviceChanged,
            ..stop
        }),
        9
    ));
    assert!(!offer.matches(LaneAction::Stop(stop), 9));
}

#[test]
fn clock_failure_is_terminal_and_overflow_never_saturates_a_new_budget() {
    let mut lane = AudioLane::new();
    let action = lane.next(&ring(), 10);
    let mut offer = ControlOffer::prepare(action, 9, 10).unwrap();
    offer.authorize(action, 9, 20).unwrap();
    assert_eq!(offer.authorize(action, 9, 19), Err(Error::ClockRegression));
    assert_eq!(offer.authorize(action, 9, 21), Err(Error::ClockRegression));
    assert!(matches!(
        ControlOffer::prepare(action, 9, u64::MAX),
        Err(Error::ClockOverflow)
    ));
    let mut edge = ControlOffer::prepare(action, 9, u64::MAX - CONTROL_SEND_US).unwrap();
    edge.authorize(action, 9, u64::MAX - 1).unwrap();
    assert_eq!(edge.authorize(action, 9, u64::MAX), Err(Error::Expired));
}

#[test]
fn packets_and_invalid_control_records_never_get_fresh_control_lifetimes() {
    for action in [
        LaneAction::Nothing,
        LaneAction::Packet {
            sequence: 1,
            generation: AudioGeneration::from_raw(1),
        },
    ] {
        assert!(matches!(
            ControlOffer::prepare(action, 9, 0),
            Err(Error::WrongAction)
        ));
    }
    let mut lane = AudioLane::new();
    let LaneAction::Configure(mut configuration) = lane.next(&ring(), 0) else {
        panic!("configuration");
    };
    configuration.sample_rate = 44_100;
    assert!(matches!(
        ControlOffer::prepare(LaneAction::Configure(configuration), 9, 0),
        Err(Error::InvalidRecord)
    ));
}
