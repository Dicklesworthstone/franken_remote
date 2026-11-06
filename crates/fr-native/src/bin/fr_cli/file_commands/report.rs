//! Content-free evidence: local admission, remote receipts and cleanup differ.
use super::{
    Output,
    options::{Action, Options},
};
use frd::native_files::local::{Cleanup, Error, Info, Phase, SelectionFailure, Stage};
use serde_json::{Value, json};

pub(super) fn success(
    command: &Options,
    info: &Info,
    operation: &str,
    changed: Option<bool>,
) -> Output {
    let s = &info.status;
    let cancellation = (operation == "cancel_requested")
        .then_some(changed)
        .flatten();
    let pacing = matches!(operation, "pause_requested" | "resume_requested")
        .then_some(changed)
        .flatten();
    let text = if command.json {
        json!({
            "schema_version": 1, "outcome": "success",
            "data": {
                "scope": "local_upload", "operation": operation,
                "target": command.target, "connection_id": command.connection,
                "lease": info.instance, "request": s.sequence,
                "ready": s.ready, "closed": s.closed,
                "phase": phase(s.phase), "stage": stage(s.stage),
                "selected": s.selected, "started": s.started,
                "published": s.published, "refused": s.refused, "interrupted": s.interrupted,
                "publication_unknown": s.publication_unknown, "durability_unknown": s.durability_unknown,
                "complete": s.complete, "failed": s.failed,
                "cancel_requested": s.cancel_requested,
                "pause_requested": s.pause_requested, "paused": s.paused,
                "pacing_changed_by_command": pacing,
                "cancellation_requested_by_command": cancellation,
                "queued_bytes": s.queued_bytes, "total_bytes": s.total_bytes,
                "cleanup": cleanup(s.cleanup), "new_connection_opened": false
            }
        }).to_string()
    } else {
        let operation = match (operation, changed) {
            ("send_admitted", _) => {
                "Selection admitted locally; publication is not yet confirmed.".to_owned()
            }
            ("cancel_requested", Some(requested)) => format!(
                "Cancellation requested: {requested}. Earlier publication and unknown effects remain."
            ),
            ("pause_requested", Some(changed)) => format!(
                "Pause intent changed: {changed}. The original transfer deadline continues."
            ),
            ("resume_requested", Some(changed)) => {
                format!("Resume intent changed: {changed}. No transfer is retried or reopened.")
            }
            _ => "Status of the original local upload owner.".to_owned(),
        };
        format!(
            "{operation}\nTarget: {}\nConnection ID: {}\nFile lease: {}\nRequest: {}; selected: {}; phase: {}; stage: {}.\nReady for another selection: {}; closed: {}.\nPause requested: {}; sender last observed paused: {}. These are separate observations.\nPublished: {}; refused: {}; interrupted: {}; publication unknown: {}; durability unknown: {}.\nComplete: {}; failed: {}; source cleanup: {}. Queued bytes do not prove publication.\n",
            command.target,
            command.connection,
            info.instance,
            s.sequence,
            s.selected,
            phase(s.phase),
            stage(s.stage),
            s.ready,
            s.closed,
            observation(s.pause_requested),
            observation(s.paused),
            s.published,
            s.refused,
            s.interrupted,
            s.publication_unknown,
            s.durability_unknown,
            s.complete,
            s.failed,
            cleanup(s.cleanup),
        )
    };
    Output { text, exit: 0 }
}

fn observation(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "true",
        Some(false) => "false",
        None => "unknown",
    }
}

pub(super) fn failure(command: &Options, error: Error) -> Output {
    let (code, detail) = error_detail(error);
    let unknown = error == Error::OutcomeUnknown;
    let request = match command.action {
        Action::Status(sequence) => sequence,
        Action::Cancel(sequence) | Action::Pause(sequence) | Action::Resume(sequence) => {
            Some(sequence)
        }
        Action::Send(_) => None,
    };
    let selection = match error {
        Error::Selection { index, reason } => json!({
            "index": usize::from(index) + 1, "reason": selection_reason(reason),
        }),
        _ => Value::Null,
    };
    let text = if command.json {
        json!({
            "schema_version": 1,
            "outcome": if unknown { "unknown_external_effect" } else { "refused" },
            "error": { "code": code, "detail": detail, "selection": selection },
            "data": {
                "scope": "local_upload", "target": command.target,
                "connection_id": command.connection, "lease": command.lease,
                "request": request, "new_connection_opened": false,
            }
        })
        .to_string()
    } else {
        let selection = match error {
            Error::Selection { index, reason } => format!(
                " Selection #{}: {}.",
                usize::from(index) + 1,
                selection_reason(reason),
            ),
            _ => String::new(),
        };
        let lease = command.lease.as_deref().unwrap_or("not discovered");
        format!(
            "{code}: {detail}{selection}\nTarget: {}\nOriginal connection ID: {}\nOriginal file lease: {lease}\n",
            command.target, command.connection
        )
    };
    Output { text, exit: 1 }
}

pub(super) fn error_detail(error: Error) -> (&'static str, &'static str) {
    match error {
        Error::InvalidTarget => (
            "file_command_invalid_target",
            "Use the original foreground connection ID from fr robot session status NODE_ID.",
        ),
        Error::InvalidInstance => (
            "file_command_invalid_lease",
            "Use the original 32-digit file lease from fr send NODE_ID --connection-id ID --status.",
        ),
        Error::InvalidPaths => (
            "file_command_invalid_paths",
            "Select up to eight absolute files or portable directory roots without parent traversal.",
        ),
        Error::InvalidRequest => (
            "file_command_invalid_request",
            "Use a nonzero retained request number from this original file lease.",
        ),
        Error::AlreadyRunning => (
            "file_command_already_running",
            "A local file command owner already occupies this original connection endpoint.",
        ),
        Error::NoSession => (
            "file_commands_unavailable",
            "The original upload endpoint is absent. It must have been explicitly enabled with connect --control --file-commands; reconnect never restores it.",
        ),
        Error::WrongInstance => (
            "file_command_lease_changed",
            "The original file lease has ended. No command was sent to a replacement owner.",
        ),
        Error::UntrustedPeer => (
            "file_command_untrusted_peer",
            "The local endpoint is not owned by the same effective user.",
        ),
        Error::Busy => (
            "file_command_busy",
            "The original intake is not ready. Wait for its control grant, file attachment and previous transfer/source cleanup; a paused or cancelled lane is not bypassed.",
        ),
        Error::Closed => (
            "file_command_closed",
            "The original upload intake is closed. Selections are never carried into another session.",
        ),
        Error::Exhausted => (
            "file_command_exhausted",
            "This original owner has reached its bounded command history or cumulative transfer allowance.",
        ),
        Error::UnknownRequest => (
            "file_command_unknown_request",
            "This original file lease has no retained request with that number.",
        ),
        Error::Selection { .. } => (
            "file_selection_refused",
            "The selected source was refused; check its type, permissions, portable name and limits.",
        ),
        Error::Unavailable => (
            "file_command_unavailable",
            "The original local endpoint could not complete this operation.",
        ),
        Error::Protocol => (
            "file_command_protocol",
            "The original local endpoint returned invalid metadata.",
        ),
        Error::Timeout => (
            "file_command_timeout",
            "The original bounded local command deadline expired.",
        ),
        Error::OutcomeUnknown => (
            "file_command_outcome_unknown",
            "The original command may have been admitted before its reply was lost. Inspect the SAME connection and file lease; never automatically resend or infer rollback.",
        ),
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
fn selection_reason(value: SelectionFailure) -> &'static str {
    match value {
        SelectionFailure::TooMany => "too_many",
        SelectionFailure::Missing => "missing",
        SelectionFailure::Symlink => "symlink",
        SelectionFailure::Directory => "directory",
        SelectionFailure::SpecialFile => "special_file",
        SelectionFailure::NotPortable => "not_portable",
        SelectionFailure::DuplicateName => "duplicate_name",
        SelectionFailure::Unreadable => "unreadable",
        SelectionFailure::Changed => "changed",
    }
}

#[cfg(test)]
#[path = "report/tests.rs"]
mod tests;
