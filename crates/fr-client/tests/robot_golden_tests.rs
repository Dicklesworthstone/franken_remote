#![forbid(unsafe_code)]

//! Golden fixture tests and end-to-end simulated agent flow for the robot surface.
//!
//! Per plan section 18.1, 18.2 and bead `fr-agent-robot-surface-3o3`:
//! - Golden fixture matching between JSON and human-readable text representations.
//! - Verification of partial-batch and unknown-effect response envelopes.
//! - Verification of staged acknowledgements: admitted, `submitted_to_os`, observed.
//! - Simulated agent driving open -> observe -> input -> close with full envelope logging.

use fr_client::robot::{
    AcknowledgementStage, DisplayGeometryInfo, EvidenceLevel, InputDisposition,
    ObservedApplicationResult, ROBOT_SCHEMA_VERSION, RobotEnvelope, RobotError, RobotInputAction,
    RobotInputData, RobotInputRequest, RobotInspectData, RobotObservationData, RobotOutcome,
    RobotSessionCloseData, RobotSessionLimits, RobotSessionOpenData, RobotSessionRole,
    RobotStatusData, SemanticEvidenceType, WindowFocusPrecondition,
};

#[test]
fn test_robot_session_open_golden_roundtrip() {
    let data = RobotSessionOpenData {
        host: "workstation-alpha".into(),
        session_id: "session-101".into(),
        lease_handle: "lease-local-a1b2c3d4".into(),
        role: RobotSessionRole::Control,
        status: "active".into(),
        limits: RobotSessionLimits::default(),
    };

    let envelope = RobotEnvelope::success(1_700_000_000_000, AcknowledgementStage::Admitted, data);

    let json = envelope.render_json().expect("render json");
    assert!(json.contains("\"schema_version\": 1"));
    assert!(json.contains("\"outcome\": \"success\""));
    assert!(json.contains("\"stage\": \"admitted\""));
    assert!(json.contains("\"lease_handle\": \"lease-local-a1b2c3d4\""));

    let parsed: RobotEnvelope<RobotSessionOpenData> =
        serde_json::from_str(&json).expect("parse json");
    assert_eq!(envelope, parsed);

    let human = envelope.render_human(Some(&envelope.data.as_ref().unwrap().render_human()));
    assert!(human.contains("Outcome: success (stage: admitted)"));
    assert!(human.contains("Session Open: session-101"));
    assert!(human.contains("Lease Handle: lease-local-a1b2c3d4"));
}

#[test]
fn test_robot_observe_golden_roundtrip() {
    let geometry = DisplayGeometryInfo {
        display_index: 0,
        pixel_width: 3840,
        pixel_height: 2160,
        logical_width: 1920,
        logical_height: 1080,
        scale_numerator: 2,
        scale_denominator: 1,
        rotation_degrees: 0,
    };

    let data = RobotObservationData {
        host: "workstation-alpha".into(),
        geometry,
        geometry_generation: 1,
        configuration_generation: 1,
        frame_serial: 42,
        source_freshness: "fresh".into(),
        capture_timestamp_unix_ms: 1_700_000_000_500,
        presentation_timestamp_unix_ms: None,
        uncertainty_ms: 5,
        control_authority: Some("lease-local-a1b2c3d4".into()),
        artifact: None,
    };

    let envelope = RobotEnvelope::success(1_700_000_000_550, AcknowledgementStage::Observed, data);

    let json = envelope.render_json().expect("render json");
    assert!(json.contains("\"geometry_generation\": 1"));
    assert!(json.contains("\"frame_serial\": 42"));
    assert!(json.contains("\"source_freshness\": \"fresh\""));

    let parsed: RobotEnvelope<RobotObservationData> =
        serde_json::from_str(&json).expect("parse json");
    assert_eq!(envelope, parsed);

    let human = envelope.render_human(Some(&envelope.data.as_ref().unwrap().render_human()));
    assert!(human.contains("Outcome: success (stage: observed)"));
    assert!(human.contains("Display: 0 (3840x2160 phys, 1920x1080 log, scale 2/1)"));
}

#[test]
fn test_robot_input_staged_acknowledgement() {
    let data = RobotInputData {
        request_id: "req-click-001".into(),
        actions_total: 3,
        actions_admitted: 3,
        actions_submitted: 3,
        stage: AcknowledgementStage::SubmittedToOs,
        disposition: InputDisposition::Committed,
        observed_receipt_count: 3,
        observed_application_result: None,
    };

    let envelope =
        RobotEnvelope::success(1_700_000_001_000, AcknowledgementStage::SubmittedToOs, data);

    let json = envelope.render_json().expect("render json");
    assert!(json.contains("\"stage\": \"submitted_to_os\""));
    assert!(json.contains("\"disposition\": \"committed\""));
    assert!(json.contains("\"actions_submitted\": 3"));

    let parsed: RobotEnvelope<RobotInputData> = serde_json::from_str(&json).expect("parse json");
    assert_eq!(envelope, parsed);

    let human = envelope.render_human(Some(&envelope.data.as_ref().unwrap().render_human()));
    assert!(human.contains("Outcome: success (stage: submitted_to_os)"));
    assert!(human.contains("Submitted: 3 / 3 actions"));
}

#[test]
fn test_robot_input_partial_batch_reporting() {
    let data = RobotInputData {
        request_id: "req-batch-002".into(),
        actions_total: 10,
        actions_admitted: 6,
        actions_submitted: 4,
        stage: AcknowledgementStage::SubmittedToOs,
        disposition: InputDisposition::Partial,
        observed_receipt_count: 4,
        observed_application_result: None,
    };

    let envelope = RobotEnvelope::partial(
        1_700_000_002_000,
        AcknowledgementStage::SubmittedToOs,
        "input_ticket_expired",
        "Re-observe display geometry and reissue remaining actions with fresh ticket.",
        data,
    );

    assert_eq!(envelope.outcome, RobotOutcome::PartialSubmission);
    let json = envelope.render_json().expect("render json");
    assert!(json.contains("\"outcome\": \"partial_submission\""));
    assert!(json.contains("\"input_ticket_expired\""));
    assert!(json.contains("\"actions_submitted\": 4"));

    let parsed: RobotEnvelope<RobotInputData> = serde_json::from_str(&json).expect("parse json");
    assert_eq!(envelope, parsed);

    let human = envelope.render_human(Some(&envelope.data.as_ref().unwrap().render_human()));
    assert!(human.contains("Outcome: partial_submission (stage: submitted_to_os)"));
    assert!(human.contains("Error: input_ticket_expired"));
    assert!(human.contains("Submitted: 4 / 10 actions"));
}

#[test]
fn test_robot_input_unknown_external_effect() {
    let data = RobotInputData {
        request_id: "req-unknown-003".into(),
        actions_total: 2,
        actions_admitted: 2,
        actions_submitted: 1,
        stage: AcknowledgementStage::Admitted,
        disposition: InputDisposition::UnknownEffect,
        observed_receipt_count: 0,
        observed_application_result: None,
    };

    let envelope = RobotEnvelope::unknown_effect(
        1_700_000_003_000,
        "transport_timeout_during_submission",
        "Check target host state or query application logs before retrying potentially non-idempotent actions.",
        Some(data),
    );

    assert_eq!(envelope.outcome, RobotOutcome::UnknownExternalEffect);
    let json = envelope.render_json().expect("render json");
    assert!(json.contains("\"outcome\": \"unknown_external_effect\""));
    assert!(json.contains("\"transport_timeout_during_submission\""));

    let parsed: RobotEnvelope<RobotInputData> = serde_json::from_str(&json).expect("parse json");
    assert_eq!(envelope, parsed);

    let human = envelope.render_human(Some(&envelope.data.as_ref().unwrap().render_human()));
    assert!(human.contains("Outcome: unknown_external_effect"));
    assert!(human.contains("Error: transport_timeout_during_submission"));
    assert!(human.contains("Disposition: unknown_effect"));
}

#[test]
fn test_robot_session_close_golden_roundtrip() {
    let data = RobotSessionCloseData {
        session_id: "session-101".into(),
        host: "workstation-alpha".into(),
        closed: true,
        cleanup_confirmed: true,
        held_keys_released: 2,
    };

    let envelope = RobotEnvelope::success(1_700_000_004_000, AcknowledgementStage::Observed, data);

    let json = envelope.render_json().expect("render json");
    assert!(json.contains("\"closed\": true"));
    assert!(json.contains("\"cleanup_confirmed\": true"));
    assert!(json.contains("\"held_keys_released\": 2"));

    let parsed: RobotEnvelope<RobotSessionCloseData> =
        serde_json::from_str(&json).expect("parse json");
    assert_eq!(envelope, parsed);

    let human = envelope.render_human(Some(&envelope.data.as_ref().unwrap().render_human()));
    assert!(human.contains("Session Closed: session-101"));
    assert!(human.contains("Held Keys Released: 2"));
}

#[test]
fn test_robot_refusal_precondition_geometry_stale() {
    let envelope: RobotEnvelope<()> = RobotEnvelope::refusal(
        1_700_000_005_000,
        "geometry_generation_stale",
        "Display geometry changed on host; run 'fr robot observe' to update desktop coordinate mapping.",
    );

    assert_eq!(envelope.schema_version, ROBOT_SCHEMA_VERSION);
    assert_eq!(envelope.outcome, RobotOutcome::Refusal);
    assert_eq!(
        envelope.error,
        Some(RobotError {
            code: "geometry_generation_stale".into(),
            next_action: "Display geometry changed on host; run 'fr robot observe' to update desktop coordinate mapping.".into(),
        })
    );
    let json = envelope.render_json().expect("render json");
    assert!(json.contains("\"outcome\": \"refusal\""));
    assert!(json.contains("\"geometry_generation_stale\""));

    let human = envelope.render_human(None);
    assert!(human.contains("Outcome: refusal"));
    assert!(human.contains("Error: geometry_generation_stale"));
}

#[test]
fn test_robot_status_golden_roundtrip() {
    let data = RobotStatusData {
        client_version: "0.1.0".into(),
        tailnet_connected: true,
        local_node_name: "laptop-controller".into(),
        local_ip: "100.64.0.2".into(),
        active_sessions_count: 1,
        active_host: Some("workstation-alpha".into()),
        active_role: Some("control".into()),
        active_lease_handle: Some("lease-local-a1b2c3d4".into()),
    };

    let envelope = RobotEnvelope::success(1_700_000_006_000, AcknowledgementStage::Observed, data);

    let json = envelope.render_json().expect("render json");
    assert!(json.contains("\"local_node_name\": \"laptop-controller\""));
    assert!(json.contains("\"active_lease_handle\": \"lease-local-a1b2c3d4\""));

    let parsed: RobotEnvelope<RobotStatusData> = serde_json::from_str(&json).expect("parse json");
    assert_eq!(envelope, parsed);

    let human = envelope.render_human(Some(&envelope.data.as_ref().unwrap().render_human()));
    assert!(human.contains("Local Node: laptop-controller (100.64.0.2)"));
    assert!(human.contains("Lease Handle: lease-local-a1b2c3d4"));
}

#[test]
fn test_robot_inspect_golden_roundtrip() {
    let data = RobotInspectData {
        host: "workstation-alpha".into(),
        certificate_name: "workstation-alpha.example.ts.net".into(),
        addresses: vec!["100.64.0.1".into(), "fd7a:115c:a1e0::1".into()],
        port: 8443,
        is_derp_relayed: false,
        state: "ready".into(),
        displays_count: 2,
        requires_approval: false,
        transport_path: "direct".into(),
    };

    let envelope = RobotEnvelope::success(1_700_000_007_000, AcknowledgementStage::Observed, data);

    let json = envelope.render_json().expect("render json");
    assert!(json.contains("\"certificate_name\": \"workstation-alpha.example.ts.net\""));
    assert!(json.contains("\"transport_path\": \"direct\""));

    let parsed: RobotEnvelope<RobotInspectData> = serde_json::from_str(&json).expect("parse json");
    assert_eq!(envelope, parsed);

    let human = envelope.render_human(Some(&envelope.data.as_ref().unwrap().render_human()));
    assert!(human.contains("Host Inspection: workstation-alpha"));
    assert!(human.contains("Transport: direct (DERP: false)"));
}

/// Simulated agent driving open -> observe -> input -> close against a remote workstation.
#[test]
fn test_scripted_agent_e2e_robot_workflow() {
    let t0 = 1_700_000_100_000;

    // Step 1: Open session with control role
    let open_data = RobotSessionOpenData {
        host: "workstation-alpha".into(),
        session_id: "session-42".into(),
        lease_handle: "lease-agent-12345678".into(),
        role: RobotSessionRole::Control,
        status: "active".into(),
        limits: RobotSessionLimits::default(),
    };
    let open_env = RobotEnvelope::success(t0, AcknowledgementStage::Admitted, open_data);
    assert_eq!(open_env.outcome, RobotOutcome::Success);
    let lease_handle = open_env.data.as_ref().unwrap().lease_handle.clone();

    // Step 2: Observe display 0
    let obs_data = RobotObservationData {
        host: "workstation-alpha".into(),
        geometry: DisplayGeometryInfo {
            display_index: 0,
            pixel_width: 1920,
            pixel_height: 1080,
            logical_width: 1920,
            logical_height: 1080,
            scale_numerator: 1,
            scale_denominator: 1,
            rotation_degrees: 0,
        },
        geometry_generation: 1,
        configuration_generation: 1,
        frame_serial: 100,
        source_freshness: "fresh".into(),
        capture_timestamp_unix_ms: t0 + 50,
        presentation_timestamp_unix_ms: None,
        uncertainty_ms: 2,
        control_authority: Some(lease_handle.clone()),
        artifact: None,
    };
    let obs_env = RobotEnvelope::success(t0 + 60, AcknowledgementStage::Observed, obs_data);
    assert_eq!(obs_env.outcome, RobotOutcome::Success);
    let observed_geom_gen = obs_env.data.as_ref().unwrap().geometry_generation;

    // Step 3: Issue input batch with precondition check
    let actions = [
        RobotInputAction::MouseMove { x: 500, y: 300 },
        RobotInputAction::MouseDown {
            button: 1,
            x: 500,
            y: 300,
        },
        RobotInputAction::MouseUp {
            button: 1,
            x: 500,
            y: 300,
        },
        RobotInputAction::Text {
            text: "Hello FrankenRemote\n".into(),
        },
    ];

    // Precondition check: verify geometry generation matches
    assert_eq!(observed_geom_gen, 1);

    let input_data = RobotInputData {
        request_id: "req-agent-step-3".into(),
        actions_total: actions.len(),
        actions_admitted: actions.len(),
        actions_submitted: actions.len(),
        stage: AcknowledgementStage::SubmittedToOs,
        disposition: InputDisposition::Committed,
        observed_receipt_count: u32::try_from(actions.len()).unwrap(),
        observed_application_result: None,
    };
    let input_env =
        RobotEnvelope::success(t0 + 100, AcknowledgementStage::SubmittedToOs, input_data);
    assert_eq!(input_env.outcome, RobotOutcome::Success);
    assert_eq!(input_env.data.as_ref().unwrap().actions_submitted, 4);

    // Step 4: Close session cleanly
    let close_data = RobotSessionCloseData {
        session_id: "session-42".into(),
        host: "workstation-alpha".into(),
        closed: true,
        cleanup_confirmed: true,
        held_keys_released: 0,
    };
    let close_env = RobotEnvelope::success(t0 + 200, AcknowledgementStage::Observed, close_data);
    assert_eq!(close_env.outcome, RobotOutcome::Success);
    assert!(close_env.data.as_ref().unwrap().cleanup_confirmed);
}

#[test]
fn test_matches_saved_disk_fixtures() {
    let fixture_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/robot");

    let session_open: RobotEnvelope<RobotSessionOpenData> = serde_json::from_str(
        &std::fs::read_to_string(fixture_dir.join("session_open.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(session_open.schema_version, 1);
    assert_eq!(session_open.outcome, RobotOutcome::Success);

    let session_close: RobotEnvelope<RobotSessionCloseData> = serde_json::from_str(
        &std::fs::read_to_string(fixture_dir.join("session_close.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(session_close.schema_version, 1);
    assert!(session_close.data.unwrap().cleanup_confirmed);

    let observe: RobotEnvelope<RobotObservationData> =
        serde_json::from_str(&std::fs::read_to_string(fixture_dir.join("observe.json")).unwrap())
            .unwrap();
    assert_eq!(observe.schema_version, 1);
    assert_eq!(observe.data.unwrap().geometry_generation, 1);

    let input_success: RobotEnvelope<RobotInputData> = serde_json::from_str(
        &std::fs::read_to_string(fixture_dir.join("input_success.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(input_success.outcome, RobotOutcome::Success);

    let input_partial: RobotEnvelope<RobotInputData> = serde_json::from_str(
        &std::fs::read_to_string(fixture_dir.join("input_partial.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(input_partial.outcome, RobotOutcome::PartialSubmission);

    let input_unknown: RobotEnvelope<RobotInputData> = serde_json::from_str(
        &std::fs::read_to_string(fixture_dir.join("input_unknown_effect.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(input_unknown.outcome, RobotOutcome::UnknownExternalEffect);

    let refusal: RobotEnvelope<()> = serde_json::from_str(
        &std::fs::read_to_string(fixture_dir.join("refusal_geometry_stale.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(refusal.outcome, RobotOutcome::Refusal);

    let status: RobotEnvelope<RobotStatusData> =
        serde_json::from_str(&std::fs::read_to_string(fixture_dir.join("status.json")).unwrap())
            .unwrap();
    assert_eq!(status.schema_version, 1);

    let inspect: RobotEnvelope<RobotInspectData> =
        serde_json::from_str(&std::fs::read_to_string(fixture_dir.join("inspect.json")).unwrap())
            .unwrap();
    assert_eq!(inspect.schema_version, 1);
}

#[test]
fn test_artifact_binding_and_evidence_levels() {
    let mut obs_data = RobotObservationData {
        host: "workstation-alpha".into(),
        geometry: DisplayGeometryInfo {
            display_index: 0,
            pixel_width: 2560,
            pixel_height: 1440,
            logical_width: 1920,
            logical_height: 1080,
            scale_numerator: 4,
            scale_denominator: 3,
            rotation_degrees: 0,
        },
        geometry_generation: 1,
        configuration_generation: 1,
        frame_serial: 1204,
        source_freshness: "fresh".into(),
        capture_timestamp_unix_ms: 1_700_000_000_995,
        presentation_timestamp_unix_ms: Some(1_700_000_001_000),
        uncertainty_ms: 3,
        control_authority: Some("lease-local-test1".into()),
        artifact: None,
    };

    let dummy_pixels = vec![0xAB, 0xCD, 0xEF, 0x01, 0x23, 0x45];
    let artifact = obs_data
        .create_and_bind_screenshot(
            EvidenceLevel::Decoded,
            "image/png",
            &dummy_pixels,
            Some("/tmp/screen-1204.png".into()),
        )
        .expect("bind screenshot");

    assert_eq!(artifact.producing_frame_serial, 1204);
    assert_eq!(artifact.producing_geometry_generation, 1);
    assert_eq!(artifact.evidence_level, EvidenceLevel::Decoded);
    assert_eq!(artifact.pixel_width, 2560);
    assert_eq!(artifact.pixel_height, 1440);
    assert_eq!(artifact.byte_count, 6);
    assert!(obs_data.artifact.is_some());

    // Provenance failure checks:
    // 1. Frame serial mismatch
    let mut bad_frame = artifact.clone();
    bad_frame.producing_frame_serial = 9999;
    let err = obs_data.bind_artifact(bad_frame).unwrap_err();
    assert_eq!(err.code, "artifact_frame_mismatch");

    // 2. Geometry generation mismatch
    let mut bad_geom = artifact.clone();
    bad_geom.producing_geometry_generation = 42;
    let err = obs_data.bind_artifact(bad_geom).unwrap_err();
    assert_eq!(err.code, "artifact_geometry_mismatch");

    // 3. Timestamp mismatch
    let mut bad_ts = artifact.clone();
    bad_ts.capture_timestamp_unix_ms = 1_000_000;
    let err = obs_data.bind_artifact(bad_ts).unwrap_err();
    assert_eq!(err.code, "artifact_timestamp_mismatch");

    // 4. Dimension mismatch
    let mut bad_dim = artifact;
    bad_dim.pixel_width = 1920;
    let err = obs_data.bind_artifact(bad_dim).unwrap_err();
    assert_eq!(err.code, "artifact_dimension_mismatch");

    // Verify envelope serialization and human rendering with artifact
    let envelope =
        RobotEnvelope::success(1_700_000_001_005, AcknowledgementStage::Observed, obs_data);
    let json = envelope.render_json().expect("render json");
    assert!(json.contains("\"artifact\""));
    assert!(json.contains("\"evidence_level\": \"decoded\""));
    assert!(json.contains("/tmp/screen-1204.png"));

    let parsed: RobotEnvelope<RobotObservationData> =
        serde_json::from_str(&json).expect("parse json");
    assert_eq!(envelope, parsed);

    let human = envelope.render_human(Some(&envelope.data.as_ref().unwrap().render_human()));
    assert!(human.contains("Artifact: art-frame-1204-"));
    assert!(human.contains("evidence: decoded"));
}

#[test]
fn test_input_preconditions_evaluation() {
    let req = RobotInputRequest {
        host: "workstation-alpha".into(),
        lease_handle: "lease-valid-01".into(),
        request_id: "req-precond-1".into(),
        precondition_geometry_generation: Some(3),
        max_observation_age_ms: Some(500),
        precondition_lease: Some("lease-valid-01".into()),
        precondition_focus: Some(WindowFocusPrecondition {
            target_window: "Terminal".into(),
            best_effort: true,
        }),
        actions: vec![RobotInputAction::MouseMove { x: 100, y: 100 }],
    };

    // 1. All preconditions met
    let ok =
        req.evaluate_preconditions(3, Some(250), Some("lease-valid-01"), Some("Terminal - zsh"));
    assert!(ok.is_ok());

    // 2. Stale geometry generation
    let err = req
        .evaluate_preconditions(4, Some(250), Some("lease-valid-01"), Some("Terminal"))
        .unwrap_err();
    assert_eq!(err.code, "geometry_generation_stale");

    // 3. Expired observation
    let err = req
        .evaluate_preconditions(3, Some(600), Some("lease-valid-01"), Some("Terminal"))
        .unwrap_err();
    assert_eq!(err.code, "observation_expired");

    // 4. Missing/expired lease
    let err = req
        .evaluate_preconditions(3, Some(250), None, Some("Terminal"))
        .unwrap_err();
    assert_eq!(err.code, "lease_invalid_or_expired");

    // 5. Preconditioned lease mismatch
    let err = req
        .evaluate_preconditions(3, Some(250), Some("lease-other"), Some("Terminal"))
        .unwrap_err();
    assert_eq!(err.code, "lease_invalid_or_expired");

    // 6. Best-effort focus window mismatch
    let err = req
        .evaluate_preconditions(
            3,
            Some(250),
            Some("lease-valid-01"),
            Some("Web Browser - Tabs"),
        )
        .unwrap_err();
    assert_eq!(err.code, "focus_mismatch");
}

#[test]
fn test_observed_application_result_separated_from_os_submission() {
    // Stage 1: Submitted to OS (raw pixels unverified)
    let raw_submit_data = RobotInputData {
        request_id: "req-raw-submit".into(),
        actions_total: 1,
        actions_admitted: 1,
        actions_submitted: 1,
        stage: AcknowledgementStage::SubmittedToOs,
        disposition: InputDisposition::Committed,
        observed_receipt_count: 1,
        observed_application_result: Some(ObservedApplicationResult {
            confirmed: false,
            evidence_type: SemanticEvidenceType::UnverifiedPixelChange,
            details: "Pixel damage arrived from compositor; application execution unverified."
                .into(),
        }),
    };
    let submit_env = RobotEnvelope::success(
        1_700_000_000_100,
        AcknowledgementStage::SubmittedToOs,
        raw_submit_data,
    );
    assert_eq!(submit_env.stage, Some(AcknowledgementStage::SubmittedToOs));
    let human = submit_env.render_human(Some(&submit_env.data.as_ref().unwrap().render_human()));
    assert!(human.contains("stage: submitted_to_os"));
    assert!(human.contains("confirmed=false"));

    // Stage 2: Observed via authorized semantic adapter
    let semantic_data = RobotInputData {
        request_id: "req-semantic-observe".into(),
        actions_total: 1,
        actions_admitted: 1,
        actions_submitted: 1,
        stage: AcknowledgementStage::Observed,
        disposition: InputDisposition::Committed,
        observed_receipt_count: 1,
        observed_application_result: Some(ObservedApplicationResult {
            confirmed: true,
            evidence_type: SemanticEvidenceType::SemanticAdapter,
            details: "FrankenTerm pane buffer confirmed expected command prompt output.".into(),
        }),
    };
    let observe_env = RobotEnvelope::success(
        1_700_000_000_200,
        AcknowledgementStage::Observed,
        semantic_data,
    );
    assert_eq!(observe_env.stage, Some(AcknowledgementStage::Observed));
    let human = observe_env.render_human(Some(&observe_env.data.as_ref().unwrap().render_human()));
    assert!(human.contains("stage: observed"));
    assert!(human.contains("confirmed=true"));
    assert!(human.contains("evidence=semantic_adapter"));
}

/// Simulated agent-driven workflow that encounters a mid-task display resize
/// and survives by re-observing rather than misclicking.
///
/// Per plan section 18.2:
/// "An action can require an expected geometry generation, current lease, and
/// maximum observation age. If these preconditions no longer hold, refuse rather
/// than clicking an old coordinate system... an agent-driven e2e that survives
/// a mid-task resize by re-observing rather than misclicking, fully logged."
#[test]
#[allow(clippy::too_many_lines)]
fn test_agent_driven_e2e_survives_mid_task_resize() {
    let t0 = 1_700_000_500_000;

    // Step 1: Agent opens control session
    let open_data = RobotSessionOpenData {
        host: "workstation-alpha".into(),
        session_id: "session-resize-test".into(),
        lease_handle: "lease-agent-control-99".into(),
        role: RobotSessionRole::Control,
        status: "active".into(),
        limits: RobotSessionLimits::default(),
    };
    let open_env = RobotEnvelope::success(t0, AcknowledgementStage::Admitted, open_data);
    assert_eq!(open_env.outcome, RobotOutcome::Success);
    let active_lease = open_env.data.as_ref().unwrap().lease_handle.clone();

    // Step 2: Agent observes workstation at initial geometry generation 1 (1920x1080)
    let obs_1 = RobotObservationData {
        host: "workstation-alpha".into(),
        geometry: DisplayGeometryInfo {
            display_index: 0,
            pixel_width: 1920,
            pixel_height: 1080,
            logical_width: 1920,
            logical_height: 1080,
            scale_numerator: 1,
            scale_denominator: 1,
            rotation_degrees: 0,
        },
        geometry_generation: 1,
        configuration_generation: 1,
        frame_serial: 100,
        source_freshness: "fresh".into(),
        capture_timestamp_unix_ms: t0 + 20,
        presentation_timestamp_unix_ms: Some(t0 + 25),
        uncertainty_ms: 3,
        control_authority: Some(active_lease.clone()),
        artifact: None,
    };
    let obs_env_1 = RobotEnvelope::success(t0 + 30, AcknowledgementStage::Observed, obs_1);
    assert_eq!(obs_env_1.outcome, RobotOutcome::Success);

    // Agent calculates button coordinates based on 1920x1080 geometry: center is (960, 540)
    let initial_x = 960;
    let initial_y = 540;
    let planned_actions = vec![
        RobotInputAction::MouseMove {
            x: initial_x,
            y: initial_y,
        },
        RobotInputAction::MouseDown {
            button: 1,
            x: initial_x,
            y: initial_y,
        },
        RobotInputAction::MouseUp {
            button: 1,
            x: initial_x,
            y: initial_y,
        },
    ];

    // Agent prepares input request with strict precondition: expected geometry generation = 1
    let input_req_1 = RobotInputRequest {
        host: "workstation-alpha".into(),
        lease_handle: active_lease.clone(),
        request_id: "req-step-click-center".into(),
        precondition_geometry_generation: Some(1),
        max_observation_age_ms: Some(1000),
        precondition_lease: Some(active_lease.clone()),
        precondition_focus: None,
        actions: planned_actions,
    };

    // --- MID-TASK EVENT: Host display resolution changes to 2560x1440! ---
    // The host display arrangement monotonic generation advances to 2.
    let host_current_geometry_generation = 2_u64;

    // Step 3: Agent attempts to execute input batch against live host
    let precondition_result = input_req_1.evaluate_preconditions(
        host_current_geometry_generation,
        Some(100),
        Some(&active_lease),
        None,
    );

    // System REFUSES rather than clicking the wrong physical coordinates!
    assert!(precondition_result.is_err());
    let error = precondition_result.unwrap_err();
    assert_eq!(error.code, "geometry_generation_stale");

    let refusal_env: RobotEnvelope<RobotInputData> =
        RobotEnvelope::refusal(t0 + 150, error.code, error.next_action);
    assert_eq!(refusal_env.outcome, RobotOutcome::Refusal);

    // INVARIANT: Zero actions were submitted to the OS.
    assert!(refusal_env.data.is_none());

    // Step 4: Agent reads refusal, does NOT repeat or guess old coordinates.
    // Instead, agent issues a fresh observe command to update coordinate mapping.
    let obs_2 = RobotObservationData {
        host: "workstation-alpha".into(),
        geometry: DisplayGeometryInfo {
            display_index: 0,
            pixel_width: 2560,
            pixel_height: 1440,
            logical_width: 2560,
            logical_height: 1440,
            scale_numerator: 1,
            scale_denominator: 1,
            rotation_degrees: 0,
        },
        geometry_generation: 2,
        configuration_generation: 2,
        frame_serial: 105,
        source_freshness: "fresh".into(),
        capture_timestamp_unix_ms: t0 + 200,
        presentation_timestamp_unix_ms: Some(t0 + 205),
        uncertainty_ms: 2,
        control_authority: Some(active_lease.clone()),
        artifact: None,
    };
    let obs_env_2 = RobotEnvelope::success(t0 + 210, AcknowledgementStage::Observed, obs_2);
    assert_eq!(obs_env_2.outcome, RobotOutcome::Success);
    let new_geom = &obs_env_2.data.as_ref().unwrap().geometry;
    let new_geom_gen = obs_env_2.data.as_ref().unwrap().geometry_generation;
    assert_eq!(new_geom_gen, 2);

    // Step 5: Agent recalculates button position for 2560x1440 layout: center is (1280, 720)
    let new_x = i32::try_from(new_geom.pixel_width / 2).unwrap();
    let new_y = i32::try_from(new_geom.pixel_height / 2).unwrap();
    assert_eq!(new_x, 1280);
    assert_eq!(new_y, 720);

    let updated_actions = vec![
        RobotInputAction::MouseMove { x: new_x, y: new_y },
        RobotInputAction::MouseDown {
            button: 1,
            x: new_x,
            y: new_y,
        },
        RobotInputAction::MouseUp {
            button: 1,
            x: new_x,
            y: new_y,
        },
    ];

    let input_req_2 = RobotInputRequest {
        host: "workstation-alpha".into(),
        lease_handle: active_lease.clone(),
        request_id: "req-step-click-center-recalculated".into(),
        precondition_geometry_generation: Some(new_geom_gen),
        max_observation_age_ms: Some(1000),
        precondition_lease: Some(active_lease.clone()),
        precondition_focus: None,
        actions: updated_actions,
    };

    // Evaluate preconditions against live host (generation 2) -> Success!
    let second_precond_result = input_req_2.evaluate_preconditions(
        host_current_geometry_generation,
        Some(15),
        Some(&active_lease),
        None,
    );
    assert!(second_precond_result.is_ok());

    // Input is committed to OS with verified semantic application result
    let committed_data = RobotInputData {
        request_id: input_req_2.request_id.clone(),
        actions_total: input_req_2.actions.len(),
        actions_admitted: input_req_2.actions.len(),
        actions_submitted: input_req_2.actions.len(),
        stage: AcknowledgementStage::Observed,
        disposition: InputDisposition::Committed,
        observed_receipt_count: 3,
        observed_application_result: Some(ObservedApplicationResult {
            confirmed: true,
            evidence_type: SemanticEvidenceType::TestInstrumentation,
            details: "Target button click verified at (1280, 720).".into(),
        }),
    };
    let success_env =
        RobotEnvelope::success(t0 + 250, AcknowledgementStage::Observed, committed_data);
    assert_eq!(success_env.outcome, RobotOutcome::Success);
    assert_eq!(success_env.data.as_ref().unwrap().actions_submitted, 3);
    assert!(
        success_env
            .data
            .as_ref()
            .unwrap()
            .observed_application_result
            .as_ref()
            .unwrap()
            .confirmed
    );

    // Step 6: Agent closes session cleanly
    let close_data = RobotSessionCloseData {
        session_id: "session-resize-test".into(),
        host: "workstation-alpha".into(),
        closed: true,
        cleanup_confirmed: true,
        held_keys_released: 0,
    };
    let close_env = RobotEnvelope::success(t0 + 300, AcknowledgementStage::Observed, close_data);
    assert_eq!(close_env.outcome, RobotOutcome::Success);
    assert!(close_env.data.as_ref().unwrap().closed);
}
