//! `fr connect --control --send PATH...|--file-drop`: the local upload owner.
//!
//! Every path is classified by type BEFORE any network I/O: symbolic links
//! and special files refuse (typed, by 1-based position, never by name).
//! Regular files and portable directory roots stay open. Before the one control
//! request is spent, each opened attempt hands frd a fresh duplicate; only a
//! granted controller ever reads it. After that request, viewing-only retries
//! skip setup and preserve the original handle/results, so a selection is never
//! resent. Later explicit drops keep that original lane and have separate
//! immutable result handles. The completion record is content-free: indices,
//! local sizes, host-reported outcomes and typed reasons from every selection.
#[path = "files/download.rs"]
pub(super) mod download;
use super::Failure;
use fr_native::{
    desktop::{Desktop, Error as DesktopError},
    viewer_window::{DroppedBatchStatus, FileDropControl},
};
use fr_wire::files::Reason;
use frd::native_files::{
    Absence, Publication, SelectReason, Selection, SendControl, SendPhase, SendStatus,
};
use frd::session_startup::{FileSendError, FileSendOutcome};
use std::{fmt::Write as _, path::PathBuf};

/// The control offer: `--send` adds the three OPTIONAL file boundaries, and
/// with `--clipboard` too, the combined profile carries both optional families
/// on the one controlled session (each lane still attaches only under the
/// granted input attachment; neither consent implies the other).
pub(super) fn offer(clipboard: bool, send: bool) -> fr_wire::negotiation::Offer {
    match (clipboard, send) {
        (true, true) => fr_client::native::control_offer_with_clipboard_and_files(),
        (false, true) => fr_client::native::control_offer_with_files(),
        (clipboard, false) => super::control::offer(clipboard),
    }
}

/// The explicit selection and the current attempt's content-free handle.
pub(super) struct Local {
    selection: Option<Selection>,
    sizes: Vec<Option<u64>>,
    attempt: Option<Attempt>,
}
enum Attempt {
    Configured {
        control: SendControl,
        drops: Option<FileDropControl>,
    },
    Absent(&'static str),
}

/// Deferred drops have no selected path or descriptor before connection.
/// `None` without either opt-in. Refusals name the argument position only.
pub(super) fn select(paths: &[PathBuf], file_drop: bool) -> Result<Option<Local>, Failure> {
    if paths.is_empty() {
        return Ok(file_drop.then(|| Local {
            selection: None,
            sizes: Vec::new(),
            attempt: None,
        }));
    }
    let selection = Selection::select(paths).map_err(|refusal| {
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
    })?;
    let sizes = selection.sizes().collect();
    Ok(Some(Local {
        selection: Some(selection),
        sizes,
        attempt: None,
    }))
}

impl Local {
    /// At each opened attempt, before service. Absence is typed, never fatal:
    /// control proceeds without files.
    pub(super) fn configure(&mut self, desktop: &mut Desktop) {
        // ready invokes this only before the original control decision is
        // spent. A pre-request startup retry still needs fresh descriptors;
        // later viewing/explicit-control attempts retain the original result.
        let configured = match &self.selection {
            Some(selection) => match selection.request() {
                Ok(request) => desktop.configure_file_send_with_intake(request),
                Err(_) => {
                    self.attempt = Some(Attempt::Absent("local_selection_unavailable"));
                    return;
                }
            },
            None => desktop.configure_file_drop(),
        };
        self.attempt = Some(match configured {
            Ok((control, _intake)) => match desktop.enable_file_drops() {
                Ok(drops) => Attempt::Configured {
                    control,
                    drops: Some(drops),
                },
                Err(_) if self.selection.is_none() => {
                    // An explicitly requested drop-only surface must report
                    // absence, not silently leave a permanently empty lane.
                    // Settle the agreed attachment, then stop it independently.
                    control.cancel();
                    Attempt::Absent("file_drop_unavailable")
                }
                Err(_) => Attempt::Configured {
                    control,
                    drops: None,
                },
            },
            Err(DesktopError::Files(absence)) => Attempt::Absent(absence_code(absence)),
            Err(_) => Attempt::Absent("files_unavailable"),
        });
    }
    pub(super) fn summary(&self) -> Summary {
        let (status, drops) = match &self.attempt {
            None => (Err("files_not_started"), None),
            Some(Attempt::Absent(code)) => (Err(*code), None),
            Some(Attempt::Configured { control, drops }) => (Ok(control.status()), drops.as_ref()),
        };
        let mut out = summary(&self.sizes, status.as_ref().map_err(|code| *code));
        if let Some(drops) = drops {
            // The original owner bounds this to eight additional batches,
            // each with at most eight selections. No path is retained here.
            for batch in drops.batches() {
                out.append_dropped(&batch);
            }
        }
        if out.requested != 0 && out.absence == Some("no_files_selected") {
            out.absence = None;
        }
        out
    }
}
fn absence_code(absence: Absence) -> &'static str {
    match absence {
        Absence::NotNegotiated => "host_files_unavailable",
        Absence::WithClipboard => "files_with_clipboard_unsupported",
        Absence::Unavailable => "files_unavailable",
        Absence::Refused(_) => "files_refused_locally",
        Absence::SetupFailed(_) => "files_setup_failed",
        Absence::ReceiveFailed(_) => "files_receive_failed",
    }
}
fn refusal_reason(outcome: FileSendOutcome) -> Option<&'static str> {
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
            FileSendError::Expired => "interrupted_expired",
            FileSendError::Cancelled | FileSendError::Closed => "interrupted_cancelled",
            FileSendError::SourceChanged | FileSendError::Source => "source_changed",
            _ => "interrupted",
        },
    })
}

/// Content-free completion: every requested index appears exactly once.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct Summary {
    pub(super) requested: usize,
    /// (index, host-reported published bytes, durable).
    pub(super) sent: Vec<(usize, u64, bool)>,
    /// (index, known local size, typed reason); unmeasured directory sizes are None.
    pub(super) refused: Vec<(usize, Option<u64>, &'static str)>,
    /// Why no batch produced a report; `None` once any batch has one.
    pub(super) absence: Option<&'static str>,
}
fn summary(sizes: &[Option<u64>], status: Result<&SendStatus, &'static str>) -> Summary {
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
            let absence = match (status.absence, status.phase) {
                (Some(absence), _) => absence_code(absence),
                (None, SendPhase::WaitingForSelection | SendPhase::SelectionAccepted)
                    if sizes.is_empty() =>
                {
                    "no_files_selected"
                }
                (None, SendPhase::WaitingForControl) => "control_not_granted",
                (None, SendPhase::Negotiating) => "files_setup_incomplete",
                (None, _) => "files_not_started",
            };
            out.absence = Some(absence);
            out.refused = (0..sizes.len())
                .map(|index| (index, sizes[index], "not_sent"))
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
    reported(sizes, report.receipts().map(|receipt| receipt.outcome))
}
fn reported(sizes: &[Option<u64>], mut outcomes: impl Iterator<Item = FileSendOutcome>) -> Summary {
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
            // Never started: an earlier file stopped the ordered selection.
            None => out.refused.push((index, size, "not_started")),
        }
    }
    out
}
impl Summary {
    fn append_dropped(&mut self, batch: &DroppedBatchStatus) {
        let mut next = summary(&batch.sizes, batch.status.as_ref().map_err(|code| *code));
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
                *refusal = reason;
            }
        }
        self.append(next);
    }
    fn append(&mut self, next: Self) {
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
    pub(super) fn not_requested() -> Self {
        Self {
            absence: Some("not_requested"),
            ..Self::default()
        }
    }
    /// `,"files_requested":..` for the JSON completion object.
    pub(super) fn json(&self) -> String {
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
        out
    }
    /// One sentence for the text completion.
    pub(super) fn text(&self) -> String {
        if let Some(absence) = self.absence {
            if absence == "not_requested" {
                return String::new();
            }
            if absence == "no_files_selected" {
                return " Files: no files selected.".to_owned();
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

#[cfg(test)]
#[path = "files/repeated_tests.rs"]
mod repeated_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use frd::native_files::SendStatus;
    #[test]
    fn deferred_upload_has_no_initial_selection_or_fabricated_file_result() {
        assert!(select(&[], false).unwrap().is_none());
        let local = select(&[], true).unwrap().unwrap();
        assert!(local.selection.is_none());
        assert!(local.sizes.is_empty());
        assert_eq!(local.summary().requested, 0);
        assert!(local.summary().sent.is_empty());
        assert!(local.summary().refused.is_empty());
        assert_eq!(local.summary().absence, Some("files_not_started"));
        let empty = summary(
            &[],
            Ok(&SendStatus {
                phase: SendPhase::WaitingForSelection,
                absence: None,
                report: None,
            }),
        );
        assert_eq!(empty.text(), " Files: no files selected.");
        let json: serde_json::Value =
            serde_json::from_str(&format!("{{\"outcome\":\"success\"{}}}", empty.json())).unwrap();
        assert_eq!(json["files_requested"], 0);
        assert_eq!(json["files_sent"], serde_json::json!([]));
        assert_eq!(json["files_absence"], "no_files_selected");
        let refused = summary(
            &[],
            Ok(&SendStatus {
                phase: SendPhase::Ended,
                absence: Some(Absence::NotNegotiated),
                report: None,
            }),
        );
        assert_eq!(refused.absence, Some("host_files_unavailable"));
        assert_eq!(refused.requested, 0);
        assert!(refused.sent.is_empty() && refused.refused.is_empty());
    }
    #[test]
    fn absence_accounts_for_every_index_without_names() {
        let sizes = [Some(10), Some(0), Some(7)];
        let absent = summary(
            &sizes,
            Ok(&SendStatus {
                phase: SendPhase::Ended,
                absence: Some(Absence::NotNegotiated),
                report: None,
            }),
        );
        assert_eq!(absent.requested, 3);
        assert_eq!(absent.sent, []);
        assert_eq!(
            absent.refused,
            [
                (0, Some(10), "not_sent"),
                (1, Some(0), "not_sent"),
                (2, Some(7), "not_sent")
            ]
        );
        assert_eq!(absent.absence, Some("host_files_unavailable"));
        let json = format!("{{\"x\":1{}}}", absent.json());
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["files_absence"], "host_files_unavailable");
        assert_eq!(value["files_refused"][2]["bytes"], 7);
        for (phase, code) in [
            (SendPhase::WaitingForControl, "control_not_granted"),
            (SendPhase::Negotiating, "files_setup_incomplete"),
        ] {
            let s = summary(
                &sizes,
                Ok(&SendStatus {
                    phase,
                    absence: None,
                    report: None,
                }),
            );
            assert_eq!(s.absence, Some(code));
        }
        assert_eq!(
            summary(&sizes, Err("files_not_started")).absence,
            Some("files_not_started")
        );
        let none = Summary::not_requested();
        let json = format!("{{\"x\":1{}}}", none.json());
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["files_requested"], 0);
        assert_eq!(value["files_absence"], "not_requested");
        assert_eq!(none.text(), "");
    }
    #[test]
    fn host_outcomes_map_to_typed_reasons() {
        for (outcome, reason) in [
            (
                FileSendOutcome::HostRefused(Reason::Conflict),
                "host_conflict",
            ),
            (FileSendOutcome::HostRefused(Reason::Resource), "host_limit"),
            (
                FileSendOutcome::HostRefused(Reason::Expired),
                "host_expired",
            ),
            (FileSendOutcome::PublicationUnknown, "publication_unknown"),
            (
                FileSendOutcome::InterruptedBeforePublication(FileSendError::Expired),
                "interrupted_expired",
            ),
        ] {
            assert_eq!(refusal_reason(outcome), Some(reason));
        }
        assert_eq!(
            refusal_reason(FileSendOutcome::HostPublished {
                bytes: 1,
                publication: Publication::Durable
            }),
            None
        );
        let text = Summary {
            requested: 2,
            sent: vec![(0, 5, true)],
            refused: vec![(1, Some(3), "host_conflict")],
            absence: None,
        }
        .text();
        assert_eq!(
            text,
            " Files: 1 of 2 published by the host; #2 host_conflict."
        );
    }
    #[test]
    fn a_refused_path_is_typed_by_position_and_never_echoed() {
        let dir = std::env::temp_dir().join(format!("fr-cli-send-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let chosen = dir.join("chosen-name.txt");
        std::fs::write(&chosen, b"x").unwrap();
        let alias = dir.join("private-alias");
        std::os::unix::fs::symlink(&dir, &alias).unwrap();
        let failure = select(&[chosen.clone(), alias.clone()], false)
            .err()
            .unwrap();
        assert_eq!(failure.code, "send_symlink");
        assert!(failure.next.starts_with("--send #2 is a symbolic link"));
        assert!(!failure.next.contains("chosen") && !failure.next.contains("fr-cli-send"));
        assert!(select(&[], false).unwrap().is_none());
        let local = select(std::slice::from_ref(&chosen), false)
            .unwrap()
            .unwrap();
        assert_eq!(local.sizes, [Some(1)]);
        assert_eq!(local.summary().absence, Some("files_not_started"));
        let tree = select(std::slice::from_ref(&dir), false).unwrap().unwrap();
        assert_eq!(tree.sizes, [None]);
        let json = format!("{{{}}}", tree.summary().json().trim_start_matches(','));
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(value["files_refused"][0]["bytes"].is_null());
        assert_eq!(value["files_requested"], 1);
        let _ = std::fs::remove_file(&alias);
        let _ = std::fs::remove_file(&chosen);
        let _ = std::fs::remove_dir(&dir);
    }
}
