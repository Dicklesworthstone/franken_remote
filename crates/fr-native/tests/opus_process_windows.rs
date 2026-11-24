#![cfg(all(windows, feature = "windows-opus-process"))]
//! Actual private Windows child, named pipe, job and libopus. The executable
//! must live on a protected local fixed volume, as required by deployment.
use fr_client::audio::playout::decoder::PolledDecoder;
use fr_core::audio::{AudioChannels, AudioDirection, AudioGeneration, AudioStreamConfig};
use fr_media::audio::{
    AudioAccessUnit, AudioDecoder, AudioEncoder, AudioMediaError, AudioPcmFrame,
};
use fr_native::opus::{
    CodecLimits, Decoder, Encoder,
    process::{ProcessDecoder, Retirement},
};
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Mutex,
    thread,
    time::{Duration, Instant},
};

static SERIAL: Mutex<()> = Mutex::new(());
fn config(channels: AudioChannels, direction: AudioDirection, millis: u16) -> AudioStreamConfig {
    AudioStreamConfig::new(direction, AudioGeneration::INITIAL, channels, millis, 20).unwrap()
}
fn tone(config: AudioStreamConfig, timestamp: u64) -> AudioPcmFrame {
    let count = usize::try_from(config.expected_samples_per_frame()).unwrap();
    let channels = usize::from(config.channels().count());
    let mut samples = vec![0; count * channels];
    for (index, frame) in samples.chunks_exact_mut(channels).enumerate() {
        for (channel, sample) in frame.iter_mut().enumerate() {
            let period = if channel == 0 { 40 } else { 53 };
            let phase = timestamp % 480 + u64::try_from(index).unwrap();
            *sample = if (phase / period).is_multiple_of(2) {
                12_000
            } else {
                -12_000
            };
        }
    }
    AudioPcmFrame::from_interleaved(config.generation(), config.channels(), timestamp, &samples)
        .unwrap()
}
fn image() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fr-opus-worker"))
}
fn make(config: AudioStreamConfig) -> (ProcessDecoder, Retirement) {
    let (mut decoder, receipt) = ProcessDecoder::new(&image(), CodecLimits::ABSOLUTE).unwrap();
    decoder.configure(config).unwrap();
    let until = Instant::now() + Duration::from_secs(3);
    while !decoder
        .poll_configured()
        .expect("protected real Windows worker must configure")
    {
        assert!(Instant::now() < until, "original setup deadline");
        thread::sleep(Duration::from_millis(1));
    }
    (decoder, receipt)
}
fn retired(receipt: &Retirement) {
    let until = Instant::now() + Duration::from_secs(3);
    while !receipt.is_complete() {
        assert!(
            Instant::now() < until,
            "original job and child must prove retirement"
        );
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(receipt.process_id(), None);
}
fn packet(config: AudioStreamConfig, sequence: u64) -> AudioAccessUnit {
    let mut encoder = Encoder::new();
    encoder.configure(config).unwrap();
    encoder
        .submit_pcm(&tone(
            config,
            1000 + sequence * u64::from(config.expected_samples_per_frame()),
        ))
        .unwrap();
    let packet = encoder.poll_packet().unwrap().unwrap();
    AudioAccessUnit::new(
        packet.direction(),
        packet.generation(),
        sequence,
        packet.timestamp_samples(),
        packet.duration_samples(),
        false,
        packet.payload(),
    )
    .unwrap()
}
fn collect(decoder: &mut ProcessDecoder) -> AudioPcmFrame {
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(pcm) = decoder.poll_pcm().expect("real private worker reply") {
            return pcm;
        }
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn private_worker_matches_real_codec_and_plc_across_profiles() {
    let _serial = SERIAL.lock().unwrap();
    for channels in [AudioChannels::Mono, AudioChannels::Stereo] {
        for duration in [5, 10, 20, 40, 60] {
            let config = config(channels, AudioDirection::Downlink, duration);
            let (mut decoder, receipt) = make(config);
            let mut oracle = Decoder::new();
            AudioDecoder::configure(&mut oracle, config).unwrap();
            let packet = packet(config, 0);
            decoder.submit_packet(&packet).unwrap();
            AudioDecoder::submit_packet(&mut oracle, &packet).unwrap();
            assert_eq!(
                collect(&mut decoder).samples(),
                AudioDecoder::poll_pcm(&mut oracle)
                    .unwrap()
                    .unwrap()
                    .samples()
            );
            let samples = u16::try_from(config.expected_samples_per_frame()).unwrap();
            assert!(decoder.submit_plc(samples).unwrap().is_none());
            let actual = collect(&mut decoder);
            let expected = oracle.decode_plc(samples).unwrap();
            assert_eq!(actual.samples(), expected.samples());
            assert_eq!(actual.timestamp_samples(), expected.timestamp_samples());
            drop(decoder);
            retired(&receipt);
        }
    }
}

#[test]
fn malformed_packet_ends_original_child_and_never_restarts() {
    let _serial = SERIAL.lock().unwrap();
    let config = config(AudioChannels::Mono, AudioDirection::Downlink, 10);
    let (mut decoder, receipt) = make(config);
    let bad = AudioAccessUnit::new(
        config.direction(),
        config.generation(),
        0,
        0,
        480,
        false,
        &[0xff, 0xff, 0xff],
    )
    .unwrap();
    decoder.submit_packet(&bad).unwrap();
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        match decoder.poll_pcm() {
            Err(_) => break,
            Ok(None) => {}
            Ok(Some(_)) => panic!("hostile codec input must not publish PCM"),
        }
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(1));
    }
    assert!(decoder.poll_pcm().is_err());
    assert!(decoder.configure(config).is_err());
    drop(decoder);
    retired(&receipt);
}

#[test]
fn four_original_owners_are_the_limit_and_old_stop_cannot_touch_replacement() {
    let _serial = SERIAL.lock().unwrap();
    let config = config(AudioChannels::Stereo, AudioDirection::Downlink, 20);
    let mut owners = (0..4).map(|_| make(config)).collect::<Vec<_>>();
    let (mut refused, empty) = ProcessDecoder::new(&image(), CodecLimits::ABSOLUTE).unwrap();
    assert_eq!(
        refused.configure(config),
        Err(AudioMediaError::Backpressure)
    );
    assert!(empty.is_complete());
    assert_eq!(empty.process_id(), None);
    let (decoder, old) = owners.pop().unwrap();
    drop(decoder);
    retired(&old);
    let (mut replacement, replacement_receipt) = make(config);
    old.stop();
    replacement.submit_packet(&packet(config, 0)).unwrap();
    assert!(!collect(&mut replacement).samples().is_empty());
    drop(replacement);
    retired(&replacement_receipt);
    for (decoder, receipt) in owners {
        drop(decoder);
        retired(&receipt);
    }
}

#[test]
fn uncollected_mailbox_and_stop_remain_bounded() {
    let _serial = SERIAL.lock().unwrap();
    let config = config(AudioChannels::Mono, AudioDirection::Downlink, 10);
    let (mut decoder, receipt) = make(config);
    decoder.submit_packet(&packet(config, 0)).unwrap();
    assert_eq!(
        decoder.submit_packet(&packet(config, 1)),
        Err(AudioMediaError::Backpressure)
    );
    assert!(matches!(
        decoder.submit_plc(480),
        Err(AudioMediaError::Backpressure)
    ));
    receipt.stop();
    drop(decoder);
    retired(&receipt);
    // Cancellation during setup keeps the same receipt even if no child has
    // yet been created. It cannot detach the eventual native launch owner.
    let (mut decoder, receipt) = ProcessDecoder::new(&image(), CodecLimits::ABSOLUTE).unwrap();
    decoder.configure(config).unwrap();
    receipt.stop();
    drop(decoder);
    retired(&receipt);
}

#[test]
fn invalid_paths_refuse_before_spawn_and_missing_image_retires() {
    let _serial = SERIAL.lock().unwrap();
    for path in [
        r"fr-opus-worker.exe",
        r"\\server\share\fr-opus-worker.exe",
        r"C:\app\..\worker.exe",
        r"C:\worker.exe:alternate",
    ] {
        assert!(ProcessDecoder::new(Path::new(path), CodecLimits::ABSOLUTE).is_err());
    }
    let missing = image().with_file_name(format!("missing-opus-worker-{}.exe", std::process::id()));
    let (mut decoder, receipt) = ProcessDecoder::new(&missing, CodecLimits::ABSOLUTE).unwrap();
    assert!(receipt.is_complete());
    decoder
        .configure(config(AudioChannels::Mono, AudioDirection::Downlink, 10))
        .unwrap();
    let until = Instant::now() + Duration::from_secs(3);
    loop {
        match decoder.poll_configured() {
            Err(_) => break,
            Ok(false) => {}
            Ok(true) => panic!("missing executable cannot become ready"),
        }
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(1));
    }
    drop(decoder);
    retired(&receipt);
}

#[test]
fn standalone_worker_refuses_unbound_standard_handles() {
    let _serial = SERIAL.lock().unwrap();
    let mut child = Command::new(image())
        .env_clear()
        .args(["--parent-pid", &std::process::id().to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let until = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert_eq!(status.code(), Some(70));
            return;
        }
        if Instant::now() >= until {
            let _ = child.kill();
            let _ = child.wait();
            panic!("unbound worker must refuse before waiting for codec input");
        }
        thread::sleep(Duration::from_millis(1));
    }
}
