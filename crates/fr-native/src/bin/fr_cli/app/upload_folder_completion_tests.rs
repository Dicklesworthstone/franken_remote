//! Completion projection tests use typed terminal documents, not live transfers.
use super::super::{FolderJobs, session_notice_with_jobs};
use super::*;
use fr_client::completion::{CompletionLimit, capture_completion_with_limit};

fn capture(value: &Value) -> CompletionCapture {
    capture_completion_with_limit(value.to_string().as_bytes(), CompletionLimit::NativeCommand)
}

fn completed(macos: bool) -> Value {
    let mut value = if macos {
        json!({
            "platform":"macos", "outcome":"success",
            "ended":"window_closed", "local_cleanup_confirmed":true,
        })
    } else {
        json!({
            "schema_version":1, "outcome":"stopped",
            "role":"control", "cleanup_confirmed":true,
        })
    };
    value.as_object_mut().unwrap().extend(
        json!({
            "files_requested":0, "files_sent":[], "files_refused":[], "files_absence":null,
            "upload_folder_job":{
                "id":41, "phase":"cancelled", "revision":17, "rounds_settled":16,
                "confirmed_publications":512, "confirmed_conflicts":27,
                "failure":"sync_cancelled", "cleanup":"finished",
            },
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    if macos {
        value.as_object_mut().unwrap().extend(
            json!({
                "files_upload_started":3, "files_upload_complete":true,
                "files_upload_cleanup":"finished", "files_upload_cancel_requested":true,
                "files_upload_progress":null,
            })
            .as_object()
            .unwrap()
            .clone(),
        );
    }
    value
}

fn check(value: &Value, job: Option<u64>, receive: bool, success: bool) -> Result<String, Failure> {
    session_notice_with_jobs(
        &capture(value),
        "workstation",
        success,
        UploadChoices {
            file_drop: job.is_some(),
            choose_files: false,
        },
        receive,
        FolderJobs {
            upload: job,
            download: None,
        },
    )
}

fn retained(failure: Failure) -> Value {
    serde_json::from_str(&crate::terminal::output(failure, true)).unwrap()
}

fn receiving(value: &mut Value, cleanup: &str) {
    value.as_object_mut().unwrap().extend(
        json!({
            "files_receive_requested":true,
            "files_received":[{"index":0,"bytes":23,"durable":true}],
            "files_receive_refused":[], "files_receive_absence":null,
            "files_receive_ended":null, "files_receive_cleanup":cleanup,
            "files_receive_progress":null, "files_receive_results_truncated":false,
            "files_receive_cancel_requested":false,
        })
        .as_object()
        .unwrap()
        .clone(),
    );
}

#[test]
fn cumulative_folder_publications_survive_zero_initial_paths_and_last_round_snapshots() {
    for macos in [false, true] {
        let value = completed(macos);
        let notice = check(&value, Some(41), false, true).unwrap();
        assert!(notice.starts_with("Folder upload job 41: cancelled, 512 confirmed publications"));
        assert!(notice.contains("27 conflict copies") && notice.contains("16 rounds settled"));
        assert!(!notice.contains("no files selected") && !notice.contains("0 published"));
        assert!(!notice.contains("512 bytes"));
        // A later unrelated viewer failure retains the original cumulative
        // counters without fabricating a per-file receipt or byte measurement.
        let failure = check(&value, Some(41), false, false).unwrap_err();
        assert_eq!(failure.code, "app_session_outcome_unknown");
        let output = retained(failure);
        assert_eq!(output["upload_folder_job"], value["upload_folder_job"]);
        assert_eq!(output["files_sent"], json!([]));
        if macos {
            assert_eq!(output["files_upload_started"], 3);
            assert_eq!(output["files_upload_cleanup"], "finished");
        }
    }
}

#[test]
fn original_job_identity_consent_and_terminal_cleanup_cannot_be_inferred_from_counters() {
    let value = completed(false);
    for expected in [None, Some(42)] {
        let failure = check(&value, expected, false, true).unwrap_err();
        assert_eq!(failure.code, "app_upload_outcome_unknown");
        assert_eq!(
            retained(failure)["upload_folder_job"],
            value["upload_folder_job"]
        );
    }
    let mut active = value.clone();
    active["upload_folder_job"]["phase"] = json!("settled");
    active["upload_folder_job"]["failure"] = Value::Null;
    assert_eq!(
        check(&active, Some(41), false, true).unwrap_err().code,
        "app_upload_outcome_unknown",
    );
    for cleanup in ["unconfirmed", "pending", "failed"] {
        let mut unfinished = completed(true);
        if cleanup == "unconfirmed" {
            unfinished["upload_folder_job"]["cleanup"] = json!(cleanup);
        } else {
            unfinished["files_upload_cleanup"] = json!(cleanup);
        }
        let failure = check(&unfinished, Some(41), false, true).unwrap_err();
        assert_eq!(failure.code, "app_upload_cleanup_unconfirmed");
        assert_eq!(
            retained(failure)["upload_folder_job"]["confirmed_publications"],
            512,
        );
    }
}

#[test]
fn publication_and_cleanup_failures_preserve_both_independent_file_directions() {
    for reason in ["sync_publication_unknown", "sync_cleanup_unconfirmed"] {
        let mut value = completed(true);
        receiving(&mut value, "finished");
        value["upload_folder_job"]["phase"] = json!("failed");
        value["upload_folder_job"]["failure"] = json!(reason);
        let failure = check(&value, Some(41), true, true).unwrap_err();
        assert_eq!(failure.code, "app_upload_outcome_unknown");
        let output = retained(failure);
        assert_eq!(output["upload_folder_job"], value["upload_folder_job"]);
        assert_eq!(output["files_received"], value["files_received"]);
    }

    let mut value = completed(true);
    receiving(&mut value, "pending");
    let failure = check(&value, Some(41), true, true).unwrap_err();
    assert_eq!(failure.code, "app_receive_cleanup_unconfirmed");
    let output = retained(failure);
    assert_eq!(output["upload_folder_job"], value["upload_folder_job"]);
    assert_eq!(output["files_received"], value["files_received"]);
}

#[test]
fn malformed_job_fields_cannot_leak_peer_text_or_erase_bounded_publication_evidence() {
    for (field, invalid) in [
        ("id", json!(0)),
        ("phase", json!("/private/secret")),
        ("failure", json!("/private/secret")),
        ("cleanup", json!("/private/secret")),
        ("confirmed_conflicts", json!(513)),
        ("rounds_settled", json!(18)),
    ] {
        let mut value = completed(true);
        value["upload_folder_job"][field] = invalid;
        let failure = check(&value, Some(41), false, true).unwrap_err();
        assert_eq!(failure.code, "app_upload_outcome_unknown");
        let output = retained(failure);
        assert_eq!(output["app_upload_evidence_incomplete"], true);
        assert_eq!(output["upload_folder_job"]["evidence_incomplete"], true);
        assert_eq!(output["upload_folder_job"]["confirmed_publications"], 512);
        assert!(!output.to_string().contains("/private/secret"));
        assert!(FolderJob::parse(output.get("upload_folder_job")).is_err());
    }
    let mut over_bound = completed(false);
    over_bound["upload_folder_job"]["confirmed_publications"] = json!(513);
    let output = retained(check(&over_bound, Some(41), false, true).unwrap_err());
    assert!(
        output["upload_folder_job"]
            .get("confirmed_publications")
            .is_none()
    );

    let mut last_round = completed(true);
    last_round["files_upload_started"] = json!(MAX_FOLDER_ROUND + 1);
    assert_eq!(
        check(&last_round, Some(41), false, true).unwrap_err().code,
        "app_upload_outcome_unknown",
    );
}

#[test]
fn absent_job_is_permitted_only_for_a_known_empty_setup_refusal() {
    for reason in [
        "files_not_started",
        "control_not_granted",
        "host_files_unavailable",
    ] {
        let mut value = completed(false);
        value.as_object_mut().unwrap().remove("upload_folder_job");
        value["files_absence"] = json!(reason);
        let notice = check(&value, Some(41), false, true).unwrap();
        assert!(notice.starts_with("Folder upload job 41 did not start"));
        value["files_absence"] = Value::Null;
        assert_eq!(
            check(&value, Some(41), false, true).unwrap_err().code,
            "app_upload_outcome_unknown",
        );
    }
    let mut value = completed(false);
    value.as_object_mut().unwrap().remove("upload_folder_job");
    value["files_absence"] = json!("no_files_selected");
    assert!(check(&value, Some(41), false, true).is_err());

    let local = retained(Failure::new(
        "files_sync_source_invalid",
        "Select a new private source directory.",
        2,
    ));
    // This exact local preflight refusal precedes both file owners.
    assert!(check(&local, Some(41), true, false).is_ok());
    let generic = retained(Failure::new(
        "cancelled",
        "The original attempt stopped.",
        130,
    ));
    assert!(check(&generic, Some(41), false, false).is_err());
}

#[test]
fn known_durability_failure_is_reported_without_discarding_committed_publications() {
    let mut value = completed(false);
    value["upload_folder_job"]["phase"] = json!("failed");
    value["upload_folder_job"]["failure"] = json!("sync_durability_unconfirmed");
    let notice = check(&value, Some(41), false, true).unwrap();
    assert!(notice.contains("512 confirmed publications"));
    assert!(notice.contains("unconfirmed durability"));
}
