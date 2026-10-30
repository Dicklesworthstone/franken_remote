use super::*;
use fr_core::ids::{CodecConfigurationGeneration, DisplayGeometryGeneration, RecoveryGeneration};
use fr_media::{access_unit::FrameId, worker::presentation_snapshot::Stamp};
use fr_wire::display::Display;
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

fn request(fitted: bool) -> (Request, PathBuf) {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let directory = std::env::temp_dir().join(format!(
        "fr-observation-report-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::create_dir(&directory).unwrap();
    let path = directory.join("screen.png");
    let mut args = "robot observe n-host --native-once --experimental-native --display only --json"
        .split_whitespace()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    args.extend(["--screenshot".to_owned(), path.to_str().unwrap().to_owned()]);
    if fitted {
        args.extend(["--fit".to_owned(), "32x24".to_owned()]);
    }
    (super::super::parse(&args).unwrap(), path)
}

fn observation(stage: PresentationStage) -> Observation {
    Observation {
        display: Display {
            handle: 7,
            geometry: DisplayGeometryGeneration::INITIAL,
            x: -1280,
            y: 0,
            pixel_width: 1280,
            pixel_height: 720,
            logical_width: 1280,
            logical_height: 720,
            scale_numerator: 1,
            scale_denominator: 1,
            rotation: 0,
        },
        completion: frd::session_startup::Presentation {
            frame: FrameId::FIRST,
            stage,
        },
    }
}

fn image(frame: u64, width: u32, height: u32) -> Snapshot {
    let stamp = Stamp {
        frame: FrameId::from_raw(frame),
        configuration: CodecConfigurationGeneration::INITIAL,
        recovery: RecoveryGeneration::INITIAL,
    };
    let pixels = [3, 2, 1, 255].repeat(usize::try_from(width * height).unwrap());
    Snapshot::decode(Snapshot::encode(stamp, width, height, &pixels).unwrap()).unwrap()
}

#[test]
fn screenshot_report_binds_real_frame_and_viewport_without_fabricating_capture_time() {
    let (request, path) = request(true);
    let rendered = render(
        &request,
        Some(observation(PresentationStage::SubmittedToCompositor)),
        Some(1234),
        None,
        Some(image(0, 32, 24)),
    )
    .unwrap();
    let result: Value = serde_json::from_str(&rendered).unwrap();
    let data = &result["data"];
    assert_eq!(data["frame_serial"], 0);
    assert_eq!(data["display"]["pixel_width"], 1280);
    assert_eq!(data["configuration_generation"], 0);
    assert_eq!(data["recovery_generation"], 0);
    assert_eq!(
        data["artifact"]["producing_frame_serial"],
        data["frame_serial"]
    );
    assert_eq!(
        data["artifact"]["producing_geometry_generation"],
        data["geometry_generation"]
    );
    assert_eq!(data["artifact"]["pixel_width"], 32);
    assert_eq!(data["artifact"]["pixel_height"], 24);
    assert_eq!(data["artifact"]["pixel_domain"], "presented_viewport");
    assert_eq!(
        data["artifact"]["evidence_level"],
        "submitted_to_compositor"
    );
    assert_eq!(data["artifact"]["capture_timestamp_unix_ms"], Value::Null);
    assert_eq!(data["capture_timestamp_unix_ms"], Value::Null);
    assert_eq!(data["source_freshness"], "unknown");
    assert_eq!(data["artifact"]["native_os_pointer_included"], false);
    let png = std::fs::read(path).unwrap();
    assert_eq!(data["artifact"]["byte_count"], png.len());
    assert_eq!(
        data["artifact"]["sha256"],
        fr_client::robot::sha256::sha256_hex(&png)
    );
}

#[test]
fn absent_wrong_frame_wrong_viewport_and_unpresented_pixels_never_open_output() {
    for (snapshot, stage, code) in [
        (
            None,
            PresentationStage::SubmittedToCompositor,
            "observation_artifact_unavailable",
        ),
        (
            Some(image(1, 32, 24)),
            PresentationStage::SubmittedToCompositor,
            "observation_artifact_mismatch",
        ),
        (
            Some(image(0, 32, 32)),
            PresentationStage::SubmittedToCompositor,
            "observation_artifact_mismatch",
        ),
        (
            Some(image(0, 32, 24)),
            PresentationStage::DecodedOnly,
            "observation_artifact_mismatch",
        ),
    ] {
        let (request, path) = request(true);
        let error = render(&request, Some(observation(stage)), None, None, snapshot).unwrap_err();
        assert_eq!(error.code, code);
        assert!(!path.exists());
    }
}

#[test]
fn stronger_requested_evidence_and_missing_observation_refuse_before_file_creation() {
    let (mut request, path) = request(true);
    request.minimum = Minimum::SubmittedToCompositor;
    let error = render(
        &request,
        Some(observation(PresentationStage::DecodedOnly)),
        None,
        None,
        Some(image(0, 32, 24)),
    )
    .unwrap_err();
    assert_eq!(error.code, "observation_evidence_unavailable");
    assert!(!path.exists());
    assert_eq!(
        render(&request, None, None, None, Some(image(0, 32, 24)))
            .unwrap_err()
            .code,
        "observation_unavailable"
    );
    assert!(!path.exists());
}

#[test]
fn screenshots_match_the_actual_clamped_viewport_for_oversized_and_mixed_fit_bounds() {
    for (requested, actual) in [
        ((1920, 1080), (1280, 720)),
        ((640, 1080), (640, 720)),
        ((1920, 360), (1280, 360)),
    ] {
        let (mut request, path) = request(true);
        request.connection.fit_window = Some(requested);
        let value = render(
            &request,
            Some(observation(PresentationStage::SubmittedToCompositor)),
            None,
            None,
            Some(image(0, actual.0, actual.1)),
        )
        .unwrap();
        let report: Value = serde_json::from_str(&value).unwrap();
        assert_eq!(report["data"]["artifact"]["pixel_width"], actual.0);
        assert_eq!(report["data"]["artifact"]["pixel_height"], actual.1);
        assert!(path.is_file());
    }
}
