//! Real capture, codec and decoded-pixel evidence for the optional software pass.
use super::*;
use fr_media::{access_unit::EncodedAccessUnit, hevc::HevcGuard};
use fr_native::HevcDecoder;
use std::time::Instant;

fn enabled(display: &Display, bitrate: u32) -> ChangeAwareCapture {
    let mut owner = display.capture_at_rate(bitrate);
    assert!(owner.enable_damage_tracking().unwrap());
    assert!(owner.enable_settle_refinement().unwrap());
    owner
}
fn observed(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_micros()).unwrap()
}
fn settle(owner: &mut ChangeAwareCapture, next: &mut u64, start: Instant) -> (FrameId, u64) {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        assert!(
            Instant::now() < deadline,
            "no refinement after continuous quiet evidence"
        );
        let candidate = *next;
        *next += 1;
        let time = observed(start);
        match capture(owner, candidate, time, false) {
            CaptureOutput::Submitted => return (FrameId::from_raw(candidate), time),
            CaptureOutput::Unchanged(proof) => {
                assert_eq!(proof.candidate.as_raw(), candidate);
                assert_eq!(proof.observed_micros, time);
            }
        }
        std::thread::sleep(Duration::from_millis(80));
    }
}
// Legible, tiny terminal-like glyphs without a font, renderer or file dependency.
fn text(offset: usize) -> BgraFrame {
    const GLYPHS: [[u8; 7]; 16] = [
        [14, 17, 19, 21, 25, 17, 14],
        [4, 12, 4, 4, 4, 4, 14],
        [14, 17, 1, 2, 4, 8, 31],
        [30, 1, 1, 14, 1, 1, 30],
        [2, 6, 10, 18, 31, 2, 2],
        [31, 16, 16, 30, 1, 1, 30],
        [14, 16, 16, 30, 17, 17, 14],
        [31, 1, 2, 4, 8, 8, 8],
        [14, 17, 17, 14, 17, 17, 14],
        [14, 17, 17, 15, 1, 1, 14],
        [14, 17, 17, 31, 17, 17, 17],
        [30, 17, 17, 30, 17, 17, 30],
        [14, 17, 16, 16, 16, 17, 14],
        [30, 17, 17, 17, 17, 17, 30],
        [31, 16, 16, 30, 16, 16, 31],
        [31, 16, 16, 30, 16, 16, 16],
    ];
    let mut pixels = Vec::with_capacity(320 * 240 * 4);
    for y in 0..240 {
        for x in 0..320 {
            let (row, col) = ((y + offset) % 10, x % 7);
            let glyph = GLYPHS[(x / 7 + (y + offset) / 10) % GLYPHS.len()];
            let value = if row < 7 && col < 5 && (glyph[row] >> (4 - col)) & 1 != 0 {
                230
            } else {
                20
            };
            pixels.extend_from_slice(&[value, value, value, 255]);
        }
    }
    BgraFrame::new(320, 240, pixels, &ProtocolLimits::ABSOLUTE).unwrap()
}
fn decode(
    decoder: &mut Option<HevcDecoder>,
    cfg: Configuration,
    unit: &EncodedAccessUnit,
) -> BgraFrame {
    let limits = cfg.limits().unwrap();
    if decoder.is_none() {
        let mut guard = HevcGuard::new(cfg.codec().unwrap(), limits, 4).unwrap();
        guard.validate_length_prefixed(unit.bytes(), true).unwrap();
        *decoder = Some(
            HevcDecoder::new(
                cfg.codec().unwrap(),
                limits,
                guard.decoder_record().unwrap().bytes(),
            )
            .unwrap(),
        );
    }
    let decoder = decoder.as_mut().unwrap();
    decoder.submit(unit).unwrap();
    let (frame, pixels) = decoder.poll_output().unwrap();
    assert_eq!(frame, unit.frame());
    pixels
}
fn squared_error(actual: &BgraFrame, expected: &BgraFrame) -> u64 {
    assert_eq!(actual.pixels().len(), expected.pixels().len());
    actual
        .pixels()
        .as_chunks::<4>()
        .0
        .iter()
        .zip(expected.pixels().as_chunks::<4>().0)
        .map(|(actual, expected)| {
            (0..3)
                .map(|channel| {
                    let difference = i64::from(actual[channel]) - i64::from(expected[channel]);
                    difference.unsigned_abs().pow(2)
                })
                .sum::<u64>()
        })
        .sum()
}

#[test]
fn settled_text_is_really_sharper_once_with_original_source_time_and_bounded_bytes() {
    let display = Display::start(true);
    let mut source = display.source();
    let cfg = configuration(500_000);
    let mut owner = enabled(&display, cfg.bitrate);
    let start = Instant::now();
    let mut decoder = None;
    let mut before = None;
    for frame in 0..21 {
        source.present(&text(21 - frame)).unwrap();
        assert_eq!(
            capture(
                &mut owner,
                u64::try_from(frame).unwrap(),
                observed(start),
                false
            ),
            CaptureOutput::Submitted
        );
        before = Some(decode(&mut decoder, cfg, &owner.poll_output().unwrap()));
    }
    // Compare to the actual newly captured drawable, not assumed font pixels.
    let expected = X11Surface::capture(Some(&display.name), cfg.limits().unwrap())
        .unwrap()
        .snapshot()
        .unwrap();
    let mut next = 21;
    let (frame, time) = settle(&mut owner, &mut next, start);
    let unit = owner.poll_output().unwrap();
    assert_eq!(unit.frame(), frame);
    assert_eq!(unit.capture_micros(), time);
    assert_eq!(unit.config_generation(), cfg.generation);
    assert!(
        !unit.is_idr(),
        "refinement must retain the existing decode chain"
    );
    assert!(unit.bytes().len() <= usize::try_from(cfg.bitrate / 40).unwrap());
    let after = decode(&mut decoder, cfg, &unit);
    let (before, after) = (
        squared_error(&before.unwrap(), &expected),
        squared_error(&after, &expected),
    );
    eprintln!(
        "settled text: decoded RGB squared error {before} -> {after}, refinement bytes {}",
        unit.bytes().len()
    );
    assert!(
        after * 100 < before * 95,
        "decoded RGB error did not improve by 5%: before={before}, after={after}"
    );
    let submissions = owner.stats().encoded_submissions;
    for _ in 0..10 {
        std::thread::sleep(Duration::from_millis(80));
        let time = observed(start);
        unchanged(
            capture(&mut owner, next, time, false),
            next,
            frame.as_raw(),
            time,
        );
        next += 1;
    }
    assert_eq!(owner.stats().encoded_submissions, submissions);
    assert_eq!(owner.stats().refinements, 1);
}

#[test]
fn damage_during_a_pending_refinement_is_preserved_and_gets_priority() {
    let display = Display::start(true);
    let mut source = display.source();
    paint(&mut source, 12);
    let mut owner = enabled(&display, 2_000_000);
    let start = Instant::now();
    encoded(&mut owner, 0, observed(start), true);
    let mut next = 1;
    let (refined, _) = settle(&mut owner, &mut next, start);
    paint(&mut source, 140);
    assert_eq!(
        owner.capture(FrameId::from_raw(next), observed(start), false, true),
        Err(NativeError::NeedDrain)
    );
    assert_eq!(owner.poll_output().unwrap().frame(), refined);
    assert_eq!(owner.stats().refinements, 1);
    // A backpressured attempt did not spend its frame identity or consume DAMAGE.
    encoded(&mut owner, next, observed(start), false);
    assert_eq!(owner.stats().encoded_submissions, 3);
    assert_eq!(owner.stats().refinements, 1);
    let time = observed(start);
    unchanged(
        capture(&mut owner, next + 1, time, false),
        next + 1,
        next,
        time,
    );
    // Recovery still performs a fresh readback and an actual IDR, even in cooldown.
    let readbacks = owner.stats().readbacks;
    encoded(&mut owner, next + 2, observed(start), true);
    assert_eq!(owner.stats().readbacks, readbacks + 1);
    assert_eq!(owner.stats().refinements, 1);
}

#[test]
fn a_due_natural_idr_skips_polish_without_disabling_idle_damage_suppression() {
    let display = Display::start(true);
    let mut source = display.source();
    let cfg = configuration(2_000_000);
    let mut owner = enabled(&display, cfg.bitrate);
    let start = Instant::now();
    let gop = u64::from(cfg.codec().unwrap().gop().max_gop_frames());
    for frame in 0..gop {
        paint(&mut source, if frame % 2 == 0 { 12 } else { 140 });
        encoded(&mut owner, frame, observed(start), frame == 0);
    }
    let (readbacks, damage) = (owner.stats().readbacks, owner.stats().damage_observations);
    for candidate in gop..gop + 12 {
        let time = observed(start);
        unchanged(
            capture(&mut owner, candidate, time, false),
            candidate,
            gop - 1,
            time,
        );
        std::thread::sleep(Duration::from_millis(80));
    }
    assert_eq!(owner.stats().encoded_submissions, gop);
    assert_eq!(owner.stats().refinements, 0);
    assert!(owner.stats().damage_observations >= damage + 6);
    assert!(owner.stats().readbacks <= readbacks + 6);
    paint(&mut source, 200);
    assert_eq!(
        capture(&mut owner, gop + 12, observed(start), false),
        CaptureOutput::Submitted
    );
    assert!(
        owner.poll_output().unwrap().is_idr(),
        "optional polish must not extend the admitted GOP"
    );
}

pub(super) fn worker_settle(
    exchange: &mut impl FnMut(fr_media::worker::Kind, Vec<u8>) -> fr_media::worker::Record,
    configuration: Configuration,
) {
    use fr_media::worker::{Kind, UnchangedCapture, capture_payload, parse_unit};
    let limits = ProtocolLimits::ABSOLUTE;
    // Exercise the actual production worker's opt-in for longer than its
    // optional-work cooldown. No local mock can enable this on its behalf.
    let start = std::time::Instant::now();
    let mut reference = FrameId::from_raw(2);
    let mut refined_at: Option<std::time::Instant> = None;
    let mut candidate = 3;
    loop {
        assert!(
            start.elapsed() < Duration::from_secs(6),
            "worker did not refine a settled source"
        );
        std::thread::sleep(Duration::from_millis(80));
        let time = 13 + u64::try_from(start.elapsed().as_micros()).unwrap();
        let reply = exchange(
            Kind::CaptureIfChanged,
            capture_payload(FrameId::from_raw(candidate), time, false),
        );
        match reply.header.kind {
            Kind::Unit => {
                let unit = parse_unit(reply.into_body(), &limits).unwrap();
                assert!(!unit.is_idr());
                assert_eq!(unit.capture_micros(), time);
                assert_eq!(unit.frame().as_raw(), candidate);
                assert_eq!(unit.config_generation(), configuration.generation);
                assert!(time >= 400_000);
                assert!(refined_at.is_none(), "static source was refined twice");
                reference = unit.frame();
                refined_at = Some(std::time::Instant::now());
            }
            Kind::Unchanged => assert_eq!(
                UnchangedCapture::decode(reply.body()).unwrap(),
                UnchangedCapture {
                    candidate: FrameId::from_raw(candidate),
                    reference,
                    observed_micros: time,
                }
            ),
            _ => panic!("unexpected conditional capture reply"),
        }
        candidate += 1;
        if refined_at.is_some_and(|refined| refined.elapsed() >= Duration::from_millis(2100)) {
            break;
        }
    }
}
