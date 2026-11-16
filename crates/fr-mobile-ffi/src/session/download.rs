//! One explicit destination and the original controller's bounded ATP receiver.
//! The session owns disk work and authority. This UI owner only registers the
//! selected directory once, requests cancellation and retains actual receipts.
#![forbid(unsafe_code)]

use fr_files::{
    receive::Publication,
    worker::{Completion, Error as WorkerError},
};
use frd::{
    native_files::{
        Absence, ConflictPolicy, Directory, DirectoryRefusal, Limits, ReceiveControl, ReceivePhase,
        ReceiveRequest, ReceiveStatus,
    },
    session_startup::{FileSendError, NativeObserver},
};
use std::{
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
};

/// Content-free evidence. Staged bytes never count as published files, and a
/// late disk result cannot repair an earlier bounded cleanup failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub configured: bool,
    pub cancel_requested: bool,
    pub absence: Option<Absence>,
    pub status: Option<ReceiveStatus>,
    pub cleanup: Option<Result<(), FileSendError>>,
}

/// Only metadata from one retained terminal disk receipt. No pathname, file
/// name, content hash or contents enter this report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    pub index: Option<u64>,
    pub outcome: &'static str,
    pub bytes: Option<u64>,
    pub durable: Option<bool>,
    pub reason: Option<String>,
}

impl Report {
    fn observe(&mut self, mut status: ReceiveStatus) {
        // Collect late publication evidence, but never repair a native cleanup
        // failure already observed for this original receiver.
        if let Some(Err(error)) = self.status.as_ref().and_then(|previous| previous.cleanup) {
            status.cleanup = Some(Err(error));
        }
        self.cancel_requested |= status.cancel_requested;
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
        let native = self.status.as_ref().and_then(|status| status.cleanup);
        if matches!(self.cleanup, Some(Err(_))) || matches!(native, Some(Err(_))) {
            return "failed";
        }
        if self.cleanup == Some(Ok(())) || native == Some(Ok(())) {
            return "finished";
        }
        if self
            .status
            .as_ref()
            .is_some_and(ReceiveStatus::may_have_started)
        {
            "pending"
        } else {
            "not_started"
        }
    }

    pub fn receipts(&self) -> Vec<Receipt> {
        let Some(status) = &self.status else {
            return Vec::new();
        };
        status
            .results
            .iter()
            .take(frd::native_files::MAX_RECEIVE_RESULTS)
            .filter_map(|result| {
                if matches!(
                    result.outcome,
                    Ok(Completion::Begun(_) | Completion::Written(_))
                ) {
                    return None;
                }
                let index = result.id.checked_sub(1);
                let mut receipt = Receipt {
                    index,
                    outcome: "publication_unknown",
                    bytes: None,
                    durable: None,
                    reason: None,
                };
                match result.outcome {
                    Ok(Completion::Published(published))
                        if index.is_some() && published.id == result.id =>
                    {
                        receipt.outcome = "locally_published";
                        receipt.bytes = Some(published.bytes);
                        receipt.durable = Some(published.publication == Publication::Durable);
                    }
                    Ok(Completion::Cancelled) => receipt.outcome = "cancelled",
                    Err(error) => {
                        receipt.outcome = match error {
                            WorkerError::UnknownEffect
                            | WorkerError::Poisoned
                            | WorkerError::Panicked
                            | WorkerError::Session(fr_files::session::Error::UnknownEffect) => {
                                "publication_unknown"
                            }
                            WorkerError::Cancelled
                            | WorkerError::Closed
                            | WorkerError::Session(fr_files::session::Error::Closed) => "cancelled",
                            _ => "refused",
                        };
                        receipt.reason = Some(format!("{error:?}"));
                    }
                    _ => {}
                }
                Some(receipt)
            })
            .collect()
    }

    pub fn results_truncated(&self) -> bool {
        self.status.as_ref().is_some_and(|status| {
            status.results_truncated
                || status.results.len() > frd::native_files::MAX_RECEIVE_RESULTS
        })
    }

    /// Failure and absence remain inspectable after the receive lane ends.
    pub fn reason(&self) -> Option<Absence> {
        self.absence
            .or_else(|| self.status.as_ref().and_then(|status| status.absence))
    }
}

struct State {
    request: Option<ReceiveRequest>,
    bound: bool,
    attempted: bool,
    retired: bool,
    report: Report,
    source: Option<ReceiveControl>,
}

impl State {
    fn collect(&mut self) {
        if let Some(source) = &self.source {
            self.report.observe(source.status());
        }
    }

    fn cancel(&mut self) -> bool {
        self.collect();
        let changed = match &self.source {
            Some(source) => source.cancel(),
            None => self.request.is_some() && !self.report.cancel_requested,
        };
        self.report.cancel_requested |= changed;
        self.collect();
        changed
    }
}

/// One explicitly selected private root. Clones retain this same consumable
/// owner; a resize, new grant or reconnect can never approve it again.
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
        let directory = Directory::open(path, limits)?.with_conflict_policy(conflict);
        Ok(Self::new(ReceiveRequest::new(&directory)))
    }

    fn new(request: ReceiveRequest) -> Arc<Self> {
        Arc::new(Self(Mutex::new(State {
            request: Some(request),
            bound: false,
            attempted: false,
            retired: false,
            report: Report {
                configured: false,
                cancel_requested: false,
                absence: None,
                status: None,
                cleanup: None,
            },
            source: None,
        })))
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
        // Synchronous registration only: the observer starts disk work after
        // its real grant and one-use attachment. No filesystem call runs here.
        match configure(request) {
            Ok(source) => {
                if state.report.cancel_requested {
                    source.cancel();
                }
                state.report.configured = true;
                state.source = Some(source);
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

    /// Pre-observer dial retries spend no destination. Once the original
    /// observer registers it, every later attempt is permanently unapproved.
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

#[cfg(test)]
mod tests {
    use super::*;
    use fr_files::{session, wire::ResultReceipt};

    fn selected() -> Arc<Download> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let random = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let path = root.join(format!(
            "fr-native-download-{}-{random}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        Download::select(&path, None, None, false).unwrap()
    }

    #[test]
    fn the_selected_destination_belongs_to_one_session_and_one_observer() {
        let owner = selected();
        let clone = Arc::clone(&owner);
        assert!(!owner.available());
        assert!(owner.bind());
        assert!(!clone.bind());
        owner.retire(); // no observer has registered it; original dial can retry
        assert!(owner.available());
        owner.configure_with(|_| Err(Absence::NotNegotiated));
        assert!(!owner.available());
        clone.configure_with(|_| panic!("destination was reapproved"));
        assert_eq!(owner.report().reason(), Some(Absence::NotNegotiated));
    }

    #[test]
    fn cancel_before_registration_cannot_reapprove_the_original_destination() {
        let owner = selected();
        assert!(owner.bind());
        assert!(owner.cancel());
        assert!(!owner.cancel());
        assert!(!owner.available());
        // The original optional attachment may already have been offered.
        // Consume its registration once; cancellation precedes worker service.
        owner.configure_with(|_| Err(Absence::Unavailable));
        owner.configure_with(|_| panic!("cancelled destination reused"));
        owner.retire();
        assert!(owner.report().cancel_requested);
        assert!(!owner.available());
    }

    #[test]
    fn finish_releases_unstarted_consent_and_cannot_restart_it() {
        let owner = selected();
        assert!(owner.bind());
        owner.finish();
        assert!(!owner.bind());
        assert!(!owner.available());
        assert!(owner.lock().request.is_none());
        owner.configure_with(|_| panic!("finished destination reused"));
        assert_eq!(owner.report().phase_name(), "cancelled_before_start");
        assert_eq!(owner.report().cleanup_name(), "not_started");
    }

    fn status() -> ReceiveStatus {
        ReceiveStatus {
            phase: ReceivePhase::Receiving,
            absence: None,
            progress: None,
            results: Vec::new(),
            results_truncated: false,
            cancel_requested: false,
            cleanup: None,
        }
    }

    #[test]
    fn original_publication_survives_cancellation_and_failed_cleanup() {
        let owner = selected();
        owner.lock().report.configured = true;
        let mut original = status();
        original.results.push(ResultReceipt {
            id: 1,
            outcome: Ok(Completion::Published(session::Receipt {
                id: 1,
                bytes: 7,
                publication: Publication::DurabilityUnknown,
            })),
        });
        owner.lock().report.status = Some(original);
        owner.cleaned(Err(FileSendError::Expired));
        owner.cleaned(Ok(()));
        owner.finish();
        let report = owner.report();
        assert_eq!(report.cleanup, Some(Err(FileSendError::Expired)));
        assert_eq!(report.cleanup_name(), "failed");
        let receipts = report.receipts();
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].outcome, "locally_published");
        assert_eq!(receipts[0].bytes, Some(7));
        assert_eq!(receipts[0].durable, Some(false));
    }

    #[test]
    fn fully_staged_bytes_never_become_a_publication_receipt() {
        let owner = selected();
        let mut original = status();
        original.results.push(ResultReceipt {
            id: 1,
            outcome: Ok(Completion::Written(session::Progress {
                id: 1,
                staged_bytes: 5,
                total_bytes: 5,
            })),
        });
        owner.lock().report.status = Some(original);
        let report = owner.report();
        assert!(report.receipts().is_empty());
        assert_eq!(report.cleanup_name(), "pending");
    }

    #[test]
    fn a_native_cleanup_failure_and_unknown_effect_never_become_success() {
        let owner = selected();
        let mut original = status();
        original.cleanup = Some(Err(WorkerError::Cleanup));
        original.results.push(ResultReceipt {
            id: 1,
            outcome: Err(WorkerError::UnknownEffect),
        });
        {
            let mut state = owner.lock();
            state.report.configured = true;
            state.report.status = Some(original);
        }
        owner.cleaned(Ok(()));
        let report = owner.report();
        assert_eq!(report.cleanup_name(), "failed");
        assert_eq!(report.receipts()[0].outcome, "publication_unknown");
        assert_eq!(report.receipts()[0].bytes, None);
    }

    #[test]
    fn ended_receivers_without_receipts_still_require_actual_cleanup() {
        let owner = selected();
        let mut original = status();
        original.phase = ReceivePhase::Ended;
        owner.lock().report.observe(original.clone());
        assert_eq!(owner.report().cleanup_name(), "pending");
        original.absence = Some(Absence::NotNegotiated);
        owner.lock().report.observe(original.clone());
        assert_eq!(owner.report().cleanup_name(), "not_started");
        original.absence = Some(Absence::ReceiveFailed(fr_files::quic::Error::Cancelled));
        owner.lock().report.observe(original);
        assert_eq!(owner.report().cleanup_name(), "pending");
    }

    #[test]
    fn late_native_collection_keeps_cleanup_failure_and_real_publication() {
        let owner = selected();
        let mut failed = status();
        failed.cleanup = Some(Err(WorkerError::Cleanup));
        owner.lock().report.observe(failed);
        let mut joined = status();
        joined.cleanup = Some(Ok(()));
        joined.results.push(ResultReceipt {
            id: 1,
            outcome: Ok(Completion::Published(session::Receipt {
                id: 1,
                bytes: 0,
                publication: Publication::DurabilityUnknown,
            })),
        });
        owner.lock().report.observe(joined);
        let report = owner.report();
        assert_eq!(report.cleanup_name(), "failed");
        assert_eq!(report.receipts()[0].outcome, "locally_published");
        assert_eq!(report.receipts()[0].bytes, Some(0));
    }

    #[test]
    fn report_bounds_and_original_result_ids_never_invent_publication() {
        let owner = selected();
        let mut original = status();
        let bound = frd::native_files::MAX_RECEIVE_RESULTS;
        for id in 1..=u64::try_from(bound + 1).unwrap() {
            original.results.push(ResultReceipt {
                id,
                outcome: Ok(Completion::Published(session::Receipt {
                    id,
                    bytes: 1,
                    publication: Publication::Durable,
                })),
            });
        }
        original.results[0].id = 0;
        original.results[1].id = 3; // receipt still belongs to 2
        owner.lock().report.observe(original);
        let report = owner.report();
        let receipts = report.receipts();
        assert_eq!(receipts.len(), bound);
        assert!(report.results_truncated());
        assert_eq!(receipts[0].index, None);
        assert_eq!(receipts[0].outcome, "publication_unknown");
        assert_eq!(receipts[1].outcome, "publication_unknown");
        assert_eq!(receipts[2].outcome, "locally_published");
    }

    #[test]
    fn invalid_roots_and_limits_refuse_before_registration() {
        assert!(matches!(
            Download::select(Path::new("relative"), None, None, false),
            Err(DirectoryRefusal::Relative)
        ));
        assert!(matches!(
            Download::select(&std::env::temp_dir(), Some(10), Some(5), false),
            Err(DirectoryRefusal::Limits)
        ));
    }
}
