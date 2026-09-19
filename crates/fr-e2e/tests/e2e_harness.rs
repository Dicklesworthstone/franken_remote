//! Integration tests for the FrankenRemote E2E test harness (`fr-e2e`).
//!
//! Acceptance verification:
//! - Canonical Phase 1 pair connects, authorizes, controls, revokes, reconnects, with full artifacts.
//! - Planted violation (deliberately delayed revoke fence) is caught by the assertion layer, proving the harness can fail.
//! - Queue high-water marks and reconnect invariants verified.
//! - Reproducibility artifact bundle validated.

use fr_e2e::artifacts::RunSummary;
use fr_e2e::fault::FaultAction;
use fr_e2e::harness::{E2eHarness, HarnessConfig};
use fr_e2e::scenario::ScenarioBuilder;
use std::fs;
use std::path::PathBuf;

fn test_temp_dir(suffix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fr_e2e_test_{}_{}", suffix, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn test_canonical_phase1_scenario_succeeds_with_full_artifacts() {
    let artifacts_dir = test_temp_dir("canonical");
    let harness = E2eHarness::new(HarnessConfig {
        artifacts_dir: artifacts_dir.clone(),
        max_queue_bytes: 65536,
    });

    let report = harness.run_canonical_phase1(42).expect("run_canonical_phase1 failed");

    // 1. Assert scenario succeeded
    assert!(
        report.is_success(),
        "Canonical Phase 1 scenario must succeed cleanly, but failed with: {:?}",
        report.summary.failure_reason
    );
    assert_eq!(report.summary.scenario_name, "phase1_canonical");

    // 2. Assert artifact bundle files exist
    let run_dir = &report.artifact_dir;
    assert!(run_dir.join("manifest.json").exists(), "manifest.json missing");
    assert!(run_dir.join("events.jsonl").exists(), "events.jsonl missing");
    assert!(run_dir.join("summary.json").exists(), "summary.json missing");
    assert!(run_dir.join("reproduce.sh").exists(), "reproduce.sh missing");

    // 3. Inspect summary.json content
    let summary_bytes = fs::read(run_dir.join("summary.json")).unwrap();
    let summary: RunSummary = serde_json::from_slice(&summary_bytes).unwrap();
    assert!(summary.passed);
    assert!(summary.event_count > 0);
    assert!(summary.assertions.iter().all(|a| a.passed));

    // 4. Inspect reproduce.sh content
    let reproduce_script = fs::read_to_string(run_dir.join("reproduce.sh")).unwrap();
    assert!(reproduce_script.contains("phase1_canonical"));
    assert!(reproduce_script.contains("--seed 42"));

    // Clean up
    let _ = fs::remove_dir_all(artifacts_dir);
}

#[test]
fn test_planted_violation_is_caught_by_assertion_layer() {
    let artifacts_dir = test_temp_dir("planted");
    let harness = E2eHarness::new(HarnessConfig {
        artifacts_dir: artifacts_dir.clone(),
        max_queue_bytes: 65536,
    });

    // Run with planted delayed revoke fence (50ms delay)
    let report = harness
        .run_planted_violation(999, 50)
        .expect("run_planted_violation failed");

    // 1. Assert scenario MUST FAIL because of the planted violation
    assert!(
        !report.is_success(),
        "Planted violation MUST be caught by the assertion layer, but reported success!"
    );

    // 2. Verify exact failing assertion
    let failed_assertion = report
        .summary
        .assertions
        .iter()
        .find(|a| !a.passed)
        .expect("At least one assertion must fail");

    assert_eq!(
        failed_assertion.assertion_name, "no_input_executed_after_revoke",
        "The failing assertion must be no_input_executed_after_revoke"
    );
    assert!(
        failed_assertion.message.contains("VIOLATION"),
        "Message must contain VIOLATION indication: {}",
        failed_assertion.message
    );
    assert!(
        failed_assertion.offending_event.is_some(),
        "Offending event must be retained in assertion failure record"
    );

    // 3. Verify failure is recorded in summary.json on disk
    let summary_bytes = fs::read(report.artifact_dir.join("summary.json")).unwrap();
    let summary: RunSummary = serde_json::from_slice(&summary_bytes).unwrap();
    assert!(!summary.passed);
    assert!(summary.failure_reason.is_some());

    // Clean up
    let _ = fs::remove_dir_all(artifacts_dir);
}

#[test]
fn test_queue_high_water_assertion_detects_overflow() {
    let artifacts_dir = test_temp_dir("queue_overflow");
    let harness = E2eHarness::new(HarnessConfig {
        artifacts_dir: artifacts_dir.clone(),
        max_queue_bytes: 1024, // Artificially low bound
    });

    let scenario = ScenarioBuilder::new("queue_bound_test")
        .seed(123)
        .connect(5000)
        .start_observation(0, 5000) // Produces 8192 bytes queue depth, exceeding 1024!
        .teardown()
        .build();

    let report = harness.run_scenario(&scenario).unwrap();

    assert!(
        !report.is_success(),
        "Queue high-water assertion must catch depth exceeding 1024 bytes"
    );

    let queue_assertion = report
        .summary
        .assertions
        .iter()
        .find(|a| a.assertion_name == "queue_high_water_bounded")
        .unwrap();

    assert!(!queue_assertion.passed);
    assert!(queue_assertion.message.contains("exceeded limit"));

    // Clean up
    let _ = fs::remove_dir_all(artifacts_dir);
}

#[test]
fn test_fault_injection_kill_worker_records_event() {
    let artifacts_dir = test_temp_dir("kill_worker");
    let harness = E2eHarness::new(HarnessConfig {
        artifacts_dir: artifacts_dir.clone(),
        max_queue_bytes: 65536,
    });

    let scenario = ScenarioBuilder::new("kill_worker_test")
        .seed(456)
        .connect(5000)
        .start_observation(0, 5000)
        .inject_fault(FaultAction::KillWorker {
            role: "media_worker".to_string(),
            signal: 9,
        })
        .teardown()
        .build();

    let report = harness.run_scenario(&scenario).unwrap();
    assert!(report.is_success());

    // Verify fault was logged in events.jsonl
    let events_content = fs::read_to_string(report.artifact_dir.join("events.jsonl")).unwrap();
    assert!(events_content.contains("kill_worker"));
    assert!(events_content.contains("media_worker"));

    // Clean up
    let _ = fs::remove_dir_all(artifacts_dir);
}
