//! Inspection models for `fr inspect <host>`.
//!
//! Per plan section 18.1:
//! - Reports detailed machine identity, transport path, approval requirements,
//!   display count, and discovery state.
//! - Shares the same underlying JSON envelope and human-readable formatting.

use serde::{Deserialize, Serialize};

/// Detailed inspection data for a candidate host machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RobotInspectData {
    /// Hostname or stable node ID.
    pub host: String,
    /// Tailnet DNS / certificate name.
    pub certificate_name: String,
    /// Advertised IP addresses.
    pub addresses: Vec<String>,
    /// Port probed or configured.
    pub port: u16,
    /// Whether connection uses DERP relay.
    pub is_derp_relayed: bool,
    /// Current discovery/readiness state.
    pub state: String,
    /// Number of detected displays; `None` unless a host session reported them.
    pub displays_count: Option<usize>,
    /// Whether local operator approval is required; `None` unless the host said.
    pub requires_approval: Option<bool>,
    /// Verified transport path kind ("direct", "`derp_relayed`", "unknown").
    pub transport_path: String,
}

impl RobotInspectData {
    /// Render human-readable summary.
    pub fn render_human(&self) -> String {
        format!(
            "Host Inspection: {}\n  Certificate Name: {}\n  Addresses: {}\n  Port: {}\n  Transport: {} (DERP: {})\n  State: {}\n  Displays: {}\n  Requires Approval: {}\n",
            self.host,
            self.certificate_name,
            self.addresses.join(", "),
            self.port,
            self.transport_path,
            self.is_derp_relayed,
            self.state,
            self.displays_count
                .map_or_else(|| "unknown".to_string(), |n| n.to_string()),
            self.requires_approval
                .map_or_else(|| "unknown".to_string(), |b| b.to_string())
        )
    }
}
