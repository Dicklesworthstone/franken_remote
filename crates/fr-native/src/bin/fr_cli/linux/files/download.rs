//! Explicit local download consent and content-free, actual disk outcomes.
//! A pinned directory is selected before network I/O. The original controlled
//! desktop owns the receive worker; this CLI keeps only its bounded status.
use crate::{Failure, FileReceiveReport};
use fr_files::{
    receive::Publication,
    session,
    wire::ResultReceipt,
    worker::{Completion, Error as WorkerError},
};
use fr_native::desktop::{Desktop, Error as DesktopError};
use frd::native_files::{
    Absence, Directory, Limits, ReceiveControl, ReceivePhase, ReceiveRequest, ReceiveStatus,
};
use std::{fmt::Write as _, path::Path};

/// The native owner retains at most 64 terminal receipts and one final result.
/// Enforce the same bound when projecting its status into terminal output.
const MAX_RESULTS: usize = 65;

pub(in crate::linux) struct Local {
    directory: Directory,
    attempt: Option<Attempt>,
}
enum Attempt {
    Configured(ReceiveControl),
    Absent(&'static str),
}

pub(in crate::linux) fn select(destination: Option<&Path>) -> Result<Option<Local>, Failure> {
    let Some(destination) = destination else {
        return Ok(None);
    };
    let directory = Directory::open(destination, Limits::default()).map_err(|reason| {
        Failure::new(
            reason.code(),
            "Choose an existing absolute --receive directory owned by you, not writable by other users or groups, with no symlink components. Nothing was received and no connection was made.",
            2,
        )
    })?;
    Ok(Some(Local {
        directory,
        attempt: None,
    }))
}

impl Local {
    pub(in crate::linux) fn configure(&mut self, desktop: &mut Desktop) {
        // Explicit consent and any outcome belong to the first opened attempt.
        // A later callback must not replace its evidence or reapprove receipt.
        if self.attempt.is_some() {
            return;
        }
        self.attempt = Some(
            match desktop.request_receive_files(ReceiveRequest::new(&self.directory)) {
                Ok(control) => Attempt::Configured(control),
                Err(DesktopError::Files(absence)) => Attempt::Absent(absence_code(absence)),
                Err(_) => Attempt::Absent("files_receive_unavailable"),
            },
        );
    }

    pub(in crate::linux) fn summary(&self) -> Summary {
        match &self.attempt {
            Some(Attempt::Configured(control)) => Summary::from_status(&control.status()),
            Some(Attempt::Absent(code)) => Summary::unavailable(code),
            None => Summary::unavailable("files_receive_not_started"),
        }
    }
}

#[derive(Debug)]
pub(in crate::linux) struct Summary {
    requested: bool,
    received: Vec<(u64, u64, bool)>,
    refused: Vec<(u64, &'static str)>,
    absence: Option<&'static str>,
    ended: Option<&'static str>,
    cleanup: &'static str,
    progress: Option<(u64, u64, u64)>,
    truncated: bool,
    cancelled: bool,
}

impl Summary {
    pub(in crate::linux) fn not_requested() -> Self {
        Self {
            requested: false,
            received: Vec::new(),
            refused: Vec::new(),
            absence: None,
            ended: None,
            cleanup: "not_started",
            progress: None,
            truncated: false,
            cancelled: false,
        }
    }

    fn unavailable(code: &'static str) -> Self {
        Self {
            requested: true,
            absence: Some(code),
            ..Self::not_requested()
        }
    }

    fn from_status(status: &ReceiveStatus) -> Self {
        // A joined owner or any real result proves that receipt began. An
        // ended owner without a pre-start refusal must still be joined; never
        // label its unconfirmed cleanup as finished. A receive failure before
        // the first result may still have started an owner, so missing join
        // evidence stays pending even without a progress sample or receipt.
        let started = status.cleanup.is_some()
            || status.progress.is_some()
            || !status.results.is_empty()
            || status.phase == ReceivePhase::Receiving
            || matches!(status.absence, Some(Absence::ReceiveFailed(_)))
            || (status.phase == ReceivePhase::Ended && status.absence.is_none());
        let mut summary = Self {
            requested: true,
            absence: if started {
                None
            } else {
                status.absence.map(absence_code).or(Some(match status.phase {
                    ReceivePhase::WaitingForControl => "control_not_granted",
                    ReceivePhase::Negotiating => "files_receive_setup_incomplete",
                    ReceivePhase::Receiving | ReceivePhase::Ended => "files_receive_unavailable",
                }))
            },
            ended: if started {
                status.absence.map(absence_code)
            } else {
                None
            },
            cleanup: match status.cleanup {
                Some(Ok(())) => "finished",
                Some(Err(_)) => "failed",
                None if started => "pending",
                None => "not_started",
            },
            progress: status.progress.and_then(|progress| {
                progress
                    .id
                    .checked_sub(1)
                    .map(|index| (index, progress.staged_bytes, progress.total_bytes))
            }),
            truncated: status.results_truncated || status.results.len() > MAX_RESULTS,
            cancelled: status.cancel_requested,
            ..Self::not_requested()
        };
        for receipt in status.results.iter().take(MAX_RESULTS) {
            summary.receipt(*receipt);
        }
        summary
    }

    fn receipt(&mut self, result: ResultReceipt) {
        // Progress is never evidence of publication, including a fully staged
        // object or an empty file. Only the disk worker's Published counts.
        if matches!(result.outcome, Ok(Completion::Begun(_) | Completion::Written(_))) {
            return;
        }
        let Some(index) = result.id.checked_sub(1) else {
            self.ended = Some("receive_invalid_result");
            return;
        };
        match result.outcome {
            Ok(Completion::Published(receipt)) if receipt.id == result.id => {
                self.received.push((
                    index,
                    receipt.bytes,
                    receipt.publication == Publication::Durable,
                ));
            }
            Ok(Completion::Published(_)) => {
                self.refused.push((index, "publication_unknown"));
            }
            Ok(Completion::Cancelled) => self.refused.push((index, "receive_cancelled")),
            Err(error) => self.refused.push((index, worker_code(error))),
            Ok(Completion::Begun(_) | Completion::Written(_)) => {}
        }
    }

    pub(in crate::linux) fn json(&self) -> String {
        let mut out = format!(
            ",\"files_receive_requested\":{},\"files_received\":[",
            self.requested
        );
        for (position, (index, bytes, durable)) in self.received.iter().enumerate() {
            if position != 0 {
                out.push(',');
            }
            let _ = write!(
                out,
                "{{\"index\":{index},\"bytes\":{bytes},\"durable\":{durable}}}"
            );
        }
        out.push_str("],\"files_receive_refused\":[");
        for (position, (index, reason)) in self.refused.iter().enumerate() {
            if position != 0 {
                out.push(',');
            }
            let _ = write!(out, "{{\"index\":{index},\"reason\":\"{reason}\"}}");
        }
        let absence = self.absence.map_or_else(|| "null".to_owned(), crate::output::quoted);
        let ended = self.ended.map_or_else(|| "null".to_owned(), crate::output::quoted);
        let progress = self.progress.map_or_else(
            || "null".to_owned(),
            |(index, staged, total)| format!(
                "{{\"index\":{index},\"staged_bytes\":{staged},\"total_bytes\":{total}}}"
            ),
        );
        let _ = write!(
            out,
            "],\"files_receive_absence\":{absence},\"files_receive_ended\":{ended},\"files_receive_cleanup\":\"{}\",\"files_receive_progress\":{progress},\"files_receive_results_truncated\":{},\"files_receive_cancel_requested\":{}",
            self.cleanup, self.truncated, self.cancelled,
        );
        out
    }

    pub(in crate::linux) fn text(&self) -> String {
        if !self.requested {
            return String::new();
        }
        let mut out = format!(
            " Downloads: {} locally published, {} refused; receive cleanup={}.",
            self.received.len(),
            self.refused.len(),
            self.cleanup,
        );
        for (index, bytes, durable) in &self.received {
            let _ = write!(
                out,
                " Selection #{index}: {bytes} bytes published, durability {}.",
                if *durable { "confirmed" } else { "unknown" },
            );
        }
        for (index, reason) in &self.refused {
            let _ = write!(out, " Selection #{index}: {reason}.");
        }
        if let Some(absence) = self.absence {
            let _ = write!(out, " Receive absence: {absence}.");
        }
        if let Some(ended) = self.ended {
            let _ = write!(out, " Receive ended: {ended}.");
        }
        if let Some((index, staged, total)) = self.progress {
            let _ = write!(
                out,
                " Selection #{index} progress: {staged}/{total} bytes staged (not publication)."
            );
        }
        if self.truncated {
            out.push_str(" Additional result evidence was truncated; do not infer no effect.");
        }
        out
    }

    pub(in crate::linux) fn preserve(&self, failure: &mut Failure) {
        if self.requested && failure.files_receive.is_none() {
            // One terminal report in this short-lived CLI process, bounded by
            // MAX_RESULTS and static reason codes. Keep Failure small and Copy,
            // like the existing one-time indexed selection refusal.
            failure.files_receive = Some(Box::leak(Box::new(FileReceiveReport {
                json: self.json(),
                text: self.text(),
            })));
        }
    }
}

fn absence_code(absence: Absence) -> &'static str {
    match absence {
        Absence::NotNegotiated => "host_download_unavailable",
        Absence::WithClipboard => "files_with_clipboard_unsupported",
        Absence::Unavailable => "files_receive_unavailable",
        Absence::Refused(_) => "files_receive_refused_locally",
        Absence::SetupFailed(_) => "files_receive_setup_failed",
        Absence::ReceiveFailed(error) => match error {
            fr_files::quic::Error::Cancelled => "receive_cancelled",
            fr_files::quic::Error::Limits => "receive_limit",
            fr_files::quic::Error::File(fr_files::wire::Error::Atp(
                fr_files::atp::receive::Error::Worker(error),
            )) => worker_code(error),
            _ => "files_receive_failed",
        },
    }
}

fn worker_code(error: WorkerError) -> &'static str {
    match error {
        WorkerError::UnknownEffect
        | WorkerError::Poisoned
        | WorkerError::Panicked
        | WorkerError::Session(session::Error::UnknownEffect) => "publication_unknown",
        WorkerError::Cancelled
        | WorkerError::Closed
        | WorkerError::Session(session::Error::Closed) => "receive_cancelled",
        WorkerError::Session(session::Error::Storage(fr_files::receive::Error::Conflict)) => {
            "receive_conflict"
        }
        WorkerError::Session(session::Error::Storage(fr_files::receive::Error::Integrity)) => {
            "receive_integrity"
        }
        WorkerError::Session(session::Error::Storage(fr_files::receive::Error::Incomplete)) => {
            "receive_incomplete"
        }
        WorkerError::Session(session::Error::Expired) => "receive_expired",
        WorkerError::Session(session::Error::Permission | session::Error::Authority(_)) => {
            "receive_permission"
        }
        WorkerError::Allocation
        | WorkerError::Session(
            session::Error::Quota
            | session::Error::RateLimited
            | session::Error::Storage(fr_files::receive::Error::Quota),
        ) => "receive_limit",
        _ => "receive_failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(results: Vec<ResultReceipt>) -> ReceiveStatus {
        ReceiveStatus {
            phase: ReceivePhase::Ended,
            absence: None,
            progress: None,
            results,
            results_truncated: false,
            cancel_requested: false,
            cleanup: Some(Ok(())),
        }
    }

    fn published(id: u64, bytes: u64, publication: Publication) -> ResultReceipt {
        ResultReceipt {
            id,
            outcome: Ok(Completion::Published(session::Receipt { id, bytes, publication })),
        }
    }

    fn json(summary: &Summary) -> serde_json::Value {
        serde_json::from_str(&format!("{{\"schema_version\":1{}}}", summary.json())).unwrap()
    }

    #[test]
    fn only_actual_publication_counts_even_for_empty_or_fully_staged_objects() {
        let progress = session::Progress { id: 3, staged_bytes: 17, total_bytes: 17 };
        let state = status(vec![
            published(1, 0, Publication::Durable),
            published(2, 9, Publication::DurabilityUnknown),
            ResultReceipt { id: 3, outcome: Ok(Completion::Begun(progress)) },
            ResultReceipt { id: 3, outcome: Ok(Completion::Written(progress)) },
        ]);
        let report = json(&Summary::from_status(&state));
        assert_eq!(
            report["files_received"],
            serde_json::json!([
                {"index": 0, "bytes": 0, "durable": true},
                {"index": 1, "bytes": 9, "durable": false},
            ])
        );
        assert_eq!(report["files_receive_refused"], serde_json::json!([]));
        assert_eq!(report["files_receive_cleanup"], "finished");
    }

    #[test]
    fn later_conflict_and_failed_cleanup_cannot_erase_a_previous_publication() {
        let mut state = status(vec![
            published(1, 42, Publication::Durable),
            ResultReceipt {
                id: 2,
                outcome: Err(WorkerError::Session(session::Error::Storage(
                    fr_files::receive::Error::Conflict,
                ))),
            },
        ]);
        state.cleanup = Some(Err(WorkerError::UnknownEffect));
        let report = json(&Summary::from_status(&state));
        assert_eq!(report["files_received"][0]["bytes"], 42);
        assert_eq!(
            report["files_receive_refused"],
            serde_json::json!([{"index": 1, "reason": "receive_conflict"}])
        );
        assert!(report["files_receive_absence"].is_null());
        assert_eq!(report["files_receive_cleanup"], "failed");
    }

    #[test]
    fn pending_cleanup_and_unavailable_host_are_distinct_from_joined_cleanup() {
        let mut state = status(Vec::new());
        state.cleanup = None;
        assert_eq!(json(&Summary::from_status(&state))["files_receive_cleanup"], "pending");
        state.absence = Some(Absence::ReceiveFailed(fr_files::quic::Error::Cancelled));
        let report = json(&Summary::from_status(&state));
        assert_eq!(report["files_receive_cleanup"], "pending");
        assert_eq!(report["files_receive_ended"], "receive_cancelled");
        assert_eq!(report["files_received"], serde_json::json!([]));
        state.absence = Some(Absence::NotNegotiated);
        let report = json(&Summary::from_status(&state));
        assert_eq!(report["files_receive_absence"], "host_download_unavailable");
        assert_eq!(report["files_receive_cleanup"], "not_started");
        assert_eq!(report["files_received"], serde_json::json!([]));
        assert!(!json(&Summary::not_requested())["files_receive_requested"].as_bool().unwrap());
    }

    #[test]
    fn invalid_ids_never_wrap_or_claim_a_different_publication() {
        let mut mismatch = published(3, 7, Publication::Durable);
        mismatch.id = 2;
        let state = status(vec![published(0, 4, Publication::Durable), mismatch]);
        let report = json(&Summary::from_status(&state));
        assert_eq!(report["files_received"], serde_json::json!([]));
        assert_eq!(report["files_receive_ended"], "receive_invalid_result");
        assert_eq!(
            report["files_receive_refused"],
            serde_json::json!([{"index": 1, "reason": "publication_unknown"}])
        );
    }

    #[test]
    fn oversized_owner_status_stays_bounded_and_reports_truncation() {
        let state = status((1..=70).map(|id| published(id, id, Publication::Durable)).collect());
        let report = json(&Summary::from_status(&state));
        assert_eq!(report["files_received"].as_array().unwrap().len(), MAX_RESULTS);
        assert_eq!(report["files_receive_results_truncated"], true);
        assert!(Summary::from_status(&state).text().contains("do not infer no effect"));
    }

    #[test]
    fn cancelled_and_host_terminal_reports_retain_local_publications() {
        use fr_core::ids::InputLeaseId;
        use fr_wire::{
            closure::{Cleanup, Closed, ClosedReason, OutstandingEffects},
            lease_revoked::{CleanupStage, EffectStage, Reason, Revoked},
        };
        let summary = Summary::from_status(&status(vec![published(1, 19, Publication::Durable)]));
        let errors = [
            Failure::new("cancelled", "The local session was stopped.", 130),
            crate::terminal::failure(Revoked {
                lease: InputLeaseId::from_raw(10),
                reason: Reason::LeaseExpired,
                cleanup: CleanupStage::Released,
                effects: EffectStage::ReceiptsComplete,
            }),
            crate::closure::failure(Closed {
                reason: ClosedReason::HostStopping,
                cleanup: Cleanup::Complete,
                effects: OutstandingEffects::Unknown,
            }),
        ];
        for mut failure in errors {
            let exit = failure.exit;
            summary.preserve(&mut failure);
            let rendered = crate::terminal::output(failure, true);
            let report: serde_json::Value = serde_json::from_str(&rendered).unwrap();
            assert_eq!(report["files_received"][0]["bytes"], 19);
            assert_eq!(report["files_receive_cleanup"], "finished");
            assert_eq!(failure.exit, exit);
            assert!(crate::terminal::output(failure, false).contains("19 bytes published"));
        }
    }
}
