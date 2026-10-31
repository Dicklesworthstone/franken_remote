use super::*;
use fr_client::robot::live::{Evidence, Observation};
use fr_core::ids::{CodecConfigurationGeneration, DisplayGeometryGeneration, RecoveryGeneration};
use fr_media::{access_unit::FrameId, worker::presentation_snapshot::Stamp};
use fr_wire::display::Display;
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
fn path() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "fr-live-image-{}-{}.png",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}
fn request(path: &std::path::Path) -> Request {
    Request {
        target: "n-host".into(),
        instance: "a3".repeat(16),
        maximum_age_ms: None,
        minimum: crate::robot_observe::Minimum::Decoded,
        json: true,
        screenshot: Some(path.to_owned()),
    }
}
fn fixture(control: bool) -> (Metadata, Snapshot) {
    let image = Snapshot::decode(
        Snapshot::encode(
            Stamp {
                frame: FrameId::from_raw(0),
                configuration: CodecConfigurationGeneration::INITIAL,
                recovery: RecoveryGeneration::INITIAL,
            },
            2,
            2,
            &[3, 2, 1, 255].repeat(4),
        )
        .unwrap(),
    )
    .unwrap();
    let mut observation = Observation::default();
    observation.begin(1).unwrap();
    observation.authenticated().unwrap();
    observation
        .ready(
            Display {
                handle: 17,
                geometry: DisplayGeometryGeneration::INITIAL,
                x: -800,
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
            Evidence::SubmittedToCompositor,
            Instant::now(),
        )
        .unwrap();
    let control = control.then(|| crate::robot_input::Snapshot {
        ready: true,
        lease: Some("02".repeat(16)),
        observation_id: Some(7),
        next_request_id: Some(11),
        geometry_generation: Some(0),
        viewport_generation: Some(0),
        configuration_generation: Some(0),
        recovery_generation: Some(0),
        frame: Some(0),
        source_age_upper_ms: Some(990),
        physical_visibility_proven: false,
    });
    (
        Metadata::new(
            observation.snapshot(Instant::now()).unwrap(),
            &image,
            control,
        )
        .unwrap(),
        image,
    )
}
#[test]
fn live_view_screenshot_is_bound_without_claiming_input_or_new_connection() {
    let output = path();
    let (metadata, image) = fixture(false);
    let text = render(&request(&output), metadata, &image, 5).unwrap();
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    let data = &value["data"];
    assert_eq!(data["new_connection_opened"], false);
    assert_eq!(data["current_input_precondition"], false);
    assert!(data["control_authority"].is_null());
    assert_eq!(data["artifact"]["producing_frame_serial"], 0);
    assert_eq!(data["artifact"]["pixel_width"], 2);
    assert_eq!(data["snapshot"]["frame"]["pixel_width"], 800);
    assert_eq!(data["artifact"]["native_os_pointer_included"], false);
    assert_eq!(&std::fs::read(output).unwrap()[..8], b"\x89PNG\r\n\x1a\n");
}
#[test]
fn delivery_expiry_keeps_historical_pixels_but_clears_input_handles_without_repinning() {
    let output = path();
    let (metadata, image) = fixture(true);
    let text = render(&request(&output), metadata, &image, 10).unwrap();
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    let data = &value["data"];
    assert_eq!(data["current_input_precondition"], false);
    assert_eq!(data["control_authority"]["ready"], false);
    assert!(data["control_authority"]["lease"].is_null());
    assert!(data["control_authority"]["observation_id"].is_null());
    assert_eq!(data["control_authority"]["next_request_id"], 11);
    assert_eq!(data["artifact"]["producing_frame_serial"], 0);
    assert!(output.exists());
}
#[test]
fn fresh_control_report_reuses_the_exported_frames_original_input_pin() {
    let output = path();
    let (mut metadata, image) = fixture(true);
    metadata.control.as_mut().unwrap().source_age_upper_ms = Some(0);
    let text = render(&request(&output), metadata, &image, 1).unwrap();
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    let data = &value["data"];
    assert_eq!(data["current_input_precondition"], true);
    assert_eq!(data["control_authority"]["observation_id"], 7);
    assert_eq!(data["control_authority"]["next_request_id"], 11);
    assert_eq!(
        data["control_authority"]["frame"],
        data["artifact"]["producing_frame_serial"]
    );
}
#[test]
fn wrong_frame_or_old_receipt_refuses_before_creating_a_file() {
    for wrong_frame in [false, true] {
        let output = path();
        let (mut metadata, image) = fixture(false);
        let mut request = request(&output);
        if wrong_frame {
            metadata.observation.frame.as_mut().unwrap().frame_serial = 1;
        } else {
            request.maximum_age_ms = Some(1);
        }
        assert!(render(&request, metadata, &image, 2).is_err());
        assert!(!output.exists());
    }
}
