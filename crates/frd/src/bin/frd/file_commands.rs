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
            "Use frd send --target NAME /absolute/file ...; --status inspects the current owner. History (--status --request N) and --cancel N also require its original --instance HEX.",
            json_output,
            2,
        );
    };
    match command(options) {
        Ok((info, operation, cancellation)) => {
            print_info(&info, operation, cancellation, json_output);
            ExitCode::SUCCESS
        }
        Err(failure) => report_command_error(&failure, json_output),
    }
}

struct CommandFailure {
    error: Error,
    target: String,
    instance: Option<String>,
}

fn command(options: Options) -> Result<(Info, &'static str, Option<bool>), CommandFailure> {
    let instance = match options.instance {
        Some(instance) => instance,
        None => {
            local::inspect(&options.target)
                .map_err(|error| CommandFailure {
                    error,
                    target: options.target.clone(),
                    instance: None,
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
    };
    result.map_err(|error| CommandFailure {
        error,
        target: options.target,
        instance: Some(instance),
    })
}

fn report_command_error(failure: &CommandFailure, json_output: bool) -> ExitCode {
    if json_output {
        println!("{}", command_failure_evidence(failure));
    } else {
        let (code, detail) = error_detail(failure.error);
        eprintln!("{code}: {detail}");
        if let Some(instance) = &failure.instance {
            eprintln!(
                "Inspect the original owner: frd send --target {} --instance {instance} --status",
                failure.target
            );
        }
    }
    ExitCode::from(1)
}

fn command_failure_evidence(failure: &CommandFailure) -> Value {
    let (code, detail) = error_detail(failure.error);
    json!({
        "type": "host_file_command", "ok": false,
        "target": failure.target, "instance": failure.instance,
        "error": {"code": code, "detail": detail}
    })
}

pub(super) fn print_info(
    info: &Info,
    operation: &str,
    cancellation: Option<bool>,
    json_output: bool,
) {
    if json_output {
        println!("{}", evidence(info, operation, cancellation));
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
    } else if let Some(requested) = cancellation {
        println!(
            "Request {} cancellation requested: {requested}. Published files and uncertain effects are retained.",
            status.sequence
        );
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

fn evidence(info: &Info, operation: &str, cancellation: Option<bool>) -> Value {
    let s = &info.status;
    json!({
        "type": "host_file_command", "operation": operation, "ok": true,
        "target": info.target, "instance": info.instance, "request": s.sequence,
        "ready": s.ready, "closed": s.closed, "phase": phase(s.phase), "stage": stage(s.stage),
        "selected": s.selected, "started": s.started, "published": s.published,
        "refused": s.refused, "interrupted": s.interrupted,
        "publication_unknown": s.publication_unknown, "durability_unknown": s.durability_unknown,
        "complete": s.complete, "failed": s.failed, "cancel_requested": s.cancel_requested,
        "cancellation_requested_by_command": cancellation,
        "queued_bytes": s.queued_bytes, "total_bytes": s.total_bytes, "cleanup": cleanup(s.cleanup)
    })
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
            "The local endpoint returned an invalid response.",
        ),
        Error::Timeout => (
            "file_command_timeout",
            "The bounded local command deadline expired.",
        ),
        Error::OutcomeUnknown => (
            "file_command_outcome_unknown",
            "The command may have been admitted before its reply was lost. Inspect the same original instance; do not automatically resend the selection or assume publication was rolled back.",
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
        });
        assert_eq!(value["ok"], false);
        assert_eq!(value["target"], "desk");
        assert_eq!(value["instance"], original);
        assert_eq!(value["error"]["code"], "file_command_outcome_unknown");
        // A command without a reply cannot invent a request ID or publication.
        assert!(value.get("request").is_none());
        assert!(value.get("published").is_none());
    }
}
