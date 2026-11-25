//! Explicit local download selection and bounded, content-free receive status.
use super::{
    Absence, Directory, HostSendIntake, MAX_SESSION_FILES, SelectRefusal, Selection, SendIntake,
    SendRequest,
};
use asupersync::cx::Cx;
use fr_files::{session::Progress, wire::ResultReceipt, worker};
use std::{
    fmt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};

/// One initial selection or deferred local command surface, shared by startup
/// profiles without duplicating the right to bind it. Taking it consumes the
/// original configuration across all clones; it never follows another session.
#[derive(Clone)]
pub struct HostSelection(Arc<Mutex<Option<SelectionKind>>>);
enum SelectionKind {
    Initial(SendRequest),
    Interactive(HostSendIntake),
    Sync(fr_files::sync::HostConfig),
}
pub(crate) enum HostDownload {
    Initial(SendRequest),
    Interactive(SendIntake),
    Sync(fr_files::sync::HostConfig),
}
impl HostSelection {
    /// One explicit host-side folder job. Merely opening this source neither
    /// scans it nor authorizes a receiver; the original client must separately
    /// accept this exact job ID and the negotiated one-way sync capability.
    pub fn sync(source: &Path, job_id: u64) -> Result<Self, fr_files::sync::LiveError> {
        let selected = SelectionKind::Sync(fr_files::sync::HostConfig::open(source, job_id)?);
        Ok(Self(Arc::new(Mutex::new(Some(selected)))))
    }
    pub fn sync_job_id(&self) -> Option<u64> {
        match self.lock().as_ref() {
            Some(SelectionKind::Sync(config)) => Some(config.job_id()),
            _ => None,
        }
    }
    pub fn select(paths: &[PathBuf]) -> Result<Self, SelectRefusal> {
        let request = Selection::select(paths)?.request()?;
        let selected = SelectionKind::Initial(request);
        Ok(Self(Arc::new(Mutex::new(Some(selected)))))
    }
    /// Prepare one empty local command surface. The original host session must
    /// consume this configuration once and establish its download attachment
    /// before the returned intake accepts any explicit local selection.
    pub fn interactive() -> (Self, HostSendIntake) {
        let intake = HostSendIntake::new();
        (
            Self(Arc::new(Mutex::new(Some(SelectionKind::Interactive(
                intake.clone(),
            ))))),
            intake,
        )
    }
    pub fn is_available(&self) -> bool {
        self.lock().is_some()
    }
    pub(crate) fn take(&self, cx: Cx) -> Result<HostDownload, Absence> {
        match self.lock().take().ok_or(Absence::Unavailable)? {
            SelectionKind::Initial(request) => Ok(HostDownload::Initial(request)),
            SelectionKind::Interactive(intake) => intake.bind(cx).map(HostDownload::Interactive),
            SelectionKind::Sync(config) => Ok(HostDownload::Sync(config)),
        }
    }
    fn lock(&self) -> MutexGuard<'_, Option<SelectionKind>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
impl fmt::Debug for HostSelection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostFileSelection")
            .field("available", &self.is_available())
            .finish_non_exhaustive()
    }
}

/// One explicit local destination, opened before connecting. This carries no
/// control authority; the original live controller supplies that at attachment.
pub struct ReceiveRequest {
    pub(crate) configuration: fr_files::quic::Configuration,
    pub(crate) sync_job: Option<u64>,
}
impl ReceiveRequest {
    pub fn new(directory: &Directory) -> Self {
        let mut configuration = directory.configuration();
        // A host-side folder choice does not opt this receiving role into a job.
        configuration.policy.max_session_transfers = MAX_SESSION_FILES;
        Self {
            configuration,
            sync_job: None,
        }
    }
    /// Explicitly accept one host-configured job, independently of ordinary
    /// file consent. The existing destination must already select `KeepBoth`.
    pub fn with_sync_job(mut self, job_id: u64) -> Result<Self, fr_files::sync::LiveError> {
        if job_id == 0 || self.sync_job.is_some() {
            return Err(fr_files::sync::LiveError::InvalidConfig);
        }
        if self.configuration.directory.conflict_policy() != super::ConflictPolicy::KeepBoth {
            return Err(fr_files::sync::LiveError::KeepBothRequired);
        }
        self.sync_job = Some(job_id);
        self.configuration.policy.max_session_transfers =
            fr_files::sync::live::MAX_SESSION_TRANSFERS;
        Ok(self)
    }
    pub const fn sync_job_id(&self) -> Option<u64> {
        self.sync_job
    }
}
impl fmt::Debug for ReceiveRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FileReceiveRequest([local destination])")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceivePhase {
    WaitingForControl,
    Negotiating,
    Receiving,
    Ended,
}

/// At most the session's admitted transfers plus its first terminal refusal.
/// Hitting this bound requests file-only cancellation without erasing receipts.
pub const MAX_RECEIVE_RESULTS: usize = MAX_SESSION_FILES as usize + 1;
// An explicitly accepted folder owner records its job before its first disk
// result. Keep every bounded publication from that owner, plus one refusal.
const MAX_FOLDER_RECEIVE_RESULTS: usize = fr_files::sync::live::MAX_SESSION_TRANSFERS as usize + 1;

/// One coherent snapshot from the original disk owner. Progress and cleanup
/// never establish publication; only a retained Published result does that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiveStatus {
    /// Latest bounded evidence from this receiver's original standing job.
    pub folder_job: Option<fr_files::sync::JobStatus>,
    pub phase: ReceivePhase,
    pub absence: Option<Absence>,
    pub progress: Option<Progress>,
    pub results: Vec<ResultReceipt>,
    pub results_truncated: bool,
    pub cancel_requested: bool,
    pub cleanup: Option<Result<(), worker::Error>>,
}
impl ReceiveStatus {
    /// Bound for this original receiver's retained terminal results.
    /// Only an explicitly accepted folder owner records a folder job; ordinary
    /// downloads keep their smaller transfer and reporting limits.
    pub const fn result_capacity(&self) -> usize {
        if self.folder_job.is_some() {
            MAX_FOLDER_RECEIVE_RESULTS
        } else {
            MAX_RECEIVE_RESULTS
        }
    }

    /// Whether this evidence requires treating a disk owner as possibly started.
    /// An ended receive failure can precede its first result; missing progress
    /// or receipts cannot prove that cleanup is unnecessary. A pre-start absence
    /// remains distinct unless later progress, results or a join prove otherwise.
    pub fn may_have_started(&self) -> bool {
        self.cleanup.is_some()
            || self.folder_job.is_some()
            || self.progress.is_some()
            || !self.results.is_empty()
            || self.phase == ReceivePhase::Receiving
            || matches!(self.absence, Some(Absence::ReceiveFailed(_)))
            || (self.phase == ReceivePhase::Ended && self.absence.is_none())
    }
}

/// Clones can inspect or stop one receive attempt, never approve another
/// destination, renew authority, retry a transfer or start a disk worker.
#[derive(Clone)]
pub struct ReceiveControl(Arc<Mutex<ReceiveStatus>>);
impl fmt::Debug for ReceiveControl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("FileReceiveControl")
            .field(&self.status())
            .finish()
    }
}
impl ReceiveControl {
    pub(crate) fn new() -> Self {
        Self(Arc::new(Mutex::new(ReceiveStatus {
            folder_job: None,
            phase: ReceivePhase::WaitingForControl,
            absence: None,
            progress: None,
            results: Vec::new(),
            results_truncated: false,
            cancel_requested: false,
            cleanup: None,
        })))
    }
    fn lock(&self) -> MutexGuard<'_, ReceiveStatus> {
        // Only bounded metadata updates run under this lock, never native I/O.
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    pub fn status(&self) -> ReceiveStatus {
        self.lock().clone()
    }
    pub fn cancel(&self) -> bool {
        let mut state = self.lock();
        if state.cancel_requested || state.phase == ReceivePhase::Ended {
            return false;
        }
        state.cancel_requested = true;
        true
    }
    pub(crate) fn cancel_requested(&self) -> bool {
        self.lock().cancel_requested
    }
    pub(crate) fn set_phase(&self, phase: ReceivePhase) {
        let mut state = self.lock();
        if state.phase != ReceivePhase::Ended {
            state.phase = phase;
        }
    }
    pub(crate) fn progress(&self, progress: Option<Progress>) {
        let mut state = self.lock();
        if state.phase != ReceivePhase::Ended {
            state.progress = progress;
        }
    }
    pub(crate) fn observe(&self, result: ResultReceipt) {
        match result.outcome {
            Ok(worker::Completion::Begun(progress) | worker::Completion::Written(progress)) => {
                self.progress(Some(progress));
                return;
            }
            Ok(worker::Completion::Published(_) | worker::Completion::Cancelled) | Err(_) => {}
        }
        let mut state = self.lock();
        state.progress = None;
        if let Some(previous) = state
            .results
            .iter_mut()
            .find(|previous| previous.id == result.id)
        {
            // A later drain can establish a real external effect after an
            // earlier cancellation or uncertain result. Preserve that evidence;
            // no subsequent cleanup failure can erase an actual publication.
            if !matches!(previous.outcome, Ok(worker::Completion::Published(_)))
                && matches!(result.outcome, Ok(worker::Completion::Published(_)))
            {
                *previous = result;
            }
            return;
        }
        let capacity = state.result_capacity();
        if state.results.len() == capacity {
            state.results_truncated = true;
            state.cancel_requested = true;
            return;
        }
        state.results.push(result);
        if state.results.len() == capacity {
            state.cancel_requested = true;
        }
    }
    pub(crate) fn end(&self, absence: Absence) {
        let mut state = self.lock();
        state.phase = ReceivePhase::Ended;
        state.absence.get_or_insert(absence);
        state.progress = None;
    }
    pub(crate) fn record_folder_job(&self, job: fr_files::sync::JobStatus) {
        self.lock().folder_job = Some(job);
    }
    pub(crate) fn cleaned(&self, cleanup: Result<(), worker::Error>) {
        let mut state = self.lock();
        // A later poll may establish a real publication or join the original
        // worker, but cannot repair an already observed cleanup failure.
        // Retain the first failure independently of those stronger receipts.
        if state.cleanup.is_none_or(|previous| previous.is_ok()) {
            state.cleanup = Some(cleanup);
        }
    }
}

#[cfg(test)]
mod tests;
