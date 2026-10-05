//! Completion projection only: fixtures never stand in for actual playback.
use super::*;

fn progress(report: Option<audio::Report>, granted: bool) -> Progress {
    Progress {
        control: Some(control::Counters {
            requested: true,
            granted,
            results: 7,
            submitted: 5,
            ..control::Counters::default()
        }),
        audio: report.map(|r| Rc::new(RefCell::new(r))),
        ..Progress::default()
    }
}

#[test]
fn control_completion_preserves_independent_audio_presence_and_submission() {
    for (report, requested, active, played, resets, absence) in [
        (None, false, false, 0, 0, None),
        (
            Some(audio::Report {
                absence: Some("host_did_not_offer"),
                ..audio::Report::default()
            }),
            true,
            false,
            0,
            0,
            Some("host_did_not_offer"),
        ),
        (
            Some(audio::Report {
                acknowledged: true,
                ..audio::Report::default()
            }),
            true,
            false,
            0,
            0,
            None,
        ),
        (
            Some(audio::Report {
                acknowledged: true,
                submitted: 19,
                resets: 2,
                absence: Some("local_output_failed"),
            }),
            true,
            true,
            19,
            2,
            Some("local_output_failed"),
        ),
    ] {
        let progress = progress(report, true);
        let json: serde_json::Value =
            serde_json::from_str(&completion(&progress, true, None)).unwrap();
        assert_eq!(json["role"], "control");
        assert_eq!(json["control_granted"], true);
        assert_eq!(json["input_results"], 7);
        assert_eq!(json["input_submitted_to_os"], 5);
        assert_eq!(json["audio_requested"], requested);
        assert_eq!(json["audio_active"], active);
        assert_eq!(json["audio_frames_submitted"], played);
        assert_eq!(json["audio_output_resets"], resets);
        assert_eq!(json["audio_absence"], serde_json::json!(absence));
        assert_eq!(json["audibility_proven"], false);
        assert_eq!(json["physical_visibility_proven"], false);
    }
}

#[test]
fn audio_readiness_is_not_a_control_grant_or_audibility_evidence() {
    let progress = progress(
        Some(audio::Report {
            acknowledged: true,
            submitted: 3,
            ..audio::Report::default()
        }),
        false,
    );
    let json: serde_json::Value =
        serde_json::from_str(&completion(&progress, true, None)).unwrap();
    assert_eq!(json["control_granted"], false);
    assert_eq!(json["audio_active"], true);
    assert_eq!(json["audibility_proven"], false);
    let text = completion(&progress, false, None);
    assert!(text.contains("requested but not granted"));
    assert!(text.contains("3 decoded frame(s) submitted"));
    assert!(text.contains("not audibility proof"));
}

#[test]
fn text_completion_names_audio_absence_without_losing_control_results() {
    let absent = progress(
        Some(audio::Report {
            absence: Some("host_did_not_offer"),
            ..audio::Report::default()
        }),
        true,
    );
    let text = completion(&absent, false, None);
    assert!(text.contains("Host audio absent: host_did_not_offer"));
    assert!(text.contains("7 host input result(s) (5 submitted"));
    let pending = progress(Some(audio::Report::default()), true);
    assert!(completion(&pending, false, None).contains("no playback confirmed"));
    let no_request = progress(None, true);
    assert!(!completion(&no_request, false, None).contains("Host audio"));
}
