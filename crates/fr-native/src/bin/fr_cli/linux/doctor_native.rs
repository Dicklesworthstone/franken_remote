//! Explicit, bounded local readiness: actual capture/HEVC plus optional input
//! discovery. No runtime, network, controller, retained picture or input effect.
use crate::{Failure, options::NativeDoctorOptions, output};
use fr_core::{
    input_submission::{Capabilities, Capability, PlatformError},
    limits::ProtocolLimits,
};
use frd::{
    input_process::{self, ProcessLaunch},
    worker::{EncoderProbe, Error as WorkerError, Launch, SpawnFailure},
};
use std::{fmt::Write as _, time::Duration};

const ENCODER_TIMEOUT: Duration = Duration::from_secs(15);
const INPUT_OPERATIONS: [(Capability, &str); 8] = [
    (Capability::Keys, "keys"),
    (Capability::Repeat, "repeat"),
    (Capability::Absolute, "absolute_pointer"),
    (Capability::Buttons, "buttons"),
    (Capability::Relative, "relative_pointer"),
    (Capability::PixelScroll, "pixel_scroll"),
    (Capability::LineScroll, "line_scroll"),
    (Capability::Text, "text"),
];

pub(super) fn run(options: &NativeDoctorOptions, json: bool) -> Result<(String, u8), Failure> {
    // Each one-shot child has its own private reply channel and is reaped by
    // the existing probe. The epoch is correlation only, never session authority.
    let launch = Launch::new(
        &options.worker,
        &options.display,
        options.xauthority.as_deref(),
        fr_media::worker::Role::Capture,
        1,
    );
    let encoder = launch.and_then(|launch| launch.probe_encoder(options.encoder, ENCODER_TIMEOUT));
    let input = options.input_agent.as_ref().map(|image| {
        ProcessLaunch::new(image, &options.display, options.xauthority.as_deref(), 1)
            .map_err(|_| PlatformError::Unavailable)
            .and_then(|launch| input_process::probe(&launch))
    });
    let report = Report::new(options.encoder, &options.display, encoder, input);
    Ok((
        report.render(json)?,
        u8::from(report.refusal_code.is_some()),
    ))
}

struct Report<'a> {
    encoder: &'a str,
    display: &'a str,
    permissions: Vec<output::DoctorPermissionReport>,
    capabilities: Vec<output::DoctorCapabilityReport>,
    input_capabilities: Vec<&'static str>,
    refusal_code: Option<&'static str>,
    next_action: Option<&'static str>,
}
impl<'a> Report<'a> {
    fn new(
        encoder: &'a str,
        display: &'a str,
        probe: Result<EncoderProbe, WorkerError>,
        input: Option<Result<Capabilities, PlatformError>>,
    ) -> Self {
        let mut report = Self {
            encoder,
            display,
            permissions: output::DoctorReport::unmeasured_permissions(),
            capabilities: output::DoctorReport::unmeasured_capabilities(),
            input_capabilities: Vec::new(),
            refusal_code: None,
            next_action: None,
        };
        report.encoder_result(probe);
        if let Some(input) = input {
            report.input_result(input);
        }
        report
    }

    fn capability(&mut self, name: &str) -> &mut output::DoctorCapabilityReport {
        self.capabilities
            .iter_mut()
            .find(|row| row.name == name)
            .expect("fixed doctor row")
    }
    fn refuse(&mut self, name: &str, status: &'static str, code: &'static str, next: &'static str) {
        let row = self.capability(name);
        row.status = status;
        row.detail = format!("{code}: {next}");
        if self.refusal_code.is_none() {
            self.refusal_code = Some(code);
            self.next_action = Some(next);
        }
    }
    fn encoder_result(&mut self, result: Result<EncoderProbe, WorkerError>) {
        let limits = ProtocolLimits::ABSOLUTE;
        if let Ok(EncoderProbe::Admitted { width, height }) = result {
            if width == 0
                || height == 0
                || width > limits.max_dimension_pixels()
                || height > limits.max_dimension_pixels()
                || u64::from(width) * u64::from(height) > limits.max_coded_pixels()
            {
                self.refuse("video_encode", "failed", "native_probe_invalid_reply",
                    "The selected worker returned invalid dimensions; install matching trusted fr and fr-media-worker binaries.");
                return;
            }
            let encoder = self.encoder;
            let row = self.capability("video_encode");
            row.status = "passed";
            row.hardware_accelerated = Some(encoder != "software");
            row.detail = format!(
                "{encoder}: one real {width}x{height} root-display capture encoded and admitted as HEVC IDR. No decode, presentation, sustained-rate or remote-session measurement."
            );
            let row = self.capability("screen_capture");
            row.status = "passed";
            row.detail = format!(
                "One {width}x{height} X11 root-display capture completed in the selected worker; selected-monitor and live-share behavior were not tested."
            );
            let permission = self
                .permissions
                .iter_mut()
                .find(|row| row.capability == "screen_capture")
                .expect("fixed permission row");
            permission.status = "granted";
            permission.detail = Some("The selected worker captured this local X11 display during this probe; this is no continuing permission or session authority.".into());
            return;
        }
        // This combined worker reply cannot attribute a failed capture versus
        // a failed encoder. Never invent a separate screen-capture failure.
        self.capability("screen_capture").detail = "The combined capture/encode probe did not complete; its reply cannot establish the individual capture outcome.".into();
        let (status, code, next) = match result {
            Ok(EncoderProbe::TimedOut) => (
                "blocked",
                "native_encoder_probe_timeout",
                "The selected capture/encoder worker exceeded 15 seconds and was stopped. Check local desktop and GPU availability, then rerun the same explicit probe.",
            ),
            Ok(EncoderProbe::Refused(reason))
                if matches!(reason.as_str(), "no_probe_reply" | "malformed_probe_reply") =>
            {
                (
                    "failed",
                    "native_probe_invalid_reply",
                    "The selected worker did not return a valid encoder probe result; install matching trusted fr and fr-media-worker binaries.",
                )
            }
            Ok(EncoderProbe::Refused(reason)) if reason == "GeometryChanged" => (
                "blocked",
                "native_probe_geometry_changed",
                "The selected display changed during capture. Let its geometry settle, then rerun the probe.",
            ),
            Ok(EncoderProbe::Refused(_)) => (
                "failed",
                "native_encoder_unavailable",
                "The selected display/backend could not produce an admitted HEVC IDR. Check the X display, its Xauthority, the selected encoder's driver and the installed worker's codec build. No other backend was tried.",
            ),
            Err(WorkerError::SpawnFailed(SpawnFailure::NotFound)) => (
                "blocked",
                "native_worker_missing",
                "Install fr-media-worker and select its absolute path with --worker.",
            ),
            Err(WorkerError::SpawnFailed(SpawnFailure::PermissionDenied)) => (
                "blocked",
                "native_worker_permission_denied",
                "Check that the locally selected installed worker is executable by this desktop user.",
            ),
            Err(_) => (
                "blocked",
                "native_worker_launch_failed",
                "The selected worker could not be launched or collected. Check local process resources and the installed worker; no backend fallback was attempted.",
            ),
            Ok(EncoderProbe::Admitted { .. }) => unreachable!("admission handled above"),
        };
        self.refuse("video_encode", status, code, next);
    }

    fn input_result(&mut self, result: Result<Capabilities, PlatformError>) {
        match result {
            Ok(capabilities) => {
                self.input_capabilities = INPUT_OPERATIONS
                    .iter()
                    .filter_map(|(capability, name)| {
                        capabilities.contains(*capability).then_some(*name)
                    })
                    .collect();
                let missing: Vec<_> = INPUT_OPERATIONS[..4]
                    .iter()
                    .filter_map(|(capability, name)| {
                        (!capabilities.contains(*capability)).then_some(*name)
                    })
                    .collect();
                if missing.is_empty() {
                    let names = self.input_capabilities.join(", ");
                    let row = self.capability("input_injection");
                    row.status = "passed";
                    row.detail = format!(
                        "The selected input agent discovered {names}. This is executor capability discovery; no key, pointer or text was injected and no application effect was measured."
                    );
                } else {
                    self.refuse("input_injection", "blocked", "native_input_capability_missing", "The selected input agent lacks an operation required by native control. Check XTEST/XKB availability on this display.");
                    let _ = write!(
                        self.capability("input_injection").detail,
                        " Missing: {}. No input was injected.",
                        missing.join(", ")
                    );
                }
            }
            Err(error) => {
                let code = match error {
                    PlatformError::Unsupported => "native_input_unsupported",
                    PlatformError::Permission => "native_input_permission_denied",
                    PlatformError::GeometryChanged => "native_input_geometry_changed",
                    PlatformError::Unavailable => "native_input_unavailable",
                };
                self.refuse("input_injection", "blocked", code, "Input capability discovery did not complete. Check the selected installed input agent, X display, Xauthority and XTEST/XKB availability. No input was injected.");
            }
        }
    }

    fn render(&self, json: bool) -> Result<String, Failure> {
        if json {
            let capabilities: Vec<_> = self.capabilities.iter().map(|row| serde_json::json!({
                "name": row.name, "status": row.status, "detail": row.detail,
                "hardware_accelerated": row.hardware_accelerated, "restriction": row.restriction,
            })).collect();
            let permissions: Vec<_> = self
                .permissions
                .iter()
                .map(|row| {
                    serde_json::json!({
                        "capability": row.capability, "status": row.status, "detail": row.detail,
                    })
                })
                .collect();
            let timestamp: serde_json::Value =
                serde_json::from_str(&output::timestamp()).unwrap_or(serde_json::Value::Null);
            serde_json::to_string(&serde_json::json!({
                "schema_version": 1, "timestamp_unix_ms": timestamp,
                "outcome": if self.refusal_code.is_some() { "refusal" } else { "success" },
                "scope": "local_native_probe", "encoder": self.encoder, "x_display": self.display,
                "tailnet_status": "not_tested", "certificate_status": "not_tested",
                "permissions": permissions, "capabilities": capabilities,
                "input_capabilities": self.input_capabilities,
                "refusal_code": self.refusal_code, "next_action": self.next_action,
            }))
            .map(|mut text| {
                text.push('\n');
                text
            })
            .map_err(|_| {
                Failure::new(
                    "output_failed",
                    "The local diagnostic report could not be encoded.",
                    74,
                )
            })
        } else {
            let mut text = format!(
                "FrankenRemote Local Native Doctor Report\n  X11 display: {}\n  Selected encoder: {}\n  Scope: one local capture/HEVC probe and optional input capability discovery.\n  Tailnet and certificates: not tested (run fr doctor separately).\n",
                self.display, self.encoder
            );
            for row in &self.capabilities {
                let _ = writeln!(text, "  {} [{}]: {}", row.name, row.status, row.detail);
            }
            if let Some(code) = self.refusal_code {
                let _ = writeln!(
                    text,
                    "  Refusal: {code}\n  Next action: {}",
                    self.next_action.unwrap_or("")
                );
            }
            Ok(text)
        }
    }
}

#[cfg(test)]
#[path = "doctor_native/tests.rs"]
mod tests;
