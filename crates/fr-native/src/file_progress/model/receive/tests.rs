use super::*;
use crate::file_progress::{Snapshot, model::COLUMNS};

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
