//! One original controller-side source for a host-configured upload job.
use fr_wire::sync_job_state::JobMode;
use super::Absence;
use fr_files::{
    sender,
    sync::{LiveError, SourceConfig},
};
use std::{
    fmt,
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
};

/// Local source consent consumed once across all clones. Opening it never
/// reads contents, grants control, or supplies the host's destination path.
#[derive(Clone)]
pub struct UploadSelection(Arc<Mutex<Option<SourceConfig>>>);
impl UploadSelection {
    pub fn folder_job(source: &Path, job_id: u64) -> Result<Self, LiveError> {
        Self::folder_job_mode(source, job_id, JobMode::Flat)
    }

    pub fn folder_job_mode(source: &Path, job_id: u64, mode: JobMode) -> Result<Self, LiveError> {
        let config = SourceConfig::open(source, job_id)?.with_mode(mode);
        Ok(Self(Arc::new(Mutex::new(Some(config)))))
    }
    pub fn sync_job_id(&self) -> Option<u64> {
        self.lock().as_ref().map(SourceConfig::job_id)
    }
    pub fn sync_job_mode(&self) -> Option<JobMode> {
        self.lock().as_ref().map(SourceConfig::mode)
    }
    pub fn is_available(&self) -> bool {
        self.lock().is_some()
    }
    pub(crate) fn take(&self) -> Result<SourceConfig, Absence> {
        self.lock().take().ok_or(Absence::Unavailable)
    }
    fn lock(&self) -> MutexGuard<'_, Option<SourceConfig>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
impl fmt::Debug for UploadSelection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FolderUploadSelection")
            .field("available", &self.is_available())
            .finish_non_exhaustive()
    }
}

pub(crate) fn sync_send_error(error: LiveError) -> sender::Error {
    match error {
        LiveError::Send(error) => error,
        LiveError::Transport(error) => sender::Error::Transport(error),
        LiveError::Wire(error) => sender::Error::Wire(error),
        LiveError::Clock => sender::Error::Clock,
        LiveError::Expired => sender::Error::Expired,
        LiveError::Cancelled => sender::Error::Cancelled,
        LiveError::WrongConnection => sender::Error::WrongConnection,
        LiveError::NotNegotiated | LiveError::WrongDirection => sender::Error::WrongRole,
        LiveError::InvalidConfig | LiveError::Resource => sender::Error::Limits,
        LiveError::Source(_) => sender::Error::SourceChanged,
        LiveError::Worker | LiveError::Panicked | LiveError::Cleanup => sender::Error::Worker,
        _ => sender::Error::Protocol,
    }
}

#[cfg(all(test, unix))]
mod tests;
