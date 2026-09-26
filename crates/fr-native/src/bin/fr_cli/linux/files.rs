//! `fr connect --control --send PATH...`: the local half of the drop lane.
//!
//! Every path is classified by type BEFORE any network I/O: symbolic links,
//! directories and special files refuse (typed, by 1-based position, never by
//! name). Accepted files stay open; each opened attempt hands frd a fresh
//! duplicate of the same descriptors, and only an attempt that is granted
//! control ever reads one (an attempt that asked for control ends
//! reconnection, so a selection is never resent). The completion record is
//! content-free: indices, local sizes, host-reported outcomes, typed reasons.
use super::Failure;
use fr_native::desktop::{Desktop, Error as DesktopError};
use fr_wire::files::Reason;
use frd::native_files::{
    Absence, Publication, SelectReason, Selection, SendControl, SendPhase, SendStatus,
};
use frd::session_startup::{FileSendError, FileSendOutcome};
use std::{fmt::Write as _, path::PathBuf};

/// The control offer: `--send` adds the three OPTIONAL file boundaries (the
/// command line refuses `--send` together with `--clipboard`).
pub(super) fn offer(clipboard: bool, send: bool) -> fr_wire::negotiation::Offer {
    if send {
        fr_client::native::control_offer_with_files()
    } else {
        super::control::offer(clipboard)
    }
}

/// The explicit selection and the current attempt's content-free handle.
pub(super) struct Local {
    selection: Selection,
    sizes: Vec<u64>,
    attempt: Option<Attempt>,
}
enum Attempt {
    Configured(SendControl),
    Absent(&'static str),
}

/// `None` without `--send`. Refusals name the argument position only.
pub(super) fn select(paths: &[PathBuf]) -> Result<Option<Local>, Failure> {
    if paths.is_empty() {
        return Ok(None);
    }
    let selection = Selection::select(paths).map_err(|refusal| {
        let what = match refusal.reason {
            SelectReason::TooMany => "is beyond the 8-file limit",
            SelectReason::Missing => "does not exist",
            SelectReason::Symlink => {
                "is a symbolic link; name the regular file it points to explicitly"
            }
            SelectReason::Directory => "is a directory; only regular files are sent",
            SelectReason::SpecialFile => {
                "is not a regular file (device, FIFO or socket); only regular files are sent"
            }
            SelectReason::NotPortable => {
                "has a file name that is not portable UTF-8 (no : \\ < > \" | ? * or control characters, no trailing dot or space, no reserved device name); rename it locally"
            }
            SelectReason::DuplicateName => {
                "has the same file name as an earlier --send; the host keeps one name per file"
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
        selection,
        sizes,
        attempt: None,
    }))
}

impl Local {
    /// At each opened attempt, before service. Absence is typed, never fatal:
    /// control proceeds without files.
    pub(super) fn configure(&mut self, desktop: &mut Desktop) {
        self.attempt = Some(match self.selection.request() {
            Err(_) => Attempt::Absent("local_selection_unavailable"),
            Ok(request) => match desktop.configure_file_send(request) {
                Ok(control) => Attempt::Configured(control),
                Err(DesktopError::Files(absence)) => Attempt::Absent(absence_code(absence)),
                Err(_) => Attempt::Absent("files_unavailable"),
            },
        });
    }
    pub(super) fn summary(&self) -> Summary {
        let status = match &self.attempt {
            None => Err("files_not_started"),
            Some(Attempt::Absent(code)) => Err(*code),
            Some(Attempt::Configured(control)) => Ok(control.status()),
        };
        summary(&self.sizes, status.as_ref().map_err(|code| *code))
    }
}
fn absence_code(absence: Absence) -> &'static str {
    match absence {
        Absence::NotNegotiated => "host_files_unavailable",
        Absence::WithClipboard => "files_with_clipboard_unsupported",
        Absence::Unavailable => "files_unavailable",
        Absence::Refused(_) => "files_refused_locally",
        Absence::SetupFailed(_) => "files_setup_failed",
    }
}
fn refusal_reason(outcome: FileSendOutcome) -> Option<&'static str> {
    Some(match outcome {
        FileSendOutcome::HostPublished { .. } => return None,
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
    /// (index, local size, typed reason).
    pub(super) refused: Vec<(usize, u64, &'static str)>,
    /// Why no lane carried the selection; `None` once the batch started.
    pub(super) absence: Option<&'static str>,
}
fn summary(sizes: &[u64], status: Result<&SendStatus, &'static str>) -> Summary {
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
    let mut receipts = report.receipts();
    for (index, &size) in sizes.iter().enumerate() {
        match receipts.next().map(|receipt| receipt.outcome) {
            Some(FileSendOutcome::HostPublished { bytes, publication }) => {
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
mod tests {
    use super::*;
    use frd::native_files::SendStatus;
    #[test]
    fn absence_accounts_for_every_index_without_names() {
        let sizes = [10, 0, 7];
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
            [(0, 10, "not_sent"), (1, 0, "not_sent"), (2, 7, "not_sent")]
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
            refused: vec![(1, 3, "host_conflict")],
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
        let failure = select(&[chosen.clone(), dir.clone()]).err().unwrap();
        assert_eq!(failure.code, "send_directory");
        assert!(failure.next.starts_with("--send #2 is a directory"));
        assert!(!failure.next.contains("chosen") && !failure.next.contains("fr-cli-send"));
        assert!(select(&[]).unwrap().is_none());
        let local = select(std::slice::from_ref(&chosen)).unwrap().unwrap();
        assert_eq!(local.sizes, [1]);
        assert_eq!(local.summary().absence, Some("files_not_started"));
        let _ = std::fs::remove_file(&chosen);
        let _ = std::fs::remove_dir(&dir);
    }
}
