//! Historical native receipt projection. Unknown host-clock evidence stays null.
use super::{Failure, Minimum, Request};
use fr_native::desktop::reconnect::Observation;
use frd::{media::PresentationStage, session_startup::ViewerCloseOutcome};
use serde_json::{Value, json};
use std::time::{SystemTime, UNIX_EPOCH};

pub(super) fn unix_ms() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok())
}

fn data(
    observation: Observation,
    minimum: Minimum,
    receipt_time: Option<u64>,
) -> Result<Value, Failure> {
    let stage = observation.completion.stage;
    if minimum == Minimum::SubmittedToCompositor
        && stage != PresentationStage::SubmittedToCompositor
    {
        return Err(Failure::new(
            "observation_evidence_unavailable",
            "The actual decoder receipt did not meet the requested compositor-submission evidence level. No stronger evidence is inferred.",
            1,
        ));
    }
    let display = observation.display;
    Ok(json!({
        "scope": "one_shot_native_observation",
        "historical": true,
        "display": {
            "handle": display.handle.to_string(),
            "x": display.x,
            "y": display.y,
            "pixel_width": display.pixel_width,
            "pixel_height": display.pixel_height,
            "logical_width": display.logical_width,
            "logical_height": display.logical_height,
            "scale_numerator": display.scale_numerator,
            "scale_denominator": display.scale_denominator,
            "rotation_degrees": u32::from(display.rotation) * 90
        },
        "geometry_generation": display.geometry.as_raw(),
        "frame_serial": observation.completion.frame.as_raw(),
        "evidence_level": match stage {
            PresentationStage::DecodedOnly => "decoded",
            PresentationStage::SubmittedToCompositor => "submitted_to_compositor",
        },
        "configuration_generation": null,
        "source_freshness": "unknown",
        "capture_timestamp_unix_ms": null,
        "presentation_timestamp_unix_ms": null,
        "receipt_observed_unix_ms": receipt_time,
        "uncertainty_ms": null,
        "control_authority": null,
        "artifact": null,
        "physical_visibility_proven": false,
        "transport_qualified": false,
        "local_cleanup_confirmed": true
    }))
}

pub(super) fn render(
    request: &Request,
    observation: Option<Observation>,
    receipt_time: Option<u64>,
    close: Option<ViewerCloseOutcome>,
) -> Result<String, Failure> {
    let observation = observation.ok_or_else(|| {
        Failure::new(
            "observation_unavailable",
            "No initial native decoder receipt was retained. No display observation or screenshot is claimed.",
            1,
        )
    })?;
    let mut data = data(observation, request.minimum, receipt_time)?;
    data["target"] = json!(request.connection.target.node);
    data["close_exchange"] = close
        .map_or(Ok(Value::Null), |outcome| {
            serde_json::from_str(&crate::closure::Report::from(outcome).json())
        })
        .map_err(|_| serialization())?;
    if request.json {
        // No input acknowledgement stage applies. The explicit evidence level
        // above is the decoder/compositor boundary, never optical observation.
        serde_json::to_string(&json!({
            "schema_version": 1,
            "timestamp_unix_ms": unix_ms(),
            "outcome": "success",
            "data": data
        }))
        .map(|mut text| {
            text.push('\n');
            text
        })
        .map_err(|_| serialization())
    } else {
        let display = observation.display;
        let closing = close.map_or_else(
            || "No completed host close exchange; remote cleanup is unknown.".to_owned(),
            |outcome| crate::closure::Report::from(outcome).text(),
        );
        Ok(format!(
            "One-shot observation complete; the read-only session has ended.\nTarget: {}\nDisplay: {} ({}x{} pixels), geometry generation {}\nInitial frame: {}, evidence: {}\nSource freshness, capture/presentation times and codec generation: unknown.\nLocal native cleanup confirmed. {closing}\nNo screenshot, input authority, physical visibility or semantic result is claimed.\n",
            request.connection.target.node,
            display.handle,
            display.pixel_width,
            display.pixel_height,
            display.geometry.as_raw(),
            observation.completion.frame.as_raw(),
            data["evidence_level"].as_str().unwrap_or("unknown"),
        ))
    }
}

fn serialization() -> Failure {
    Failure::new(
        "serialization_error",
        "Failed to render native observation metadata.",
        1,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use fr_core::ids::DisplayGeometryGeneration;
    use fr_media::access_unit::FrameId;
    use fr_wire::display::Display;
    use frd::session_startup::Presentation;

    fn observation(stage: PresentationStage) -> Observation {
        Observation {
            display: Display {
                handle: u128::MAX,
                geometry: DisplayGeometryGeneration::INITIAL,
                x: -1280,
                y: 0,
                pixel_width: 1280,
                pixel_height: 720,
                logical_width: 640,
                logical_height: 360,
                scale_numerator: 2,
                scale_denominator: 1,
                rotation: 1,
            },
            completion: Presentation {
                frame: FrameId::FIRST,
                stage,
            },
        }
    }

    #[test]
    fn metadata_preserves_actual_geometry_and_frame_zero_without_inventing_freshness() {
        let value = data(
            observation(PresentationStage::DecodedOnly),
            Minimum::Decoded,
            Some(1234),
        )
        .unwrap();
        assert_eq!(value["display"]["handle"], u128::MAX.to_string());
        assert_eq!(value["display"]["x"], -1280);
        assert_eq!(value["display"]["rotation_degrees"], 90);
        assert_eq!(value["display"]["scale_numerator"], 2);
        assert_eq!(value["frame_serial"], 0);
        assert_eq!(value["evidence_level"], "decoded");
        assert_eq!(value["receipt_observed_unix_ms"], 1234);
        assert_eq!(value["source_freshness"], "unknown");
        for field in [
            "configuration_generation",
            "capture_timestamp_unix_ms",
            "presentation_timestamp_unix_ms",
            "uncertainty_ms",
            "control_authority",
            "artifact",
        ] {
            assert_eq!(value.get(field), Some(&Value::Null), "{field}");
        }
        assert_eq!(value["physical_visibility_proven"], false);
        assert_eq!(value["transport_qualified"], false);
        assert_eq!(value["historical"], true);
    }

    #[test]
    fn requested_evidence_is_a_minimum_not_permission_to_upgrade_the_receipt() {
        assert_eq!(
            data(
                observation(PresentationStage::DecodedOnly),
                Minimum::SubmittedToCompositor,
                None,
            )
            .unwrap_err()
            .code,
            "observation_evidence_unavailable"
        );
        for minimum in [Minimum::Decoded, Minimum::SubmittedToCompositor] {
            let value = data(
                observation(PresentationStage::SubmittedToCompositor),
                minimum,
                None,
            )
            .unwrap();
            assert_eq!(value["evidence_level"], "submitted_to_compositor");
            assert_eq!(value["receipt_observed_unix_ms"], Value::Null);
            assert_eq!(value["physical_visibility_proven"], false);
        }
    }

    #[test]
    fn complete_envelope_keeps_unknown_close_and_omits_input_acknowledgements() {
        let args = "robot observe n-host --native-once --experimental-native --display only --json"
            .split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let request = super::super::parse(&args).unwrap();
        let frame = observation(PresentationStage::SubmittedToCompositor);
        let text = render(&request, Some(frame), Some(1234), None).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["outcome"], "success");
        assert_eq!(value["data"]["target"], "n-host");
        assert_eq!(value["data"]["close_exchange"], Value::Null);
        assert_eq!(value["data"]["local_cleanup_confirmed"], true);
        assert!(value.get("stage").is_none());
        assert!(value.get("lease").is_none());
        assert_eq!(
            render(&request, None, None, None).unwrap_err().code,
            "observation_unavailable"
        );
    }
}
