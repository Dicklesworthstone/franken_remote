//! Staged robot input execution, precondition fencing, and partial submission reporting.
//!
//! Per plan section 18.1 and 18.2:
//! - Actions require an expected geometry generation, active lease, and maximum observation age.
//! - After input, report "submitted to OS" separately from "observed application result".
//! - Never report "exactly once".
//! - Explicitly label partial batch submission and unknown external effects.

use super::envelope::{AcknowledgementStage, RobotError};
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

/// Classification of semantic evidence backing an observed application result.
///
/// Per plan section 18.2:
/// "Ordinary arbitrary pixels do not prove an application committed a semantic action.
/// A test application or an authorized semantic adapter can provide stronger evidence."
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticEvidenceType {
    /// No semantic verification available; only raw OS submission occurred.
    None,
    /// Unverified display activity (pixels changed, but semantic action unconfirmed).
    UnverifiedPixelChange,
    /// Verified by authorized semantic adapter (e.g. `FrankenTerm` pane inspect).
    SemanticAdapter,
    /// Verified by instrumented test application loopback.
    TestInstrumentation,
}

impl fmt::Display for SemanticEvidenceType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => write!(f, "none"),
            Self::UnverifiedPixelChange => write!(f, "unverified_pixel_change"),
            Self::SemanticAdapter => write!(f, "semantic_adapter"),
            Self::TestInstrumentation => write!(f, "test_instrumentation"),
        }
    }
}

/// Observed semantic effect on the target application, reported separately from OS submission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedApplicationResult {
    /// Whether semantic application execution was positively confirmed.
    pub confirmed: bool,
    /// Type of evidence verifying the application result.
    pub evidence_type: SemanticEvidenceType,
    /// Notes or details describing what was observed.
    pub details: String,
}

/// Best-effort window and focus precondition.
///
/// Per plan section 18.2:
/// "Window/focus preconditions are best-effort checks unless the platform
/// provides a genuinely atomic facility; do not claim an application cannot
/// change between check and injection."
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowFocusPrecondition {
    /// Expected target window identifier or title substring.
    pub target_window: String,
    /// Explicit marker indicating that window/focus validation is best-effort.
    pub best_effort: bool,
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
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub precondition_geometry_generation: Option<u64>,
    /// Optional precondition: maximum age of the observation that planned this action.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub max_observation_age_ms: Option<u64>,
    /// Optional precondition: expected active lease handle.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub precondition_lease: Option<String>,
    /// Optional precondition: expected window/focus state (best-effort).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub precondition_focus: Option<WindowFocusPrecondition>,
    /// Sequential list of actions to execute atomically if possible.
    pub actions: Vec<RobotInputAction>,
}

impl RobotInputRequest {
    /// Evaluate all declared preconditions against the current live workstation state.
    ///
    /// Per plan section 18.2:
    /// "An action can require an expected geometry generation, current lease, and
    /// maximum observation age. If these preconditions no longer hold, refuse rather
    /// than clicking an old coordinate system... the client must not auto-refresh its
    /// precondition and repeat a potentially destructive action."
    pub fn evaluate_preconditions(
        &self,
        current_geometry_generation: u64,
        observation_age_ms: Option<u64>,
        active_lease: Option<&str>,
        current_focus_window: Option<&str>,
    ) -> Result<(), RobotError> {
        // 1. Lease existence and validity check
        let active_l = active_lease.unwrap_or("");
        if self.lease_handle.is_empty() || active_l.is_empty() {
            return Err(RobotError::new(
                "lease_invalid_or_expired",
                "No active control lease held for destination workstation. Request control authority before issuing input.",
            ));
        }
        if self.lease_handle != active_l {
            return Err(RobotError::new(
                "lease_invalid_or_expired",
                format!(
                    "Supplied lease handle '{}' does not match active control lease. Request fresh control lease.",
                    self.lease_handle
                ),
            ));
        }

        // 2. Precondition lease equality (if explicitly declared)
        if let Some(req_l) = &self.precondition_lease
            && req_l != active_l
        {
            return Err(RobotError::new(
                "lease_mismatch_or_expired",
                format!(
                    "Preconditioned lease '{req_l}' does not match active lease '{active_l}'. Re-acquire lease before retrying input."
                ),
            ));
        }

        // 3. Geometry generation fence (prevents clicking old coordinate system)
        if let Some(req_geom) = self.precondition_geometry_generation
            && req_geom != current_geometry_generation
        {
            return Err(RobotError::new(
                "geometry_generation_stale",
                format!(
                    "Observed geometry generation {req_geom} is stale; host is at generation {current_geometry_generation}. Re-observe before retrying input."
                ),
            ));
        }

        // 4. Observation freshness fence
        if let Some(max_age) = self.max_observation_age_ms
            && let Some(age) = observation_age_ms
            && age > max_age
        {
            return Err(RobotError::new(
                "observation_expired",
                format!(
                    "Observation age {age} ms exceeds maximum allowed age {max_age} ms. Re-observe workstation before retrying input."
                ),
            ));
        }

        // 5. Focus window check (labeled best-effort)
        if let Some(focus) = &self.precondition_focus
            && let Some(curr_win) = current_focus_window
            && !curr_win.contains(&focus.target_window)
        {
            return Err(RobotError::new(
                "focus_mismatch",
                format!(
                    "Target window '{}' does not match current active window '{}' (checked best-effort). Re-observe before retrying input.",
                    focus.target_window, curr_win
                ),
            ));
        }

        Ok(())
    }
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
    /// Explicit application result observation (separated from OS submission).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub observed_application_result: Option<ObservedApplicationResult>,
}

impl RobotInputData {
    /// Render human-readable summary.
    pub fn render_human(&self) -> String {
        use std::fmt::Write as _;
        let mut out = format!(
            "Input Result: {}\n  Disposition: {}\n  Stage: {}\n  Submitted: {} / {} actions ({} admitted)\n  Observed Receipts: {}\n",
            self.request_id,
            self.disposition,
            self.stage,
            self.actions_submitted,
            self.actions_total,
            self.actions_admitted,
            self.observed_receipt_count
        );
        if let Some(sem) = &self.observed_application_result {
            let _ = writeln!(
                out,
                "  Observed Application Result: confirmed={}, evidence={}, details={}",
                sem.confirmed, sem.evidence_type, sem.details
            );
        }
        out
    }
}
