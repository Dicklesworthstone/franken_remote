//! Staged robot input execution, precondition fencing, and partial submission reporting.
//!
//! Per plan section 18.1 and 18.2:
//! - Actions require an expected geometry generation, active lease, and maximum observation age.
//! - After input, report "submitted to OS" separately from "observed application result".
//! - Never report "exactly once".
//! - Explicitly label partial batch submission and unknown external effects.

use super::envelope::AcknowledgementStage;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Supported discrete input action primitives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RobotInputAction {
    /// Move cursor to coordinates.
    MouseMove { x: i32, y: i32 },
    /// Depress mouse button.
    MouseDown { button: u8, x: i32, y: i32 },
    /// Release mouse button.
    MouseUp { button: u8, x: i32, y: i32 },
    /// Depress physical or virtual key.
    KeyDown { key: String },
    /// Release physical or virtual key.
    KeyUp { key: String },
    /// Committed Unicode text input.
    Text { text: String },
    /// Scroll wheel lines.
    Scroll { dx: i32, dy: i32 },
}

/// Execution disposition of an input batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputDisposition {
    /// All actions in the batch were admitted and submitted to the OS.
    Committed,
    /// Only a prefix of the batch was submitted before interruption, timeout, or lease expiry.
    Partial,
    /// Status of external effect cannot be verified (e.g. timeout during dispatch).
    UnknownEffect,
    /// Batch was refused before any action was dispatched to the OS.
    Refused,
}

impl fmt::Display for InputDisposition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Committed => write!(f, "committed"),
            Self::Partial => write!(f, "partial"),
            Self::UnknownEffect => write!(f, "unknown_effect"),
            Self::Refused => write!(f, "refused"),
        }
    }
}

/// Incoming agent input request with precondition fences.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RobotInputRequest {
    /// Destination workstation.
    pub host: String,
    /// Opaque local lease handle authorizing input.
    pub lease_handle: String,
    /// Client-supplied idempotency / tracking request identifier.
    pub request_id: String,
    /// Optional precondition: required geometry generation counter.
    pub precondition_geometry_generation: Option<u64>,
    /// Optional precondition: maximum age of the observation that planned this action.
    pub max_observation_age_ms: Option<u64>,
    /// Sequential list of actions to execute atomically if possible.
    pub actions: Vec<RobotInputAction>,
}

/// Response payload reporting input execution results and staged acknowledgements.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RobotInputData {
    /// Request identifier matching request.
    pub request_id: String,
    /// Total actions requested in batch.
    pub actions_total: usize,
    /// Actions admitted past security and rate checks.
    pub actions_admitted: usize,
    /// Actions successfully submitted to the OS input subsystem.
    pub actions_submitted: usize,
    /// Current acknowledgement stage.
    pub stage: AcknowledgementStage,
    /// Execution disposition.
    pub disposition: InputDisposition,
    /// Number of distinct host receipts confirmed.
    pub observed_receipt_count: u32,
}

impl RobotInputData {
    /// Render human-readable summary.
    pub fn render_human(&self) -> String {
        format!(
            "Input Result: {}\n  Disposition: {}\n  Stage: {}\n  Submitted: {} / {} actions ({} admitted)\n  Observed Receipts: {}\n",
            self.request_id,
            self.disposition,
            self.stage,
            self.actions_submitted,
            self.actions_total,
            self.actions_admitted,
            self.observed_receipt_count
        )
    }
}
