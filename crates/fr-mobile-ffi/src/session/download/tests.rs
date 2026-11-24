use super::*;
use fr_files::{session, wire::ResultReceipt};

fn selected() -> Arc<Download> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let random = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir();
    // Unix system temp paths may traverse /var aliases. Windows canonicalize
    // adds the deliberately unsupported verbatim prefix, so retain its ordinary
    // absolute temp path and let the real receiver validate each component.
    #[cfg(unix)]
    let root = std::fs::canonicalize(root).unwrap();
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
        folder_job: None,
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
    assert_eq!(report.receipts().len(), 0);
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

fn report(results: Vec<ResultReceipt>) -> Report {
    Report {
        configured: true,
        cancel_requested: false,
        absence: None,
        status: Some(ReceiveStatus {
            folder_job: None,
            phase: ReceivePhase::Ended,
            absence: None,
            progress: None,
            results,
            results_truncated: false,
            cancel_requested: false,
            cleanup: Some(Ok(())),
        }),
        cleanup: Some(Ok(())),
    }
}

fn published(id: u64, bytes: u64, publication: Publication) -> ResultReceipt {
    ResultReceipt {
        id,
        outcome: Ok(Completion::Published(session::Receipt {
            id,
            bytes,
            publication,
        })),
    }
}

#[test]
fn atomic_publication_and_directory_durability_remain_distinct() {
    // Ordinary Receiving::stop sets Ended without an absence. Actual cleanup
    // and Published receipts establish effects; closing alone establishes none.
    let result = report(vec![
        published(1, 0, Publication::Durable),
        published(2, 19, Publication::DurabilityUnknown),
    ]);
    assert!(result.is_success());
    assert!(result.cleanup_complete());
    assert_eq!(result.cleanup_name(), "finished");
    let receipts = result.receipts();
    assert_eq!(receipts[0].id, 1);
    assert_eq!(receipts[0].index, Some(0));
    assert_eq!(receipts[0].outcome, "locally_published");
    assert_eq!(receipts[0].receipt, "published");
    assert_eq!(receipts[0].bytes, Some(0));
    assert_eq!(receipts[0].durable, Some(true));
    assert_eq!(receipts[1].receipt, "published_durability_unknown");
    assert_eq!(receipts[1].outcome, "locally_published");
    assert_eq!(receipts[1].bytes, Some(19));
    assert_eq!(receipts[1].durable, Some(false));
    assert_eq!(receipts[1].reason, None);
    assert!(report(Vec::new()).is_success()); // consent is not a file request
}

#[test]
fn staging_and_invalid_receipts_cannot_claim_publication_or_success() {
    let progress = session::Progress {
        id: 1,
        staged_bytes: 19,
        total_bytes: 19,
    };
    for outcome in [Completion::Begun(progress), Completion::Written(progress)] {
        let result = report(vec![ResultReceipt {
            id: 1,
            outcome: Ok(outcome),
        }]);
        assert_eq!(result.receipts(), []);
        assert!(!result.is_success());
    }
    let mut mismatch = published(2, 19, Publication::Durable);
    mismatch.id = 1;
    for invalid in [mismatch, published(0, 19, Publication::Durable)] {
        let result = report(vec![invalid]);
        assert_eq!(result.receipts()[0].receipt, "publication_unknown");
        assert_eq!(result.receipts()[0].bytes, None);
        assert_eq!(result.receipts()[0].durable, None);
        assert!(!result.is_success());
    }
}

#[test]
fn cancellation_and_later_errors_retain_prior_published_bytes() {
    for outcome in [
        Ok(Completion::Cancelled),
        Err(WorkerError::UnknownEffect),
        Err(WorkerError::Session(session::Error::Storage(
            fr_files::receive::Error::Conflict,
        ))),
    ] {
        let mut result = report(vec![
            published(1, 19, Publication::DurabilityUnknown),
            ResultReceipt { id: 2, outcome },
        ]);
        result.cancel_requested = true;
        let receipts = result.receipts();
        assert_eq!(receipts[0].bytes, Some(19));
        assert_eq!(receipts[0].durable, Some(false));
        assert!(receipts[1].reason.is_some());
        assert!(!result.is_success());
    }
}

#[test]
fn receive_failure_before_the_first_sample_still_requires_actual_cleanup() {
    let mut result = report(Vec::new());
    result.cleanup = None;
    let status = result.status.as_mut().unwrap();
    status.cleanup = None;
    status.absence = Some(Absence::ReceiveFailed(fr_files::quic::Error::Cancelled));
    assert_eq!(result.cleanup_name(), "pending");
    assert!(!result.cleanup_complete());
    assert!(!result.is_success());
    assert_eq!(result.reason_code(), Some("receive_cancelled"));
    // The aggregate observer result cannot substitute for this receiver's
    // positive cleanup evidence after disk work may have started.
    result.cleanup = Some(Ok(()));
    assert_eq!(result.cleanup_name(), "pending");
    assert!(!result.cleanup_complete());
    result.status.as_mut().unwrap().cleanup = Some(Err(WorkerError::Cleanup));
    assert_eq!(result.cleanup_name(), "failed");
    assert!(!result.cleanup_complete());
}

#[test]
fn reports_bound_receipts_and_never_hide_omissions() {
    let result = report(
        (1..=70)
            .map(|id| published(id, 19, Publication::Durable))
            .collect(),
    );
    assert_eq!(result.receipts().len(), MAX_RECEIVE_RESULTS);
    assert!(result.results_truncated());
    assert!(!result.is_success());
}

#[test]
fn ui_document_keeps_progress_publication_and_cleanup_separate() {
    let mut original = report(vec![published(1, 19, Publication::DurabilityUnknown)]);
    let status = original.status.as_mut().unwrap();
    status.phase = ReceivePhase::Receiving;
    status.cleanup = None;
    status.progress = Some(session::Progress {
        id: 2,
        staged_bytes: 11,
        total_bytes: 11,
    });
    let document = original.document();
    assert_eq!(document["version"], 1);
    assert_eq!(document["progress"]["staged_bytes"], 11);
    assert_eq!(document["receipts"].as_array().unwrap().len(), 1);
    assert_eq!(document["receipts"][0]["bytes"], 19);
    assert_eq!(document["receipts"][0]["durable"], false);
    assert_eq!(document["cleanup"], "pending");
    assert_eq!(document["cleanup_complete"], false);
    assert_eq!(document["success"], false);
    assert!(document.get("path").is_none());
    assert!(document.get("name").is_none());
    original.cleanup = Some(Err(FileSendError::Expired));
    original.cancel_requested = true;
    let stopped = original.document();
    assert_eq!(stopped["receipts"], document["receipts"]);
    assert_eq!(stopped["cancel_requested"], true);
    assert_eq!(stopped["cleanup"], "failed");
    assert_eq!(stopped["cleanup_complete"], false);
}

#[test]
fn ui_document_stays_bounded_and_retains_a_typed_absence() {
    let mut original = report(
        (1..=70)
            .map(|id| published(id, 19, Publication::Durable))
            .collect(),
    );
    original.absence = Some(Absence::NotNegotiated);
    let document = original.document();
    assert_eq!(document["absence"], "host_download_unavailable");
    assert_eq!(
        document["receipts"].as_array().unwrap().len(),
        MAX_RECEIVE_RESULTS
    );
    assert_eq!(document["receipts_truncated"], true);
    assert_eq!(document["success"], false);
    assert!(document.to_string().len() < 32_768);
}
