//! Fault injection types and actions for end-to-end testing.
//!
//! Standard fault injectors: kill/stall worker, drop transport, expire tickets,
//! resize display, and planted violations for proving assertion layer sensitivity.

use serde::{Deserialize, Serialize};

/// Specific kind of deliberate planted violation used to test assertion sensitivity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlantedViolationKind {
    /// Deliberately delay the revoke fence so an input submission can land
    /// after revoke was called, verifying that `assert_no_input_executed_after_revoke`
    /// catches the violation.
    DelayedRevokeFence { delay_ms: u64 },
    /// Deliberately allow an input action with an expired ticket to be submitted.
    SubmitWithExpiredTicket { ticket_id: u64 },
    /// Deliberately restore an input lease across reconnect without a new grant.
    ResurrectLeaseAcrossReconnect { lease_id: u64 },
    /// Deliberately inflate a queue beyond its byte ceiling.
    QueueLimitBreach { queue_name: String, excess_bytes: usize },
}

/// Actions the fault injector can apply to a running session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", content = "params", rename_all = "snake_case")]
pub enum FaultAction {
    /// Terminate a helper process (e.g. `MediaWorker`, `SessionAgent`) with a signal.
    KillWorker { role: String, signal: i32 },
    /// Pause execution of a worker or event loop for a duration.
    StallWorker { role: String, duration_ms: u64 },
    /// Sever or drop network transport packets for a duration.
    DropTransport { duration_ms: u64 },
    /// Expire an input ticket before OS submission.
    ExpireTicket { ticket_id: u64 },
    /// Change the virtual display geometry during active viewing.
    ResizeDisplay { width: u32, height: u32, scale_pct: u32 },
    /// Planted violation for proving the test harness assertion layer fails on bugs.
    PlantedViolation(PlantedViolationKind),
}

/// A configured fault injector ready for execution during a scenario.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FaultInjector {
    pub name: String,
    pub description: String,
    pub action: FaultAction,
}

impl FaultInjector {
    #[must_use]
    pub const fn new(name: String, description: String, action: FaultAction) -> Self {
        Self {
            name,
            description,
            action,
        }
    }

    /// Kill a media worker process.
    #[must_use]
    pub fn kill_media_worker(signal: i32) -> Self {
        Self {
            name: "kill_media_worker".to_string(),
            description: "Terminate media worker process with signal".to_string(),
            action: FaultAction::KillWorker {
                role: "media_worker".to_string(),
                signal,
            },
        }
    }

    /// Stall worker for a specific duration.
    #[must_use]
    pub fn stall_worker(role: &str, duration_ms: u64) -> Self {
        Self {
            name: format!("stall_{role}"),
            description: format!("Stall worker {role} for {duration_ms}ms"),
            action: FaultAction::StallWorker {
                role: role.to_string(),
                duration_ms,
            },
        }
    }

    /// Drop transport packets for a duration.
    #[must_use]
    pub fn drop_transport(duration_ms: u64) -> Self {
        Self {
            name: "drop_transport".to_string(),
            description: format!("Drop transport for {duration_ms}ms"),
            action: FaultAction::DropTransport { duration_ms },
        }
    }

    /// Plant a delayed revoke fence violation.
    #[must_use]
    pub fn planted_delayed_revoke(delay_ms: u64) -> Self {
        Self {
            name: "planted_delayed_revoke".to_string(),
            description: "Planted violation: delay revoke fence to test assertion failure".to_string(),
            action: FaultAction::PlantedViolation(PlantedViolationKind::DelayedRevokeFence {
                delay_ms,
            }),
        }
    }
}
