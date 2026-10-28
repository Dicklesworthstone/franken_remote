//! Actual X11 capture and child-process rate changes retain the original decode chain.
use super::*;
use fr_media::{
    access_unit::EncodedAccessUnit,
    hevc::HevcGuard,
    worker::{self, Identity, Kind, Record, capture_payload},
};
use fr_native::HevcDecoder;

struct OriginalDecoder {
    guard: HevcGuard,
    decoder: HevcDecoder,
}
impl OriginalDecoder {
    fn new(unit: &EncodedAccessUnit, bitrate: u32) -> Self {
        let cfg = configuration(bitrate);
        let limits = cfg.limits().unwrap();
        let mut guard = HevcGuard::new(cfg.codec().unwrap(), limits, 4).unwrap();
        guard.validate_length_prefixed(unit.bytes(), true).unwrap();
        let decoder = HevcDecoder::new(
            cfg.codec().unwrap(),
            limits,
            guard.decoder_record().unwrap().bytes(),
        )
        .unwrap();
        Self { guard, decoder }
    }
    fn accept(&mut self, unit: &EncodedAccessUnit, frame: u64, time: u64, idr: bool) {
        assert_eq!(unit.frame().as_raw(), frame);
        assert_eq!(unit.capture_micros(), time);
        assert_eq!(
            unit.config_generation(),
            CodecConfigurationGeneration::INITIAL
        );
        assert_eq!(unit.is_idr(), idr);
        self.guard
            .validate_length_prefixed(unit.bytes(), unit.is_idr())
            .unwrap();
        self.decoder.submit(unit).unwrap();
        assert_eq!(self.decoder.poll_output().unwrap().0, unit.frame());
    }
}
fn request(
    owner: &mut ChangeAwareCapture,
    frame: u64,
    time: u64,
    bitrate: u32,
) -> Result<CaptureOutput, NativeError> {
    owner.capture_with_bitrate(FrameId::from_raw(frame), time, false, true, bitrate)
}

#[test]
fn pending_and_idle_targets_do_not_mutate_actual_rate_or_legacy_recovery() {
    let display = Display::start(true);
    let mut source = display.source();
    paint(&mut source, 12);
    let mut owner = display.capture_at_rate(8_000_000);
    assert!(owner.enable_damage_tracking().unwrap());
    assert_eq!(capture(&mut owner, 0, 0, true), CaptureOutput::Submitted);
    assert_eq!(
        request(&mut owner, 1, 1, 4_000_000),
        Err(NativeError::NeedDrain)
    );
    assert_eq!(owner.stats().requested_bitrate_bps, 8_000_000);
    let first = owner.poll_output().unwrap();
    let mut decoder = OriginalDecoder::new(&first, 8_000_000);
    decoder.accept(&first, 0, 0, true);

    assert_eq!(
        request(&mut owner, 1, 1, 9_000_000),
        Err(NativeError::InvalidConfiguration)
    );
    assert_eq!(owner.stats().requested_bitrate_bps, 8_000_000);
    unchanged(request(&mut owner, 1, 1, 4_000_000).unwrap(), 1, 0, 1);
    assert_eq!(owner.stats().requested_bitrate_bps, 4_000_000);
    assert_eq!(owner.stats().encoder_bitrate_bps, 8_000_000);
    assert_eq!(owner.stats().bitrate_reconfigurations, 0);
    assert_eq!(owner.stats().encoded_submissions, 1);

    // Legacy recovery cannot apply a deferred target without a rate reply.
    assert_eq!(capture(&mut owner, 2, 2, true), CaptureOutput::Submitted);
    decoder.accept(&owner.poll_output().unwrap(), 2, 2, true);
    assert_eq!(owner.stats().requested_bitrate_bps, 8_000_000);
    assert_eq!(owner.stats().encoder_bitrate_bps, 8_000_000);
    assert_eq!(owner.stats().bitrate_reconfigurations, 0);
    unchanged(request(&mut owner, 3, 3, 4_000_000).unwrap(), 3, 2, 3);

    paint(&mut source, 140);
    assert_eq!(
        request(&mut owner, 4, 4, 4_000_000).unwrap(),
        CaptureOutput::Submitted
    );
    // Actual application is reported only after output and source validation.
    assert_eq!(owner.stats().encoder_bitrate_bps, 8_000_000);
    assert_eq!(owner.stats().bitrate_reconfigurations, 0);
    assert_eq!(
        request(&mut owner, 5, 5, 2_000_000),
        Err(NativeError::NeedDrain)
    );
    assert_eq!(
        owner.capture(FrameId::from_raw(5), 5, true, true),
        Err(NativeError::NeedDrain)
    );
    assert_eq!(owner.stats().requested_bitrate_bps, 4_000_000);
    decoder.accept(&owner.poll_output().unwrap(), 4, 4, true);
    assert_eq!(owner.stats().encoder_bitrate_bps, 4_000_000);
    assert_eq!(owner.stats().bitrate_reconfigurations, 1);

    paint(&mut source, 200);
    assert_eq!(
        request(&mut owner, 5, 5, 4_000_000).unwrap(),
        CaptureOutput::Submitted
    );
    decoder.accept(&owner.poll_output().unwrap(), 5, 5, false);
    assert_eq!(owner.stats().bitrate_reconfigurations, 1);
}

#[test]
fn a_second_real_reopen_waits_for_native_and_source_dwell_without_forcing_idle_video() {
    let display = Display::start(true);
    let mut source = display.source();
    paint(&mut source, 12);
    let mut owner = display.capture_at_rate(8_000_000);
    assert!(owner.enable_damage_tracking().unwrap());
    assert_eq!(capture(&mut owner, 0, 0, true), CaptureOutput::Submitted);
    let first = owner.poll_output().unwrap();
    let mut decoder = OriginalDecoder::new(&first, 8_000_000);
    decoder.accept(&first, 0, 0, true);
    paint(&mut source, 80);
    assert_eq!(
        request(&mut owner, 1, 10, 4_000_000).unwrap(),
        CaptureOutput::Submitted
    );
    decoder.accept(&owner.poll_output().unwrap(), 1, 10, true);

    // A large parent-clock jump cannot buy elapsed native time.
    paint(&mut source, 140);
    assert_eq!(
        request(&mut owner, 2, 2_000_010, 2_000_000).unwrap(),
        CaptureOutput::Submitted
    );
    decoder.accept(&owner.poll_output().unwrap(), 2, 2_000_010, false);
    assert_eq!(owner.stats().encoder_bitrate_bps, 4_000_000);
    assert_eq!(owner.stats().bitrate_reconfigurations, 1);
    std::thread::sleep(Duration::from_millis(2050));

    // Even a now-due target alone cannot create a capture or reference frame.
    unchanged(
        request(&mut owner, 3, 2_000_011, 2_000_000).unwrap(),
        3,
        2,
        2_000_011,
    );
    assert_eq!(owner.stats().bitrate_reconfigurations, 1);
    paint(&mut source, 200);
    assert_eq!(
        request(&mut owner, 4, 2_000_012, 2_000_000).unwrap(),
        CaptureOutput::Submitted
    );
    decoder.accept(&owner.poll_output().unwrap(), 4, 2_000_012, true);
    assert_eq!(owner.stats().encoder_bitrate_bps, 2_000_000);
    assert_eq!(owner.stats().bitrate_reconfigurations, 2);
}

struct ChildOwner(Child);
impl Drop for ChildOwner {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn target(frame: u64, time: u64, bitrate: u32) -> Vec<u8> {
    worker::rate::Capture {
        frame: FrameId::from_raw(frame),
        observed_micros: time,
        force_idr: false,
        only_if_changed: true,
        bitrate,
    }
    .encode()
    .unwrap()
}

#[test]
fn actual_worker_reports_applied_rate_and_keeps_original_configuration_through_reopen() {
    let display = Display::start(true);
    let mut source = display.source();
    paint(&mut source, 12);
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
    let cfg = configuration(8_000_000);
    assert_eq!(
        exchange(Kind::Configure, cfg.encode().unwrap()).header.kind,
        Kind::Ready
    );
    let first = exchange(
        Kind::CaptureIfChanged,
        capture_payload(FrameId::FIRST, 0, true),
    );
    assert_eq!(first.header.kind, Kind::Unit);
    let first = worker::parse_unit(first.into_body(), &limits).unwrap();
    let mut decoder = OriginalDecoder::new(&first, cfg.bitrate);
    decoder.accept(&first, 0, 0, true);

    let idle = exchange(Kind::CaptureAtRate, target(1, 1, 4_000_000));
    assert_eq!(idle.header.kind, Kind::RateUnchanged);
    let (actual, proof) = worker::rate::parse_unchanged(idle.body()).unwrap();
    assert_eq!(actual, 8_000_000);
    assert_eq!(proof.reference, FrameId::FIRST);
    assert_eq!(proof.candidate, FrameId::from_raw(1));
    assert_eq!(proof.observed_micros, 1);
    let legacy = exchange(
        Kind::CaptureIfChanged,
        capture_payload(FrameId::from_raw(2), 2, true),
    );
    assert_eq!(legacy.header.kind, Kind::Unit);
    decoder.accept(
        &worker::parse_unit(legacy.into_body(), &limits).unwrap(),
        2,
        2,
        true,
    );
    let idle = exchange(Kind::CaptureAtRate, target(3, 3, 4_000_000));
    assert_eq!(idle.header.kind, Kind::RateUnchanged);
    let (actual, proof) = worker::rate::parse_unchanged(idle.body()).unwrap();
    assert_eq!(
        actual, 8_000_000,
        "legacy recovery must not apply a deferred target"
    );
    assert_eq!(proof.reference, FrameId::from_raw(2));

    for (frame, color, requested, expected_idr) in [
        (4, 140, 4_000_000, true),
        (5, 200, 4_000_000, false),
        (6, 60, 2_000_000, false),
    ] {
        paint(&mut source, color);
        let reply = exchange(Kind::CaptureAtRate, target(frame, frame, requested));
        assert_eq!(reply.header.kind, Kind::RateUnit);
        let (actual, unit) = worker::rate::parse_unit(reply.into_body(), &limits).unwrap();
        assert_eq!(
            actual, 4_000_000,
            "report actual rate while a newer target awaits dwell"
        );
        decoder.accept(&unit, frame, frame, expected_idr);
    }
    let idle = exchange(Kind::CaptureAtRate, target(7, 7, 2_000_000));
    assert_eq!(idle.header.kind, Kind::RateUnchanged);
    let (actual, proof) = worker::rate::parse_unchanged(idle.body()).unwrap();
    assert_eq!(actual, 4_000_000);
    assert_eq!(proof.reference, FrameId::from_raw(6));
    assert_eq!(exchange(Kind::Stop, Vec::new()).header.kind, Kind::Stopped);
    drop(exchange);
    assert!(child.0.wait().unwrap().success());
}
