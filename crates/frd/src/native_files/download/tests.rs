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
fn selection_clones_cannot_send_twice_or_reopen_a_replaced_path() {
    let selected = Selected::new();
    let host = HostSelection::select(std::slice::from_ref(&selected.0)).unwrap();
    let clone = host.clone();
    // The original descriptors are already selected. A later claim cannot
    // reopen a disappeared name or give another session a second request.
    fs::remove_file(&selected.0).unwrap();
    assert!(host.is_available() && clone.is_available());
    let first = std::thread::spawn(move || clone.take_request().is_ok());
    let second = host.take_request().is_ok();
    assert_ne!(first.join().unwrap(), second);
    assert!(!host.is_available());
    assert!(matches!(host.take_request(), Err(Absence::Unavailable)));
    let debug = format!("{host:?}");
    assert!(!debug.contains(selected.0.to_str().unwrap()));
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
