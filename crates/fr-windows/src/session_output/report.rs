//! A desktop notice from the original child's completion and cleanup evidence.
//! Retained diagnostic bytes never replace a complete terminal report, and a
//! failed reader, process exit or local log cannot turn into a positive notice.
#![forbid(unsafe_code)]

use super::{CompletionError, STATUS_LINE_LIMIT, child::ChildOutput};
use crate::outcome::{self, Continuation, Summary};

/// Preserve the session's own outcome while reporting independent failures in
/// local output capture, process cleanup and logging.
pub(crate) fn summarize(output: &ChildOutput, machine: &str, log_incomplete: bool) -> Summary {
    let completion = output.stdout.as_ref().map_or(
        Err(CompletionError::Missing),
        super::CompletionCapture::completion,
    );
    let mut summary = match completion {
        Ok(document) => outcome::summarize(document, machine),
        Err(error) => {
            let reason = match error {
                CompletionError::Read(_) => "its completion report could not be read",
                CompletionError::Truncated => "its completion output exceeded the size limit",
                CompletionError::Missing => "no completion report was received",
                CompletionError::InvalidUtf8 | CompletionError::NotDocument => {
                    "its completion output was invalid"
                }
            };
            let mut message =
                format!("The session outcome for {machine} is unknown because {reason}.");
            if let Some(guidance) = guidance(output) {
                message.push_str(" Viewer message: ");
                message.push_str(&guidance);
            }
            Summary {
                ok: false,
                continuation: Continuation::Stop,
                message,
            }
        }
    };
    // A normal exit 1 is part of the viewer's confirmed-refusal contract.
    // Conversely a claimed success followed by exit 1, a crash, or missing
    // status is not proof of original cleanup. Local log writes do not affect
    // custody of the child or the terminal report and cannot force a retry.
    let expected = summary.continuation;
    let exit_matches = expected.accepts_exit(output.status.and_then(|status| status.code()));
    let followed = output.failure.is_none()
        && output.cleanup_error.is_none()
        && output
            .stderr
            .as_ref()
            .is_some_and(|stderr| stderr.report.complete());
    if !followed || !exit_matches {
        summary.continuation = Continuation::Stop;
    }
    summary.ok &= output.complete() && !log_incomplete;

    if output.failure.is_some() {
        summary
            .message
            .push_str(" The viewer's output could not be followed completely.");
    }
    if output.cleanup_error.is_some() {
        summary
            .message
            .push_str(" Local viewer cleanup could not be confirmed.");
    }
    match output.status {
        Some(status) if !status.success() && !exit_matches => match status.code() {
            Some(code) => {
                use std::fmt::Write as _;
                let _ = write!(summary.message, " The viewer exited with code {code}.");
            }
            None => summary
                .message
                .push_str(" The viewer terminated without a normal exit code."),
        },
        None => summary
            .message
            .push_str(" The viewer's process exit could not be confirmed."),
        Some(_) if !exit_matches && expected != Continuation::Stop => summary
            .message
            .push_str(" The viewer's process exit did not match its completion report."),
        Some(_) => {}
    }
    if let Some(stderr) = &output.stderr {
        if stderr.report.truncated_lines != 0 {
            summary
                .message
                .push_str(" Some viewer diagnostics were truncated.");
        }
        if stderr.report.read_error.is_some() {
            summary
                .message
                .push_str(" Viewer diagnostics could not be read completely.");
        }
    } else {
        summary
            .message
            .push_str(" Viewer diagnostics were unavailable.");
    }
    if log_incomplete
        || output
            .stderr
            .as_ref()
            .is_some_and(|stderr| stderr.log_error.is_some())
    {
        summary
            .message
            .push_str(" The local session log could not be saved completely.");
    }
    summary
}

fn guidance(output: &ChildOutput) -> Option<String> {
    let bytes = output.stderr.as_ref()?.first_line.as_deref()?;
    // The reader already bounds this prefix. Keep the same bound at the UI
    // boundary and remove control characters that could hide later warnings.
    let bytes = &bytes[..bytes.len().min(STATUS_LINE_LIMIT)];
    let text: String = String::from_utf8_lossy(bytes)
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    let text = text.trim();
    let text = text.strip_prefix("fr-windows: ").unwrap_or(text).trim();
    (!text.is_empty()).then(|| text.to_owned())
}

#[cfg(all(test, any(unix, windows)))]
#[path = "report/tests.rs"]
mod tests;
