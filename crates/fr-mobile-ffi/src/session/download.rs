//! One explicitly selected local destination on the original native session.
//! The observer retains ATP, authority and disk ownership; this handle exposes
//! only bounded progress, committed publication receipts and file-only cancel.
#![forbid(unsafe_code)]

use fr_files::{
    receive::Publication,
    session,
    wire::ResultReceipt,
    worker::{Completion, Error as WorkerError},
};
use frd::{
    native_files::{
        Absence, ConflictPolicy, Directory, DirectoryRefusal, Limits, MAX_RECEIVE_RESULTS,
        ReceiveControl, ReceivePhase, ReceiveRequest, ReceiveStatus,
    },
    session_startup::{FileSendError, NativeObserver},
};
use std::{
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
};

/// One original terminal result. `bytes` appears only for an actual atomic
/// publication, never for progress or a fully staged but unpublished object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    pub id: u64,
    /// Preserve the zero-based selection index used by the native result document.
    pub index: Option<u64>,
    pub outcome: &'static str,
    pub receipt: &'static str,
    pub bytes: Option<u64>,
    pub durable: Option<bool>,
    pub reason: Option<String>,
}

/// Content-free original receive evidence, retained after its view closes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub configured: bool,
    pub cancel_requested: bool,
    pub absence: Option<Absence>,
    pub status: Option<ReceiveStatus>,
    /// The original aggregate file cleanup result. Its first failure cannot be
    /// repaired by a later successful collection of the retained disk owner.
    pub cleanup: Option<Result<(), FileSendError>>,
}

impl Report {
    fn observe(&mut self, mut status: ReceiveStatus) {
        self.cancel_requested |= status.cancel_requested;
        if let Some(Err(error)) = self.status.as_ref().and_then(|previous| previous.cleanup) {
            status.cleanup = Some(Err(error));
        }
        // Retain bounded publication evidence even if a producer violates its
        // own result bound. An explicit omission can never report success.
        if status.results.len() > MAX_RECEIVE_RESULTS {
            status.results.truncate(MAX_RECEIVE_RESULTS);
            status.results_truncated = true;
        }
        self.status = Some(status);
    }

    pub fn phase_name(&self) -> &'static str {
        match self.status.as_ref().map(|status| status.phase) {
            Some(ReceivePhase::WaitingForControl) => "waiting_for_control",
            Some(ReceivePhase::Negotiating) => "negotiating",
            Some(ReceivePhase::Receiving) => "receiving",
            Some(ReceivePhase::Ended) => "ended",
            None if self.cancel_requested => "cancelled_before_start",
            None if self.absence.is_some() => "unavailable",
            None => "selected",
        }
    }

    pub fn cleanup_name(&self) -> &'static str {
        if matches!(self.cleanup, Some(Err(_)))
            || self
                .status
                .as_ref()
                .is_some_and(|status| matches!(status.cleanup, Some(Err(_))))
        {
            return "failed";
        }
        match self.status.as_ref() {
            Some(status) if status.cleanup == Some(Ok(())) => "finished",
            Some(status) if status.may_have_started() => "pending",
            _ if self.configured && self.cleanup.is_none() => "pending",
            _ => "not_started",
        }
    }

    /// Positive original cleanup evidence, or a pre-start refusal with no disk
    /// owner. A receive failure before its first progress sample stays pending.
    pub fn cleanup_complete(&self) -> bool {
        matches!(self.cleanup_name(), "finished" | "not_started")
    }

    pub fn reason(&self) -> Option<Absence> {
        self.absence
            .or_else(|| self.status.as_ref().and_then(|status| status.absence))
    }

    pub fn reason_code(&self) -> Option<&'static str> {
        self.reason().map(absence_code)
    }

    pub fn results_truncated(&self) -> bool {
        self.status.as_ref().is_some_and(|status| {
            status.results_truncated || status.results.len() > MAX_RECEIVE_RESULTS
        })
    }

    pub fn receipts(&self) -> Vec<Receipt> {
        self.status
            .as_ref()
            .into_iter()
            .flat_map(|status| status.results.iter().take(MAX_RECEIVE_RESULTS))
            .filter_map(|result| receipt(*result))
            .collect()
    }

    /// Receipt permission needs no particular number of host selections. When
    /// an actual transfer did occur, only verified atomic publication counts.
    /// Windows' unconfirmed directory durability is separately visible and is
    /// never reported as a durable publication or unknown publication effect.
    pub fn is_success(&self) -> bool {
        self.configured
            && self.cleanup_name() == "finished"
            && self.reason().is_none()
            && !self.results_truncated()
            && self.status.as_ref().is_some_and(|status| {
                status.phase == ReceivePhase::Ended
                    && status.progress.is_none()
                    && status.results.iter().all(|result| {
                        matches!(
                            result.outcome,
                            Ok(Completion::Published(published))
                                if result.id != 0 && published.id == result.id
                        )
                    })
            })
    }
}

fn receipt(result: ResultReceipt) -> Option<Receipt> {
    let mut receipt = Receipt {
        id: result.id,
        index: result.id.checked_sub(1),
        outcome: "publication_unknown",
        receipt: "publication_unknown",
        bytes: None,
        durable: None,
        reason: None,
    };
    match result.outcome {
        Ok(Completion::Begun(_) | Completion::Written(_)) => return None,
        Ok(Completion::Published(published)) if result.id != 0 && published.id == result.id => {
            let durable = published.publication == Publication::Durable;
            receipt.outcome = "locally_published";
            receipt.receipt = if durable {
                "published"
            } else {
                "published_durability_unknown"
            };
            receipt.bytes = Some(published.bytes);
            receipt.durable = Some(durable);
        }
        Ok(Completion::Published(_)) => {
            receipt.reason = Some("receive_invalid_result".into());
        }
        Ok(Completion::Cancelled) => {
            receipt.outcome = "cancelled";
            receipt.receipt = "cancelled";
            receipt.reason = Some("receive_cancelled".into());
        }
        Err(error) => {
            receipt.outcome = match worker_code(error) {
                "publication_unknown" => "publication_unknown",
                "receive_cancelled" => "cancelled",
                _ => "refused",
            };
            receipt.receipt = receipt.outcome;
            receipt.reason = Some(format!("{error:?}"));
        }
    }
    Some(receipt)
}

fn absence_code(absence: Absence) -> &'static str {
    match absence {
        Absence::NotNegotiated => "host_download_unavailable",
        Absence::WithClipboard => "files_with_clipboard_unsupported",
        Absence::Unavailable => "files_receive_unavailable",
        Absence::Refused(FileSendError::Cancelled) => "receive_cancelled",
        Absence::Refused(_) => "files_receive_refused_locally",
        Absence::SetupFailed(_) => "files_receive_setup_failed",
        Absence::ReceiveFailed(error) => match error {
            fr_files::quic::Error::Cancelled => "receive_cancelled",
            fr_files::quic::Error::Limits => "receive_limit",
            fr_files::quic::Error::Transport(fr_transport::quic::Error::SendExpired) => {
                "send_expired"
            }
            fr_files::quic::Error::Transport(
                fr_transport::quic::Error::Expired
                | fr_transport::quic::Error::Stream(fr_wire::stream::StreamError::Expired),
            ) => "receive_expired",
            fr_files::quic::Error::File(fr_files::wire::Error::Atp(
                fr_files::atp::receive::Error::Worker(error),
            )) => worker_code(error),
            _ => "files_receive_failed",
        },
    }
}

fn worker_code(error: WorkerError) -> &'static str {
    match error {
        WorkerError::UnknownEffect
        | WorkerError::Poisoned
        | WorkerError::Panicked
        | WorkerError::Session(session::Error::UnknownEffect) => "publication_unknown",
        WorkerError::Cancelled
        | WorkerError::Closed
        | WorkerError::Session(session::Error::Closed) => "receive_cancelled",
        WorkerError::Session(session::Error::Storage(fr_files::receive::Error::Conflict)) => {
            "receive_conflict"
        }
        WorkerError::Session(session::Error::Storage(fr_files::receive::Error::Integrity)) => {
            "receive_integrity"
        }
        WorkerError::Session(session::Error::Storage(fr_files::receive::Error::Incomplete)) => {
            "receive_incomplete"
        }
        WorkerError::Session(session::Error::Expired) => "receive_expired",
        WorkerError::Session(session::Error::Permission | session::Error::Authority(_)) => {
            "receive_permission"
        }
        WorkerError::Allocation
        | WorkerError::Session(
            session::Error::Quota
            | session::Error::RateLimited
            | session::Error::Storage(fr_files::receive::Error::Quota),
        ) => "receive_limit",
        _ => "receive_failed",
    }
}

struct State {
    request: Option<ReceiveRequest>,
    bound: bool,
    attempted: bool,
    retired: bool,
    control: Option<ReceiveControl>,
    report: Report,
}

impl State {
    fn collect(&mut self) {
        if let Some(control) = &self.control {
            self.report.observe(control.status());
        }
    }

    fn cancel(&mut self) -> bool {
        self.collect();
        let changed = match &self.control {
            Some(control) => control.cancel(),
            None => self.request.is_some() && !self.report.cancel_requested,
        };
        if changed {
            self.report.cancel_requested = true;
        }
        self.collect();
        changed
    }
}

/// An explicit pinned directory approved once for one native session. Clones
/// inspect or cancel the original lane; they cannot reopen a pathname, renew a
/// deadline, grant authority or approve another connection.
pub struct Download(Mutex<State>);

impl Download {
    pub fn select(
        path: &Path,
        max_file_bytes: Option<u64>,
        max_session_bytes: Option<u64>,
        keep_both: bool,
    ) -> Result<Arc<Self>, DirectoryRefusal> {
        let defaults = Limits::default();
        let limits = Limits {
            max_file_bytes: max_file_bytes.unwrap_or(defaults.max_file_bytes),
            max_session_bytes: max_session_bytes.unwrap_or(defaults.max_session_bytes),
        };
        let conflict = if keep_both {
            ConflictPolicy::KeepBoth
        } else {
            ConflictPolicy::Reject
        };
        Self::select_with_limits(path, limits, conflict)
    }

    /// Pin an explicitly chosen directory with typed byte and conflict policy.
    /// Both constructors produce the same original, consumable receive owner.
    pub fn select_with_limits(
        path: &Path,
        limits: Limits,
        conflict: ConflictPolicy,
    ) -> Result<Arc<Self>, DirectoryRefusal> {
        let directory = Directory::open(path, limits)?.with_conflict_policy(conflict);
        Ok(Arc::new(Self(Mutex::new(State {
            request: Some(ReceiveRequest::new(&directory)),
            bound: false,
            attempted: false,
            retired: false,
            control: None,
            report: Report {
                configured: false,
                cancel_requested: false,
                absence: None,
                status: None,
                cleanup: None,
            },
        }))))
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(|poison| {
            let mut state = poison.into_inner();
            state.bound = true;
            state.retired = true;
            state.cancel();
            state.report.absence.get_or_insert(Absence::Unavailable);
            state
        })
    }

    pub(super) fn bind(&self) -> bool {
        let mut state = self.lock();
        if state.bound || state.attempted || state.retired {
            return false;
        }
        state.bound = true;
        true
    }

    pub(super) fn available(&self) -> bool {
        let state = self.lock();
        state.bound && !state.attempted && !state.retired && !state.report.cancel_requested
    }

    pub(super) fn configure(&self, observer: &mut NativeObserver) {
        self.configure_with(|request| observer.request_receive_files(request));
    }

    fn configure_with(
        &self,
        configure: impl FnOnce(ReceiveRequest) -> Result<ReceiveControl, Absence>,
    ) {
        let mut state = self.lock();
        if !state.bound || state.attempted || state.retired {
            return;
        }
        state.attempted = true;
        let Some(request) = state.request.take() else {
            return;
        };
        // Registration performs no native I/O or wait. Cancellation after
        // negotiation still installs the original attachment expectation with
        // its stop set before the first controller turn can admit disk work.
        match configure(request) {
            Ok(control) => {
                if state.report.cancel_requested {
                    control.cancel();
                }
                state.report.configured = true;
                state.control = Some(control);
            }
            Err(absence) => {
                state.report.absence.get_or_insert(absence);
            }
        }
        state.collect();
    }

    pub fn cancel(&self) -> bool {
        self.lock().cancel()
    }

    pub(super) fn retire(&self) {
        let mut state = self.lock();
        if state.attempted {
            state.retired = true;
            state.cancel();
        }
    }

    pub(super) fn finish(&self) {
        let mut state = self.lock();
        state.cancel();
        state.request = None;
        state.attempted = true;
        state.retired = true;
    }

    pub(super) fn cleaned(&self, result: Result<(), FileSendError>) {
        let mut state = self.lock();
        if state.report.configured && !matches!(state.report.cleanup, Some(Err(_))) {
            state.report.cleanup = Some(result);
        }
        state.collect();
    }

    pub fn report(&self) -> Report {
        let mut state = self.lock();
        state.collect();
        state.report.clone()
    }
}

impl Drop for Download {
    fn drop(&mut self) {
        self.finish();
    }
}

// Also included through an explicit path by portable test hosts.
#[cfg(test)]
#[path = "download/tests.rs"]
mod tests;
