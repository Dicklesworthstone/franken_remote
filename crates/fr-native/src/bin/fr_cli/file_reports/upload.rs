//! Shared content-free upload results for native clients.
//!
//! Selection and receipt projection are independent of windowing and local
//! command endpoints. Only actual sender receipts establish publication.
use crate::Failure;
use fr_wire::files::Reason;
#[cfg(any(test, all(target_os = "macos", feature = "macos-desktop")))]
use frd::native_files::SendSnapshot;
use frd::{
    native_files::{
        Absence, Publication, SelectReason, SelectRefusal, Selection, SendPhase, SendStatus,
    },
    session_startup::{FileSendError, FileSendOutcome},
};
use std::{fmt::Write as _, path::PathBuf};

/// Classify and pin explicit sources before network I/O, without reading any
/// contents. Empty means no file choice, never an empty ATP transfer.
pub(crate) fn select(paths: &[PathBuf]) -> Result<Option<Selection>, Failure> {
    if paths.is_empty() {
        return Ok(None);
    }
    Selection::select(paths)
        .map(Some)
        .map_err(selection_failure)
}

/// A bounded, content-free refusal for initial selection or descriptor cloning.
pub(crate) fn selection_failure(refusal: SelectRefusal) -> Failure {
    let what = match refusal.reason {
        SelectReason::TooMany => "is beyond the 8-selection limit",
        SelectReason::Missing => "does not exist",
        SelectReason::Symlink => {
            "is a symbolic link; name the file or directory it points to explicitly"
        }
        SelectReason::Directory => "cannot be selected as a portable directory",
        SelectReason::SpecialFile => {
            "is a device, FIFO or socket; only regular files and directories are sent"
        }
        SelectReason::NotPortable => {
            "has a file name that is not portable UTF-8 (no : \\ < > \" | ? * or control characters, no trailing dot or space, no reserved device name); rename it locally"
        }
        SelectReason::DuplicateName => {
            "has the same file name as an earlier --send; the host keeps one name per selection"
        }
        SelectReason::Unreadable => "cannot be opened for reading",
        SelectReason::Changed => "changed while it was being opened; select it again",
    };
    // One short-lived process reports one refusal; the text carries the
    // position and the typed reason only, never the path or name.
    let next: &'static str = Box::leak(
        format!(
            "--send #{} {what}. Nothing was sent and no connection was made.",
            refusal.index + 1
        )
        .into_boxed_str(),
    );
    Failure::new(refusal.reason.code(), next, 2)
}

pub(crate) fn absence_code(absence: Absence) -> &'static str {
    match absence {
        Absence::NotNegotiated => "host_files_unavailable",
        Absence::WithClipboard => "files_with_clipboard_unsupported",
        Absence::Unavailable => "files_unavailable",
        Absence::Refused(FileSendError::Cancelled) => "files_cancelled",
        Absence::Refused(_) => "files_refused_locally",
        Absence::SetupFailed(_) => "files_setup_failed",
        Absence::ReceiveFailed(_) => "files_receive_failed",
    }
}
pub(crate) fn refusal_reason(outcome: FileSendOutcome) -> Option<&'static str> {
    Some(match outcome {
        FileSendOutcome::HostPublished { .. } | FileSendOutcome::ControllerPublished { .. } => {
            return None;
        }
        FileSendOutcome::HostRefused(reason) => match reason {
            Reason::Conflict => "host_conflict",
            Reason::Resource => "host_limit",
            Reason::Permission => "host_permission",
            Reason::Expired => "host_expired",
            Reason::Integrity => "host_integrity",
            Reason::Invalid => "host_invalid",
            Reason::UnknownEffect => "publication_unknown",
            Reason::Cancelled | Reason::User => "host_cancelled",
            Reason::None => "host_refused",
        },
        FileSendOutcome::ControllerRefused(reason) => match reason {
            Reason::Conflict => "controller_conflict",
            Reason::Resource => "controller_limit",
            Reason::Permission => "controller_permission",
            Reason::Expired => "controller_expired",
            Reason::Integrity => "controller_integrity",
            Reason::Invalid => "controller_invalid",
            Reason::UnknownEffect => "publication_unknown",
            Reason::Cancelled | Reason::User => "controller_cancelled",
            Reason::None => "controller_refused",
        },
        FileSendOutcome::PublicationUnknown => "publication_unknown",
        FileSendOutcome::InterruptedBeforePublication(error) => match error {
            FileSendError::Expired
            | FileSendError::Transport(
                fr_transport::quic::Error::Expired
                | fr_transport::quic::Error::Stream(fr_wire::stream::StreamError::Expired),
            ) => "interrupted_expired",
            FileSendError::Cancelled | FileSendError::Closed => "interrupted_cancelled",
            FileSendError::SourceChanged | FileSendError::Source => "source_changed",
            _ => "interrupted",
        },
    })
}

/// Content-free completion: every requested index appears exactly once.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Summary {
    pub(crate) requested: usize,
    /// (index, host-reported published bytes, durable).
    pub(crate) sent: Vec<(usize, u64, bool)>,
    /// (index, known local size, typed reason); unmeasured directory sizes are None.
    pub(crate) refused: Vec<(usize, Option<u64>, &'static str)>,
    /// Why no batch produced a report; `None` once any batch has one.
    pub(crate) absence: Option<&'static str>,
    /// Original local-command setup only, never present authority/readiness.
    pub(crate) commands: Option<Result<(), &'static str>>,
    /// Exact original request -> range in the combined per-file outcomes.
    /// Drop/command batches can interleave; an aggregate index is not an ID.
    pub(crate) command_requests: Vec<(u64, usize, usize)>,
    /// One original source's cleanup and progress, independent of publication.
    pub(crate) evidence: Option<SourceEvidence>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceEvidence {
    started: Option<usize>,
    complete: Option<bool>,
    cleanup: &'static str,
    cancel_requested: bool,
    /// Transfer ID, bytes queued locally, and measured size (if known).
    progress: Option<(u64, u64, Option<u64>)>,
}

fn uncertain_without_report(status: &SendStatus) -> bool {
    matches!(status.phase, SendPhase::Sending | SendPhase::Finished)
        || matches!(
            (status.phase, status.absence),
            (SendPhase::Ended, None | Some(Absence::ReceiveFailed(_)))
        )
}

pub(crate) fn summary(sizes: &[Option<u64>], status: Result<&SendStatus, &'static str>) -> Summary {
    let mut out = Summary {
        requested: sizes.len(),
        ..Summary::default()
    };
    let report = match status {
        Ok(SendStatus {
            report: Some(report),
            ..
        }) => *report,
        Ok(status) => {
            let uncertain = uncertain_without_report(status);
            let absence = match (status.absence, status.phase) {
                (Some(absence), _) => absence_code(absence),
                (None, SendPhase::WaitingForSelection | SendPhase::SelectionAccepted)
                    if sizes.is_empty() =>
                {
                    "no_files_selected"
                }
                (None, SendPhase::WaitingForControl) => "control_not_granted",
                (None, SendPhase::Negotiating) => "files_setup_incomplete",
                (None, _) if uncertain => "files_results_incomplete",
                (None, _) => "files_not_started",
            };
            out.absence = Some(absence);
            out.refused = (0..sizes.len())
                .map(|index| {
                    (
                        index,
                        sizes[index],
                        if uncertain {
                            "publication_unknown"
                        } else {
                            "not_sent"
                        },
                    )
                })
                .collect();
            return out;
        }
        Err(absence) => {
            out.absence = Some(absence);
            out.refused = (0..sizes.len())
                .map(|index| (index, sizes[index], "not_sent"))
                .collect();
            return out;
        }
    };
    let mut out = reported(sizes, report.receipts().map(|receipt| receipt.outcome));
    account_started(&mut out, report.started);
    out
}

/// A started transfer can precede its receipt. Its effect is not established
/// by an incomplete receipt prefix; only later queued selections never started.
fn account_started(summary: &mut Summary, started: usize) {
    for (index, _, reason) in &mut summary.refused {
        if *index < started && *reason == "not_started" {
            *reason = "publication_unknown";
        }
    }
}
pub(crate) fn reported(
    sizes: &[Option<u64>],
    mut outcomes: impl Iterator<Item = FileSendOutcome>,
) -> Summary {
    let mut out = Summary {
        requested: sizes.len(),
        ..Summary::default()
    };
    for (index, &size) in sizes.iter().enumerate() {
        match outcomes.next() {
            Some(
                FileSendOutcome::HostPublished { bytes, publication }
                | FileSendOutcome::ControllerPublished { bytes, publication },
            ) => {
                out.sent
                    .push((index, bytes, publication == Publication::Durable));
            }
            Some(outcome) => out.refused.push((
                index,
                size,
                refusal_reason(outcome).unwrap_or("host_refused"),
            )),
            // The caller with a real report accounts for its started prefix.
            None => out.refused.push((index, size, "not_started")),
        }
    }
    out
}
impl Summary {
    #[cfg(any(test, all(target_os = "linux", feature = "linux-desktop")))]
    pub(crate) fn append_later(
        &mut self,
        sizes: &[Option<u64>],
        status: Result<&SendStatus, &'static str>,
    ) {
        let mut next = summary(sizes, status);
        if let Some(reason) = next.absence {
            // An admitted successor awaits its original controller turn; it
            // does not request a new control grant. Preserve other typed local
            // selection/refusal reasons per index even after an earlier send.
            let reason = if reason == "control_not_granted" {
                "files_queued"
            } else {
                reason
            };
            for (_, _, refusal) in &mut next.refused {
                if *refusal == "not_sent" {
                    *refusal = reason;
                }
            }
        }
        self.append(next);
    }
    #[cfg(any(test, all(target_os = "linux", feature = "linux-desktop")))]
    pub(crate) fn append(&mut self, next: Self) {
        // A single source's cleanup cannot certify later independent batches.
        self.evidence = None;
        if next.absence.is_none() {
            self.absence = None;
        }
        let offset = self.requested;
        self.requested += next.requested;
        self.sent.extend(
            next.sent
                .into_iter()
                .map(|(index, bytes, durable)| (offset + index, bytes, durable)),
        );
        self.refused.extend(
            next.refused
                .into_iter()
                .map(|(index, bytes, reason)| (offset + index, bytes, reason)),
        );
    }
    pub(crate) fn not_requested() -> Self {
        Self {
            absence: Some("not_requested"),
            ..Self::default()
        }
    }
    /// `,"files_requested":..` for the JSON completion object.
    pub(crate) fn json(&self) -> String {
        let mut out = format!(",\"files_requested\":{},\"files_sent\":[", self.requested);
        for (n, (index, bytes, durable)) in self.sent.iter().enumerate() {
            let comma = if n == 0 { "" } else { "," };
            let _ = write!(
                out,
                "{comma}{{\"index\":{index},\"bytes\":{bytes},\"durable\":{durable}}}"
            );
        }
        out.push_str("],\"files_refused\":[");
        for (n, (index, bytes, reason)) in self.refused.iter().enumerate() {
            let bytes = bytes.map_or_else(|| "null".to_owned(), |n| n.to_string());
            let comma = if n == 0 { "" } else { "," };
            let _ = write!(
                out,
                "{comma}{{\"index\":{index},\"bytes\":{bytes},\"reason\":\"{reason}\"}}"
            );
        }
        let _ = write!(
            out,
            "],\"files_absence\":{}",
            self.absence
                .map_or_else(|| "null".to_owned(), |a| format!("\"{a}\""))
        );
        if let Some(evidence) = &self.evidence {
            let started = evidence
                .started
                .map_or_else(|| "null".to_owned(), |n| n.to_string());
            let complete = evidence
                .complete
                .map_or_else(|| "null".to_owned(), |v| v.to_string());
            let _ = write!(
                out,
                ",\"files_upload_started\":{started},\"files_upload_complete\":{complete},\"files_upload_cleanup\":\"{}\",\"files_upload_cancel_requested\":{},\"files_upload_progress\":",
                evidence.cleanup, evidence.cancel_requested
            );
            if let Some((transfer, queued, total)) = evidence.progress {
                let total = total.map_or_else(|| "null".to_owned(), |n| n.to_string());
                let _ = write!(
                    out,
                    "{{\"transfer_id\":{transfer},\"queued_bytes\":{queued},\"total_bytes\":{total}}}"
                );
            } else {
                out.push_str("null");
            }
        }
        if let Some(commands) = self.commands {
            let absence = commands
                .err()
                .map_or_else(|| "null".to_owned(), |error| format!("\"{error}\""));
            let _ = write!(
                out,
                ",\"files_commands\":{{\"originally_enabled\":{},\"absence\":{absence},\"requests\":[",
                commands.is_ok()
            );
            for (index, (request, first, selected)) in self.command_requests.iter().enumerate() {
                let comma = if index == 0 { "" } else { "," };
                let _ = write!(
                    out,
                    "{comma}{{\"request\":{request},\"first_index\":{first},\"selected\":{selected}}}"
                );
            }
            out.push_str("]}");
        }
        out
    }
    /// One sentence for the text completion.
    pub(crate) fn text(&self) -> String {
        let mut text = self.transfer_text();
        if let Some(evidence) = &self.evidence {
            let _ = write!(text, " Upload source cleanup: {}.", evidence.cleanup);
        }
        if let Some(commands) = self.commands {
            match commands {
                Ok(()) => {
                    text.push_str(" Local file commands belonged to the original upload only.");
                }
                Err(error) => {
                    let _ = write!(text, " Local file commands unavailable: {error}.");
                }
            }
        }
        for (request, first, selected) in &self.command_requests {
            let _ = write!(
                text,
                " File request {request}: #{} through #{} in the combined outcomes.",
                first + 1,
                first + selected
            );
        }
        text
    }
    fn transfer_text(&self) -> String {
        if let Some(absence) = self.absence {
            if absence == "not_requested" {
                return String::new();
            }
            if absence == "no_files_selected" {
                return " Files: no files selected.".to_owned();
            }
            if self
                .refused
                .iter()
                .any(|(_, _, reason)| *reason == "publication_unknown")
            {
                return format!(" Files incomplete: {absence} (publication unconfirmed).");
            }
            return format!(" Files absent: {absence} (nothing sent).");
        }
        let mut text = format!(
            " Files: {} of {} published by the host",
            self.sent.len(),
            self.requested
        );
        for (index, _, reason) in &self.refused {
            let _ = write!(text, "; #{} {reason}", index + 1);
        }
        text.push('.');
        text
    }
}

impl Summary {
    pub(crate) fn from_status(sizes: &[Option<u64>], status: &SendStatus) -> Self {
        summary(sizes, Ok(status))
    }

    /// Keep the original source's retirement independent of its recipient's
    /// publication receipts. Locally queued bytes never count as published.
    #[cfg(any(test, all(target_os = "macos", feature = "macos-desktop")))]
    pub(crate) fn from_snapshot(sizes: &[Option<u64>], snapshot: &SendSnapshot) -> Self {
        let mut out = Self::from_status(sizes, &snapshot.status);
        let cleanup = match snapshot.cleanup {
            Some(Ok(())) => "finished",
            Some(Err(_)) => "failed",
            None if snapshot.stage.is_none()
                && snapshot.current.is_none()
                && snapshot.status.report.is_none()
                && !uncertain_without_report(&snapshot.status) =>
            {
                "not_started"
            }
            None => "pending",
        };
        out.evidence = Some(SourceEvidence {
            started: snapshot.status.report.map(|report| report.started),
            complete: snapshot.status.report.map(|report| report.complete),
            cleanup,
            cancel_requested: snapshot.cancel_requested,
            progress: snapshot
                .current
                .map(|progress| (progress.id, progress.queued_bytes, progress.total_bytes)),
        });
        out
    }

    pub(crate) fn unavailable(sizes: &[Option<u64>], code: &'static str) -> Self {
        summary(sizes, Err(code))
    }

    /// A later session failure never erases the original upload's receipts.
    /// Append to unrelated control history once; a later startup fallback must
    /// not replace already-retained file receipts or cleanup evidence.
    #[cfg(any(test, all(target_os = "macos", feature = "macos-desktop")))]
    pub(crate) fn preserve(&self, failure: &mut Failure) {
        if self.requested == 0
            || failure
                .control_history
                .is_some_and(|history| history.json.contains(",\"files_requested\":"))
        {
            return;
        }
        let (mut json, mut text) = failure.control_history.map_or_else(
            || (String::new(), String::new()),
            |history| (history.json.clone(), history.text.clone()),
        );
        json.push_str(&self.json());
        text.push_str(&self.text());
        failure.control_history = Some(Box::leak(Box::new(crate::ControlHistory { json, text })));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(summary: &Summary) -> serde_json::Value {
        serde_json::from_str(&format!("{{\"schema_version\":1{}}}", summary.json())).unwrap()
    }

    #[test]
    fn started_transfer_without_a_receipt_is_not_reported_as_unstarted() {
        let mut summary = reported(
            &[Some(3), Some(7), None],
            [FileSendOutcome::HostPublished {
                bytes: 3,
                publication: Publication::Durable,
            }]
            .into_iter(),
        );
        account_started(&mut summary, 2);
        assert_eq!(summary.sent, [(0, 3, true)]);
        assert_eq!(
            summary.refused,
            [
                (1, Some(7), "publication_unknown"),
                (2, None, "not_started")
            ]
        );
        assert!(!summary.text().contains("nothing sent"));
    }

    #[test]
    fn snapshot_keeps_queued_bytes_and_source_cleanup_separate_from_publication() {
        let mut snapshot = SendSnapshot {
            status: SendStatus {
                phase: SendPhase::Sending,
                absence: None,
                report: None,
            },
            stage: Some(fr_files::sender::Stage::AwaitingProof),
            current: Some(fr_files::sender::Progress {
                id: 17,
                queued_bytes: 7,
                total_bytes: Some(7),
            }),
            cancel_requested: true,
            pause_requested: false,
            paused: Some(false),
            cleanup: None,
        };
        let summary = Summary::from_snapshot(&[Some(7)], &snapshot);
        assert!(summary.sent.is_empty());
        assert_eq!(summary.refused, [(0, Some(7), "publication_unknown")]);
        assert!(!summary.text().contains("nothing sent"));
        let output = json(&summary);
        assert_eq!(output["files_upload_cleanup"], "pending");
        assert_eq!(output["files_upload_cancel_requested"], true);
        assert_eq!(output["files_upload_progress"]["queued_bytes"], 7);
        assert!(output["files_upload_started"].is_null());
        assert!(output["files_upload_complete"].is_null());
        snapshot.cleanup = Some(Err(FileSendError::Source));
        assert_eq!(
            json(&Summary::from_snapshot(&[Some(7)], &snapshot))["files_upload_cleanup"],
            "failed"
        );
        snapshot.cleanup = Some(Ok(()));
        let cleaned = Summary::from_snapshot(&[Some(7)], &snapshot);
        assert_eq!(json(&cleaned)["files_upload_cleanup"], "finished");
        assert!(cleaned.sent.is_empty());
        snapshot.status.phase = SendPhase::Ended;
        snapshot.stage = None;
        snapshot.current = None;
        snapshot.cleanup = None;
        let ended = Summary::from_snapshot(&[Some(7)], &snapshot);
        assert_eq!(json(&ended)["files_upload_cleanup"], "pending");
        assert_eq!(ended.refused, [(0, Some(7), "publication_unknown")]);
    }

    #[test]
    fn file_evidence_merges_unrelated_history_once_without_fallback_overwrite() {
        let mut failure = Failure::new("native_cleanup_incomplete", "Cleanup pending.", 1);
        failure.control_history = Some(Box::leak(Box::new(crate::ControlHistory {
            json: ",\"original_control_retained\":true".to_owned(),
            text: " Original control retained.".to_owned(),
        })));
        let actual = reported(
            &[Some(19)],
            [FileSendOutcome::HostPublished {
                bytes: 19,
                publication: Publication::Durable,
            }]
            .into_iter(),
        );
        actual.preserve(&mut failure);
        let original = failure.control_history;
        Summary::unavailable(&[Some(19)], "files_not_started").preserve(&mut failure);
        assert_eq!(failure.control_history, original);
        let output: serde_json::Value =
            serde_json::from_str(&crate::terminal::output(failure, true)).unwrap();
        assert_eq!(output["original_control_retained"], true);
        assert_eq!(output["files_sent"][0]["bytes"], 19);
        assert_eq!(failure.exit, 1);
        assert!(crate::terminal::output(failure, false).contains("Original control retained."));
    }

    #[test]
    fn waiting_and_unavailable_selections_preserve_every_original_index() {
        let sizes = [Some(5), None, Some(0)];
        let status = SendStatus {
            phase: SendPhase::WaitingForControl,
            absence: None,
            report: None,
        };
        let waiting = Summary::from_status(&sizes, &status);
        assert_eq!(waiting.requested, 3);
        assert_eq!(waiting.absence, Some("control_not_granted"));
        assert_eq!(
            waiting.refused,
            [
                (0, Some(5), "not_sent"),
                (1, None, "not_sent"),
                (2, Some(0), "not_sent"),
            ]
        );
        assert!(waiting.sent.is_empty());
        let unavailable = Summary::unavailable(&sizes, "host_files_unavailable");
        assert_eq!(unavailable.absence, Some("host_files_unavailable"));
        assert_eq!(unavailable.refused, waiting.refused);
        let none = Summary::not_requested();
        assert!(none.text().is_empty());
        assert_eq!(json(&none)["files_absence"], "not_requested");
        assert!(select(&[]).unwrap().is_none());
    }

    #[test]
    fn only_retained_publication_receipts_count_and_uncertain_effects_remain_named() {
        // Typed receipt fixtures exercise projection only, not an authenticated
        // host, native transfer or filesystem publication.
        let summary = reported(
            &[Some(0), Some(7), None, Some(11)],
            [
                FileSendOutcome::HostPublished {
                    bytes: 0,
                    publication: Publication::DurabilityUnknown,
                },
                FileSendOutcome::HostRefused(Reason::Conflict),
                FileSendOutcome::PublicationUnknown,
                FileSendOutcome::InterruptedBeforePublication(FileSendError::Expired),
            ]
            .into_iter(),
        );
        assert_eq!(summary.sent, [(0, 0, false)]);
        assert_eq!(
            summary.refused,
            [
                (1, Some(7), "host_conflict"),
                (2, None, "publication_unknown"),
                (3, Some(11), "interrupted_expired"),
            ]
        );
        assert!(summary.absence.is_none());
        assert!(!summary.text().contains("nothing sent"));
        let output = json(&summary);
        assert_eq!(output["files_sent"][0]["durable"], false);
        assert!(output["files_refused"][1]["bytes"].is_null());
    }

    #[test]
    fn later_selection_refusal_does_not_erase_previous_publication() {
        let mut summary = reported(
            &[Some(19)],
            [FileSendOutcome::HostPublished {
                bytes: 19,
                publication: Publication::Durable,
            }]
            .into_iter(),
        );
        summary.append_later(&[None, Some(7)], Err("send_symlink"));
        summary.append(Summary::unavailable(&[Some(13)], "files_cancelled"));
        assert_eq!(summary.requested, 4);
        assert_eq!(summary.sent, [(0, 19, true)]);
        assert_eq!(
            summary.refused,
            [
                (1, None, "send_symlink"),
                (2, Some(7), "send_symlink"),
                (3, Some(13), "not_sent"),
            ]
        );
        assert!(summary.absence.is_none());
    }

    #[test]
    fn cancellation_before_setup_is_named_without_inventing_a_publication() {
        assert_eq!(
            absence_code(Absence::Refused(FileSendError::Cancelled)),
            "files_cancelled"
        );
        let cancelled = Summary::unavailable(&[Some(7)], "files_cancelled");
        assert!(cancelled.sent.is_empty());
        assert_eq!(json(&cancelled)["files_refused"][0]["reason"], "not_sent");
        assert_eq!(json(&cancelled)["files_absence"], "files_cancelled");
        let refusal = selection_failure(SelectRefusal {
            index: 2,
            reason: SelectReason::Symlink,
        });
        assert_eq!(refusal.code, "send_symlink");
        assert!(refusal.next.starts_with("--send #3 is a symbolic link"));
    }

    #[test]
    fn terminal_failures_preserve_original_upload_receipts_and_exit_status() {
        use fr_core::ids::InputLeaseId;
        use fr_wire::lease_revoked::{CleanupStage, EffectStage, Reason, Revoked};

        let summary = reported(
            &[Some(19), Some(7)],
            [
                FileSendOutcome::HostPublished {
                    bytes: 19,
                    publication: Publication::Durable,
                },
                FileSendOutcome::PublicationUnknown,
            ]
            .into_iter(),
        );
        let errors = [
            Failure::new("cancelled", "The local session was stopped.", 130),
            Failure::new("native_cleanup_incomplete", "Native cleanup is pending.", 1),
            crate::terminal::failure(Revoked {
                lease: InputLeaseId::from_raw(10),
                reason: Reason::LeaseExpired,
                cleanup: CleanupStage::Released,
                effects: EffectStage::ReceiptsComplete,
            }),
        ];
        for mut failure in errors {
            let exit = failure.exit;
            summary.preserve(&mut failure);
            let output = crate::terminal::output(failure, true);
            let output: serde_json::Value = serde_json::from_str(&output).unwrap();
            assert_eq!(output["files_sent"][0]["bytes"], 19);
            assert_eq!(output["files_refused"][0]["reason"], "publication_unknown");
            assert_eq!(failure.exit, exit);
            assert!(crate::terminal::output(failure, false).contains("1 of 2 published"));
            let original = failure.control_history;
            summary.preserve(&mut failure);
            assert_eq!(failure.control_history, original);
        }
    }
}
