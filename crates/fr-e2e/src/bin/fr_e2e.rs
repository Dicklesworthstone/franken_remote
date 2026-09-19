//! Standalone CLI executable for running `FrankenRemote` E2E scenarios.
//!
//! Usage:
//!   `fr_e2e` [--scenario <name>] [--seed <num>] [--artifacts-dir <dir>] [--planted-delay <ms>]

use fr_e2e::harness::{E2eHarness, HarnessConfig};
use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

fn print_usage() {
    eprintln!(
        r"FrankenRemote End-to-End Test Runner (`fr_e2e`)

Usage:
    fr_e2e [OPTIONS]

Options:
    --scenario <NAME>       Scenario name: 'phase1_canonical' (default), 'phase1_planted_violation'
    --seed <NUMBER>         RNG seed for deterministic runs (default: 42)
    --artifacts-dir <PATH>  Output directory for run artifacts (default: artifacts)
    --planted-delay <MS>    Delay in milliseconds for planted violation test (default: 50)
    --help, -h              Print this help text
"
    );
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();

    let mut scenario_name = "phase1_canonical".to_string();
    let mut seed = 42u64;
    let mut artifacts_dir = PathBuf::from("artifacts");
    let mut planted_delay_ms = 50u64;

    let mut idx = 1;
    while idx < args.len() {
        match args[idx].as_str() {
            "--scenario" => {
                idx += 1;
                if idx < args.len() {
                    scenario_name.clone_from(&args[idx]);
                }
            }
            "--seed" => {
                idx += 1;
                if idx < args.len() {
                    seed = args[idx].parse().unwrap_or(42);
                }
            }
            "--artifacts-dir" => {
                idx += 1;
                if idx < args.len() {
                    artifacts_dir = PathBuf::from(&args[idx]);
                }
            }
            "--planted-delay" => {
                idx += 1;
                if idx < args.len() {
                    planted_delay_ms = args[idx].parse().unwrap_or(50);
                }
            }
            "--help" | "-h" => {
                print_usage();
                return ExitCode::SUCCESS;
            }
            unknown => {
                eprintln!("Unknown argument: {unknown}");
                print_usage();
                return ExitCode::FAILURE;
            }
        }
        idx += 1;
    }

    println!("=== FrankenRemote E2E Harness ===");
    println!("Scenario:      {scenario_name}");
    println!("Seed:          {seed}");
    println!("Artifacts dir: {}", artifacts_dir.display());

    let harness = E2eHarness::new(HarnessConfig {
        artifacts_dir,
        max_queue_bytes: 65536,
    });

    let report = match scenario_name.as_str() {
        "phase1_canonical" => harness.run_canonical_phase1(seed),
        "phase1_planted_violation" => harness.run_planted_violation(seed, planted_delay_ms),
        custom => {
            eprintln!("Error: Unknown scenario '{custom}'. Supported: 'phase1_canonical', 'phase1_planted_violation'");
            return ExitCode::FAILURE;
        }
    };

    let report = match report {
        Ok(r) => r,
        Err(err) => {
            eprintln!("Error executing scenario: {err}");
            return ExitCode::FAILURE;
        }
    };

    println!("\n=== Execution Summary ===");
    println!("Run ID:        {}", report.run_id);
    println!("Passed:        {}", report.summary.passed);
    println!("Duration:      {}ms", report.summary.duration_ms);
    println!("Total events:  {}", report.summary.event_count);
    println!("Artifacts:     {}", report.artifact_dir.display());

    println!("\n--- Assertion Outcomes ---");
    for assertion in &report.summary.assertions {
        let status = if assertion.passed { "PASS" } else { "FAIL" };
        println!("[{status}] {}: {}", assertion.assertion_name, assertion.message);
    }

    if let Some(reason) = &report.summary.failure_reason {
        eprintln!("\nFAILURE: {reason}");
    }

    println!("\nReproduce this exact run with:");
    println!("  scripts/e2e/run.sh --scenario {scenario_name} --seed {seed}");

    if report.is_success() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
