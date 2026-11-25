use super::*;
use crate::file_progress::{Snapshot, model::COLUMNS};
use frd::native_files::MAX_RECEIVE_RESULTS;

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
fn text(row: &[u8; COLUMNS]) -> &str {
    std::str::from_utf8(&row[..row.iter().position(|byte| *byte == 0).unwrap()]).unwrap()
}
fn published(id: u64, bytes: u64, publication: Publication) -> ResultReceipt {
    ResultReceipt {
        id,
        outcome: Ok(worker::Completion::Published(session::Receipt {
            id,
            bytes,
            publication,
        })),
    }
}

#[test]
fn a_download_permits_only_cancellation_on_the_original_active_attempt() {
    for phase in [
        ReceivePhase::WaitingForControl,
        ReceivePhase::Negotiating,
        ReceivePhase::Receiving,
        ReceivePhase::Ended,
    ] {
        for cancel_requested in [false, true] {
            let mut s = status();
            s.phase = phase;
            s.cancel_requested = cancel_requested;
            let frame = Frame::receive(&s);
            assert!(!frame.permits(Action::Pause));
            assert!(!frame.permits(Action::Resume));
            assert_eq!(
                frame.permits(Action::Cancel),
                !cancel_requested && phase != ReceivePhase::Ended
            );
        }
    }
}

#[test]
fn fully_staged_and_empty_objects_never_claim_publication() {
    let mut s = status();
    for total in [0, 4096] {
        let progress = session::Progress {
            id: 2,
            staged_bytes: total,
            total_bytes: total,
        };
        s.progress = Some(progress);
        s.results = vec![ResultReceipt {
            id: 2,
            outcome: Ok(worker::Completion::Written(progress)),
        }];
        let frame = Frame::receive(&s);
        assert_eq!(frame.progress, if total == 0 { -1 } else { 1000 });
        assert!(text(&frame.lines[2]).starts_with("Selection #1 staged:"));
        assert!(text(&frame.lines[4]).starts_with("Published locally: 0"));
        assert!(text(&frame.lines[7]).starts_with("Staged bytes do not prove publication"));
    }
}

#[test]
fn full_width_counts_are_exact_and_the_progress_bar_stays_bounded() {
    let mut s = status();
    for (staged, total, ratio) in [
        (u64::MAX, u64::MAX, 1000),
        (u64::MAX, 1, 1000),
        (u64::MAX / 2, u64::MAX, 499),
        (0, 1, 0),
    ] {
        s.progress = Some(session::Progress {
            id: u64::MAX,
            staged_bytes: staged,
            total_bytes: total,
        });
        let frame = Frame::receive(&s);
        assert_eq!(frame.progress, ratio);
        let row = text(&frame.lines[2]);
        assert!(row.contains(&(u64::MAX - 1).to_string()));
        assert!(row.contains(&staged.to_string()));
        assert!(row.contains(&total.to_string()));
        for row in &frame.lines {
            assert!(text(row).is_ascii());
        }
    }
}

#[test]
fn cancellation_and_failed_cleanup_preserve_partial_batch_publications() {
    let mut s = status();
    s.results = vec![
        published(1, 0, Publication::Durable),
        published(2, 55, Publication::DurabilityUnknown),
        ResultReceipt {
            id: 3,
            outcome: Err(worker::Error::Session(session::Error::Storage(
                fr_files::receive::Error::Conflict,
            ))),
        },
        ResultReceipt {
            id: 4,
            outcome: Err(worker::Error::UnknownEffect),
        },
        ResultReceipt {
            id: 5,
            outcome: Ok(worker::Completion::Cancelled),
        },
    ];
    s.phase = ReceivePhase::Ended;
    s.cancel_requested = true;
    s.absence = Some(Absence::ReceiveFailed(fr_files::quic::Error::Cancelled));
    for cleanup in [None, Some(Ok(())), Some(Err(worker::Error::Cleanup))] {
        s.cleanup = cleanup;
        let original = s.clone();
        let frame = Frame::snapshot(&Snapshot::Download(s.clone()));
        assert_eq!(frame.actions, 0);
        assert_eq!(
            text(&frame.lines[4]),
            "Published locally: 2 | Durability unknown: 1"
        );
        assert_eq!(
            text(&frame.lines[5]),
            "Refused: 1 | Cancelled: 1 | Publication unknown: 1"
        );
        assert!(text(&frame.lines[7]).starts_with("Download cancelled"));
        assert_eq!(s, original);
    }
}

#[test]
fn mismatched_receipts_and_unknown_effects_are_never_counted_as_publications() {
    let mut s = status();
    let mut mismatch = published(1, 55, Publication::Durable);
    mismatch.id = 2;
    s.results = vec![
        mismatch,
        published(0, 55, Publication::Durable),
        ResultReceipt {
            id: 3,
            outcome: Err(worker::Error::Poisoned),
        },
        ResultReceipt {
            id: 4,
            outcome: Err(worker::Error::Panicked),
        },
        ResultReceipt {
            id: 5,
            outcome: Err(worker::Error::Session(session::Error::UnknownEffect)),
        },
    ];
    s.progress = Some(session::Progress {
        id: 0,
        staged_bytes: 55,
        total_bytes: 55,
    });
    let frame = Frame::receive(&s);
    assert_eq!(frame.progress, -1);
    assert!(text(&frame.lines[2]).contains("invalid selection index"));
    assert!(text(&frame.lines[4]).starts_with("Published locally: 0"));
    assert!(text(&frame.lines[5]).ends_with("Publication unknown: 5"));
}

#[test]
fn omitted_results_remain_explicit_and_do_not_expand_the_rendering_bound() {
    let mut s = status();
    s.results = (1..=MAX_RECEIVE_RESULTS as u64 + 1)
        .map(|id| published(id, 1, Publication::Durable))
        .collect();
    let frame = Frame::receive(&s);
    assert!(text(&frame.lines[3]).contains("65 retained"));
    assert!(text(&frame.lines[3]).contains("missing results may have effects"));
    assert!(text(&frame.lines[4]).starts_with("Published locally: 65"));
    s.results.clear();
    s.results_truncated = true;
    assert!(text(&Frame::receive(&s).lines[3]).contains("missing results may have effects"));
}

#[test]
fn no_remote_download_capability_is_distinct_from_no_publication_yet() {
    let mut s = status();
    s.phase = ReceivePhase::Ended;
    s.absence = Some(Absence::NotNegotiated);
    let frame = Frame::receive(&s);
    assert_eq!(frame.actions, 0);
    assert_eq!(
        text(&frame.lines[7]),
        "The host did not enable the download capability"
    );
    assert!(text(&frame.lines[4]).starts_with("Published locally: 0"));
    assert_eq!(text(&frame.lines[6]), "Disk cleanup: not started");
}

#[test]
fn missing_cleanup_uses_original_owner_evidence_even_when_the_lane_has_ended() {
    let mut s = status();
    s.phase = ReceivePhase::Ended;
    s.absence = Some(Absence::ReceiveFailed(fr_files::quic::Error::Cancelled));
    assert_eq!(
        text(&Frame::receive(&s).lines[6]),
        "Disk cleanup: pending / not yet reported"
    );
    s.phase = ReceivePhase::WaitingForControl;
    s.absence = None;
    assert_eq!(
        text(&Frame::receive(&s).lines[6]),
        "Disk cleanup: not started"
    );
    s.results.push(published(1, 0, Publication::Durable));
    assert_eq!(
        text(&Frame::receive(&s).lines[6]),
        "Disk cleanup: pending / not yet reported"
    );
}

#[test]
fn standing_folder_job_shows_round_progress_conflicts_and_terminal_failure() {
    use fr_files::sync::{JobStatus, LiveError};
    use fr_wire::sync_job_state::{ConflictPolicy, SyncDirection, SyncJobState, SyncPhase};

    let mut s = status();
    s.results = vec![published(1, 8, Publication::Durable)];
    let mut job = JobStatus {
        state: SyncJobState {
            job_id: u64::MAX,
            sync_direction: SyncDirection::HostToController,
            conflict_policy: ConflictPolicy::KeepBoth,
            phase: SyncPhase::Transferring,
            reason: fr_wire::files::Reason::None,
            entries_total: 2,
            entries_completed: 1,
            bytes_total: 16,
            bytes_completed: 8,
            conflicts_kept: 1,
            manifest_revision: u64::MAX,
        },
        confirmed_publications: 1,
        confirmed_conflicts: 1,
        rounds_settled: u64::MAX - 1,
        failure: None,
        cleanup_complete: false,
    };
    s.folder_job = Some(job);
    let active = Frame::snapshot(&Snapshot::Download(s.clone()));
    assert!(active.permits(Action::Cancel));
    assert_eq!(
        text(&active.lines[0]),
        format!("Folder job {}: receiving changes", u64::MAX)
    );
    assert!(text(&active.lines[1]).contains(&(u64::MAX - 1).to_string()));
    assert_eq!(
        text(&active.lines[3]),
        "This round: 1 / 2 files published, 8 / 16 bytes"
    );
    assert!(text(&active.lines[4]).contains("Conflict copies kept: 1"));
    assert_eq!(active.progress, -1);

    job.state.phase = SyncPhase::Failed;
    job.failure = Some(LiveError::Expired);
    job.confirmed_publications = 3;
    s.folder_job = Some(job);
    s.cancel_requested = true;
    s.results_truncated = true;
    let ended = Frame::receive(&s);
    assert_eq!(ended.actions, 0);
    assert!(text(&ended.lines[1]).ends_with("Cancel requested"));
    assert!(text(&ended.lines[3]).contains("missing results may have effects"));
    assert!(text(&ended.lines[4]).starts_with("Published locally: 3 | Conflict copies kept: 1"));
    assert_eq!(
        text(&ended.lines[6]),
        "Disk cleanup: pending / not yet reported"
    );
    assert_eq!(text(&ended.lines[7]), "Folder job ended: sync_expired");
}

fn folder_status() -> ReceiveStatus {
    use fr_wire::sync_job_state::{ConflictPolicy, SyncDirection, SyncJobState, SyncPhase};

    let mut status = status();
    status.folder_job = Some(JobStatus {
        state: SyncJobState {
            job_id: 7,
            sync_direction: SyncDirection::HostToController,
            conflict_policy: ConflictPolicy::KeepBoth,
            phase: SyncPhase::Transferring,
            reason: fr_wire::files::Reason::None,
            entries_total: 32,
            entries_completed: 31,
            bytes_total: 32,
            bytes_completed: 31,
            conflicts_kept: 0,
            manifest_revision: 8,
        },
        confirmed_publications: 0,
        confirmed_conflicts: 0,
        rounds_settled: 7,
        failure: None,
        cleanup_complete: false,
    });
    status
}

#[test]
fn standing_folder_job_reports_the_full_folder_and_a_late_durability_receipt() {
    let mut s = folder_status();
    let count = fr_files::sync::live::MAX_SOURCE_ENTRIES;
    assert!(count as usize > MAX_RECEIVE_RESULTS);
    s.results = (1..=u64::from(count))
        .map(|id| {
            published(
                id,
                1,
                if id == u64::from(count) {
                    Publication::DurabilityUnknown
                } else {
                    Publication::Durable
                },
            )
        })
        .collect();
    // The final disk receipt can precede the controller job's observation.
    s.folder_job.as_mut().unwrap().confirmed_publications = count - 1;
    let original = s.clone();
    let frame = Frame::receive(&s);
    assert_eq!(
        text(&frame.lines[3]),
        "This round: 31 / 32 files published, 31 / 32 bytes"
    );
    assert_eq!(
        text(&frame.lines[4]),
        format!(
            "Published locally: {count} | Conflict copies kept: 0 | Retained durability unknown: 1"
        )
    );
    assert_eq!(
        text(&frame.lines[5]),
        "Refused: 0 | Cancelled: 0 | Publication unknown: 0"
    );
    assert_eq!(s, original);
}

#[test]
fn standing_folder_job_bounds_receipts_and_preserves_omission_evidence() {
    let mut s = folder_status();
    let transfers = fr_files::sync::live::MAX_SESSION_TRANSFERS;
    let capacity = transfers as usize + 1;
    assert_eq!(s.result_capacity(), capacity);
    s.results = (1..=u64::from(transfers))
        .map(|id| published(id, 1, Publication::Durable))
        .collect();
    s.results.push(ResultReceipt {
        id: u64::from(transfers) + 1,
        outcome: Err(worker::Error::Session(session::Error::Storage(
            fr_files::receive::Error::Conflict,
        ))),
    });
    let frame = Frame::receive(&s);
    assert!(!text(&frame.lines[3]).contains("omitted"));
    assert!(text(&frame.lines[4]).starts_with(&format!("Published locally: {transfers} |")));
    assert_eq!(
        text(&frame.lines[5]),
        "Refused: 1 | Cancelled: 0 | Publication unknown: 0"
    );

    // An oversized snapshot cannot expand the bounded rendering work or count
    // evidence outside the original owner's retained-result capacity.
    s.results.push(published(
        u64::from(transfers) + 2,
        1,
        Publication::DurabilityUnknown,
    ));
    let overflow = Frame::receive(&s);
    assert!(text(&overflow.lines[3]).contains(&format!("{capacity} retained")));
    assert!(text(&overflow.lines[3]).contains("missing results may have effects"));
    assert_eq!(text(&overflow.lines[4]), text(&frame.lines[4]));
    assert_eq!(text(&overflow.lines[5]), text(&frame.lines[5]));

    s.results.clear();
    s.results_truncated = true;
    let omitted = Frame::receive(&s);
    assert!(text(&omitted.lines[3]).contains("missing results may have effects"));
    assert!(text(&omitted.lines[4]).starts_with("Published locally: 0 |"));
}
