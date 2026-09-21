//! Tailnet-native peer discovery, capability probing, saved hosts, and directory policy.
//!
//! Per plan section 6.4:
//! - Native desktop clients obtain peers from local Tailscale state, then perform
//!   bounded capability probes ONLY against those node addresses.
//! - Results are cached by stable node identity with an expiry, and invalidated on
//!   any tailnet change. Broad subnet scans are strictly forbidden.
//! - Mobile and web baseline uses saved hosts and explicit host links (`fr://<host>[:<port>]`).
//!   Bearer tokens and credentials in links are unconditionally rejected.
//! - Optional authenticated directory served by a known `frd`: opt-in, restricted by
//!   local policy, discovery information only (never delegated permission or proxy).
//! - Discovery responses never carry screenshots, window/application titles, clipboard
//!   previews, or audio.

pub mod cache;
pub mod directory;
pub mod prober;
pub mod saved_hosts;

use serde::{Deserialize, Serialize};
use std::net::IpAddr;

pub use cache::{DEFAULT_TTL_US, DiscoveryCache, MAX_CACHE_ENTRIES};
pub use directory::{
    DIRECTORY_DISCLAIMER, DirectoryHostRecord, DirectoryPolicy, DirectoryRefusalReason,
    DirectoryRequest, DirectoryResponse, DirectoryService, MAX_DIRECTORY_RECORDS,
};
pub use prober::{
    CapabilityProber, DEFAULT_PROBE_TIMEOUT_US, INITIAL_BACKOFF_US, MAX_BACKOFF_US,
    MAX_CONCURRENT_PROBES, ProbeScheduler, ProbeTarget, compute_backoff_us,
};
pub use saved_hosts::{
    DEFAULT_SERVICE_PORT, HostLink, HostLinkError, MAX_SAVED_HOSTS, SavedHost, SavedHostError,
    SavedHostStore,
};

/// Distinct discovery and readiness states of a remote machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PeerDiscoveryState {
    /// Host machine not responding or offline on tailnet.
    Offline,
    /// Host is responding on tailnet, but frd is not installed or service port refused.
    NotInstalled,
    /// frd is active, but host has no interactive display session logged in.
    NoInteractiveSession,
    /// frd is active, but access requires approval or is restricted by tailnet policy.
    PermissionRequired { reason: String },
    /// frd is active and interactive desktop session is ready for connection.
    Ready {
        displays: usize,
        requires_approval: bool,
    },
    /// Capability probe is currently in-flight.
    Probing,
}

/// A discovered tailnet host machine candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredHost {
    /// Stable Tailscale node ID (e.g. "node-abcdef1234").
    pub stable_id: String,
    /// Tailnet DNS / certificate name (e.g. "workstation.tailnet.ts.net").
    pub certificate_name: String,
    /// Tailnet IP addresses assigned to this node.
    pub addresses: Vec<IpAddr>,
    /// Port where frd service is expected/probed (default 8443).
    pub port: u16,
    /// Whether traffic to this host traverses a DERP relay.
    pub is_derp_relayed: bool,
    /// Current discovery state.
    pub state: PeerDiscoveryState,
    /// Host monotonic timestamp of last probe, in microseconds.
    pub last_probed_us: Option<u64>,
    /// Number of consecutive probe failures (for exponential backoff).
    pub probe_failures: u32,
    /// Earliest timestamp (microseconds) when next probe is permitted.
    pub backoff_until_us: u64,
}

impl DiscoveredHost {
    /// Create a new discovered host candidate with initial `Offline` state.
    pub fn new(
        stable_id: impl Into<String>,
        certificate_name: impl Into<String>,
        addresses: Vec<IpAddr>,
        port: u16,
        is_derp_relayed: bool,
    ) -> Self {
        Self {
            stable_id: stable_id.into(),
            certificate_name: certificate_name.into(),
            addresses,
            port,
            is_derp_relayed,
            state: PeerDiscoveryState::Offline,
            last_probed_us: None,
            probe_failures: 0,
            backoff_until_us: 0,
        }
    }

    /// Whether this host is currently ready for a workstation session.
    pub fn is_ready(&self) -> bool {
        matches!(self.state, PeerDiscoveryState::Ready { .. })
    }

    /// Whether a probe is currently permitted given current monotonic timestamp.
    pub fn can_probe(&self, now_us: u64) -> bool {
        self.state != PeerDiscoveryState::Probing && now_us >= self.backoff_until_us
    }
}
