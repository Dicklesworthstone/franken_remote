//! The receive disk owner's terminal evidence is independent of viewer cleanup.
//!
//! The application consumes only the final complete child document. Publication,
//! durability, cancellation, and owner cleanup keep their producer meanings.
//! No destination, file name, or unvalidated producer string reaches notices.
use super::{CompletionCapture, Failure, Report, notice_text};
use crate::FileReceiveReport;
use serde_json::{Value, json};
use std::{collections::BTreeSet, fmt::Write as _};

/// An independent UI parsing bound: the native producer retains at most 64
/// terminal transfer receipts plus one refusal. Keep this boundary portable;
/// default client builds do not expose the platform receive-owner module.
const MAX_RESULTS: usize = 65;

#[derive(Default)]
pub(super) struct Inspection {
    notice: Option<String>,
    evidence: Option<FileReceiveReport>,
}

impl Inspection {
    pub(super) fn preserve(&self, failure: &mut Failure) {
        if failure.files_receive.is_none()
            && let Some(evidence) = &self.evidence
        {
            // A terminal app failure ends this process. Preserve the same
            // bounded receipt document without replacing an existing owner.
            failure.files_receive = Some(Box::leak(Box::new(FileReceiveReport {
                json: evidence.json.clone(),
                text: evidence.text.clone(),
            })));
        }
    }

    pub(super) fn with_session_notice(&self, session: String) -> String {
        match &self.notice {
            // Put publication/durability ahead of the generic session notice:
            // the picker's text bound must not erase the file outcome.
            Some(receive) => notice_text(&format!("{receive} {session}")),
            None => session,
        }
    }
}

pub(super) fn inspect_job(
    stdout: &CompletionCapture,
    expected: bool,
    expected_job: Option<u64>,
    report: Option<&Report>,
) -> Result<Inspection, Failure> {
    let Ok(line) = stdout.completion() else {
        // Failed drainage cannot support a clean end, but a complete final
        // document may still contain real publication receipts. Preserve
        // only that bounded candidate; never certify a clean completion.
        let evidence = stdout
            .retained_document()
            .and_then(|line| serde_json::from_str::<Value>(line).ok())
            .filter(has_metadata)
            .map(|value| {
                Record::parse(&value)
                    .map_or_else(|()| incomplete(&value), |record| record.evidence())
            });
        return if expected || evidence.is_some() {
            Err(unknown(evidence))
        } else {
            Ok(Inspection::default())
        };
    };
    let value = serde_json::from_str::<Value>(line).ok();
    let Some(value) = value else {
        return if expected {
            Err(unknown(None))
        } else {
            Ok(Inspection::default())
        };
    };
    if !has_metadata(&value) {
        // Errors before an owner exists have no receive summary. Actual later
        // failures preserve the original Summary through terminal::output.
        // A successful completion must positively account for requested work.
        return if expected && !pre_start_refusal(&value, report) {
            Err(unknown(None))
        } else {
            Ok(Inspection::default())
        };
    }
    let record = Record::parse(&value).map_err(|()| unknown(Some(incomplete(&value))))?;
    let evidence = record.evidence();
    if record.requested != expected || !record.consistent(expected_job) {
        return Err(unknown(Some(evidence)));
    }
    if matches!(record.cleanup, Cleanup::Pending | Cleanup::Failed) {
        let mut failure = Failure::new(
            "app_receive_cleanup_unconfirmed",
            "The original file receiver has not confirmed disk-worker cleanup. Inspect its retained receive evidence before starting another connection; receiving will not be retried.",
            1,
        )
        .uncertain();
        failure.files_receive = Some(Box::leak(Box::new(evidence)));
        return Err(failure);
    }
    if record.truncated
        || record.folder_job.as_ref().is_some_and(|job| {
            matches!(
                job.failure,
                Some("sync_publication_unknown" | "sync_cleanup_unconfirmed")
            )
        })
        || record.ended == Some("receive_invalid_result")
        || [record.absence, record.ended].contains(&Some("publication_unknown"))
        || record
            .refused
            .iter()
            .any(|receipt| receipt.reason == "publication_unknown")
    {
        return Err(unknown(Some(evidence)));
    }
    Ok(if record.requested {
        Inspection {
            notice: Some(record.notice()),
            evidence: Some(evidence),
        }
    } else {
        Inspection::default()
    })
}

fn has_metadata(value: &Value) -> bool {
    value.as_object().is_some_and(|object| {
        object.keys().any(|key| {
            key == "files_received" || key == "folder_job" || key.starts_with("files_receive_")
        })
    })
}

/// These codes are produced only by local argument/configuration/destination
/// validation, before the native receive owner can start. Generic transport,
/// cancellation, host refusal or cleanup errors cannot use this exception.
fn pre_start_refusal(value: &Value, report: Option<&Report>) -> bool {
    if value.get("outcome").and_then(Value::as_str) != Some("refused")
        || !value.as_object().is_some_and(|object| {
            object.keys().all(|key| {
                matches!(
                    key.as_str(),
                    "schema_version" | "timestamp_unix_ms" | "outcome" | "error"
                )
            })
        })
        || !value
            .get("error")
            .and_then(Value::as_object)
            .is_some_and(|object| {
                object
                    .keys()
                    .all(|key| matches!(key.as_str(), "code" | "next_action"))
            })
    {
        return false;
    }
    matches!(report, Some(Report::Failed { code, unknown_effect: false, .. })
        if matches!(code.as_str(),
            "invalid_arguments" | "receive_requires_control" | "receive_directory_required"
            | "files_sync_job_invalid" | "files_sync_directory_required"
            | "files_sync_keep_both_required"
            | "files_limits_invalid" | "files_directory_relative" | "files_directory_missing"
            | "files_directory_not_directory" | "files_directory_symlink"
            | "files_directory_not_owned" | "files_directory_writable_by_others"
            | "files_directory_unsafe" | "files_directory_unavailable"
            | "invalid_trust_store" | "invalid_native_configuration"
            | "app_unavailable" | "gui_unavailable"))
}

fn unknown(evidence: Option<FileReceiveReport>) -> Failure {
    let mut failure = Failure::new(
        "app_receive_outcome_unknown",
        "The original file-receive outcome is incomplete, contradictory or uncertain. Inspect retained publication and cleanup evidence before starting another connection. Earlier publications are not rolled back or retried.",
        1,
    )
    .uncertain();
    failure.files_receive = evidence.map(|evidence| &*Box::leak(Box::new(evidence)));
    failure
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Cleanup {
    NotStarted,
    Finished,
    Pending,
    Failed,
}
impl Cleanup {
    fn parse(value: Option<&Value>) -> Result<Self, ()> {
        match value.and_then(Value::as_str) {
            Some("not_started") => Ok(Self::NotStarted),
            Some("finished") => Ok(Self::Finished),
            Some("pending") => Ok(Self::Pending),
            Some("failed") => Ok(Self::Failed),
            _ => Err(()),
        }
    }
    const fn code(self) -> &'static str {
        match self {
            Self::NotStarted => "not_started",
            Self::Finished => "finished",
            Self::Pending => "pending",
            Self::Failed => "failed",
        }
    }
}

struct Published {
    index: u64,
    bytes: u64,
    durable: bool,
}
impl Published {
    fn parse(value: &Value) -> Option<Self> {
        Some(Self {
            index: value.get("index")?.as_u64()?,
            bytes: value.get("bytes")?.as_u64()?,
            durable: value.get("durable")?.as_bool()?,
        })
    }
    fn json(&self) -> Value {
        json!({"index": self.index, "bytes": self.bytes, "durable": self.durable})
    }
}

struct Refused {
    index: u64,
    reason: &'static str,
}
impl Refused {
    fn parse(value: &Value) -> Option<Self> {
        let reason = refusal(value.get("reason")?.as_str()?)?;
        Some(Self {
            index: value.get("index")?.as_u64()?,
            reason,
        })
    }
    fn json(&self) -> Value {
        json!({"index": self.index, "reason": self.reason})
    }
}

/// Job observations supplement the publication receipts; they never replace
/// proof of publication or the original disk worker's cleanup result.
struct FolderJob {
    id: u64,
    phase: &'static str,
    revision: u64,
    rounds_settled: u64,
    confirmed_publications: u64,
    confirmed_conflicts: Option<u64>,
    failure: Option<&'static str>,
    cleanup: Cleanup,
}

impl FolderJob {
    fn parse(value: Option<&Value>) -> Result<Option<Self>, ()> {
        let Some(value) = value else {
            return Ok(None);
        };
        let phase = value.get("phase").and_then(Value::as_str).ok_or(())?;
        let phase = [
            "advertised",
            "accepted",
            "scanning",
            "transferring",
            "settled",
            "conflict_recorded",
            "cancelled",
            "failed",
        ]
        .into_iter()
        .find(|known| *known == phase)
        .ok_or(())?;
        let failure = match value.get("failure") {
            Some(Value::Null) => None,
            Some(Value::String(code)) => Some(sync_failure(code).ok_or(())?),
            _ => return Err(()),
        };
        let job = Self {
            id: value.get("id").and_then(Value::as_u64).ok_or(())?,
            phase,
            revision: value.get("revision").and_then(Value::as_u64).ok_or(())?,
            rounds_settled: value
                .get("rounds_settled")
                .and_then(Value::as_u64)
                .ok_or(())?,
            confirmed_publications: value
                .get("confirmed_publications")
                .and_then(Value::as_u64)
                .ok_or(())?,
            confirmed_conflicts: value
                .get("confirmed_conflicts")
                .map(|count| count.as_u64().ok_or(()))
                .transpose()?,
            failure,
            cleanup: Cleanup::parse(value.get("cleanup"))?,
        };
        if job.id == 0
            || job.rounds_settled > job.revision
            || job
                .confirmed_conflicts
                .is_some_and(|n| n > job.confirmed_publications)
            || job.confirmed_publications > 64
            || matches!(job.cleanup, Cleanup::NotStarted)
            || match job.phase {
                "cancelled" => job.failure != Some("sync_cancelled"),
                "failed" => job.failure.is_none() || job.failure == Some("sync_cancelled"),
                _ => job.failure.is_some(),
            }
        {
            return Err(());
        }
        Ok(Some(job))
    }

    fn json(&self) -> Value {
        let mut value = json!({
            "id": self.id,
            "phase": self.phase,
            "revision": self.revision,
            "rounds_settled": self.rounds_settled,
            "confirmed_publications": self.confirmed_publications,
            "failure": self.failure,
            "cleanup": self.cleanup.code(),
        });
        if let Some(count) = self.confirmed_conflicts {
            value["confirmed_conflicts"] = json!(count);
        }
        value
    }
}

fn sync_failure(value: &str) -> Option<&'static str> {
    [
        "sync_invalid_config",
        "sync_keep_both_required",
        "sync_not_negotiated",
        "sync_reconnect_refused",
        "sync_job_mismatch",
        "sync_direction_unsupported",
        "sync_state_refused",
        "sync_clock",
        "sync_expired",
        "sync_cancelled",
        "sync_resource_limit",
        "sync_source_refused",
        "sync_transfer_failed",
        "sync_worker_failed",
        "sync_cleanup_unconfirmed",
        "sync_publication_unknown",
        "sync_durability_unconfirmed",
        "sync_transfer_stopped",
        "sync_peer_failed",
    ]
    .into_iter()
    .find(|known| *known == value)
}

struct Record {
    requested: bool,
    folder_job: Option<FolderJob>,
    received: Vec<Published>,
    refused: Vec<Refused>,
    bytes: u64,
    absence: Option<&'static str>,
    ended: Option<&'static str>,
    cleanup: Cleanup,
    progress: Value,
    truncated: bool,
    cancelled: bool,
}
impl Record {
    fn parse(value: &Value) -> Result<Self, ()> {
        let received = value
            .get("files_received")
            .and_then(Value::as_array)
            .ok_or(())?;
        let refused = value
            .get("files_receive_refused")
            .and_then(Value::as_array)
            .ok_or(())?;
        if received.len().saturating_add(refused.len()) > MAX_RESULTS {
            return Err(());
        }
        let received: Vec<_> = received
            .iter()
            .map(Published::parse)
            .collect::<Option<_>>()
            .ok_or(())?;
        let refused: Vec<_> = refused
            .iter()
            .map(Refused::parse)
            .collect::<Option<_>>()
            .ok_or(())?;
        let mut indices = BTreeSet::new();
        if received
            .iter()
            .map(|entry| entry.index)
            .chain(refused.iter().map(|entry| entry.index))
            .any(|index| index == u64::MAX || !indices.insert(index))
        {
            return Err(());
        }
        let bytes = received
            .iter()
            .try_fold(0_u64, |total, entry| total.checked_add(entry.bytes))
            .ok_or(())?;
        Ok(Self {
            requested: value
                .get("files_receive_requested")
                .and_then(Value::as_bool)
                .ok_or(())?,
            folder_job: FolderJob::parse(value.get("folder_job"))?,
            received,
            refused,
            bytes,
            absence: optional_code(value.get("files_receive_absence"))?,
            ended: optional_code(value.get("files_receive_ended"))?,
            cleanup: Cleanup::parse(value.get("files_receive_cleanup"))?,
            progress: progress(value.get("files_receive_progress"))?,
            truncated: value
                .get("files_receive_results_truncated")
                .and_then(Value::as_bool)
                .ok_or(())?,
            cancelled: value
                .get("files_receive_cancel_requested")
                .and_then(Value::as_bool)
                .ok_or(())?,
        })
    }

    fn consistent(&self, expected_job: Option<u64>) -> bool {
        let no_work = self.received.is_empty()
            && self.refused.is_empty()
            && self.progress.is_null()
            && self.folder_job.is_none();
        match (&self.folder_job, expected_job) {
            (Some(job), Some(expected)) => {
                if job.id != expected || job.cleanup != self.cleanup || !self.requested {
                    return false;
                }
                if matches!(self.cleanup, Cleanup::Finished)
                    && !matches!(job.phase, "cancelled" | "failed")
                {
                    return false;
                }
            }
            (None, Some(_)) => {
                // Attachment can fail before the accepted job owner exists.
                // Only this empty, typed setup result proves that exception.
                let setup_refused = matches!(self.cleanup, Cleanup::Finished)
                    && self.ended == Some("files_receive_setup_failed")
                    && no_work
                    && !self.truncated;
                if !matches!(self.cleanup, Cleanup::NotStarted) && !setup_refused {
                    return false;
                }
            }
            (Some(_), None) => return false,
            _ => {}
        }
        if !self.requested {
            return matches!(self.cleanup, Cleanup::NotStarted)
                && no_work
                && self.absence.is_none()
                && self.ended.is_none()
                && !self.truncated
                && !self.cancelled;
        }
        match self.cleanup {
            Cleanup::NotStarted => {
                no_work && self.absence.is_some() && self.ended.is_none() && !self.truncated
            }
            Cleanup::Finished | Cleanup::Pending | Cleanup::Failed => self.absence.is_none(),
        }
    }

    fn notice(&self) -> String {
        if matches!(self.cleanup, Cleanup::NotStarted) {
            return format!(
                "Receiving did not start ({}).",
                self.absence.unwrap_or("unavailable")
            );
        }
        let durability_unknown = self
            .received
            .iter()
            .filter(|receipt| !receipt.durable)
            .count();
        let publication_unknown = self
            .refused
            .iter()
            .filter(|receipt| receipt.reason == "publication_unknown")
            .count();
        let mut notice = format!(
            "Downloads: {} published ({} bytes), {} refused; {} durability unconfirmed; receive cleanup {}.",
            self.received.len(),
            self.bytes,
            self.refused.len() - publication_unknown,
            durability_unknown,
            self.cleanup.code(),
        );
        if let Some(job) = &self.folder_job {
            let _ = write!(
                notice,
                " Folder job {}: {}, {} rounds settled, {} confirmed publications.",
                job.id, job.phase, job.rounds_settled, job.confirmed_publications,
            );
            if let Some(count) = job.confirmed_conflicts {
                let _ = write!(notice, " Conflict copies kept: {count}.");
            }
            if let Some(failure) = job.failure {
                let _ = write!(notice, " Job ended: {failure}.");
            }
        }
        if publication_unknown != 0 {
            let _ = write!(
                notice,
                " {publication_unknown} publication outcome(s) unknown."
            );
        }
        if self.cancelled {
            notice.push_str(" Receive cancellation was requested.");
        }
        if let Some(ended) = self.ended {
            let _ = write!(notice, " Receive ended: {ended}.");
        }
        if self.truncated {
            notice.push_str(" Additional receipt evidence was truncated.");
        }
        notice
    }

    fn evidence(&self) -> FileReceiveReport {
        let mut value = json!({
            "files_receive_requested": self.requested,
            "files_received": self.received.iter().map(Published::json).collect::<Vec<_>>(),
            "files_receive_refused": self.refused.iter().map(Refused::json).collect::<Vec<_>>(),
            "files_receive_absence": self.absence,
            "files_receive_ended": self.ended,
            "files_receive_cleanup": self.cleanup.code(),
            "files_receive_progress": self.progress,
            "files_receive_results_truncated": self.truncated,
            "files_receive_cancel_requested": self.cancelled,
        });
        if let Some(job) = &self.folder_job {
            value["folder_job"] = job.json();
        }
        evidence(&value, &self.notice())
    }
}

fn evidence(value: &Value, text: &str) -> FileReceiveReport {
    let object = value.to_string();
    FileReceiveReport {
        json: format!(",{}", &object[1..object.len() - 1]),
        text: format!(" {text}"),
    }
}

/// Malformed metadata cannot erase earlier well-typed receipt fragments.
/// This is an explicitly incomplete projection, never fabricated zero effect.
fn incomplete(value: &Value) -> FileReceiveReport {
    let received: Vec<_> = value
        .get("files_received")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(MAX_RESULTS)
        .filter_map(Published::parse)
        .map(|receipt| receipt.json())
        .collect();
    let refused: Vec<_> = value
        .get("files_receive_refused")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(MAX_RESULTS.saturating_sub(received.len()))
        .filter_map(Refused::parse)
        .map(|receipt| receipt.json())
        .collect();
    let mut retained = json!({
        "app_receive_evidence_incomplete": true,
        "files_received": received,
        "files_receive_refused": refused,
    });
    if let Ok(Some(job)) = FolderJob::parse(value.get("folder_job")) {
        retained["folder_job"] = job.json();
    }
    evidence(
        &retained,
        "Original receive evidence is incomplete; well-typed receipt fragments are retained. Do not infer no publication.",
    )
}

fn progress(value: Option<&Value>) -> Result<Value, ()> {
    let value = value.ok_or(())?;
    if value.is_null() {
        return Ok(Value::Null);
    }
    let index = value.get("index").and_then(Value::as_u64).ok_or(())?;
    let staged = value
        .get("staged_bytes")
        .and_then(Value::as_u64)
        .ok_or(())?;
    let total = value.get("total_bytes").and_then(Value::as_u64).ok_or(())?;
    if index == u64::MAX || staged > total {
        return Err(());
    }
    Ok(json!({"index": index, "staged_bytes": staged, "total_bytes": total}))
}

fn optional_code(value: Option<&Value>) -> Result<Option<&'static str>, ()> {
    match value {
        Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => match value.as_str() {
            "files_receive_not_started" => Ok(Some("files_receive_not_started")),
            "control_not_granted" => Ok(Some("control_not_granted")),
            "files_receive_setup_incomplete" => Ok(Some("files_receive_setup_incomplete")),
            "host_download_unavailable" => Ok(Some("host_download_unavailable")),
            "files_with_clipboard_unsupported" => Ok(Some("files_with_clipboard_unsupported")),
            "files_receive_unavailable" => Ok(Some("files_receive_unavailable")),
            "files_receive_refused_locally" => Ok(Some("files_receive_refused_locally")),
            "files_receive_setup_failed" => Ok(Some("files_receive_setup_failed")),
            "files_receive_failed" => Ok(Some("files_receive_failed")),
            "receive_invalid_result" => Ok(Some("receive_invalid_result")),
            "send_expired" => Ok(Some("send_expired")),
            value => refusal(value).map(Some).ok_or(()),
        },
        _ => Err(()),
    }
}

fn refusal(value: &str) -> Option<&'static str> {
    match value {
        "publication_unknown" => Some("publication_unknown"),
        "receive_cancelled" => Some("receive_cancelled"),
        "receive_conflict" => Some("receive_conflict"),
        "receive_integrity" => Some("receive_integrity"),
        "receive_incomplete" => Some("receive_incomplete"),
        "receive_expired" => Some("receive_expired"),
        "receive_permission" => Some("receive_permission"),
        "receive_limit" => Some("receive_limit"),
        "receive_failed" => Some("receive_failed"),
        _ => None,
    }
}

#[cfg(test)]
#[path = "receive_completion_tests.rs"]
mod tests;
