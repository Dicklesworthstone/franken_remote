use super::*;
use frd::native_files::local::Status;

fn command(action: Action) -> Options {
    Options {
        target: "n-host".into(),
        connection: "a".repeat(32),
        lease: Some("b".repeat(32)),
        action,
        json: true,
    }
}
fn json(output: &Output) -> Value {
    serde_json::from_str(&output.text).unwrap()
}
fn info(status: Status) -> Info {
    Info {
        target: "a".repeat(32),
        instance: "b".repeat(32),
        status,
    }
}

#[test]
fn admission_does_not_claim_publication_or_cleanup() {
    let command = command(Action::Send(vec!["/private/do-not-disclose".into()]));
    let info = info(Status {
        sequence: 1,
        selected: 1,
        phase: Phase::WaitingForControl,
        ..Status::default()
    });
    let output = json(&success(&command, &info, "send_admitted", None));
    assert_eq!(output["outcome"], "success");
    let data = &output["data"];
    assert_eq!(data["operation"], "send_admitted");
    assert_eq!(data["request"], 1);
    assert_eq!(data["selected"], 1);
    assert_eq!(data["published"], 0);
    assert_eq!(data["complete"], false);
    assert_eq!(data["cleanup"], "unknown");
    assert!(!output.to_string().contains("do-not-disclose"));
}

#[test]
fn cancellation_keeps_actual_publication_uncertainty_and_original_request() {
    let command = command(Action::Cancel(9));
    let info = info(Status {
        sequence: 9,
        selected: 3,
        published: 1,
        publication_unknown: 1,
        durability_unknown: 1,
        phase: Phase::Ended,
        cleanup: Cleanup::Confirmed,
        cancel_requested: true,
        ..Status::default()
    });
    let output = json(&success(&command, &info, "cancel_requested", Some(false)));
    let data = &output["data"];
    assert_eq!(data["request"], 9);
    assert_eq!(data["published"], 1);
    assert_eq!(data["publication_unknown"], 1);
    assert_eq!(data["durability_unknown"], 1);
    assert_eq!(data["cleanup"], "confirmed");
    assert_eq!(data["complete"], false);
    assert_eq!(data["cancellation_requested_by_command"], false);
    assert_eq!(data["cancel_requested"], true);
}

#[test]
fn lost_reply_and_selection_refusal_keep_handles_but_never_paths() {
    let request = command(Action::Send(vec!["/private/do-not-disclose".into()]));
    let uncertain = failure(&request, Error::OutcomeUnknown);
    assert_eq!(uncertain.exit, 1);
    let output = json(&uncertain);
    assert_eq!(output["outcome"], "unknown_external_effect");
    assert_eq!(output["data"]["connection_id"], request.connection);
    assert_eq!(output["data"]["lease"], request.lease.unwrap());
    assert!(output["data"]["request"].is_null());
    assert!(!output.to_string().contains("do-not-disclose"));
    let mut text_command = command(Action::Send(vec!["/private/do-not-disclose".into()]));
    text_command.json = false;
    let refusal = failure(
        &text_command,
        Error::Selection {
            index: 2,
            reason: SelectionFailure::Symlink,
        },
    );
    assert!(refusal.text.contains("Selection #3: symlink"));
    assert!(!refusal.text.contains("do-not-disclose"));
}

#[test]
fn pacing_reports_local_intent_and_observed_gate_without_claiming_an_acknowledgement() {
    let mut command = command(Action::Resume(9));
    let observed = info(Status {
        sequence: 9,
        selected: 2,
        started: 2,
        published: 1,
        publication_unknown: 1,
        pause_requested: Some(false),
        paused: Some(true),
        ..Status::default()
    });
    let output = json(&success(
        &command,
        &observed,
        "resume_requested",
        Some(true),
    ));
    let data = &output["data"];
    assert_eq!(data["request"], 9);
    assert_eq!(data["pacing_changed_by_command"], true);
    assert!(data["cancellation_requested_by_command"].is_null());
    assert_eq!(data["pause_requested"], false);
    assert_eq!(data["paused"], true);
    assert_eq!(data["published"], 1);
    assert_eq!(data["publication_unknown"], 1);
    command.json = false;
    let text = success(&command, &observed, "resume_requested", Some(true)).text;
    assert!(text.contains("Resume intent changed: true"));
    assert!(text.contains("Pause requested: false; sender last observed paused: true"));
    let unknown = info(Status::default());
    assert!(
        success(&command, &unknown, "status", None)
            .text
            .contains("Pause requested: unknown")
    );
}

#[test]
fn pacing_refusal_and_uncertain_reply_retain_only_the_original_request() {
    for action in [Action::Pause(4), Action::Resume(4)] {
        let command = command(action);
        let output = json(&failure(&command, Error::OutcomeUnknown));
        assert_eq!(output["outcome"], "unknown_external_effect");
        assert_eq!(output["data"]["request"], 4);
        assert_eq!(output["data"]["lease"], command.lease.unwrap());
    }
}
