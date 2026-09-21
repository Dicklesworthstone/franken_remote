#![forbid(unsafe_code)]

//! Golden fixture tests and end-to-end simulated agent flow for the robot surface.
//!
//! Per plan section 18.1, 18.2 and bead `fr-agent-robot-surface-3o3`:
//! - Golden fixture matching between JSON and human-readable text representations.
//! - Verification of partial-batch and unknown-effect response envelopes.
//! - Verification of staged acknowledgements: admitted, `submitted_to_os`, observed.
//! - Simulated agent driving open -> observe -> input -> close with full envelope logging.

use fr_client::robot::{
    AcknowledgementStage, DisplayGeometryInfo, InputDisposition, ROBOT_SCHEMA_VERSION,
    RobotEnvelope, RobotError, RobotInputAction, RobotInputData, RobotInspectData,
    RobotObservationData, RobotOutcome, RobotSessionCloseData, RobotSessionLimits,
    RobotSessionOpenData, RobotSessionRole, RobotStatusData,
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
        uncertainty_ms: 5,
        control_authority: Some("lease-local-a1b2c3d4".into()),
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
        uncertainty_ms: 2,
        control_authority: Some(lease_handle.clone()),
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
