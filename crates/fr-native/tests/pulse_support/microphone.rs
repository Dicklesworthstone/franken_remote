//! Shared real private recording-source fixture. Synthetic PCM is supplied by
//! an independent process; recording/Opus boundaries remain production code.
use super::Server;
use std::{
    fs,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

const FEEDER: &str = r"
import array, math, os, pathlib, sys, time
path, ready = map(pathlib.Path, sys.argv[1:])
fd = os.open(path, os.O_WRONLY | os.O_NONBLOCK)
ready.touch()
phase = 0
try:
    while True:
        block = array.array('h', [round(9000 * math.sin(2 * math.pi * (phase + i) / 48))
                                  for i in range(480)])
        if sys.byteorder != 'little':
            block.byteswap()
        try:
            os.write(fd, block.tobytes())
        except BlockingIOError:
            pass
        phase = (phase + 480) % 48
        time.sleep(.01)
finally:
    os.close(fd)
";
pub struct Feeder(Child);
impl Feeder {
    pub fn start(server: &Server) -> Self {
        let input = server.directory.join("synthetic-source.pcm");
        let output = Command::new("pactl")
            .args([
                "--server",
                &format!("unix:{}", server.socket.display()),
                "load-module",
                "module-pipe-source",
                "source_name=fr_test_mic",
                "format=s16le",
                "rate=48000",
                "channels=1",
            ])
            .arg(format!("file={}", input.display()))
            .output()
            .expect("native qualification requires pactl");
        assert!(
            output.status.success(),
            "private pipe source: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let ready = server.directory.join("feeder-ready");
        let child = Command::new("python3")
            .args(["-u", "-c", FEEDER])
            .arg(&input)
            .arg(&ready)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(fs::File::create(server.directory.join("feeder.stderr")).unwrap())
            .spawn()
            .unwrap();
        let mut feeder = Self(child);
        let until = Instant::now() + Duration::from_secs(3);
        while !ready.exists() {
            assert!(
                feeder.0.try_wait().unwrap().is_none(),
                "see retained feeder.stderr"
            );
            assert!(Instant::now() < until, "independent pipe-source startup");
            std::thread::sleep(Duration::from_millis(1));
        }
        feeder
    }
}
impl Drop for Feeder {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
pub fn assert_recording_stopped(server: &Server) {
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        let output = Command::new("pactl")
            .args([
                "--server",
                &format!("unix:{}", server.socket.display()),
                "list",
                "short",
                "source-outputs",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "independent recording-stream inventory failed"
        );
        if output.stdout.is_empty() {
            break;
        }
        assert!(
            Instant::now() < until,
            "recording stream survived its original owner"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let output = Command::new("pactl")
        .args([
            "--server",
            &format!("unix:{}", server.socket.display()),
            "list",
            "short",
            "sources",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("fr_test_mic"),
        "capture cleanup must not remove the selected device"
    );
}
pub fn power(samples: &[i16], hz: f64) -> f64 {
    let coefficient = 2.0 * (2.0 * std::f64::consts::PI * hz / 48_000.0).cos();
    let (mut previous, mut older) = (0.0, 0.0);
    for &value in samples {
        let next = f64::from(value) + coefficient * previous - older;
        older = previous;
        previous = next;
    }
    previous * previous + older * older - coefficient * previous * older
}
