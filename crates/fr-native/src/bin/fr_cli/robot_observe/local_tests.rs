use super::*;
use fr_client::robot::live::{Observation, Phase};
use fr_core::ids::DisplayGeometryGeneration;
use fr_wire::display::Display;
use std::time::Instant;
fn request(tail: &str) -> Request {
    parse(
        &format!(
            "robot observe n-host --connection-id {} {tail}",
            "a3".repeat(16)
        )
        .split_whitespace()
        .map(str::to_owned)
        .collect::<Vec<_>>(),
    )
    .unwrap()
}
fn snapshot() -> Snapshot {
    let now = Instant::now();
    let mut owner = Observation::default();
    owner.begin(1).unwrap();
    owner.authenticated().unwrap();
    owner
        .ready(
            Display {
                handle: 17,
                geometry: DisplayGeometryGeneration::INITIAL,
                x: 0,
                y: 0,
                pixel_width: 800,
                pixel_height: 600,
                logical_width: 800,
                logical_height: 600,
                scale_numerator: 1,
                scale_denominator: 1,
                rotation: 0,
            },
            0,
            Evidence::Decoded,
            now,
        )
        .unwrap();
    owner.snapshot(now).unwrap()
}

#[test]
fn local_output_preserves_receipts_without_inventing_source_freshness_or_authority() {
    let text = render(&request("--json"), &snapshot(), 7).unwrap();
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(value["outcome"], "success");
    assert!(value.get("stage").is_none());
    let data = &value["data"];
    assert_eq!(data["new_connection_opened"], false);
    assert_eq!(data["physical_visibility_proven"], false);
    assert_eq!(data["current_input_precondition"], false);
    assert_eq!(data["source_freshness"], "unknown");
    assert!(data["control_authority"].is_null());
    assert!(data["capture_timestamp_unix_ms"].is_null());
    assert_eq!(data["receipt_age_upper_ms"], 7);
    assert_eq!(data["snapshot"]["frame"]["frame_serial"], 0);
}

#[test]
fn caller_age_includes_the_query_and_never_refreshes_the_original_receipt() {
    let mut value = snapshot();
    value.frame.as_mut().unwrap().receipt_age_ms = 25;
    assert!(render(&request("--max-receipt-age-ms 30"), &value, 5).is_ok());
    assert_eq!(
        render(&request("--max-receipt-age-ms 30"), &value, 6)
            .unwrap_err()
            .code,
        "observation_receipt_too_old"
    );
    assert_eq!(value.frame.unwrap().receipt_age_ms, 25);
    let mut value = snapshot();
    value.frame.as_mut().unwrap().receipt_age_ms = u64::MAX;
    assert_eq!(
        render(&request(""), &value, 1).unwrap_err().code,
        "observation_age_unavailable"
    );
}

#[test]
fn pending_or_insufficient_observations_do_not_become_success() {
    let mut value = snapshot();
    value.phase = Phase::Cleaning;
    value.frame = None;
    assert_eq!(
        render(&request(""), &value, 0).unwrap_err().code,
        "observation_not_ready"
    );
    assert_eq!(
        render(
            &request("--evidence-level submitted_to_compositor"),
            &snapshot(),
            0
        )
        .unwrap_err()
        .code,
        "observation_evidence_insufficient"
    );
}

#[test]
fn local_query_rejects_new_connection_and_privileged_flags_before_io() {
    for tail in [
        "--native-once",
        "--experimental-native",
        "--port 8443",
        "--socket /local/socket",
        "--lease secret",
        "--control",
        "--max-receipt-age-ms 0",
        "--max-receipt-age-ms 60001",
        "--json --json",
    ] {
        let text = format!(
            "robot observe n-host --connection-id {} {tail}",
            "a3".repeat(16)
        );
        assert!(
            parse(
                &text
                    .split_whitespace()
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            )
            .is_err(),
            "{tail}"
        );
    }
    for id in [
        "",
        "short",
        "A3A3A3A3A3A3A3A3A3A3A3A3A3A3A3A3",
        "../../private",
    ] {
        let text = format!("robot observe n-host --connection-id {id}");
        assert!(
            parse(
                &text
                    .split_whitespace()
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            )
            .is_err()
        );
    }
}

#[test]
fn explicit_local_screenshot_accepts_only_one_local_output_path() {
    let parsed = request("--screenshot /private/new.png --json");
    assert_eq!(parsed.screenshot, Some(PathBuf::from("/private/new.png")));
    for tail in [
        "--screenshot",
        "--screenshot --json",
        "--screenshot first --screenshot second",
    ] {
        let text = format!(
            "robot observe n-host --connection-id {} {tail}",
            "a3".repeat(16)
        );
        assert!(
            parse(
                &text
                    .split_whitespace()
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            )
            .is_err()
        );
    }
}
