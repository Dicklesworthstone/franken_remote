#![cfg(all(target_os = "linux", feature = "linux-pulse-playback"))]
//! Actual FRD0 -> libopus -> selected native output -> independent native monitor.
//! Null-output server evidence, not hardware/remote session or audible latency.
#[allow(dead_code)] // Shared actual-server helper also serves device-only tests.
mod pulse_support;
use fr_core::audio::{
    AudioChannels, AudioDirection, AudioGeneration, AudioStopReason, AudioStreamConfig,
};
use fr_media::audio::{AudioAccessUnit, AudioEncoder, AudioPcmFrame};
use fr_native::{
    opus::{Encoder, playout::ReceiveResult},
    pulse::{
        Error as DeviceError, PlaybackDevice, Selection, State, StopOutcome,
        playout::{Error, PulsePlayout, RenderResult},
    },
};
use fr_wire::audio::{self, AudioConfiguration, AudioPacket, AudioStop};
use pulse_support::Server;
use std::time::{Duration, Instant};
const BINDING: u32 = 31;
fn offer(channels: AudioChannels, duration: u16) -> AudioConfiguration {
    AudioConfiguration {
        direction: AudioDirection::Downlink,
        generation: AudioGeneration::from_raw(11),
        channels,
        sample_rate: 48_000,
        frame_duration_ms: duration,
        max_packet_bytes: 1275,
        max_decoded_samples: u32::from(duration) * 48,
        jitter_target_ms: 20,
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
fn device(server: &Server, offer: AudioConfiguration) -> PlaybackDevice {
    let mut device = PlaybackDevice::connect(
        Selection::new(&server.socket, "fr_test").unwrap(),
        config(offer),
        server.now(),
    )
    .unwrap();
    server.ready(&mut device);
    device
}
fn owner(server: &Server, offer: AudioConfiguration) -> PulsePlayout {
    let mut owner =
        PulsePlayout::new(BINDING, offer, device(server, offer), || Ok(server.now())).unwrap();
    acknowledge(server, &mut owner);
    owner
}
fn acknowledge(server: &Server, owner: &mut PulsePlayout) {
    let offer = owner.configuration();
    owner
        .acknowledge(
            || Ok(server.now()),
            |record| {
                let ack = audio::decode_configured(record, BINDING).unwrap();
                assert!(ack.accepted);
                assert_eq!(ack.generation, offer.generation);
                assert_eq!(ack.actual_sample_rate, offer.sample_rate);
                assert_eq!(ack.actual_channels, offer.channels);
                assert_eq!(ack.actual_frame_duration_ms, offer.frame_duration_ms);
                Ok(())
            },
        )
        .unwrap();
}
fn wire(packet: &AudioAccessUnit) -> Vec<u8> {
    let mut bytes = vec![0; audio::AUDIO_PACKET_OVERHEAD + 1275];
    let n = audio::encode_packet(
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
    bytes.truncate(n);
    bytes
}
fn packets(offer: AudioConfiguration, count: u16) -> Vec<Vec<u8>> {
    let mut encoder = Encoder::new();
    encoder.configure(config(offer)).unwrap();
    let n = usize::from(offer.frame_duration_ms) * 48;
    (0..count)
        .map(|seq| {
            let data: Vec<i16> = (0..n)
                .flat_map(|i| {
                    let sample =
                        (i16::try_from((i + usize::from(seq) * n) % 97).unwrap() - 48) * 200;
                    [sample, -sample]
                        .into_iter()
                        .take(usize::from(offer.channels.count()))
                })
                .collect();
            let pcm = AudioPcmFrame::from_interleaved(
                offer.generation,
                offer.channels,
                1_000_000 + u64::from(seq) * u64::try_from(n).unwrap(),
                &data,
            )
            .unwrap();
            encoder.submit_pcm(&pcm).unwrap();
            wire(&encoder.poll_packet().unwrap().unwrap())
        })
        .collect()
}
fn stop(offer: AudioConfiguration) -> Vec<u8> {
    let mut bytes = vec![0; audio::AUDIO_STOP_RECORD_BYTES];
    audio::encode_stop(
        &AudioStop {
            direction: offer.direction,
            generation: offer.generation,
            reason: AudioStopReason::HostDisabled,
        },
        BINDING,
        &mut bytes,
    )
    .unwrap();
    bytes
}
fn stopped(server: &Server, owner: &mut PulsePlayout) {
    let start = Instant::now();
    while owner.poll_stop(|| Ok(server.now())).unwrap() != State::Closed {
        assert!(start.elapsed() < Duration::from_millis(100));
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(owner.stop_outcome(), Some(StopOutcome::Flushed));
}
#[test]
fn real_opus_and_device_profiles_play_with_loss_and_stop_without_queued_replay() {
    for duration in [10, 20] {
        for channels in [AudioChannels::Mono, AudioChannels::Stereo] {
            let offer = offer(channels, duration);
            let packets = packets(offer, 24); // Real encoder before device creation, never stall its loop.
            let server = Server::start();
            // Configure the independent observer first, then qualify the actual
            // output with the final shared sink scheduling already in place.
            let capture = server.capture(|| {});
            let device = device(&server, offer);
            let mut owner = PulsePlayout::new(BINDING, offer, device, || Ok(server.now())).unwrap();
            acknowledge(&server, &mut owner);
            let first = owner.service(|| Ok(server.now())).unwrap();
            assert_eq!(
                owner
                    .receive_record(&packets[0], || Ok(server.now()))
                    .unwrap(),
                ReceiveResult::Queued
            );
            let mut feed = 1usize;
            let mut rendered = 0usize;
            let mut original_lead = None;
            let start = Instant::now();
            while rendered < 24 {
                let clock = owner.service(|| Ok(server.now())).unwrap();
                if feed < 24
                    && clock.output_samples
                        >= first.output_samples
                            + u64::try_from(feed).unwrap() * u64::from(duration) * 48
                {
                    if feed != 5 {
                        // Deliberate network loss; no fake codec.
                        assert_eq!(
                            owner
                                .receive_record(&packets[feed], || Ok(server.now()))
                                .unwrap(),
                            ReceiveResult::Queued
                        );
                        assert_eq!(
                            owner
                                .receive_record(&packets[feed], || Ok(server.now()))
                                .unwrap(),
                            ReceiveResult::Ignored
                        );
                    }
                    feed += 1;
                }
                if let RenderResult::Submitted(receipt) = owner.render(|| Ok(server.now()))
                    .unwrap_or_else(|error| panic!("duration={duration} channels={channels:?} feed={feed} rendered={rendered}: {error:?}"))
                {
                    assert_eq!(receipt.audio.sequence, u64::try_from(rendered).unwrap());
                    assert_eq!(receipt.audio.concealed, rendered == 5);
                    assert_eq!(
                        receipt.audio.source_samples,
                        1_000_000 + u64::try_from(rendered).unwrap() * u64::from(duration) * 48
                    );
                    let lead = receipt.scheduled_output_sample - receipt.audio.output_samples;
                    assert!((240..=960).contains(&lead));
                    assert_eq!(*original_lead.get_or_insert(lead), lead);
                    rendered += 1;
                }
                assert!(start.elapsed() < Duration::from_secs(2));
                std::thread::sleep(Duration::from_millis(1));
            }
            assert_eq!(
                owner
                    .receive_record(&stop(offer), || Ok(server.now()))
                    .unwrap(),
                ReceiveResult::Stopped(AudioStopReason::HostDisabled)
            );
            assert_eq!(owner.queued_packets(), 0);
            assert_eq!(
                owner.receive_record(&packets[23], || Ok(server.now())),
                Err(Error::Closed)
            );
            assert_eq!(owner.state(), State::Stopping); // Late media does not abort the original flush.
            stopped(&server, &mut owner);
            let actual = capture.finish();
            assert!(
                actual.iter().filter(|s| s.unsigned_abs() > 500).count() > 2000,
                "native monitor received no decoded sound"
            );
            assert!(
                actual[76_800..].iter().all(|s| s.unsigned_abs() < 5),
                "old sound replayed after stop"
            );
        }
    }
}
#[test]
fn real_device_mismatch_and_local_denial_cannot_construct_a_receiver() {
    let server = Server::start();
    let offer = offer(AudioChannels::Stereo, 10);
    let mut wrong = offer;
    wrong.generation = offer.generation.next().unwrap();
    assert!(matches!(
        PulsePlayout::new(BINDING, wrong, device(&server, offer), || Ok(server.now())),
        Err(Error::Configuration)
    ));
    assert!(matches!(
        PulsePlayout::new(BINDING, offer, device(&server, offer), || Err(
            DeviceError::Denied
        )),
        Err(Error::Device(DeviceError::Denied))
    ));
}
#[test]
fn revoked_or_panicking_playout_discards_native_and_codec_queues_when_retained() {
    let offer = offer(AudioChannels::Stereo, 10);
    let packets = packets(offer, 2);
    let server = Server::start();
    for panic in [false, true] {
        let mut owner = owner(&server, offer);
        owner
            .receive_record(&packets[0], || Ok(server.now()))
            .unwrap();
        owner
            .receive_record(&packets[1], || Ok(server.now()))
            .unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            owner.render(|| {
                assert!(!panic, "explicit local checkpoint panic fixture");
                Err(DeviceError::Denied)
            })
        }));
        if panic {
            assert!(result.is_err());
        } else {
            assert_eq!(result.unwrap(), Err(Error::Device(DeviceError::Denied)));
        }
        assert_eq!(owner.queued_packets(), 0);
        assert_eq!(owner.state(), State::Closed);
        assert_eq!(owner.stop_outcome(), Some(StopOutcome::Disconnected));
        assert!(
            owner
                .receive_record(&packets[0], || panic!("retired owner called permission"))
                .is_err()
        );
    }
}
#[test]
fn stale_stop_is_ignored_and_stop_survives_revoked_permission_without_new_audio() {
    let offer = offer(AudioChannels::Stereo, 10);
    let packets = packets(offer, 2);
    let server = Server::start();
    let mut owner = owner(&server, offer);
    owner
        .receive_record(&packets[0], || Ok(server.now()))
        .unwrap();
    let mut stale = offer;
    stale.generation = AudioGeneration::INITIAL;
    assert_eq!(
        owner
            .receive_record(&stop(stale), || Ok(server.now()))
            .unwrap(),
        ReceiveResult::Ignored
    );
    assert_eq!(owner.queued_packets(), 1);
    assert_eq!(
        owner.receive_record(&stop(offer), || Err(DeviceError::Denied)),
        Err(Error::Device(DeviceError::Denied))
    );
    assert_eq!(owner.queued_packets(), 0);
    assert_eq!(owner.state(), State::Closed);
    assert_eq!(owner.stop_outcome(), Some(StopOutcome::Disconnected));
}
#[test]
fn malformed_opus_retires_both_owners_without_native_output() {
    let offer = offer(AudioChannels::Stereo, 10);
    let server = Server::start();
    let mut owner = owner(&server, offer);
    for seq in 0..2 {
        let packet = AudioAccessUnit::new(
            offer.direction,
            offer.generation,
            seq,
            seq * 480,
            480,
            false,
            &[0xff],
        )
        .unwrap();
        owner
            .receive_record(&wire(&packet), || Ok(server.now()))
            .unwrap();
    }
    let start = Instant::now();
    loop {
        match owner.render(|| Ok(server.now())) {
            Ok(RenderResult::Waiting) => (),
            Err(Error::Receiver(_)) => break,
            other => panic!("unexpected malformed native result {other:?}"),
        }
        assert!(start.elapsed() < Duration::from_millis(100));
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(owner.state(), State::Closed);
    assert_eq!(owner.queued_packets(), 0);
}

#[test]
fn configured_ack_is_truthful_one_use_and_unknown_send_is_terminal() {
    let offer = offer(AudioChannels::Stereo, 10);
    let server = Server::start();
    let mut prepared =
        PulsePlayout::new(BINDING, offer, device(&server, offer), || Ok(server.now())).unwrap();
    assert_eq!(
        prepared.render(|| panic!("decode before acknowledgement")),
        Err(Error::NotAcknowledged)
    );
    acknowledge(&server, &mut prepared);
    assert_eq!(
        prepared.acknowledge(
            || panic!("second acknowledgement"),
            |_| panic!("second send")
        ),
        Err(Error::AlreadyAcknowledged)
    );
    prepared.stop(server.now()).unwrap();
    stopped(&server, &mut prepared);
    for panic in [false, true] {
        let mut candidate =
            PulsePlayout::new(BINDING, offer, device(&server, offer), || Ok(server.now())).unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            candidate.acknowledge(
                || Ok(server.now()),
                |_| {
                    assert!(!panic, "explicit acknowledgement callback panic fixture");
                    Err(())
                },
            )
        }));
        if panic {
            assert!(result.is_err());
        } else {
            assert_eq!(result.unwrap(), Err(Error::AcknowledgementUnknown));
        }
        assert_eq!(candidate.state(), State::Closed);
        assert_eq!(candidate.queued_packets(), 0);
    }
    let packets = packets(offer, 1);
    let mut early =
        PulsePlayout::new(BINDING, offer, device(&server, offer), || Ok(server.now())).unwrap();
    assert_eq!(
        early.receive_record(&packets[0], || panic!("early packet serviced native work")),
        Err(Error::NotAcknowledged)
    );
    assert_eq!(early.state(), State::Closed);
    assert_eq!(early.queued_packets(), 0);
}
