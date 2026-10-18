//! One attempt's bounded, content-free UI state and file-only stop request.
use super::{Absence, Report, SendPhase, SendStatus};
use fr_files::sender::{Error, Progress, Stage};
use std::{
    fmt,
    sync::{Arc, Mutex, MutexGuard},
};

/// One coherent sample from the original file owner, not independent evidence
/// of host disk effects. A fully queued object still needs a publication receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendSnapshot {
    pub status: SendStatus,
    pub stage: Option<Stage>,
    /// The current transfer only. An unknown directory size stays `None` until
    /// measured; a queued byte is not an acknowledged or published byte.
    pub current: Option<Progress>,
    /// A local request, NOT proof of retirement, rollback or completed cleanup.
    pub cancel_requested: bool,
    /// Set only after polling the original stopped source. `None` is pending or
    /// not requested; `Ok` proves local source cleanup, not remote publication.
    pub cleanup: Option<Result<(), Error>>,
}

/// A clone can inspect or request cancellation of ONE explicit selection. It
/// cannot grant control, start a source, resubmit a selection or renew a deadline.
#[derive(Clone)]
pub struct SendControl(Arc<Mutex<SendSnapshot>>);
impl fmt::Debug for SendControl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("FileSendControl")
            .field(&self.snapshot())
            .finish()
    }
}
impl SendControl {
    pub(crate) fn new() -> Self {
        Self(Arc::new(Mutex::new(SendSnapshot {
            status: SendStatus {
                phase: SendPhase::WaitingForControl,
                absence: None,
                report: None,
            },
            stage: None,
            current: None,
            cancel_requested: false,
            cleanup: None,
        })))
    }
    fn lock(&self) -> MutexGuard<'_, SendSnapshot> {
        // Only private, bounded status updates run under this lock; never I/O.
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    pub fn status(&self) -> SendStatus {
        self.lock().status
    }
    pub fn snapshot(&self) -> SendSnapshot {
        *self.lock()
    }
    /// Request file-only cancellation on the next ordinary owner turn. This
    /// never cancels the desktop context or revokes its input/clipboard grants.
    /// Before a file lane exists, the already-negotiated attachment must still
    /// settle (under its original deadline); no selected source is started.
    /// In flight, earlier receipts and uncertain effects remain authoritative.
    /// Returns true only for the first request on a nonterminal selection.
    pub fn cancel(&self) -> bool {
        let mut state = self.lock();
        if state.cancel_requested
            || matches!(state.status.phase, SendPhase::Finished | SendPhase::Ended)
        {
            return false;
        }
        state.cancel_requested = true;
        true
    }
    pub(crate) fn cancel_requested(&self) -> bool {
        self.lock().cancel_requested
    }
    pub(crate) fn update(&self, change: impl FnOnce(&mut SendStatus)) {
        change(&mut self.lock().status);
    }
    pub(crate) fn end(&self, absence: Absence) {
        self.update(|status| {
            status.phase = SendPhase::Ended;
            status.absence.get_or_insert(absence);
        });
    }
    pub(crate) fn observe(
        &self,
        report: Option<&Report>,
        stage: Option<Stage>,
        current: Option<Progress>,
    ) {
        let mut snapshot = self.lock();
        snapshot.stage = stage;
        snapshot.current = current;
        if let Some(report) = report {
            snapshot.status.report = Some(*report);
            if report.complete {
                snapshot.status.phase = SendPhase::Finished;
            }
        }
    }
    pub(crate) fn cleaned(&self, cleanup: Option<Result<(), Error>>) {
        if let Some(cleanup) = cleanup {
            self.lock().cleanup = Some(cleanup);
        }
    }
}

#[cfg(test)]
mod tests;
