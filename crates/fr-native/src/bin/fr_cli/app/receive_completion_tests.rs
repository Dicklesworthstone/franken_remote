use super::super::session_notice_checked;
use super::*;
use fr_client::completion::{CompletionLimit, capture_completion_with_limit};

fn metadata() -> Value {
    json!({
        "files_receive_requested": true,
        "files_received": [],
        "files_receive_refused": [],
        "files_receive_absence": null,
        "files_receive_ended": null,
        "files_receive_cleanup": "finished",
        "files_receive_progress": null,
        "files_receive_results_truncated": false,
        "files_receive_cancel_requested": false,
    })
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

fn folder_completion(macos: bool) -> Value {
    let mut value = completion(macos);
    value["files_received"] = json!([{"index":0,"bytes":19,"durable":true}]);
    value["folder_job"] = json!({
        "id":41,"phase":"cancelled","revision":3,"rounds_settled":2,
        "confirmed_publications":1,"failure":"sync_cancelled","cleanup":"finished"
    });
    value
}

fn check_job(value: &Value, job: Option<u64>) -> Result<String, Failure> {
    super::super::session_notice_with_job(&capture(value), "workstation", true, false, true, job)
}

#[test]
fn folder_completion_keeps_original_job_and_publications_after_cancellation() {
    for macos in [false, true] {
        let value = folder_completion(macos);
        let notice = check_job(&value, Some(41)).unwrap();
        assert!(notice.contains("1 published (19 bytes)"));
        assert!(
            notice
                .contains("Folder job 41: cancelled, 2 rounds settled, 1 confirmed publications.")
        );
        assert!(notice.contains("sync_cancelled"));
        let inspection = inspect_job(&capture(&value), true, Some(41), None).unwrap();
        let mut failure = Failure::new("session_failed", "The original viewer stopped.", 1);
        inspection.preserve(&mut failure);
        let report = retained(failure);
        assert_eq!(report["folder_job"], value["folder_job"]);
        assert_eq!(report["files_received"], value["files_received"]);

        // A job observation is not another publication receipt. A final disk
        // receipt can strengthen evidence after the job's last counter sample.
        let mut later_receipt = value;
        later_receipt["files_received"] = json!([
            {"index":0,"bytes":19,"durable":true},
            {"index":1,"bytes":7,"durable":false},
        ]);
        let notice = check_job(&later_receipt, Some(41)).unwrap();
        assert!(notice.contains("2 published (26 bytes)"));
        assert!(notice.contains("1 durability unconfirmed"));
        assert!(notice.contains("1 confirmed publications"));
    }
}

#[test]
fn folder_completion_preserves_available_conflict_counts_and_leaves_legacy_counts_unknown() {
    let mut value = folder_completion(false);
    assert!(
        !check_job(&value, Some(41))
            .unwrap()
            .contains("Conflict copies")
    );
    value["folder_job"]["confirmed_conflicts"] = json!(1);
    assert!(
        check_job(&value, Some(41))
            .unwrap()
            .contains("Conflict copies kept: 1")
    );
    let inspection = inspect_job(&capture(&value), true, Some(41), None).unwrap();
    let mut failure = Failure::new("session_failed", "The original viewer stopped.", 1);
    inspection.preserve(&mut failure);
    assert_eq!(retained(failure)["folder_job"], value["folder_job"]);
}

#[test]
fn wrong_missing_or_unrequested_folder_job_never_authorizes_another_picker() {
    let value = folder_completion(false);
    for job in [None, Some(42)] {
        let failure = check_job(&value, job).unwrap_err();
        assert_eq!(failure.code, "app_receive_outcome_unknown");
        let report = retained(failure);
        assert_eq!(report["folder_job"], value["folder_job"]);
        assert_eq!(report["files_received"], value["files_received"]);
    }
    let mut missing = value;
    missing.as_object_mut().unwrap().remove("folder_job");
    assert_eq!(
        check_job(&missing, Some(41)).unwrap_err().code,
        "app_receive_outcome_unknown"
    );
    let mut not_started = completion(false);
    not_started["files_receive_cleanup"] = json!("not_started");
    not_started["files_receive_absence"] = json!("host_download_unavailable");
    assert!(
        check_job(&not_started, Some(41))
            .unwrap()
            .starts_with("Receiving did not start")
    );
    let standalone = json!({"folder_job":folder_completion(false)["folder_job"]});
    assert!(inspect_job(&capture(&standalone), false, None, None).is_err());

    let mut setup_refused = completion(false);
    setup_refused["files_receive_ended"] = json!("files_receive_setup_failed");
    assert!(check_job(&setup_refused, Some(41)).is_ok());
    for (field, invalid) in [
        ("files_receive_ended", json!("receive_cancelled")),
        ("files_receive_ended", json!("files_receive_failed")),
        ("files_receive_ended", Value::Null),
        ("files_receive_cleanup", json!("pending")),
        ("files_receive_cleanup", json!("failed")),
        ("files_receive_results_truncated", json!(true)),
        ("files_receive_requested", json!(false)),
        (
            "files_receive_progress",
            json!({"index":0,"staged_bytes":0,"total_bytes":19}),
        ),
        (
            "files_receive_refused",
            json!([{"index":0,"reason":"receive_cancelled"}]),
        ),
    ] {
        let mut contradictory = setup_refused.clone();
        contradictory[field] = invalid;
        assert_eq!(
            check_job(&contradictory, Some(41)).unwrap_err().code,
            "app_receive_outcome_unknown"
        );
    }
    setup_refused["files_received"] = json!([{"index":0,"bytes":19,"durable":true}]);
    let failure = check_job(&setup_refused, Some(41)).unwrap_err();
    assert_eq!(failure.code, "app_receive_outcome_unknown");
    assert_eq!(
        retained(failure)["files_received"],
        setup_refused["files_received"]
    );
}

#[test]
fn folder_job_metadata_is_bounded_sanitized_and_consistent_with_owner_cleanup() {
    for (field, invalid) in [
        ("id", json!(0)),
        ("rounds_settled", json!(4)),
        ("confirmed_publications", json!(65)),
        ("confirmed_conflicts", json!(2)),
        ("confirmed_conflicts", json!(-1)),
        ("phase", json!("/private/untrusted")),
        ("failure", json!("/private/untrusted")),
        ("failure", Value::Null),
        ("cleanup", json!("pending")),
        ("cleanup", json!("not_started")),
    ] {
        let mut value = folder_completion(false);
        value["folder_job"][field] = invalid;
        let failure = check_job(&value, Some(41)).unwrap_err();
        assert_eq!(failure.code, "app_receive_outcome_unknown");
        let report = retained(failure);
        assert_eq!(report["files_received"], value["files_received"]);
        assert!(!report.to_string().contains("/private/untrusted"));
    }
    for failure in ["sync_publication_unknown", "sync_cleanup_unconfirmed"] {
        let mut value = folder_completion(false);
        value["folder_job"]["phase"] = json!("failed");
        value["folder_job"]["failure"] = json!(failure);
        let report = retained(check_job(&value, Some(41)).unwrap_err());
        assert_eq!(report["folder_job"]["failure"], failure);
        assert_eq!(report["files_received"], value["files_received"]);
    }
    let mut active = folder_completion(false);
    active["folder_job"]["phase"] = json!("settled");
    active["folder_job"]["failure"] = Value::Null;
    assert_eq!(
        check_job(&active, Some(41)).unwrap_err().code,
        "app_receive_outcome_unknown"
    );
    active["folder_job"]["cleanup"] = json!("pending");
    active["files_receive_cleanup"] = json!("pending");
    let failure = check_job(&active, Some(41)).unwrap_err();
    assert_eq!(failure.code, "app_receive_cleanup_unconfirmed");
    assert_eq!(retained(failure)["folder_job"], active["folder_job"]);
}

fn check(value: &Value, expected: bool, success: bool) -> Result<String, Failure> {
    session_notice_checked(&capture(value), "workstation", success, expected)
}

fn retained(failure: Failure) -> Value {
    serde_json::from_str(&crate::terminal::output(failure, true)).unwrap()
}

fn refusal(code: &'static str, exit: u8) -> Value {
    serde_json::from_str(&crate::terminal::output(
        Failure::new(code, "The original request was refused.", exit),
        true,
    ))
    .unwrap()
}

#[test]
fn clean_completion_names_publication_refusal_and_independent_durability() {
    for macos in [false, true] {
        let mut value = completion(macos);
        value["files_received"] = json!([
            {"index":0,"bytes":0,"durable":true},
            {"index":1,"bytes":19,"durable":false},
        ]);
        value["files_receive_refused"] = json!([{"index":2,"reason":"receive_conflict"}]);
        // Even a fully staged object is not another publication.
        value["files_receive_progress"] = json!({"index":3,"staged_bytes":41,"total_bytes":41});
        let notice = check(&value, true, true).unwrap();
        assert!(notice.starts_with("Downloads: 2 published (19 bytes), 1 refused; 1 durability unconfirmed; receive cleanup finished."));
        assert!(notice.contains("local cleanup was confirmed"));
        assert!(!notice.contains("3 published"));
        assert!(notice.len() <= super::super::MAX_APP_NOTICE);
    }
}

#[test]
fn explicit_not_started_is_distinct_from_a_joined_receiver_and_from_missing_evidence() {
    let mut value = completion(false);
    value["files_receive_cleanup"] = json!("not_started");
    value["files_receive_absence"] = json!("host_download_unavailable");
    assert!(
        check(&value, true, true)
            .unwrap()
            .starts_with("Receiving did not start (host_download_unavailable).")
    );
    value["files_receive_requested"] = json!(false);
    value["files_receive_absence"] = Value::Null;
    let notice = check(&value, false, true).unwrap();
    assert!(!notice.contains("Downloads:") && !notice.contains("Receiving did not start"));
    assert_eq!(
        check(&value, true, true).unwrap_err().code,
        "app_receive_outcome_unknown"
    );
    value
        .as_object_mut()
        .unwrap()
        .retain(|key, _| !key.starts_with("files_receive") && key != "files_received");
    assert!(check(&value, false, true).is_ok());
    assert_eq!(
        check(&value, true, true).unwrap_err().code,
        "app_receive_outcome_unknown"
    );
}

#[test]
fn global_viewer_cleanup_never_substitutes_for_the_disk_owner() {
    for macos in [false, true] {
        for cleanup in ["pending", "failed"] {
            let mut value = completion(macos);
            value["files_received"] = json!([{"index":0,"bytes":42,"durable":true}]);
            value["files_receive_cleanup"] = json!(cleanup);
            let failure = check(&value, true, true).unwrap_err();
            assert_eq!(failure.code, "app_receive_cleanup_unconfirmed");
            assert!(failure.unknown_effect);
            let report = retained(failure);
            assert_eq!(report["outcome"], "unknown_external_effect");
            assert_eq!(report["files_received"][0]["bytes"], 42);
            assert_eq!(report["files_receive_cleanup"], cleanup);
        }
    }
}

#[test]
fn earlier_publication_survives_later_unknown_and_truncated_results() {
    for field in ["receipt", "ended", "truncated"] {
        let mut value = completion(false);
        value["files_received"] = json!([{"index":0,"bytes":42,"durable":false}]);
        match field {
            "receipt" => {
                value["files_receive_refused"] =
                    json!([{"index":1,"reason":"publication_unknown"}]);
            }
            "ended" => value["files_receive_ended"] = json!("publication_unknown"),
            _ => value["files_receive_results_truncated"] = json!(true),
        }
        let failure = check(&value, true, true).unwrap_err();
        assert_eq!(failure.code, "app_receive_outcome_unknown");
        let report = retained(failure);
        assert_eq!(report["files_received"], value["files_received"]);
        assert_eq!(
            report["files_receive_refused"],
            value["files_receive_refused"]
        );
        assert_eq!(report["files_receive_ended"], value["files_receive_ended"]);
        assert_eq!(
            report["files_receive_results_truncated"],
            value["files_receive_results_truncated"]
        );
    }
}

#[test]
fn cancellation_preserves_joined_publication_but_cannot_hide_unknown_receipts() {
    for exit in [1, 130] {
        let mut value = refusal(
            if exit == 130 {
                "cancelled"
            } else {
                "native_session_failed"
            },
            exit,
        );
        value
            .as_object_mut()
            .unwrap()
            .extend(metadata().as_object().unwrap().clone());
        value["files_received"] = json!([{"index":0,"bytes":19,"durable":true}]);
        value["files_receive_ended"] = json!("receive_cancelled");
        value["files_receive_cancel_requested"] = json!(true);
        let notice = check(&value, true, false).unwrap();
        assert!(notice.starts_with("Downloads: 1 published (19 bytes)"));
        assert!(notice.contains("Receive cancellation was requested"));
        value["files_receive_refused"] = json!([{"index":1,"reason":"publication_unknown"}]);
        let failure = check(&value, true, false).unwrap_err();
        assert_eq!(failure.code, "app_receive_outcome_unknown");
        assert_eq!(retained(failure)["files_received"][0]["bytes"], 19);
    }
}

#[test]
fn only_known_simple_pre_start_refusals_may_omit_receive_metadata() {
    for code in [
        "files_directory_missing",
        "files_directory_symlink",
        "files_limits_invalid",
        "receive_requires_control",
        "invalid_arguments",
        "invalid_trust_store",
    ] {
        let value = refusal(code, 2);
        assert!(check(&value, true, false).is_ok(), "{code}");
        let mut contradictory = value.clone();
        contradictory["control_granted"] = json!(true);
        assert_eq!(
            check(&contradictory, true, false).unwrap_err().code,
            "app_receive_outcome_unknown"
        );
        let mut history = value;
        history["attempts_ended"] = json!([{"attempt":1,"code":"native_session_failed"}]);
        assert!(check(&history, true, false).unwrap_err().unknown_effect);
    }
    for (code, exit) in [
        ("native_session_failed", 1),
        ("host_not_answering", 1),
        ("cancelled", 130),
        ("native_cleanup_incomplete", 1),
        ("files_directory_unknown_future_code", 2),
    ] {
        assert_eq!(
            check(&refusal(code, exit), true, false).unwrap_err().code,
            "app_receive_outcome_unknown"
        );
    }
}

#[test]
fn missing_or_malformed_metadata_fences_and_preserves_sanitized_partial_receipts() {
    for field in metadata().as_object().unwrap().keys() {
        let mut value = completion(false);
        value["files_received"] =
            json!([{"index":0,"bytes":19,"durable":true,"name":"/private/secret"}]);
        value.as_object_mut().unwrap().remove(field);
        let failure = check(&value, true, true).unwrap_err();
        assert!(failure.unknown_effect, "{field}");
        let report = retained(failure);
        assert_eq!(report["app_receive_evidence_incomplete"], true);
        assert!(!report.to_string().contains("/private/secret"));
        if field != "files_received" {
            assert_eq!(report["files_received"][0]["bytes"], 19);
        }
    }
    for (field, malformed) in [
        ("files_receive_requested", json!("true")),
        ("files_receive_cleanup", json!("finished_maybe")),
        ("files_receive_results_truncated", Value::Null),
        ("files_receive_cancel_requested", json!(0)),
        ("files_receive_absence", json!("/private/secret")),
        ("files_receive_ended", json!("unknown_future_state")),
        (
            "files_received",
            json!([{"index":0,"bytes":"19","durable":true}]),
        ),
        (
            "files_receive_refused",
            json!([{"index":0,"reason":"/private/secret"}]),
        ),
        (
            "files_receive_progress",
            json!({"index":1,"staged_bytes":8,"total_bytes":7}),
        ),
    ] {
        let mut value = completion(false);
        value[field] = malformed;
        let failure = check(&value, true, true).unwrap_err();
        assert!(failure.unknown_effect, "{field}");
        assert!(!crate::terminal::output(failure, true).contains("/private/secret"));
    }
}

#[test]
fn contradictory_no_start_duplicate_receipts_and_bounds_cannot_claim_clean_completion() {
    let mut value = completion(false);
    value["files_received"] = json!([{"index":0,"bytes":1,"durable":true}]);
    for (field, contradicting) in [
        ("files_receive_requested", json!(false)),
        ("files_receive_cleanup", json!("not_started")),
        ("files_receive_absence", json!("control_not_granted")),
        ("files_receive_ended", json!("receive_invalid_result")),
        (
            "files_receive_refused",
            json!([{"index":0,"reason":"receive_conflict"}]),
        ),
    ] {
        let mut report = value.clone();
        report[field] = contradicting;
        assert!(
            check(&report, true, true).unwrap_err().unknown_effect,
            "{field}"
        );
    }
    value["files_received"] = json!([
        {"index":0,"bytes":u64::MAX,"durable":true},
        {"index":1,"bytes":1,"durable":true},
    ]);
    assert!(check(&value, true, true).unwrap_err().unknown_effect);
    value["files_received"] = Value::Array(
        (0..=MAX_RESULTS)
            .map(|index| json!({"index":index,"bytes":1,"durable":true}))
            .collect(),
    );
    let failure = check(&value, true, true).unwrap_err();
    let retained = retained(failure);
    assert_eq!(retained["app_receive_evidence_incomplete"], true);
    assert!(retained["files_received"].as_array().unwrap().len() <= MAX_RESULTS);
}

#[test]
fn unexpected_receiving_and_other_session_uncertainty_keep_publication_evidence() {
    let mut value = completion(false);
    value["files_received"] = json!([{"index":0,"bytes":19,"durable":true}]);
    assert_eq!(
        check(&value, false, true).unwrap_err().code,
        "app_receive_outcome_unknown"
    );
    // A failed process cannot certify a successful viewer, even with a joined
    // receiver. Its already-published files still belong in the failure report.
    let failure = check(&value, true, false).unwrap_err();
    assert_eq!(failure.code, "app_session_outcome_unknown");
    assert_eq!(retained(failure)["files_received"][0]["bytes"], 19);
    let mut unknown = refusal("disconnect_outcome_unknown", 1);
    unknown["outcome"] = json!("unknown_external_effect");
    unknown.as_object_mut().unwrap().extend(
        value
            .as_object()
            .unwrap()
            .iter()
            .filter(|(key, _)| key.starts_with("files_receive") || key.as_str() == "files_received")
            .map(|(key, value)| (key.clone(), value.clone())),
    );
    let failure = check(&unknown, true, false).unwrap_err();
    assert_eq!(failure.code, "app_child_outcome_unknown");
    assert_eq!(retained(failure)["files_received"][0]["bytes"], 19);
}

#[test]
fn malformed_final_capture_never_recovers_an_earlier_receive_success() {
    let value = completion(false);
    for tail in ["{broken", "viewer failed", "{}"] {
        let capture = capture_completion_with_limit(
            format!("{value}\n{tail}\n").as_bytes(),
            CompletionLimit::NativeCommand,
        );
        assert!(
            session_notice_checked(&capture, "workstation", true, true)
                .unwrap_err()
                .unknown_effect
        );
    }
}

#[test]
#[cfg(any(
    all(target_os = "linux", feature = "linux-desktop"),
    all(target_os = "macos", feature = "macos-desktop")
))]
fn actual_native_formatter_and_terminal_preservation_match_the_application_contract() {
    use crate::file_reports::download::Summary;
    use fr_files::{
        receive::Publication,
        session,
        wire::ResultReceipt,
        worker::{Completion, Error as WorkerError},
    };
    use frd::native_files::{ReceivePhase, ReceiveStatus};
    let mut status = ReceiveStatus {
        folder_job: None,
        phase: ReceivePhase::Ended,
        absence: None,
        progress: None,
        results: vec![ResultReceipt {
            id: 1,
            outcome: Ok(Completion::Published(session::Receipt {
                id: 1,
                bytes: 19,
                publication: Publication::DurabilityUnknown,
            })),
        }],
        results_truncated: false,
        cancel_requested: false,
        cleanup: Some(Ok(())),
    };
    let summary = Summary::from_status(&status);
    let value: Value = serde_json::from_str(&format!(
        "{{\"schema_version\":1,\"outcome\":\"stopped\",\"role\":\"control\",\"cleanup_confirmed\":true{}}}",
        summary.json(),
    )).unwrap();
    assert!(
        check(&value, true, true)
            .unwrap()
            .contains("1 durability unconfirmed")
    );

    // Real producer failure preservation keeps the earlier publication when a
    // later object has an unknown publication outcome, even after owner join.
    status.results.push(ResultReceipt {
        id: 2,
        outcome: Err(WorkerError::UnknownEffect),
    });
    let mut original = Failure::new("cancelled", "The original session was stopped.", 130);
    Summary::from_status(&status).preserve(&mut original);
    let value = retained(original);
    let failure = check(&value, true, false).unwrap_err();
    let reported = retained(failure);
    assert_eq!(
        reported["files_received"][0],
        json!({"index":0,"bytes":19,"durable":false})
    );
    assert_eq!(
        reported["files_receive_refused"][0]["reason"],
        "publication_unknown"
    );

    // An unavailable receiver is explicit pre-start evidence, not a guessed
    // success derived from the overall native cleanup boolean.
    let mut original = Failure::new("host_not_answering", "The host is unavailable.", 1);
    Summary::unavailable("files_receive_not_started").preserve(&mut original);
    assert!(
        check(&retained(original), true, false)
            .unwrap()
            .starts_with("Receiving did not start")
    );
}

#[test]
fn incomplete_capture_preserves_only_the_complete_final_receive_document() {
    use fr_client::completion::NATIVE_COMPLETION_LINE_LIMIT;
    use std::io::{self, Read};

    struct FailedTail;
    impl Read for FailedTail {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "capture failed"))
        }
    }

    let mut value = completion(false);
    value["files_received"] = json!([{"index":0,"bytes":19,"durable":false}]);
    let terminal = format!("{value}\n");
    let broken_pipe = capture_completion_with_limit(
        terminal.as_bytes().chain(FailedTail),
        CompletionLimit::NativeCommand,
    );
    let partial = format!("{terminal}unfinished diagnostic");
    let partial_tail = capture_completion_with_limit(
        partial.as_bytes().chain(FailedTail),
        CompletionLimit::NativeCommand,
    );
    let mut oversized = vec![b'x'; NATIVE_COMPLETION_LINE_LIMIT + 1];
    oversized.push(b'\n');
    oversized.extend_from_slice(terminal.as_bytes());
    let overflow =
        capture_completion_with_limit(oversized.as_slice(), CompletionLimit::NativeCommand);

    for capture in [broken_pipe, partial_tail, overflow] {
        assert!(capture.completion().is_err());
        assert!(capture.retained_document().is_some());
        for expected in [false, true] {
            let failure =
                session_notice_checked(&capture, "workstation", true, expected).unwrap_err();
            assert_eq!(failure.code, "app_receive_outcome_unknown");
            let report = retained(failure);
            assert_eq!(report["outcome"], "unknown_external_effect");
            assert_eq!(report["files_received"], value["files_received"]);
        }
    }

    // The final candidate owns the evidence, even after an earlier overflow.
    // A later non-document cannot resurrect the publication above.
    oversized.extend_from_slice(b"viewer failed\n");
    let invalid_tail =
        capture_completion_with_limit(oversized.as_slice(), CompletionLimit::NativeCommand);
    let failure = session_notice_checked(&invalid_tail, "workstation", true, true).unwrap_err();
    assert!(failure.unknown_effect);
    assert!(failure.files_receive.is_none());
}
