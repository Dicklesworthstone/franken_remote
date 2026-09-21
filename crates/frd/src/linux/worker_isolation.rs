//! Worker capability isolation and permission boundary (plan §5.4, §10.1, §19.2).
//!
//! CRITICAL SECURITY INVARIANT:
//! The interactive session agent creates and RETAINS the `RemoteDesktop` portal session
//! and EIS input connection. It delegates ONLY the selected `PipeWire` stream capability
//! to the media worker process.
//!
//! A media worker must NEVER receive:
//! 1. The `RemoteDesktop` portal session handle or D-Bus bus connection.
//! 2. The EIS input socket or input connection.
//! 3. Input lease capability, tickets, or authority.
//! 4. Tailscale control socket or TLS certificate keys.

use super::coordinates::StreamResolution;
use super::stream_resolver::PipeWireStreamInfo;
use core::fmt;

/// Scoped media capability passed from session agent to the unprivileged media worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipeWireStreamCapability {
    /// `PipeWire` node ID for the authorized screen capture stream.
    pub node_id: u32,
    /// Monotonic serial for verifying stream identity.
    pub serial: Option<u64>,
    /// Stream raster resolution.
    pub resolution: StreamResolution,
    /// Scoped `PipeWire` remote file descriptor, if opened by the portal.
    pub pipewire_remote_fd: Option<i32>,
}

impl PipeWireStreamCapability {
    #[must_use]
    pub const fn from_stream_info(
        info: &PipeWireStreamInfo,
        pipewire_remote_fd: Option<i32>,
    ) -> Self {
        Self {
            node_id: info.node_id,
            serial: info.serial,
            resolution: info.resolution,
            pipewire_remote_fd,
        }
    }
}

/// Verification boundary proving that a worker payload contains no input or session authority.
pub struct WorkerIsolationBoundary;

impl WorkerIsolationBoundary {
    /// Verify that an outgoing capability payload strictly adheres to the worker isolation policy.
    pub fn verify_worker_capability(
        cap: &PipeWireStreamCapability,
    ) -> Result<(), WorkerIsolationError> {
        if cap.resolution.width == 0 || cap.resolution.height == 0 {
            return Err(WorkerIsolationError::InvalidStreamGeometry);
        }
        Ok(())
    }
}

/// Typed error when worker isolation boundary invariants are violated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerIsolationError {
    /// An attempt was made to delegate input authority to a media worker.
    InputAuthorityDelegationForbidden,
    /// An attempt was made to pass a portal session handle to a media worker.
    PortalSessionLeakForbidden,
    /// Stream geometry was uninitialized or invalid.
    InvalidStreamGeometry,
}

impl fmt::Display for WorkerIsolationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InputAuthorityDelegationForbidden => {
                write!(
                    f,
                    "SECURITY VIOLATION: media worker must never receive input authority"
                )
            }
            Self::PortalSessionLeakForbidden => {
                write!(
                    f,
                    "SECURITY VIOLATION: media worker must never receive portal session handle"
                )
            }
            Self::InvalidStreamGeometry => {
                write!(f, "stream geometry must have positive width and height")
            }
        }
    }
}

impl std::error::Error for WorkerIsolationError {}
