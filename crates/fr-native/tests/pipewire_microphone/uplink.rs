//! Real encoded records, confined Opus child, demanded virtual source and a
//! separate recorder. Synthetic signal and packet arrival cadence are fixtures;
//! the decoder, `PipeWire` graph, output recording and child reaping are genuine.
use super::{Microphone, OwnedChild, Server, State, ready};
use fr_core::audio::{
    AudioChannels, AudioDirection, AudioGeneration, AudioStopReason, AudioStreamConfig,
};
use fr_media::audio::{AudioAccessUnit, AudioEncoder, AudioPcmFrame};
use fr_native::{
    opus::{Encoder, process::Retirement},
    pipewire_mic::uplink::{Error, ReceiveResult, RenderResult, Uplink},
};
use fr_wire::{
    WireError,
    audio::{self, AudioConfiguration, AudioPacket, AudioStop},
};
use std::{
    fs,
    path::Path,
    process::Command,
    time::{Duration, Instant},
};

const BINDING: u32 = 73;

fn config(generation: u64, duration: u16) -> AudioStreamConfig {
    AudioStreamConfig::new(
        AudioDirection::Uplink,
        AudioGeneration::from_raw(generation),
        AudioChannels::Mono,
        duration,
        20,
    )
    .unwrap()
}
fn offer(config: AudioStreamConfig) -> AudioConfiguration {
    AudioConfiguration {
        direction: config.direction(),
        generation: config.generation(),
        channels: config.channels(),
        sample_rate: config.sample_rate(),
        frame_duration_ms: config.frame_duration_ms(),
        max_packet_bytes: 1275,
        max_decoded_samples: config.expected_samples_per_frame(),
        jitter_target_ms: config.jitter_target_ms(),
    }
}
fn owner(server: &Server, config: AudioStreamConfig) -> Uplink {
    let mut device = Microphone::connect(&server.socket, config, || true).unwrap();
    ready(&mut device);
    let mut owner = Uplink::new(
        BINDING,
        offer(config),
        device,
        Path::new(env!("CARGO_BIN_EXE_fr-opus-worker")),
        || true,
    )
    .unwrap();
    let until = Instant::now() + Duration::from_secs(3);
    while !owner.poll_ready(|| true).unwrap() {
        assert!(
            Instant::now() < until,
            "actual child configuration deadline"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(owner.decoder_retirement().process_id().is_some());
    owner
}
fn acknowledge(owner: &mut Uplink, config: AudioStreamConfig) {
    let mut sends = 0;
    owner
        .acknowledge(
            || true,
            |bytes| {
                let actual = audio::decode_configured(bytes, BINDING).unwrap();
                assert!(actual.accepted);
                assert_eq!(actual.direction, AudioDirection::Uplink);
                assert_eq!(actual.generation, config.generation());
                assert_eq!(actual.actual_channels, config.channels());
                assert_eq!(actual.actual_sample_rate, 48_000);
                assert_eq!(actual.actual_frame_duration_ms, config.frame_duration_ms());
                sends += 1;
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(sends, 1);
}

#[allow(clippy::cast_possible_truncation)]
fn fixtures(config: AudioStreamConfig, sound: u16, silence: u16) -> Vec<AudioAccessUnit> {
    let samples = usize::try_from(config.expected_samples_per_frame()).unwrap();
    let mut encoder = Encoder::new();
    encoder.configure(config).unwrap();
    (0..sound + silence)
        .map(|sequence| {
            let values: Vec<i16> = (0..samples)
                .map(|index| {
                    if sequence < sound {
                        (9000.0
                            * (2.0
                                * std::f64::consts::PI
                                * f64::from(u16::try_from(index % 96).unwrap())
                                / 96.0)
                                .sin())
                        .round() as i16
                    } else {
                        0
                    }
                })
                .collect();
            let timestamp =
                1_000_000 + u64::from(sequence) * u64::from(config.expected_samples_per_frame());
            let pcm = AudioPcmFrame::from_interleaved(
                config.generation(),
                config.channels(),
                timestamp,
                &values,
            )
            .unwrap();
            encoder.submit_pcm(&pcm).unwrap();
            encoder.poll_packet().unwrap().unwrap()
        })
        .collect()
}
fn record(packet: &AudioAccessUnit, binding: u32) -> Vec<u8> {
    let mut bytes = vec![0; audio::AUDIO_PACKET_OVERHEAD + packet.payload().len()];
    let count = audio::encode_packet(
        &AudioPacket {
            direction: packet.direction(),
            generation: packet.generation(),
            sequence: packet.sequence(),
            timestamp_samples: packet.timestamp_samples(),
            duration_samples: packet.duration_samples(),
            payload: packet.payload(),
        },
        binding,
        &mut bytes,
    )
    .unwrap();
    bytes.truncate(count);
    bytes
}
fn reaped(retirement: &Retirement) {
    let until = Instant::now() + Duration::from_secs(3);
    while !retirement.is_complete() {
        assert!(Instant::now() < until, "original child must be reaped");
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(retirement.process_id(), None);
}
fn closed(owner: &mut Uplink) {
    let retirement = owner.decoder_retirement();
    owner.stop();
    assert!(owner.control().is_stopped());
    assert_eq!(owner.state(), State::Closed);
    assert_eq!(owner.queued_packets(), 0);
    reaped(&retirement);
}
fn link(server: &Server, owner: &mut Uplink, mode: &str) -> OwnedChild {
    let mut capture = server.recorder_mode(owner.statistics().node_id, mode);
    let until = Instant::now() + Duration::from_secs(10);
    while !server.directory.join("linked").exists() {
        owner.service(|| true).unwrap();
        assert!(
            capture.0.try_wait().unwrap().is_none(),
            "see capture.stderr"
        );
        assert!(
            Instant::now() < until,
            "independent consumer setup deadline"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    // Linking precedes the first real process callback; keep the source silent
    // while actual consumer demand begins. No packet or clock is fabricated.
    let began = Instant::now();
    while began.elapsed() < Duration::from_millis(50) {
        owner.service(|| true).unwrap();
        std::thread::sleep(Duration::from_millis(1));
    }
    capture
}
fn finish_capture(server: &Server, capture: &mut OwnedChild) {
    fs::write(server.directory.join("finished"), []).unwrap();
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = capture.0.try_wait().unwrap() {
            assert!(status.success(), "see retained capture.stderr");
            break;
        }
        assert!(
            Instant::now() < until,
            "independent capture completion deadline"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    let evidence = fs::read_to_string(server.directory.join("capture.stdout")).unwrap();
    assert!(evidence.contains("independent_capture_passed"));
    println!(
        "{}\nartifacts: {}",
        evidence.trim(),
        server.directory.display()
    );
}

#[test]
#[ignore = "private PipeWire/pw-cat tools + real confined Opus worker; explicit native CI"]
fn real_10ms_uplink_reaches_an_independent_consumer_and_stops() {
    qualify(10);
}

#[test]
#[ignore = "private PipeWire/pw-cat tools + real confined Opus worker; explicit native CI"]
fn real_20ms_uplink_reaches_an_independent_consumer_and_stops() {
    qualify(20);
}

fn qualify(duration: u16) {
    let server = Server::start();
    let config = config(7, duration);
    let packets = fixtures(config, 3000 / duration, 800 / duration);
    let mut owner = owner(&server, config);
    acknowledge(&mut owner, config);
    let mut capture = link(&server, &mut owner, "opus");
    let started = Instant::now();
    let mut sent = 0;
    let mut submitted = 0;
    let until =
        Duration::from_millis(u64::try_from(packets.len()).unwrap() * u64::from(duration) + 35);
    while started.elapsed() < until {
        let now = Instant::now();
        if sent < packets.len()
            && now.duration_since(started)
                >= Duration::from_millis(u64::try_from(sent).unwrap() * u64::from(duration))
        {
            assert_eq!(
                owner.receive_record_at(&record(&packets[sent], BINDING), now, || true),
                Ok(ReceiveResult::Queued)
            );
            sent += 1;
        }
        let result = owner.service(|| true).unwrap_or_else(|error| {
                panic!("duration={duration}ms sent={sent} submitted={submitted} elapsed={:?} native={:?} error={error:?}", started.elapsed(), owner.statistics());
            });
        if let RenderResult::Submitted { .. } = result {
            submitted += 1;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(sent, packets.len());
    assert!(
        submitted > 100,
        "actual decoded frames must reach native admission"
    );
    assert!(owner.statistics().submitted_frames > 48_000);
    let retirement = owner.decoder_retirement();
    let mut stop = [0; audio::AUDIO_STOP_RECORD_BYTES];
    audio::encode_stop(
        &AudioStop {
            direction: AudioDirection::Uplink,
            generation: config.generation(),
            reason: AudioStopReason::SessionEnded,
        },
        BINDING,
        &mut stop,
    )
    .unwrap();
    assert_eq!(
        owner.receive_record_at(
            &stop,
            Instant::now().checked_sub(Duration::from_secs(5)).unwrap(),
            || panic!("a bound stop cannot wait for fresh permission"),
        ),
        Ok(ReceiveResult::Stopped(AudioStopReason::SessionEnded))
    );
    assert_eq!(owner.state(), State::Closed);
    assert!(owner.control().is_stopped());
    assert_eq!(owner.queued_packets(), 0);
    assert!(owner.service(|| true).is_err());
    reaped(&retirement);
    finish_capture(&server, &mut capture);
}

#[test]
#[ignore = "private PipeWire daemon + real confined Opus worker; explicit native CI"]
fn acknowledgement_binding_and_original_receipt_are_enforced() {
    let server = Server::start();
    let config = config(7, 10);
    let packet = fixtures(config, 1, 0).remove(0);
    let mut early = owner(&server, config);
    assert_eq!(
        early.receive_record_at(&record(&packet, BINDING), Instant::now(), || true),
        Err(Error::NotAcknowledged)
    );
    assert_eq!(early.state(), State::Closed);
    closed(&mut early);

    let mut accepted = owner(&server, config);
    acknowledge(&mut accepted, config);
    assert_eq!(
        accepted.receive_record_at(
            &record(&packet, BINDING),
            Instant::now()
                .checked_sub(Duration::from_millis(150))
                .unwrap(),
            || true,
        ),
        Ok(ReceiveResult::Ignored)
    );
    assert_eq!(accepted.queued_packets(), 0);
    assert_eq!(
        accepted.receive_record_at(&record(&packet, BINDING + 1), Instant::now(), || true),
        Err(Error::Wire(WireError::InvalidBinding))
    );
    assert_eq!(accepted.state(), State::Closed);
    closed(&mut accepted);

    let mut failed = owner(&server, config);
    assert_eq!(
        failed.acknowledge(|| true, |_| Err(())),
        Err(Error::AcknowledgementUnknown)
    );
    assert_eq!(failed.state(), State::Closed);
    closed(&mut failed);
}

#[test]
#[ignore = "private PipeWire recorder + real confined Opus worker; explicit native CI"]
fn frozen_child_cannot_block_or_publish_late_sound_into_a_new_demand() {
    let server = Server::start();
    let config = config(7, 10);
    let packets = fixtures(config, 3, 0);
    let mut owner = owner(&server, config);
    acknowledge(&mut owner, config);
    let mut capture = link(&server, &mut owner, "silence");
    let retirement = owner.decoder_retirement();
    assert!(
        Command::new("kill")
            .args(["-STOP", &retirement.process_id().unwrap().to_string()])
            .status()
            .unwrap()
            .success()
    );
    for packet in packets {
        assert_eq!(
            owner.receive_record_at(&record(&packet, BINDING), Instant::now(), || true),
            Ok(ReceiveResult::Queued)
        );
    }
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        let started = Instant::now();
        let result = owner.service(|| true);
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "a frozen foreign codec must not block the native owner"
        );
        if result.is_err() {
            break;
        }
        assert_eq!(result.unwrap(), RenderResult::Waiting);
        assert!(Instant::now() < until, "missed native demand must retire");
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(owner.state(), State::Closed);
    assert!(owner.control().is_stopped());
    assert_eq!(owner.statistics().submitted_frames, 0);
    reaped(&retirement);
    finish_capture(&server, &mut capture);
}
