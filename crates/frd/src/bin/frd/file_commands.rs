//! Reachable local host-file commands. No tailnet connection or remote path API.
#[path = "file_commands/options.rs"]
mod options;
use frd::native_files::local::{self, Cleanup, Error, Info, Phase, Stage};
use options::{Action, Options};
use serde_json::{Value, json};
use std::process::ExitCode;

pub(super) fn execute(args: &[String], json_output: bool) -> ExitCode {
    let Ok(options) = Options::parse(args) else {
        return failure(
            "invalid_file_command",
            "Use frd send --target NAME /absolute/file ...; --status inspects the current owner. History (--status --request N), --cancel N, --pause N and --resume N require its original --instance HEX.",
            json_output,
            2,
        );
    };
    match command(options) {
        Ok((info, operation, changed)) => {
            print_info(&info, operation, changed, json_output);
            ExitCode::SUCCESS
        }
        Err(failure) => report_command_error(&failure, json_output),
    }
}

struct CommandFailure {
    error: Error,
    target: String,
    instance: Option<String>,
    request: Option<u64>,
}

fn command(options: Options) -> Result<(Info, &'static str, Option<bool>), CommandFailure> {
    let request = match &options.action {
        Action::Status(sequence) => *sequence,
        Action::Cancel(sequence) | Action::Pause(sequence) | Action::Resume(sequence) => {
            Some(*sequence)
        }
        Action::Send(_) => None,
    };
    let instance = match options.instance {
        Some(instance) => instance,
        None => {
            local::inspect(&options.target)
                .map_err(|error| CommandFailure {
                    error,
                    target: options.target.clone(),
                    instance: None,
                    request,
                })?
                .instance
        }
    };
    // Keep that exact instance through the command. An owner replacement is a
    // refusal, never an instruction to discover and try a different session.
    let result = match options.action {
        Action::Send(paths) => local::send(&options.target, &instance, &paths)
            .map(|sent| (sent.info, "send_admitted", None)),
        Action::Status(sequence) => {
            local::status(&options.target, &instance, sequence).map(|info| (info, "status", None))
        }
        Action::Cancel(sequence) => local::cancel(&options.target, &instance, sequence)
            .map(|result| (result.info, "cancel_requested", Some(result.requested))),
        Action::Pause(sequence) => local::pause(&options.target, &instance, sequence)
            .map(|result| (result.info, "pause_requested", Some(result.changed))),
        Action::Resume(sequence) => local::resume(&options.target, &instance, sequence)
            .map(|result| (result.info, "resume_requested", Some(result.changed))),
    };
    result.map_err(|error| CommandFailure {
        error,
        target: options.target,
        instance: Some(instance),
        request,
    })
}

fn report_command_error(failure: &CommandFailure, json_output: bool) -> ExitCode {
    if json_output {
        println!("{}", command_failure_evidence(failure));
    } else {
        let (code, detail) = error_detail(failure.error);
        eprintln!("{code}: {detail}");
        if let Some(instance) = &failure.instance {
            let request = failure
                .request
                .map_or_else(String::new, |sequence| format!(" --request {sequence}"));
            eprintln!(
                "Inspect the original owner: frd send --target {} --instance {instance} --status{request}",
                failure.target
            );
        }
    }
    ExitCode::from(1)
}

fn command_failure_evidence(failure: &CommandFailure) -> Value {
    let (code, detail) = error_detail(failure.error);
    let mut value = json!({
        "type": "host_file_command", "ok": false,
        "target": failure.target, "instance": failure.instance,
        "error": {"code": code, "detail": detail}
    });
    if let Some(request) = failure.request {
        value["request"] = json!(request);
    }
    value
}

pub(super) fn print_info(info: &Info, operation: &str, changed: Option<bool>, json_output: bool) {
    if json_output {
        println!("{}", evidence(info, operation, changed));
        return;
    }
    let status = &info.status;
    println!(
        "Host files: target {}, instance {}.",
        info.target, info.instance
    );
    if operation == "endpoint_opened" {
        println!(
            "Local file commands enabled. Connect a controller with --receive DIR, then use frd send --target {} /absolute/file.",
            info.target
        );
    } else if operation == "send_admitted" {
        println!(
            "Request {} admitted with {} selected item(s). Admission does not confirm publication.",
            status.sequence, status.selected
        );
    } else {
        match (operation, changed) {
            ("cancel_requested", Some(requested)) => println!(
                "Request {} cancellation requested: {requested}. Published files and uncertain effects are retained.",
                status.sequence
            ),
            ("pause_requested", Some(changed)) => println!(
                "Request {} pause intent changed: {changed}. The original transfer deadline continues.",
                status.sequence
            ),
            ("resume_requested", Some(changed)) => println!(
                "Request {} resume intent changed: {changed}. No transfer is retried or reopened.",
                status.sequence
            ),
            _ => {}
        }
    }
    println!(
        "State: {}; stage: {}; ready for another selection: {}; closed: {}.",
        phase(status.phase),
        stage(status.stage),
        status.ready,
        status.closed
    );
    if status.sequence != 0 {
        println!(
            "Pause requested: {}; sender last observed paused: {}. These are separate observations.",
            observation(status.pause_requested),
            observation(status.paused)
        );
        println!(
            "Request {}: {} published, {} refused, {} interrupted, {} publication unknown, {} durability unknown; complete: {}; failed: {}; source cleanup: {}.",
            status.sequence,
            status.published,
            status.refused,
            status.interrupted,
            status.publication_unknown,
            status.durability_unknown,
            status.complete,
            status.failed,
            cleanup(status.cleanup)
        );
        if let (Some(queued), Some(total)) = (status.queued_bytes, status.total_bytes) {
            println!(
                "Source bytes queued: {queued} of {total}. Queued bytes do not confirm publication."
            );
        }
        println!(
            "Inspect: frd send --target {} --instance {} --status --request {}",
            info.target, info.instance, status.sequence
        );
    }
}

fn evidence(info: &Info, operation: &str, changed: Option<bool>) -> Value {
    let s = &info.status;
    let cancellation = (operation == "cancel_requested")
        .then_some(changed)
        .flatten();
    let pacing = matches!(operation, "pause_requested" | "resume_requested")
        .then_some(changed)
        .flatten();
    json!({
        "type": "host_file_command", "operation": operation, "ok": true,
        "target": info.target, "instance": info.instance, "request": s.sequence,
        "ready": s.ready, "closed": s.closed, "phase": phase(s.phase), "stage": stage(s.stage),
        "selected": s.selected, "started": s.started, "published": s.published,
        "refused": s.refused, "interrupted": s.interrupted,
        "publication_unknown": s.publication_unknown, "durability_unknown": s.durability_unknown,
        "complete": s.complete, "failed": s.failed, "cancel_requested": s.cancel_requested,
        "pause_requested": s.pause_requested, "paused": s.paused,
        "pacing_changed_by_command": pacing,
        "cancellation_requested_by_command": cancellation,
        "queued_bytes": s.queued_bytes, "total_bytes": s.total_bytes, "cleanup": cleanup(s.cleanup)
    })
}

fn observation(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "true",
        Some(false) => "false",
        None => "unknown",
    }
}

fn phase(value: Phase) -> &'static str {
    match value {
        Phase::NoSelection => "no_selection",
        Phase::WaitingForControl => "waiting_for_control",
        Phase::Negotiating => "negotiating",
        Phase::WaitingForSelection => "waiting_for_selection",
        Phase::SelectionAccepted => "selection_accepted",
        Phase::Sending => "sending",
        Phase::Finished => "finished",
        Phase::Ended => "ended",
    }
}
fn stage(value: Stage) -> &'static str {
    match value {
        Stage::Unknown => "unknown",
        Stage::Idle => "idle",
        Stage::Queued => "queued",
        Stage::Preparing => "preparing",
        Stage::AwaitingAcceptance => "awaiting_acceptance",
        Stage::Streaming => "streaming",
        Stage::AwaitingProof => "awaiting_proof",
        Stage::Finished => "finished",
        Stage::Closed => "closed",
    }
}
fn cleanup(value: Cleanup) -> &'static str {
    match value {
        Cleanup::Unknown => "unknown",
        Cleanup::Confirmed => "confirmed",
        Cleanup::Failed => "failed",
    }
}

/// Only the host side (`frd run --send-control`) reports through this.
#[cfg(host_stack)]
pub(super) fn report_error(error: Error, json_output: bool) -> ExitCode {
    let (code, detail) = error_detail(error);
    failure(code, detail, json_output, 1)
}

fn error_detail(error: Error) -> (&'static str, &'static str) {
    match error {
        Error::InvalidTarget => (
            "file_command_invalid_target",
            "The local endpoint name must have 1 to 64 ASCII letters, digits, dots, underscores or hyphens.",
        ),
        Error::InvalidInstance => (
            "file_command_invalid_instance",
            "Use the original 32-digit hexadecimal instance reported by the host endpoint.",
        ),
        Error::InvalidPaths => (
            "file_command_invalid_paths",
            "Select up to eight existing absolute files or portable directory roots without parent traversal.",
        ),
        Error::InvalidRequest => (
            "file_command_invalid_request",
            "Use a nonzero request number from this original host instance.",
        ),
        Error::AlreadyRunning => (
            "file_command_already_running",
            "The selected local endpoint name is already occupied.",
        ),
        Error::NoSession => (
            "file_command_no_session",
            "No local host endpoint is listening under this name. Start frd run with --once, --input-agent and --send-control NAME.",
        ),
        Error::WrongInstance => (
            "file_command_wrong_instance",
            "The original host instance has ended. This command was not sent to its replacement.",
        ),
        Error::UntrustedPeer => (
            "file_command_untrusted_peer",
            "The local endpoint is not owned by the same effective user.",
        ),
        Error::Busy => (
            "file_command_busy",
            "The original file lane is not ready for another selection. Wait for control, attachment and the previous transfer to finish.",
        ),
        Error::Closed => (
            "file_command_closed",
            "The original file intake is closed. No selection is carried into a later session.",
        ),
        Error::Exhausted => (
            "file_command_exhausted",
            "This session has reached its bounded command history or transfer allowance.",
        ),
        Error::UnknownRequest => (
            "file_command_unknown_request",
            "This original instance has no record of that request number.",
        ),
        Error::Selection { .. } => (
            "file_selection_refused",
            "An explicitly selected source was refused. Check source existence, permissions, symlinks, portable names and limits.",
        ),
        Error::Unavailable => (
            "file_command_unavailable",
            "The local endpoint could not complete this operation.",
        ),
        Error::Protocol => (
            "file_command_protocol",
            "The local endpoint returned an unsupported or invalid response. This client requires local protocol v2; no fallback command was sent.",
        ),
        Error::Timeout => (
            "file_command_timeout",
            "The bounded local command deadline expired.",
        ),
        Error::OutcomeUnknown => (
            "file_command_outcome_unknown",
            "The command may have admitted a selection or changed its control before its reply was lost. Inspect the same original instance and request; do not automatically repeat send, cancel, pause or resume, or assume publication was rolled back.",
        ),
    }
}

fn failure(code: &str, detail: &str, json_output: bool, exit: u8) -> ExitCode {
    if json_output {
        println!(
            "{}",
            json!({"type":"host_file_command", "ok":false, "error":{"code":code,"detail":detail}})
        );
    } else {
        eprintln!("{code}: {detail}");
    }
    ExitCode::from(exit)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn admission_and_unknown_publication_are_distinct_from_completion() {
        let info = Info {
            target: "desk".into(),
            instance: "a".repeat(32),
            status: local::Status {
                sequence: 3,
                selected: 2,
                phase: Phase::SelectionAccepted,
                ..local::Status::default()
            },
        };
        let admitted = evidence(&info, "send_admitted", None);
        assert_eq!(admitted["published"], 0);
        assert_eq!(admitted["complete"], false);
        assert_eq!(admitted["cleanup"], "unknown");
        assert!(admitted["queued_bytes"].is_null());
        let mut ended = info;
        ended.status.phase = Phase::Ended;
        ended.status.publication_unknown = 1;
        ended.status.published = 1;
        ended.status.cleanup = Cleanup::Confirmed;
        let value = evidence(&ended, "status", None);
        assert_eq!(value["publication_unknown"], 1);
        assert_eq!(value["published"], 1);
        assert_eq!(value["complete"], false);
    }

    #[test]
    fn uncertain_admission_keeps_the_original_instance_for_inspection() {
        let original = "0123456789abcdef0123456789abcdef";
        let value = command_failure_evidence(&CommandFailure {
            error: Error::OutcomeUnknown,
            target: "desk".into(),
            instance: Some(original.into()),
            request: None,
        });
        assert_eq!(value["ok"], false);
        assert_eq!(value["target"], "desk");
        assert_eq!(value["instance"], original);
        assert_eq!(value["error"]["code"], "file_command_outcome_unknown");
        // A command without a reply cannot invent a request ID or publication.
        assert!(value.get("request").is_none());
        assert!(value.get("published").is_none());
    }

    #[test]
    fn pacing_evidence_does_not_claim_sender_acknowledgement_or_cancellation() {
        let mut info = Info {
            target: "desk".into(),
            instance: "a".repeat(32),
            status: local::Status {
                sequence: 7,
                pause_requested: Some(true),
                paused: Some(false),
                ..local::Status::default()
            },
        };
        let pause = evidence(&info, "pause_requested", Some(true));
        assert_eq!(pause["pause_requested"], true);
        assert_eq!(pause["paused"], false);
        assert_eq!(pause["pacing_changed_by_command"], true);
        assert!(pause["cancellation_requested_by_command"].is_null());
        assert_eq!(pause["complete"], false);
        assert_eq!(pause["published"], 0);

        info.status.pause_requested = Some(false);
        info.status.paused = Some(true);
        let resume = evidence(&info, "resume_requested", Some(true));
        assert_eq!(resume["pause_requested"], false);
        assert_eq!(resume["paused"], true);
        info.status.cancel_requested = true;
        let terminal = evidence(&info, "resume_requested", Some(false));
        assert_eq!(terminal["pacing_changed_by_command"], false);
        assert_eq!(terminal["cancel_requested"], true);
        assert!(terminal["cancellation_requested_by_command"].is_null());
        assert!(evidence(&info, "status", None)["pacing_changed_by_command"].is_null());
        assert!(
            evidence(&info, "cancel_requested", Some(true))["pacing_changed_by_command"].is_null()
        );

        info.status.pause_requested = None;
        info.status.paused = None;
        let unknown = evidence(&info, "status", None);
        assert!(unknown["pause_requested"].is_null());
        assert!(unknown["paused"].is_null());
        assert_eq!(observation(None), "unknown");
    }

    #[test]
    fn uncertain_pacing_retains_exact_request_without_inventing_control_or_publication() {
        let value = command_failure_evidence(&CommandFailure {
            error: Error::OutcomeUnknown,
            target: "desk".into(),
            instance: Some("b".repeat(32)),
            request: Some(7),
        });
        assert_eq!(value["instance"], "b".repeat(32));
        assert_eq!(value["request"], 7);
        assert_eq!(value["error"]["code"], "file_command_outcome_unknown");
        for field in [
            "pause_requested",
            "paused",
            "pacing_changed_by_command",
            "published",
        ] {
            assert!(value.get(field).is_none());
        }
    }
}
