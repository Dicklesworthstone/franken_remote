//! Content-free native audio completion fields, shared by control and observation.
use super::*;

#[test]
fn bounded_inbox_drops_are_separate_from_failures_and_playback_evidence() {
    let report = Rc::new(RefCell::new(audio::Report {
        acknowledged: true,
        submitted: 11,
        dropped_packets: 3,
        ..audio::Report::default()
    }));
    let progress = Progress {
        audio: Some(report.clone()),
        ..Progress::default()
    };
    let value: serde_json::Value = serde_json::from_str(&format!(
        "{{\"start\":true{}}}",
        audio_completion(&progress, true)
    ))
    .unwrap();
    assert_eq!(value["audio_output_dropped_packets"], 3);
    assert_eq!(value["audio_frames_submitted"], 11);
    assert_eq!(value["audio_output_failed_generations"], 0);
    assert_eq!(value["audio_output_expired_generations"], 0);
    assert_eq!(value["audio_output_recovered_generations"], 0);
    assert_eq!(value["audio_active"], true);
    assert_eq!(value["audibility_proven"], false);
    assert!(audio_completion(&progress, false).contains("3 packet(s) dropped at capacity"));
    report.borrow_mut().absence = Some("audio_output_late");
    report.borrow_mut().expired_generations = 1;
    assert!(audio_completion(&progress, false).contains("Host audio absent: audio_output_late"));
    assert!(audio_completion(&progress, false).contains("3 packet(s) dropped at capacity"));
}

#[test]
fn no_audio_means_zero_drops_and_saturated_counts_remain_valid_json() {
    let none = Progress::default();
    let value: serde_json::Value = serde_json::from_str(&format!(
        "{{\"start\":true{}}}",
        audio_completion(&none, true)
    ))
    .unwrap();
    assert_eq!(value["audio_output_dropped_packets"], 0);
    assert_eq!(value["audio_requested"], false);
    assert_eq!(audio_completion(&none, false), "");
    let progress = Progress {
        audio: Some(Rc::new(RefCell::new(audio::Report {
            dropped_packets: u64::MAX,
            ..audio::Report::default()
        }))),
        ..Progress::default()
    };
    let value: serde_json::Value = serde_json::from_str(&format!(
        "{{\"start\":true{}}}",
        audio_completion(&progress, true)
    ))
    .unwrap();
    assert_eq!(
        value["audio_output_dropped_packets"].as_u64(),
        Some(u64::MAX)
    );
    assert_eq!(value["audio_active"], false);
}
