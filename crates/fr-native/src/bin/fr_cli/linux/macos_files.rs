//! Explicit Mac file selection and terminal evidence. This module never owns
//! a second transfer, replays a selection or infers publication from progress.
use super::Failure;
use crate::{
    file_reports::{download, upload},
    options::Connection,
};
use fr_native::macos::files::{Files, Status};

pub(super) fn select(
    connection: &Connection,
    chosen: Option<&[std::path::PathBuf]>,
) -> Result<Files, Failure> {
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
    let download = connection
        .receive
        .as_ref()
        .map(crate::options::ReceiveOptions::open)
        .transpose()?;
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
