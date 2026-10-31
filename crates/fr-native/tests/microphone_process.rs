#![cfg(all(target_os = "linux", feature = "linux-microphone-process"))]
//! Actual process-owned `PulseAudio` RECORD → real Opus, using an independently
//! fed test-owned source. No physical microphone, network or user OS consent is
//! claimed by this native fixture. Failures retain private daemon artifacts.
#[allow(dead_code)]
mod pulse_support;
use fr_core::audio::{AudioChannels, AudioDirection, AudioGeneration, AudioStreamConfig};
use fr_media::audio::AudioDecoder;
use fr_native::{
    audio_process::{Batch, CaptureOptions, Error, MicrophoneCapture, Retirement},
    opus::Decoder,
};
use pulse_support::{
    Server,
    microphone::{Feeder, assert_recording_stopped, power},
};
use std::{
    path::PathBuf,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

fn image() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fr-audio-worker"))
}
fn options(server: PathBuf, generation: u64) -> CaptureOptions {
    CaptureOptions {
        server,
        source: "fr_test_mic".into(),
        configuration: AudioStreamConfig::new(
            AudioDirection::Uplink,
            AudioGeneration::from_raw(generation),
            AudioChannels::Mono,
            20,
            20,
        )
        .unwrap(),
        bitrate: 64_000,
        max_packet_bytes: 1000,
    }
}
fn start(
    server: &Server,
    generation: u64,
    until: Instant,
    permitted: Arc<AtomicBool>,
) -> MicrophoneCapture {
    MicrophoneCapture::start(
        &image(),
        options(server.socket.clone(), generation),
        until,
        move || permitted.load(Ordering::Acquire),
    )
    .unwrap()
}
fn ready(source: &mut MicrophoneCapture) {
    let until = Instant::now() + Duration::from_secs(3);
    while !source.poll_ready(|| true).unwrap() {
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn pull(source: &mut MicrophoneCapture) -> Batch {
    let expires = Instant::now() + Duration::from_millis(80);
    source.request(expires, || true).unwrap();
    loop {
        if let Some(batch) = source.poll_batch(|| true).unwrap() {
            assert!(
                batch.expires() <= expires,
                "IPC cannot renew original custody"
            );
            return batch;
        }
        assert!(Instant::now() < expires);
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn nonempty(source: &mut MicrophoneCapture) -> Batch {
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        let batch = pull(source);
        if batch.packet_count() != 0 {
            return batch;
        }
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(2));
    }
}
fn retired(receipt: &Retirement) {
    let until = Instant::now() + Duration::from_secs(3);
    while !receipt.is_complete() {
        assert!(Instant::now() < until, "original child must actually reap");
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(receipt.process_id(), None);
}

#[test]
fn absent_consent_expired_authority_and_default_sources_never_start_capture() {
    let absent = PathBuf::from("/nonexistent/fr-pulse-mic-server");
    assert_eq!(
        MicrophoneCapture::start(
            &image(),
            options(absent.clone(), 7),
            Instant::now() + Duration::from_secs(1),
            || false
        )
        .unwrap_err(),
        Error::Denied
    );
    assert_eq!(
        MicrophoneCapture::start(
            &image(),
            options(absent.clone(), 7),
            Instant::now() + Duration::from_secs(1),
            || panic!("fixture permission disappeared")
        )
        .unwrap_err(),
        Error::Denied
    );
    assert_eq!(
        MicrophoneCapture::start(&image(), options(absent.clone(), 7), Instant::now(), || {
            true
        })
        .unwrap_err(),
        Error::Expired
    );
    for name in [
        "",
        "@DEFAULT_SOURCE@",
        "remote:source",
        "source with spaces",
    ] {
        let mut selected = options(absent.clone(), 7);
        selected.source = name.into();
        assert_eq!(
            MicrophoneCapture::start(
                &image(),
                selected,
                Instant::now() + Duration::from_secs(1),
                || true
            )
            .unwrap_err(),
            Error::Selection
        );
    }
}

#[test]
#[ignore = "actual private PulseAudio and independent pipe source; explicit native CI"]
fn actual_child_capture_is_contiguous_and_stop_fences_retained_batches() {
    let server = Server::start();
    let _feeder = Feeder::start(&server);
    let allowed = Arc::new(AtomicBool::new(true));
    let mut source = start(
        &server,
        7,
        Instant::now() + Duration::from_secs(5),
        allowed.clone(),
    );
    ready(&mut source);
    let receipt = source.retirement();
    let old = source.control();
    let mut decoder = Decoder::new();
    decoder.configure(source.configuration()).unwrap();
    let (mut count, mut tones, mut next) = (0, 0, None);
    let until = Instant::now() + Duration::from_millis(800);
    while Instant::now() < until {
        let batch = pull(&mut source);
        assert!(batch.packet_count() <= 4);
        for packet in batch.consume(|| true).unwrap() {
            assert_eq!(packet.direction(), AudioDirection::Uplink);
            assert_eq!(packet.generation().as_raw(), 7);
            if let Some(next) = next {
                assert_eq!((packet.sequence(), packet.timestamp_samples()), next);
            }
            next = Some((packet.sequence() + 1, packet.timestamp_samples() + 960));
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
        std::thread::sleep(Duration::from_millis(3));
    }
    assert!(count >= 20, "only {count} real child packets");
    assert!(
        tones * 2 > count,
        "independent tone in {tones}/{count} packets"
    );
    let batch = nonempty(&mut source);
    old.stop();
    assert_eq!(batch.consume(|| true).unwrap_err(), Error::Stopped);
    retired(&receipt);
    assert_recording_stopped(&server);
    let mut replacement = start(&server, 8, Instant::now() + Duration::from_secs(3), allowed);
    ready(&mut replacement);
    old.stop();
    assert!(replacement.poll_ready(|| true).unwrap());
    replacement.stop();
    retired(&replacement.retirement());
    assert_recording_stopped(&server);
    println!(
        "process microphone: packets={count}, independent_tone={tones}; artifacts={}",
        server.directory.display()
    );
}

#[test]
#[ignore = "actual private PulseAudio and independent pipe source; explicit native CI"]
fn idle_revocation_and_original_authority_expiry_kill_without_caller_polling() {
    let server = Server::start();
    let _feeder = Feeder::start(&server);
    let allowed = Arc::new(AtomicBool::new(true));
    let mut source = start(
        &server,
        7,
        Instant::now() + Duration::from_secs(5),
        allowed.clone(),
    );
    ready(&mut source);
    let batch = nonempty(&mut source);
    let receipt = source.retirement();
    allowed.store(false, Ordering::Release);
    // This loop observes reaping only. It never polls the source or permission.
    retired(&receipt);
    assert_eq!(batch.consume(|| true).unwrap_err(), Error::Denied);
    assert_recording_stopped(&server);
    let until = Instant::now() + Duration::from_millis(500);
    let mut source = start(&server, 8, until, Arc::new(AtomicBool::new(true)));
    ready(&mut source);
    retired(&source.retirement());
    assert!(Instant::now() >= until);
    assert_eq!(source.poll_ready(|| true), Err(Error::Expired));
    assert_recording_stopped(&server);
}

#[test]
#[ignore = "actual private PulseAudio and SIGSTOP of the exact native child; explicit native CI"]
fn frozen_child_never_blocks_caller_and_retires_on_original_pull_deadline() {
    let server = Server::start();
    let _feeder = Feeder::start(&server);
    let mut source = start(
        &server,
        7,
        Instant::now() + Duration::from_secs(5),
        Arc::new(AtomicBool::new(true)),
    );
    ready(&mut source);
    let receipt = source.retirement();
    assert!(
        Command::new("kill")
            .args(["-STOP", &receipt.process_id().unwrap().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let expires = Instant::now() + Duration::from_millis(50);
    let started = Instant::now();
    source.request(expires, || true).unwrap();
    assert!(source.poll_batch(|| true).unwrap().is_none());
    assert!(
        started.elapsed() < Duration::from_millis(30),
        "caller performed native/IPC work"
    );
    assert_eq!(
        source.request(Instant::now() + Duration::from_millis(90), || true),
        Err(Error::Backpressure)
    );
    retired(&receipt);
    assert!(Instant::now() >= expires);
    assert_eq!(source.poll_batch(|| true).unwrap_err(), Error::Expired);
    assert_recording_stopped(&server);
}

#[test]
#[ignore = "actual private PulseAudio and independent pipe source; explicit native CI"]
fn retained_expiry_and_owner_drop_cannot_be_revived_by_new_permission() {
    let server = Server::start();
    let _feeder = Feeder::start(&server);
    let mut source = start(
        &server,
        7,
        Instant::now() + Duration::from_secs(5),
        Arc::new(AtomicBool::new(true)),
    );
    ready(&mut source);
    let batch = nonempty(&mut source);
    let expires = batch.expires();
    std::thread::sleep(
        expires.saturating_duration_since(Instant::now()) + Duration::from_millis(2),
    );
    assert_eq!(batch.consume(|| true).unwrap_err(), Error::Expired);
    retired(&source.retirement());
    assert_recording_stopped(&server);
    let mut source = start(
        &server,
        8,
        Instant::now() + Duration::from_secs(5),
        Arc::new(AtomicBool::new(true)),
    );
    ready(&mut source);
    let batch = nonempty(&mut source);
    let receipt = source.retirement();
    drop(source);
    assert_eq!(batch.consume(|| true).unwrap_err(), Error::Stopped);
    retired(&receipt);
    assert_recording_stopped(&server);
}

#[test]
#[ignore = "actual private PulseAudio and independent pipe source; explicit native CI"]
fn uncollected_result_expires_without_any_caller_service() {
    let server = Server::start();
    let _feeder = Feeder::start(&server);
    let mut source = start(
        &server,
        7,
        Instant::now() + Duration::from_secs(5),
        Arc::new(AtomicBool::new(true)),
    );
    ready(&mut source);
    source
        .request(Instant::now() + Duration::from_millis(70), || true)
        .unwrap();
    // The healthy child can complete normally, but a result held in the
    // supervisor's mailbox must not outlive that original request either.
    retired(&source.retirement());
    assert_eq!(source.poll_batch(|| true).unwrap_err(), Error::Expired);
    assert_recording_stopped(&server);
}

#[test]
#[ignore = "actual private PulseAudio and forced child crash; explicit native CI"]
fn idle_child_crash_is_observed_and_reaped_without_another_request() {
    let server = Server::start();
    let _feeder = Feeder::start(&server);
    let mut source = start(
        &server,
        7,
        Instant::now() + Duration::from_secs(5),
        Arc::new(AtomicBool::new(true)),
    );
    ready(&mut source);
    let retirement = source.retirement();
    let control = source.control();
    assert!(
        Command::new("kill")
            .args(["-KILL", &retirement.process_id().unwrap().to_string()])
            .status()
            .unwrap()
            .success()
    );
    // Reaping must precede the five-second permission limit without any new
    // source poll or request. Merely checking the input monitor cannot see this.
    retired(&retirement);
    assert!(control.is_stopped());
    assert_eq!(source.poll_ready(|| true), Err(Error::Native));
    assert_recording_stopped(&server);
}
