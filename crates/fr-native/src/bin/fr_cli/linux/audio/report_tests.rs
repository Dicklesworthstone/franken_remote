//! Content-free reporting state; no fixture submission is native playback proof.
use super::{AudioRequest, Report};

#[test]
fn reconnect_keeps_latest_preference_but_old_handles_cannot_change_the_new_attempt() {
    let mut report = Report::for_request(&AudioRequest {
        server: None,
        sink: None,
        volume: 37,
        muted: false,
    });
    let mut original = report.begin_attempt();
    assert_eq!(original.volume().to_bits(), 0.37_f32.to_bits());
    original.set_volume(0.25);
    original.set_muted(true);
    report.controls = Some(Err(fr_native::file_progress::Error::InvalidParent));
    report.acknowledged = true;
    report.submitted = 1;
    assert!(report.active());

    let mut replacement = report.begin_attempt();
    assert_eq!(replacement.snapshot(), (0.25, true));
    assert!(report.controls.is_none());
    original.set_volume(1.0);
    original.set_muted(false);
    assert_eq!(replacement.snapshot(), (0.25, true));
    assert_eq!(report.volume.snapshot(), (0.25, true));
    replacement.set_volume(0.5);
    assert_eq!(report.volume.snapshot(), (0.5, true));
    assert!(!report.active(), "a preference never starts playback");
    assert!(!report.acknowledged);
    assert_eq!(report.submitted, 1, "retain prior submitted PCM accounting");
    assert_eq!(report.generation_start, 1);
}

#[test]
fn a_silent_replacement_cannot_recover_an_expired_generation() {
    let mut report = Report {
        acknowledged: true,
        submitted: 12,
        ..Report::default()
    };
    report.failed_generation("audio_output_late", true);
    assert_eq!(report.expired_generations, 1);
    assert_eq!(report.failed_generations, 0);
    assert_eq!(report.first_failure, None);
    assert!(!report.active(), "failed output is not active playback");
    report.submitted += 1;
    report.recovered();
    assert_eq!(
        report.recovered_generations, 0,
        "late accounting is not recovery"
    );
    report.begin_generation();
    report.recovered();
    assert!(!report.active());
    assert_eq!(report.absence, Some("audio_output_late"));
    assert_eq!(report.recovered_generations, 0);

    report.acknowledged = true;
    report.recovered();
    assert!(
        !report.active(),
        "old PCM is not playback by the new output"
    );
    assert_eq!(report.absence, Some("audio_output_late"));
    assert_eq!(report.recovered_generations, 0);

    report.submitted += 1;
    report.recovered();
    assert!(report.active());
    assert_eq!(report.absence, None);
    assert_eq!(report.expired_generations, 1);
    assert_eq!(report.recovered_generations, 1);
    report.recovered();
    assert_eq!(report.recovered_generations, 1);
}

#[test]
fn recovery_retains_real_failures_and_counts_each_interruption() {
    let mut report = Report::default();
    report.failed_generation("audio_output_failed", false);
    report.begin_generation();
    report.failed_generation("audio_output_late", true);
    report.begin_generation();
    report.failed_generation("audio_output_worker_stalled", false);
    assert_eq!(report.failed_generations, 2);
    assert_eq!(report.expired_generations, 1);
    assert_eq!(report.recovered_generations, 0);
    assert_eq!(report.first_failure, Some("audio_output_failed"));
    assert_eq!(report.absence, Some("audio_output_worker_stalled"));

    report.begin_generation();
    report.submitted += 1;
    report.recovered();
    assert_eq!(report.recovered_generations, 0, "submission needs its ACK");
    report.acknowledged = true;
    report.recovered();
    assert_eq!(report.absence, None);
    assert_eq!(report.recovered_generations, 3);
    assert_eq!(report.first_failure, Some("audio_output_failed"));
    assert_eq!(report.failed_generations, 2);
    assert_eq!(report.expired_generations, 1);
}

#[test]
fn an_unrecovered_new_expiry_does_not_erase_earlier_recovery() {
    let mut report = Report::default();
    report.failed_generation("audio_output_late", true);
    report.begin_generation();
    report.acknowledged = true;
    report.submitted += 1;
    report.recovered();
    report.failed_generation("audio_output_late", true);
    report.begin_generation();
    report.recovered();
    assert_eq!(report.expired_generations, 2);
    assert_eq!(report.recovered_generations, 1);
    assert_eq!(report.absence, Some("audio_output_late"));
}
