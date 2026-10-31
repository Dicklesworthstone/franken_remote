#![cfg(all(target_os = "linux", feature = "linux-microphone-process"))]
//! Accepted control-grant/clock records and synthetic presentation are policy
//! fixtures. The named `PulseAudio` recording source, isolated encoder child,
//! encoded wire records, independent decoder and process reaping are real.
//! This is not TLS, physical-device permission or a full fr connect UI test.
#[allow(dead_code)]
#[path = "../../fr-client/tests/clipboard_controller/mod.rs"]
mod authority;
#[allow(dead_code)]
mod pulse_support;

use fr_client::{
    audio::uplink::Profile,
    input::{ClientInstant, InputClient},
};
use fr_core::audio::{
    AudioChannels, AudioDirection, AudioGeneration, AudioStreamConfig, MicPermission, MicTalkMode,
};
use fr_media::audio::{AudioAccessUnit, AudioDecoder};
use fr_native::{
    audio_process::{
        Retirement,
        transmit::{Source, State, Transmitter},
    },
    opus::Decoder,
};
use fr_wire::{
    attachment,
    audio::{self, AudioConfiguration, AudioConfigured},
    audio_up,
    negotiation::{Capability, Role, Selection},
};
use pulse_support::{
    Server,
    microphone::{Feeder, assert_recording_stopped, power},
};
use std::{
    path::Path,
    process::Command,
    time::{Duration, Instant},
};

const CHANNEL: u32 = 73;
fn now(origin: Instant) -> ClientInstant {
    authority::client(u64::try_from(origin.elapsed().as_micros()).unwrap())
}
fn selection() -> Selection {
    let mut capabilities: Vec<_> = [
        (attachment::CAPABILITY, attachment::VERSION),
        (attachment::INPUT_CAPABILITY, attachment::INPUT_VERSION),
        (audio_up::CAPABILITY, audio_up::VERSION),
    ]
    .into_iter()
    .map(|(name, version)| Capability {
        name: name.into(),
        version,
        required: false,
    })
    .collect();
    capabilities.sort_by(|a, b| a.name.cmp(&b.name));
    Selection {
        version: 0,
        profile: 1,
        profile_version: 0,
        role: Role::RequestControl,
        limits: authority::L,
        capabilities,
    }
}
fn transmitter(server: &Path) -> (InputClient, Transmitter, Instant) {
    let origin = Instant::now();
    let mut input = authority::accepted();
    authority::ready(&mut input, 0, 0);
    let profile = Profile {
        channels: AudioChannels::Mono,
        frame_duration_ms: 20,
        max_packet_bytes: 1000,
        max_record_bytes: audio::AUDIO_PACKET_OVERHEAD + 1000,
        jitter_target_ms: 20,
    };
    let sender = input
        .attach_microphone(
            authority::grant().request.parent,
            CHANNEL,
            &selection(),
            true,
            profile,
            now(origin),
        )
        .unwrap();
    let transmitter = Transmitter::new(
        sender,
        Path::new(env!("CARGO_BIN_EXE_fr-audio-worker")),
        Source {
            server: server.into(),
            name: "fr_test_mic".into(),
            bitrate: 64_000,
        },
        || now(origin),
    );
    (input, transmitter, origin)
}
fn begin(transmitter: &mut Transmitter) -> AudioConfiguration {
    transmitter.set_permission(MicPermission::Granted);
    let offer = transmitter
        .begin_talk(MicTalkMode::OpenMic { active: true })
        .unwrap();
    assert_eq!(transmitter.state(), State::AwaitingHost);
    assert!(transmitter.retirement().is_none());
    assert!(
        transmitter
            .send_configuration(|bytes, until| {
                assert_eq!(audio::decode_configuration(bytes, CHANNEL).unwrap(), offer);
                assert!(until > authority::client(0));
                Ok(true)
            })
            .unwrap()
    );
    assert!(transmitter.retirement().is_none());
    offer
}
fn acknowledgement(offer: AudioConfiguration) -> Vec<u8> {
    let mut bytes = vec![0; audio::AUDIO_CONFIGURED_RECORD_BYTES];
    audio::encode_configured(
        &AudioConfigured {
            direction: AudioDirection::Uplink,
            generation: offer.generation,
            accepted: true,
            actual_channels: offer.channels,
            actual_sample_rate: offer.sample_rate,
            actual_frame_duration_ms: offer.frame_duration_ms,
        },
        CHANNEL,
        &mut bytes,
    )
    .unwrap();
    bytes
}
fn reaped(retirement: &Retirement) {
    let until = Instant::now() + Duration::from_secs(3);
    while !retirement.is_complete() {
        assert!(
            Instant::now() < until,
            "original microphone child was not reaped"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(retirement.process_id(), None);
}

#[test]
fn no_process_opens_before_explicit_talk_and_exact_positive_host_ack() {
    let (_input, mut transmitter, _) = transmitter(Path::new("/nonexistent/fr-microphone"));
    assert_eq!(transmitter.state(), State::Disabled);
    assert!(transmitter.retirement().is_none());
    transmitter.set_permission(MicPermission::Granted);
    assert_eq!(transmitter.state(), State::Disabled);
    assert!(transmitter.retirement().is_none());
    let mut offer = begin(&mut transmitter);
    offer.generation = AudioGeneration::from_raw(offer.generation.as_raw() + 1);
    assert!(transmitter.configured(&acknowledgement(offer)).is_err());
    assert_eq!(transmitter.state(), State::Stopped);
    assert!(transmitter.retirement().is_none());
}

#[test]
fn muted_ticks_and_early_talk_refusal_preserve_a_later_explicit_enable() {
    let (_input, mut transmitter, _) = transmitter(Path::new("/nonexistent/fr-microphone"));
    assert_eq!(
        transmitter
            .service(|_, _| panic!("muted microphone emitted bytes"))
            .unwrap()
            .admitted_packets,
        0
    );
    assert!(matches!(
        transmitter.begin_talk(MicTalkMode::OpenMic { active: true }),
        Err(fr_native::audio_process::transmit::Error::Consent(
            fr_client::audio::uplink::Error::Permission
        ))
    ));
    assert_eq!(transmitter.state(), State::Disabled);
    let first = begin(&mut transmitter);
    assert_eq!(
        transmitter
            .service(|_, _| panic!("unacknowledged microphone emitted bytes"))
            .unwrap()
            .admitted_packets,
        0
    );
    transmitter.mute();
    transmitter
        .service(|_, _| panic!("muted microphone emitted bytes"))
        .unwrap();
    let second = transmitter
        .begin_talk(MicTalkMode::OpenMic { active: true })
        .unwrap();
    assert!(second.generation > first.generation);
    assert!(transmitter.retirement().is_none());
}

#[test]
#[ignore = "private PulseAudio/pipe-source, real isolated Opus worker; explicit native CI"]
fn accepted_controller_talk_records_real_wire_opus_and_mutes_locally() {
    let server = Server::start();
    let _feeder = Feeder::start(&server);
    let (mut input, mut transmitter, origin) = transmitter(&server.socket);
    let offer = begin(&mut transmitter);
    assert_recording_stopped(&server);
    transmitter.configured(&acknowledgement(offer)).unwrap();
    let retirement = transmitter.retirement().unwrap();
    let egress = transmitter.egress().unwrap();
    assert!(egress.is_open());
    let mut decoder = Decoder::new();
    decoder
        .configure(
            AudioStreamConfig::new(
                AudioDirection::Uplink,
                offer.generation,
                offer.channels,
                offer.frame_duration_ms,
                offer.jitter_target_ms,
            )
            .unwrap(),
        )
        .unwrap();
    let (mut packets, mut tones, mut next) = (0_usize, 0_usize, None);
    let until = Instant::now() + Duration::from_millis(700);
    while Instant::now() < until {
        let progress = transmitter
            .service(|bytes, expiry| {
                assert!(
                    now(origin) < expiry,
                    "wire admission retains original custody"
                );
                let packet = audio::decode_packet(bytes, CHANNEL).unwrap();
                assert_eq!(packet.direction, AudioDirection::Uplink);
                assert_eq!(packet.generation, offer.generation);
                assert_eq!(packet.duration_samples, 960);
                if let Some(expected) = next {
                    assert_eq!((packet.sequence, packet.timestamp_samples), expected);
                } else {
                    assert_eq!(packet.sequence, 0);
                }
                next = Some((packet.sequence + 1, packet.timestamp_samples + 960));
                let packet = AudioAccessUnit::new(
                    packet.direction,
                    packet.generation,
                    packet.sequence,
                    packet.timestamp_samples,
                    packet.duration_samples,
                    false,
                    packet.payload,
                )
                .unwrap();
                decoder.submit_packet(&packet).unwrap();
                let pcm = decoder.poll_pcm().unwrap().unwrap();
                let tone = power(pcm.samples(), 1000.0);
                if tone > 1.0e11
                    && [600.0, 800.0, 1200.0, 1500.0]
                        .into_iter()
                        .all(|hz| tone > 30.0 * power(pcm.samples(), hz))
                {
                    tones += 1;
                }
                packets += 1;
                Ok(true)
            })
            .unwrap();
        assert!(progress.admitted_packets <= 4);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(packets >= 10, "too few actual captured packets: {packets}");
    assert!(
        tones * 2 > packets,
        "independent tone decoded in {tones}/{packets}"
    );
    let stop = transmitter.mute().unwrap();
    assert!(
        !egress.is_open(),
        "queued speech keeps the original talk fence"
    );
    assert_eq!(stop.generation, offer.generation);
    assert_eq!(transmitter.state(), State::Disabled);
    reaped(&retirement);
    assert_recording_stopped(&server);
    input.tick(now(origin)).unwrap();
    println!(
        "actual controller microphone: {packets} records; {tones} tone packets; artifacts={}",
        server.directory.display()
    );
}

fn first_blocked(transmitter: &mut Transmitter) -> ClientInstant {
    let until = Instant::now() + Duration::from_secs(1);
    loop {
        let mut deadline = None;
        let progress = transmitter
            .service(|_, expiry| {
                deadline = Some(expiry);
                Ok(false)
            })
            .unwrap();
        assert_eq!(progress.admitted_packets, 0);
        if progress.blocked {
            return deadline.unwrap();
        }
        assert!(
            Instant::now() < until,
            "actual source did not produce a packet"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
#[ignore = "private PulseAudio/pipe-source, real isolated Opus worker; explicit native CI"]
fn dropping_controller_reaps_idle_child_and_fences_backpressured_speech() {
    let server = Server::start();
    let _feeder = Feeder::start(&server);
    let (input, mut transmitter, _) = transmitter(&server.socket);
    let offer = begin(&mut transmitter);
    transmitter.configured(&acknowledgement(offer)).unwrap();
    first_blocked(&mut transmitter);
    let retirement = transmitter.retirement().unwrap();
    drop(input);
    // Intentionally do not poll the transmitter or call stop: the durable
    // original-controller monitor must end recording and reap independently.
    reaped(&retirement);
    assert_recording_stopped(&server);
    assert_eq!(transmitter.state(), State::Stopped);
    assert!(
        transmitter
            .service(|_, _| panic!("revoked speech reached wire admission"))
            .is_err()
    );
}

#[test]
#[ignore = "private PulseAudio/pipe-source, real isolated Opus worker; explicit native CI"]
fn late_transport_capacity_cannot_refresh_a_recorded_packet_deadline() {
    let server = Server::start();
    let _feeder = Feeder::start(&server);
    let (_input, mut transmitter, origin) = transmitter(&server.socket);
    let offer = begin(&mut transmitter);
    transmitter.configured(&acknowledgement(offer)).unwrap();
    let deadline = first_blocked(&mut transmitter);
    let retirement = transmitter.retirement().unwrap();
    while now(origin) < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    let mut sent = false;
    assert!(
        transmitter
            .service(|_, _| {
                sent = true;
                Ok(true)
            })
            .is_err()
    );
    assert!(!sent);
    reaped(&retirement);
    assert_recording_stopped(&server);
}

#[test]
#[ignore = "private PulseAudio/pipe-source, real isolated Opus worker; explicit native CI"]
fn idle_child_crash_removes_recording_status_without_an_application_turn() {
    let server = Server::start();
    let _feeder = Feeder::start(&server);
    let (_input, mut transmitter, _) = transmitter(&server.socket);
    let offer = begin(&mut transmitter);
    transmitter.configured(&acknowledgement(offer)).unwrap();
    first_blocked(&mut transmitter);
    assert_eq!(transmitter.state(), State::Ready);
    let retirement = transmitter.retirement().unwrap();
    let pid = retirement.process_id().unwrap();
    assert!(
        Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status()
            .unwrap()
            .success()
    );
    // No pending native request or further application turn discovers EOF.
    // Observe child failure independently, before the original view/lease cap.
    let until = Instant::now() + Duration::from_millis(500);
    while !retirement.is_complete() {
        assert!(
            Instant::now() < until,
            "idle child death must not wait for lease expiry"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(retirement.process_id(), None);
    assert_eq!(transmitter.state(), State::Stopped);
    assert_recording_stopped(&server);
}
