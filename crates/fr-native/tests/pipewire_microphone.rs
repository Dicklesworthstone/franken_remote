#![cfg(all(target_os = "linux", feature = "linux-pipewire-mic"))]
//! Real native source tests, with a private daemon and independent recorder.
//! The ignored test is explicitly run by the native CI lane; missing tools fail.
use fr_core::audio::{AudioChannels, AudioDirection, AudioGeneration, AudioStreamConfig};
use fr_media::audio::AudioPcmFrame;
use fr_native::pipewire_mic::{Error, MAX_QUEUED_BLOCKS, Microphone, State};
use std::{
    fs::{self, File},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

fn configuration(generation: u64) -> AudioStreamConfig {
    AudioStreamConfig::new(
        AudioDirection::Uplink,
        AudioGeneration::from_raw(generation),
        AudioChannels::Mono,
        10,
        20,
    )
    .unwrap()
}

fn tone(generation: u64, timestamp: u64) -> AudioPcmFrame {
    let samples = std::array::from_fn::<_, 480, _>(|i| if i % 96 < 48 { 12_000 } else { -12_000 });
    AudioPcmFrame::from_interleaved(
        AudioGeneration::from_raw(generation),
        AudioChannels::Mono,
        timestamp,
        &samples,
    )
    .unwrap()
}

#[test]
fn permission_and_server_choice_are_explicit_before_native_setup() {
    let nonexistent = Path::new("/frankenremote-test-no-server/pipewire-0");
    assert_eq!(
        Microphone::connect(nonexistent, configuration(1), || false).unwrap_err(),
        Error::PermissionEnded
    );
    for invalid in ["pipewire-0", "/tmp/../pipewire-0"] {
        assert_eq!(
            Microphone::connect(Path::new(invalid), configuration(1), || true).unwrap_err(),
            Error::InvalidServer
        );
    }
    for (direction, duration) in [
        (AudioDirection::Downlink, 10),
        (AudioDirection::Uplink, 120),
    ] {
        let config = AudioStreamConfig::new(
            direction,
            AudioGeneration::from_raw(1),
            AudioChannels::Mono,
            duration,
            20,
        )
        .unwrap();
        assert_eq!(
            Microphone::connect(nonexistent, config, || true).unwrap_err(),
            Error::InvalidConfiguration
        );
    }
}

static NEXT: AtomicU64 = AtomicU64::new(0);
struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
struct Server {
    directory: PathBuf,
    socket: PathBuf,
    child: OwnedChild,
}
impl Server {
    fn start() -> Self {
        let directory = std::env::temp_dir().join(format!(
            "fr-mic-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let child = Command::new("pipewire")
            .env("XDG_RUNTIME_DIR", &directory)
            .env("PIPEWIRE_RUNTIME_DIR", &directory)
            .env_remove("PIPEWIRE_REMOTE")
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .stdin(Stdio::null())
            .stdout(File::create(directory.join("daemon.stdout")).unwrap())
            .stderr(File::create(directory.join("daemon.stderr")).unwrap())
            .spawn()
            .expect("native microphone qualification requires pipewire, no skip fallback");
        let mut server = Self {
            socket: directory.join("pipewire-0"),
            directory,
            child: OwnedChild(child),
        };
        let until = Instant::now() + Duration::from_secs(10);
        while !server.socket.exists() {
            assert!(server.child.0.try_wait().unwrap().is_none());
            assert!(Instant::now() < until, "private PipeWire startup deadline");
            std::thread::sleep(Duration::from_millis(2));
        }
        server
    }
    fn recorder(&self, node: u32) -> OwnedChild {
        // Include the repo-owned helper so the exact same test can also run
        // from an external dependency harness without pulling dev-dependencies.
        let script = self.directory.join("capture.py");
        fs::write(&script, include_str!("pipewire_microphone/capture.py")).unwrap();
        OwnedChild(
            Command::new("python3")
                .arg(script)
                .arg(&self.directory)
                .arg(node.to_string())
                .env("XDG_RUNTIME_DIR", &self.directory)
                .env("PIPEWIRE_RUNTIME_DIR", &self.directory)
                .env("PIPEWIRE_REMOTE", "pipewire-0")
                .stdin(Stdio::null())
                .stdout(File::create(self.directory.join("capture.stdout")).unwrap())
                .stderr(File::create(self.directory.join("capture.stderr")).unwrap())
                .spawn()
                .unwrap(),
        )
    }
}
fn ready(microphone: &mut Microphone) {
    while microphone.poll(|| true).unwrap() != State::Ready {
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
#[ignore = "requires private Unix sockets and PipeWire/pw-cat/pw-cli/pw-link/pw-dump; explicitly exercised in native CI"]
fn independent_capture_observes_pcm_expiry_terminal_stop_and_exact_cleanup() {
    qualify(false);
}

#[test]
#[ignore = "requires private Unix sockets and PipeWire capture tools; explicitly exercised in native CI"]
fn permission_revoked_during_a_native_turn_cannot_publish_queued_pcm() {
    qualify(true);
}

#[test]
#[ignore = "requires a private PipeWire daemon; explicitly exercised in native CI"]
fn a_panicking_permission_callback_fences_without_unwinding_through_c() {
    let server = Server::start();
    let mut microphone = Microphone::connect(&server.socket, configuration(1), || true).unwrap();
    ready(&mut microphone);
    let mut checks = 0;
    assert_eq!(
        microphone.poll(|| {
            checks += 1;
            assert!(checks < 2, "intentional native permission-callback panic");
            true
        }),
        Err(Error::PermissionEnded)
    );
    assert_eq!(checks, 2, "the failure must happen inside the native call");
    assert_eq!(microphone.state(), State::Closed);
    assert!(microphone.control().is_stopped());
}

fn validate_admission(microphone: &mut Microphone) -> u64 {
    let until = Instant::now() + Duration::from_millis(70);
    assert_eq!(
        microphone.submit(&tone(8, 0), until, || true),
        Err(Error::Generation)
    );
    let stereo = AudioPcmFrame::from_interleaved(
        AudioGeneration::from_raw(7),
        AudioChannels::Stereo,
        0,
        &[12_000; 960],
    )
    .unwrap();
    assert_eq!(
        microphone.submit(&stereo, until, || true),
        Err(Error::Format)
    );
    let mut timestamp = 0;
    for _ in 0..MAX_QUEUED_BLOCKS {
        microphone
            .submit(&tone(7, timestamp), until, || true)
            .unwrap();
        timestamp += 480;
    }
    assert_eq!(microphone.statistics().queued_frames, 1_920);
    assert_eq!(
        microphone.submit(&tone(7, timestamp), until, || true),
        Err(Error::Backpressure)
    );
    assert_eq!(
        microphone.submit(&tone(7, timestamp), until, || true),
        Err(Error::Timeline),
        "a refused sound cannot be replayed with refreshed custody"
    );
    timestamp += 480;
    std::thread::sleep(Duration::from_millis(85));
    microphone.poll(|| true).unwrap();
    assert_eq!(microphone.statistics().queued_frames, 0);
    assert_eq!(microphone.statistics().expired_frames, 1_920);

    timestamp
}

fn record_sound_and_expiry(
    server: &Server,
    microphone: &mut Microphone,
    mut timestamp: u64,
) -> (OwnedChild, u64) {
    let mut capture = server.recorder(microphone.statistics().node_id);
    let setup_deadline = Instant::now() + Duration::from_secs(10);
    while !server.directory.join("linked").exists() {
        microphone.poll(|| true).unwrap();
        assert!(
            capture.0.try_wait().unwrap().is_none(),
            "see retained capture.stderr"
        );
        assert!(
            Instant::now() < setup_deadline,
            "private recorder setup deadline"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    let started = Instant::now();
    let mut next_frame = started;
    while started.elapsed() < Duration::from_secs(3) {
        microphone.poll(|| true).unwrap();
        let now = Instant::now();
        if now >= next_frame {
            let result =
                microphone.submit(&tone(7, timestamp), now + Duration::from_millis(80), || {
                    true
                });
            assert!(matches!(result, Ok(()) | Err(Error::Backpressure)));
            timestamp += 480;
            // Skip missed wall time instead of manufacturing an old sound burst.
            next_frame = now + Duration::from_millis(10);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    // The independent recorder must observe silence after original deadlines,
    // despite continued calls that attempt to submit audible expired content.
    let silence = Instant::now();
    while silence.elapsed() < Duration::from_millis(800) {
        assert_eq!(
            microphone.submit(&tone(7, timestamp), Instant::now(), || true),
            Err(Error::Expired)
        );
        timestamp += 480;
        microphone.poll(|| true).unwrap();
        std::thread::sleep(Duration::from_millis(2));
    }
    (capture, timestamp)
}

fn qualify(revoke_in_callback: bool) {
    let mut server = Server::start();
    let mut microphone = Microphone::connect(&server.socket, configuration(7), || true).unwrap();
    ready(&mut microphone);
    let old_control = microphone.control();
    let timestamp = validate_admission(&mut microphone);
    let (mut capture, timestamp) = record_sound_and_expiry(&server, &mut microphone, timestamp);
    let before_stop = microphone.statistics();
    assert!(before_stop.submitted_frames > 48_000);
    assert!(before_stop.silent_frames > 12_000);
    // Stop from another thread with newly queued sound still unpolled.
    microphone
        .submit(
            &tone(7, timestamp),
            Instant::now() + Duration::from_millis(80),
            || true,
        )
        .unwrap();
    if revoke_in_callback {
        // The public poll entry still has permission; revoke at the fourth
        // check, after the callback has copied PCM and before OS submission.
        std::thread::sleep(Duration::from_millis(30));
        let mut checks = 0;
        assert_eq!(
            microphone.poll(|| {
                checks += 1;
                checks < 4
            }),
            Err(Error::PermissionEnded)
        );
        assert_eq!(checks, 4);
        assert!(
            microphone.statistics().silent_frames > before_stop.silent_frames,
            "the native callback must actually execute this rejection"
        );
    } else {
        let stop = old_control.clone();
        std::thread::spawn(move || stop.stop()).join().unwrap();
        assert_eq!(microphone.poll(|| true), Err(Error::PermissionEnded));
    }
    assert_eq!(microphone.state(), State::Closed);
    assert_eq!(microphone.statistics().queued_frames, 0);
    assert_eq!(
        microphone.statistics().submitted_frames,
        before_stop.submitted_frames
    );
    assert_eq!(
        microphone.submit(&tone(7, timestamp + 480), Instant::now(), || true),
        Err(Error::Closed)
    );
    fs::write(server.directory.join("finished"), []).unwrap();
    let completion_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = capture.0.try_wait().unwrap() {
            assert!(status.success(), "see retained capture.stderr");
            break;
        }
        assert!(
            Instant::now() < completion_deadline,
            "independent capture deadline"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    let evidence = fs::read_to_string(server.directory.join("capture.stdout")).unwrap();
    println!(
        "{}\nartifacts: {}",
        evidence.trim(),
        server.directory.display()
    );
    assert!(evidence.contains("independent_capture_passed"));

    // An old stop handle cannot fence a fresh original generation. A daemon
    // failure then closes that replacement instead of leaving a false Ready.
    let mut replacement = Microphone::connect(&server.socket, configuration(8), || true).unwrap();
    ready(&mut replacement);
    old_control.stop();
    assert_eq!(replacement.poll(|| true).unwrap(), State::Ready);
    assert!(!replacement.control().is_stopped());
    server.child.0.kill().unwrap();
    server.child.0.wait().unwrap();
    let failure_deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if replacement.poll(|| true).is_err() {
            assert_eq!(replacement.state(), State::Closed);
            break;
        }
        assert!(
            Instant::now() < failure_deadline,
            "daemon loss must fail closed"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}
