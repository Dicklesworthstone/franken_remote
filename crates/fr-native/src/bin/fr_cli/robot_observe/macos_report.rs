//! Historical Mac receipt, consumed only after the original operation and cleanup.
//! Native readback owns viewport geometry; a host display size is not a window size.
use super::{Failure, Minimum, artifact};
use fr_client::robot::live::Evidence;
use fr_media::worker::presentation_snapshot::Snapshot;
use fr_wire::display::Display;
use serde_json::{Value, json};
use std::{
    path::Path,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Copy)]
pub(crate) struct Request<'a> {
    pub target: &'a str,
    pub json: bool,
    pub minimum: Minimum,
    pub screenshot: Option<&'a Path>,
}

/// Moved from the original session after its supervised completion. Pixel data
/// is neither cloned nor formatted; the monotonic receipt time is unchanged.
pub(crate) struct Receipt {
    pub display: Display,
    pub serial: u64,
    pub evidence: Evidence,
    pub received_at: Instant,
    pub snapshot: Option<Snapshot>,
}

pub(crate) fn render(
    request: Request<'_>,
    original: Option<Receipt>,
    close: Option<crate::closure::Report>,
) -> Result<String, Failure> {
    let original = original.ok_or_else(|| {
        Failure::new(
            "observation_unavailable",
            "The completed original native session retained no initial receipt. No display observation or screenshot is claimed.",
            1,
        )
    })?;
    let mut data = metadata(&request, &original)?;
    data["close_exchange"] = close
        .map_or(Ok(Value::Null), |report| {
            serde_json::from_str(&report.json())
        })
        .map_err(|_| serialization())?;
    // Evidence, receipt identity and dimensions are checked before opening the
    // output path. The caller has already required actual operation + cleanup.
    if let Some(image) = save_artifact(request.screenshot, &original)? {
        data["configuration_generation"] = image["configuration_generation"].clone();
        data["recovery_generation"] = image["recovery_generation"].clone();
        data["artifact"] = image;
    }
    // Readback, bootstrap acknowledgement, closing and PNG I/O all age the
    // original receipt. No wall-clock observation timestamp is synthesized.
    data["receipt_age_upper_ms"] = json!(receipt_age(original.received_at, Instant::now()));
    if request.json {
        return serde_json::to_string(&json!({
            "schema_version": 1,
            "timestamp_unix_ms": unix_ms(),
            "outcome": "success",
            "data": data,
        }))
        .map(|mut text| {
            text.push('\n');
            text
        })
        .map_err(|_| serialization());
    }
    let closing = close.map_or_else(
        || "No completed host close exchange; remote cleanup is unknown.".to_owned(),
        crate::closure::Report::text,
    );
    let image = if data["artifact"].is_null() {
        "No screenshot requested.".to_owned()
    } else {
        format!(
            "Screenshot: {} (PNG, {}x{} presented viewport, {} bytes, SHA-256 {}). Remote overlay may be included; the native OS pointer is excluded.",
            data["artifact"]["storage_path"]
                .as_str()
                .unwrap_or_default(),
            data["artifact"]["pixel_width"],
            data["artifact"]["pixel_height"],
            data["artifact"]["byte_count"],
            data["artifact"]["sha256"].as_str().unwrap_or_default(),
        )
    };
    let age = data["receipt_age_upper_ms"]
        .as_u64()
        .map_or_else(|| "unknown".to_owned(), |age| format!("at most {age} ms"));
    Ok(format!(
        "One-shot observation complete; the read-only session has ended.\nTarget: {}\nDisplay: {} ({}x{} pixels), geometry generation {}\nInitial frame: {}, evidence: {}\nOriginal local receipt age: {age}.\nSource freshness and capture/presentation times: unknown.\nLocal native cleanup confirmed. {closing}\n{image}\nNo input authority, physical visibility or semantic result is claimed.\n",
        request.target,
        original.display.handle,
        original.display.pixel_width,
        original.display.pixel_height,
        original.display.geometry.as_raw(),
        original.serial,
        evidence(original.evidence),
    ))
}

fn metadata(request: &Request<'_>, original: &Receipt) -> Result<Value, Failure> {
    if request.minimum == Minimum::SubmittedToCompositor
        && original.evidence != Evidence::SubmittedToCompositor
    {
        return Err(Failure::new(
            "observation_evidence_unavailable",
            "The original native receipt did not meet the requested compositor-submission evidence level. No stronger evidence or screenshot is claimed.",
            1,
        ));
    }
    let display = original.display;
    Ok(json!({
        "scope": "one_shot_native_observation",
        "historical": true,
        "target": request.target,
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
            "rotation_degrees": u32::from(display.rotation) * 90,
        },
        "geometry_generation": display.geometry.as_raw(),
        "frame_serial": original.serial,
        "evidence_level": evidence(original.evidence),
        "configuration_generation": null,
        "recovery_generation": null,
        "source_freshness": "unknown",
        "capture_timestamp_unix_ms": null,
        "presentation_timestamp_unix_ms": null,
        "receipt_observed_unix_ms": null,
        "uncertainty_ms": null,
        "control_authority": null,
        "artifact": null,
        "physical_visibility_proven": false,
        "transport_qualified": false,
        "local_cleanup_confirmed": true,
    }))
}

fn save_artifact(path: Option<&Path>, original: &Receipt) -> Result<Option<Value>, Failure> {
    let Some(path) = path else {
        return Ok(None);
    };
    let snapshot = original.snapshot.as_ref().ok_or_else(|| {
        Failure::new(
            "observation_artifact_unavailable",
            "The original native media owner supplied no initial screenshot. No screenshot file was opened.",
            1,
        )
    })?;
    let stamp = snapshot.stamp();
    if stamp.frame.as_raw() != original.serial
        || original.evidence != Evidence::SubmittedToCompositor
    {
        return Err(Failure::new(
            "observation_artifact_mismatch",
            "The screenshot does not match the original presented frame. No screenshot file was opened.",
            1,
        ));
    }
    // snapshot_initial already fenced the original configuration/recovery and
    // native geometry revision. Snapshot validates absolute dimensions and the
    // exact pixel length; do not clamp a Retina viewport to the source display.
    let saved = artifact::write(path, snapshot.width(), snapshot.height(), snapshot.pixels())?;
    Ok(Some(json!({
        "artifact_id": format!("viewport-{}-{}", original.serial, &saved.sha256[..12]),
        "producing_frame_serial": original.serial,
        "producing_geometry_generation": original.display.geometry.as_raw(),
        "configuration_generation": stamp.configuration.as_raw(),
        "recovery_generation": stamp.recovery.as_raw(),
        "capture_timestamp_unix_ms": null,
        "evidence_level": "submitted_to_compositor",
        "pixel_domain": "presented_viewport",
        "cursor_content": "remote_overlay_if_present",
        "native_os_pointer_included": false,
        "mime_type": "image/png",
        "pixel_width": snapshot.width(),
        "pixel_height": snapshot.height(),
        "byte_count": saved.byte_count,
        "sha256": saved.sha256,
        "storage_path": path,
    })))
}

fn receipt_age(received_at: Instant, reported_at: Instant) -> Option<u64> {
    reported_at
        .checked_duration_since(received_at)
        .and_then(|elapsed| u64::try_from(elapsed.as_nanos().div_ceil(1_000_000)).ok())
}

const fn evidence(evidence: Evidence) -> &'static str {
    match evidence {
        Evidence::Decoded => "decoded",
        Evidence::SubmittedToCompositor => "submitted_to_compositor",
    }
}

fn unix_ms() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok())
}

fn serialization() -> Failure {
    Failure::new(
        "serialization_error",
        "Failed to render native observation metadata.",
        1,
    )
}

#[cfg(test)]
#[path = "macos_report_tests.rs"]
mod tests;
