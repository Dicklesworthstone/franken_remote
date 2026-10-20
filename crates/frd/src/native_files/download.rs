//! Explicit local download selection and bounded, content-free receive status.
use super::{Absence, Directory, MAX_SESSION_FILES, SelectRefusal, Selection, SendRequest};
use fr_files::{session::Progress, wire::ResultReceipt, worker};
use std::{
    fmt,
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
};

/// One operator selection, shared by startup profiles without duplicating the
/// right to send it. Taking it consumes the original request across all clones.
#[derive(Clone)]
pub struct HostSelection(Arc<Mutex<Option<SendRequest>>>);
impl HostSelection {
    pub fn select(paths: &[PathBuf]) -> Result<Self, SelectRefusal> {
        let request = Selection::select(paths)?.request()?;
        Ok(Self(Arc::new(Mutex::new(Some(request)))))
    }
    pub fn is_available(&self) -> bool {
        self.lock().is_some()
    }
    pub(crate) fn take_request(&self) -> Result<SendRequest, Absence> {
        self.lock().take().ok_or(Absence::Unavailable)
    }
    fn lock(&self) -> MutexGuard<'_, Option<SendRequest>> {
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
}
impl ReceiveRequest {
    pub fn new(directory: &Directory) -> Self {
        Self {
            configuration: directory.configuration(),
        }
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

/// One coherent snapshot from the original disk owner. Progress and cleanup
/// never establish publication; only a retained Published result does that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiveStatus {
    pub phase: ReceivePhase,
    pub absence: Option<Absence>,
    pub progress: Option<Progress>,
    pub results: Vec<ResultReceipt>,
    pub results_truncated: bool,
    pub cancel_requested: bool,
    pub cleanup: Option<Result<(), worker::Error>>,
}
impl ReceiveStatus {
    /// Whether this evidence requires treating a disk owner as possibly started.
    /// An ended receive failure can precede its first result; missing progress
    /// or receipts cannot prove that cleanup is unnecessary. A pre-start absence
    /// remains distinct unless later progress, results or a join prove otherwise.
    pub fn may_have_started(&self) -> bool {
        self.cleanup.is_some()
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
        if state.results.len() == MAX_RECEIVE_RESULTS {
            state.results_truncated = true;
            state.cancel_requested = true;
            return;
        }
        state.results.push(result);
        if state.results.len() == MAX_RECEIVE_RESULTS {
            state.cancel_requested = true;
        }
    }
    pub(crate) fn end(&self, absence: Absence) {
        let mut state = self.lock();
        state.phase = ReceivePhase::Ended;
        state.absence.get_or_insert(absence);
        state.progress = None;
    }
    pub(crate) fn cleaned(&self, cleanup: Result<(), worker::Error>) {
        self.lock().cleanup = Some(cleanup);
    }
}

#[cfg(test)]
mod tests;
