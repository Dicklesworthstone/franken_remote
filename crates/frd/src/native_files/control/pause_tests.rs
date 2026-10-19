//! UI intent is reversible; cancellation, publication and authority are not.
use super::*;

#[test]
fn pause_before_control_is_only_intent_and_clones_share_it() {
    let control = SendControl::new();
    assert!(!control.resume());
    assert!(control.pause());
    assert!(!control.clone().pause());
    let snapshot = control.snapshot();
    assert!(snapshot.pause_requested);
    assert_eq!(snapshot.paused, None);
    assert_eq!(snapshot.status.phase, SendPhase::WaitingForControl);
    assert!(snapshot.current.is_none() && snapshot.status.report.is_none());
    assert!(control.clone().resume());
    assert!(!control.pause_requested());
    assert_eq!(control.status(), snapshot.status);
}

#[test]
fn pause_acknowledgement_comes_from_owner_not_button_press() {
    let control = SendControl::new();
    control.update(|s| s.phase = SendPhase::Sending);
    assert!(control.pause());
    let progress = Some(Progress {
        id: 4,
        queued_bytes: 3,
        total_bytes: Some(9),
    });
    control.observe(None, Some(Stage::Streaming), progress, Some(false));
    assert_eq!(control.snapshot().paused, Some(false));
    control.observe(None, Some(Stage::Streaming), progress, Some(true));
    assert_eq!(control.snapshot().paused, Some(true));
    assert!(control.resume());
    let snapshot = control.snapshot();
    assert!(!snapshot.pause_requested);
    assert_eq!(
        snapshot.paused,
        Some(true),
        "last observation is not rewritten as an acknowledgement"
    );
    assert_eq!(snapshot.current, progress);
    assert!(snapshot.status.report.is_none());
    assert!(snapshot.cleanup.is_none());
}

#[test]
fn cancellation_and_terminal_status_win_over_resume() {
    let control = SendControl::new();
    assert!(control.pause());
    assert!(control.cancel());
    let snapshot = control.snapshot();
    assert!(!control.resume() && !control.pause());
    assert_eq!(snapshot, control.snapshot());
    for phase in [SendPhase::Finished, SendPhase::Ended] {
        let control = SendControl::new();
        control.pause();
        control.update(|s| s.phase = phase);
        let snapshot = control.snapshot();
        assert!(!control.resume() && !control.pause());
        assert_eq!(snapshot, control.snapshot());
    }
}

#[test]
fn concurrent_pause_requests_have_one_transition_and_no_fabricated_ack() {
    let control = SendControl::new();
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let control = control.clone();
            std::thread::spawn(move || control.pause())
        })
        .collect();
    assert_eq!(
        threads
            .into_iter()
            .map(|t| usize::from(t.join().unwrap()))
            .sum::<usize>(),
        1
    );
    assert_eq!(control.snapshot().paused, None);
    assert!(control.resume());
    assert!(!control.resume());
}
