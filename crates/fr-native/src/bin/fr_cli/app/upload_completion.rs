//! Preserve the original sender's terminal publication evidence in `fr app`.
//!
//! Progress is never publication. A single source may report its own cleanup;
//! aggregate drop histories deliberately omit that field and keep the original
//! viewer's cleanup obligation. Neither form permits replay or a new selection.
use super::{CompletionCapture, Failure, Report, UploadChoices, notice_text};
use crate::ControlHistory;
use serde_json::{Value, json};
use std::{collections::BTreeSet, fmt::Write as _};

/// Native drops allow eight later batches of eight, independent of the initial picker batch.
/// It never enables the separately bounded local file-command endpoint.
const MAX_RESULTS: usize = 64;
const SOURCE_FIELDS: [&str; 5] = [
    "files_upload_started",
    "files_upload_complete",
    "files_upload_cleanup",
    "files_upload_cancel_requested",
    "files_upload_progress",
];

#[derive(Default)]
pub(super) struct Inspection {
    notice: Option<String>,
    evidence: Option<ControlHistory>,
}

impl Inspection {
    pub(super) fn preserve(&self, failure: &mut Failure) {
        if let Some(evidence) = &self.evidence {
            preserve(evidence, failure);
        }
    }

    pub(super) fn with_session_notice(&self, session: String) -> String {
        match &self.notice {
            Some(upload) => notice_text(&format!("{upload} {session}")),
            None => session,
        }
    }
}

fn preserve(evidence: &ControlHistory, failure: &mut Failure) {
    if failure.control_history.is_some_and(|history| {
        history.json.contains("\"files_requested\":")
            || history.json.contains("\"app_upload_evidence_incomplete\":")
    }) {
        return;
    }
    let (mut json, mut text) = failure.control_history.map_or_else(
        || (String::new(), String::new()),
        |history| (history.json.clone(), history.text.clone()),
    );
    json.push_str(&evidence.json);
    text.push_str(&evidence.text);
    // This terminates the app, matching the ordinary command's bounded
    // retained-history lifetime; no repeated picker accumulates these values.
    failure.control_history = Some(Box::leak(Box::new(ControlHistory { json, text })));
}

pub(super) fn inspect(
    stdout: &CompletionCapture,
    uploads: UploadChoices,
    report: Option<&Report>,
) -> Result<Inspection, Failure> {
    let expected = uploads.requested();
    let limit = match (uploads.choose_files, uploads.file_drop) {
        (true, true) => MAX_RESULTS + 8,
        (true, false) => 8,
        // Keep bounded evidence of unexpected publications even without an opt-in.
        (false, _) => MAX_RESULTS,
    };
    let Ok(line) = stdout.completion() else {
        let evidence = stdout
            .retained_document()
            .and_then(|line| serde_json::from_str::<Value>(line).ok())
            .filter(has_metadata)
            .map(|value| {
                Record::parse(&value, limit)
                    .map_or_else(|()| incomplete(&value, limit), |record| record.evidence())
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
        return if expected && !pre_start_refusal(&value, report, uploads.choose_files) {
            Err(unknown(None))
        } else {
            Ok(Inspection::default())
        };
    }
    let record =
        Record::parse(&value, limit).map_err(|()| unknown(Some(incomplete(&value, limit))))?;
    let evidence = record.evidence();
    if !record.consistent(expected) {
        return Err(unknown(Some(evidence)));
    }
    if record
        .source
        .as_ref()
        .is_some_and(|source| matches!(source.cleanup, "pending" | "failed"))
    {
        let mut failure = Failure::new(
            "app_upload_cleanup_unconfirmed",
            "The original file sender has not confirmed source cleanup. Inspect its retained upload evidence before starting another connection; selected files will not be retried.",
            1,
        )
        .uncertain();
        preserve(&evidence, &mut failure);
        return Err(failure);
    }
    if record.absence == Some("files_results_incomplete")
        || record
            .refused
            .iter()
            .any(|receipt| receipt.reason == "publication_unknown")
    {
        return Err(unknown(Some(evidence)));
    }
    Ok(if expected {
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
            matches!(
                key.as_str(),
                "files_requested"
                    | "files_sent"
                    | "files_refused"
                    | "files_absence"
                    | "files_commands"
            ) || key.starts_with("files_upload_")
        })
    })
}

/// Only exact, local preflight failures establish that no source owner exists.
/// A generic transport/cancellation failure does not prove that nothing was sent.
fn pre_start_refusal(value: &Value, report: Option<&Report>, choose_files: bool) -> bool {
    matches!(
        value.get("outcome").and_then(Value::as_str),
        Some("refused" | "cancelled")
    ) && value.as_object().is_some_and(|object| {
        object.keys().all(|key| {
            matches!(
                key.as_str(),
                "schema_version" | "timestamp_unix_ms" | "outcome" | "error"
            ) || key == "files_received"
                || key.starts_with("files_receive_")
        })
    }) && value
        .get("error")
        .and_then(Value::as_object)
        .is_some_and(|object| {
            object
                .keys()
                .all(|key| matches!(key.as_str(), "code" | "next_action"))
        })
        && matches!(report, Some(Report::Failed { code, unknown_effect: false, .. })
            if matches!(code.as_str(), "invalid_arguments" | "invalid_trust_store"
                | "invalid_native_configuration" | "app_unavailable" | "gui_unavailable"
                | "files_limits_invalid" | "files_directory_relative" | "files_directory_missing"
                | "files_directory_not_directory" | "files_directory_symlink"
                | "files_directory_not_owned" | "files_directory_writable_by_others"
                | "files_directory_unsafe" | "files_directory_unavailable"
                | "receive_directory_choice_cancelled" | "receive_directory_choice_expired"
                | "receive_directory_choice_invalid" | "receive_directory_choice_event_overflow"
                | "receive_directory_picker_unavailable")
                || (choose_files && matches!(code.as_str(),
                    "file_choice_cancelled" | "file_choice_expired" | "file_choice_invalid"
                    | "file_choice_event_overflow" | "file_picker_unavailable"
                    | "send_too_many" | "send_missing" | "send_symlink" | "send_directory"
                    | "send_special_file" | "send_name_not_portable" | "send_duplicate_name"
                    | "send_unreadable" | "send_changed")))
}

fn unknown(evidence: Option<ControlHistory>) -> Failure {
    let mut failure = Failure::new(
        "app_upload_outcome_unknown",
        "The original upload outcome is incomplete, contradictory or uncertain. Inspect retained host-publication evidence before starting another connection. Published files are not rolled back or retried.",
        1,
    )
    .uncertain();
    if let Some(evidence) = evidence {
        preserve(&evidence, &mut failure);
    }
    failure
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
        json!({"index":self.index,"bytes":self.bytes,"durable":self.durable})
    }
}

struct Refused {
    index: u64,
    bytes: Option<u64>,
    reason: &'static str,
}
impl Refused {
    fn parse(value: &Value) -> Option<Self> {
        let bytes = optional_number(value.get("bytes")).ok()?;
        Some(Self {
            index: value.get("index")?.as_u64()?,
            bytes,
            reason: refusal_code(value.get("reason")?.as_str()?)?,
        })
    }
    fn json(&self) -> Value {
        json!({"index":self.index,"bytes":self.bytes,"reason":self.reason})
    }
}

struct Source {
    started: Option<u64>,
    complete: Option<bool>,
    cleanup: &'static str,
    cancelled: bool,
    progress: Value,
}
impl Source {
    fn parse(value: &Value) -> Result<Option<Self>, ()> {
        if !SOURCE_FIELDS.iter().any(|field| value.get(field).is_some()) {
            return Ok(None);
        }
        let started = optional_number(value.get("files_upload_started"))?;
        let complete = match value.get("files_upload_complete") {
            Some(Value::Null) => None,
            Some(Value::Bool(value)) => Some(*value),
            _ => return Err(()),
        };
        if started.is_some() != complete.is_some() {
            return Err(());
        }
        let cleanup = match value.get("files_upload_cleanup").and_then(Value::as_str) {
            Some("not_started") => "not_started",
            Some("finished") => "finished",
            Some("pending") => "pending",
            Some("failed") => "failed",
            _ => return Err(()),
        };
        let progress = progress(value.get("files_upload_progress"))?;
        if cleanup == "not_started" && (started.is_some() || !progress.is_null()) {
            return Err(());
        }
        Ok(Some(Self {
            started,
            complete,
            cleanup,
            cancelled: value
                .get("files_upload_cancel_requested")
                .and_then(Value::as_bool)
                .ok_or(())?,
            progress,
        }))
    }
    fn append(&self, value: &mut Value) {
        let object = value.as_object_mut().expect("local evidence object");
        object.insert("files_upload_started".into(), json!(self.started));
        object.insert("files_upload_complete".into(), json!(self.complete));
        object.insert("files_upload_cleanup".into(), json!(self.cleanup));
        object.insert(
            "files_upload_cancel_requested".into(),
            json!(self.cancelled),
        );
        object.insert("files_upload_progress".into(), self.progress.clone());
    }
}

struct Record {
    requested: u64,
    sent: Vec<Published>,
    refused: Vec<Refused>,
    absence: Option<&'static str>,
    source: Option<Source>,
}
impl Record {
    fn parse(value: &Value, limit: usize) -> Result<Self, ()> {
        // fr app has no file-command endpoint. Do not silently project its
        // unrelated request identities into a native-drop completion.
        if value.get("files_commands").is_some()
            || value.as_object().is_some_and(|object| {
                object.keys().any(|key| {
                    key.starts_with("files_upload_") && !SOURCE_FIELDS.contains(&key.as_str())
                })
            })
        {
            return Err(());
        }
        let requested = value
            .get("files_requested")
            .and_then(Value::as_u64)
            .ok_or(())?;
        let sent = value
            .get("files_sent")
            .and_then(Value::as_array)
            .ok_or(())?;
        let refused = value
            .get("files_refused")
            .and_then(Value::as_array)
            .ok_or(())?;
        if requested > limit as u64 || sent.len().saturating_add(refused.len()) > limit {
            return Err(());
        }
        let sent = sent
            .iter()
            .map(Published::parse)
            .collect::<Option<Vec<_>>>()
            .ok_or(())?;
        let refused = refused
            .iter()
            .map(Refused::parse)
            .collect::<Option<Vec<_>>>()
            .ok_or(())?;
        let absence = match value.get("files_absence") {
            Some(Value::Null) => None,
            Some(Value::String(value)) => Some(code(value).ok_or(())?),
            _ => return Err(()),
        };
        Ok(Self {
            requested,
            sent,
            refused,
            absence,
            source: Source::parse(value)?,
        })
    }

    fn consistent(&self, expected: bool) -> bool {
        let indices: BTreeSet<_> = self
            .sent
            .iter()
            .map(|item| item.index)
            .chain(self.refused.iter().map(|item| item.index))
            .collect();
        if indices.len() != self.sent.len() + self.refused.len()
            || indices.len() as u64 != self.requested
            || indices.iter().copied().ne(0..self.requested)
            || self
                .sent
                .iter()
                .try_fold(0_u64, |sum, item| sum.checked_add(item.bytes))
                .is_none()
        {
            return false;
        }
        if !expected {
            return self.requested == 0
                && self.absence == Some("not_requested")
                && self.source.is_none();
        }
        if self.absence == Some("not_requested")
            || (self.absence == Some("no_files_selected") && self.requested != 0)
            || (self.absence.is_some() && !self.sent.is_empty())
        {
            return false;
        }
        self.source.as_ref().is_none_or(|source| {
            (source.started.is_some() || self.sent.is_empty())
                && source.started.is_none_or(|started| {
                    // A stopped batch clears its remaining selections before
                    // successful cleanup marks it complete. That does not mean
                    // every selection started, or erase the unstarted suffix.
                    started <= self.requested && self.sent.iter().all(|item| item.index < started)
                })
                && (source.cleanup != "not_started" || self.sent.is_empty())
        })
    }

    fn notice(&self) -> String {
        if self.requested == 0 {
            return match self.absence {
                Some("no_files_selected" | "not_requested") | None => {
                    "Uploads: no files selected.".into()
                }
                Some(reason) => format!("Uploads did not start ({reason})."),
            };
        }
        let unknown = self
            .refused
            .iter()
            .filter(|item| item.reason == "publication_unknown")
            .count();
        let uncertain_durability = self.sent.iter().filter(|item| !item.durable).count();
        let bytes = self
            .sent
            .iter()
            .try_fold(0_u64, |sum, item| sum.checked_add(item.bytes));
        let bytes = bytes.map_or_else(
            || "unconfirmed byte count".into(),
            |bytes| format!("{bytes} bytes"),
        );
        let mut notice = format!(
            "Uploads: {} of {} published by host ({bytes}); {} not published; {uncertain_durability} durability unconfirmed.",
            self.sent.len(),
            self.requested,
            self.refused.len() - unknown,
        );
        if unknown != 0 {
            let _ = write!(notice, " {unknown} upload publication outcome(s) unknown.");
        }
        if let Some(source) = &self.source {
            let _ = write!(notice, " Upload source cleanup {}.", source.cleanup);
            if source.cancelled {
                notice.push_str(" Upload cancellation requested.");
            }
        }
        notice
    }

    fn evidence(&self) -> ControlHistory {
        let mut value = json!({
            "files_requested":self.requested,
            "files_sent":self.sent.iter().map(Published::json).collect::<Vec<_>>(),
            "files_refused":self.refused.iter().map(Refused::json).collect::<Vec<_>>(),
            "files_absence":self.absence,
        });
        if let Some(source) = &self.source {
            source.append(&mut value);
        }
        evidence(&value, &self.notice())
    }
}

fn evidence(value: &Value, notice: &str) -> ControlHistory {
    let object = value.to_string();
    ControlHistory {
        json: format!(",{}", &object[1..object.len() - 1]),
        text: format!(" {notice}"),
    }
}

fn incomplete(value: &Value, limit: usize) -> ControlHistory {
    let sent: Vec<_> = value
        .get("files_sent")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(limit)
        .filter_map(Published::parse)
        .map(|receipt| receipt.json())
        .collect();
    let refused: Vec<_> = value
        .get("files_refused")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(limit.saturating_sub(sent.len()))
        .filter_map(Refused::parse)
        .map(|receipt| receipt.json())
        .collect();
    evidence(
        &json!({
            "app_upload_evidence_incomplete":true,"files_sent":sent,"files_refused":refused,
        }),
        "Original upload evidence is incomplete; well-typed receipt fragments are retained. Do not infer no publication.",
    )
}

fn optional_number(value: Option<&Value>) -> Result<Option<u64>, ()> {
    match value {
        Some(Value::Null) => Ok(None),
        Some(value) => value.as_u64().map(Some).ok_or(()),
        None => Err(()),
    }
}

fn progress(value: Option<&Value>) -> Result<Value, ()> {
    let value = value.ok_or(())?;
    if value.is_null() {
        return Ok(Value::Null);
    }
    let transfer = value
        .get("transfer_id")
        .and_then(Value::as_u64)
        .filter(|id| *id != 0)
        .ok_or(())?;
    let queued = value
        .get("queued_bytes")
        .and_then(Value::as_u64)
        .ok_or(())?;
    let total = optional_number(value.get("total_bytes"))?;
    if total.is_some_and(|total| queued > total) {
        return Err(());
    }
    Ok(json!({"transfer_id":transfer,"queued_bytes":queued,"total_bytes":total}))
}

/// Copy only known content-free producer codes into retained evidence.
fn code(value: &str) -> Option<&'static str> {
    const CODES: &[&str] = &[
        "not_requested",
        "no_files_selected",
        "not_sent",
        "not_started",
        "control_not_granted",
        "host_files_unavailable",
        "files_with_clipboard_unsupported",
        "files_unavailable",
        "files_cancelled",
        "files_send_expired",
        "files_refused_locally",
        "files_setup_failed",
        "files_receive_failed",
        "files_setup_incomplete",
        "files_results_incomplete",
        "files_not_started",
        "files_queued",
        "local_selection_unavailable",
        "file_drop_unavailable",
        "file_selection_expired",
        "file_selection_stopped",
        "file_selection_unavailable",
        "file_selection_preparing",
        "send_too_many",
        "send_missing",
        "send_symlink",
        "send_directory",
        "send_special_file",
        "send_name_not_portable",
        "send_duplicate_name",
        "send_unreadable",
        "send_changed",
        "host_conflict",
        "host_limit",
        "host_permission",
        "host_expired",
        "host_integrity",
        "host_invalid",
        "host_cancelled",
        "host_refused",
        "controller_conflict",
        "controller_limit",
        "controller_permission",
        "controller_expired",
        "controller_integrity",
        "controller_invalid",
        "controller_cancelled",
        "controller_refused",
        "publication_unknown",
        "interrupted_send_expired",
        "interrupted_expired",
        "interrupted_cancelled",
        "source_changed",
        "interrupted",
    ];
    CODES.iter().copied().find(|code| *code == value)
}

fn refusal_code(value: &str) -> Option<&'static str> {
    code(value).filter(|code| !matches!(*code, "not_requested" | "no_files_selected"))
}

#[cfg(test)]
#[path = "upload_completion_tests.rs"]
mod tests;
