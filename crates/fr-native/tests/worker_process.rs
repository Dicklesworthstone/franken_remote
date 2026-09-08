#![cfg(all(target_os = "linux", feature = "linux-media"))]
use fr_core::{ids::CodecConfigurationGeneration, limits::ProtocolLimits};
use fr_media::{access_unit::FrameId, worker::*};
use fr_native::{BgraFrame, X11Surface};
use std::{
    io::{BufRead, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};

struct Display {
    child: Child,
    name: String,
}
impl Display {
    fn start() -> Self {
        let mut child = Command::new("Xvfb")
            .args([
                "-displayfd",
                "1",
                "-screen",
                "0",
                "320x240x24",
                "-nolisten",
                "tcp",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("Xvfb is a native test prerequisite");
        let mut name = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut name)
            .unwrap();
        let number = name.trim().parse::<u32>().expect("Xvfb displayfd response");
        Self {
            child,
            name: format!(":{number}"),
        }
    }
}
impl Drop for Display {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
struct Worker {
    child: Child,
    input: ChildStdin,
    output: ChildStdout,
    next: u64,
}
impl Worker {
    fn start(display: &Display, role: Role) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_fr-media-worker"));
        cmd.env_clear()
            .env("DISPLAY", &display.name)
            .arg(match role {
                Role::Capture => "--capture",
                Role::Present => "--present",
            });
        cmd.arg("--parent-pid").arg(std::process::id().to_string());
        if role == Role::Present {
            cmd.arg("--verify-readback");
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        Self {
            input: child.stdin.take().unwrap(),
            output: child.stdout.take().unwrap(),
            child,
            next: 0,
        }
    }
    fn transact(&mut self, kind: Kind, body: Vec<u8>) -> Record {
        let identity = Identity {
            epoch: 7,
            sequence: self.next,
        };
        self.next += 1;
        Record::new(kind, identity, body, &ProtocolLimits::ABSOLUTE)
            .unwrap()
            .write(&mut self.input, &ProtocolLimits::ABSOLUTE)
            .unwrap();
        let response = Record::read(&mut self.output, &ProtocolLimits::ABSOLUTE)
            .unwrap()
            .unwrap();
        assert_eq!(response.header.identity, identity);
        response
    }
    fn configure(&mut self) {
        assert_eq!(
            self.transact(Kind::Configure, configuration().encode().unwrap())
                .header
                .kind,
            Kind::Ready
        );
    }
    fn stop(&mut self) {
        assert_eq!(self.transact(Kind::Stop, vec![]).header.kind, Kind::Stopped);
        assert!(self.child.wait().unwrap().success());
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn configuration() -> Configuration {
    Configuration {
        width: 320,
        height: 240,
        fps: 30,
        backend: Backend::SoftwareExplicit,
        bitrate: 2_000_000,
        max_access_unit_bytes: 1024 * 1024,
        generation: CodecConfigurationGeneration::INITIAL,
    }
}

#[test]
fn distinct_processes_capture_encode_decode_and_present_changing_desktop() {
    let source_display = Display::start();
    let viewer_display = Display::start();
    let limits = configuration().limits().unwrap();
    let mut source = X11Surface::presenter(Some(&source_display.name), 320, 240, limits).unwrap();
    let mut capture = Worker::start(&source_display, Role::Capture);
    capture.configure();
    let mut viewer = Worker::start(&viewer_display, Role::Present);
    viewer.configure();
    assert_ne!(capture.child.id(), viewer.child.id());
    assert_ne!(capture.child.id(), std::process::id());
    let mut observed = X11Surface::capture(Some(&viewer_display.name), limits).unwrap();
    let mut previous = None;
    for frame in 0..8_u64 {
        let mut pixels = vec![0; 320 * 240 * 4];
        for (index, p) in pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            p.copy_from_slice(&[
                u8::try_from(index % 200).unwrap(),
                40,
                u8::try_from(frame * 25).unwrap(),
                255,
            ]);
        }
        source
            .present(&BgraFrame::new(320, 240, pixels, &limits).unwrap())
            .unwrap();
        let response = capture.transact(
            Kind::Capture,
            capture_payload(FrameId::from_raw(frame), frame * 33_333, frame == 4),
        );
        assert_eq!(response.header.kind, Kind::Unit);
        let unit = parse_unit(response.into_body(), &limits).unwrap();
        assert_eq!(unit.frame().as_raw(), frame);
        assert_eq!(unit.is_idr(), frame == 0 || frame == 4);
        let shown = viewer.transact(Kind::Present, unit_payload(&unit).unwrap());
        assert_eq!(shown.header.kind, Kind::Presented);
        assert_eq!(shown.body(), frame.to_be_bytes());
        let current = observed.snapshot().unwrap();
        if let Some(old) = previous {
            assert_ne!(current.pixels(), old, "presentation did not change");
        }
        previous = Some(current.pixels().to_vec());
    }
    capture.stop();
    viewer.stop();
}
#[test]
fn wrong_role_refuses_before_capture_and_exits() {
    let display = Display::start();
    let mut viewer = Worker::start(&display, Role::Present);
    viewer.configure();
    let response = viewer.transact(Kind::Capture, capture_payload(FrameId::FIRST, 0, false));
    assert_eq!(response.header.kind, Kind::Refused);
    assert_eq!(response.body(), (Error::WrongRole as u16).to_be_bytes());
    assert!(!viewer.child.wait().unwrap().success());
}
#[test]
fn repeated_configuration_and_wrong_epoch_are_terminal() {
    let display = Display::start();
    let mut worker = Worker::start(&display, Role::Present);
    worker.configure();
    assert_eq!(
        worker
            .transact(Kind::Configure, configuration().encode().unwrap())
            .header
            .kind,
        Kind::Refused
    );
    assert!(!worker.child.wait().unwrap().success());
    let mut worker = Worker::start(&display, Role::Present);
    worker.configure();
    Record::new(
        Kind::Stop,
        Identity {
            epoch: 8,
            sequence: 1,
        },
        vec![],
        &ProtocolLimits::ABSOLUTE,
    )
    .unwrap()
    .write(&mut worker.input, &ProtocolLimits::ABSOLUTE)
    .unwrap();
    assert!(
        Record::read(&mut worker.output, &ProtocolLimits::ABSOLUTE)
            .unwrap()
            .is_none()
    );
    assert!(!worker.child.wait().unwrap().success());
}
