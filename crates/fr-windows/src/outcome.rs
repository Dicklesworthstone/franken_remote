//! Plain-language summaries of a viewer's completion document, shown by the
//! desktop application when it returns to the machine picker.
//!
//! The summary is derived only from the content-free completion fields
//! and original auxiliary-owner reports; it never adds a claim the document
//! does not make. `dxva` is the session core's decode path: 1 hardware,
//! 0 software, -1 not yet known. An uncertain terminal report stops the app;
//! a warning by itself never authorizes another session.
#![forbid(unsafe_code)]

use fr_client::clipboard::history::Completion as ClipboardCompletion;
use serde_json::Value;

/// Whether this exact terminal document permits another explicit choice.
/// The viewer deliberately exits 1 for a confirmed refusal or an orderly end
/// before the first picture. Keep that contract separate from notice styling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Continuation {
    Picker { expected_exit: i32 },
    Stop,
}

impl Continuation {
    pub const fn accepts_exit(self, code: Option<i32>) -> bool {
        matches!((self, code), (Self::Picker { expected_exit }, Some(code)) if expected_exit == code)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Summary {
    /// The session showed pictures and ended in order.
    pub ok: bool,
    /// Original terminal evidence, independent of local logging warnings.
    pub continuation: Continuation,
    /// One sentence for the picker's notice line.
    pub message: String,
}

/// `machine` is the short display name of the machine that was chosen.
pub(crate) fn summarize(completion: &str, machine: &str) -> Summary {
    let Ok(document) = serde_json::from_str::<Value>(completion.trim()) else {
        return Summary {
            ok: false,
            continuation: Continuation::Stop,
            message: format!("The session with {machine} ended without a report."),
        };
    };
    if document["status"] == "refused" {
        return summarize_refusal(&document, machine);
    }
    if document["status"] != "ended" {
        return Summary {
            ok: false,
            continuation: Continuation::Stop,
            message: format!(
                "The session outcome for {machine} is unknown because its completion status was invalid."
            ),
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
    let continuation = continuation(&document, end);
    let ok = continuation == (Continuation::Picker { expected_exit: 0 });
    let mut message = match end {
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
            "{machine} did not answer. Make sure FrankenRemote is hosting there and that your tailnet policy allows UDP to its configured service port from this PC."
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
            "The session with {machine} failed after {}{}.",
            duration(seconds),
            detail(&document)
        ),
        _ => format!("The session with {machine} ended unexpectedly ({end})."),
    };
    if continuation == Continuation::Stop {
        if document["reopens"]
            .as_u64()
            .is_some_and(|reopens| clipboard_history_incomplete(reopens, &document["clipboard"]))
        {
            message.push_str(
                " Clipboard completion for every viewer connection could not be confirmed.",
            );
        } else {
            message.push_str(
                " The report did not confirm the original session's outcome and cleanup.",
            );
        }
    }
    Summary {
        ok,
        continuation,
        message,
    }
}

fn summarize_refusal(document: &Value, machine: &str) -> Summary {
    let reason = document["reason"].as_str().unwrap_or("unknown");
    Summary {
        ok: false,
        continuation: if document.as_object().is_some_and(|fields| fields.len() == 2)
            && matches!(
                reason,
                "no_d3d11_device"
                    | "window_unavailable"
                    | "input_menu_unavailable"
                    | "file_menu_unavailable"
                    | "receive_menu_unavailable"
                    | "session_thread_unavailable"
            ) {
            Continuation::Picker { expected_exit: 1 }
        } else {
            // swap_chain_unavailable and viewer_lost can follow a live
            // Session::start. Its Drop only requests detached cleanup.
            Continuation::Stop
        },
        message: refused(reason, machine),
    }
}

/// The current viewer's terminal contract, not a general JSON-success test.
/// End names that conflate service/cleanup failure remain unconfirmed. Normal
/// connection refusals are usable only before any pictures were shown.
fn continuation(document: &Value, end: &str) -> Continuation {
    let (Some(drawn), Some(reopens)) = (document["drawn"].as_u64(), document["reopens"].as_u64())
    else {
        return Continuation::Stop;
    };
    let orderly = matches!(end, "ok" | "cancelled");
    let known_end = orderly
        || end == "host_closed"
        || (drawn == 0
            && matches!(
                end,
                "name_unresolved"
                    | "outside_tailnet"
                    | "identity_changed"
                    | "tls_refused"
                    | "unreachable"
                    | "host_refused"
                    | "host_busy"
            ));
    let (Some(reopen), Some(clipboard), Some(upload), Some(download)) = (
        document.get("reopen_refusal"),
        document.get("clipboard"),
        document.get("upload"),
        document.get("download"),
    ) else {
        return Continuation::Stop;
    };
    if !known_end
        || !reopen.is_null()
        || !clipboard_known(clipboard, reopens)
        || !upload_known(upload)
        || !download_known(download)
    {
        return Continuation::Stop;
    }
    let (Some(upload_ok), Some(download_ok)) =
        (transfer_success(upload), transfer_success(download))
    else {
        return Continuation::Stop;
    };
    // Match windows::viewer::report's exit contract, before diagnostics or
    // local logging can change the notice. A known failed transfer remains a
    // failure even when the desktop itself ended normally.
    let succeeded = drawn > 0 && orderly && upload_ok && download_ok;
    Continuation::Picker {
        expected_exit: i32::from(!succeeded),
    }
}

fn clipboard_history_incomplete(reopens: u64, clipboard: &Value) -> bool {
    if clipboard.get("history").is_none() && clipboard.get("collection").is_none() {
        // Older viewers report only the first earlier clipboard owner after
        // a replacement. Their scalar fields cannot certify later owners.
        return reopens != 0 && clipboard["requested"] == true;
    }
    reopens.checked_add(1).is_none_or(|owners| {
        ClipboardCompletion::from_document(clipboard)
            .map_or(true, |completion| !completion.covers(owners))
    })
}

fn cleanup_known(document: &Value) -> bool {
    matches!(
        document["cleanup"].as_str(),
        Some("finished" | "not_started")
    )
}

fn clipboard_known(clipboard: &Value, reopens: u64) -> bool {
    if clipboard.get("history").is_some() || clipboard.get("collection").is_some() {
        return reopens.checked_add(1).is_some_and(|owners| {
            ClipboardCompletion::from_document(clipboard)
                .is_ok_and(|completion| completion.known(owners))
        });
    }
    clipboard.is_object()
        && !clipboard_history_incomplete(reopens, clipboard)
        && cleanup_known(clipboard)
        && clipboard["received_unknown"].as_u64() == Some(0)
        && clipboard["received_submitted"].as_u64().is_some()
        && clipboard["received_refused"].as_u64().is_some()
        && clipboard["requested"].as_bool().is_some()
        && clipboard["configured"].as_bool().is_some()
        && clipboard["native_started"]
            .as_bool()
            .is_some_and(|started| !started || clipboard["cleanup"] == "finished")
}

fn transfer_success(transfer: &Value) -> Option<bool> {
    if transfer.is_null() {
        Some(true)
    } else {
        transfer["succeeded"].as_bool()
    }
}

fn upload_known(upload: &Value) -> bool {
    upload.is_null()
        || (upload_batch_known(upload)
            && upload["choice_cleanup"] == "finished"
            && upload["batches"]
                .as_array()
                .is_some_and(|batches| batches.iter().all(upload_batch_known)))
}

fn upload_batch_known(batch: &Value) -> bool {
    cleanup_known(batch)
        && batch["publication_unknown_files"].as_u64() == Some(0)
        && batch["unexpected_direction_files"].as_u64() == Some(0)
        && batch["receipts"].as_array().is_some_and(|receipts| {
            // A started file without its terminal receipt cannot become a
            // confirmed refusal merely because another selection completed.
            let all_reported = match batch.get("started_files") {
                Some(Value::Null) => receipts.is_empty(),
                Some(count) => count.as_u64() == u64::try_from(receipts.len()).ok(),
                None => false,
            };
            all_reported
                && receipts.iter().all(|receipt| {
                    matches!(
                        receipt["outcome"].as_str(),
                        Some(
                            "host_published"
                                | "host_published_durability_unknown"
                                | "host_refused"
                                | "controller_refused"
                                | "interrupted_before_publication"
                        )
                    )
                })
        })
}

fn download_known(download: &Value) -> bool {
    download.is_null()
        || (cleanup_known(download)
            && download["results_truncated"].as_bool() == Some(false)
            && download.get("progress").is_some_and(Value::is_null)
            && download.get("reason_code").is_some_and(|reason| {
                reason.is_null()
                    || reason
                        .as_str()
                        .is_some_and(|code| code != "publication_unknown")
            })
            && download["receipts"].as_array().is_some_and(|receipts| {
                receipts.iter().all(|receipt| {
                    matches!(
                        receipt["outcome"].as_str(),
                        Some("locally_published" | "refused" | "cancelled")
                    )
                })
            }))
}

/// The completion's typed, content-free failure detail, e.g.
/// ` (Renewal(Transport(Expired)))`; empty when absent or implausible.
fn detail(document: &Value) -> String {
    match document["detail"].as_str() {
        Some(text)
            if !text.is_empty()
                && text.len() <= 160
                && text
                    .bytes()
                    .all(|byte| byte.is_ascii_graphic() || byte == b' ') =>
        {
            format!(" ({text})")
        }
        _ => String::new(),
    }
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

    fn ended(end: &str, drawn: u64) -> Value {
        serde_json::json!({
            "status": "ended", "end": end, "drawn": drawn,
            "seconds": 9, "dxva": -1, "reopens": 0, "reopen_refusal": null,
            "clipboard": {
                "requested": false, "configured": false, "native_started": false,
                "received_submitted": 0, "received_unknown": 0, "received_refused": 0,
                "cleanup": "not_started", "reason": null,
            },
            "upload": null, "download": null,
        })
    }

    fn summary(document: &Value) -> Summary {
        summarize(&document.to_string(), "studio")
    }

    #[test]
    fn orderly_sessions_report_duration_and_decode_path() {
        let mut document = ended("ok", 120);
        document["dxva"] = 1.into();
        document["seconds"] = 185.4.into();
        let gpu = summary(&document);
        assert!(gpu.ok);
        assert_eq!(
            gpu.message,
            "Disconnected from studio after 3 min 05 s. Decoded on the GPU."
        );
        let mut document = ended("cancelled", 3);
        document["dxva"] = 0.into();
        let cpu = summarize(&document.to_string(), "box");
        assert!(cpu.ok);
        assert!(
            cpu.message.contains("Decoded on the CPU"),
            "{}",
            cpu.message
        );
        let unknown = summarize(&ended("ok", 3).to_string(), "box");
        assert_eq!(unknown.message, "Disconnected from box after 9 s.");
    }

    #[test]
    fn orderly_zero_picture_ends_and_known_refusals_have_their_own_exit_contract() {
        for end in ["ok", "cancelled"] {
            let before_picture = summary(&ended(end, 0));
            assert!(!before_picture.ok);
            assert_eq!(
                before_picture.continuation,
                Continuation::Picker { expected_exit: 1 }
            );
            assert_eq!(
                summary(&ended(end, 1)).continuation,
                Continuation::Picker { expected_exit: 0 }
            );
        }
        for end in [
            "name_unresolved",
            "outside_tailnet",
            "identity_changed",
            "tls_refused",
            "unreachable",
            "host_refused",
            "host_busy",
        ] {
            let result = summary(&ended(end, 0));
            assert!(!result.ok);
            assert_eq!(
                result.continuation,
                Continuation::Picker { expected_exit: 1 },
                "{end}"
            );
            assert_eq!(
                summary(&ended(end, 1)).continuation,
                Continuation::Stop,
                "{end}"
            );
        }
        assert_eq!(
            summary(&ended("host_closed", 4)).continuation,
            Continuation::Picker { expected_exit: 1 }
        );
        assert!(!summary(&ended("unreachable", 0)).message.contains("8443"));
    }

    #[test]
    fn only_known_pre_start_native_refusals_allow_a_new_picker() {
        for reason in [
            "no_d3d11_device",
            "window_unavailable",
            "input_menu_unavailable",
            "file_menu_unavailable",
            "receive_menu_unavailable",
            "session_thread_unavailable",
        ] {
            let document = serde_json::json!({"status": "refused", "reason": reason});
            assert_eq!(
                summary(&document).continuation,
                Continuation::Picker { expected_exit: 1 },
                "{reason}"
            );
        }
        for reason in ["swap_chain_unavailable", "viewer_lost", "unknown", ""] {
            let document = serde_json::json!({"status": "refused", "reason": reason});
            assert_eq!(
                summary(&document).continuation,
                Continuation::Stop,
                "{reason}"
            );
        }
        let conflicting = serde_json::json!({
            "status": "refused", "reason": "no_d3d11_device", "drawn": 12,
        });
        assert_eq!(summary(&conflicting).continuation, Continuation::Stop);
    }

    #[test]
    fn terminal_status_and_original_owner_evidence_are_required() {
        for status in [Value::Null, true.into(), "ok".into(), "selected".into()] {
            let mut document = ended("ok", 12);
            document["status"] = status;
            let result = summary(&document);
            assert!(!result.ok);
            assert_eq!(result.continuation, Continuation::Stop);
        }
        for field in [
            "status",
            "end",
            "drawn",
            "reopens",
            "reopen_refusal",
            "clipboard",
            "upload",
            "download",
        ] {
            let mut document = ended("ok", 12);
            document.as_object_mut().unwrap().remove(field);
            let result = summary(&document);
            assert!(!result.ok, "{field}");
            assert_eq!(result.continuation, Continuation::Stop, "{field}");
        }
        for end in [
            "internal",
            "session_failed",
            "control_stopped",
            "future_end",
        ] {
            assert_eq!(
                summary(&ended(end, 12)).continuation,
                Continuation::Stop,
                "{end}"
            );
        }
        let mut document = ended("ok", 12);
        document["reopen_refusal"] = "cleanup_incomplete".into();
        assert_eq!(summary(&document).continuation, Continuation::Stop);
        document["reopen_refusal"] = Value::Null;
        document["drawn"] = "12".into();
        assert_eq!(summary(&document).continuation, Continuation::Stop);
    }

    #[test]
    fn clipboard_effect_and_cleanup_uncertainty_stop_even_after_pictures() {
        for cleanup in ["unknown", "pending", "failed", "future_cleanup"] {
            let mut document = ended("ok", 12);
            document["clipboard"]["cleanup"] = cleanup.into();
            assert_eq!(
                summary(&document).continuation,
                Continuation::Stop,
                "{cleanup}"
            );
        }
        let mut document = ended("ok", 12);
        document["clipboard"]["received_unknown"] = 1.into();
        assert_eq!(summary(&document).continuation, Continuation::Stop);
        document["clipboard"]["received_unknown"] = 0.into();
        document["clipboard"]["native_started"] = true.into();
        assert_eq!(summary(&document).continuation, Continuation::Stop);
        document["clipboard"]["cleanup"] = "finished".into();
        assert_eq!(
            summary(&document).continuation,
            Continuation::Picker { expected_exit: 0 }
        );
    }

    #[test]
    fn a_first_clipboard_report_cannot_certify_later_replacement_owners() {
        let mut document = ended("ok", 12);
        document["reopens"] = 1.into();
        assert_eq!(
            summary(&document).continuation,
            Continuation::Picker { expected_exit: 0 }
        );
        document["clipboard"]["requested"] = true.into();
        document["clipboard"]["configured"] = true.into();
        document["clipboard"]["native_started"] = true.into();
        document["clipboard"]["cleanup"] = "finished".into();
        let incomplete = summary(&document);
        assert_eq!(incomplete.continuation, Continuation::Stop);
        assert!(
            incomplete.message.contains(
                "Clipboard completion for every viewer connection could not be confirmed"
            )
        );
        document["reopens"] = 0.into();
        assert_eq!(
            summary(&document).continuation,
            Continuation::Picker { expected_exit: 0 }
        );
    }

    fn clipboard_owner(submitted: u64) -> fr_client::clipboard::history::Snapshot {
        use fr_client::clipboard::history::{Cleanup, Collection, Snapshot};
        Snapshot {
            requested: true,
            configured: true,
            native_started: true,
            received_submitted: submitted,
            cleanup: Cleanup::Finished,
            collection: Collection::Finished,
            ..Snapshot::default()
        }
    }

    fn with_clipboard_history(reports: &[fr_client::clipboard::history::Snapshot]) -> Value {
        use fr_client::clipboard::history::History;
        let (current, retired) = reports.split_last().unwrap();
        let mut history = History::default();
        for (owner, report) in retired.iter().enumerate() {
            history.retire(u64::try_from(owner).unwrap(), *report);
        }
        let reopens = u64::try_from(retired.len()).unwrap();
        let mut document = ended("ok", 12);
        document["reopens"] = reopens.into();
        document["clipboard"] = serde_json::json!(history.completion(reopens, *current));
        document
    }

    #[test]
    fn complete_original_clipboard_history_allows_clean_replacements() {
        let document =
            with_clipboard_history(&[clipboard_owner(2), clipboard_owner(3), clipboard_owner(5)]);
        let result = summary(&document);
        assert!(result.ok);
        assert_eq!(
            result.continuation,
            Continuation::Picker { expected_exit: 0 }
        );
        assert_eq!(document["clipboard"]["received_submitted"], 10);
        assert_eq!(document["clipboard"]["history"]["owners"], 3);
        assert!(!result.message.contains("could not be confirmed"));
    }

    #[test]
    fn an_unknown_clipboard_effect_in_any_connection_stops_continuation() {
        for owner in 0..3 {
            let mut reports = [clipboard_owner(2); 3];
            reports[owner].received_unknown = 1;
            let document = with_clipboard_history(&reports);
            assert_eq!(summary(&document).continuation, Continuation::Stop);
            assert_eq!(document["clipboard"]["received_unknown"], 1);
            assert_eq!(document["clipboard"]["received_submitted"], 6);
        }
    }

    #[test]
    fn earlier_clipboard_cleanup_cannot_be_hidden_by_a_clean_replacement() {
        use fr_client::clipboard::history::Cleanup;
        for cleanup in [Cleanup::Unknown, Cleanup::Pending, Cleanup::Failed] {
            let mut original = clipboard_owner(2);
            original.cleanup = cleanup;
            let document = with_clipboard_history(&[original, clipboard_owner(3)]);
            assert_eq!(
                summary(&document).continuation,
                Continuation::Stop,
                "{cleanup:?}"
            );
            assert_eq!(document["clipboard"]["received_submitted"], 5);
        }
    }

    #[test]
    fn final_collection_is_required_for_retired_and_current_clipboard_owners() {
        use fr_client::clipboard::history::Collection;
        for owner in 0..2 {
            let mut reports = [clipboard_owner(2); 2];
            reports[owner].collection = Collection::Unconfirmed;
            let document = with_clipboard_history(&reports);
            assert_eq!(document["clipboard"]["cleanup"], "finished");
            assert_eq!(document["clipboard"]["received_submitted"], 4);
            assert_eq!(summary(&document).continuation, Continuation::Stop);
        }
        let mut document = with_clipboard_history(&[clipboard_owner(2)]);
        document["clipboard"]["collection"] = "unconfirmed".into();
        // Even a purported complete history cannot upgrade an uncollected
        // current snapshot whose final publication receipt may still arrive.
        assert_eq!(summary(&document).continuation, Continuation::Stop);
        document["clipboard"]
            .as_object_mut()
            .unwrap()
            .remove("collection");
        assert_eq!(summary(&document).continuation, Continuation::Stop);
    }

    #[test]
    fn missing_malformed_or_unsupported_history_cannot_replace_owner_evidence() {
        let document = with_clipboard_history(&[clipboard_owner(1), clipboard_owner(2)]);
        for field in ["schema_version", "owners", "complete", "overflowed"] {
            let mut missing = document.clone();
            missing["clipboard"]["history"]
                .as_object_mut()
                .unwrap()
                .remove(field);
            assert_eq!(
                summary(&missing).continuation,
                Continuation::Stop,
                "{field}"
            );
        }
        for (field, value) in [
            ("schema_version", 2.into()),
            ("owners", 0.into()),
            ("owners", 1.into()),
            ("owners", 3.into()),
            ("owners", "2".into()),
            ("complete", false.into()),
            ("complete", 1.into()),
            ("overflowed", true.into()),
            ("overflowed", Value::Null),
        ] {
            let mut invalid = document.clone();
            invalid["clipboard"]["history"][field] = value;
            assert_eq!(
                summary(&invalid).continuation,
                Continuation::Stop,
                "{field}"
            );
        }
        for history in [
            Value::Null,
            true.into(),
            "complete".into(),
            serde_json::json!([]),
        ] {
            let mut invalid = document.clone();
            invalid["clipboard"]["history"] = history;
            assert_eq!(summary(&invalid).continuation, Continuation::Stop);
        }
    }

    #[test]
    fn new_single_owner_reports_do_not_fall_back_from_invalid_coverage_to_legacy_fields() {
        let mut document = with_clipboard_history(&[clipboard_owner(1)]);
        assert_eq!(
            summary(&document).continuation,
            Continuation::Picker { expected_exit: 0 }
        );
        document["clipboard"]["history"] = Value::Null;
        assert_eq!(summary(&document).continuation, Continuation::Stop);
        document["clipboard"]
            .as_object_mut()
            .unwrap()
            .remove("history");
        assert_eq!(summary(&document).continuation, Continuation::Stop);
        document["clipboard"]
            .as_object_mut()
            .unwrap()
            .remove("collection");
        assert_eq!(
            summary(&document).continuation,
            Continuation::Picker { expected_exit: 0 }
        );
    }

    #[test]
    fn clipboard_owner_and_receipt_counters_never_wrap_into_continuation() {
        let document = with_clipboard_history(&[clipboard_owner(u64::MAX - 1), clipboard_owner(2)]);
        assert_eq!(summary(&document).continuation, Continuation::Stop);
        let mut document = with_clipboard_history(&[clipboard_owner(1)]);
        document["reopens"] = u64::MAX.into();
        document["clipboard"]["history"]["owners"] = 0.into();
        assert_eq!(summary(&document).continuation, Continuation::Stop);
    }

    fn upload_batch() -> Value {
        serde_json::json!({
            "cleanup": "finished", "publication_unknown_files": 0,
            "unexpected_direction_files": 0, "started_files": 1,
            "receipts": [{"outcome": "host_refused"}],
        })
    }

    fn upload() -> Value {
        let mut upload = upload_batch();
        upload["batches"] = serde_json::json!([upload_batch()]);
        upload["choice_cleanup"] = "finished".into();
        upload["succeeded"] = false.into();
        upload
    }

    #[test]
    fn known_upload_refusal_is_distinct_from_missing_receipts_and_prior_unknown_effects() {
        let mut document = ended("ok", 12);
        document["upload"] = upload();
        let result = summary(&document);
        assert!(!result.ok);
        assert_eq!(
            result.continuation,
            Continuation::Picker { expected_exit: 1 }
        );
        for field in ["publication_unknown_files", "unexpected_direction_files"] {
            let mut unknown = document.clone();
            unknown["upload"]["batches"][0][field] = 1.into();
            assert_eq!(
                summary(&unknown).continuation,
                Continuation::Stop,
                "{field}"
            );
        }
        for field in ["cleanup", "choice_cleanup"] {
            let mut unknown = document.clone();
            unknown["upload"][field] = "pending".into();
            assert_eq!(
                summary(&unknown).continuation,
                Continuation::Stop,
                "{field}"
            );
        }
        document["upload"]["batches"][0]["started_files"] = 2.into();
        assert_eq!(summary(&document).continuation, Continuation::Stop);
        document["upload"]["batches"][0]["started_files"] = 1.into();
        document["upload"]["batches"][0]["receipts"][0]["outcome"] = "publication_unknown".into();
        assert_eq!(summary(&document).continuation, Continuation::Stop);
    }

    fn download() -> Value {
        serde_json::json!({
            "cleanup": "finished", "results_truncated": false, "progress": null,
            "reason_code": "receive_conflict", "succeeded": false,
            "receipts": [{"outcome": "refused"}],
        })
    }

    #[test]
    fn known_download_conflict_is_distinct_from_uncertain_publication_or_cleanup() {
        let mut document = ended("ok", 12);
        document["download"] = download();
        assert_eq!(
            summary(&document).continuation,
            Continuation::Picker { expected_exit: 1 }
        );
        for (field, value) in [
            ("cleanup", "pending".into()),
            ("results_truncated", true.into()),
            ("progress", serde_json::json!({"staged_bytes": 1})),
            ("reason_code", "publication_unknown".into()),
        ] {
            let mut unknown = document.clone();
            unknown["download"][field] = value;
            assert_eq!(
                summary(&unknown).continuation,
                Continuation::Stop,
                "{field}"
            );
        }
        document["download"]["receipts"][0]["outcome"] = "publication_unknown".into();
        assert_eq!(summary(&document).continuation, Continuation::Stop);
    }

    #[test]
    fn download_reason_cannot_be_omitted_or_retyped_to_hide_unknown_publication() {
        let mut document = ended("ok", 12);
        document["download"] = download();
        document["download"]["receipts"] = serde_json::json!([]);
        for reason in [
            false.into(),
            0.into(),
            serde_json::json!({}),
            serde_json::json!([]),
        ] {
            document["download"]["reason_code"] = reason;
            assert_eq!(summary(&document).continuation, Continuation::Stop);
        }
        document["download"]
            .as_object_mut()
            .unwrap()
            .remove("reason_code");
        assert_eq!(summary(&document).continuation, Continuation::Stop);
    }

    #[test]
    fn known_publication_keeps_its_separate_unconfirmed_durability_result() {
        let mut document = ended("ok", 12);
        document["upload"] = upload();
        document["upload"]["receipts"][0]["outcome"] = "host_published_durability_unknown".into();
        document["upload"]["batches"][0]["receipts"][0]["outcome"] =
            "host_published_durability_unknown".into();
        assert_eq!(
            summary(&document).continuation,
            Continuation::Picker { expected_exit: 1 }
        );

        document["upload"] = Value::Null;
        document["download"] = download();
        document["download"]["reason_code"] = Value::Null;
        document["download"]["succeeded"] = true.into();
        document["download"]["receipts"][0] = serde_json::json!({
            "outcome": "locally_published", "receipt": "published_durability_unknown",
            "published_bytes": 4, "durable": false,
        });
        assert_eq!(
            summary(&document).continuation,
            Continuation::Picker { expected_exit: 0 }
        );
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
