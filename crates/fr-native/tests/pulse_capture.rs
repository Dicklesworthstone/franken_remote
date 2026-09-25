#![cfg(all(target_os = "linux", feature = "linux-audio"))]
//! Actual playback-monitor capture: an independent libpulse-simple player
//! writes a sine into a private null sink; `PlaybackSource` records that sink's
//! monitor and encodes real Opus; an independent native decoder decodes the
//! packets and a Goertzel filter finds the tone. Null-sink server evidence
//! only: no sound card, desktop session, network or latency claim.
#[allow(dead_code)] // Shared real-server helper also serves playback tests.
mod pulse_support;
use fr_core::audio::{AudioChannels, AudioDirection, AudioGeneration, AudioStreamConfig};
use fr_media::{
    audio::{AudioDecoder, AudioMediaError},
    worker::audio::{Capture, Monitor},
};
use fr_native::{
    opus::Decoder,
    pulse::{Error, source::PlaybackSource},
};
use pulse_support::Server;
use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

const PLAYER: &str = r#"
import ctypes as c, math, sys
class Spec(c.Structure):
    _fields_ = [("format", c.c_int), ("rate", c.c_uint32), ("channels", c.c_uint8)]
lib = c.CDLL("libpulse-simple.so.0")
lib.pa_simple_new.argtypes = [c.c_char_p, c.c_char_p, c.c_int, c.c_char_p, c.c_char_p,
                              c.POINTER(Spec), c.c_void_p, c.c_void_p, c.POINTER(c.c_int)]
lib.pa_simple_new.restype = c.c_void_p
lib.pa_simple_write.argtypes = [c.c_void_p, c.c_void_p, c.c_size_t, c.POINTER(c.c_int)]
error = c.c_int()
s = lib.pa_simple_new(("unix:" + sys.argv[1]).encode(), b"fr-test-player", 1, b"fr_test",
                      b"synthetic-tone", c.byref(Spec(3, 48000, 2)), None, None, c.byref(error))
assert s
block = (c.c_int16 * 960)()
phase = 0.0
print("ready", flush=True)
while True:
    for i in range(480):
        block[2 * i] = block[2 * i + 1] = int(9000 * math.sin(phase))
        phase = (phase + 2 * math.pi * 1000 / 48000) % (2 * math.pi)
    if lib.pa_simple_write(s, block, c.sizeof(block), c.byref(error)) < 0:
        raise SystemExit("write failed")
"#;
struct Player(Child);
impl Player {
    fn start(server: &Server) -> Self {
        let mut child = Command::new("python3")
            .args(["-u", "-c", PLAYER])
            .arg(&server.socket)
            .env("HOME", &server.directory)
            .env("XDG_CONFIG_HOME", &server.directory)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert_eq!(line.trim(), "ready");
        Self(child)
    }
}
impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn capture(server: &Server, monitor: Monitor) -> Capture {
    Capture {
        generation: AudioGeneration::from_raw(7),
        channels: AudioChannels::Stereo,
        frame_duration_ms: 20,
        bitrate: 96_000,
        max_packet_bytes: 1000,
        server: server.socket.to_str().unwrap().to_owned(),
        monitor,
    }
}
fn micros(origin: Instant) -> u64 {
    u64::try_from(origin.elapsed().as_micros()).unwrap()
}
fn power(block: &[f64], hz: f64) -> f64 {
    let k = 2.0 * (2.0 * std::f64::consts::PI * hz / 48_000.0).cos();
    let (mut s1, mut s2) = (0.0, 0.0);
    for &x in block {
        let s0 = x + k * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    s1 * s1 + s2 * s2 - k * s1 * s2
}

#[test]
fn monitor_capture_records_real_time_and_encodes_the_played_tone() {
    let server = Server::start();
    let _player = Player::start(&server);
    let origin = Instant::now();
    let mut source =
        PlaybackSource::connect(capture(&server, Monitor::DefaultSink), micros(origin)).unwrap();
    while !source.poll_ready(micros(origin)).unwrap() {
        assert!(origin.elapsed() < Duration::from_secs(2), "record startup");
        std::thread::sleep(Duration::from_millis(1));
    }
    let start = Instant::now();
    let mut packets = Vec::new();
    while start.elapsed() < Duration::from_millis(1500) {
        let (batch, _) = source.pull(micros(origin)).unwrap();
        assert!(batch.len() <= fr_media::worker::audio::MAX_BATCH_PACKETS);
        packets.extend(batch);
        std::thread::sleep(Duration::from_millis(10));
    }
    let elapsed = start.elapsed();
    // Real-time capture: one 20 ms packet per 20 ms, within a small startup
    // and scheduling margin. Consecutive sequence, non-overlapping timeline.
    let expected = elapsed.as_millis() / 20;
    let got = u128::try_from(packets.len()).unwrap();
    assert!(
        got + 8 >= expected && got <= expected + 8,
        "{got} packets in {elapsed:?}"
    );
    for pair in packets.windows(2) {
        assert_eq!(pair[1].sequence(), pair[0].sequence() + 1);
        assert!(pair[1].timestamp_samples() >= pair[0].timestamp_samples() + 960);
        assert_eq!(pair[1].generation(), AudioGeneration::from_raw(7));
        assert_eq!(pair[1].direction(), AudioDirection::Downlink);
    }
    // Independent native decode of the captured stream finds the tone.
    let mut decoder = Decoder::new();
    decoder
        .configure(
            AudioStreamConfig::new(
                AudioDirection::Downlink,
                AudioGeneration::from_raw(7),
                AudioChannels::Stereo,
                20,
                20,
            )
            .unwrap(),
        )
        .unwrap();
    let mut toned = 0;
    for packet in &packets {
        decoder.submit_packet(packet).unwrap();
        let pcm = decoder.poll_pcm().unwrap().unwrap();
        let mono: Vec<f64> = pcm
            .samples()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|s| f64::midpoint(f64::from(s[0]), f64::from(s[1])))
            .collect();
        let tone = power(&mono, 1000.0);
        if tone > 1.0e11
            && [600.0, 800.0, 1200.0, 1500.0]
                .iter()
                .all(|&f| tone > 30.0 * power(&mono, f))
        {
            toned += 1;
        }
    }
    assert!(
        toned * 2 > packets.len(),
        "{toned} of {} packets carried the tone",
        packets.len()
    );
    source.disconnect();
}

#[test]
fn a_missing_monitor_or_server_is_a_typed_refusal() {
    let server = Server::start();
    let origin = Instant::now();
    let mut missing =
        PlaybackSource::connect(capture(&server, Monitor::Sink("absent".into())), 0).unwrap();
    let result = loop {
        match missing.poll_ready(micros(origin)) {
            Ok(false) => std::thread::sleep(Duration::from_millis(1)),
            other => break other,
        }
    };
    assert_eq!(result, Err(Error::Unavailable));
    let mut absent = capture(&server, Monitor::DefaultSink);
    absent.server = "/nonexistent/fr-pulse-missing".into();
    let error = PlaybackSource::connect(absent, 0).and_then(|mut s| {
        loop {
            match s.poll_ready(micros(origin)) {
                Ok(false) => std::thread::sleep(Duration::from_millis(1)),
                Ok(true) => return Ok(()),
                Err(e) => return Err(e),
            }
        }
    });
    assert_eq!(error, Err(Error::Unavailable));
    // Profiles outside the qualified set refuse before any connection.
    let mut five = capture(&server, Monitor::DefaultSink);
    five.frame_duration_ms = 5;
    assert_eq!(
        PlaybackSource::connect(five, 0).err(),
        Some(Error::Configuration)
    );
    let _ = AudioMediaError::NotConfigured;
}
