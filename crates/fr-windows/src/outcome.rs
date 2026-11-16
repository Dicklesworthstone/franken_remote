//! Plain-language summaries of a viewer's completion document, shown by the
//! desktop application when it returns to the machine picker.
//!
//! The summary is derived only from the content-free completion fields
//! (`status`, `end`, `reason`, `drawn`, `dxva`, `seconds`); it never adds a
//! claim the document does not make. `dxva` is the session core's decode
//! path: 1 hardware, 0 software, -1 not yet known.
#![forbid(unsafe_code)]

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Summary {
    /// The session showed pictures and ended in order.
    pub ok: bool,
    /// One sentence for the picker's notice line.
    pub message: String,
}

/// `machine` is the short display name of the machine that was chosen.
pub(crate) fn summarize(completion: &str, machine: &str) -> Summary {
    let Ok(document) = serde_json::from_str::<Value>(completion.trim()) else {
        return Summary {
            ok: false,
            message: format!("The session with {machine} ended without a report."),
        };
    };
    if document["status"] == "refused" {
        let reason = document["reason"].as_str().unwrap_or("unknown");
        return Summary {
            ok: false,
            message: refused(reason, machine),
        };
    }
    let end = document["end"].as_str().unwrap_or("unknown");
    let drawn = document["drawn"].as_u64().unwrap_or(0);
    let seconds = document["seconds"].as_f64().unwrap_or(0.0);
    let decode = match document["dxva"].as_i64() {
        Some(1) => " Decoded on the GPU.",
        Some(0) if drawn > 0 => " Decoded on the CPU (no GPU HEVC decoding).",
        _ => "",
    };
    let ok = drawn > 0 && matches!(end, "ok" | "cancelled");
    let message = match end {
        "ok" | "cancelled" if drawn > 0 => {
            format!(
                "Disconnected from {machine} after {}.{decode}",
                duration(seconds)
            )
        }
        "ok" | "cancelled" => {
            format!("The session with {machine} closed before a picture arrived.")
        }
        "unreachable" => format!(
            "{machine} did not answer. Make sure FrankenRemote is hosting there and that your tailnet policy allows UDP 8443 from this PC."
        ),
        "host_busy" => format!("{machine} is serving another viewer. Try again when it is free."),
        "host_refused" => format!(
            "{machine} refused this viewer (its sharing scope, local approval or control setting)."
        ),
        "host_closed" => format!("{machine} ended the session.{decode}"),
        "tls_refused" => format!(
            "{machine}'s certificate could not be verified. Tailscale HTTPS certificates must be enabled for your tailnet."
        ),
        "name_unresolved" | "outside_tailnet" | "identity_changed" => format!(
            "{machine} is no longer the machine that was listed (renamed, removed or re-keyed). Refresh and choose again."
        ),
        "control_stopped" => format!("Control of {machine} stopped.{decode}"),
        "session_failed" if drawn == 0 => format!(
            "The session with {machine} failed before a picture was shown. Check that this PC can decode HEVC."
        ),
        "session_failed" => format!(
            "The session with {machine} failed after {}.",
            duration(seconds)
        ),
        _ => format!("The session with {machine} ended unexpectedly ({end})."),
    };
    Summary { ok, message }
}

fn refused(reason: &str, machine: &str) -> String {
    match reason {
        "no_d3d11_device" => {
            "This PC has no Direct3D 11 graphics device to show the remote desktop.".to_owned()
        }
        "window_unavailable" | "swap_chain_unavailable" => {
            "Windows could not create the viewer window.".to_owned()
        }
        _ => format!("Could not start a session with {machine} ({reason})."),
    }
}

/// The status-bar text for this PC's decode path and, when the path is
/// degraded or missing, a warning for the notice line. These are local
/// capability observations, not a claim that a session will decode.
pub(crate) fn decoder_status(
    gpu_device: bool,
    decoder_installed: bool,
    gpu_hevc_profile: bool,
    adapter: Option<&str>,
) -> (String, Option<String>) {
    let adapter = adapter.map_or_else(String::new, |name| format!(" ({name})"));
    match (gpu_device, decoder_installed, gpu_hevc_profile) {
        (false, _, _) => (
            "No Direct3D 11 GPU".to_owned(),
            Some(
                "This PC has no Direct3D 11 graphics device, so remote desktops cannot be shown."
                    .to_owned(),
            ),
        ),
        (true, false, _) => (
            "HEVC decoder missing".to_owned(),
            Some(
                "Windows' HEVC decoder is not installed. Get Microsoft's HEVC Video Extensions from the Store, then reopen FrankenRemote."
                    .to_owned(),
            ),
        ),
        (true, true, true) => (format!("GPU HEVC decoding{adapter}"), None),
        (true, true, false) => (
            format!("CPU HEVC decoding{adapter}"),
            Some(
                "This GPU has no HEVC decoder, so pictures are decoded on the CPU. Large desktops may be less smooth."
                    .to_owned(),
            ),
        ),
    }
}

/// "45 s", "3 min 05 s", "1 h 02 min".
pub(crate) fn duration(seconds: f64) -> String {
    let total = if seconds.is_finite() && seconds > 0.0 {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "positive, finite and rounded first"
        )]
        let whole = seconds.round().min(f64::from(u32::MAX)) as u64;
        whole
    } else {
        0
    };
    match total {
        0..60 => format!("{total} s"),
        60..3600 => format!("{} min {:02} s", total / 60, total % 60),
        _ => format!("{} h {:02} min", total / 3600, (total % 3600) / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orderly_sessions_report_duration_and_decode_path() {
        let gpu = summarize(
            r#"{"status":"ended","end":"ok","drawn":120,"dxva":1,"seconds":185.4}"#,
            "studio",
        );
        assert!(gpu.ok);
        assert_eq!(
            gpu.message,
            "Disconnected from studio after 3 min 05 s. Decoded on the GPU."
        );
        let cpu = summarize(
            r#"{"status":"ended","end":"cancelled","drawn":3,"dxva":0,"seconds":9}"#,
            "box",
        );
        assert!(cpu.ok);
        assert!(
            cpu.message.contains("Decoded on the CPU"),
            "{}",
            cpu.message
        );
        let unknown = summarize(
            r#"{"status":"ended","end":"ok","drawn":3,"dxva":-1,"seconds":9}"#,
            "box",
        );
        assert_eq!(unknown.message, "Disconnected from box after 9 s.");
    }

    #[test]
    fn failures_never_read_as_success() {
        for (document, needle) in [
            (
                r#"{"status":"ended","end":"unreachable","drawn":0}"#,
                "did not answer",
            ),
            (
                r#"{"status":"ended","end":"host_busy","drawn":0}"#,
                "another viewer",
            ),
            (
                r#"{"status":"ended","end":"ok","drawn":0}"#,
                "before a picture",
            ),
            (
                r#"{"status":"ended","end":"session_failed","drawn":0}"#,
                "decode HEVC",
            ),
            (
                r#"{"status":"ended","end":"identity_changed","drawn":0}"#,
                "Refresh",
            ),
            (
                r#"{"status":"refused","reason":"no_d3d11_device"}"#,
                "Direct3D 11",
            ),
            (r#"{"status":"refused","reason":"x"}"#, "(x)"),
            ("garbage", "without a report"),
            (
                r#"{"status":"ended","end":"brand_new","drawn":5}"#,
                "(brand_new)",
            ),
        ] {
            let summary = summarize(document, "m");
            assert!(!summary.ok, "{document}");
            assert!(summary.message.contains(needle), "{}", summary.message);
        }
        // A host that closed after showing pictures is still not "ok".
        assert!(!summarize(r#"{"status":"ended","end":"host_closed","drawn":9}"#, "m").ok);
    }

    #[test]
    fn decoder_status_warns_only_when_degraded_or_missing() {
        let (line, warning) = decoder_status(true, true, true, Some("Intel Iris Xe"));
        assert_eq!(line, "GPU HEVC decoding (Intel Iris Xe)");
        assert_eq!(warning, None);
        let (line, warning) = decoder_status(true, true, false, None);
        assert_eq!(line, "CPU HEVC decoding");
        assert!(warning.unwrap().contains("CPU"));
        let (line, warning) = decoder_status(true, false, true, None);
        assert_eq!(line, "HEVC decoder missing");
        assert!(warning.unwrap().contains("HEVC Video Extensions"));
        assert!(
            decoder_status(false, false, false, None)
                .1
                .unwrap()
                .contains("Direct3D 11")
        );
    }

    #[test]
    fn durations_are_rounded_and_bounded() {
        for (seconds, text) in [
            (0.0, "0 s"),
            (-4.0, "0 s"),
            (f64::NAN, "0 s"),
            (59.4, "59 s"),
            (59.6, "1 min 00 s"),
            (3_725.0, "1 h 02 min"),
        ] {
            assert_eq!(duration(seconds), text);
        }
    }
}
