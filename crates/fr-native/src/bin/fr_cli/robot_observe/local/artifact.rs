//! Publish only a fully sealed live-owner image; retain original ages and pins.
use super::{Failure, Request};
use crate::session_control::{
    Error,
    screenshot::{Metadata, ReadError, Refusal},
};
use fr_media::worker::presentation_snapshot::Snapshot;
use serde_json::json;
use std::time::Instant;

pub(super) fn render(
    request: &Request,
    mut metadata: Metadata,
    snapshot: &Snapshot,
    round_trip_ms: u64,
) -> Result<String, Failure> {
    metadata.validate_image(snapshot).map_err(|_| protocol())?;
    metadata.elapse(round_trip_ms).map_err(|_| protocol())?;
    let path = request.screenshot.as_ref().ok_or_else(super::usage)?;
    check_age(request, &metadata, false)?;
    let began = Instant::now();
    let saved = super::super::artifact::write(
        path,
        snapshot.width(),
        snapshot.height(),
        snapshot.pixels(),
    )?;
    let elapsed =
        u64::try_from(began.elapsed().as_nanos().div_ceil(1_000_000)).map_err(|_| protocol())?;
    metadata.elapse(elapsed).map_err(|_| protocol())?;
    // File I/O cannot extend the input pin's original lifetime. A separately
    // requested receipt-age ceiling is checked again with an explicit late error.
    check_age(request, &metadata, true)?;
    let frame = metadata.observation.frame.as_ref().ok_or_else(protocol)?;
    let input_ready = metadata
        .control
        .as_ref()
        .is_some_and(|control| control.ready);
    let artifact = json!({
        "artifact_id": format!("viewport-{}-{}", frame.frame_serial, &saved.sha256[..12]),
        "producing_frame_serial": frame.frame_serial,
        "producing_geometry_generation": frame.geometry_generation,
        "configuration_generation": metadata.configuration_generation,
        "recovery_generation": metadata.recovery_generation,
        "capture_timestamp_unix_ms": null,
        "evidence_level": "submitted_to_compositor",
        "pixel_domain": "presented_viewport",
        "cursor_content": "remote_overlay_if_present",
        "native_os_pointer_included": false,
        "mime_type": "image/png",
        "pixel_width": snapshot.width(), "pixel_height": snapshot.height(),
        "byte_count": saved.byte_count, "sha256": saved.sha256, "storage_path": path,
    });
    if request.json {
        serde_json::to_string_pretty(&json!({
            "schema_version": 1, "timestamp_unix_ms": crate::output::timestamp(), "outcome": "success",
            "data": {
                "scope": "local_session_observation", "target": request.target,
                "connection_id": request.instance, "snapshot": metadata.observation,
                "receipt_age_upper_ms": frame.receipt_age_ms, "query_round_trip_ms": round_trip_ms,
                "source_freshness": if input_ready { "bounded_by_original_input_observation" } else { "unknown" },
                "capture_timestamp_unix_ms": null, "presentation_timestamp_unix_ms": null,
                "configuration_generation": metadata.configuration_generation,
                "recovery_generation": metadata.recovery_generation,
                "control_authority": metadata.control, "current_input_precondition": input_ready,
                "physical_visibility_proven": false, "new_connection_opened": false,
                "artifact": artifact,
            }
        })).map_err(|_| protocol())
    } else {
        let control = metadata.control.as_ref().filter(|control| control.ready).map_or_else(
            || "No current input precondition is retained. A new explicit observation is required before input.".to_owned(),
            |control| format!(
                "Original input lease: {}\nObservation ID: {}\nNext request ID: {}\nOriginal source age upper bound: {} ms",
                control.lease.as_deref().unwrap_or_default(),
                control.observation_id.unwrap_or_default(), control.next_request_id.unwrap_or_default(),
                control.source_age_upper_ms.unwrap_or_default(),
            ),
        );
        Ok(format!(
            "Local session screenshot: {}\nConnection ID: {}\nDisplay: {} ({}x{} source pixels), geometry generation {}\nFrame: {} (submitted_to_compositor), configuration {}, recovery {}\nScreenshot: {} (PNG, {}x{} presented viewport, {} bytes, SHA-256 {})\nLocal receipt age upper bound: {} ms\n{control}\nCapture/presentation wall times and physical visibility remain unknown. The image includes any remote overlay, excluding the native OS pointer. No new connection or input action was opened.\n",
            request.target,
            request.instance,
            frame.display_handle,
            frame.pixel_width,
            frame.pixel_height,
            frame.geometry_generation,
            frame.frame_serial,
            metadata.configuration_generation,
            metadata.recovery_generation,
            path.display(),
            snapshot.width(),
            snapshot.height(),
            saved.byte_count,
            artifact["sha256"].as_str().unwrap_or_default(),
            frame.receipt_age_ms,
        ))
    }
}
fn check_age(request: &Request, metadata: &Metadata, written: bool) -> Result<(), Failure> {
    let frame = metadata.observation.frame.as_ref().ok_or_else(protocol)?;
    if request
        .maximum_age_ms
        .is_some_and(|maximum| frame.receipt_age_ms > maximum)
    {
        return Err(Failure::new(
            "observation_receipt_too_old",
            if written {
                "The screenshot file was written, but the original receipt exceeded the requested age while writing it. No current input precondition is claimed; make a new explicit observation before acting."
            } else {
                "The original receipt exceeds the requested local age bound. No screenshot file was opened and no new observation or input was requested."
            },
            1,
        ));
    }
    Ok(())
}
fn protocol() -> Failure {
    Failure::new(
        "local_session_protocol",
        "The original owner returned inconsistent screenshot metadata. No new input authority is inferred.",
        1,
    )
}
pub(super) fn read_failure(error: ReadError) -> Failure {
    let code = match error {
        ReadError::Refused(Refusal::NotReady) => "observation_not_ready",
        ReadError::Refused(Refusal::NotCurrent) => "observation_artifact_changed",
        ReadError::Refused(Refusal::Busy) => "observation_artifact_busy",
        ReadError::Refused(Refusal::Expired) => "observation_artifact_expired",
        ReadError::Local(Error::InvalidInstance) => "invalid_connection_id",
        ReadError::Local(Error::InvalidTarget) => "invalid_local_target",
        ReadError::Local(Error::NoSession) => "no_local_session",
        ReadError::Refused(Refusal::Closed) | ReadError::Local(Error::WrongOwner) => {
            "local_session_changed"
        }
        ReadError::Local(Error::UntrustedPeer) => "local_session_untrusted",
        ReadError::Local(Error::Protocol) => "local_session_protocol",
        ReadError::Local(Error::Timeout) => "local_session_timeout",
        ReadError::Local(Error::Cancelled) => {
            return Failure::new(
                "cancelled",
                "Local screenshot lookup was cancelled; no screenshot file or input action was created.",
                130,
            );
        }
        ReadError::Refused(Refusal::Unavailable) | ReadError::Local(_) => {
            "observation_artifact_unavailable"
        }
    };
    Failure::new(
        code,
        "No screenshot file was opened. Use the original explicit --publish-observations or --accept-input owner and its exact connection_id. Observe again only after deciding whether the original picture remains useful; this command never retries or substitutes a newer input pin.",
        1,
    )
}

#[cfg(test)]
#[path = "artifact_tests.rs"]
mod tests;
