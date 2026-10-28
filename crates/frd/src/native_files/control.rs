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
    /// Latest local pacing intent. Unlike cancellation this is reversible, but
    /// never changes the original selection lifetime or controller authority.
    pub pause_requested: bool,
    /// Original sender's last observed admission gate, not an acknowledgement of the
    /// latest UI request. `None` means no sender has supplied an observation yet.
    pub paused: Option<bool>,
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
            pause_requested: false,
            paused: None,
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
            || matches!(
                state.status.phase,
                SendPhase::SelectionAccepted | SendPhase::Finished | SendPhase::Ended
            )
        {
            return false;
        }
        state.cancel_requested = true;
        true
    }
    /// Request same-connection pacing of this selection, including before its
    /// lane is established. The owner settles the original handshake and queues
    /// the selection once, then pauses it before any source starts. Ordinary
    /// deadlines, permission checks, receipts and cancellation continue to run.
    pub fn pause(&self) -> bool {
        self.set_paused(true)
    }
    /// Resume the same selection, never retry or recreate a cancelled/expired
    /// sender. A stale UI handle cannot reactivate terminal work.
    pub fn resume(&self) -> bool {
        self.set_paused(false)
    }
    fn set_paused(&self, paused: bool) -> bool {
        let mut state = self.lock();
        if state.cancel_requested
            || matches!(
                state.status.phase,
                SendPhase::WaitingForSelection
                    | SendPhase::SelectionAccepted
                    | SendPhase::Finished
                    | SendPhase::Ended
            )
            || state.pause_requested == paused
        {
            return false;
        }
        state.pause_requested = paused;
        true
    }
    pub(crate) fn pause_requested(&self) -> bool {
        self.lock().pause_requested
    }
    pub(crate) fn cancel_requested(&self) -> bool {
        self.lock().cancel_requested
    }
    pub(crate) fn update(&self, change: impl FnOnce(&mut SendStatus)) {
        change(&mut self.lock().status);
    }
    /// Linearize the empty setup handle's retirement against its Cancel button.
    /// A successful cancellation cannot disappear while the first selection is
    /// handed to a new owner; after handoff the old handle is terminal.
    pub(crate) fn accept_selection(&self) -> bool {
        let mut state = self.lock();
        if state.cancel_requested || state.status.phase != SendPhase::WaitingForSelection {
            return false;
        }
        state.status.phase = SendPhase::SelectionAccepted;
        true
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
        paused: Option<bool>,
    ) {
        let mut snapshot = self.lock();
        snapshot.stage = stage;
        snapshot.current = current;
        snapshot.paused = paused;
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

#[cfg(test)]
mod pause_tests;
