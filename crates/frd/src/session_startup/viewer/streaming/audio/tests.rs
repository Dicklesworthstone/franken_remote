//! The viewer-side gate in front of the native output. The output here is an
//! explicitly labelled RECORDING TEST DOUBLE: it proves which validated
//! records frd lets through, never codec, device or audibility behavior (the
//! real output is exercised by the namespace e2e and the fr-native tests).
use super::*;
use asupersync::net::quic_native::StreamId;
use fr_core::audio::{AudioChannels, AudioStopReason};
use fr_transport::quic::{DatagramRoute, Messages, Priority, StreamRoute};
use std::{cell::RefCell, rc::Rc};

const BINDING: u32 = 9;

#[derive(Debug, Default)]
struct Calls {
    configured: Vec<AudioConfiguration>,
    received: Vec<Vec<u8>>,
    ended: Vec<AudioEnd>,
    resets: u32,
    fail_receive: bool,
}
struct Recording(Rc<RefCell<Calls>>);
impl AudioOutput for Recording {
    fn configure(&mut self, _: u32, offer: AudioConfiguration) -> Result<(), OutputRefused> {
        self.0.borrow_mut().configured.push(offer);
        Ok(())
    }
    fn service(
        &mut self,
        _: &mut dyn FnMut() -> bool,
        _: &mut dyn FnMut(&[u8]) -> Result<(), OutputRefused>,
    ) -> Result<(), OutputRefused> {
        Ok(())
    }
    fn receive(&mut self, bytes: &[u8], _: &mut dyn FnMut() -> bool) -> Result<(), OutputRefused> {
        let mut calls = self.0.borrow_mut();
        calls.received.push(bytes.to_vec());
        if calls.fail_receive {
            Err(OutputRefused)
        } else {
            Ok(())
        }
    }
    fn ended(&mut self, end: AudioEnd) {
        self.0.borrow_mut().ended.push(end);
    }
    fn reset(&mut self) {
        self.0.borrow_mut().resets += 1;
    }
}
fn stream(id: u64, outbound: bool, messages: Messages) -> StreamRoute {
    StreamRoute {
        stream: StreamId(id),
        binding: BINDING,
        messages,
        priority: Priority::Critical,
        outbound,
        maximum: 1150,
    }
}
fn lanes() -> AudioLanes {
    AudioLanes {
        control: stream(7, false, Messages::AudioControl),
        replies: stream(6, true, Messages::AudioReplies),
        packets: DatagramRoute {
            binding: BINDING,
            kind: 0x0062,
            outbound: false,
        },
        binding: BINDING,
        packet_maximum: 1150,
    }
}
fn owner() -> (ViewerAudio, Rc<RefCell<Calls>>) {
    let calls = Rc::new(RefCell::new(Calls::default()));
    let mut audio = ViewerAudio {
        lanes: lanes(),
        output: None,
        state: State::Idle,
        last: None,
        stop: None,
        statistics: AudioStatistics::default(),
    };
    audio.configure(Box::new(Recording(calls.clone()))).unwrap();
    (audio, calls)
}
fn offer(generation: u64) -> AudioConfiguration {
    AudioConfiguration {
        direction: AudioDirection::Downlink,
        generation: AudioGeneration::from_raw(generation),
        channels: AudioChannels::Stereo,
        sample_rate: 48_000,
        frame_duration_ms: 20,
        max_packet_bytes: 200,
        max_decoded_samples: 960,
        jitter_target_ms: 20,
    }
}
fn configuration(generation: u64) -> Vec<u8> {
    let mut record = vec![0; wire::AUDIO_CONFIGURATION_RECORD_BYTES];
    wire::encode_configuration(&offer(generation), BINDING, &mut record).unwrap();
    record
}
fn packet(generation: u64, sequence: u64, payload: usize) -> Vec<u8> {
    let payload = vec![0x42_u8; payload];
    let mut record = vec![0; wire::AUDIO_PACKET_OVERHEAD + payload.len()];
    let n = wire::encode_packet(
        &wire::AudioPacket {
            direction: AudioDirection::Downlink,
            generation: AudioGeneration::from_raw(generation),
            sequence,
            timestamp_samples: sequence * 960,
            duration_samples: 960,
            payload: &payload,
        },
        BINDING,
        &mut record,
    )
    .unwrap();
    record.truncate(n);
    record
}
fn stop(generation: u64, reason: AudioStopReason) -> Vec<u8> {
    let mut record = vec![0; wire::AUDIO_STOP_RECORD_BYTES];
    wire::encode_stop(
        &wire::AudioStop {
            direction: AudioDirection::Downlink,
            generation: AudioGeneration::from_raw(generation),
            reason,
        },
        BINDING,
        &mut record,
    )
    .unwrap();
    record
}
fn control() -> Route {
    Route::Stream(lanes().control)
}
fn datagram() -> Route {
    Route::Datagram(lanes().packets)
}
fn live() -> impl FnMut() -> bool {
    || true
}

#[test]
fn no_packet_reaches_the_output_before_its_own_acknowledgement() {
    let (mut audio, calls) = owner();
    // Before any configuration.
    audio.receive(datagram(), &packet(1, 0, 40), &mut live());
    audio.receive(control(), &configuration(1), &mut live());
    assert_eq!(calls.borrow().configured, [offer(1)]);
    // Configured but not yet acknowledged: still dropped, never forwarded.
    audio.receive(datagram(), &packet(1, 1, 40), &mut live());
    assert_eq!(calls.borrow().received.len(), 0);
    assert_eq!(audio.statistics().dropped, 2);
    // Only the acknowledgement admitted by the transport opens the lane.
    audio.state = State::Active(offer(1));
    audio.receive(datagram(), &packet(1, 2, 40), &mut live());
    assert_eq!(calls.borrow().received.len(), 1);
    assert_eq!(audio.statistics().packets, 1);
}

#[test]
fn hostile_or_stale_records_are_refused_before_the_output_sees_them() {
    let (mut audio, calls) = owner();
    audio.receive(control(), &configuration(3), &mut live());
    audio.state = State::Active(offer(3));
    for bytes in [
        // Payload above the negotiated 200-byte ceiling (still a valid record).
        packet(3, 0, 201),
        // Stale and future generations.
        packet(2, 1, 40),
        packet(4, 2, 40),
        // Truncated and garbage records.
        packet(3, 3, 40)[..30].to_vec(),
        vec![0xff; 70],
    ] {
        audio.receive(datagram(), &bytes, &mut live());
    }
    assert_eq!(calls.borrow().received.len(), 0);
    assert_eq!(audio.statistics().dropped, 5);
    // A record for another binding never parses as ours.
    let mut foreign = packet(3, 4, 40);
    foreign[16..20].copy_from_slice(&(BINDING + 1).to_be_bytes());
    audio.receive(datagram(), &foreign, &mut live());
    assert_eq!(calls.borrow().received.len(), 0);
    // A stale (older) configuration can never reopen or reconfigure the lane.
    audio.receive(control(), &configuration(2), &mut live());
    assert_eq!(calls.borrow().configured.len(), 1);
    assert_eq!(calls.borrow().ended, [AudioEnd::Local]);
}

#[test]
fn a_host_stop_fences_immediately_and_stale_stops_change_nothing() {
    let (mut audio, calls) = owner();
    audio.receive(control(), &configuration(5), &mut live());
    audio.state = State::Active(offer(5));
    // A stop for another generation is ignored.
    audio.receive(
        control(),
        &stop(4, AudioStopReason::HostDisabled),
        &mut live(),
    );
    assert_eq!(calls.borrow().ended.len(), 0);
    audio.receive(
        control(),
        &stop(5, AudioStopReason::HostDisabled),
        &mut live(),
    );
    assert_eq!(
        calls.borrow().ended,
        [AudioEnd::Host(AudioStopReason::HostDisabled)]
    );
    // The stop itself reached the output (so it fences its own queue) ...
    assert_eq!(calls.borrow().received.len(), 1);
    // ... and nothing queued or late follows it.
    audio.receive(datagram(), &packet(5, 9, 40), &mut live());
    assert_eq!(calls.borrow().received.len(), 1);
    // A source that failed before configuring: typed absence, no output use.
    let (mut idle, idle_calls) = owner();
    idle.receive(
        control(),
        &stop(1, AudioStopReason::HostDisabled),
        &mut live(),
    );
    assert_eq!(
        idle_calls.borrow().ended,
        [AudioEnd::Host(AudioStopReason::HostDisabled)]
    );
    assert_eq!(idle_calls.borrow().configured.len(), 0);
}

#[test]
fn local_failures_reset_with_a_bounded_budget_and_ask_for_a_fresh_epoch() {
    let (mut audio, calls) = owner();
    calls.borrow_mut().fail_receive = true;
    for generation in 1..=MAX_RESTARTS + 1 {
        audio.receive(control(), &configuration(generation), &mut live());
        audio.state = State::Active(offer(generation));
        audio.receive(datagram(), &packet(generation, 0, 40), &mut live());
        // The failed epoch is reported to the host as DeviceChanged.
        assert_eq!(
            audio.stop,
            Some((
                AudioGeneration::from_raw(generation),
                AudioStopReason::DeviceChanged
            ))
        );
        audio.stop = None;
    }
    assert_eq!(audio.statistics().restarts, MAX_RESTARTS);
    assert_eq!(calls.borrow().resets, u32::try_from(MAX_RESTARTS).unwrap());
    // Beyond the budget audio ends for this session, typed.
    assert_eq!(calls.borrow().ended, [AudioEnd::Local]);
    audio.receive(control(), &configuration(MAX_RESTARTS + 2), &mut live());
    assert_eq!(
        calls.borrow().configured.len(),
        usize::try_from(MAX_RESTARTS + 1).unwrap()
    );
}

#[test]
fn a_viewer_without_an_output_refuses_the_configuration_by_type() {
    let mut audio = ViewerAudio {
        lanes: lanes(),
        output: None,
        state: State::Idle,
        last: None,
        stop: None,
        statistics: AudioStatistics::default(),
    };
    audio.receive(control(), &configuration(1), &mut live());
    assert_eq!(
        audio.stop,
        Some((AudioGeneration::from_raw(1), AudioStopReason::UserMute))
    );
    assert_eq!(audio.state, State::Ended);
    // Records on other routes or of other kinds are not owned.
    assert!(!audio.owns(datagram(), &configuration(1)));
    assert!(!audio.owns(control(), &packet(1, 0, 40)));
    assert!(audio.owns(datagram(), &packet(1, 0, 40)));
    assert!(audio.owns(control(), &stop(1, AudioStopReason::UserMute)));
}
