#![cfg(all(target_os = "macos", feature = "macos-media"))]
//! The real macOS capture worker as a child process: `ScreenCaptureKit`
//! frames of this Mac's display, `VideoToolbox` HEVC, FRW0 over pipes.
//! Every unit is re-admitted here by an independent `HevcGuard`.
//!
//! Needs a logged-in desktop and the Screen Recording grant for the process
//! running the tests; without them discovery is refused and these tests fail
//! (they never pass without capturing).
//!
//! One share at a time, as the host runs it (one shared capture worker): on
//! macOS 27 several capture clients starting at once can be declined or
//! stall in the system capture service, which the worker's setup lock only
//! partly absorbs. The tests hold `SERIAL` for their whole run.
use fr_core::{ids::CodecConfigurationGeneration, limits::ProtocolLimits};
use fr_media::{
    access_unit::{FrameId, FrameKind},
    hevc::HevcGuard,
    worker::{
        self, Backend, Configuration, Error, Identity, Kind, Record, UnchangedCapture,
        capture::monitors,
    },
};
use fr_wire::display::Catalog;
use std::{
    io::Write,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

static SERIAL: Mutex<()> = Mutex::new(());
fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Worker {
    child: Child,
    input: Option<ChildStdin>,
    output: ChildStdout,
    next: u64,
}
impl Worker {
    fn spawn(args: &[&str]) -> Child {
        Command::new(env!("CARGO_BIN_EXE_fr-media-worker"))
            .env_clear()
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap()
    }
    fn start() -> Self {
        let pid = std::process::id().to_string();
        let mut child = Self::spawn(&["--capture", "--parent-pid", &pid]);
        Self {
            input: child.stdin.take(),
            output: child.stdout.take().unwrap(),
            child,
            next: 0,
        }
    }
    fn transact(&mut self, kind: Kind, body: Vec<u8>) -> Record {
        let limits = ProtocolLimits::ABSOLUTE;
        let identity = Identity {
            epoch: 23,
            sequence: self.next,
        };
        self.next += 1;
        let request = Record::new(kind, identity, body, &limits).unwrap();
        let input = self.input.as_mut().unwrap();
        request.write(input, &limits).unwrap();
        input.flush().unwrap();
        let reply = Record::read(&mut self.output, &limits)
            .unwrap()
            .expect("a reply, not EOF");
        assert_eq!(reply.header.identity, identity);
        reply
    }
    fn wait(&mut self, within: Duration) -> std::process::ExitStatus {
        let until = Instant::now() + within;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < until, "worker did not exit");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        if self.child.try_wait().unwrap().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn refused(reply: &Record) -> Error {
    assert_eq!(reply.header.kind, Kind::Refused);
    Error::from_code(u16::from_be_bytes(reply.body().try_into().unwrap())).unwrap()
}

/// Discover, then configure the main display (the one at the desktop origin).
fn configure(worker: &mut Worker, fps: u16, bitrate: u32) -> (Configuration, Catalog, Record) {
    let limits = ProtocolLimits::ABSOLUTE;
    let reply = worker.transact(Kind::DiscoverMonitors, Vec::new());
    assert_eq!(
        reply.header.kind,
        Kind::CaptureMonitors,
        "discovery refused: is Screen Recording granted to this process? {:?}",
        (reply.header.kind == Kind::Refused).then(|| refused(&reply))
    );
    let catalog = monitors::decode_catalog(reply.body(), &limits).unwrap();
    let main = *catalog
        .displays()
        .iter()
        .find(|d| d.x == 0 && d.y == 0)
        .unwrap_or(&catalog.displays()[0]);
    // The encoded size never exceeds the protocol, and is the display's own
    // pixels or a clean fraction of them; logical size is the display's points.
    assert!(main.pixel_width * main.pixel_height <= 16_777_216);
    assert_eq!(main.pixel_width % 2, 0);
    assert!(main.logical_width > 0 && main.logical_width <= main.pixel_width * 2);
    let configuration = Configuration {
        width: main.pixel_width,
        height: main.pixel_height,
        fps,
        backend: Backend::VideoToolbox,
        bitrate,
        max_access_unit_bytes: limits.max_encoded_access_unit_bytes(),
        generation: CodecConfigurationGeneration::INITIAL,
    };
    let selection = catalog.selection(main.handle).unwrap();
    let body = monitors::encode_configuration(configuration, selection, catalog).unwrap();
    let reply = worker.transact(Kind::ConfigureMonitor, body.clone());
    assert_eq!(reply.header.kind, Kind::MonitorReady);
    assert_eq!(reply.body(), body.as_slice(), "Ready echoes the request");
    (configuration, catalog, reply)
}

#[track_caller]
fn unit(reply: Record, configuration: Configuration) -> fr_media::access_unit::EncodedAccessUnit {
    assert_eq!(reply.header.kind, Kind::Unit, "{:?}", refused_kind(&reply));
    worker::parse_unit(reply.into_body(), &configuration.limits().unwrap()).unwrap()
}

#[track_caller]
fn rate_unit(
    worker: &mut Worker,
    configuration: Configuration,
    frame: u64,
    observed_micros: u64,
    bitrate: u32,
) -> (u32, fr_media::access_unit::EncodedAccessUnit) {
    let request = worker::rate::Capture {
        frame: FrameId::from_raw(frame),
        observed_micros,
        force_idr: false,
        only_if_changed: false,
        bitrate,
    };
    let reply = worker.transact(Kind::CaptureAtRate, request.encode().unwrap());
    assert_eq!(
        reply.header.kind,
        Kind::RateUnit,
        "{:?}",
        refused_kind(&reply)
    );
    let (actual, unit) =
        worker::rate::parse_unit(reply.into_body(), &configuration.limits().unwrap()).unwrap();
    assert_eq!(unit.frame(), request.frame);
    assert_eq!(unit.capture_micros(), request.observed_micros);
    (actual, unit)
}

fn refused_kind(reply: &Record) -> Option<Error> {
    (reply.header.kind == Kind::Refused).then(|| refused(reply))
}

#[test]
fn live_display_units_are_admitted_idr_then_predicted_and_stop_is_clean() {
    let _serial = serial();
    let started = Instant::now();
    let mut worker = Worker::start();
    let (configuration, _, _) = configure(&mut worker, 60, 20_000_000);
    let mut guard = HevcGuard::new(
        configuration.codec().unwrap(),
        configuration.limits().unwrap(),
        4,
    )
    .unwrap();

    let idr = unit(
        worker.transact(
            Kind::CaptureIfChanged,
            worker::capture_payload(FrameId::from_raw(1), 1_000, true),
        ),
        configuration,
    );
    // Spawn to first admitted IDR inside the host's 2 s preparation budget.
    let first = started.elapsed();
    assert!(first < Duration::from_secs(2), "first IDR after {first:?}");
    assert_eq!(idr.frame(), FrameId::from_raw(1));
    assert_eq!(idr.capture_micros(), 1_000);
    assert!(matches!(idr.kind(), FrameKind::Idr { .. }));
    guard.validate_length_prefixed(idr.bytes(), true).unwrap();

    // `Capture` always encodes: P pictures chained to the previous unit.
    let mut last = idr.frame();
    for n in 2..=30u64 {
        let p = unit(
            worker.transact(
                Kind::Capture,
                worker::capture_payload(FrameId::from_raw(n), 1_000 + n, false),
            ),
            configuration,
        );
        assert_eq!(p.capture_micros(), 1_000 + n);
        assert_eq!(p.kind(), FrameKind::Predicted { references: last });
        guard.validate_length_prefixed(p.bytes(), false).unwrap();
        last = p.frame();
    }

    // `CaptureIfChanged` either encodes a P picture of new content or proves
    // nothing changed since the last unit; both name that unit.
    for n in 31..=40u64 {
        let reply = worker.transact(
            Kind::CaptureIfChanged,
            worker::capture_payload(FrameId::from_raw(n), 2_000 + n, false),
        );
        if reply.header.kind == Kind::Unchanged {
            let proof = UnchangedCapture::decode(reply.body()).unwrap();
            assert_eq!(
                proof,
                UnchangedCapture {
                    candidate: FrameId::from_raw(n),
                    reference: last,
                    observed_micros: 2_000 + n,
                }
            );
        } else {
            let p = unit(reply, configuration);
            assert_eq!(p.kind(), FrameKind::Predicted { references: last });
            guard.validate_length_prefixed(p.bytes(), false).unwrap();
            last = p.frame();
        }
    }

    // A forced IDR mid-stream restarts the chain.
    let again = unit(
        worker.transact(
            Kind::Capture,
            worker::capture_payload(FrameId::from_raw(41), 3_000, true),
        ),
        configuration,
    );
    assert!(matches!(again.kind(), FrameKind::Idr { .. }));
    guard.validate_length_prefixed(again.bytes(), true).unwrap();

    let cursor = worker.transact(Kind::ReadCursor, Vec::new());
    assert_eq!(cursor.header.kind, Kind::CursorSnapshot);
    assert_eq!(cursor.body(), [2], "the pointer is in the picture");
    assert_eq!(
        worker.transact(Kind::CheckMonitor, Vec::new()).header.kind,
        Kind::MonitorValid
    );
    assert_eq!(
        worker.transact(Kind::Stop, Vec::new()).header.kind,
        Kind::Stopped
    );
    assert!(worker.wait(Duration::from_secs(3)).success());
}

#[test]
fn native_rate_changes_report_only_admitted_idrs_and_keep_the_original_dwell() {
    let _serial = serial();
    let mut worker = Worker::start();
    let (configuration, _, _) = configure(&mut worker, 30, 8_000_000);
    let mut guard = HevcGuard::new(
        configuration.codec().unwrap(),
        configuration.limits().unwrap(),
        4,
    )
    .unwrap();
    let mut last = FrameId::from_raw(0);
    // Bootstrap at the configured rate before changing the original session.
    // The next target is admitted only with a real IDR. Host time alone keeps
    // the subsequent request inside that transition's original dwell, even on
    // a loaded machine where the native calls take more worker time.
    for (frame, observed, target, expected, idr) in [
        (1, 1_000, 4_000_000, 8_000_000, true),
        (2, 2_000, 4_000_000, 4_000_000, true),
        (3, 3_000, 2_000_000, 4_000_000, false),
    ] {
        let (actual, unit) = rate_unit(&mut worker, configuration, frame, observed, target);
        assert_eq!(actual, expected);
        if idr {
            assert!(matches!(unit.kind(), FrameKind::Idr { .. }));
        } else {
            assert_eq!(unit.kind(), FrameKind::Predicted { references: last });
        }
        guard.validate_length_prefixed(unit.bytes(), idr).unwrap();
        last = unit.frame();
    }

    // Both original clocks now pass the bound. Retargeting does not extend it:
    // the new desired setting can apply immediately, again only with an IDR
    // that this independent guard admits under the original configuration.
    std::thread::sleep(Duration::from_millis(2_100));
    let (actual, unit) = rate_unit(&mut worker, configuration, 4, 2_103_000, 8_000_000);
    assert_eq!(actual, 8_000_000);
    assert!(matches!(unit.kind(), FrameKind::Idr { .. }));
    guard.validate_length_prefixed(unit.bytes(), true).unwrap();
    assert_eq!(
        worker.transact(Kind::Stop, Vec::new()).header.kind,
        Kind::Stopped
    );
    assert!(worker.wait(Duration::from_secs(3)).success());
}

#[test]
fn stale_frames_are_refused_and_end_the_worker() {
    let _serial = serial();
    let mut worker = Worker::start();
    let (configuration, _, _) = configure(&mut worker, 30, 8_000_000);
    unit(
        worker.transact(
            Kind::Capture,
            worker::capture_payload(FrameId::from_raw(5), 1, true),
        ),
        configuration,
    );
    let reply = worker.transact(
        Kind::Capture,
        worker::capture_payload(FrameId::from_raw(5), 2, false),
    );
    assert_eq!(refused(&reply), Error::NativeFailure);
    assert!(!worker.wait(Duration::from_secs(3)).success());
}

#[test]
fn other_backends_and_roles_are_refused() {
    let _serial = serial();
    let limits = ProtocolLimits::ABSOLUTE;
    let mut worker = Worker::start();
    let reply = worker.transact(Kind::DiscoverMonitors, Vec::new());
    let catalog = monitors::decode_catalog(reply.body(), &limits).unwrap();
    let d = catalog.displays()[0];
    let configuration = Configuration {
        width: d.pixel_width,
        height: d.pixel_height,
        fps: 30,
        backend: Backend::Nvenc,
        bitrate: 8_000_000,
        max_access_unit_bytes: limits.max_encoded_access_unit_bytes(),
        generation: CodecConfigurationGeneration::INITIAL,
    };
    let body = monitors::encode_configuration(
        configuration,
        catalog.selection(d.handle).unwrap(),
        catalog,
    )
    .unwrap();
    assert_eq!(
        refused(&worker.transact(Kind::ConfigureMonitor, body)),
        Error::Unsupported
    );
    let pid = std::process::id().to_string();
    for role in ["--present", "--audio"] {
        let status = Worker::spawn(&[role, "--parent-pid", &pid]).wait().unwrap();
        assert_eq!(status.code(), Some(2));
    }
    // Not our parent: refused before any capture.
    let status = Worker::spawn(&["--capture", "--parent-pid", "1"])
        .wait()
        .unwrap();
    assert_eq!(status.code(), Some(2));
}

#[test]
fn closing_the_pipe_ends_a_configured_worker() {
    let _serial = serial();
    let mut worker = Worker::start();
    let (configuration, _, _) = configure(&mut worker, 60, 20_000_000);
    unit(
        worker.transact(
            Kind::Capture,
            worker::capture_payload(FrameId::from_raw(1), 1, true),
        ),
        configuration,
    );
    drop(worker.input.take());
    assert!(worker.wait(Duration::from_secs(3)).success());
}
