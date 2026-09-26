//! Actual CLI output owner, private `PulseAudio` null sink and restricted decoder.
//! Records, permissions and sound are local fixtures, not live-tailnet evidence.
use super::*;
#[allow(dead_code)]
#[path = "../../../../../tests/pulse_support/mod.rs"]
mod pulse_support;
use fr_core::audio::{AudioChannels, AudioDirection, AudioGeneration};
use fr_media::audio::{AudioEncoder, AudioPcmFrame};
use fr_native::opus::Encoder;
use fr_wire::audio::{self, AudioConfigured, AudioPacket};
use pulse_support::Server;
use std::{process::Command, sync::Mutex, thread, time::Duration};
static SERIAL: Mutex<()> = Mutex::new(());
const BINDING: u32 = 61;
fn offer(generation: u64) -> AudioConfiguration {
    AudioConfiguration {
        direction: AudioDirection::Downlink,
        generation: AudioGeneration::from_raw(generation),
        channels: AudioChannels::Stereo,
        sample_rate: 48_000,
        frame_duration_ms: 20,
        max_packet_bytes: 1275,
        max_decoded_samples: 960,
        jitter_target_ms: 20,
    }
}
fn image() -> PathBuf {
    if let Some(path) = std::env::var_os("FR_OPUS_WORKER_TEST_BIN") {
        return PathBuf::from(path);
    }
    let exe = std::env::current_exe().unwrap();
    let direct = exe.parent().unwrap().join("fr-opus-worker");
    if direct.is_file() {
        direct
    } else {
        exe.parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("fr-opus-worker")
    }
}
fn owner(server: &Server) -> Output {
    let report = Rc::new(RefCell::new(Report::default()));
    let mut output = Output::new(server.socket.clone(), Some("fr_test".into()), report);
    output.image = Some(image()); // libtest lives in deps/, not the installed fr directory.
    output
}
fn turn(output: &mut Output, ack: &mut Option<AudioConfigured>) -> Result<(), ViewerAudioRefused> {
    output.service(&mut || true, &mut |bytes| {
        assert!(ack.is_none(), "configuration acknowledged only once");
        *ack = Some(audio::decode_configured(bytes, BINDING).unwrap());
        Ok(())
    })
}
fn configured(output: &mut Output, generation: u64) -> Retirement {
    output.configure(BINDING, offer(generation)).unwrap();
    assert!(!output.report.borrow().acknowledged);
    let until = Instant::now() + Duration::from_secs(3);
    let mut ack = None;
    while ack.is_none() {
        turn(output, &mut ack).unwrap();
        assert!(
            Instant::now() < until,
            "original native setup must complete"
        );
        thread::sleep(Duration::from_millis(1));
    }
    let ack = ack.unwrap();
    assert!(ack.accepted);
    assert_eq!(ack.generation.as_raw(), generation);
    assert_eq!(ack.actual_channels, AudioChannels::Stereo);
    assert_eq!(ack.actual_sample_rate, 48_000);
    assert_eq!(ack.actual_frame_duration_ms, 20);
    let retirement = output.retirement.as_ref().unwrap().clone();
    assert!(retirement.process_id().is_some());
    assert!(matches!(output.stage, Stage::Playing(_)));
    retirement
}
fn retired(receipt: &Retirement) {
    let until = Instant::now() + Duration::from_secs(3);
    while !receipt.is_complete() {
        assert!(Instant::now() < until, "original decoder must be reaped");
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(receipt.process_id(), None);
}
fn packets(generation: u64, count: u16) -> Vec<Vec<u8>> {
    let offer = offer(generation);
    let config = AudioStreamConfig::new(
        offer.direction,
        offer.generation,
        offer.channels,
        offer.frame_duration_ms,
        offer.jitter_target_ms,
    )
    .unwrap();
    let mut encoder = Encoder::new();
    encoder.configure(config).unwrap();
    (0..count)
        .map(|sequence| {
            let samples: Vec<_> = (0..960u16)
                .flat_map(|n| {
                    let v = (i16::try_from((u32::from(n) + u32::from(sequence) * 960) % 97)
                        .unwrap()
                        - 48)
                        * 200;
                    [v, -v]
                })
                .collect();
            encoder
                .submit_pcm(
                    &AudioPcmFrame::from_interleaved(
                        offer.generation,
                        offer.channels,
                        90_000 + u64::from(sequence) * 960,
                        &samples,
                    )
                    .unwrap(),
                )
                .unwrap();
            let packet = encoder.poll_packet().unwrap().unwrap();
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
        })
        .collect()
}

#[test]
#[ignore = "requires the real PulseAudio test server and installed fr-opus-worker"]
fn cli_audio_uses_restricted_child_and_real_output_with_no_decoder_on_session_thread() {
    let _serial = SERIAL.lock().unwrap();
    let packets = packets(41, 25);
    let server = Server::start();
    let mut output = owner(&server);
    let retirement = configured(&mut output, 41);
    let monitor = server.capture(|| {
        output
            .service(&mut || true, &mut |_| panic!("duplicate acknowledgement"))
            .unwrap();
    });
    let begin = Instant::now();
    for (i, bytes) in packets.iter().enumerate() {
        while begin.elapsed() < Duration::from_millis(u64::try_from(i).unwrap() * 20) {
            output
                .service(&mut || true, &mut |_| panic!("duplicate acknowledgement"))
                .unwrap();
            thread::sleep(Duration::from_millis(1));
        }
        output.receive(bytes, &mut || true).unwrap();
    }
    while output.report.borrow().submitted < 25 {
        assert!(begin.elapsed() < Duration::from_secs(2));
        output
            .service(&mut || true, &mut |_| panic!("duplicate acknowledgement"))
            .unwrap();
        thread::sleep(Duration::from_millis(1));
    }
    output.ended(ViewerAudioEnd::Local);
    while !matches!(output.stage, Stage::Done) {
        assert!(begin.elapsed() < Duration::from_secs(3));
        output
            .service(&mut || false, &mut |_| {
                panic!("no acknowledgement while stopping")
            })
            .unwrap();
        thread::sleep(Duration::from_millis(1));
    }
    retired(&retirement);
    let samples = monitor.finish();
    assert!(
        samples.iter().filter(|s| s.unsigned_abs() > 50).count() > 1000,
        "independent native monitor must receive real decoded sound"
    );
    assert_eq!(output.report.borrow().submitted, 25);
}

#[test]
#[ignore = "requires the real PulseAudio test server and installed fr-opus-worker"]
fn failed_child_configuration_cannot_acknowledge_or_fall_back_to_in_process_decode() {
    let _serial = SERIAL.lock().unwrap();
    let server = Server::start();
    let mut output = owner(&server);
    output.image = Some(PathBuf::from("/etc/hosts")); // exists but cannot execute
    output.configure(BINDING, offer(11)).unwrap();
    let mut ack = None;
    let until = Instant::now() + Duration::from_secs(3);
    while turn(&mut output, &mut ack).is_ok() {
        assert!(ack.is_none());
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(1));
    }
    assert!(ack.is_none());
    assert!(!output.report.borrow().acknowledged);
    assert_eq!(output.report.borrow().submitted, 0);
    assert!(matches!(output.stage, Stage::Done));
    retired(output.retirement.as_ref().unwrap());
}

#[test]
#[ignore = "requires the real PulseAudio test server and installed fr-opus-worker"]
fn frozen_decoder_fails_without_blocking_the_cli_or_submitting_late_pcm() {
    let _serial = SERIAL.lock().unwrap();
    let packets = packets(8, 3);
    let server = Server::start();
    let mut output = owner(&server);
    let retirement = configured(&mut output, 8);
    assert!(
        Command::new("kill")
            .args(["-STOP", &retirement.process_id().unwrap().to_string()])
            .status()
            .unwrap()
            .success()
    );
    for bytes in packets {
        output.receive(&bytes, &mut || true).unwrap();
    }
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        let started = Instant::now();
        let result = output.service(&mut || true, &mut |_| panic!("duplicate acknowledgement"));
        assert!(
            started.elapsed() < Duration::from_millis(40),
            "no child wait on CLI"
        );
        if result.is_err() {
            break;
        }
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(output.report.borrow().submitted, 0);
    retired(&retirement);
}

#[test]
#[ignore = "requires the real PulseAudio test server and installed fr-opus-worker"]
fn output_reset_needs_original_decoder_retirement_and_a_new_child() {
    let _serial = SERIAL.lock().unwrap();
    let server = Server::start();
    let mut output = owner(&server);
    let original = configured(&mut output, 70);
    let pid = original.process_id().unwrap();
    output.reset();
    if !original.is_complete() {
        assert!(output.configure(BINDING, offer(71)).is_err());
    }
    retired(&original);
    output.report.borrow_mut().acknowledged = false;
    let replacement = configured(&mut output, 71);
    assert_ne!(replacement.process_id(), Some(pid));
    drop(output);
    retired(&replacement);
}
