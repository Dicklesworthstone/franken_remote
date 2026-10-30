use super::*;
use fr_core::ids::DisplayGeometryGeneration;
use std::time::Duration;

fn display() -> Display {
    Display {
        handle: 17, geometry: DisplayGeometryGeneration::INITIAL,
        x: -1920, y: 0, pixel_width: 1920, pixel_height: 1080,
        logical_width: 960, logical_height: 540,
        scale_numerator: 2, scale_denominator: 1, rotation: 0,
    }
}
fn streaming(now: Instant) -> Observation {
    let mut value = Observation::default();
    value.begin(1).unwrap();
    value.authenticated().unwrap();
    value.ready(display(), 0, Evidence::Decoded, now).unwrap();
    value
}

#[test]
fn first_frame_is_available_on_an_idle_screen_and_has_no_inferred_freshness() {
    let now = Instant::now();
    let value = streaming(now);
    let snapshot = value.snapshot(now + Duration::from_secs(7)).unwrap();
    snapshot.validate().unwrap();
    let frame = snapshot.frame.as_ref().unwrap();
    assert_eq!(frame.frame_serial, 0);
    assert_eq!(frame.receipt_age_ms, 7000);
    assert_eq!(frame.origin_x, -1920);
    assert_eq!(frame.evidence_level, Evidence::Decoded);
    let json = serde_json::to_value(snapshot).unwrap();
    for absent in ["source_freshness", "capture_timestamp_unix_ms", "configuration_generation", "control_authority"] {
        assert!(json["frame"].get(absent).is_none());
    }
}

#[test]
fn every_retirement_removes_geometry_and_reconnect_needs_a_new_receipt() {
    let now = Instant::now();
    for phase in [Phase::Cleaning, Phase::Reconnecting, Phase::Stopped] {
        let mut value = streaming(now);
        value.retire(phase);
        let old = value.snapshot(now).unwrap();
        old.validate().unwrap();
        assert_eq!(old.phase, phase);
        assert!(old.frame.is_none());
        value.begin(2).unwrap();
        value.authenticated().unwrap();
        value.approval().unwrap();
        value.approval().unwrap();
        let waiting = value.snapshot(now).unwrap();
        assert_eq!(waiting.attempt, 2);
        assert!(waiting.frame.is_none());
        let mut changed = display();
        changed.handle = 18;
        value.ready(changed, 0, Evidence::SubmittedToCompositor, now).unwrap();
        assert_eq!(value.snapshot(now).unwrap().frame.unwrap().display_handle, format!("{:032x}", 18));
        assert!(old.frame.is_none(), "prior snapshot must not be mutated");
    }
}

#[test]
fn no_counter_wrap_or_attempt_reuse_preserves_an_old_frame() {
    let now = Instant::now();
    for attempt in [0, 1, 33, u8::MAX] {
        let mut value = streaming(now);
        assert_eq!(value.begin(attempt), Err(Error::Order));
        assert!(value.snapshot(now).unwrap().frame.is_none());
    }
}

#[test]
fn a_new_receipt_changes_only_its_actual_stage_serial_and_local_age() {
    let now = Instant::now();
    let mut value = streaming(now);
    let first = value.snapshot(now).unwrap();
    value.presented(21, Evidence::SubmittedToCompositor, now + Duration::from_secs(1)).unwrap();
    let current = value.snapshot(now + Duration::from_millis(1250)).unwrap().frame.unwrap();
    assert_eq!(current.receipt_age_ms, 250);
    assert_eq!(current.frame_serial, 21);
    assert_eq!(current.evidence_level, Evidence::SubmittedToCompositor);
    assert_eq!(first.frame.unwrap().frame_serial, 0);
}

#[test]
fn clock_regression_cannot_turn_old_metadata_into_a_zero_age_receipt() {
    let now = Instant::now();
    let mut value = streaming(now + Duration::from_secs(1));
    assert_eq!(value.snapshot(now), Err(Error::Clock));
    assert_eq!(value.presented(1, Evidence::Decoded, now), Err(Error::Clock));
    assert!(value.snapshot(now).unwrap().frame.is_none());
}

#[test]
fn invalid_display_or_illegal_phase_fences_the_slot() {
    let now = Instant::now();
    let mut value = Observation::default();
    assert_eq!(value.ready(display(), 0, Evidence::Decoded, now), Err(Error::Order));
    value.begin(1).unwrap();
    value.authenticated().unwrap();
    let mut invalid = display();
    invalid.scale_denominator = 0;
    assert_eq!(value.ready(invalid, 0, Evidence::Decoded, now), Err(Error::InvalidMetadata));
    assert!(value.snapshot(now).unwrap().frame.is_none());
}

#[test]
fn bounded_wire_snapshot_validation_rejects_incoherence_and_bad_geometry() {
    let now = Instant::now();
    let original = streaming(now).snapshot(now).unwrap();
    for corrupt in 0..7 {
        let mut value = original.clone();
        match corrupt {
            0 => value.schema_version = 2,
            1 => value.attempt = 0,
            2 => value.phase = Phase::Cleaning,
            3 => value.frame = None,
            4 => value.frame.as_mut().unwrap().display_handle = "0".repeat(32),
            5 => value.frame.as_mut().unwrap().pixel_width = u32::MAX,
            _ => value.frame.as_mut().unwrap().rotation_quarter_turns = 4,
        }
        assert_eq!(value.validate(), Err(Error::InvalidMetadata));
    }
    let mut json = serde_json::to_value(original).unwrap();
    json["frame"]["evidence_level"] = serde_json::json!("instrumentally_observed");
    assert!(serde_json::from_value::<Snapshot>(json).is_err());
}

#[test]
fn repeated_initial_receipt_cannot_refresh_metadata_and_fractional_age_rounds_up() {
    let now = Instant::now();
    let mut value = streaming(now);
    value.presented(0, Evidence::Decoded, now + Duration::from_secs(1)).unwrap();
    let snapshot = value.snapshot(now + Duration::from_micros(1_000_001)).unwrap();
    assert_eq!(snapshot.frame.unwrap().receipt_age_ms, 1001);
}
