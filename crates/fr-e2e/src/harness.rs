//! High-level end-to-end test harness orchestrator.
//!
//! Executes data-driven scenarios, collects structured event logs, evaluates
//! assertions, and persists complete artifact bundles with single-command reproduction.

use crate::artifacts::{ArtifactBundle, RunSummary};
use crate::assertions::evaluate_all_assertions;
use crate::scenario::{Scenario, phase1_canonical, phase1_planted_violation};
use crate::session_driver::SessionDriver;
use std::io;
use std::path::PathBuf;

/// Configuration options for the E2E harness.
#[derive(Debug, Clone)]
pub struct HarnessConfig {
    /// Output directory for per-run artifact folders.
    pub artifacts_dir: PathBuf,
    /// Maximum allowed queue depth in bytes for bounding assertions.
    pub max_queue_bytes: usize,
}

impl Default for HarnessConfig {
    fn default() -> Self {
        Self {
            artifacts_dir: PathBuf::from("artifacts"),
            max_queue_bytes: 65536,
        }
    }
}

/// Comprehensive report returned from an executed scenario run.
#[derive(Debug, Clone)]
pub struct HarnessReport {
    pub run_id: String,
    pub scenario_name: String,
    pub summary: RunSummary,
    pub artifact_dir: PathBuf,
}

impl HarnessReport {
    /// True if the run completed and all evaluated assertions passed.
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.summary.passed
    }
}

/// The runnable E2E harness instance.
pub struct E2eHarness {
    config: HarnessConfig,
}

impl E2eHarness {
    /// Create a new harness with the specified configuration.
    #[must_use]
    pub fn new(config: HarnessConfig) -> Self {
        Self { config }
    }

    /// Run a scenario, collect all structured logs, evaluate assertions, and persist artifacts.
    pub fn run_scenario(&self, scenario: &Scenario) -> io::Result<HarnessReport> {
        let start_time = std::time::Instant::now();
        let mut bundle =
            ArtifactBundle::new(&self.config.artifacts_dir, &scenario.name, scenario.seed)?;

        let mut driver = SessionDriver::new(&mut bundle, scenario.seed);
        let execution_result = driver.execute_scenario(scenario);

        let duration_ms = u64::try_from(start_time.elapsed().as_millis()).unwrap_or(u64::MAX);

        // Evaluate all assertions over the captured event log
        let mut assertions = evaluate_all_assertions(bundle.events(), self.config.max_queue_bytes);

        // If the driver itself reported an execution error, add it as a failed assertion
        if let Err(err) = execution_result {
            assertions.push(crate::assertions::AssertionResult::fail(
                "scenario_execution_clean".to_string(),
                format!("Execution step error: {err}"),
                None,
            ));
        }

        let artifact_dir = bundle.dir.clone();
        let run_id = bundle.run_id.clone();
        let summary = bundle.flush_to_disk(assertions, duration_ms)?;

        Ok(HarnessReport {
            run_id,
            scenario_name: scenario.name.clone(),
            summary,
            artifact_dir,
        })
    }

    /// Run the canonical Phase 1 scenario (connect/control/revoke/reconnect).
    pub fn run_canonical_phase1(&self, seed: u64) -> io::Result<HarnessReport> {
        let scenario = phase1_canonical(seed);
        self.run_scenario(&scenario)
    }

    /// Run the planted violation scenario (deliberately delayed revoke fence).
    ///
    /// This is required to PROVE that the assertion layer actually fails when
    /// an invalid action (submitting input after revoke) occurs.
    pub fn run_planted_violation(&self, seed: u64, delay_ms: u64) -> io::Result<HarnessReport> {
        let scenario = phase1_planted_violation(seed, delay_ms);
        self.run_scenario(&scenario)
    }
}
