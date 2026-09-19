#![forbid(unsafe_code)]
//! Scripted end-to-end session harness with structured logging and artifact capture
//! for FrankenRemote (`fr-e2e`).
//!
//! # Architecture & Scope (Plan Sections 23 Phase 1 Gate, 24.2)
//!
//! This crate provides the runnable spine for all end-to-end integration and
//! fault-injection testing across FrankenRemote.
//!
//! - **Data-driven Scenarios**: Scenarios script sessions as pure data (`ScenarioStep`),
//!   exercising real client cores (`fr_client::ClientSession`) and host authority.
//! - **Structured Logging**: Every process transition, queue depth snapshot, input
//!   disposition, recovery event, and fault is captured into machine-readable JSONL.
//! - **Standard Fault Injectors**: Kill/stall workers, drop transport, expire tickets,
//!   resize displays, and planted violation injectors.
//! - **Assertion Layer**: Verifies critical invariants over captured logs (e.g.
//!   no input submitted after revoke, queue depths strictly bounded, no lease resurrection
//!   on reconnect, orderly teardown sequence).
//! - **Artifact Retention**: Every run captures a full bundle:
//!   - `manifest.json`: Commit hash, configuration, seed, platform metadata.
//!   - `events.jsonl`: Chronological structured event log.
//!   - `summary.json`: Detailed assertion outcomes and execution stats.
//!   - `reproduce.sh`: Exact single-command reproduction script for any run.

pub mod artifacts;
pub mod assertions;
pub mod event;
pub mod fault;
pub mod harness;
pub mod scenario;
pub mod session_driver;

pub use artifacts::{ArtifactBundle, RunManifest, RunSummary};
pub use assertions::{
    assert_no_input_executed_after_revoke, assert_no_lease_resurrection_on_reconnect,
    assert_orderly_teardown, assert_queue_high_water, evaluate_all_assertions, AssertionResult,
};
pub use event::{AuthorityState, EventKind, EventSource, InputStage, StructuredLogEvent};
pub use fault::{FaultAction, FaultInjector, PlantedViolationKind};
pub use harness::{E2eHarness, HarnessConfig, HarnessReport};
pub use scenario::{
    phase1_canonical, phase1_planted_violation, Scenario, ScenarioBuilder, ScenarioStep,
};
pub use session_driver::SessionDriver;
