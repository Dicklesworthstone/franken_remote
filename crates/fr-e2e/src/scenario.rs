//! Data-driven scenario definitions for end-to-end sessions.
//!
//! Scenarios script sessions as pure data: connect, authorize, view, control,
//! inject faults, revoke, and tear down.

use crate::fault::FaultAction;
use serde::{Deserialize, Serialize};

/// An individual discrete step within a scenario.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "step", content = "params", rename_all = "snake_case")]
pub enum ScenarioStep {
    /// Connect client to the host endpoint.
    Connect { timeout_ms: u64 },
    /// Request and obtain observation authorization.
    Authorize { mode: String, timeout_ms: u64 },
    /// Start observation stream for a display.
    StartObservation { display_id: u32, timeout_ms: u64 },
    /// Explicitly request input authority.
    RequestControl { timeout_ms: u64 },
    /// Submit simulated user input (key or pointer action).
    SendInput {
        action_type: String,
        x: i32,
        y: i32,
        key_code: u32,
        count: u32,
    },
    /// Inject a configured fault into the session or host/client environment.
    InjectFault(FaultAction),
    /// Revoke input control (synchronously at the authority decision point).
    RevokeControl { immediate: bool },
    /// Initiate a media/transport reconnect while verifying input lease is not resurrected.
    Reconnect { timeout_ms: u64 },
    /// Orderly teardown: fence authority, release held keys, close transport.
    TearDown,
    /// Pause execution for a specified duration.
    Sleep { duration_ms: u64 },
    /// Marker to evaluate an assertion at this point in the scenario.
    VerifyAssertion { assertion_name: String },
}

/// A complete runnable scenario definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scenario {
    pub name: String,
    pub description: String,
    pub seed: u64,
    pub timeout_ms: u64,
    pub steps: Vec<ScenarioStep>,
}

impl Scenario {
    /// Create a new empty scenario.
    #[must_use]
    pub fn new(name: impl Into<String>, description: impl Into<String>, seed: u64) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            seed,
            timeout_ms: 30_000,
            steps: Vec::new(),
        }
    }

    /// Add a step to the scenario.
    pub fn add_step(&mut self, step: ScenarioStep) {
        self.steps.push(step);
    }
}

/// Fluent builder for constructing data-driven scenarios.
#[derive(Debug, Default)]
pub struct ScenarioBuilder {
    name: String,
    description: String,
    seed: u64,
    timeout_ms: u64,
    steps: Vec<ScenarioStep>,
}

impl ScenarioBuilder {
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: String::new(),
            seed: 42,
            timeout_ms: 30_000,
            steps: Vec::new(),
        }
    }

    #[must_use]
    pub fn description(mut self, desc: impl Into<String>) -> Self {
        self.description = desc.into();
        self
    }

    #[must_use]
    pub const fn seed(mut self, seed: u64) -> Self {
        self.seed = seed;
        self
    }

    #[must_use]
    pub const fn timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = timeout_ms;
        self
    }

    #[must_use]
    pub fn connect(mut self, timeout_ms: u64) -> Self {
        self.steps.push(ScenarioStep::Connect { timeout_ms });
        self
    }

    #[must_use]
    pub fn authorize(mut self, mode: impl Into<String>, timeout_ms: u64) -> Self {
        self.steps.push(ScenarioStep::Authorize {
            mode: mode.into(),
            timeout_ms,
        });
        self
    }

    #[must_use]
    pub fn start_observation(mut self, display_id: u32, timeout_ms: u64) -> Self {
        self.steps.push(ScenarioStep::StartObservation {
            display_id,
            timeout_ms,
        });
        self
    }

    #[must_use]
    pub fn request_control(mut self, timeout_ms: u64) -> Self {
        self.steps.push(ScenarioStep::RequestControl { timeout_ms });
        self
    }

    #[must_use]
    pub fn send_input(
        mut self,
        action_type: impl Into<String>,
        x: i32,
        y: i32,
        key_code: u32,
        count: u32,
    ) -> Self {
        self.steps.push(ScenarioStep::SendInput {
            action_type: action_type.into(),
            x,
            y,
            key_code,
            count,
        });
        self
    }

    #[must_use]
    pub fn inject_fault(mut self, fault: FaultAction) -> Self {
        self.steps.push(ScenarioStep::InjectFault(fault));
        self
    }

    #[must_use]
    pub fn revoke_control(mut self, immediate: bool) -> Self {
        self.steps.push(ScenarioStep::RevokeControl { immediate });
        self
    }

    #[must_use]
    pub fn reconnect(mut self, timeout_ms: u64) -> Self {
        self.steps.push(ScenarioStep::Reconnect { timeout_ms });
        self
    }

    #[must_use]
    pub fn teardown(mut self) -> Self {
        self.steps.push(ScenarioStep::TearDown);
        self
    }

    #[must_use]
    pub fn sleep(mut self, duration_ms: u64) -> Self {
        self.steps.push(ScenarioStep::Sleep { duration_ms });
        self
    }

    #[must_use]
    pub fn verify_assertion(mut self, assertion_name: impl Into<String>) -> Self {
        self.steps.push(ScenarioStep::VerifyAssertion {
            assertion_name: assertion_name.into(),
        });
        self
    }

    #[must_use]
    pub fn build(self) -> Scenario {
        Scenario {
            name: self.name,
            description: self.description,
            seed: self.seed,
            timeout_ms: self.timeout_ms,
            steps: self.steps,
        }
    }
}

/// Standard Phase 1 canonical connect/control/revoke/reconnect scenario.
#[must_use]
pub fn phase1_canonical(seed: u64) -> Scenario {
    ScenarioBuilder::new("phase1_canonical")
        .description("Canonical Phase 1 connect, authorize, observe, control, revoke, reconnect sequence")
        .seed(seed)
        .connect(5000)
        .authorize("prompt_always", 5000)
        .start_observation(0, 5000)
        .request_control(5000)
        .send_input("key_press", 0, 0, 42, 1)
        .send_input("pointer_move", 100, 200, 0, 1)
        .revoke_control(true)
        .reconnect(5000)
        .teardown()
        .build()
}

/// Planted violation scenario: deliberately delay revoke fence so an action lands after revoke.
#[must_use]
pub fn phase1_planted_violation(seed: u64, delay_ms: u64) -> Scenario {
    use crate::fault::PlantedViolationKind;

    ScenarioBuilder::new("phase1_planted_violation")
        .description("Planted violation scenario with delayed revoke fence to verify assertion failure")
        .seed(seed)
        .connect(5000)
        .authorize("prompt_always", 5000)
        .start_observation(0, 5000)
        .request_control(5000)
        .send_input("key_press", 0, 0, 42, 1)
        // Inject planted violation: delayed revoke fence
        .inject_fault(FaultAction::PlantedViolation(
            PlantedViolationKind::DelayedRevokeFence { delay_ms },
        ))
        .revoke_control(true)
        // Send input during the delayed window to trigger the assertion failure
        .send_input("key_press_after_revoke", 0, 0, 43, 1)
        .teardown()
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scenario_builder_creates_expected_steps() {
        let scenario = phase1_canonical(12345);
        assert_eq!(scenario.name, "phase1_canonical");
        assert_eq!(scenario.seed, 12345);
        assert_eq!(scenario.steps.len(), 9);
    }

    #[test]
    fn scenario_serialization_roundtrip() {
        let scenario = phase1_canonical(999);
        let json = serde_json::to_string_pretty(&scenario).unwrap();
        let decoded: Scenario = serde_json::from_str(&json).unwrap();
        assert_eq!(scenario, decoded);
    }
}
