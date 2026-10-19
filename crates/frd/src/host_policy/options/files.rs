//! Local collision preference carried all the way into the pinned disk owner.
use super::{ConflictPolicy, Error, RunOptions};
use crate::native_files::{Directory, DirectoryRefusal, Limits};

pub(super) fn parse_conflict(value: &str) -> Result<ConflictPolicy, Error> {
    match value {
        "reject" => Ok(ConflictPolicy::Reject),
        "keep-both" => Ok(ConflictPolicy::KeepBoth),
        _ => Err(Error::InvalidArgument),
    }
}
impl RunOptions {
    /// A host selection is consumed once. Never persist it in a restartable
    /// daemon or infer control consent from a source path.
    pub fn validate_file_send(&self) -> Result<(), Error> {
        if self.send.is_empty() {
            return Ok(());
        }
        if !self.once
            || self.input_agent.is_none()
            || self.files.is_some()
            || self.send.len() > crate::native_files::MAX_SENDS
            || self.send.iter().any(|path| {
                !path.is_absolute()
                    || path
                        .components()
                        .any(|part| part == std::path::Component::ParentDir)
            })
        {
            return Err(Error::InvalidArgument);
        }
        Ok(())
    }

    /// Reject orphaned file settings for programmatic callers as well as CLI
    /// input. No spelling can select replacement, merging or automatic resume.
    pub fn file_conflict_policy(&self) -> Result<ConflictPolicy, Error> {
        if self.files.is_none()
            && (self.files_conflict.is_some()
                || self.files_max_file_bytes.is_some()
                || self.files_max_session_bytes.is_some())
        {
            return Err(Error::InvalidArgument);
        }
        Ok(self.files_conflict.unwrap_or_default())
    }

    /// The actual `frd run` setup path, before desktop/worker/network startup.
    /// Selecting keep-both never opens a directory by itself. This creates no
    /// receive permission beyond the existing explicit directory configuration;
    /// the original control attachment and its separate file grant still apply.
    pub fn open_files(&self) -> Result<Option<Directory>, DirectoryRefusal> {
        let conflict = self
            .file_conflict_policy()
            .map_err(|_| DirectoryRefusal::Limits)?;
        let Some(path) = &self.files else {
            return Ok(None);
        };
        let defaults = Limits::default();
        let limits = Limits {
            max_file_bytes: self.files_max_file_bytes.unwrap_or(defaults.max_file_bytes),
            max_session_bytes: self
                .files_max_session_bytes
                .unwrap_or(defaults.max_session_bytes),
        };
        Directory::open(path, limits)
            .map(|directory| Some(directory.with_conflict_policy(conflict)))
    }
}

#[cfg(test)]
#[path = "files_tests.rs"]
mod tests;
