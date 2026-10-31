use super::*;
use fr_client::robot::live::{Evidence, Observation};
use fr_core::ids::{CodecConfigurationGeneration, DisplayGeometryGeneration, RecoveryGeneration};
use fr_media::{access_unit::FrameId, worker::presentation_snapshot::Stamp};
use fr_wire::display::Display;

pub(super) fn image() -> Snapshot {
    Snapshot::decode(
        Snapshot::encode(
            Stamp {
                frame: FrameId::from_raw(0),
                configuration: CodecConfigurationGeneration::INITIAL,
                recovery: RecoveryGeneration::INITIAL,
            },
            2,
            2,
            &[1, 2, 3, 255].repeat(4),
        )
        .unwrap(),
    )
    .unwrap()
}
pub(super) fn metadata(control: bool) -> Metadata {
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
        lease: Some("01".repeat(16)),
        observation_id: Some(5),
        next_request_id: Some(1),
        geometry_generation: Some(0),
        viewport_generation: Some(0),
        configuration_generation: Some(0),
        recovery_generation: Some(0),
        frame: Some(0),
        source_age_upper_ms: Some(500),
        physical_visibility_proven: false,
    });
    Metadata::new(
        observation.snapshot(Instant::now()).unwrap(),
        &image(),
        control,
    )
    .unwrap()
}
#[test]
fn framing_bounds_and_owner_are_checked_before_any_pixel_allocation() {
    let id = [2; 16];
    for size in [
        0,
        presentation_snapshot::HEADER_BYTES + 4,
        presentation_snapshot::MAX_BODY_BYTES,
    ] {
        let value = header(id, MAX_METADATA, size).unwrap();
        assert_eq!(reply_lengths(&value, id), Ok((MAX_METADATA, size)));
        assert_eq!(reply_lengths(&value, [3; 16]), Err(Error::Protocol));
    }
    for metadata in [0, MAX_METADATA + 1, usize::from(u16::MAX)] {
        assert_eq!(header(id, metadata, 0), Err(Error::Protocol));
    }
    for pixels in [1, 31, 32, 33, 35, presentation_snapshot::MAX_BODY_BYTES + 4] {
        assert_eq!(header(id, 1, pixels), Err(Error::Protocol));
    }
    let mut value = header(id, 1, 0).unwrap();
    value[0] ^= 1;
    assert_eq!(reply_lengths(&value, id), Err(Error::Protocol));
}
#[test]
fn image_must_match_the_original_frame_geometry_and_all_known_generations() {
    let valid = metadata(true);
    assert_eq!(valid.validate_image(&image()), Ok(()));
    assert_eq!(
        valid.validate_length(presentation_snapshot::HEADER_BYTES + 16),
        Ok(())
    );
    assert_eq!(
        valid.validate_length(presentation_snapshot::MAX_BODY_BYTES),
        Err(Error::Protocol)
    );
    for field in 0..8 {
        let mut bad = valid.clone();
        match field {
            0 => bad.observation.frame.as_mut().unwrap().frame_serial += 1,
            1 => bad.configuration_generation += 1,
            2 => bad.recovery_generation += 1,
            3 => bad.viewport_width += 1,
            4 => bad.viewport_height += 1,
            5 => bad.control.as_mut().unwrap().geometry_generation = Some(1),
            6 => bad.control.as_mut().unwrap().configuration_generation = Some(1),
            _ => bad.control.as_mut().unwrap().recovery_generation = Some(1),
        }
        assert_eq!(
            bad.validate_image(&image()),
            Err(Error::Protocol),
            "field {field}"
        );
    }
    let mut decoded = valid;
    decoded.observation.frame.as_mut().unwrap().evidence_level = Evidence::Decoded;
    assert_eq!(decoded.validate_image(&image()), Err(Error::Protocol));
}
#[test]
fn original_input_age_includes_delivery_and_writing_and_expiry_clears_handles() {
    let mut value = metadata(true);
    let original_id = value.control.as_ref().unwrap().observation_id;
    value.elapse(499).unwrap();
    assert_eq!(
        value.control.as_ref().unwrap().source_age_upper_ms,
        Some(999)
    );
    assert_eq!(value.control.as_ref().unwrap().observation_id, original_id);
    value.elapse(1).unwrap();
    let expired = value.control.as_ref().unwrap();
    assert!(!expired.ready);
    assert_eq!(expired.lease, None);
    assert_eq!(expired.observation_id, None);
    assert_eq!(expired.frame, None);
    assert_eq!(expired.source_age_upper_ms, None);
    assert_eq!(expired.next_request_id, Some(1));
    assert_eq!(
        value.validate_image(&image()),
        Ok(()),
        "historical pixels remain bound"
    );
}
#[test]
fn complete_metadata_is_bounded_and_rejects_unknown_fields() {
    let reply = Reply::Image {
        metadata: Box::new(metadata(true)),
    };
    let bytes = serde_json::to_vec(&reply).unwrap();
    assert!(bytes.len() <= MAX_METADATA);
    assert_eq!(serde_json::from_slice::<Reply>(&bytes).unwrap(), reply);
    let mut value = serde_json::to_value(reply).unwrap();
    value["metadata"]["pixels"] = serde_json::json!([1, 2, 3]);
    assert!(serde_json::from_value::<Reply>(value).is_err());
}

#[cfg(feature = "linux-desktop")]
#[path = "ipc_tests.rs"]
mod ipc;
