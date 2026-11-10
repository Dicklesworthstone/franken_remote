//! Content-free completion of the original Mac clipboard owner. Setup, OS
//! publication and actual worker cleanup are separate observations. A later
//! viewing retry or unrelated failure cannot replace this evidence.
use super::{ClipboardReport, Failure};
use crate::clipboard_reasons;
use frd::native_clipboard::Cleanup;

fn absence(report: &ClipboardReport) -> Option<&'static str> {
    if !report.requested {
        Some("not_requested")
    } else if report.active {
        None
    } else if report.disabled {
        Some("clipboard_disabled")
    } else if report.reason.is_some() {
        Some(clipboard_reasons::absence(report.reason))
    } else {
        Some("clipboard_not_started")
    }
}

fn ended(report: &ClipboardReport) -> Option<&'static str> {
    if !report.active {
        None
    } else if report.disabled {
        Some("clipboard_disabled")
    } else {
        report
            .reason
            .map(|reason| clipboard_reasons::ended(Some(reason)))
    }
}

fn cleanup(report: &ClipboardReport) -> &'static str {
    match report.cleanup {
        None if report.configured => "unknown",
        None | Some(Cleanup::NotStarted) => "not_started",
        Some(Cleanup::Pending) => "pending",
        Some(Cleanup::Finished(Ok(()))) => "finished",
        Some(Cleanup::Finished(Err(_))) => "failed",
    }
}

fn code(code: Option<&'static str>) -> String {
    code.map_or_else(|| "null".into(), |code| format!("\"{code}\""))
}

fn evidence(report: &ClipboardReport) -> crate::ControlHistory {
    let absence = absence(report);
    let ended = ended(report);
    let cleanup = cleanup(report);
    // Keep the existing scalar key; the new explicit fields explain what was
    // actually observed. `started` alone does not claim successful publication.
    let state = if report.active {
        "started"
    } else {
        absence.unwrap_or("clipboard_not_started")
    };
    let json = format!(
        ",\"clipboard\":\"{state}\",\"clipboard_requested\":{},\"clipboard_configured\":{},\"clipboard_active\":{},\"clipboard_received\":{},\"clipboard_received_unknown\":{},\"clipboard_received_refused\":{},\"clipboard_disabled\":{},\"clipboard_absence\":{},\"clipboard_ended\":{},\"clipboard_cleanup\":\"{cleanup}\"",
        report.requested,
        report.configured,
        report.active,
        report.received_submitted,
        report.received_unknown,
        report.received_refused,
        report.disabled,
        code(absence),
        code(ended),
    );
    let text = if report.requested {
        format!(
            " Clipboard: {}; {} host text item(s) submitted to the local pasteboard, {} with unknown publication, {} refused. Local Off selected: {}. Clipboard worker cleanup: {cleanup}.{}",
            if report.active {
                "native worker started"
            } else {
                absence.unwrap_or("clipboard_not_started")
            },
            report.received_submitted,
            report.received_unknown,
            report.received_refused,
            report.disabled,
            ended.map_or_else(String::new, |reason| format!(" Ended: {reason}.")),
        )
    } else {
        " Clipboard: not requested.".to_owned()
    };
    crate::ControlHistory { json, text }
}

pub(super) fn preserve(report: &ClipboardReport, failure: &mut Failure) {
    if failure
        .control_history
        .is_some_and(|previous| previous.json.contains(",\"clipboard_requested\":"))
    {
        // An early startup fallback never overwrites the actual owner sample.
        return;
    }
    let next = evidence(report);
    let (mut json, mut text) = failure.control_history.map_or_else(
        || (String::new(), String::new()),
        |previous| (previous.json.clone(), previous.text.clone()),
    );
    json.push_str(&next.json);
    text.push_str(&next.text);
    failure.control_history = Some(Box::leak(Box::new(crate::ControlHistory { json, text })));
}

pub(super) fn finish(
    report: &ClipboardReport,
    result: Result<String, Failure>,
    json: bool,
) -> Result<String, Failure> {
    match result {
        Err(mut failure) => {
            preserve(report, &mut failure);
            Err(failure)
        }
        Ok(text) if json => {
            let Some(prefix) = text.trim_end().strip_suffix('}') else {
                let mut failure = Failure::new(
                    "native_output_invalid",
                    "The session completion could not be formatted; original clipboard publication and cleanup observations remain available in this error.",
                    1,
                );
                preserve(report, &mut failure);
                return Err(failure);
            };
            Ok(format!("{prefix}{}}}\n", evidence(report).json))
        }
        Ok(mut text) => {
            text.push_str(evidence(report).text.trim_start());
            text.push('\n');
            Ok(text)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use frd::clipboard_quic::Error;

    fn value(report: &ClipboardReport) -> serde_json::Value {
        serde_json::from_str(&format!(
            "{{\"outcome\":\"success\"{}}}",
            evidence(report).json
        ))
        .unwrap()
    }

    #[test]
    fn native_setup_is_distinct_from_publication_and_actual_cleanup() {
        let mut report = ClipboardReport {
            requested: true,
            configured: true,
            ..ClipboardReport::default()
        };
        let pending = value(&report);
        assert!(!pending["clipboard_active"].as_bool().unwrap());
        assert_eq!(pending["clipboard_received"], 0);
        assert_eq!(pending["clipboard_cleanup"], "unknown");
        assert_eq!(pending["clipboard_absence"], "clipboard_not_started");
        report.active = true;
        report.received_submitted = 2;
        report.received_unknown = 1;
        report.received_refused = 3;
        report.cleanup = Some(Cleanup::Pending);
        let active = value(&report);
        assert!(active["clipboard_absence"].is_null());
        assert_eq!(active["clipboard_received"], 2);
        assert_eq!(active["clipboard_received_unknown"], 1);
        assert_eq!(active["clipboard_received_refused"], 3);
        assert_eq!(active["clipboard_cleanup"], "pending");
    }

    #[test]
    fn local_off_and_cleanup_failure_do_not_erase_earlier_publication() {
        let report = ClipboardReport {
            requested: true,
            configured: true,
            active: true,
            received_submitted: 2,
            received_unknown: 1,
            disabled: true,
            reason: Some(Error::SwitchChanged),
            cleanup: Some(Cleanup::Finished(Err(Error::Native))),
            ..ClipboardReport::default()
        };
        let value = value(&report);
        assert_eq!(value["clipboard_received"], 2);
        assert_eq!(value["clipboard_received_unknown"], 1);
        assert!(value["clipboard_absence"].is_null());
        assert_eq!(value["clipboard_ended"], "clipboard_disabled");
        assert_eq!(value["clipboard_cleanup"], "failed");
    }

    #[test]
    fn startup_fallback_preserves_exact_retained_clipboard_file_and_close_evidence() {
        let actual = ClipboardReport {
            requested: true,
            configured: true,
            active: true,
            received_submitted: 1,
            received_unknown: 1,
            cleanup: Some(Cleanup::Pending),
            ..ClipboardReport::default()
        };
        let mut failure = Failure::new("native_cleanup_incomplete", "Original cleanup pending.", 1);
        failure.control_history = Some(Box::leak(Box::new(crate::ControlHistory {
            json: ",\"files_requested\":2,\"display_changes\":1".to_owned(),
            text: " Original file and display evidence.".to_owned(),
        })));
        preserve(&actual, &mut failure);
        let retained = failure.control_history;
        preserve(
            &ClipboardReport {
                requested: true,
                ..ClipboardReport::default()
            },
            &mut failure,
        );
        assert_eq!(failure.control_history, retained);
        assert_eq!(failure.code, "native_cleanup_incomplete");
        let value: serde_json::Value = serde_json::from_str(&format!(
            "{{\"outcome\":\"error\"{}}}",
            failure.control_history.unwrap().json
        ))
        .unwrap();
        assert_eq!(value["files_requested"], 2);
        assert_eq!(value["display_changes"], 1);
        assert_eq!(value["clipboard_received"], 1);
        assert_eq!(value["clipboard_received_unknown"], 1);
        assert_eq!(value["clipboard_cleanup"], "pending");
    }

    #[test]
    fn not_requested_and_host_unavailable_are_distinct_without_native_access() {
        assert_eq!(
            value(&ClipboardReport::default())["clipboard_absence"],
            "not_requested"
        );
        let refused = ClipboardReport {
            requested: true,
            reason: Some(Error::NotNegotiated),
            ..ClipboardReport::default()
        };
        let refused = value(&refused);
        assert_eq!(refused["clipboard_absence"], "host_clipboard_unavailable");
        assert_eq!(refused["clipboard_cleanup"], "not_started");
    }
}
