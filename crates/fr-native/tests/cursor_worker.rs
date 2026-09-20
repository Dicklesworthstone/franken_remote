#![cfg(all(target_os = "linux", feature = "linux-media"))]
//! Actual child, XFIXES and HEVC. Logical cursor IPC cannot advance video state.
mod cursor_support;
use cursor_support::Server;
use fr_core::{
    ids::CodecConfigurationGeneration,
    limits::{LimitOverrides, ProtocolLimits},
};
use fr_media::{
    access_unit::FrameId,
    worker::{self, Backend, Configuration, Identity, Kind, Record},
};
use std::{
    os::fd::{AsFd, AsRawFd},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    time::{Duration, Instant},
};

#[repr(C)]
struct Poll {
    fd: i32,
    events: i16,
    revents: i16,
}
unsafe extern "C" {
    fn poll(fds: *mut Poll, n: usize, timeout: i32) -> i32;
}
struct Worker {
    child: Child,
    input: ChildStdin,
    output: ChildStdout,
    sequence: u64,
}
impl Worker {
    fn start(server: &Server) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_fr-media-worker"))
            .env_clear()
            .env("DISPLAY", &server.name)
            .args(["--capture", "--parent-pid", &std::process::id().to_string()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        Self {
            input: child.stdin.take().unwrap(),
            output: child.stdout.take().unwrap(),
            child,
            sequence: 0,
        }
    }
    fn read(&mut self) -> Option<Record> {
        let mut fd = Poll {
            fd: self.output.as_fd().as_raw_fd(),
            events: 1,
            revents: 0,
        };
        // SAFETY: one writable descriptor record, descriptor remains borrowed.
        assert!(
            unsafe { poll(&raw mut fd, 1, 3000) } > 0,
            "worker reply deadline"
        );
        Record::read(&mut self.output, &ProtocolLimits::ABSOLUTE).unwrap()
    }
    fn request(&mut self, kind: Kind, body: Vec<u8>) -> Record {
        let identity = Identity {
            epoch: 11,
            sequence: self.sequence,
        };
        self.sequence += 1;
        Record::new(kind, identity, body, &ProtocolLimits::ABSOLUTE)
            .unwrap()
            .write(&mut self.input, &ProtocolLimits::ABSOLUTE)
            .unwrap();
        let response = self.read().expect("response, not EOF");
        assert_eq!(response.header.identity, identity);
        response
    }
    fn configure(&mut self) {
        let config = Configuration {
            width: 640,
            height: 480,
            fps: 30,
            backend: Backend::SoftwareExplicit,
            bitrate: 2_000_000,
            max_access_unit_bytes: 1_048_576,
            generation: CodecConfigurationGeneration::INITIAL,
        };
        assert_eq!(
            self.request(Kind::Configure, config.encode().unwrap())
                .header
                .kind,
            Kind::Ready
        );
    }
    fn wait(&mut self) {
        let until = Instant::now() + Duration::from_secs(3);
        while self.child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < until, "worker exit deadline");
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
#[test]
fn native_cursor_ipc_does_not_consume_video_ids_or_unchanged_evidence() {
    let server = Server::start(true);
    let mut worker = Worker::start(&server);
    worker.configure();
    server.cursor(8, 4, (2, 1), 0xff00_ff00);
    server.warp(17, 29);
    // Cursor capture is usable before any video, and does not pretend to decode.
    let reply = worker.request(Kind::ReadCursor, vec![]);
    assert_eq!(reply.header.kind, Kind::CursorSnapshot);
    let first = worker::cursor::decode(reply.body(), &ProtocolLimits::ABSOLUTE)
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            first.x,
            first.y,
            first.width,
            first.height,
            first.hotspot_x,
            first.hotspot_y
        ),
        (17, 29, 8, 4, 2, 1)
    );
    assert_eq!(first.rgba, [0, 255, 0, 255].repeat(32));
    assert!(!format!("{first:?}").contains("255"));
    server.hide();
    let hidden_reply = worker.request(Kind::ReadCursor, vec![]);
    assert_eq!(hidden_reply.header.kind, Kind::CursorSnapshot);
    let hidden = worker::cursor::decode(hidden_reply.body(), &ProtocolLimits::ABSOLUTE)
        .unwrap()
        .unwrap();
    assert_eq!(
        hidden.rgba, first.rgba,
        "logical image is not visibility evidence"
    );
    let capture = worker::capture_payload(FrameId::FIRST, 1, true);
    let mut unit = worker.request(Kind::CaptureIfChanged, capture);
    while unit.header.kind == Kind::NeedInput {
        unit = worker.request(Kind::Poll, vec![]);
    }
    assert_eq!(unit.header.kind, Kind::Unit);
    for x in [18, 39, 82] {
        server.warp(x, 29);
        let reply = worker.request(Kind::ReadCursor, vec![]);
        assert_eq!(reply.header.kind, Kind::CursorSnapshot);
        let s = worker::cursor::decode(reply.body(), &ProtocolLimits::ABSOLUTE)
            .unwrap()
            .unwrap();
        assert_eq!((s.x, s.y), (x, 29));
        assert_eq!(s.native_serial, first.native_serial);
    }
    let reply = worker.request(
        Kind::CaptureIfChanged,
        worker::capture_payload(FrameId::from_raw(1), 2, false),
    );
    assert_eq!(reply.header.kind, Kind::Unchanged);
    let evidence = worker::UnchangedCapture::decode(reply.body()).unwrap();
    assert_eq!(evidence.reference, FrameId::FIRST);
    assert_eq!(evidence.candidate, FrameId::from_raw(1));
    assert_eq!(evidence.observed_micros, 2);
    assert_eq!(
        worker.request(Kind::Stop, vec![]).header.kind,
        Kind::Stopped
    );
    assert!(worker.read().is_none());
    worker.wait();
}
#[test]
fn cursor_request_cannot_skip_role_bootstrap() {
    let server = Server::start(true);
    let mut worker = Worker::start(&server);
    let reply = worker.request(Kind::ReadCursor, vec![]);
    assert_eq!(reply.header.kind, Kind::Refused);
    assert!(worker.read().is_none());
    worker.wait();
    assert_eq!(worker.child.try_wait().unwrap().unwrap().code(), Some(2));
}
#[test]
fn cursor_payload_is_borrowed_bounded_and_not_a_visibility_record() {
    let limits = ProtocolLimits::ABSOLUTE;
    let rgba = [90, 80, 70, 255];
    let s = worker::cursor::Snapshot {
        native_serial: 7,
        x: 10,
        y: 20,
        width: 1,
        height: 1,
        hotspot_x: 0,
        hotspot_y: 0,
        rgba: &rgba,
    };
    let body = worker::cursor::encode(Some(s), &limits).unwrap();
    assert_eq!(&body[..13], &[1, 0, 0, 0, 7, 0, 0, 0, 10, 0, 0, 0, 20]);
    let decoded = worker::cursor::decode(&body, &limits).unwrap().unwrap();
    assert_eq!(s, decoded);
    assert_eq!(decoded.rgba.as_ptr(), body[25..].as_ptr());
    assert_eq!(
        worker::cursor::decode(&worker::cursor::encode(None, &limits).unwrap(), &limits).unwrap(),
        None
    );
    for n in 0..body.len() {
        assert!(worker::cursor::decode(&body[..n], &limits).is_err());
    }
    for (offset, value) in [(0, 2), (5, 255), (13, 2), (17, 1), (24, 9)] {
        let mut invalid = body.clone();
        invalid[offset] = value;
        assert!(worker::cursor::decode(&invalid, &limits).is_err());
    }
    let small = ProtocolLimits::with_overrides(LimitOverrides {
        max_control_message_bytes: Some(1024),
        ..LimitOverrides::default()
    })
    .unwrap();
    let large = vec![0; 1024];
    let s = worker::cursor::Snapshot {
        width: 16,
        height: 16,
        rgba: &large,
        ..s
    };
    assert!(worker::cursor::encode(Some(s), &small).is_err());
    assert!(
        Record::new(
            Kind::CursorSnapshot,
            Identity {
                epoch: 1,
                sequence: 1
            },
            vec![0; 1024],
            &small
        )
        .is_err()
    );
    assert!(
        Record::new(
            Kind::ReadCursor,
            Identity {
                epoch: 1,
                sequence: 1
            },
            vec![0],
            &limits
        )
        .is_err()
    );
}
