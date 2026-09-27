#![cfg(all(target_os = "linux", feature = "linux-opus-process"))]
//! Actual restricted child and real libopus. Tones and local clocks are fixtures.
#[allow(dead_code)] // The shared oracle is used by opus_decoder, not here.
mod opus_support;
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
use fr_client::{
    audio::playout::{
        AudioPlayout, PlayoutClock, PlayoutError, PlayoutResult, decoder::PolledDecoder,
    },
    input::ClientInstant,
};
use fr_core::audio::{AudioChannels, AudioDirection, AudioStreamConfig};
use fr_media::audio::{
    AudioAccessUnit, AudioDecoder, AudioEncoder, AudioMediaError, AudioPcmFrame,
};
use fr_native::opus::{
    CodecLimits, Decoder, Encoder,
    process::{ProcessDecoder, Retirement},
};
use opus_support::{config, tone};
use std::{
    io::{Read, Write},
    os::{fd::OwnedFd, unix::net::UnixStream},
    path::PathBuf,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};
fn image() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fr-opus-worker"))
}
fn make(c: AudioStreamConfig) -> (ProcessDecoder, Retirement) {
    let (mut d, r) = ProcessDecoder::new(&image(), CodecLimits::ABSOLUTE).unwrap();
    d.configure(c).unwrap();
    let until = Instant::now() + Duration::from_secs(3);
    while !d
        .poll_configured()
        .expect("real child must configure under confinement")
    {
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(1));
    }
    (d, r)
}
fn packet(c: AudioStreamConfig, seq: u64) -> AudioAccessUnit {
    let mut e = Encoder::new();
    e.configure(c).unwrap();
    e.submit_pcm(&tone(
        c,
        1000 + seq * u64::from(c.expected_samples_per_frame()),
    ))
    .unwrap();
    let p = e.poll_packet().unwrap().unwrap();
    AudioAccessUnit::new(
        p.direction(),
        p.generation(),
        seq,
        p.timestamp_samples(),
        p.duration_samples(),
        false,
        p.payload(),
    )
    .unwrap()
}
fn collect(d: &mut ProcessDecoder) -> AudioPcmFrame {
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(p) = d.poll_pcm().expect("real child reply") {
            return p;
        }
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(1));
    }
}
fn retired(r: &Retirement) {
    let until = Instant::now() + Duration::from_secs(3);
    while !r.is_complete() {
        assert!(Instant::now() < until, "exact child must reap");
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(r.process_id(), None);
}
fn signal(r: &Retirement, name: &str) {
    assert!(
        Command::new("kill")
            .args([name, &r.process_id().unwrap().to_string()])
            .status()
            .unwrap()
            .success()
    );
}
#[test]
fn restricted_process_matches_native_decode_and_plc_for_all_profiles() {
    let _serial = SERIAL.lock().unwrap();
    for channels in [AudioChannels::Mono, AudioChannels::Stereo] {
        for duration in [5, 10, 20, 40, 60] {
            let c = config(channels, AudioDirection::Downlink, duration);
            let (mut d, r) = make(c);
            let mut oracle = Decoder::new();
            AudioDecoder::configure(&mut oracle, c).unwrap();
            let p = packet(c, 0);
            d.submit_packet(&p).unwrap();
            AudioDecoder::submit_packet(&mut oracle, &p).unwrap();
            assert_eq!(
                collect(&mut d).samples(),
                AudioDecoder::poll_pcm(&mut oracle)
                    .unwrap()
                    .unwrap()
                    .samples()
            );
            assert!(
                d.submit_plc(u16::try_from(c.expected_samples_per_frame()).unwrap())
                    .unwrap()
                    .is_none()
            );
            let expected = oracle
                .decode_plc(u16::try_from(c.expected_samples_per_frame()).unwrap())
                .unwrap();
            let actual = collect(&mut d);
            assert_eq!(actual.samples(), expected.samples());
            assert_eq!(actual.timestamp_samples(), expected.timestamp_samples());
            drop(d);
            retired(&r);
        }
    }
}
#[test]
fn frozen_decoder_never_blocks_the_playout_thread_or_retimes_pcm() {
    let _serial = SERIAL.lock().unwrap();
    let c = config(AudioChannels::Mono, AudioDirection::Downlink, 10);
    let (d, r) = ProcessDecoder::new(&image(), CodecLimits::ABSOLUTE).unwrap();
    let clk = |t, s| PlayoutClock {
        now: ClientInstant(t),
        output_samples: s,
    };
    let mut owner = AudioPlayout::new(c, d, clk(0, 0)).unwrap();
    while !owner.poll_configured(clk(0, 0)).unwrap() {
        thread::sleep(Duration::from_millis(1));
    }
    signal(&r, "-STOP");
    for n in 0..3 {
        owner.receive(packet(c, n), clk(0, 0)).unwrap();
    }
    let began = Instant::now();
    assert_eq!(
        owner.render(|| Ok(clk(20_000, 960)), |_, _| panic!("frozen child")),
        Ok(PlayoutResult::Waiting)
    );
    assert!(
        began.elapsed() < Duration::from_millis(40),
        "no foreign wait in caller"
    );
    assert_eq!(
        owner.render(|| Ok(clk(30_000, 1440)), |_, _| panic!("late child")),
        Err(PlayoutError::MissedDeviceSlot)
    );
    retired(&r); // SIGKILL terminates even the SIGSTOPed child; no continuation/retry.
}
#[test]
fn a_crashed_child_retires_and_never_restarts_itself() {
    let _serial = SERIAL.lock().unwrap();
    let c = config(AudioChannels::Stereo, AudioDirection::Downlink, 10);
    let (mut d, r) = make(c);
    signal(&r, "-KILL");
    d.submit_packet(&packet(c, 0)).unwrap();
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        if d.poll_pcm().is_err() {
            break;
        }
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(1));
    }
    assert!(d.poll_pcm().is_err());
    assert!(d.configure(c).is_err());
    drop(d);
    retired(&r);
}
#[test]
fn uncollected_work_cannot_queue_another_packet_and_stop_reaps_the_original() {
    let _serial = SERIAL.lock().unwrap();
    let c = config(AudioChannels::Mono, AudioDirection::Downlink, 20);
    let (mut d, r) = make(c);
    signal(&r, "-STOP");
    d.submit_packet(&packet(c, 0)).unwrap();
    assert_eq!(
        d.submit_packet(&packet(c, 1)),
        Err(AudioMediaError::Backpressure)
    );
    assert!(matches!(
        d.submit_plc(960),
        Err(AudioMediaError::Backpressure)
    ));
    drop(d);
    retired(&r);
}
#[test]
fn real_codec_rejects_hostile_payload_without_returning_pcm() {
    let _serial = SERIAL.lock().unwrap();
    let c = config(AudioChannels::Mono, AudioDirection::Downlink, 10);
    let (mut d, r) = make(c);
    let p = AudioAccessUnit::new(
        c.direction(),
        c.generation(),
        0,
        0,
        480,
        false,
        &[0xff, 0xff, 0xff],
    )
    .unwrap();
    d.submit_packet(&p).unwrap();
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        match d.poll_pcm() {
            Err(_) => break,
            Ok(None) => {}
            Ok(Some(_)) => panic!("invalid payload decoded"),
        }
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(1));
    }
    drop(d);
    retired(&r);
}
#[test]
fn construction_does_not_start_a_child_and_relative_images_refuse() {
    let _serial = SERIAL.lock().unwrap();
    assert!(
        ProcessDecoder::new(
            std::path::Path::new("fr-opus-worker"),
            CodecLimits::ABSOLUTE
        )
        .is_err()
    );
    let (d, r) = ProcessDecoder::new(&image(), CodecLimits::ABSOLUTE).unwrap();
    assert!(r.is_complete());
    assert_eq!(r.process_id(), None);
    drop(d);
    assert!(r.is_complete());
}
#[test]
fn confinement_forbids_files_sockets_and_new_processes() {
    const FLAG: &str = "FR_OPUS_SANDBOX_TEST_CHILD";
    let _serial = SERIAL.lock().unwrap();
    if std::env::var_os(FLAG).is_some() {
        fr_native::bind_parent(
            std::env::var("FR_OPUS_TEST_PARENT")
                .unwrap()
                .parse()
                .unwrap(),
        )
        .unwrap();
        fr_native::opus::process::child::confine().unwrap();
        assert_eq!(
            std::fs::File::open("/etc/passwd")
                .unwrap_err()
                .raw_os_error(),
            Some(1)
        );
        assert!(UnixStream::pair().is_err());
        assert!(Command::new("/bin/true").spawn().is_err());
        std::io::stdout().write_all(b"confined\n").unwrap();
        return;
    }
    let (mut peer, child) = UnixStream::pair().unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(4))).unwrap();
    let out: OwnedFd = child.try_clone().unwrap().into();
    let input: OwnedFd = child.into();
    let mut p = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "confinement_forbids_files_sockets_and_new_processes",
            "--nocapture",
        ])
        .env(FLAG, "1")
        .env("FR_OPUS_TEST_PARENT", std::process::id().to_string())
        .env("RUST_TEST_THREADS", "1")
        .stdin(Stdio::from(input))
        .stdout(Stdio::from(out))
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    // Libtest cannot finish its own post-test bookkeeping under the intentional
    // process-creation/descriptor restrictions. The native sentinel is evidence.
    let mut bytes = Vec::new();
    let mut b = [0; 256];
    while !bytes.windows(9).any(|v| v == b"confined\n") {
        let n = peer.read(&mut b).unwrap();
        assert_ne!(n, 0, "{}", String::from_utf8_lossy(&bytes));
        bytes.extend_from_slice(&b[..n]);
    }
    let _ = p.kill();
    p.wait().unwrap();
}

#[test]
fn decoder_watchdog_reaps_a_stalled_child_without_another_client_poll() {
    let _serial = SERIAL.lock().unwrap();
    let c = config(AudioChannels::Mono, AudioDirection::Downlink, 10);
    let (mut d, r) = make(c);
    signal(&r, "-STOP");
    d.submit_packet(&packet(c, 0)).unwrap();
    // Deliberately do not call poll_pcm or drop: the native supervisor owns
    // its original 100ms command deadline independently of session progress.
    retired(&r);
    assert!(d.poll_pcm().is_err());
}
#[test]
fn admission_and_retirement_are_bounded_and_cancelled_configuration_never_starts() {
    let _serial = SERIAL.lock().unwrap();
    let c = config(AudioChannels::Mono, AudioDirection::Downlink, 10);
    let (mut cancelled, r) = ProcessDecoder::new(&image(), CodecLimits::ABSOLUTE).unwrap();
    r.stop();
    assert!(cancelled.configure(c).is_err());
    assert_eq!(r.process_id(), None);
    assert!(r.is_complete());
    let workers: Vec<_> = (0..4).map(|_| make(c)).collect();
    let (mut fifth, _) = ProcessDecoder::new(&image(), CodecLimits::ABSOLUTE).unwrap();
    assert_eq!(fifth.configure(c), Err(AudioMediaError::Backpressure));
    for (d, r) in workers {
        drop(d);
        retired(&r);
    }
    let (d, r) = make(c);
    drop(d);
    retired(&r);
}
#[test]
fn failed_spawn_is_terminal_and_does_not_consume_a_retirement_slot() {
    let _serial = SERIAL.lock().unwrap();
    let c = config(AudioChannels::Mono, AudioDirection::Downlink, 10);
    let (mut d, r) =
        ProcessDecoder::new(std::path::Path::new("/etc/hosts"), CodecLimits::ABSOLUTE).unwrap();
    d.configure(c).unwrap();
    let until = Instant::now() + Duration::from_secs(3);
    loop {
        match d.poll_configured() {
            Err(_) => break,
            Ok(false) => {}
            Ok(true) => panic!("non-executable file configured"),
        }
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(1));
    }
    retired(&r);
    assert!(d.configure(c).is_err());
}
