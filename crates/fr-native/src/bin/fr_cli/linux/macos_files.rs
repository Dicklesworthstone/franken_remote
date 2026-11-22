//! Explicit Mac file selection and terminal evidence. This module never owns
//! a second transfer, replays a selection or infers publication from progress.
use super::Failure;
use crate::{
    file_reports::{download, upload},
    options::{Connection, ReceiveOptions},
};
use fr_native::macos::files::{Files, Status};
use frd::native_files::Directory;
use std::path::Path;

/// Open the original destination immediately after its native choice, before
/// another chooser can delay pinning it. No disk worker or transfer starts here.
pub(super) fn receive(
    connection: &Connection,
    chosen: Option<&Path>,
) -> Result<Option<Directory>, Failure> {
    if connection.choose_receive_directory != chosen.is_some()
        || (chosen.is_some() && connection.receive.is_some())
    {
        return Err(Failure::new(
            "receive_directory_choice_invalid",
            "The original local destination choice is missing or ambiguous. Choose one destination for a new controlling connection.",
            2,
        ));
    }
    let chosen = chosen
        .map(|path| ReceiveOptions::chosen_directory(path.to_owned()))
        .transpose()?;
    chosen
        .as_ref()
        .or(connection.receive.as_ref())
        .map(ReceiveOptions::open)
        .transpose()
}

/// This fallback applies only before the original Files owner is constructed.
/// It never replaces actual receipts or later cleanup evidence.
pub(super) fn preserve_not_started(connection: &Connection, failure: &mut Failure) {
    if connection.receive.is_some() || connection.choose_receive_directory {
        download::Summary::unavailable("files_receive_not_started").preserve(failure);
    }
}

pub(super) fn select(
    connection: &Connection,
    chosen: Option<&[std::path::PathBuf]>,
    download: Option<Directory>,
) -> Result<Files, Failure> {
    if (connection.receive.is_some() || connection.choose_receive_directory) != download.is_some() {
        return Err(Failure::new(
            "receive_directory_choice_invalid",
            "The original receive destination was not pinned. Start a new connection with one local destination choice.",
            2,
        ));
    }
    // A native choice is never a fallback for explicit paths, or vice versa.
    if connection.upload.choose_files != chosen.is_some()
        || chosen.is_some_and(|paths| paths.is_empty() || !connection.upload.paths.is_empty())
    {
        return Err(Failure::new(
            "file_choice_invalid",
            "The original local file choice is missing or ambiguous. Start a new connection with one explicit selection.",
            2,
        ));
    }
    let upload = upload::select(chosen.unwrap_or(&connection.upload.paths))?;
    let files = Files::new(upload, download).map_err(upload::selection_failure)?;
    let files = if connection.upload.file_commands {
        files.with_commands()
    } else {
        files
    };
    Ok(if connection.upload.file_drop {
        files.with_file_drop()
    } else {
        files
    })
}

/// Copied once from the original owners after their cleanup turn. A fallback
/// taken before startup only fills an absent error slot; it cannot replace
/// later actual receipts or the original host closure/revocation report.
pub(super) struct Summary {
    upload: upload::Summary,
    download: download::Summary,
}
impl Summary {
    pub(super) fn capture(files: &Files) -> Self {
        let sizes = files.upload_sizes();
        let mut upload = match files.upload_status() {
            None => upload::Summary::not_requested(),
            Some(Status::NotStarted) => upload::Summary::unavailable(sizes, "files_not_started"),
            Some(Status::Unavailable(absence)) => {
                upload::Summary::unavailable(sizes, upload::absence_code(absence))
            }
            Some(Status::Configured(snapshot)) => upload::Summary::from_snapshot(sizes, &snapshot),
        };
        for batch in files.dropped_batches() {
            upload.append_later(&batch.sizes, batch.status.as_ref().map_err(|code| *code));
        }
        upload.commands = files.command_results().map(|result| match result {
            Err(error) => Err(crate::file_commands::local_failure(error).code),
            Ok(results) => {
                for batch in results.batches() {
                    upload.append_command(&batch);
                }
                Ok(())
            }
        });
        if upload.requested != 0 && upload.absence == Some("no_files_selected") {
            upload.absence = None;
        }
        let download = match files.download_status() {
            None => download::Summary::not_requested(),
            Some(Status::NotStarted) => download::Summary::unavailable("files_receive_not_started"),
            Some(Status::Unavailable(absence)) => {
                download::Summary::unavailable(download::absence_code(absence))
            }
            Some(Status::Configured(status)) => download::Summary::from_status(&status),
        };
        Self { upload, download }
    }

    pub(super) fn preserve(&self, failure: &mut Failure) {
        self.upload.preserve(failure);
        self.download.preserve(failure);
    }

    pub(super) fn finish(
        &self,
        result: Result<String, Failure>,
        json: bool,
    ) -> Result<String, Failure> {
        match result {
            Err(mut failure) => {
                self.preserve(&mut failure);
                Err(failure)
            }
            Ok(text) if json => {
                // The existing session formatter supplies one complete JSON
                // object. Add bounded typed file fields before its final brace
                // without rebuilding or dropping its host terminal evidence.
                let Some(prefix) = text.trim_end().strip_suffix('}') else {
                    let mut failure = Failure::new(
                        "native_output_invalid",
                        "The session completion could not be formatted; retained file results remain available in this error.",
                        1,
                    );
                    self.preserve(&mut failure);
                    return Err(failure);
                };
                Ok(format!(
                    "{prefix}{}{}}}\n",
                    self.upload.json(),
                    self.download.json()
                ))
            }
            Ok(mut text) => {
                for evidence in [self.upload.text(), self.download.text()] {
                    if !evidence.is_empty() {
                        text.push_str(evidence.trim_start());
                        text.push('\n');
                    }
                }
                Ok(text)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fr_wire::closure::{Cleanup, Closed, ClosedReason, OutstandingEffects};

    fn connection(extra: &str) -> Connection {
        let args = format!("connect nHOST --control --experimental-native --display only {extra}");
        let crate::options::Command::Connect(connection) = crate::options::parse(
            &args
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>(),
        )
        .unwrap()
        .command
        else {
            panic!("ordinary connection");
        };
        connection
    }

    #[test]
    fn receive_chooser_and_pinned_destination_must_match_the_original_request() {
        let plain = connection("");
        assert!(receive(&plain, None).unwrap().is_none());
        assert_eq!(
            receive(&plain, Some(Path::new("/private/unrequested")))
                .err()
                .unwrap()
                .code,
            "receive_directory_choice_invalid"
        );
        let mut chosen = connection("--choose-receive-directory");
        assert_eq!(
            receive(&chosen, None).err().unwrap().code,
            "receive_directory_choice_invalid"
        );
        assert_eq!(
            select(&chosen, None, None).err().unwrap().code,
            "receive_directory_choice_invalid"
        );
        chosen.receive = connection("--receive /private/original").receive;
        assert_eq!(
            receive(&chosen, Some(Path::new("/private/ambiguous")))
                .err()
                .unwrap()
                .code,
            "receive_directory_choice_invalid"
        );
    }

    #[test]
    fn preflight_failure_preserves_known_nonstart_without_replacing_existing_evidence() {
        let chosen = connection("--choose-receive-directory");
        let mut failure = Failure::new(
            "receive_directory_choice_cancelled",
            "No connection was made.",
            2,
        );
        preserve_not_started(&chosen, &mut failure);
        let retained = failure.files_receive;
        let value: serde_json::Value =
            serde_json::from_str(&crate::terminal::output(failure, true)).unwrap();
        assert_eq!(value["files_receive_requested"], true);
        assert_eq!(value["files_receive_cleanup"], "not_started");
        assert_eq!(value["files_received"], serde_json::json!([]));
        preserve_not_started(&chosen, &mut failure);
        assert_eq!(failure.files_receive, retained);
        let mut plain = Failure::new("local_refusal", "No connection was made.", 2);
        preserve_not_started(&connection(""), &mut plain);
        assert!(plain.files_receive.is_none());
    }

    fn absent() -> Summary {
        Summary {
            upload: upload::Summary::unavailable(&[Some(7)], "host_files_unavailable"),
            download: download::Summary::unavailable("host_download_unavailable"),
        }
    }

    #[test]
    fn file_evidence_does_not_replace_existing_success_terminal_fields() {
        let original = "{\"outcome\":\"success\",\"close_exchange\":{\"cleanup\":\"complete\",\"effects\":\"unknown\"},\"lease_revocation\":null}\n".to_owned();
        let merged = absent().finish(Ok(original), true).unwrap();
        let value: serde_json::Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(value["close_exchange"]["effects"], "unknown");
        assert!(value["lease_revocation"].is_null());
        assert_eq!(value["files_requested"], 1);
        assert_eq!(value["files_absence"], "host_files_unavailable");
        assert_eq!(value["files_receive_absence"], "host_download_unavailable");
        assert_eq!(value["files_receive_cleanup"], "not_started");
    }

    #[test]
    fn startup_fallback_cannot_replace_retained_terminal_or_later_file_evidence() {
        let mut original = crate::closure::host_closed(Closed {
            reason: ClosedReason::HostStopping,
            cleanup: Cleanup::Complete,
            effects: OutstandingEffects::Unknown,
        })
        .unwrap();
        let closure = original.closure;
        let exit = original.exit;
        absent().preserve(&mut original);
        let control_history = original.control_history;
        let receive_history = original.files_receive;
        let fallback = Summary {
            upload: upload::Summary::unavailable(&[Some(7)], "files_not_started"),
            download: download::Summary::unavailable("files_receive_not_started"),
        };
        let error = fallback.finish(Err(original), true).err().unwrap();
        assert_eq!(error.closure, closure);
        assert_eq!(error.exit, exit);
        assert_eq!(error.control_history, control_history);
        assert_eq!(error.files_receive, receive_history);
    }
}
