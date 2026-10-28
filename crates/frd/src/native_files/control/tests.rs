//! Status and cancellation are local observations/intent, never grants or proofs.
use super::*;

#[test]
fn queued_bytes_and_unknown_sizes_never_fabricate_publication() {
    let control = SendControl::new();
    control.update(|s| s.phase = SendPhase::Sending);
    for (stage, size, queued) in [
        (Stage::Preparing, None, 0),
        (Stage::Streaming, Some(7), 3),
        (Stage::AwaitingProof, Some(7), 7),
        (Stage::AwaitingProof, Some(0), 0),
    ] {
        let progress = Progress {
            id: 1,
            queued_bytes: queued,
            total_bytes: size,
        };
        control.observe(None, Some(stage), Some(progress), None);
        let snapshot = control.snapshot();
        assert_eq!(snapshot.status.phase, SendPhase::Sending);
        assert!(snapshot.status.report.is_none());
        assert_eq!(snapshot.stage, Some(stage));
        assert_eq!(snapshot.current, Some(progress));
        assert!(snapshot.cleanup.is_none());
    }
}

#[test]
fn cloned_requests_are_monotonic_and_do_not_claim_cleanup() {
    let control = SendControl::new();
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let control = control.clone();
            std::thread::spawn(move || control.cancel())
        })
        .collect();
    let first = threads
        .into_iter()
        .map(|t| usize::from(t.join().unwrap()))
        .sum::<usize>();
    assert_eq!(first, 1);
    assert!(!control.cancel());
    let snapshot = control.snapshot();
    assert!(snapshot.cancel_requested);
    assert_eq!(snapshot.status.phase, SendPhase::WaitingForControl);
    assert_eq!(snapshot.cleanup, None);
    assert_eq!(snapshot.status.report, None);
}

#[test]
fn terminal_status_cannot_be_reactivated_by_a_stale_handle() {
    for phase in [
        SendPhase::SelectionAccepted,
        SendPhase::Finished,
        SendPhase::Ended,
    ] {
        let control = SendControl::new();
        control.update(|s| s.phase = phase);
        let before = control.snapshot();
        assert!(!control.cancel());
        assert_eq!(control.snapshot(), before);
    }
}

#[test]
fn disappearing_current_transfer_is_not_an_old_files_progress() {
    let control = SendControl::new();
    control.observe(
        None,
        Some(Stage::Streaming),
        Some(Progress {
            id: 1,
            queued_bytes: 4,
            total_bytes: Some(4),
        }),
        None,
    );
    control.observe(None, Some(Stage::Queued), None, None);
    assert!(control.snapshot().current.is_none());
    control.observe(
        None,
        Some(Stage::Preparing),
        Some(Progress {
            id: 2,
            queued_bytes: 0,
            total_bytes: None,
        }),
        None,
    );
    assert_eq!(control.snapshot().current.unwrap().id, 2);
    assert_eq!(control.snapshot().current.unwrap().total_bytes, None);
}

#[test]
fn pending_or_failed_cleanup_is_not_success_and_preserves_original_absence() {
    let control = SendControl::new();
    control.end(Absence::SetupFailed(Error::Expired));
    control.end(Absence::Refused(Error::Cancelled));
    assert_eq!(
        control.status().absence,
        Some(Absence::SetupFailed(Error::Expired))
    );
    control.cleaned(Some(Err(Error::Worker)));
    control.cleaned(None);
    assert_eq!(control.snapshot().cleanup, Some(Err(Error::Worker)));
    assert!(control.status().report.is_none());
}

#[test]
fn deferred_setup_cancel_and_first_selection_handoff_have_one_winner() {
    for cancelled_first in [false, true] {
        let control = SendControl::new();
        control.update(|status| status.phase = SendPhase::WaitingForSelection);
        assert!(!control.pause());
        assert!(!control.resume());
        if cancelled_first {
            assert!(control.cancel());
            assert!(!control.accept_selection());
            assert_eq!(control.status().phase, SendPhase::WaitingForSelection);
        } else {
            assert!(control.accept_selection());
            assert!(!control.cancel());
            assert_eq!(control.status().phase, SendPhase::SelectionAccepted);
        }
        assert!(!control.accept_selection());
        assert!(control.status().report.is_none());
        assert!(control.snapshot().current.is_none());
    }
    // These are the actual public UI cancellation and owner transition, raced
    // under their shared mutex. Both must never report success for one handle.
    for _ in 0..32 {
        let control = SendControl::new();
        control.update(|status| status.phase = SendPhase::WaitingForSelection);
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let cancel = control.clone();
        let ready = barrier.clone();
        let cancellation = std::thread::spawn(move || {
            ready.wait();
            cancel.cancel()
        });
        barrier.wait();
        let accepted = control.accept_selection();
        let cancelled = cancellation.join().unwrap();
        assert_ne!(accepted, cancelled);
        let snapshot = control.snapshot();
        assert_eq!(snapshot.cancel_requested, cancelled);
        assert_eq!(
            snapshot.status.phase,
            if accepted {
                SendPhase::SelectionAccepted
            } else {
                SendPhase::WaitingForSelection
            }
        );
        assert!(snapshot.status.report.is_none());
        assert!(snapshot.current.is_none());
    }
}
