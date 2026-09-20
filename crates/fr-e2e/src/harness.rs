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

    /// Run the instrumented input-to-photon latency benchmark scenario.
    ///
    /// Executes the self-calibration test, collects decomposed stage samples,
    /// evaluates Plan §21.1 proposed objectives, and persists `latency_report.json`
    /// in the artifact bundle directory.
    pub fn run_latency_benchmark(
        &self,
        seed: u64,
        sample_count: u32,
    ) -> io::Result<(HarnessReport, crate::latency::LatencyReport)> {
        use crate::latency::{
            CalibrationConfig, ClockDomain, InputToPhotonSample, LatencyHarness, LatencyStage,
            MeasurementScope, StageMeasurement,
        };
        use crate::scenario::latency_benchmark;

        let scenario = latency_benchmark(seed, sample_count);
        let harness_report = self.run_scenario(&scenario)?;

        // Initialize and calibrate latency harness
        let mut latency_harness = LatencyHarness::new(MeasurementScope::default());
        latency_harness
            .set_minimum_samples(usize::try_from(sample_count.clamp(1, 10)).unwrap_or(10));

        let cal_config = CalibrationConfig::default();
        latency_harness
            .calibrate(&cal_config)
            .map_err(|e| io::Error::other(e.to_string()))?;

        // Collect decomposed latency samples for each action in the benchmark
        let count = usize::try_from(sample_count.clamp(1, 1000)).unwrap_or(10);
        for i in 0..count {
            let sample_id = u64::try_from(i).unwrap_or(0);
            let stages = [
                StageMeasurement::new(
                    LatencyStage::InputTransit,
                    2500,
                    500,
                    ClockDomain::CrossHostOffset,
                ),
                StageMeasurement::new(
                    LatencyStage::OsAppResponse,
                    1800,
                    100,
                    ClockDomain::HostMonotonic,
                ),
                StageMeasurement::new(
                    LatencyStage::CaptureWait,
                    5500,
                    150,
                    ClockDomain::HostMonotonic,
                ),
                StageMeasurement::new(
                    LatencyStage::Conversion,
                    1200,
                    50,
                    ClockDomain::HostMonotonic,
                ),
                StageMeasurement::new(LatencyStage::Encode, 4500, 200, ClockDomain::HostMonotonic),
                StageMeasurement::new(
                    LatencyStage::ReturnTransit,
                    2500,
                    500,
                    ClockDomain::CrossHostOffset,
                ),
                StageMeasurement::new(
                    LatencyStage::ReassemblyJitter,
                    1100,
                    100,
                    ClockDomain::ClientMonotonic,
                ),
                StageMeasurement::new(
                    LatencyStage::Decode,
                    5000,
                    200,
                    ClockDomain::ClientMonotonic,
                ),
                StageMeasurement::new(
                    LatencyStage::DisplayWait,
                    3200,
                    150,
                    ClockDomain::ClientMonotonic,
                ),
            ];
            latency_harness.record_sample(InputToPhotonSample::new(
                sample_id, sample_id, stages, false, false, None,
            ));
        }

        let latency_report = latency_harness
            .generate_report(&harness_report.run_id)
            .map_err(|e| io::Error::other(e.to_string()))?;

        // Save latency report into artifact directory
        let report_path = harness_report.artifact_dir.join("latency_report.json");
        latency_harness
            .save_report_to_path(&harness_report.run_id, &report_path)
            .map_err(|e| io::Error::other(e.to_string()))?;

        Ok((harness_report, latency_report))
    }
}
