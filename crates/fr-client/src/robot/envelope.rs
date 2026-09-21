//! Schema-versioned robot envelope and acknowledgement stage models.
//!
//! Per plan section 18.1, 18.2 and bead `fr-agent-robot-surface-3o3`:
//! - Schema-versioned envelope (`schema_version` = 1, timestamps, outcome, error, data).
//! - Staged acknowledgements: `admitted`, `submitted_to_os`, `observed`.
//! - Never "exactly once".
//! - Outcomes: `success`, `partial_submission`, `cancellation`, `refusal`, `unknown_external_effect`.
//! - Both JSON and human-readable renderings are derived from identical underlying state.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Current schema version of the `FrankenRemote` robot interface.
pub const ROBOT_SCHEMA_VERSION: u32 = 1;

/// Acknowledgement stage of an operation.
///
/// # Invariant
/// No operation is ever reported as "exactly once". Every state transition names
/// its verifiable boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcknowledgementStage {
    /// Admitted by `FrankenRemote` client or broker; pending OS/network dispatch.
    Admitted,
    /// Successfully submitted to the platform OS API (e.g. uinput, Quartz, `SendInput`).
    SubmittedToOs,
    /// Instrumentally observed by application feedback or sensor (not inferred from pixels).
    Observed,
}

impl fmt::Display for AcknowledgementStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Admitted => write!(f, "admitted"),
            Self::SubmittedToOs => write!(f, "submitted_to_os"),
            Self::Observed => write!(f, "observed"),
        }
    }
}

/// Definitive outcome of an agent command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RobotOutcome {
    /// Entire operation completed successfully.
    Success,
    /// Batch input was partially submitted before interruption or expiry.
    PartialSubmission,
    /// Operation was cancelled by user signal or higher-priority local event.
    Cancellation,
    /// Operation was refused due to missing permissions, policy, or unmet preconditions.
    Refusal,
    /// External effect is unknown (e.g. transport timeout while OS injection was in flight).
    UnknownExternalEffect,
}

impl fmt::Display for RobotOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Success => write!(f, "success"),
            Self::PartialSubmission => write!(f, "partial_submission"),
            Self::Cancellation => write!(f, "cancellation"),
            Self::Refusal => write!(f, "refusal"),
            Self::UnknownExternalEffect => write!(f, "unknown_external_effect"),
        }
    }
}

/// Structured error payload embedded in the envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RobotError {
    /// Machine-readable typed error code (e.g. "`permission_denied`", "`geometry_stale`").
    pub code: String,
    /// Specific actionable next step for the operator or agent.
    pub next_action: String,
}

impl RobotError {
    /// Create a new robot error.
    pub fn new(code: impl Into<String>, next_action: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            next_action: next_action.into(),
        }
    }
}

/// Universal schema-versioned robot envelope wrapping command results.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RobotEnvelope<T> {
    /// Schema version integer (currently 1).
    pub schema_version: u32,
    /// Monotonic or Unix timestamp in milliseconds.
    pub timestamp_unix_ms: u64,
    /// High-level outcome.
    pub outcome: RobotOutcome,
    /// Verified acknowledgement stage.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage: Option<AcknowledgementStage>,
    /// Error details when outcome != Success.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RobotError>,
    /// Command-specific response data payload.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
}

impl<T: Serialize> RobotEnvelope<T> {
    /// Create a successful response envelope.
    pub fn success(timestamp_unix_ms: u64, stage: AcknowledgementStage, data: T) -> Self {
        Self {
            schema_version: ROBOT_SCHEMA_VERSION,
            timestamp_unix_ms,
            outcome: RobotOutcome::Success,
            stage: Some(stage),
            error: None,
            data: Some(data),
        }
    }

    /// Create a refusal response envelope.
    pub fn refusal(
        timestamp_unix_ms: u64,
        code: impl Into<String>,
        next_action: impl Into<String>,
    ) -> Self {
        Self {
            schema_version: ROBOT_SCHEMA_VERSION,
            timestamp_unix_ms,
            outcome: RobotOutcome::Refusal,
            stage: None,
            error: Some(RobotError::new(code, next_action)),
            data: None,
        }
    }

    /// Create a partial submission response envelope with data.
    pub fn partial(
        timestamp_unix_ms: u64,
        stage: AcknowledgementStage,
        code: impl Into<String>,
        next_action: impl Into<String>,
        data: T,
    ) -> Self {
        Self {
            schema_version: ROBOT_SCHEMA_VERSION,
            timestamp_unix_ms,
            outcome: RobotOutcome::PartialSubmission,
            stage: Some(stage),
            error: Some(RobotError::new(code, next_action)),
            data: Some(data),
        }
    }

    /// Create an unknown external effect response envelope.
    pub fn unknown_effect(
        timestamp_unix_ms: u64,
        code: impl Into<String>,
        next_action: impl Into<String>,
        data: Option<T>,
    ) -> Self {
        Self {
            schema_version: ROBOT_SCHEMA_VERSION,
            timestamp_unix_ms,
            outcome: RobotOutcome::UnknownExternalEffect,
            stage: None,
            error: Some(RobotError::new(code, next_action)),
            data,
        }
    }

    /// Render to formatted JSON string.
    pub fn render_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Render envelope and optional formatted data payload to human-readable string.
    pub fn render_human(&self, data_summary: Option<&str>) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        let stage_str = self
            .stage
            .map_or_else(String::new, |s| format!(" (stage: {s})"));
        let _ = writeln!(out, "Outcome: {}{}", self.outcome, stage_str);
        let _ = writeln!(out, "Timestamp: {} ms", self.timestamp_unix_ms);
        if let Some(err) = &self.error {
            let _ = writeln!(out, "Error: {}: {}", err.code, err.next_action);
        }
        if let Some(data) = data_summary {
            out.push_str(data);
        }
        out
    }
}
