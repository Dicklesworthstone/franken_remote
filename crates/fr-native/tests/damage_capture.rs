#![cfg(all(target_os = "linux", feature = "linux-media"))]
//! Real X11 DAMAGE + software HEVC regression tests, not GPU/tailnet qualification.
use fr_core::{ids::CodecConfigurationGeneration, limits::ProtocolLimits};
use fr_media::{
    access_unit::FrameId,
    worker::{Backend, Configuration},
};
use fr_native::{
    BgraFrame, EncodeBackend, HevcEncoder, NativeError, X11Surface,
    capture::{CaptureOutput, ChangeAwareCapture},
};
use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    time::Duration,
};

struct Display {
    child: Child,
    name: String,
}
impl Display {
    fn start(damage: bool) -> Self {
        let mut command = Command::new("Xvfb");
        command.args([
            "-displayfd",
            "1",
            "-screen",
            "0",
            "320x240x24",
            "-nolisten",
            "tcp",
        ]);
        if !damage {
            command.args(["-extension", "DAMAGE"]);
        }
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("Xvfb is a native test prerequisite");
        let mut number = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut number)
            .unwrap();
        Self {
            child,
            name: format!(":{}", number.trim().parse::<u32>().unwrap()),
        }
    }
    fn source(&self) -> X11Surface {
        X11Surface::presenter(Some(&self.name), 320, 240, ProtocolLimits::ABSOLUTE).unwrap()
    }
    fn capture(&self) -> ChangeAwareCapture {
        let configuration = Configuration {
            width: 320,
            height: 240,
            fps: 30,
            backend: Backend::SoftwareExplicit,
            bitrate: 2_000_000,
            max_access_unit_bytes: 1024 * 1024,
            generation: CodecConfigurationGeneration::INITIAL,
        };
        let limits = configuration.limits().unwrap();
        ChangeAwareCapture::new(
            X11Surface::capture(Some(&self.name), limits).unwrap(),
            HevcEncoder::new(
                configuration.codec().unwrap(),
                limits,
                EncodeBackend::SoftwareExplicit,
                30,
                2_000_000,
            )
            .unwrap(),
        )
    }
}
impl Drop for Display {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn paint(source: &mut X11Surface, color: u8) {
    let bytes = [color, 32, 100, 255].repeat(320 * 240);
    source
        .present(&BgraFrame::new(320, 240, bytes, &ProtocolLimits::ABSOLUTE).unwrap())
        .unwrap();
}
fn capture(capture: &mut ChangeAwareCapture, frame: u64, time: u64, force: bool) -> CaptureOutput {
    capture
        .capture(FrameId::from_raw(frame), time, force, true)
        .unwrap()
}
fn encoded(capture: &mut ChangeAwareCapture, frame: u64, time: u64, force: bool) {
    assert_eq!(
        self::capture(capture, frame, time, force),
        CaptureOutput::Submitted
    );
    let unit = capture.poll_output().unwrap();
    assert_eq!(unit.frame().as_raw(), frame);
    if force {
        assert!(unit.is_idr());
    }
}
fn unchanged(output: CaptureOutput, candidate: u64, reference: u64, time: u64) {
    let CaptureOutput::Unchanged(proof) = output else {
        panic!("expected unchanged drawable");
    };
    assert_eq!(proof.candidate.as_raw(), candidate);
    assert_eq!(proof.reference.as_raw(), reference);
    assert_eq!(proof.observed_micros, time);
}
#[test]
fn idle_drawable_avoids_readbacks_but_periodically_verifies_full_pixels() {
    let display = Display::start(true);
    let mut source = display.source();
    paint(&mut source, 12);
    let mut owner = display.capture();
    assert!(owner.enable_damage_tracking().unwrap());
    encoded(&mut owner, 0, 0, true);
    // Explicit verification also avoids wall-clock timing dependence on the
    // initial software encoder setup. Subsequent checks do no codec work.
    unchanged(capture(&mut owner, 1, 250_000, false), 1, 0, 250_000);
    assert_eq!(owner.stats().readbacks, 2);
    unchanged(capture(&mut owner, 2, 250_001, false), 2, 0, 250_001);
    assert_eq!(owner.stats().readbacks, 2);
    assert_eq!(owner.stats().damage_observations, 1);
    unchanged(capture(&mut owner, 3, 500_000, false), 3, 0, 500_000);
    assert_eq!(owner.stats().readbacks, 3);
    assert_eq!(owner.stats().encoded_submissions, 1);
}
#[test]
fn drawing_during_pending_encode_is_not_cleared_at_completion() {
    let display = Display::start(true);
    let mut source = display.source();
    paint(&mut source, 12);
    let mut owner = display.capture();
    assert!(owner.enable_damage_tracking().unwrap());
    assert_eq!(capture(&mut owner, 0, 0, true), CaptureOutput::Submitted);
    assert_eq!(owner.enable_damage_tracking(), Err(NativeError::NeedDrain));
    paint(&mut source, 140);
    assert_eq!(owner.poll_output().unwrap().frame(), FrameId::FIRST);
    encoded(&mut owner, 1, 1, false);
    assert_eq!(owner.stats().readbacks, 2);
    assert_eq!(owner.stats().encoded_submissions, 2);
    unchanged(capture(&mut owner, 2, 250_001, false), 2, 1, 250_001);
    unchanged(capture(&mut owner, 3, 250_002, false), 3, 1, 250_002);
    assert_eq!(owner.stats().damage_observations, 1);
}
#[test]
fn same_pixel_repaint_is_compared_not_encoded_and_recovery_always_encodes() {
    let display = Display::start(true);
    let mut source = display.source();
    paint(&mut source, 12);
    let mut owner = display.capture();
    assert!(owner.enable_damage_tracking().unwrap());
    encoded(&mut owner, 0, 0, true);
    paint(&mut source, 12);
    unchanged(capture(&mut owner, 1, 1, false), 1, 0, 1);
    assert_eq!(owner.stats().readbacks, 2);
    encoded(&mut owner, 2, 2, true);
    assert_eq!(owner.stats().readbacks, 3);
    assert_eq!(owner.stats().encoded_submissions, 2);
    assert_eq!(
        owner
            .capture(FrameId::from_raw(3), 3, false, false)
            .unwrap(),
        CaptureOutput::Submitted
    );
    assert_eq!(owner.poll_output().unwrap().frame().as_raw(), 3);
    assert_eq!(owner.stats().readbacks, 4);
}
#[test]
fn missing_extension_retains_exact_full_readback_without_a_fake_damage_witness() {
    let display = Display::start(false);
    let mut source = display.source();
    paint(&mut source, 12);
    let mut owner = display.capture();
    assert!(!owner.enable_damage_tracking().unwrap());
    encoded(&mut owner, 0, 0, true);
    unchanged(capture(&mut owner, 1, 1, false), 1, 0, 1);
    unchanged(capture(&mut owner, 2, 2, false), 2, 0, 2);
    assert_eq!(owner.stats().readbacks, 3);
    assert_eq!(owner.stats().damage_observations, 0);
}
#[test]
fn real_elapsed_time_forces_verification_even_when_parent_timestamp_does_not_advance() {
    let display = Display::start(true);
    let mut source = display.source();
    paint(&mut source, 12);
    let mut owner = display.capture();
    assert!(owner.enable_damage_tracking().unwrap());
    encoded(&mut owner, 0, 0, true);
    std::thread::sleep(Duration::from_millis(270));
    unchanged(capture(&mut owner, 1, 0, false), 1, 0, 0);
    assert_eq!(owner.stats().readbacks, 2);
    assert_eq!(owner.stats().damage_observations, 0);
}
#[test]
fn damage_tracking_does_not_revive_stale_capture_generations() {
    let display = Display::start(true);
    let mut source = display.source();
    paint(&mut source, 12);
    let mut owner = display.capture();
    assert!(owner.enable_damage_tracking().unwrap());
    encoded(&mut owner, 0, 10, true);
    assert_eq!(
        owner.capture(FrameId::from_raw(1), 9, false, true),
        Err(NativeError::StaleGeneration)
    );
    assert_eq!(owner.enable_damage_tracking(), Err(NativeError::Closed));
    assert_eq!(
        owner.capture(FrameId::from_raw(2), 20, false, true),
        Err(NativeError::Closed)
    );
    assert_eq!(owner.stats().readbacks, 1);
}

#[test]
fn supervised_worker_keeps_conditional_capture_and_recovery_wire_contracts() {
    use fr_media::worker::{Identity, Kind, Record, UnchangedCapture, capture_payload, parse_unit};
    let display = Display::start(true);
    let mut source = display.source();
    paint(&mut source, 12);
    struct ChildOwner(Child);
    impl Drop for ChildOwner {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = ChildOwner(
        Command::new(env!("CARGO_BIN_EXE_fr-media-worker"))
            .env_clear()
            .env("DISPLAY", &display.name)
            .args(["--capture", "--parent-pid", &std::process::id().to_string()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let mut input = child.0.stdin.take().unwrap();
    let mut output = child.0.stdout.take().unwrap();
    let mut next = 0;
    let limits = ProtocolLimits::ABSOLUTE;
    let mut exchange = |kind, body| {
        let identity = Identity {
            epoch: 7,
            sequence: next,
        };
        next += 1;
        Record::new(kind, identity, body, &limits)
            .unwrap()
            .write(&mut input, &limits)
            .unwrap();
        let reply = Record::read(&mut output, &limits).unwrap().unwrap();
        assert_eq!(reply.header.identity, identity);
        reply
    };
    let configuration = Configuration {
        width: 320,
        height: 240,
        fps: 30,
        backend: Backend::SoftwareExplicit,
        bitrate: 2_000_000,
        max_access_unit_bytes: 1024 * 1024,
        generation: CodecConfigurationGeneration::INITIAL,
    };
    assert_eq!(
        exchange(Kind::Configure, configuration.encode().unwrap())
            .header
            .kind,
        Kind::Ready
    );
    let first = exchange(
        Kind::CaptureIfChanged,
        capture_payload(FrameId::FIRST, 10, true),
    );
    assert_eq!(first.header.kind, Kind::Unit);
    assert!(parse_unit(first.into_body(), &limits).unwrap().is_idr());
    let idle = exchange(
        Kind::CaptureIfChanged,
        capture_payload(FrameId::from_raw(1), 11, false),
    );
    assert_eq!(idle.header.kind, Kind::Unchanged);
    let idle = UnchangedCapture::decode(idle.body()).unwrap();
    assert_eq!(idle.reference, FrameId::FIRST);
    assert_eq!(idle.candidate.as_raw(), 1);
    assert_eq!(idle.observed_micros, 11);
    let recovery = exchange(
        Kind::CaptureIfChanged,
        capture_payload(FrameId::from_raw(2), 12, true),
    );
    assert_eq!(recovery.header.kind, Kind::Unit);
    assert!(parse_unit(recovery.into_body(), &limits).unwrap().is_idr());
    assert_eq!(exchange(Kind::Stop, vec![]).header.kind, Kind::Stopped);
    assert!(child.0.wait().unwrap().success());
}
