//! `fr connect --control --send PATH...|--file-drop|--file-commands`: local uploads.
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
pub(super) use crate::file_reports::upload::Summary;
use crate::file_reports::upload::absence_code;
use fr_native::{
    desktop::{Desktop, Error as DesktopError},
    viewer_window::{DroppedBatchStatus, FileDropControl},
};
use frd::native_files::{Selection, SendControl, local};
use std::path::PathBuf;

#[cfg(test)]
use crate::file_reports::upload::{refusal_reason, reported, summary};
#[cfg(test)]
use fr_wire::files::Reason;
#[cfg(test)]
use frd::{
    native_files::{Absence, Publication, SendPhase, SendStatus},
    session_startup::{FileSendError, FileSendOutcome},
};

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
/// Omit only file directions that no longer have an explicit local owner.
/// A pre-control retry may retain download consent while command-only upload
/// consent is already spent. Shared file framing survives for that download.
pub(super) fn retain_offer(offer: &mut fr_wire::negotiation::Offer, upload: bool, download: bool) {
    offer
        .capabilities
        .retain(|capability| match capability.name.as_str() {
            fr_wire::attachment::FILES_CAPABILITY | fr_wire::files::CAPABILITY => {
                upload || download
            }
            fr_wire::files::CHANNEL_SCOPE_CAPABILITY => upload,
            fr_wire::files::DOWNLOAD_CAPABILITY => download,
            fr_wire::files::DUPLEX_CAPABILITY => upload && download,
            _ => true,
        });
}

/// The explicit selection and the current attempt's content-free handle.
pub(super) struct Local {
    selection: Option<Selection>,
    sizes: Vec<Option<u64>>,
    attempt: Option<Attempt>,
    file_drop: bool,
    commands: Option<Commands>,
}
struct Commands {
    connection: Option<String>,
    // Spent on the first configuration, even if it failed before a control
    // request. Never reset this when the foreground connection retries viewing.
    outcome: Option<Result<local::Results, local::Error>>,
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
pub(super) fn select(
    paths: &[PathBuf],
    file_drop: bool,
    file_commands: bool,
) -> Result<Option<Local>, Failure> {
    let commands = file_commands.then_some(Commands {
        connection: None,
        outcome: None,
    });
    if paths.is_empty() {
        return Ok((file_drop || file_commands).then(|| Local {
            selection: None,
            sizes: Vec::new(),
            attempt: None,
            file_drop,
            commands,
        }));
    }
    let selection =
        crate::file_reports::upload::select(paths)?.expect("nonempty initial selection");
    let sizes = selection.sizes().collect();
    Ok(Some(Local {
        selection: Some(selection),
        sizes,
        attempt: None,
        file_drop: true,
        commands,
    }))
}

impl Local {
    pub(super) fn can_configure_upload(&self) -> bool {
        self.selection.is_some()
            || self.file_drop
            || self
                .commands
                .as_ref()
                .is_some_and(|commands| commands.outcome.is_none())
    }
    pub(super) fn command_connection(&mut self, connection: &str) {
        if let Some(commands) = &mut self.commands
            && commands.connection.is_none()
            && commands.outcome.is_none()
        {
            commands.connection = Some(connection.to_owned());
        }
    }
    fn configure_commands(&mut self, desktop: &mut Desktop) -> bool {
        let Some(commands) = &mut self.commands else {
            return false;
        };
        if commands.outcome.is_some() {
            return false;
        }
        let outcome = commands
            .connection
            .as_deref()
            .ok_or(local::Error::InvalidInstance)
            .and_then(|connection| desktop.enable_file_commands(connection));
        let enabled = outcome.is_ok();
        commands.outcome = Some(outcome);
        enabled
    }
    fn refuse_commands(&mut self) {
        if let Some(commands) = &mut self.commands
            && commands.outcome.is_none()
        {
            commands.outcome = Some(Err(local::Error::Unavailable));
        }
    }
    /// At each opened attempt, before service. Absence is typed, never fatal:
    /// control proceeds without files.
    pub(super) fn configure(&mut self, desktop: &mut Desktop) {
        if !self.can_configure_upload() {
            // The command-only choice was already consumed. Keep its original
            // results; a later Desktop gets no replacement intake or endpoint.
            return;
        }
        // ready invokes this only before the original control decision is
        // spent. A pre-request startup retry still needs fresh descriptors;
        // later viewing/explicit-control attempts retain the original result.
        let configured = match &self.selection {
            Some(selection) => {
                if let Ok(request) = selection.request() {
                    desktop.configure_file_send_with_intake(request)
                } else {
                    self.attempt = Some(Attempt::Absent("local_selection_unavailable"));
                    self.refuse_commands();
                    return;
                }
            }
            None => desktop.configure_file_drop(),
        };
        self.attempt = Some(match configured {
            Ok((control, _intake)) => {
                let commands = self.configure_commands(desktop);
                let drops = self
                    .file_drop
                    .then(|| desktop.enable_file_drops().ok())
                    .flatten();
                if self.selection.is_none() && drops.is_none() && !commands {
                    // No selected source or reachable explicit intake remains.
                    control.cancel();
                    Attempt::Absent(if self.file_drop {
                        "file_drop_unavailable"
                    } else {
                        "file_commands_unavailable"
                    })
                } else {
                    Attempt::Configured { control, drops }
                }
            }
            Err(error) => {
                self.refuse_commands();
                Attempt::Absent(match error {
                    DesktopError::Files(absence) => absence_code(absence),
                    _ => "files_unavailable",
                })
            }
        });
    }
    pub(super) fn summary(&self) -> Summary {
        let (status, drops) = match &self.attempt {
            None => (Err("files_not_started"), None),
            Some(Attempt::Absent(code)) => (Err(*code), None),
            Some(Attempt::Configured { control, drops }) => (Ok(control.status()), drops.as_ref()),
        };
        let mut out = match &status {
            Ok(status) => Summary::from_status(&self.sizes, status),
            Err(code) => Summary::unavailable(&self.sizes, code),
        };
        if let Some(drops) = drops {
            // The original owner bounds this to eight additional batches,
            // each with at most eight selections. No path is retained here.
            for batch in drops.batches() {
                out.append_dropped(&batch);
            }
        }
        if let Some(commands) = &self.commands {
            out.commands = Some(match &commands.outcome {
                None => Err(crate::file_commands::local_failure(local::Error::Unavailable).code),
                Some(Err(error)) => Err(crate::file_commands::local_failure(*error).code),
                Some(Ok(results)) => {
                    for batch in results.batches() {
                        out.append_command(&batch);
                    }
                    Ok(())
                }
            });
        }
        if out.requested != 0 && out.absence == Some("no_files_selected") {
            out.absence = None;
        }
        out
    }
}
impl Summary {
    fn append_dropped(&mut self, batch: &DroppedBatchStatus) {
        self.append_later(&batch.sizes, batch.status.as_ref().map_err(|code| *code));
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
        assert!(select(&[], false, false).unwrap().is_none());
        let local = select(&[], true, false).unwrap().unwrap();
        assert!(local.selection.is_none());
        assert_eq!(local.sizes, [] as [std::option::Option<u64>; 0]);
        assert_eq!(local.summary().requested, 0);
        assert_eq!(local.summary().sent, [] as [(usize, u64, bool); 0]);
        assert_eq!(
            local.summary().refused,
            [] as [(usize, std::option::Option<u64>, &str); 0]
        );
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
                FileSendOutcome::InterruptedBeforePublication(FileSendError::Transport(
                    fr_transport::quic::Error::SendExpired,
                )),
                "interrupted_send_expired",
            ),
            (
                FileSendOutcome::InterruptedBeforePublication(FileSendError::Expired),
                "interrupted_expired",
            ),
            (
                FileSendOutcome::InterruptedBeforePublication(FileSendError::Transport(
                    fr_transport::quic::Error::Stream(fr_wire::stream::StreamError::Expired),
                )),
                "interrupted_expired",
            ),
            (
                FileSendOutcome::InterruptedBeforePublication(FileSendError::Transport(
                    fr_transport::quic::Error::Expired,
                )),
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
            commands: None,
            command_requests: Vec::new(),
            ..Summary::default()
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
        let failure = select(&[chosen.clone(), alias.clone()], false, false)
            .err()
            .unwrap();
        assert_eq!(failure.code, "send_symlink");
        assert!(failure.next.starts_with("--send #2 is a symbolic link"));
        assert!(!failure.next.contains("chosen") && !failure.next.contains("fr-cli-send"));
        assert!(select(&[], false, false).unwrap().is_none());
        let local = select(std::slice::from_ref(&chosen), false, false)
            .unwrap()
            .unwrap();
        assert_eq!(local.sizes, [Some(1)]);
        assert_eq!(local.summary().absence, Some("files_not_started"));
        let tree = select(std::slice::from_ref(&dir), false, false)
            .unwrap()
            .unwrap();
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
