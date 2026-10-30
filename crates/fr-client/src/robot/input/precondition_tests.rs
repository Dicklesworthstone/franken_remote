use super::*;

fn request() -> RobotInputRequest {
    RobotInputRequest {
        host: "private-workstation".into(),
        lease_handle: "private-lease".into(),
        request_id: "private-request".into(),
        precondition_geometry_generation: Some(7),
        max_observation_age_ms: Some(250),
        precondition_lease: Some("private-lease".into()),
        precondition_focus: Some(WindowFocusPrecondition {
            target_window: "private-document".into(),
            best_effort: true,
        }),
        actions: vec![RobotInputAction::Text {
            text: "private-typed-text".into(),
        }],
    }
}

fn evaluate(
    request: &RobotInputRequest,
    age: Option<u64>,
    focus: Option<&str>,
) -> Result<(), RobotError> {
    request.evaluate_preconditions(7, age, Some("private-lease"), focus)
}

#[test]
fn requested_age_and_focus_fail_closed_when_their_evidence_is_missing() {
    let request = request();
    assert_eq!(
        evaluate(&request, None, Some("private-document"))
            .unwrap_err()
            .code,
        "observation_age_unavailable"
    );
    assert_eq!(
        evaluate(&request, Some(200), None).unwrap_err().code,
        "focus_unavailable"
    );
    assert!(evaluate(&request, Some(250), Some("private-document - Editor")).is_ok());
    assert_eq!(
        evaluate(&request, Some(251), Some("private-document"))
            .unwrap_err()
            .code,
        "observation_expired"
    );
}

#[test]
fn absent_optional_preconditions_do_not_require_fabricated_evidence() {
    let mut request = request();
    request.max_observation_age_ms = None;
    request.precondition_focus = None;
    assert!(evaluate(&request, None, None).is_ok());
    assert_eq!(
        request
            .evaluate_preconditions(7, None, None, None)
            .unwrap_err()
            .code,
        "lease_invalid_or_expired"
    );
}

#[test]
fn unsupported_atomic_focus_and_empty_targets_are_not_silently_weakened() {
    let mut request = request();
    request.precondition_focus.as_mut().unwrap().best_effort = false;
    assert_eq!(
        evaluate(&request, Some(0), Some("private-document"))
            .unwrap_err()
            .code,
        "focus_precondition_unsupported"
    );
    let focus = request.precondition_focus.as_mut().unwrap();
    focus.best_effort = true;
    focus.target_window.clear();
    assert_eq!(
        evaluate(&request, Some(0), Some("anything"))
            .unwrap_err()
            .code,
        "focus_precondition_unsupported"
    );
}

#[test]
fn refusal_diagnostics_never_echo_lease_focus_or_action_content() {
    let mut request = request();
    let mut errors = vec![
        request
            .evaluate_preconditions(7, Some(1), Some("other-private-lease"), None)
            .unwrap_err(),
        evaluate(&request, Some(1), Some("unrelated-sensitive-window")).unwrap_err(),
        evaluate(&request, None, None).unwrap_err(),
    ];
    request.precondition_lease = Some("other-private-lease".into());
    errors.push(evaluate(&request, Some(1), Some("private-document")).unwrap_err());
    for error in errors {
        let rendered = serde_json::to_string(&error).unwrap();
        for forbidden in [
            "private-lease",
            "private-document",
            "private-typed-text",
            "private-request",
            "private-workstation",
        ] {
            assert!(
                !rendered.contains(forbidden),
                "sensitive value escaped in refusal"
            );
        }
    }
}
