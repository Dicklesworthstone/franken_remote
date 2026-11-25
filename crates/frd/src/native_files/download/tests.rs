use super::*;
use crate::native_files::Publication;
use fr_files::session::Receipt;
use std::{
    fs,
    sync::atomic::{AtomicU64, Ordering},
};

struct Selected(PathBuf);
impl Selected {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "fr-host-selection-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&path, b"selected locally").unwrap();
        Self(path)
    }
}
impl Drop for Selected {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
fn published(id: u64) -> ResultReceipt {
    ResultReceipt {
        id,
        outcome: Ok(worker::Completion::Published(Receipt {
            id,
            bytes: 17,
            publication: Publication::Durable,
        })),
    }
}

#[test]
fn selection_clones_cannot_send_twice_and_retain_the_original_source() {
    let rt = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    let cx = rt.request_cx_with_budget(asupersync::types::Budget::INFINITE);
    let selected = Selected::new();
    let host = HostSelection::select(std::slice::from_ref(&selected.0)).unwrap();
    let clone = host.clone();
    // Unix permits removing the selected name; Windows retains an actual
    // handle that denies write/delete sharing. Neither case can redirect a
    // later claim or give another session a second request.
    #[cfg(not(windows))]
    fs::remove_file(&selected.0).unwrap();
    #[cfg(windows)]
    {
        assert!(fs::remove_file(&selected.0).is_err());
        assert!(fs::write(&selected.0, b"replacement").is_err());
    }
    assert!(host.is_available() && clone.is_available());
    let other = cx.clone();
    let first = std::thread::spawn(move || clone.take(other).ok());
    let second = host.take(cx.clone()).ok();
    let first = first.join().unwrap();
    assert_ne!(first.is_some(), second.is_some());
    let original = first.or(second).unwrap();
    assert!(matches!(&original, HostDownload::Initial(request) if request.selection.len() == 1));
    assert!(!host.is_available());
    assert!(matches!(host.take(cx), Err(Absence::Unavailable)));
    let debug = format!("{host:?}");
    assert!(!debug.contains(selected.0.to_str().unwrap()));
    #[cfg(windows)]
    {
        // Consuming the configuration transfers the pinned source into the
        // original request; it must not unlock it while that request lives.
        assert!(fs::remove_file(&selected.0).is_err());
        assert!(fs::write(&selected.0, b"replacement").is_err());
    }
    drop(original);
    #[cfg(windows)]
    fs::write(&selected.0, b"after the original owner ended").unwrap();
}

#[test]
fn progress_is_not_a_result_and_publication_survives_terminal_updates() {
    let control = ReceiveControl::new();
    control.set_phase(ReceivePhase::Receiving);
    let progress = Progress {
        id: 7,
        staged_bytes: 3,
        total_bytes: 17,
    };
    for outcome in [
        worker::Completion::Begun(progress),
        worker::Completion::Written(progress),
    ] {
        control.observe(ResultReceipt {
            id: 7,
            outcome: Ok(outcome),
        });
        let status = control.status();
        assert_eq!(status.progress, Some(progress));
        assert_eq!(status.results, [] as [fr_files::wire::ResultReceipt; 0]);
    }
    let receipt = published(7);
    control.observe(receipt);
    control.observe(receipt);
    assert!(control.cancel());
    assert!(!control.cancel());
    control.end(Absence::Unavailable);
    control.observe(ResultReceipt {
        id: 7,
        outcome: Ok(worker::Completion::Cancelled),
    });
    control.cleaned(Err(worker::Error::UnknownEffect));
    control.set_phase(ReceivePhase::Receiving);
    control.progress(Some(progress));
    let status = control.status();
    assert_eq!(status.results, [receipt]);
    assert_eq!(status.phase, ReceivePhase::Ended);
    assert_eq!(status.progress, None);
    assert_eq!(status.cleanup, Some(Err(worker::Error::UnknownEffect)));
    assert!(status.cancel_requested && !status.results_truncated);
}

#[test]
fn report_capacity_stops_files_and_reports_any_omission() {
    let control = ReceiveControl::new();
    for id in 0..MAX_RECEIVE_RESULTS {
        control.observe(ResultReceipt {
            id: u64::try_from(id).unwrap(),
            outcome: Err(worker::Error::Session(fr_files::session::Error::Quota)),
        });
    }
    let full = control.status();
    assert_eq!(full.results.len(), MAX_RECEIVE_RESULTS);
    assert!(full.cancel_requested && !full.results_truncated);
    assert!(control.cancel_requested());
    // Reobserving the retained last result is not overflow.
    control.observe(*full.results.last().unwrap());
    assert!(!control.status().results_truncated);
    control.observe(published(u64::try_from(MAX_RECEIVE_RESULTS).unwrap()));
    let overflow = control.status();
    assert_eq!(overflow.results, full.results);
    assert!(overflow.results_truncated);
}

#[test]
fn a_late_real_publication_replaces_uncertainty_but_never_loses_to_cleanup() {
    for earlier in [
        Ok(worker::Completion::Cancelled),
        Err(worker::Error::UnknownEffect),
    ] {
        let control = ReceiveControl::new();
        control.end(Absence::Unavailable);
        control.observe(ResultReceipt {
            id: 2,
            outcome: earlier,
        });
        let receipt = published(2);
        control.observe(receipt);
        control.observe(ResultReceipt {
            id: 2,
            outcome: Err(worker::Error::UnknownEffect),
        });
        control.observe(ResultReceipt {
            id: 2,
            outcome: Ok(worker::Completion::Cancelled),
        });
        control.cleaned(Ok(()));
        let status = control.status();
        assert_eq!(status.phase, ReceivePhase::Ended);
        assert_eq!(status.results, [receipt]);
        assert_eq!(status.cleanup, Some(Ok(())));
        assert!(!control.cancel());
    }
}

#[test]
fn first_cleanup_failure_survives_later_collection_and_late_publication() {
    let control = ReceiveControl::new();
    let collector = control.clone();
    control.set_phase(ReceivePhase::Receiving);
    control.observe(ResultReceipt {
        id: 9,
        outcome: Err(worker::Error::UnknownEffect),
    });
    control.cleaned(Ok(()));
    control.cleaned(Err(worker::Error::Cleanup));
    control.end(Absence::Unavailable);

    // A late original disk receipt proves the committed effect, even though
    // cleanup failed. A later successful join must not rewrite that failure.
    let receipt = published(9);
    collector.observe(receipt);
    collector.cleaned(Ok(()));
    collector.cleaned(Err(worker::Error::Panicked));
    let status = control.status();
    assert_eq!(status.results, [receipt]);
    assert_eq!(status.cleanup, Some(Err(worker::Error::Cleanup)));
    assert_eq!(status.phase, ReceivePhase::Ended);
    assert!(status.may_have_started());
}

#[test]
fn cleanup_evidence_distinguishes_pre_start_absence_from_an_unjoined_owner() {
    for absence in [
        Absence::NotNegotiated,
        Absence::WithClipboard,
        Absence::Unavailable,
    ] {
        let control = ReceiveControl::new();
        assert!(!control.status().may_have_started());
        control.end(absence);
        assert!(!control.status().may_have_started());
        // A later real receipt is stronger evidence than the earlier absence.
        control.observe(published(1));
        assert!(control.status().may_have_started());
    }
    let control = ReceiveControl::new();
    control.end(Absence::ReceiveFailed(fr_files::quic::Error::Cancelled));
    let status = control.status();
    assert_eq!(status.results, [] as [ResultReceipt; 0]);
    assert_eq!(status.cleanup, None);
    assert!(status.may_have_started());
    // An explicit join also establishes an owner even before a progress sample.
    let control = ReceiveControl::new();
    control.cleaned(Ok(()));
    assert!(control.status().may_have_started());
}

#[test]
fn folder_receipts_keep_the_full_job_budget_without_expanding_ordinary_receives() {
    let control = ReceiveControl::new();
    let job = fr_files::sync::JobStatus {
        state: fr_wire::sync_job_state::SyncJobState {
            job_id: 41,
            sync_direction: fr_wire::sync_job_state::SyncDirection::HostToController,
            conflict_policy: fr_wire::sync_job_state::ConflictPolicy::KeepBoth,
            phase: fr_wire::sync_job_state::SyncPhase::Advertised,
            reason: fr_wire::files::Reason::None,
            entries_total: 0,
            entries_completed: 0,
            conflicts_kept: 0,
            bytes_total: 0,
            bytes_completed: 0,
            manifest_revision: 0,
        },
        failure: None,
        confirmed_publications: 0,
        confirmed_conflicts: 0,
        rounds_settled: 0,
        cleanup_complete: false,
    };
    control.record_folder_job(job);
    for id in 0..MAX_FOLDER_RECEIVE_RESULTS - 1 {
        control.observe(published(u64::try_from(id).unwrap()));
    }
    let status = control.status();
    assert_eq!(status.results.len(), MAX_FOLDER_RECEIVE_RESULTS - 1);
    assert!(!status.cancel_requested && !status.results_truncated);
    assert_eq!(status.folder_job, Some(job));
    let final_id = u64::try_from(MAX_FOLDER_RECEIVE_RESULTS - 1).unwrap();
    let refusal = ResultReceipt {
        id: final_id,
        outcome: Err(worker::Error::Session(fr_files::session::Error::Quota)),
    };
    control.observe(refusal);
    assert!(control.cancel_requested());
    assert!(!control.status().results_truncated);
    // Late proof can strengthen the final result even after the bound stops work.
    control.observe(published(final_id));
    assert_eq!(control.status().results.last(), Some(&published(final_id)));
    control.observe(published(final_id + 1));
    let stopped = control.status();
    assert!(stopped.results_truncated);
    assert_eq!(stopped.results.len(), MAX_FOLDER_RECEIVE_RESULTS);
    assert_eq!(stopped.results.first(), Some(&published(0)));

    let ordinary = ReceiveControl::new();
    for id in 0..MAX_RECEIVE_RESULTS {
        ordinary.observe(published(u64::try_from(id).unwrap()));
    }
    assert!(ordinary.cancel_requested());
    assert_eq!(ordinary.status().results.len(), MAX_RECEIVE_RESULTS);
}
