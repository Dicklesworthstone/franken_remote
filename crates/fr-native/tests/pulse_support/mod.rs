//! A real private `PulseAudio` daemon with a null output. No host sound device,
//! user server, default microphone, or network socket is opened by these tests.
use fr_client::input::ClientInstant;
use fr_native::pulse::{Error, PlaybackDevice, State};
use std::{
    fs::{self, File},
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};
static NEXT: AtomicU64 = AtomicU64::new(0);

pub struct Server {
    pub directory: PathBuf,
    pub socket: PathBuf,
    child: Child,
    origin: Instant,
}
impl Server {
    pub fn start() -> Self {
        Self::with_rewinds(false)
    }
    pub fn with_rewinds(rewinds: bool) -> Self {
        let directory = std::env::temp_dir().join(format!(
            "fr-pulse-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).expect("fresh private test directory");
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let socket = directory.join("native");
        let config = directory.join("daemon.pa");
        // Anonymous auth is confined to this 0700 synthetic fixture directory;
        // production always uses the installed server's normal authentication.
        fs::write(&config, format!("load-module module-native-protocol-unix socket={} auth-anonymous=1\nload-module module-null-sink sink_name=fr_test format=s16le rate=48000 channels=2 norewinds={}\n",socket.display(),u8::from(!rewinds))).unwrap();
        let stdout = File::create(directory.join("daemon.stdout")).unwrap();
        let stderr = File::create(directory.join("daemon.stderr")).unwrap();
        let mut command = Command::new(
            std::env::var_os("FR_PULSE_BINARY").unwrap_or_else(|| "pulseaudio".into()),
        );
        command
            .args([
                "--daemonize=no",
                "--use-pid-file=no",
                "--exit-idle-time=-1",
                "--disable-shm=yes",
                "--log-level=warning",
                "-nF",
            ])
            .arg(&config)
            .env("PULSE_RUNTIME_PATH", &directory)
            .env("XDG_RUNTIME_DIR", &directory)
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr);
        if let Some(modules) = std::env::var_os("FR_PULSE_MODULE_DIR") {
            command.arg("--dl-search-path").arg(modules);
        }
        if let Some(libraries) = std::env::var_os("FR_PULSE_LIBRARY_PATH") {
            command.env("LD_LIBRARY_PATH", libraries);
        }
        let child = command
            .spawn()
            .expect("real native tests require pulseaudio; no mock/skip fallback");
        let mut server = Self {
            directory,
            socket,
            child,
            origin: Instant::now(),
        };
        while !server.socket.exists() {
            assert!(
                server.child.try_wait().unwrap().is_none(),
                "private PulseAudio exited; retained daemon logs"
            );
            assert!(
                server.origin.elapsed() < Duration::from_secs(3),
                "private PulseAudio socket startup timed out"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        server
    }
    pub fn now(&self) -> ClientInstant {
        ClientInstant(u64::try_from(self.origin.elapsed().as_micros()).unwrap())
    }
    pub fn ready(&self, device: &mut PlaybackDevice) {
        let start = Instant::now();
        loop {
            if device.poll(|| Ok(self.now())).unwrap() == State::Ready {
                return;
            }
            assert!(start.elapsed() < Duration::from_secs(2));
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    pub fn service_until(&self, device: &mut PlaybackDevice, samples: u64) {
        let start = Instant::now();
        loop {
            device.poll(|| Ok(self.now())).unwrap();
            if device.clock(self.now()).unwrap().output_samples >= samples {
                return;
            }
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "native device clock failed to advance"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    pub fn kill(&mut self) {
        self.child.kill().unwrap();
        let _ = self.child.wait().unwrap();
    }
    pub fn capture(&self, mut service: impl FnMut()) -> Capture {
        let output = self.directory.join("synthetic-monitor.pcm");
        let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        // The verification runner supplies the exact checked-in script path;
        // normal package tests resolve it relative to the native crate.
        let script = std::env::var_os("FR_PULSE_CAPTURE_SCRIPT").map_or_else(
            || script.join("tests/pulse_support/capture.py"),
            PathBuf::from,
        );
        let child = Command::new("python3")
            .arg(script)
            .arg(&self.socket)
            .arg(&output)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(File::create(self.directory.join("capture.stderr")).unwrap())
            .spawn()
            .unwrap();
        let mut capture = Capture { child, output };
        let start = Instant::now();
        while !capture.output.with_extension("ready").exists() {
            service();
            assert!(
                capture.child.try_wait().unwrap().is_none(),
                "independent native monitor failed"
            );
            assert!(start.elapsed() < Duration::from_secs(2));
            std::thread::sleep(Duration::from_millis(1));
        }
        service();
        capture
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
pub struct Capture {
    child: Child,
    output: PathBuf,
}
impl Capture {
    pub fn finish(mut self) -> Vec<i16> {
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(3),
                "independent monitor timed out"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        let bytes = fs::read(&self.output).unwrap();
        assert!(bytes.len() <= 192_000, "fixture byte bound");
        bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect()
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn absent_server_refuses() -> Result<(), Error> {
    use fr_core::audio::{AudioChannels, AudioDirection, AudioGeneration, AudioStreamConfig};
    use fr_native::pulse::Selection;
    let config = AudioStreamConfig::new(
        AudioDirection::Downlink,
        AudioGeneration::INITIAL,
        AudioChannels::Stereo,
        10,
        20,
    )
    .unwrap();
    let selection = Selection::new(
        std::path::Path::new("/nonexistent/fr-pulse-missing-socket"),
        "fr_test",
    )
    .unwrap();
    let mut device = PlaybackDevice::connect(selection, config, ClientInstant(0))?;
    let result = device.poll(|| Ok(ClientInstant(1))).map(|_| ());
    device.disconnect();
    result
}
