//! Explicit local uploads owned by one original native session. The existing
//! observer owns ATP, authority, disk work and publication receipts. Every new
//! batch consumes a newly selected descriptor; no earlier selection is replayed.
#![forbid(unsafe_code)]

use fr_wire::negotiation::Offer;
use frd::{
    native_files::{
        Absence, Publication, SelectReason, SelectRefusal, Selection, SendControl, SendIntake,
        SendPhase, SendRequest, SendSnapshot,
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
    /// Monotonic original-owner batch; zero means no local file selected.
    pub batch: u8,
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
        if self.batch == 0 {
            return true; // no local file request; says nothing about capability
        }
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
            None if self.batch == 0 => "no_file_selected",
            None => "selected",
        }
    }

    /// Bounded content-free UI evidence. Byte progress never stands in for
    /// the original host's publication receipt or the source's cleanup.
    #[cfg(any(test, target_os = "ios", target_os = "macos"))]
    pub(crate) fn document(&self) -> serde_json::Value {
        let snapshot = self.snapshot;
        let progress = snapshot.and_then(|value| value.current);
        let complete = snapshot
            .and_then(|value| value.status.report)
            .is_some_and(|report| report.complete);
        serde_json::json!({
            "batch": self.batch,
            "selected_bytes": self.selected_bytes,
            "phase": self.phase_name(),
            "stage": snapshot.and_then(|value| value.stage).map(stage_name),
            "queued_bytes": progress.map(|value| value.queued_bytes),
            "total_bytes": progress.and_then(|value| value.total_bytes),
            "cancel_requested": self.cancel_requested,
            "pause_requested": snapshot.is_some_and(|value| value.pause_requested),
            "paused": snapshot.and_then(|value| value.paused),
            "receipt": self.receipt_name(),
            "receipt_detail": self.receipt_detail(),
            "published_bytes": self.published_bytes(),
            "cleanup": self.cleanup_name(),
            "complete": complete,
            "success": self.is_success(),
            "absence": self.absence.map(|reason| format!("{reason:?}")),
        })
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

#[cfg(any(test, target_os = "ios", target_os = "macos"))]
fn stage_name(stage: fr_files::sender::Stage) -> &'static str {
    use fr_files::sender::Stage;
    match stage {
        Stage::Idle => "idle",
        Stage::Queued => "queued",
        Stage::Preparing => "preparing",
        Stage::AwaitingAcceptance => "awaiting_acceptance",
        Stage::Streaming => "streaming",
        Stage::AwaitingProof => "awaiting_proof",
        Stage::Finished => "finished",
        Stage::Closed => "closed",
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

#[cfg(test)]
fn file_capability(name: &str) -> bool {
    matches!(
        name,
        fr_wire::attachment::FILES_CAPABILITY
            | fr_wire::files::CAPABILITY
            | fr_wire::files::CHANNEL_SCOPE_CAPABILITY
            | fr_wire::files::DOWNLOAD_CAPABILITY
            | fr_wire::files::DUPLEX_CAPABILITY
    )
}

/// Retain only the original command's still-unspent file directions. Common
/// attachment/ATP capabilities belong to both directions; retiring an upload
/// cannot remove those while an explicit download still needs them.
pub(super) fn offer(original: &Offer, upload: bool, download: bool) -> Result<Offer, ()> {
    super::file_offer::retain(original, upload, download)
}

/// One command retains at most nine explicit files and their original receipts.
pub const MAX_UPLOADS: usize = 9;

/// A pinned descriptor prepared off the UI/session threads. No public path or
/// content survives selection; submission consumes this value even on refusal.
pub struct PreparedUpload {
    request: SendRequest,
    bytes: u64,
}

#[cfg(any(target_os = "ios", target_os = "macos"))]
impl PreparedUpload {
    pub(crate) const fn selected_bytes(&self) -> u64 {
        self.bytes
    }
}

struct State {
    request: Option<SendRequest>,
    /// Bind at `Session::start`, before any thread/connection can race another
    /// clone for this choice. An internal retry stays within that same owner.
    bound: bool,
    attempted: bool,
    report: Report,
    source: Option<SendControl>,
    intake: Option<SendIntake>,
    retired: bool,
    history: Vec<Report>,
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

/// One original upload lane and bounded history. An initial selection is pinned
/// before startup; later explicit choices prepare descriptors off the UI thread.
/// No pathname or contents are retained in public diagnostics.
pub struct Upload(Mutex<State>);

impl Upload {
    pub fn prepare(path: &Path) -> Result<PreparedUpload, SelectRefusal> {
        let selected = Selection::select(&[path.to_path_buf()])?;
        let selected_bytes = selected.sizes().next().flatten().ok_or(SelectRefusal {
            index: 0,
            reason: SelectReason::Directory,
        })?;
        // Duplicates the selected descriptor, never reopens its pathname. Only
        // the original observer can start the bounded disk source after grant.
        let request = selected.request()?;
        Ok(PreparedUpload {
            request,
            bytes: selected_bytes,
        })
    }

    pub fn select(path: &Path) -> Result<Arc<Self>, SelectRefusal> {
        Ok(Self::new(Some(Self::prepare(path)?)))
    }

    /// Negotiate one optional original upload lane without opening any source.
    /// Every actual file still requires a later explicit local selection.
    pub fn interactive() -> Arc<Self> {
        Self::new(None)
    }

    fn new(selected: Option<PreparedUpload>) -> Arc<Self> {
        let (request, batch, bytes) = selected.map_or((None, 0, 0), |prepared| {
            (Some(prepared.request), 1, prepared.bytes)
        });
        Arc::new(Self(Mutex::new(State {
            request,
            bound: false,
            attempted: false,
            report: Report {
                batch,
                selected_bytes: bytes,
                configured: false,
                cancel_requested: false,
                absence: None,
                snapshot: None,
                cleanup: None,
            },
            source: None,
            intake: None,
            retired: false,
            history: Vec::with_capacity(MAX_UPLOADS - 1),
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
        if state.bound || state.attempted {
            return false;
        }
        state.bound = true;
        true
    }

    /// The first opened observer consumes this choice even if the host declines
    /// files. Reconnect never silently grants another chance to publish it.
    pub(super) fn configure(&self, observer: &mut NativeObserver) {
        self.configure_with(|request| match request {
            Some(request) => observer.configure_file_send_with_intake(request),
            None => observer.configure_file_drop(),
        });
    }

    fn configure_with(
        &self,
        configure: impl FnOnce(Option<SendRequest>) -> Result<(SendControl, SendIntake), Absence>,
    ) {
        let mut state = self.lock();
        if !state.bound || state.attempted || state.retired {
            return;
        }
        state.attempted = true;
        let request = state.request.take();
        // Only synchronous session registration runs under this lock: no
        // source open, hash, read, network wait, or native join. An early Cancel
        // still installs/settles a possibly negotiated attachment, with its
        // stop already set before any service turn can start a source.
        match configure(request) {
            Ok((source, intake)) => {
                if state.report.cancel_requested {
                    source.cancel();
                }
                state.report.configured = true;
                state.source = Some(source);
                state.intake = Some(intake);
            }
            Err(absence) => {
                state.report.absence.get_or_insert(absence);
            }
        }
        state.collect();
    }

    pub(super) fn available(&self) -> bool {
        let state = self.lock();
        state.bound && !state.attempted && !state.retired && !state.report.cancel_requested
    }

    /// File-only cancellation. A negotiated but unstarted attachment still
    /// settles under its original deadline; no selected contents are read.
    pub fn cancel(&self) -> bool {
        let mut state = self.lock();
        // Hiding an observing view to choose its first file must not cancel an
        // empty negotiated lane. Terminal retire/finish still stop that setup
        // owner through State::cancel.
        state.report.batch != 0 && state.cancel()
    }

    /// Local selection capacity, including before the first control grant.
    /// Opens no source and grants no permission to read or send its contents.
    #[cfg(any(test, target_os = "ios", target_os = "macos"))]
    pub(crate) fn can_select(&self) -> bool {
        let mut state = self.lock();
        state.collect();
        state.bound
            && state.attempted
            && !state.retired
            && state.report.configured
            && state.report.absence.is_none()
            && usize::from(state.report.batch) < MAX_UPLOADS
            && (state.report.batch == 0
                || state.intake.as_ref().is_some_and(SendIntake::ready))
    }

    /// A failed attempt before observer creation has spent no file selection.
    /// Once configured, this owner is terminal across every viewing retry.
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

    /// The original lane has capacity for one newly selected file. This is
    /// never an authority guarantee; the original controller rechecks it.
    pub fn ready(&self) -> bool {
        let mut state = self.lock();
        state.collect();
        !state.retired
            && usize::from(state.report.batch) < MAX_UPLOADS
            && state.intake.as_ref().is_some_and(SendIntake::ready)
    }

    /// Consume a freshly prepared descriptor on this exact configured intake.
    /// The caller must first validate its original native choice and current
    /// session. The intake retains its own original policy/deadline/authority.
    pub fn submit(&self, prepared: PreparedUpload) -> Result<(), FileSendError> {
        self.submit_if(prepared, || true)
    }

    /// Charge the original local choice through both bounded owner locks.
    /// The predicate may only check local choice state in the reviewed order
    /// upload -> intake -> granted source -> input queue. Bounded native scalar
    /// clock/foreground reads may not pump messages. No native UI/file work or
    /// callbacks into this upload/intake are permitted. The intake resamples its
    /// original transfer clock after the predicate returns.
    pub fn submit_if(
        &self,
        prepared: PreparedUpload,
        check: impl FnMut() -> bool,
    ) -> Result<(), FileSendError> {
        let mut state = self.lock();
        state.collect();
        if state.retired {
            return Err(FileSendError::Closed);
        }
        if usize::from(state.report.batch) >= MAX_UPLOADS {
            return Err(FileSendError::Limits);
        }
        let intake = state.intake.as_ref().ok_or(FileSendError::Closed)?;
        let source = intake.submit_if(prepared.request, check)?;
        let batch = state.report.batch + 1;
        if state.report.batch != 0 {
            let previous = state.report;
            state.history.push(previous);
        }
        state.report = Report {
            batch,
            selected_bytes: prepared.bytes,
            configured: true,
            cancel_requested: false,
            absence: None,
            snapshot: None,
            cleanup: None,
        };
        state.source = Some(source);
        state.collect();
        Ok(())
    }

    /// Menu commands name the original batch they displayed. An old Cancel or
    /// pacing command cannot affect a newer explicit file selection.
    pub fn cancel_batch(&self, batch: u8) -> bool {
        let mut state = self.lock();
        state.report.batch == batch && batch != 0 && state.cancel()
    }

    pub fn pace_batch(&self, batch: u8, paused: bool) -> bool {
        let mut state = self.lock();
        if state.report.batch != batch || batch == 0 || state.retired {
            return false;
        }
        let changed = state.source.as_ref().is_some_and(|source| {
            if paused {
                source.pause()
            } else {
                source.resume()
            }
        });
        state.collect();
        changed
    }

    /// Bounded content-free history, including the current actual selection.
    /// Waiting with no file contributes no fabricated transfer receipt.
    pub fn reports(&self) -> Vec<Report> {
        let mut state = self.lock();
        state.collect();
        let mut reports = state.history.clone();
        if state.report.batch != 0 {
            reports.push(state.report);
        }
        reports
    }

    pub fn is_success(&self) -> bool {
        self.reports().iter().all(Report::is_success)
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
        let original = fr_wire::control::with_committed_text(
            fr_client::native::control_offer_with_auxiliary(true, true, false),
        )
        .unwrap();
        assert_eq!(
            offer(&original, upload.available(), false).unwrap(),
            original
        );
        upload.configure_with(|_| Err(Absence::NotNegotiated));
        let pruned = offer(&original, upload.available(), false).unwrap();
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
        assert!(offer(&required, upload.available(), false).is_err());
    }

    #[test]
    fn each_unspent_direction_keeps_its_common_boundaries_without_reapproving_the_other() {
        let original = fr_wire::control::with_committed_text(
            fr_client::native::control_offer_with_duplex(true, false),
        )
        .unwrap();
        for (upload, download) in [(true, true), (true, false), (false, true), (false, false)] {
            let retained = offer(&original, upload, download).unwrap();
            retained.validate().unwrap();
            let selected = |name| retained.capabilities.iter().any(|cap| cap.name == name);
            assert_eq!(
                selected(fr_wire::attachment::FILES_CAPABILITY),
                upload || download
            );
            assert_eq!(selected(fr_wire::files::CAPABILITY), upload || download);
            assert_eq!(selected(fr_wire::files::CHANNEL_SCOPE_CAPABILITY), upload);
            assert_eq!(selected(fr_wire::files::DOWNLOAD_CAPABILITY), download);
            assert_eq!(
                selected(fr_wire::files::DUPLEX_CAPABILITY),
                upload && download
            );
            assert!(selected(fr_wire::clipboard::CAPABILITY));
            assert!(selected(fr_wire::control::TEXT_CAPABILITY));
            assert!(
                retained
                    .capabilities
                    .iter()
                    .all(|cap| original.capabilities.contains(cap))
            );
        }
        let mut required = original;
        required
            .capabilities
            .iter_mut()
            .find(|cap| cap.name == fr_wire::files::DOWNLOAD_CAPABILITY)
            .unwrap()
            .required = true;
        assert!(offer(&required, true, false).is_err());
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
    fn an_empty_interactive_lane_has_no_selected_file_or_fabricated_receipt() {
        let upload = Upload::interactive();
        assert!(upload.bind());
        assert!(!upload.ready());
        upload.configure_with(|request| {
            assert!(request.is_none());
            Err(Absence::NotNegotiated)
        });
        assert_eq!(upload.report().batch, 0);
        assert_eq!(upload.report().receipt_name(), "none");
        assert_eq!(upload.report().published_bytes(), None);
        assert_eq!(upload.reports(), [] as [Report; 0]);
        assert!(upload.is_success(), "no file was requested");
        upload.configure_with(|_| panic!("refused intake replayed"));
    }

    #[test]
    fn hiding_without_a_file_does_not_spend_an_interactive_lane() {
        let upload = Upload::interactive();
        assert!(upload.bind());
        assert!(!upload.cancel());
        assert!(upload.available());
        assert!(!upload.report().cancel_requested);
        assert_eq!(upload.report().batch, 0);
        upload.finish();
        assert!(!upload.available());
        assert!(!upload.can_select());
    }

    #[test]
    fn upload_status_contains_only_original_bounded_evidence() {
        let upload = selected();
        assert!(upload.bind());
        let report = upload.report();
        let document = report.document();
        assert_eq!(document["batch"], 1);
        assert_eq!(document["selected_bytes"], 13);
        assert_eq!(document["phase"], "selected");
        assert_eq!(document["receipt"], "none");
        assert_eq!(document["published_bytes"], serde_json::Value::Null);
        assert_eq!(document["queued_bytes"], serde_json::Value::Null);
        assert_eq!(document["cleanup"], "not_started");
        assert_eq!(document["complete"], false);
        assert_eq!(document["success"], false);
        let encoded = document.to_string();
        assert!(!encoded.contains("fr-selected-upload"));
        assert!(!encoded.contains("chosen source"));
        assert!(encoded.len() < 1024);
        assert!(upload.cancel());
        assert_eq!(upload.report().document()["cancel_requested"], true);
        assert_eq!(upload.report().document()["receipt"], "none");
    }

    #[test]
    fn status_never_upgrades_queued_bytes_to_host_publication() {
        let upload = selected();
        let mut report = upload.report();
        report.configured = true;
        report.snapshot = Some(SendSnapshot {
            status: frd::native_files::SendStatus {
                phase: SendPhase::Sending,
                absence: None,
                report: None,
            },
            stage: Some(fr_files::sender::Stage::AwaitingProof),
            current: Some(fr_files::sender::Progress {
                id: 1,
                queued_bytes: 13,
                total_bytes: Some(13),
            }),
            cancel_requested: false,
            pause_requested: false,
            paused: Some(false),
            cleanup: None,
        });
        let document = report.document();
        assert_eq!(document["queued_bytes"], 13);
        assert_eq!(document["total_bytes"], 13);
        assert_eq!(document["stage"], "awaiting_proof");
        assert_eq!(document["published_bytes"], serde_json::Value::Null);
        assert_eq!(document["receipt"], "none");
        assert_eq!(document["complete"], false);
        assert_eq!(document["success"], false);
        assert_eq!(document["cleanup"], "pending");
    }

    #[test]
    fn prepared_descriptors_cannot_enter_a_retired_or_unconfigured_intake() {
        let source = selected();
        let request = source.lock().request.take().unwrap();
        let upload = Upload::interactive();
        upload.finish();
        assert_eq!(
            upload.submit(PreparedUpload { request, bytes: 13 }),
            Err(FileSendError::Closed),
        );
        assert_eq!(upload.reports(), [] as [Report; 0]);
        assert!(!upload.ready());
        let source = selected();
        let request = source.lock().request.take().unwrap();
        assert_eq!(
            Upload::interactive().submit(PreparedUpload { request, bytes: 13 }),
            Err(FileSendError::Closed),
        );
    }

    #[test]
    fn the_ninth_original_batch_refuses_another_descriptor_without_replacing_history() {
        let upload = selected();
        {
            let mut state = upload.lock();
            for batch in 1..9 {
                let mut report = state.report;
                report.batch = batch;
                state.history.push(report);
            }
            state.report.batch = 9;
        }
        let source = selected();
        let request = source.lock().request.take().unwrap();
        assert_eq!(
            upload.submit(PreparedUpload { request, bytes: 13 }),
            Err(FileSendError::Limits),
        );
        assert_eq!(
            upload.reports().iter().map(|r| r.batch).collect::<Vec<_>>(),
            (1..=9).collect::<Vec<_>>()
        );
        assert!(!upload.ready());
    }

    #[test]
    fn an_old_batch_cancel_or_pacing_command_cannot_touch_the_next_selection() {
        let upload = selected();
        upload.lock().report.batch = 2;
        assert!(!upload.cancel_batch(1));
        assert!(!upload.pace_batch(1, true));
        assert!(!upload.pace_batch(1, false));
        assert!(!upload.report().cancel_requested);
        assert!(upload.cancel_batch(2));
        assert!(upload.report().cancel_requested);
    }

    #[test]
    fn later_reports_cannot_erase_an_earlier_failed_request() {
        let upload = selected();
        {
            let mut state = upload.lock();
            let mut earlier = state.report;
            earlier.cleanup = Some(Err(FileSendError::Expired));
            state.history.push(earlier);
            state.report.batch = 2;
        }
        assert!(!upload.is_success());
        upload.cleaned(Ok(()));
        assert_eq!(
            upload.reports()[0].cleanup,
            Some(Err(FileSendError::Expired))
        );
        assert!(!upload.is_success());
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
