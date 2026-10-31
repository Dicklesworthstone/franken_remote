#![cfg(all(target_os = "linux", feature = "linux-microphone-capture"))]
//! Actual named-source recording → Opus. An independent Python process feeds
//! synthetic PCM to a test-owned pipe source on a private `PulseAudio` daemon.
//! This qualifies the native boundary, not physical mic consent or networking.
#[allow(dead_code)]
mod pulse_support;
use fr_core::audio::{AudioChannels, AudioDirection, AudioGeneration, AudioStreamConfig};
use fr_media::audio::AudioDecoder;
use fr_native::{
    opus::Decoder,
    pulse::{
        Error,
        microphone::{Batch, MicrophoneSource},
    },
};
use pulse_support::{
    Server,
    microphone::{Feeder, assert_recording_stopped, power},
};
use std::{
    path::Path,
    time::{Duration, Instant},
};

fn config(generation: u64) -> AudioStreamConfig {
    AudioStreamConfig::new(
        AudioDirection::Uplink,
        AudioGeneration::from_raw(generation),
        AudioChannels::Mono,
        20,
        20,
    )
    .unwrap()
}
fn connect(server: &Server, generation: u64) -> MicrophoneSource {
    MicrophoneSource::connect(
        &server.socket,
        "fr_test_mic",
        config(generation),
        64_000,
        1000,
        || true,
    )
    .unwrap()
}
fn ready(source: &mut MicrophoneSource) {
    let until = Instant::now() + Duration::from_secs(2);
    while !source.poll_ready(|| true).unwrap() {
        assert!(Instant::now() < until, "native microphone startup deadline");
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn next_batch(source: &mut MicrophoneSource) -> Batch {
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        let batch = source
            .pull(Instant::now() + Duration::from_millis(80), || true)
            .unwrap();
        if batch.packet_count() > 0 {
            return batch;
        }
        assert!(Instant::now() < until, "actual captured packet deadline");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn microphone_selection_and_original_consent_are_required_before_native_setup() {
    let absent = Path::new("/nonexistent/fr-microphone-server");
    assert_eq!(
        MicrophoneSource::connect(absent, "named_source", config(7), 64_000, 1000, || false)
            .unwrap_err(),
        Error::Denied
    );
    for name in [
        "",
        "@DEFAULT_SOURCE@",
        "source with space",
        "remote:device",
        "a/b",
    ] {
        assert_eq!(
            MicrophoneSource::connect(absent, name, config(7), 64_000, 1000, || true).unwrap_err(),
            Error::Selection
        );
    }
    let down = AudioStreamConfig::new(
        AudioDirection::Downlink,
        AudioGeneration::from_raw(7),
        AudioChannels::Mono,
        20,
        20,
    )
    .unwrap();
    assert_eq!(
        MicrophoneSource::connect(absent, "named_source", down, 64_000, 1000, || true).unwrap_err(),
        Error::Configuration
    );
    assert_eq!(
        MicrophoneSource::connect(
            Path::new("/tmp/../pulse"),
            "named_source",
            config(7),
            64_000,
            1000,
            || true
        )
        .unwrap_err(),
        Error::Selection
    );
    for (generation, bitrate, payload) in [(0, 64_000, 1000), (7, 1, 1000), (7, 64_000, 1)] {
        assert_eq!(
            MicrophoneSource::connect(
                absent,
                "named_source",
                config(generation),
                bitrate,
                payload,
                || true
            )
            .unwrap_err(),
            Error::Configuration
        );
    }
}

#[test]
#[ignore = "private PulseAudio/pactl/pipe-source + independent synthetic feeder; explicit native CI"]
fn named_source_records_real_opus_and_terminal_stop_fences_retained_batches() {
    let server = Server::start();
    let _feeder = Feeder::start(&server);
    let mut source = connect(&server, 7);
    ready(&mut source);
    let old = source.control();
    let mut decoder = Decoder::new();
    decoder.configure(config(7)).unwrap();
    let (mut count, mut tones, mut previous) = (0_usize, 0_usize, None);
    let started = Instant::now();
    while started.elapsed() < Duration::from_millis(1800) {
        let expires = Instant::now() + Duration::from_millis(80);
        let batch = source.pull(expires, || true).unwrap();
        assert!(batch.packet_count() <= 4);
        assert!(batch.expires() <= expires);
        for packet in batch.consume(|| true).unwrap() {
            assert_eq!(packet.direction(), AudioDirection::Uplink);
            assert_eq!(packet.generation(), config(7).generation());
            assert_eq!(packet.duration_samples(), 960);
            assert!(packet.payload().len() <= 1000);
            if let Some((sequence, timestamp)) = previous {
                assert_eq!(packet.sequence(), sequence + 1);
                assert!(packet.timestamp_samples() >= timestamp + 960);
            }
            previous = Some((packet.sequence(), packet.timestamp_samples()));
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
            count += 1;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(count >= 30, "native source produced only{count} packets");
    assert!(
        tones * 2 > count,
        "independently fed tone present in{tones}/{count} packets"
    );
    let retained = next_batch(&mut source);
    let stop = old.clone();
    std::thread::spawn(move || stop.stop()).join().unwrap();
    assert_eq!(retained.consume(|| true).unwrap_err(), Error::Denied);
    assert_eq!(source.poll_ready(|| true), Err(Error::Denied));
    assert!(!source.is_ready());
    assert_eq!(source.poll_ready(|| true), Err(Error::Closed));
    assert_recording_stopped(&server);

    let mut replacement = connect(&server, 8);
    ready(&mut replacement);
    old.stop();
    assert!(!replacement.control().is_stopped());
    assert!(replacement.poll_ready(|| true).unwrap());
    replacement.close();
    println!(
        "native microphone capture: packets={count} matching_tone_packets={tones}; artifacts={}",
        server.directory.display()
    );
}

#[test]
#[ignore = "private PulseAudio/pactl/pipe-source; explicit native CI"]
fn capture_permission_loss_and_original_deadlines_close_without_resuming() {
    let mut server = Server::start();
    let _feeder = Feeder::start(&server);
    let mut source = connect(&server, 7);
    ready(&mut source);
    let mut checks = 0;
    let result = source.pull(Instant::now() + Duration::from_millis(80), || {
        checks += 1;
        checks < 3
    });
    assert_eq!(result.unwrap_err(), Error::Denied);
    assert!(
        checks >= 3,
        "permission must change inside the bounded native turn"
    );
    assert!(source.control().is_stopped());
    assert_eq!(source.poll_ready(|| true), Err(Error::Closed));

    let mut source = connect(&server, 8);
    ready(&mut source);
    let batch = next_batch(&mut source);
    std::thread::sleep(Duration::from_millis(85));
    assert_eq!(batch.consume(|| true).unwrap_err(), Error::Expired);
    assert_eq!(
        source.pull(Instant::now(), || true).unwrap_err(),
        Error::Expired
    );
    assert_eq!(source.poll_ready(|| true), Err(Error::Closed));

    let mut source = connect(&server, 9);
    ready(&mut source);
    server.kill();
    let until = Instant::now() + Duration::from_secs(2);
    while source.poll_ready(|| true).is_ok() {
        assert!(Instant::now() < until, "daemon loss must close capture");
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(source.control().is_stopped());
    assert_eq!(source.poll_ready(|| true), Err(Error::Closed));
}

#[test]
#[ignore = "private PulseAudio/pactl/pipe-source; explicit native CI"]
fn uncertain_permission_and_capture_gaps_cannot_resume_an_uplink_epoch() {
    let server = Server::start();
    let _feeder = Feeder::start(&server);
    let mut source = connect(&server, 7);
    ready(&mut source);
    let retained = next_batch(&mut source);
    let mut checks = 0;
    let result = source.pull(Instant::now() + Duration::from_millis(80), || {
        checks += 1;
        assert!(checks < 3, "intentional microphone permission panic");
        true
    });
    assert_eq!(result.unwrap_err(), Error::Denied);
    assert!(source.control().is_stopped());
    assert_eq!(retained.consume(|| true).unwrap_err(), Error::Denied);
    assert_eq!(source.poll_ready(|| true), Err(Error::Closed));

    let mut source = connect(&server, 8);
    ready(&mut source);
    let retained = next_batch(&mut source);
    assert_eq!(
        retained
            .consume(|| panic!("intentional batch permission panic"))
            .unwrap_err(),
        Error::Denied
    );
    assert_eq!(source.poll_ready(|| true), Err(Error::Denied));
    assert_eq!(source.poll_ready(|| true), Err(Error::Closed));

    let mut source = connect(&server, 9);
    ready(&mut source);
    // More than the admitted native queue can elapse. Never encode only the
    // newest raw frames as though their sample positions were still contiguous
    // with consecutive packet sequences in the original jitter epoch.
    std::thread::sleep(Duration::from_millis(250));
    assert_eq!(
        source
            .pull(Instant::now() + Duration::from_millis(80), || true)
            .unwrap_err(),
        Error::CaptureDiscontinuity
    );
    assert!(source.control().is_stopped());
    assert_eq!(source.poll_ready(|| true), Err(Error::Closed));
}
