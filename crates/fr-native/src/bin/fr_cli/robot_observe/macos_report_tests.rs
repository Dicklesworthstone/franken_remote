use super::*;
use fr_core::ids::{CodecConfigurationGeneration, DisplayGeometryGeneration, RecoveryGeneration};
use fr_media::{access_unit::FrameId, worker::presentation_snapshot::Stamp};
use fr_wire::closure::{Cleanup, Closed, ClosedReason, OutstandingEffects};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

fn receipt(image: bool) -> Receipt {
    let stamp = Stamp {
        frame: FrameId::FIRST,
        configuration: CodecConfigurationGeneration::from_raw(3),
        recovery: RecoveryGeneration::from_raw(7),
    };
    Receipt {
        display: Display {
            handle: u128::MAX,
            geometry: DisplayGeometryGeneration::INITIAL,
            x: -320,
            y: 17,
            pixel_width: 4,
            pixel_height: 3,
            logical_width: 2,
            logical_height: 3,
            scale_numerator: 2,
            scale_denominator: 1,
            rotation: 1,
        },
        serial: stamp.frame.as_raw(),
        evidence: if image {
            Evidence::SubmittedToCompositor
        } else {
            Evidence::Decoded
        },
        received_at: Instant::now().checked_sub(Duration::from_secs(2)).unwrap(),
        snapshot: image.then(|| {
            Snapshot::decode(Snapshot::encode(stamp, 8, 6, &[0x7f; 192]).unwrap()).unwrap()
        }),
    }
}

fn request(path: Option<&Path>) -> Request<'_> {
    Request {
        target: "n-observed",
        json: true,
        minimum: Minimum::Decoded,
        screenshot: path,
    }
}

fn output_path() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "fr-macos-once-{}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        NEXT.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::create_dir(&path).unwrap();
    path.join("new-screen.png")
}

#[test]
fn historical_metadata_preserves_original_geometry_and_does_not_invent_freshness() {
    let original = receipt(false);
    let received_at = original.received_at;
    let rendered = render(request(None), Some(original), None).unwrap();
    let value: Value = serde_json::from_str(&rendered).unwrap();
    assert_eq!(value["outcome"], "success");
    assert_eq!(value["data"]["scope"], "one_shot_native_observation");
    assert_eq!(value["data"]["historical"], true);
    assert_eq!(value["data"]["frame_serial"], 0);
    assert_eq!(value["data"]["evidence_level"], "decoded");
    assert_eq!(value["data"]["display"]["handle"], u128::MAX.to_string());
    assert_eq!(value["data"]["display"]["x"], -320);
    assert_eq!(value["data"]["display"]["y"], 17);
    assert_eq!(value["data"]["display"]["rotation_degrees"], 90);
    assert_eq!(value["data"]["source_freshness"], "unknown");
    assert!(value["data"]["receipt_age_upper_ms"].as_u64().unwrap() >= 2000);
    assert!(
        value["data"]["receipt_age_upper_ms"].as_u64().unwrap()
            <= receipt_age(received_at, Instant::now()).unwrap()
    );
    for field in [
        "configuration_generation",
        "recovery_generation",
        "capture_timestamp_unix_ms",
        "presentation_timestamp_unix_ms",
        "receipt_observed_unix_ms",
        "uncertainty_ms",
        "control_authority",
        "artifact",
        "close_exchange",
    ] {
        assert!(value["data"][field].is_null(), "{field}");
    }
    assert_eq!(value["data"]["physical_visibility_proven"], false);
    assert_eq!(value["data"]["transport_qualified"], false);
    assert_eq!(value["data"]["local_cleanup_confirmed"], true);
}

#[test]
fn receipt_age_includes_original_waits_and_rounds_conservatively() {
    let at = Instant::now();
    assert_eq!(receipt_age(at, at), Some(0));
    assert_eq!(receipt_age(at, at + Duration::from_nanos(1)), Some(1));
    assert_eq!(
        receipt_age(at, at + Duration::from_millis(2137)),
        Some(2137)
    );
    assert_eq!(
        receipt_age(
            at,
            at + Duration::from_millis(2137) + Duration::from_nanos(1)
        ),
        Some(2138)
    );
    assert_eq!(
        receipt_age(at, at.checked_sub(Duration::from_nanos(1)).unwrap()),
        None
    );
}

#[test]
fn insufficient_or_missing_evidence_never_opens_the_screenshot_path() {
    let path = output_path();
    let mut stronger = request(Some(&path));
    stronger.minimum = Minimum::SubmittedToCompositor;
    assert_eq!(
        render(stronger, Some(receipt(false)), None)
            .unwrap_err()
            .code,
        "observation_evidence_unavailable"
    );
    assert!(!path.exists());
    assert_eq!(
        render(request(Some(&path)), None, None).unwrap_err().code,
        "observation_unavailable"
    );
    assert!(!path.exists());
    assert_eq!(
        render(request(Some(&path)), Some(receipt(false)), None)
            .unwrap_err()
            .code,
        "observation_artifact_unavailable"
    );
    assert!(!path.exists());
}

#[test]
fn wrong_frame_or_decode_only_image_is_refused_before_file_creation() {
    for wrong_frame in [false, true] {
        let path = output_path();
        let mut original = receipt(true);
        if wrong_frame {
            original.serial += 1;
        } else {
            original.evidence = Evidence::Decoded;
        }
        assert_eq!(
            render(request(Some(&path)), Some(original), None)
                .unwrap_err()
                .code,
            "observation_artifact_mismatch"
        );
        assert!(!path.exists());
    }
}

#[test]
fn png_uses_actual_retina_viewport_and_original_frame_and_generations() {
    let path = output_path();
    let rendered = render(request(Some(&path)), Some(receipt(true)), None).unwrap();
    let value: Value = serde_json::from_str(&rendered).unwrap();
    let data = &value["data"];
    let image = &data["artifact"];
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
    assert_eq!(&bytes[16..20], &8_u32.to_be_bytes());
    assert_eq!(&bytes[20..24], &6_u32.to_be_bytes());
    assert_eq!(data["display"]["pixel_width"], 4);
    assert_eq!(data["display"]["pixel_height"], 3);
    assert_eq!(image["pixel_width"], 8);
    assert_eq!(image["pixel_height"], 6);
    assert_eq!(image["producing_frame_serial"], data["frame_serial"]);
    assert_eq!(
        image["producing_geometry_generation"],
        data["geometry_generation"]
    );
    assert_eq!(data["configuration_generation"], 3);
    assert_eq!(data["recovery_generation"], 7);
    assert_eq!(image["configuration_generation"], 3);
    assert_eq!(image["recovery_generation"], 7);
    assert_eq!(image["evidence_level"], "submitted_to_compositor");
    assert_eq!(image["pixel_domain"], "presented_viewport");
    assert_eq!(image["cursor_content"], "remote_overlay_if_present");
    assert_eq!(image["native_os_pointer_included"], false);
    assert_eq!(image["byte_count"], bytes.len());
    assert_eq!(
        image["sha256"],
        fr_client::robot::sha256::sha256_hex(&bytes)
    );
    assert_eq!(image["storage_path"], json!(path));
    assert!(data["control_authority"].is_null());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn existing_artifacts_are_preserved_and_text_is_explicitly_historical() {
    let path = output_path();
    std::fs::write(&path, b"original user content").unwrap();
    assert_eq!(
        render(request(Some(&path)), Some(receipt(true)), None)
            .unwrap_err()
            .code,
        "observation_artifact_exists"
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"original user content");
    let mut text = request(None);
    text.json = false;
    let rendered = render(text, Some(receipt(false)), None).unwrap();
    assert!(rendered.contains("the read-only session has ended"));
    assert!(rendered.contains("Original local receipt age: at most"));
    assert!(rendered.contains("No screenshot requested."));
    assert!(rendered.contains("remote cleanup is unknown"));
    assert!(rendered.contains("No input authority, physical visibility or semantic result"));
}

#[test]
fn host_close_report_and_acknowledgements_remain_independent_of_local_cleanup() {
    let host = Closed {
        reason: ClosedReason::ClientRequested,
        cleanup: Cleanup::Unconfirmed,
        effects: OutstandingEffects::Known {
            pending: 2,
            uncertain: 7,
        },
    };
    for acknowledged in [false, true] {
        for completed in [false, true] {
            for reported in [None, Some(host)] {
                let close = crate::closure::Report::test_report(acknowledged, completed, reported);
                let rendered = render(request(None), Some(receipt(false)), Some(close)).unwrap();
                let value: Value = serde_json::from_str(&rendered).unwrap();
                let exchange = &value["data"]["close_exchange"];
                assert_eq!(exchange["request_acknowledged"], acknowledged);
                assert_eq!(exchange["transport_completed"], completed);
                assert_eq!(value["data"]["local_cleanup_confirmed"], true);
                if reported.is_some() {
                    assert_eq!(exchange["host_report"]["cleanup_stage"], "unconfirmed");
                    assert_eq!(exchange["host_report"]["outstanding_effects"]["pending"], 2);
                    assert_eq!(
                        exchange["host_report"]["outstanding_effects"]["uncertain"],
                        7
                    );
                } else {
                    assert!(exchange["host_report"].is_null());
                }
            }
        }
    }
}
