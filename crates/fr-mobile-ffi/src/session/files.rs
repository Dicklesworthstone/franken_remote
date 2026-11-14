//! One explicit local upload, owned by one original native session. The
//! existing observer owns ATP, authority, disk work and publication receipts.
//! Cloning this handle can inspect/cancel it, never select or send it again.
#![forbid(unsafe_code)]

use fr_wire::negotiation::Offer;
use frd::{
    native_files::{
        Absence, Publication, SelectReason, SelectRefusal, Selection, SendControl, SendPhase,
        SendRequest, SendSnapshot,
    },
    session_startup::{FileSendError, FileSendOutcome, FileSendReceipt, NativeObserver},
};
use std::{
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
};

/// Content-free evidence from the original upload. Queued bytes are not host
/// publication; only the original snapshot's actual receipt says that. Local
/// cancellation cannot erase a receipt or resolve an uncertain publication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    pub selected_bytes: u64,
    pub configured: bool,
    pub cancel_requested: bool,
    pub absence: Option<Absence>,
    pub snapshot: Option<SendSnapshot>,
    /// Original cleanup result. Its first failure, including the original
    /// deadline, remains failed after a later successful source collection.
    pub cleanup: Option<Result<(), FileSendError>>,
}

impl Report {
    fn receipt(&self) -> Option<FileSendReceipt> {
        let report = self.snapshot?.status.report?;
        let mut receipts = report.receipts();
        if report.total != 1 || receipts.len() != 1 {
            return None;
        }
        receipts.next()
    }

    pub fn receipt_name(&self) -> &'static str {
        receipt_name(self.receipt())
    }

    /// The original receipt's typed refusal/interruption reason. These enums
    /// contain no path, basename, file contents, or native error strings.
    pub fn receipt_detail(&self) -> Option<String> {
        receipt_detail(self.receipt())
    }

    /// Only the original host's publication receipt supplies these bytes.
    /// Even a complete transport queue cannot establish this value.
    pub fn published_bytes(&self) -> Option<u64> {
        match self.receipt()?.outcome {
            FileSendOutcome::HostPublished { bytes, .. } => Some(bytes),
            _ => None,
        }
    }

    /// The one selected file has a durable host publication receipt and the
    /// original source completed without a batch/cleanup failure. This is
    /// independent of, and can never repair, the containing session's outcome.
    pub fn is_success(&self) -> bool {
        let completed = self.snapshot.is_some_and(|snapshot| {
            snapshot.status.report.is_some_and(|report| {
                report.total == 1 && report.started == 1 && report.complete && report.stop.is_none()
            })
        });
        let cleanup_ok = self.cleanup.is_none_or(|result| result.is_ok())
            && self
                .snapshot
                .is_none_or(|snapshot| snapshot.cleanup.is_none_or(|result| result.is_ok()));
        completed_upload_succeeded(completed, cleanup_ok, self.receipt())
    }

    pub fn phase_name(&self) -> &'static str {
        match self.snapshot.map(|snapshot| snapshot.status.phase) {
            Some(SendPhase::WaitingForControl) => "waiting_for_control",
            Some(SendPhase::Negotiating) => "negotiating",
            Some(SendPhase::WaitingForSelection) => "waiting_for_selection",
            Some(SendPhase::SelectionAccepted) => "selection_accepted",
            Some(SendPhase::Sending) => "sending",
            Some(SendPhase::Finished) => "finished",
            Some(SendPhase::Ended) => "ended",
            None if self.cancel_requested => "cancelled_before_start",
            None if self.absence.is_some() => "unavailable",
            None => "selected",
        }
    }

    pub fn cleanup_name(&self) -> &'static str {
        match self
            .cleanup
            .or_else(|| self.snapshot.and_then(|snapshot| snapshot.cleanup))
        {
            Some(Ok(())) => "finished",
            Some(Err(_)) => "failed",
            None if self.snapshot.is_some_and(|snapshot| {
                snapshot.status.report.is_some_and(|report| report.complete)
            }) =>
            {
                "finished"
            }
            None if self.configured => "pending",
            None => "not_started",
        }
    }
}

fn completed_upload_succeeded(
    completed: bool,
    cleanup_ok: bool,
    receipt: Option<FileSendReceipt>,
) -> bool {
    completed
        && cleanup_ok
        && matches!(
            receipt.map(|receipt| receipt.outcome),
            Some(FileSendOutcome::HostPublished {
                publication: Publication::Durable,
                ..
            })
        )
}

fn receipt_name(receipt: Option<FileSendReceipt>) -> &'static str {
    match receipt.map(|receipt| receipt.outcome) {
        None => "none",
        Some(FileSendOutcome::HostPublished {
            publication: Publication::Durable,
            ..
        }) => "host_published",
        Some(FileSendOutcome::HostPublished {
            publication: Publication::DurabilityUnknown,
            ..
        }) => "host_published_durability_unknown",
        Some(FileSendOutcome::HostRefused(_)) => "host_refused",
        Some(FileSendOutcome::ControllerPublished { .. }) => "controller_published",
        Some(FileSendOutcome::ControllerRefused(_)) => "controller_refused",
        Some(FileSendOutcome::InterruptedBeforePublication(_)) => "interrupted_before_publication",
        Some(FileSendOutcome::PublicationUnknown) => "publication_unknown",
    }
}

fn receipt_detail(receipt: Option<FileSendReceipt>) -> Option<String> {
    match receipt?.outcome {
        FileSendOutcome::HostRefused(reason) | FileSendOutcome::ControllerRefused(reason) => {
            Some(format!("{reason:?}"))
        }
        FileSendOutcome::InterruptedBeforePublication(error) => Some(format!("{error:?}")),
        _ => None,
    }
}

fn file_capability(name: &str) -> bool {
    matches!(
        name,
        fr_wire::attachment::FILES_CAPABILITY
            | fr_wire::files::CAPABILITY
            | fr_wire::files::CHANNEL_SCOPE_CAPABILITY
    )
}

struct State {
    request: Option<SendRequest>,
    /// Bind at Session::start, before any thread/connection can race another
    /// clone for this choice. An internal retry stays within that same owner.
    bound: bool,
    attempted: bool,
    report: Report,
    source: Option<SendControl>,
}

impl State {
    fn collect(&mut self) {
        if let Some(source) = &self.source {
            let snapshot = source.snapshot();
            self.report.cancel_requested |= snapshot.cancel_requested;
            if self.report.absence.is_none() {
                self.report.absence = snapshot.status.absence;
            }
            self.report.snapshot = Some(snapshot);
        }
    }

    fn cancel(&mut self) -> bool {
        self.collect();
        let changed = match &self.source {
            Some(source) => source.cancel(),
            None => self.request.is_some() && !self.report.cancel_requested,
        };
        if changed {
            self.report.cancel_requested = true;
        }
        self.collect();
        changed
    }
}

/// An already selected, pinned file. Selection/opening happens before session
/// startup; no pathname or contents are retained in public diagnostics.
pub struct Upload(Mutex<State>);

impl Upload {
    pub fn select(path: &Path) -> Result<Arc<Self>, SelectRefusal> {
        let selected = Selection::select(&[path.to_path_buf()])?;
        let selected_bytes = selected.sizes().next().flatten().ok_or(SelectRefusal {
            index: 0,
            reason: SelectReason::Directory,
        })?;
        // Duplicates the selected descriptor, never reopens its pathname. Only
        // the original observer can start the bounded disk source after grant.
        let request = selected.request()?;
        Ok(Arc::new(Self(Mutex::new(State {
            request: Some(request),
            bound: false,
            attempted: false,
            report: Report {
                selected_bytes,
                configured: false,
                cancel_requested: false,
                absence: None,
                snapshot: None,
                cleanup: None,
            },
            source: None,
        }))))
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(|poison| {
            let mut state = poison.into_inner();
            state.bound = true;
            state.cancel();
            state.report.absence.get_or_insert(Absence::Unavailable);
            state
        })
    }

    pub(super) fn bind(&self) -> bool {
        let mut state = self.lock();
        if state.bound || state.attempted {
            return false;
        }
        state.bound = true;
        true
    }

    /// The first opened observer consumes this choice even if the host declines
    /// files. Reconnect never silently grants another chance to publish it.
    pub(super) fn configure(&self, observer: &mut NativeObserver) {
        self.configure_with(|request| observer.configure_file_send(request));
    }

    fn configure_with(&self, configure: impl FnOnce(SendRequest) -> Result<SendControl, Absence>) {
        let mut state = self.lock();
        if !state.bound || state.attempted {
            return;
        }
        state.attempted = true;
        let Some(request) = state.request.take() else {
            return;
        };
        // Only synchronous session registration runs under this lock: no
        // source open, hash, read, network wait, or native join. An early Cancel
        // still installs/settles a possibly negotiated attachment, with its
        // stop already set before any service turn can start a source.
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

    pub(super) fn offer(&self, original: &Offer) -> Result<Offer, ()> {
        let state = self.lock();
        if state.bound && !state.attempted && !state.report.cancel_requested {
            return Ok(original.clone());
        }
        if original
            .capabilities
            .iter()
            .any(|cap| cap.required && file_capability(&cap.name))
        {
            return Err(());
        }
        let mut offer = original.clone();
        offer.capabilities.retain(|cap| !file_capability(&cap.name));
        Ok(offer)
    }

    /// File-only cancellation. A negotiated but unstarted attachment still
    /// settles under its original deadline; no selected contents are read.
    pub fn cancel(&self) -> bool {
        self.lock().cancel()
    }

    /// A failed attempt before observer creation has spent no file selection.
    /// Once configured, this owner is terminal across every viewing retry.
    pub(super) fn retire(&self) {
        let mut state = self.lock();
        if state.source.is_some() {
            state.cancel();
        }
    }

    pub(super) fn finish(&self) {
        let mut state = self.lock();
        state.cancel();
        state.request = None;
        state.attempted = true;
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
        state.report
    }
}

impl Drop for Upload {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs::OpenOptions,
        io::Write,
        sync::atomic::{AtomicU64, Ordering},
    };

    fn selected() -> Arc<Upload> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "fr-selected-upload-{}-{unique}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        file.write_all(b"chosen source").unwrap();
        drop(file);
        Upload::select(&path).unwrap()
    }

    #[test]
    fn a_selected_descriptor_binds_to_exactly_one_session_even_through_clones() {
        let upload = selected();
        let clone = Arc::clone(&upload);
        assert_eq!(upload.report().selected_bytes, 13);
        assert!(upload.bind());
        assert!(!clone.bind());
        assert!(!upload.report().configured);
        assert_eq!(upload.report().cleanup_name(), "not_started");
    }

    #[test]
    fn an_unstarted_network_retry_preserves_the_same_one_use_selection() {
        let upload = selected();
        assert!(upload.bind());
        upload.retire();
        assert!(!upload.report().cancel_requested);
        let mut configured = 0;
        upload.configure_with(|_| {
            configured += 1;
            Err(Absence::NotNegotiated)
        });
        upload.configure_with(|_| panic!("a declined selection was replayed"));
        assert_eq!(configured, 1);
        assert_eq!(upload.report().absence, Some(Absence::NotNegotiated));
    }

    #[test]
    fn cancellation_before_configuration_is_retained_and_the_request_is_consumed_once() {
        let upload = selected();
        assert!(upload.bind());
        assert!(upload.cancel());
        assert!(!upload.cancel());
        assert!(
            upload.lock().request.is_some(),
            "an already offered attachment still needs its original configuration"
        );
        upload.configure_with(|_| Err(Absence::NotNegotiated));
        assert!(upload.report().cancel_requested);
        assert!(upload.lock().request.is_none());
        upload.configure_with(|_| panic!("cancelled choice was repeated"));
    }

    #[test]
    fn terminal_finish_drops_unstarted_selection_and_never_makes_it_reusable() {
        let upload = selected();
        assert!(upload.bind());
        upload.finish();
        assert!(upload.lock().request.is_none());
        assert!(!upload.bind());
        upload.configure_with(|_| panic!("terminal selection was configured"));
        assert_eq!(upload.report().phase_name(), "cancelled_before_start");
    }

    #[test]
    fn spent_upload_prunes_only_its_optional_markers_and_keeps_clipboard_text() {
        let upload = selected();
        assert!(upload.bind());
        let original = super::super::control_policy::offer(true, true, true).unwrap();
        assert_eq!(upload.offer(&original).unwrap(), original);
        upload.configure_with(|_| Err(Absence::NotNegotiated));
        let pruned = upload.offer(&original).unwrap();
        assert!(
            !pruned
                .capabilities
                .iter()
                .any(|cap| file_capability(&cap.name))
        );
        let mut expected = original.clone();
        expected
            .capabilities
            .retain(|cap| !file_capability(&cap.name));
        assert_eq!(pruned, expected);
        let mut required = original;
        required
            .capabilities
            .iter_mut()
            .find(|cap| file_capability(&cap.name))
            .unwrap()
            .required = true;
        assert!(upload.offer(&required).is_err());
    }

    #[test]
    fn directories_and_missing_sources_never_produce_an_upload_owner() {
        assert!(Upload::select(&std::env::temp_dir()).is_err());
        assert!(
            Upload::select(
                &std::env::temp_dir().join("fr-no-such-selected-upload-directory/source")
            )
            .is_err()
        );
    }

    #[test]
    fn original_cleanup_timeout_stays_failed_after_later_collection() {
        let upload = selected();
        assert!(upload.bind());
        // The policy fixture represents an already configured original owner;
        // this test makes no claim that a native transfer or join took place.
        upload.lock().report.configured = true;
        upload.cleaned(Err(FileSendError::Expired));
        upload.cleaned(Ok(()));
        let report = upload.report();
        assert_eq!(report.cleanup, Some(Err(FileSendError::Expired)));
        assert_eq!(report.cleanup_name(), "failed");
        assert!(!report.is_success());
        assert_eq!(report.published_bytes(), None);
        assert_eq!(report.receipt_name(), "none");
    }

    #[test]
    fn publication_receipts_remain_distinct_from_progress_durability_and_cleanup() {
        let receipt = |outcome| Some(FileSendReceipt { id: 1, outcome });
        let durable = receipt(FileSendOutcome::HostPublished {
            bytes: 7,
            publication: Publication::Durable,
        });
        assert!(completed_upload_succeeded(true, true, durable));
        assert_eq!(receipt_name(durable), "host_published");
        assert_eq!(receipt_detail(durable), None);
        assert!(
            !completed_upload_succeeded(false, true, durable),
            "a receipt does not prove source cleanup"
        );
        assert!(
            !completed_upload_succeeded(true, false, durable),
            "a published effect does not repair cleanup failure"
        );
        assert!(
            !completed_upload_succeeded(true, true, None),
            "all bytes queued is not a receipt"
        );
        for (outcome, name, detail) in [
            (
                FileSendOutcome::HostPublished {
                    bytes: 7,
                    publication: Publication::DurabilityUnknown,
                },
                "host_published_durability_unknown",
                None,
            ),
            (
                FileSendOutcome::HostRefused(fr_wire::files::Reason::Conflict),
                "host_refused",
                Some("Conflict"),
            ),
            (
                FileSendOutcome::HostRefused(fr_wire::files::Reason::Permission),
                "host_refused",
                Some("Permission"),
            ),
            (
                FileSendOutcome::HostRefused(fr_wire::files::Reason::Integrity),
                "host_refused",
                Some("Integrity"),
            ),
            (
                FileSendOutcome::InterruptedBeforePublication(FileSendError::Cancelled),
                "interrupted_before_publication",
                Some("Cancelled"),
            ),
            (
                FileSendOutcome::PublicationUnknown,
                "publication_unknown",
                None,
            ),
        ] {
            let retained = receipt(outcome);
            assert_eq!(receipt_name(retained), name);
            assert_eq!(receipt_detail(retained).as_deref(), detail);
            assert!(!completed_upload_succeeded(true, true, retained));
        }
    }
}
