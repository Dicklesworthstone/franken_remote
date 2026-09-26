//! Host playback audio through the shipped path (plan §15.4). An independent
//! libpulse-simple player writes a known sine into the HOST's private
//! `PulseAudio` null sink. `frd run --audio` (in-process, fixture tailnet) starts
//! the real `fr-media-worker --audio` child, which records that sink's monitor
//! and encodes Opus; packets cross real UDP/TLS/QUIC to the shipped
//! `fr connect --view-only --audio`, which decodes them with real libopus into
//! a SEPARATE private `PulseAudio` server. An independent recorder reads that
//! server's null-sink monitor and this test detects the tone frequency with a
//! Goertzel filter (bin energy against neighbouring bins).
//!
//! Scope: two private `PulseAudio` daemons with null sinks inside the serial
//! namespace, two Xvfb displays and software Opus. This is not a desktop
//! `PipeWire` session, a sound card or speakers, a live tailnet, or audibility
//! evidence; any latency printed here is only for that scope.
use super::real_media::{
    COLOUR, Xvfb, close_window, near, set_root, sibling, warp_pointer, window_pixels,
};
use super::shipped_client::{ClientApi, wait_for, wait_for_event};
use super::*;
use frd::host_run::{self, AudioOptions, Event, Options, Reporter, StopHandle};
use std::{
    io::{BufRead, BufReader, Read, Write},
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc,
    thread,
    time::Instant,
};

/// Goertzel over 20 ms blocks (960 samples at 48 kHz): 50 Hz bins, so both
/// tones sit exactly on a bin.
const BLOCK: usize = 960;
const WARMUP_HZ: f64 = 450.0;
const MARKER_HZ: f64 = 1050.0;
/// Neighbouring bins that must stay far below the detected tone.
const PROBES: [f64; 6] = [300.0, 600.0, 750.0, 900.0, 1200.0, 1500.0];

/// A private `PulseAudio` daemon with one null sink (its default sink).
struct Pulse {
    dir: PathBuf,
    socket: PathBuf,
    sink: &'static str,
    child: Child,
}
impl Pulse {
    fn start(sink: &'static str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = PathBuf::from(format!(
            "/run/fr-audio-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        let socket = dir.join("native");
        let config = dir.join("daemon.pa");
        // Anonymous auth is confined to this 0700 private namespace directory.
        fs::write(
            &config,
            format!(
                "load-module module-native-protocol-unix socket={} auth-anonymous=1\n\
                 load-module module-null-sink sink_name={sink} format=s16le rate=48000 channels=2 norewinds=1\n\
                 set-default-sink {sink}\n",
                socket.display()
            ),
        )
        .unwrap();
        let mut child = Command::new("pulseaudio")
            .args([
                "--daemonize=no",
                "--use-pid-file=no",
                "--exit-idle-time=-1",
                "--disable-shm=yes",
                "--log-level=warning",
                "-nF",
            ])
            .arg(&config)
            .envs(Self::env(&dir))
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(fs::File::create(dir.join("daemon.stderr")).unwrap())
            .spawn()
            .expect("real PulseAudio required; no mock or skip");
        let start = Instant::now();
        while !socket.exists() {
            assert!(
                child.try_wait().unwrap().is_none(),
                "private PulseAudio exited: {}",
                fs::read_to_string(dir.join("daemon.stderr")).unwrap_or_default()
            );
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "PulseAudio socket"
            );
            thread::sleep(Duration::from_millis(5));
        }
        Self {
            dir,
            socket,
            sink,
            child,
        }
    }
    /// Keep every libpulse client's cookie/runtime files in this directory.
    fn env(dir: &Path) -> [(&'static str, PathBuf); 4] {
        [
            ("HOME", dir.to_path_buf()),
            ("XDG_CONFIG_HOME", dir.to_path_buf()),
            ("XDG_RUNTIME_DIR", dir.to_path_buf()),
            ("PULSE_RUNTIME_PATH", dir.to_path_buf()),
        ]
    }
    fn server(&self) -> String {
        format!("unix:{}", self.socket.display())
    }
}
impl Drop for Pulse {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Independent libpulse-simple player on the HOST server: continuous blocks
/// of silence or a sine, switched by a stdin command (acknowledged).
struct Player {
    child: Child,
    input: ChildStdin,
    output: BufReader<std::process::ChildStdout>,
}
impl Player {
    const SCRIPT: &str = r#"
import ctypes as c, math, sys, threading
class Spec(c.Structure):
    _fields_ = [("format", c.c_int), ("rate", c.c_uint32), ("channels", c.c_uint8)]
class Attr(c.Structure):
    _fields_ = [(n, c.c_uint32) for n in ("maxlength", "tlength", "prebuf", "minreq", "fragsize")]
lib = c.CDLL("libpulse-simple.so.0")
lib.pa_simple_new.argtypes = [c.c_char_p, c.c_char_p, c.c_int, c.c_char_p, c.c_char_p,
                              c.POINTER(Spec), c.c_void_p, c.POINTER(Attr), c.POINTER(c.c_int)]
lib.pa_simple_new.restype = c.c_void_p
lib.pa_simple_write.argtypes = [c.c_void_p, c.c_void_p, c.c_size_t, c.POINTER(c.c_int)]
error = c.c_int()
spec = Spec(3, 48000, 2)
attr = Attr(0xffffffff, 3840, 0xffffffff, 0xffffffff, 0xffffffff)
s = lib.pa_simple_new(sys.argv[1].encode(), b"fr-test-player", 1, sys.argv[2].encode(),
                      b"synthetic-tone", c.byref(spec), None, c.byref(attr), c.byref(error))
assert s, "player connection failed"
state = {"hz": float(sys.argv[3])}
def commands():
    for line in sys.stdin:
        state["hz"] = float(line)
        print("ok", flush=True)
threading.Thread(target=commands, daemon=True).start()
phase, block = 0.0, (c.c_int16 * 960)()
print("ready", flush=True)
while True:
    hz = state["hz"]
    for i in range(480):
        v = int(9000 * math.sin(phase)) if hz > 0 else 0
        block[2 * i] = block[2 * i + 1] = v
        phase = (phase + 2 * math.pi * hz / 48000) % (2 * math.pi)
    if lib.pa_simple_write(s, block, c.sizeof(block), c.byref(error)) < 0:
        raise SystemExit("player write failed")
"#;
    fn start(pulse: &Pulse, hz: f64) -> Self {
        let mut child = Command::new("python3")
            .args(["-u", "-c", Self::SCRIPT, &pulse.server(), pulse.sink])
            .arg(hz.to_string())
            .envs(Pulse::env(&pulse.dir))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut player = Self {
            input: child.stdin.take().unwrap(),
            output: BufReader::new(child.stdout.take().unwrap()),
            child,
        };
        player.expect("ready");
        player
    }
    fn expect(&mut self, want: &str) {
        let mut line = String::new();
        self.output.read_line(&mut line).unwrap();
        assert_eq!(line.trim(), want, "tone player failed");
    }
    fn tone(&mut self, hz: f64) {
        writeln!(self.input, "{hz}").unwrap();
        self.input.flush().unwrap();
        self.expect("ok");
    }
}
impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Independent libpulse-simple recorder of the CLIENT sink's monitor. Each
/// 20 ms block is timestamped when this test receives it.
struct Recorder {
    child: Child,
    blocks: mpsc::Receiver<(Instant, Vec<f64>)>,
}
impl Recorder {
    const SCRIPT: &str = r#"
import ctypes as c, sys
class Spec(c.Structure):
    _fields_ = [("format", c.c_int), ("rate", c.c_uint32), ("channels", c.c_uint8)]
class Attr(c.Structure):
    _fields_ = [(n, c.c_uint32) for n in ("maxlength", "tlength", "prebuf", "minreq", "fragsize")]
lib = c.CDLL("libpulse-simple.so.0")
lib.pa_simple_new.argtypes = [c.c_char_p, c.c_char_p, c.c_int, c.c_char_p, c.c_char_p,
                              c.POINTER(Spec), c.c_void_p, c.POINTER(Attr), c.POINTER(c.c_int)]
lib.pa_simple_new.restype = c.c_void_p
lib.pa_simple_read.argtypes = [c.c_void_p, c.c_void_p, c.c_size_t, c.POINTER(c.c_int)]
error = c.c_int()
spec = Spec(3, 48000, 2)
attr = Attr(19200, 0, 0, 0, 3840)
s = lib.pa_simple_new(sys.argv[1].encode(), b"fr-test-recorder", 2, (sys.argv[2] + ".monitor").encode(),
                      b"synthetic-monitor", c.byref(spec), None, c.byref(attr), c.byref(error))
assert s, "recorder connection failed"
block = (c.c_int16 * 1920)()
out = sys.stdout.buffer
while True:
    if lib.pa_simple_read(s, block, c.sizeof(block), c.byref(error)) < 0:
        raise SystemExit("recorder read failed")
    out.write(bytes(block))
    out.flush()
"#;
    fn start(pulse: &Pulse) -> Self {
        let mut child = Command::new("python3")
            .args(["-u", "-c", Self::SCRIPT, &pulse.server(), pulse.sink])
            .envs(Pulse::env(&pulse.dir))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let (send, blocks) = mpsc::channel();
        thread::spawn(move || {
            let mut raw = [0_u8; BLOCK * 4];
            while stdout.read_exact(&mut raw).is_ok() {
                // Mono mix of the stereo s16le block; no samples are logged.
                let mono = raw
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|f| {
                        f64::midpoint(
                            f64::from(i16::from_le_bytes([f[0], f[1]])),
                            f64::from(i16::from_le_bytes([f[2], f[3]])),
                        )
                    })
                    .collect();
                if send.send((Instant::now(), mono)).is_err() {
                    break;
                }
            }
        });
        Self { child, blocks }
    }
    /// The first block (within `limit`) where `hz` dominates, with its time.
    fn await_tone(&self, hz: f64, limit: Duration) -> Option<Instant> {
        let until = Instant::now() + limit;
        while let Some(left) = until.checked_duration_since(Instant::now()) {
            match self.blocks.recv_timeout(left) {
                Ok((at, block)) if dominant(&block, hz) => return Some(at),
                Ok(_) => {}
                Err(_) => return None,
            }
        }
        None
    }
    /// Discard already recorded blocks.
    fn drain(&self) {
        while self.blocks.try_recv().is_ok() {}
    }
    /// Blocks recorded over `span` where `hz` dominates (expected: none).
    fn count_tone(&self, hz: f64, span: Duration) -> (usize, usize) {
        let until = Instant::now() + span;
        let (mut seen, mut total) = (0, 0);
        while let Some(left) = until.checked_duration_since(Instant::now()) {
            match self.blocks.recv_timeout(left) {
                Ok((_, block)) => {
                    total += 1;
                    seen += usize::from(dominant(&block, hz));
                }
                Err(_) => break,
            }
        }
        (seen, total)
    }
}
impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Goertzel power of `hz` over one block.
fn power(block: &[f64], hz: f64) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let coefficient = 2.0 * (2.0 * std::f64::consts::PI * hz / 48_000.0).cos();
    let (mut s1, mut s2) = (0.0, 0.0);
    for &x in block {
        let s0 = x + coefficient * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    s1 * s1 + s2 * s2 - coefficient * s1 * s2
}
/// `hz` is loud (well above codec/noise floor) and at least 30x every probe
/// bin and the other tone.
fn dominant(block: &[f64], hz: f64) -> bool {
    let target = power(block, hz);
    // A 9000-peak sine over 960 samples has Goertzel power ~(9000*480)^2/4.
    let loud = target > 1.0e11;
    let others = PROBES
        .iter()
        .chain([WARMUP_HZ, MARKER_HZ].iter())
        .filter(|&&f| (f - hz).abs() > 1.0)
        .map(|&f| power(block, f))
        .fold(0.0, f64::max);
    loud && target > 30.0 * others
}

/// libpulse in the env-cleared audio worker falls back to the passwd home
/// for its client cookie; keep that write inside this private mount
/// namespace instead of the real /root.
fn private_root_home() {
    let status = fs::read_to_string("/proc/self/status").unwrap();
    let root = status
        .lines()
        .find(|l| l.starts_with("Uid:"))
        .is_some_and(|l| l.split_whitespace().nth(1) == Some("0"));
    if root
        && !fs::read_to_string("/proc/self/mounts")
            .unwrap()
            .lines()
            .any(|l| l.split_whitespace().nth(1) == Some("/root"))
    {
        assert!(
            Command::new("mount")
                .args(["-t", "tmpfs", "-o", "mode=0700", "tmpfs", "/root"])
                .status()
                .unwrap()
                .success()
        );
    }
}

/// The `frd run` composition of `real_media.rs`, with an optional audio enable.
fn options(
    api: &fixture::Api,
    tools: &Tools,
    worker: &Path,
    display: &str,
    roots: &Path,
    audio: Option<AudioOptions>,
) -> Options {
    Options {
        socket: Some(api.path.clone()),
        port: address().port(),
        interface: "fr-fixture".into(),
        worker: worker.to_path_buf(),
        display: display.to_owned(),
        xauthority: None,
        trust_roots: roots.to_path_buf(),
        sharing: fr_tailnet::Scope::OwnUser,
        fps: 15,
        bitrate: 2_000_000,
        ingress_tools: Some((tools.0.join("nft"), tools.0.join("ip"))),
        once: false,
        handle_signals: false,
        input_agent: None,
        clipboard: false,
        audio,
        files: None,
    }
}

/// The shipped client, view-only with `--audio`, playing into `pulse`.
fn connect_audio(
    fr: &Path,
    api: &Path,
    roots: &Path,
    display: &str,
    worker: &Path,
    pulse: &Pulse,
) -> Child {
    let port = address().port().to_string();
    Command::new(fr)
        .args([
            "connect",
            "n-host",
            "--view-only",
            "--audio",
            "--experimental-native",
        ])
        .args(["--display", "only", "--attempts", "1", "--json"])
        .arg("--socket")
        .arg(api)
        .arg("--trust-roots")
        .arg(roots)
        .args(["--port", &port, "--x-display", display])
        .arg("--worker")
        .arg(worker)
        .env_remove("DISPLAY")
        .env_remove("XAUTHORITY")
        .envs(Pulse::env(&pulse.dir))
        .env("PULSE_SERVER", pulse.server())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

/// The viewer window showing the host colour; `None` if `fr` exited first
/// (its output is still collected and reported) or time ran out.
fn await_window(display: &str, client: &mut Child) -> Option<u64> {
    let until = Instant::now() + Duration::from_secs(45);
    while Instant::now() < until && client.try_wait().unwrap().is_none() {
        if let Some((window, _)) = window_pixels(display)
            .into_iter()
            .find(|(_, pixel)| near(*pixel, COLOUR))
        {
            return Some(window);
        }
        thread::sleep(Duration::from_millis(250));
    }
    None
}

/// One complete share: host daemon, audio client, `observe`, clean stop.
fn share(
    host_pulse: &Pulse,
    client_pulse: &Pulse,
    audio: Option<AudioOptions>,
    observe: impl FnOnce(&Recorder) -> Result<(), String>,
) -> (serde_json::Value, Result<(), String>, String) {
    private_root_home();
    let (fr, worker) = (sibling("fr"), sibling("fr-media-worker"));
    let host = Xvfb::start("640x480x24");
    let viewer = Xvfb::start("800x600x24");
    set_root(&host.display, COLOUR);
    // The viewer draws the forwarded host cursor; keep it off the sampled centre.
    warp_pointer(&host.display, 16, 16);
    let recorder = Recorder::start(client_pulse);
    let api = fixture::Api::new();
    let tools = Tools::new();
    let roots = fixture::pki().join("ca.pem");
    let options = options(&api, &tools, &worker, &host.display, &roots, audio);
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let report: Reporter = Arc::new(move |event| sink.lock().unwrap().push(event));
    let stop = Arc::new(StopHandle::default());
    let host_stop = stop.clone();
    let daemon = thread::spawn(move || host_run::run(&options, &report, &host_stop));
    let dump = || format!("{:?}", events.lock().unwrap());
    assert!(
        wait_for_event(&events, Duration::from_secs(20), |e| matches!(
            e,
            Event::Listening { .. }
        )),
        "frd never listened: {}",
        dump()
    );
    let client_api = ClientApi::new();
    let mut client = connect_audio(
        &fr,
        &client_api.path,
        &roots,
        &viewer.display,
        &worker,
        client_pulse,
    );
    let window = await_window(&viewer.display, &mut client);
    let observed = match window {
        Some(_) => observe(&recorder),
        None => Err("no host picture reached the viewer window".into()),
    };
    if let Some(window) = window {
        close_window(&viewer.display, window);
    } else if client.try_wait().unwrap().is_none() {
        let _ = Command::new("kill")
            .args(["-INT", &client.id().to_string()])
            .status();
    }
    let output = wait_for(client, Duration::from_secs(30));
    stop.request();
    let result = daemon.join().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let diagnostics = format!(
        "fr: {stdout} {}; host: {}; host pulse: {}",
        String::from_utf8_lossy(&output.stderr),
        dump(),
        fs::read_to_string(host_pulse.dir.join("daemon.stderr")).unwrap_or_default()
    );
    let completion: serde_json::Value =
        serde_json::from_str(stdout.trim()).unwrap_or(serde_json::Value::Null);
    assert_eq!(result, Ok(()), "{diagnostics}");
    let events = events.lock().unwrap();
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::CleanupFailed { .. })),
        "{events:?}"
    );
    assert!(matches!(events.last(), Some(Event::Stopped)), "{events:?}");
    (completion, observed, diagnostics)
}

#[test]
#[ignore = "explicit isolated namespace; synthetic ingress; two Xvfb and two private PulseAudio daemons"]
fn fr_connect_audio_plays_the_host_playback_tone_through_frd_run_audio() {
    // Capture and Opus run in the separate worker process: neither the frd
    // binary nor this in-process host links the audio server library or codec.
    let linked = |image: &Path| {
        let output = Command::new("ldd").arg(image).output().unwrap();
        assert!(output.status.success(), "ldd {}", image.display());
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    for image in [sibling("frd"), std::env::current_exe().unwrap()] {
        let libraries = linked(&image);
        assert!(
            !libraries.contains("libpulse") && !libraries.contains("libopus"),
            "{} links audio libraries",
            image.display()
        );
    }
    let worker = linked(&sibling("fr-media-worker"));
    assert!(worker.contains("libpulse.so.0") && worker.contains("libopus.so.0"));
    let host_pulse = Pulse::start("fr_host");
    let client_pulse = Pulse::start("fr_view");
    let mut player = Player::start(&host_pulse, WARMUP_HZ);
    // A reference recorder on the HOST sink's own monitor separates the
    // local player/sink/recorder delay from the FrankenRemote path.
    let reference = Recorder::start(&host_pulse);
    let mut latency = None;
    let mut sustained = None;
    let (completion, observed, diagnostics) = share(
        &host_pulse,
        &client_pulse,
        // Default sink's monitor on the host's own (private) server.
        Some(AudioOptions {
            server: host_pulse.socket.clone(),
            sink: None,
        }),
        |recorder| {
            // The warm-up tone proves the whole pipeline is live.
            recorder
                .await_tone(WARMUP_HZ, Duration::from_secs(20))
                .ok_or("warm-up tone never reached the client's output")?;
            reference
                .await_tone(WARMUP_HZ, Duration::from_secs(5))
                .ok_or("warm-up tone absent from the host's own monitor")?;
            // Then a frequency change at a known instant: steady-state delay.
            recorder.drain();
            reference.drain();
            let changed = Instant::now();
            player.tone(MARKER_HZ);
            let local = reference
                .await_tone(MARKER_HZ, Duration::from_secs(5))
                .ok_or("marker tone absent from the host's own monitor")?;
            let heard = recorder
                .await_tone(MARKER_HZ, Duration::from_secs(5))
                .ok_or("marker tone not detected within 5 s")?;
            latency = Some((local.duration_since(changed), heard.duration_since(changed)));
            // Sustained real-time playback, not a burst: over two seconds at
            // least 80% of the client's 20 ms monitor blocks carry the tone.
            let (seen, total) = recorder.count_tone(MARKER_HZ, Duration::from_secs(2));
            if total < 90 || seen * 5 < total * 4 {
                return Err(format!("sustained tone in only {seen} of {total} blocks"));
            }
            sustained = Some((seen, total));
            Ok(())
        },
    );
    assert_eq!(observed, Ok(()), "{diagnostics}");
    assert_eq!(completion["outcome"], "stopped", "{diagnostics}");
    assert_eq!(completion["audio_requested"], true, "{diagnostics}");
    assert_eq!(completion["audio_active"], true, "{diagnostics}");
    // At least the two sustained seconds (100 frames of 20 ms) were submitted.
    assert!(
        completion["audio_frames_submitted"].as_u64().unwrap_or(0) >= 100,
        "{diagnostics}"
    );
    assert_eq!(completion["audibility_proven"], false, "{diagnostics}");
    let (local, heard) = latency.unwrap();
    // Scope: private PulseAudio null sinks in this namespace, Xvfb, software
    // Opus, 20 ms blocks (the measurement's own resolution), shared host.
    eprintln!(
        "audio_tone_change_host_monitor_ms={} audio_tone_change_client_monitor_ms={} \
         (scope: private PulseAudio null sinks, serial namespace, software Opus, 20 ms \
         detection blocks, same-machine clock; not speakers or a live tailnet)",
        local.as_millis(),
        heard.as_millis()
    );
    let (seen, total) = sustained.unwrap();
    eprintln!("audio_sustained_marker_blocks={seen}/{total} (20 ms client monitor blocks)");
    eprintln!("fr completion: {completion}");
    assert!(heard < Duration::from_secs(5), "{heard:?}");
}

#[test]
#[ignore = "explicit isolated namespace; synthetic ingress; two Xvfb and two private PulseAudio daemons"]
fn a_host_without_audio_reports_typed_absence_and_the_client_hears_no_tone() {
    let host_pulse = Pulse::start("fr_host");
    let client_pulse = Pulse::start("fr_view");
    let _player = Player::start(&host_pulse, WARMUP_HZ);
    let (completion, observed, diagnostics) = share(&host_pulse, &client_pulse, None, |recorder| {
        let (seen, total) = recorder.count_tone(WARMUP_HZ, Duration::from_secs(4));
        if total < 100 {
            return Err(format!("recorder produced only {total} blocks"));
        }
        if seen > 0 {
            return Err(format!("{seen} of {total} blocks carried the host tone"));
        }
        Ok(())
    });
    eprintln!("fr completion (host without --audio): {completion}");
    assert_eq!(observed, Ok(()), "{diagnostics}");
    assert_eq!(completion["outcome"], "stopped", "{diagnostics}");
    assert_eq!(completion["audio_requested"], true, "{diagnostics}");
    assert_eq!(completion["audio_active"], false, "{diagnostics}");
    assert_eq!(completion["audio_frames_submitted"], 0, "{diagnostics}");
    assert_eq!(
        completion["audio_absence"], "host_did_not_offer",
        "{diagnostics}"
    );
}

#[test]
#[ignore = "explicit isolated namespace; synthetic ingress; two Xvfb and two private PulseAudio daemons"]
fn a_missing_host_monitor_is_a_typed_stop_while_video_continues() {
    let host_pulse = Pulse::start("fr_host");
    let client_pulse = Pulse::start("fr_view");
    let _player = Player::start(&host_pulse, WARMUP_HZ);
    let (completion, observed, diagnostics) = share(
        &host_pulse,
        &client_pulse,
        Some(AudioOptions {
            server: host_pulse.socket.clone(),
            sink: Some("no_such_sink".into()),
        }),
        |recorder| {
            let (seen, _) = recorder.count_tone(WARMUP_HZ, Duration::from_secs(4));
            if seen > 0 {
                return Err(format!(
                    "{seen} blocks carried a tone from a missing monitor"
                ));
            }
            Ok(())
        },
    );
    eprintln!("fr completion (missing host monitor): {completion}");
    // The picture arrived (share() requires it) and the session stopped cleanly.
    assert_eq!(observed, Ok(()), "{diagnostics}");
    assert_eq!(completion["outcome"], "stopped", "{diagnostics}");
    assert_eq!(completion["audio_active"], false, "{diagnostics}");
    assert_eq!(
        completion["audio_absence"], "host_audio_unavailable",
        "{diagnostics}"
    );
}
