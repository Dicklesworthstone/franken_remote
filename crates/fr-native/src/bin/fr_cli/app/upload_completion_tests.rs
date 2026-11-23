use super::super::session_notice_with_transfers;
use super::*;
use fr_client::completion::{CompletionLimit, capture_completion_with_limit};

fn metadata() -> Value {
    json!({"files_requested":0,"files_sent":[],"files_refused":[],"files_absence":"no_files_selected"})
}

fn completion(macos: bool) -> Value {
    let mut value = if macos {
        json!({"platform":"macos","outcome":"success","ended":"window_closed","local_cleanup_confirmed":true})
    } else {
        json!({"schema_version":1,"outcome":"stopped","role":"control","cleanup_confirmed":true})
    };
    value
        .as_object_mut()
        .unwrap()
        .extend(metadata().as_object().unwrap().clone());
    value
}

fn capture(value: &Value) -> CompletionCapture {
    capture_completion_with_limit(value.to_string().as_bytes(), CompletionLimit::NativeCommand)
}

fn check(value: &Value, expected: bool, success: bool) -> Result<String, Failure> {
    session_notice_with_transfers(&capture(value), "workstation", success, expected, false)
}

fn retained(failure: Failure) -> Value {
    serde_json::from_str(&crate::terminal::output(failure, true)).unwrap()
}

fn published(value: &mut Value) {
    value["files_requested"] = json!(3);
    value["files_sent"] = json!([
        {"index":0,"bytes":0,"durable":true},
        {"index":1,"bytes":19,"durable":false},
    ]);
    value["files_refused"] = json!([{"index":2,"bytes":21,"reason":"host_conflict"}]);
    value["files_absence"] = Value::Null;
}

fn source(value: &mut Value, cleanup: &str) {
    value.as_object_mut().unwrap().extend(
        json!({
            "files_upload_started":3,"files_upload_complete":true,
            "files_upload_cleanup":cleanup,"files_upload_cancel_requested":false,
            "files_upload_progress":null,
        })
        .as_object()
        .unwrap()
        .clone(),
    );
}

fn receive(value: &mut Value, cleanup: &str) {
    value.as_object_mut().unwrap().extend(
        json!({
            "files_receive_requested":true,"files_received":[{"index":0,"bytes":23,"durable":true}],
            "files_receive_refused":[],"files_receive_absence":null,"files_receive_ended":null,
            "files_receive_cleanup":cleanup,"files_receive_progress":null,
            "files_receive_results_truncated":false,"files_receive_cancel_requested":false,
        })
        .as_object()
        .unwrap()
        .clone(),
    );
}

#[test]
fn original_upload_publication_and_durability_reach_both_native_app_notices() {
    for macos in [false, true] {
        let mut value = completion(macos);
        published(&mut value);
        let notice = check(&value, true, true).unwrap();
        assert!(notice.starts_with("Uploads: 2 of 3 published by host (19 bytes); 1 not published; 1 durability unconfirmed."));
        assert!(notice.contains("local cleanup was confirmed"));
        assert!(notice.len() <= super::super::MAX_APP_NOTICE);
        // The aggregate producer intentionally omits single-source cleanup.
        assert!(!notice.contains("Upload source cleanup"));
    }
}

#[test]
fn no_selection_unavailable_and_missing_outcome_are_distinct() {
    let mut value = completion(false);
    assert!(
        check(&value, true, true)
            .unwrap()
            .starts_with("Uploads: no files selected.")
    );
    value["files_absence"] = json!("host_files_unavailable");
    assert!(
        check(&value, true, true)
            .unwrap()
            .starts_with("Uploads did not start (host_files_unavailable).")
    );
    value
        .as_object_mut()
        .unwrap()
        .retain(|key, _| !key.starts_with("files_"));
    assert!(check(&value, false, true).is_ok());
    assert_eq!(
        check(&value, true, true).unwrap_err().code,
        "app_upload_outcome_unknown"
    );
}

#[test]
fn original_consent_cannot_be_inferred_from_child_upload_fields() {
    let mut value = completion(false);
    value["files_absence"] = json!("not_requested");
    assert!(check(&value, false, true).is_ok());
    assert_eq!(
        check(&value, true, true).unwrap_err().code,
        "app_upload_outcome_unknown"
    );
    published(&mut value);
    let failure = check(&value, false, true).unwrap_err();
    assert_eq!(failure.code, "app_upload_outcome_unknown");
    assert_eq!(retained(failure)["files_sent"], value["files_sent"]);
}

#[test]
fn unknown_publication_cannot_be_hidden_by_successful_viewer_cleanup() {
    for macos in [false, true] {
        let mut value = completion(macos);
        published(&mut value);
        value["files_refused"][0]["reason"] = json!("publication_unknown");
        let failure = check(&value, true, true).unwrap_err();
        assert_eq!(failure.code, "app_upload_outcome_unknown");
        assert!(failure.unknown_effect);
        let terminal = retained(failure);
        assert_eq!(terminal["outcome"], "unknown_external_effect");
        assert_eq!(terminal["files_sent"], value["files_sent"]);
        assert_eq!(terminal["files_refused"], value["files_refused"]);
    }
}

#[test]
fn pending_or_failed_source_cleanup_cannot_be_replaced_by_viewer_cleanup() {
    for cleanup in ["pending", "failed"] {
        let mut value = completion(true);
        published(&mut value);
        source(&mut value, cleanup);
        let failure = check(&value, true, true).unwrap_err();
        assert_eq!(failure.code, "app_upload_cleanup_unconfirmed");
        let terminal = retained(failure);
        assert_eq!(terminal["files_upload_cleanup"], cleanup);
        assert_eq!(terminal["files_sent"], value["files_sent"]);
    }
}

#[test]
fn queued_bytes_and_cancel_intent_never_become_publication() {
    let mut value = completion(true);
    published(&mut value);
    source(&mut value, "finished");
    value["files_upload_complete"] = json!(false);
    value["files_upload_cancel_requested"] = json!(true);
    value["files_upload_progress"] = json!({"transfer_id":7,"queued_bytes":21,"total_bytes":21});
    value["files_refused"][0]["reason"] = json!("interrupted_cancelled");
    let notice = check(&value, true, true).unwrap();
    assert!(notice.contains("2 of 3 published by host (19 bytes)"));
    assert!(!notice.contains("40 bytes") && !notice.contains("3 of 3"));
    assert!(notice.contains("Upload cancellation requested."));
    assert!(notice.contains("Upload source cleanup finished."));
    value["files_upload_progress"]["total_bytes"] = Value::Null;
    assert!(check(&value, true, true).is_ok());
    value["files_upload_progress"]["total_bytes"] = json!(20);
    assert_eq!(
        check(&value, true, true).unwrap_err().code,
        "app_upload_outcome_unknown"
    );
}

#[test]
fn upload_receipt_indices_counts_and_byte_totals_are_checked_before_projection() {
    let mut value = completion(false);
    published(&mut value);
    let mut cases = Vec::new();
    let mut duplicate = value.clone();
    duplicate["files_refused"][0]["index"] = json!(1);
    cases.push(duplicate);
    let mut missing = value.clone();
    missing["files_requested"] = json!(4);
    cases.push(missing);
    let mut gap = value.clone();
    gap["files_refused"][0]["index"] = json!(3);
    cases.push(gap);
    let mut overflow = value.clone();
    overflow["files_sent"][0]["bytes"] = json!(u64::MAX);
    cases.push(overflow);
    let mut absent = value.clone();
    absent["files_absence"] = json!("no_files_selected");
    cases.push(absent);
    let mut incomplete = value.clone();
    incomplete["files_refused"][0]["bytes"] = json!("private path");
    cases.push(incomplete);
    for case in cases {
        let failure = check(&case, true, true).unwrap_err();
        assert_eq!(failure.code, "app_upload_outcome_unknown");
        let terminal = retained(failure);
        assert_eq!(terminal["files_sent"][1]["bytes"], 19);
        assert!(!terminal.to_string().contains("private path"));
    }
}

#[test]
fn malformed_or_contradictory_source_fields_retain_known_published_receipts() {
    let mut value = completion(true);
    published(&mut value);
    source(&mut value, "finished");
    for (field, replacement) in [
        ("files_upload_started", json!(1)),
        ("files_upload_started", json!(4)),
        ("files_upload_complete", Value::Null),
        ("files_upload_cleanup", json!("not_started")),
        ("files_upload_cancel_requested", json!(1)),
        ("files_upload_unknown", json!("private source")),
        (
            "files_upload_progress",
            json!({"transfer_id":0,"queued_bytes":0,"total_bytes":null}),
        ),
    ] {
        let mut case = value.clone();
        case[field] = replacement;
        let failure = check(&case, true, true).unwrap_err();
        assert_eq!(failure.code, "app_upload_outcome_unknown");
        let terminal = retained(failure);
        assert_eq!(terminal["files_sent"], value["files_sent"]);
        assert!(!terminal.to_string().contains("private source"));
    }
    value
        .as_object_mut()
        .unwrap()
        .remove("files_upload_complete");
    assert!(check(&value, true, true).is_err());
}

#[test]
fn absent_source_report_cannot_explain_known_publications() {
    let mut value = completion(true);
    published(&mut value);
    source(&mut value, "finished");
    value["files_upload_started"] = Value::Null;
    value["files_upload_complete"] = Value::Null;
    let failure = check(&value, true, true).unwrap_err();
    assert_eq!(failure.code, "app_upload_outcome_unknown");
    assert!(failure.unknown_effect);
    assert_eq!(retained(failure)["files_sent"], value["files_sent"]);
}

#[test]
fn completed_source_retains_its_unstarted_suffix_after_cancellation() {
    for started in [0, 1] {
        let mut value = completion(true);
        value["files_requested"] = json!(3);
        value["files_absence"] = Value::Null;
        value["files_sent"] = Value::Array(
            (0..started)
                .map(|index| json!({"index":index,"bytes":19,"durable":true}))
                .collect(),
        );
        value["files_refused"] = Value::Array(
            (started..3)
                .map(|index| json!({"index":index,"bytes":21,"reason":"not_started"}))
                .collect(),
        );
        source(&mut value, "finished");
        value["files_upload_started"] = json!(started);
        value["files_upload_cancel_requested"] = json!(true);
        let notice = check(&value, true, true).unwrap();
        assert!(notice.contains(&format!("{started} of 3 published by host")));
        assert!(notice.contains(&format!("{} not published", 3 - started)));
        assert!(notice.contains("Upload source cleanup finished."));
        assert!(notice.contains("Upload cancellation requested."));
        if started != 0 {
            // The same count cannot authorize a publication after the original
            // started prefix, even when every selection index is accounted for.
            value["files_sent"][0]["index"] = json!(2);
            value["files_refused"][0]["index"] = json!(0);
            value["files_refused"][1]["index"] = json!(1);
            let failure = check(&value, true, true).unwrap_err();
            assert_eq!(failure.code, "app_upload_outcome_unknown");
            assert_eq!(retained(failure)["files_sent"], value["files_sent"]);
        }
    }
}

#[test]
fn bounded_native_drop_history_refuses_oversized_and_unrequested_command_metadata() {
    let mut value = completion(false);
    value["files_requested"] = json!(MAX_RESULTS);
    value["files_absence"] = Value::Null;
    value["files_sent"] = Value::Array(
        (0..MAX_RESULTS)
            .map(|index| json!({"index":index,"bytes":0,"durable":true}))
            .collect(),
    );
    assert!(check(&value, true, true).is_ok());
    let mut extra = value.clone();
    extra["files_requested"] = json!(MAX_RESULTS + 1);
    extra["files_sent"]
        .as_array_mut()
        .unwrap()
        .push(json!({"index":MAX_RESULTS,"bytes":1,"durable":true}));
    let failure = check(&extra, true, true).unwrap_err();
    let terminal = retained(failure);
    assert_eq!(terminal["app_upload_evidence_incomplete"], true);
    assert_eq!(
        terminal["files_sent"].as_array().unwrap().len(),
        MAX_RESULTS
    );
    value["files_commands"] = json!({"originally_enabled":true,"requests":[]});
    assert_eq!(
        check(&value, true, true).unwrap_err().code,
        "app_upload_outcome_unknown"
    );
}

#[test]
fn producer_refusal_strings_are_never_copied_without_validation() {
    let mut value = completion(false);
    published(&mut value);
    value["files_refused"][0]["reason"] = json!("/private/selected/file\nsecret contents");
    let failure = check(&value, true, true).unwrap_err();
    let terminal = retained(failure).to_string();
    assert!(terminal.contains("app_upload_evidence_incomplete"));
    assert!(!terminal.contains("/private/") && !terminal.contains("secret contents"));
}

#[test]
fn later_viewer_failure_preserves_original_upload_results_once() {
    let mut value = completion(false);
    published(&mut value);
    let failure = check(&value, true, false).unwrap_err();
    assert_eq!(failure.code, "app_session_outcome_unknown");
    let report = retained(failure);
    assert_eq!(report["files_sent"], value["files_sent"]);
    let evidence = Record::parse(&value).unwrap().evidence();
    let mut original = failure;
    preserve(&evidence, &mut original);
    assert_eq!(retained(original)["files_sent"], value["files_sent"]);
    assert_eq!(
        original
            .control_history
            .unwrap()
            .json
            .matches("\"files_requested\":")
            .count(),
        1
    );
}

#[test]
fn both_transfer_directions_keep_their_receipts_when_either_owner_fails() {
    for bad_upload in [false, true] {
        for bad_receive in [false, true] {
            let mut value = completion(false);
            published(&mut value);
            receive(&mut value, if bad_receive { "pending" } else { "finished" });
            if bad_upload {
                value["files_refused"][0]["reason"] = json!("publication_unknown");
            }
            let outcome =
                session_notice_with_transfers(&capture(&value), "workstation", true, true, true);
            if bad_upload || bad_receive {
                let failure = outcome.unwrap_err();
                assert!(failure.unknown_effect);
                let terminal = retained(failure);
                assert_eq!(terminal["files_sent"], value["files_sent"]);
                assert_eq!(terminal["files_received"], value["files_received"]);
            } else {
                let notice = outcome.unwrap();
                assert!(notice.contains("Uploads: 2 of 3 published by host (19 bytes)"));
                assert!(notice.contains("Downloads: 1 published (23 bytes)"));
                assert!(notice.len() <= super::super::MAX_APP_NOTICE);
            }
        }
    }
}

#[test]
fn maximum_width_notices_retain_both_direction_counts_and_unconfirmed_durability() {
    let bytes = u64::MAX / MAX_RESULTS as u64;
    for macos in [false, true] {
        let mut value = completion(macos);
        value["files_requested"] = json!(MAX_RESULTS);
        value["files_absence"] = Value::Null;
        value["files_sent"] = Value::Array(
            (0..MAX_RESULTS)
                .map(|index| json!({"index":index,"bytes":bytes,"durable":false}))
                .collect(),
        );
        if macos {
            source(&mut value, "finished");
            value["files_upload_started"] = json!(MAX_RESULTS);
            value["files_upload_cancel_requested"] = json!(true);
        }
        receive(&mut value, "finished");
        value["files_received"] = value["files_sent"].clone();
        value["files_receive_cancel_requested"] = json!(true);
        let notice =
            session_notice_with_transfers(&capture(&value), "workstation", true, true, true)
                .unwrap();
        assert!(notice.starts_with("Uploads: 64 of 64 published by host"));
        assert!(notice.contains("Downloads: 64 published"));
        assert_eq!(notice.matches("64 durability unconfirmed").count(), 2);
        assert_eq!(
            notice
                .matches(&(bytes * MAX_RESULTS as u64).to_string())
                .count(),
            2
        );
        assert!(notice.contains("receive cleanup finished"));
        assert!(notice.len() <= super::super::MAX_APP_NOTICE);
    }
}

#[test]
fn only_named_preflight_refusals_permit_absent_upload_evidence() {
    for (code, exit, allowed) in [
        ("invalid_arguments", 2, true),
        ("receive_directory_choice_cancelled", 130, true),
        ("receive_directory_choice_expired", 1, true),
        ("receive_directory_picker_unavailable", 1, true),
        ("receive_directory_choice_event_overflow", 1, true),
        ("receive_directory_picker_cleanup_incomplete", 1, false),
        ("host_not_heard", 1, false),
        ("cancelled", 130, false),
    ] {
        let value = retained(Failure::new(code, "The original request ended.", exit));
        assert_eq!(check(&value, true, false).is_ok(), allowed, "{code}");
    }
    let mut value = retained(Failure::new("invalid_arguments", "Check local options.", 2));
    value["outcome"] = json!("unknown_external_effect");
    assert!(check(&value, true, false).is_err());
}

#[test]
fn unsuccessful_output_drain_retains_only_the_original_final_document() {
    use fr_client::completion::NATIVE_COMPLETION_LINE_LIMIT;
    use std::io::{self, Read};
    struct FailedTail;
    impl Read for FailedTail {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::ErrorKind::Other.into())
        }
    }
    let mut value = completion(false);
    published(&mut value);
    let terminal = format!("{value}\n");
    let failed = capture_completion_with_limit(
        terminal.as_bytes().chain(FailedTail),
        CompletionLimit::NativeCommand,
    );
    let mut overflow = vec![b'x'; NATIVE_COMPLETION_LINE_LIMIT + 1];
    overflow.push(b'\n');
    overflow.extend_from_slice(terminal.as_bytes());
    let oversized =
        capture_completion_with_limit(overflow.as_slice(), CompletionLimit::NativeCommand);
    for output in [failed, oversized] {
        assert!(output.completion().is_err());
        let failure =
            session_notice_with_transfers(&output, "workstation", true, true, false).unwrap_err();
        assert_eq!(failure.code, "app_upload_outcome_unknown");
        assert_eq!(retained(failure)["files_sent"], value["files_sent"]);
    }
    overflow.extend_from_slice(b"later invalid terminal\n");
    let final_invalid =
        capture_completion_with_limit(overflow.as_slice(), CompletionLimit::NativeCommand);
    let failure = session_notice_with_transfers(&final_invalid, "workstation", true, true, false)
        .unwrap_err();
    assert!(failure.unknown_effect && failure.control_history.is_none());
}

#[test]
#[cfg(any(
    all(target_os = "linux", feature = "linux-desktop"),
    all(target_os = "macos", feature = "macos-desktop")
))]
fn actual_upload_formatter_roundtrips_mixed_publications_and_aggregate_drops() {
    use crate::file_reports::upload::{Summary, reported};
    use frd::{
        native_files::{Publication, SendPhase, SendStatus},
        session_startup::FileSendOutcome,
    };
    let mut summary = reported(
        &[Some(0), Some(19), Some(21)],
        [
            FileSendOutcome::HostPublished {
                bytes: 0,
                publication: Publication::Durable,
            },
            FileSendOutcome::HostPublished {
                bytes: 19,
                publication: Publication::DurabilityUnknown,
            },
            FileSendOutcome::HostRefused(fr_wire::files::Reason::Conflict),
        ]
        .into_iter(),
    );
    let value: Value = serde_json::from_str(&format!("{{\"schema_version\":1,\"outcome\":\"stopped\",\"role\":\"control\",\"cleanup_confirmed\":true{}}}", summary.json())).unwrap();
    assert!(
        check(&value, true, true)
            .unwrap()
            .contains("2 of 3 published by host")
    );
    summary.append_later(
        &[Some(11)],
        Ok(&SendStatus {
            phase: SendPhase::WaitingForControl,
            absence: None,
            report: None,
        }),
    );
    let value: Value = serde_json::from_str(&format!("{{\"schema_version\":1,\"outcome\":\"stopped\",\"role\":\"control\",\"cleanup_confirmed\":true{}}}", summary.json())).unwrap();
    assert!(
        check(&value, true, true)
            .unwrap()
            .contains("2 of 4 published by host")
    );
    let pending = Summary::unavailable(&[], "files_not_started");
    let mut failure = Failure::new("host_not_heard", "Check the original host.", 1);
    pending.preserve(&mut failure);
    let value = retained(failure);
    assert_eq!(value["files_requested"], 0);
    assert!(
        check(&value, true, false)
            .unwrap()
            .starts_with("Uploads did not start (files_not_started).")
    );
}
