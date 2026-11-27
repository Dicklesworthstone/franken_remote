//! One-use native-app consent for a standing controller-to-host folder job.
//! The ordinary connection parser and source owner remain authoritative.
use super::{Failure, UploadChoices, notice_text};
use crate::options::{self, Command};
use fr_wire::sync_job_state::JobMode;
use std::fmt;

/// An original local choice cannot be copied into another viewer or preference.
#[derive(PartialEq, Eq)]
pub(super) struct Sending {
    arguments: Vec<String>,
    job_id: u64,
    mode: JobMode,
}

impl fmt::Debug for Sending {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Sending").finish_non_exhaustive()
    }
}

impl Sending {
    pub(super) fn parse(arguments: Vec<String>) -> Result<Option<Self>, Failure> {
        if arguments.is_empty() {
            return Ok(None);
        }
        // This validates the path and job scope without opening or scanning it.
        // The selected viewer pins the source before its own network startup.
        let mut validation: Vec<String> = [
            "connect",
            "--choose-host",
            "--control",
            "--experimental-native",
            "--display",
            "only",
        ]
        .map(str::to_owned)
        .into();
        validation.extend(arguments.iter().cloned());
        let Command::Connect(connection) = options::parse(&validation)?.command else {
            return Err(invalid());
        };
        let source = connection.upload.sync_source.ok_or_else(invalid)?;
        Ok(Some(Self {
            arguments,
            job_id: source.job_id,
            mode: source.mode,
        }))
    }

    pub(super) const fn job_id(&self) -> u64 {
        self.job_id
    }

    fn into_session_args(self, mut arguments: Vec<String>) -> Result<Vec<String>, Failure> {
        arguments.extend(self.arguments);
        // Check the actual role, target and all independent choices together.
        // Never upgrade a viewing-only selection into control.
        options::parse(&arguments)?;
        Ok(arguments)
    }
}

fn invalid() -> Failure {
    Failure::new(
        "app_folder_source_invalid",
        "Select one source with --sync-source and --sync-job or --sync-tree-job. Accept its picker notice for that controlling connection without Choose files now.",
        2,
    )
}

pub(super) const fn consent_label() -> &'static str {
    if cfg!(target_os = "macos") {
        "Allow file drops"
    } else {
        "Send files"
    }
}

/// The notice is rebuilt for an unused picker refresh without exposing a path
/// or reading the source. Its consent wording precedes any older outcome.
pub(super) fn notice(pending: Option<&Sending>, previous: Option<&str>) -> Option<String> {
    pending.map(|source| {
        let scope = if source.mode == JobMode::Recursive {
            " with nested trees"
        } else {
            ""
        };
        notice_text(&format!(
            "Upload folder job {}{scope}: choose View and control and {}. This authorizes new source versions throughout this connection; conflicts keep both. File drops stay disabled. Leave it off to decline; omit Choose files now. This source applies once. {}",
            source.job_id,
            consent_label(),
            previous.unwrap_or_default(),
        ))
    })
}

/// Spend a configured source before validation or launch, including decline.
/// The named file-sending control is standing-job consent for this choice; it does
/// not also grant generic drop intake. A later picker starts with no source.
pub(super) fn take_for_choice(
    pending: &mut Option<Sending>,
    uploads: &mut UploadChoices,
) -> Result<Option<Sending>, Failure> {
    let source = pending.take();
    if source.is_none() || !uploads.file_drop {
        return Ok(None);
    }
    if uploads.choose_files {
        return Err(invalid());
    }
    uploads.file_drop = false;
    Ok(source)
}

pub(super) fn apply(
    selected: Option<Sending>,
    arguments: Vec<String>,
) -> Result<Vec<String>, Failure> {
    match selected {
        Some(source) => source.into_session_args(arguments),
        None => Ok(arguments),
    }
}
