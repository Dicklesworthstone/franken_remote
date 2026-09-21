//! Client status models for `fr status`.
//!
//! Per plan section 18.1:
//! - Reports client process state, active session, tailnet connectivity, and lease.
//! - Shares the same underlying JSON envelope and human-readable formatting.

use serde::{Deserialize, Serialize};

/// Client status data payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RobotStatusData {
    /// `FrankenRemote` client version string.
    pub client_version: String,
    /// Whether local Tailscale is connected.
    pub tailnet_connected: bool,
    /// Local Tailscale machine name.
    pub local_node_name: String,
    /// Primary Tailscale IP address.
    pub local_ip: String,
    /// Number of active remote connections.
    pub active_sessions_count: usize,
    /// Currently connected host, if any.
    pub active_host: Option<String>,
    /// Active session role ("view" or "control"), if connected.
    pub active_role: Option<String>,
    /// Opaque lease handle, if active.
    pub active_lease_handle: Option<String>,
}

impl RobotStatusData {
    /// Render human-readable summary.
    pub fn render_human(&self) -> String {
        format!(
            "FrankenRemote Client Status:\n  Version: {}\n  Tailnet Connected: {}\n  Local Node: {} ({})\n  Active Sessions: {}\n  Connected Host: {}\n  Session Role: {}\n  Lease Handle: {}\n",
            self.client_version,
            self.tailnet_connected,
            self.local_node_name,
            self.local_ip,
            self.active_sessions_count,
            self.active_host.as_deref().unwrap_or("none"),
            self.active_role.as_deref().unwrap_or("none"),
            self.active_lease_handle.as_deref().unwrap_or("none")
        )
    }
}
