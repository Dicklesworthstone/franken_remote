//! Robot session open and close commands, roles, and opaque local lease handles.
//!
//! Per plan section 18.1:
//! - An agent explicitly opens a session with view/control role.
//! - Receives status, limits, and an opaque local lease handle.
//! - Commands reuse the live local client session (never a secret new host controller per input).
//! - The local client authenticates its caller — a handle printed on the command line
//!   is not authority; bearer material stays out of argv and ordinary JSON.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Requested or active role for a robot session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RobotSessionRole {
    /// Read-only observation role (pixels, geometry, and status; no input control).
    View,
    /// Interactive control role with exclusive input lease.
    Control,
}

impl fmt::Display for RobotSessionRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::View => write!(f, "view"),
            Self::Control => write!(f, "control"),
        }
    }
}

/// Operational limits associated with an admitted robot session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RobotSessionLimits {
    /// Maximum record size in bytes.
    pub max_record_bytes: usize,
    /// Lease duration in milliseconds before renewal is required.
    pub lease_duration_ms: u64,
    /// Maximum permitted age of observation for coordinate transformation.
    pub max_observation_age_ms: u64,
    /// Permitted input event submission rate in Hz.
    pub input_rate_limit_hz: u32,
}

impl Default for RobotSessionLimits {
    fn default() -> Self {
        Self {
            max_record_bytes: 65_536,
            lease_duration_ms: 3_000,
            max_observation_age_ms: 1_500,
            input_rate_limit_hz: 120,
        }
    }
}

/// Data payload returned upon successful session open.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RobotSessionOpenData {
    /// Destination workstation identity.
    pub host: String,
    /// Unique remote session identifier.
    pub session_id: String,
    /// Opaque local lease handle (e.g. "lease-local-9a8b7c6d").
    ///
    /// # Security Invariant
    /// This handle is an opaque identifier for the live local client session, NOT
    /// a bearer secret or raw credential.
    pub lease_handle: String,
    /// Admitted session role.
    pub role: RobotSessionRole,
    /// Operational status string (e.g. "active", "`waiting_approval`").
    pub status: String,
    /// Session operational limits.
    pub limits: RobotSessionLimits,
}

impl RobotSessionOpenData {
    /// Render human-readable summary.
    pub fn render_human(&self) -> String {
        format!(
            "Session Open: {}\n  Host: {}\n  Role: {}\n  Status: {}\n  Lease Handle: {}\n  Lease TTL: {} ms\n  Max Observation Age: {} ms\n",
            self.session_id,
            self.host,
            self.role,
            self.status,
            self.lease_handle,
            self.limits.lease_duration_ms,
            self.limits.max_observation_age_ms
        )
    }
}

/// Data payload returned upon session closure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RobotSessionCloseData {
    /// Remote session identifier.
    pub session_id: String,
    /// Host workstation name.
    pub host: String,
    /// Whether session is closed.
    pub closed: bool,
    /// Whether local cleanup (held modifier release, generation fencing) was confirmed.
    pub cleanup_confirmed: bool,
    /// Number of synthetic release actions generated for held keys/buttons.
    pub held_keys_released: u32,
}

impl RobotSessionCloseData {
    /// Render human-readable summary.
    pub fn render_human(&self) -> String {
        format!(
            "Session Closed: {}\n  Host: {}\n  Closed: {}\n  Cleanup Confirmed: {}\n  Held Keys Released: {}\n",
            self.session_id,
            self.host,
            self.closed,
            self.cleanup_confirmed,
            self.held_keys_released
        )
    }
}
