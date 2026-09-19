//! Process-role family and role-specific capabilities (plan sections 5.1, 5.2, 5.3, 5.4, 19.2).
//!
//! `FrankenRemote` operates as a signed process family with three primary roles:
//! - `Broker`: Privileged platform daemon / broker. Minimal platform service,
//!   owns tailnet listeners, session registry, peer cache, and protected Tailscale ops.
//!   Never executes media capture, encoding, or arbitrary privileged RPC.
//! - `SessionAgent`: Interactive-session agent running in the user's GUI session.
//!   Owns user consent, visible indicator, input watchdog, and input lease.
//! - `MediaWorker`: On-demand private worker for screen capture, GPU surface management,
//!   and HEVC encoding.
//!
//! CRITICAL SECURITY INVARIANT:
//! Workers receive NO Tailscale control socket, NO certificate keys, NO approval endpoint,
//! and NO input-lease capability.

use core::fmt;

/// Distinct process roles in the `FrankenRemote` signed process family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ProcessRole {
    /// Privileged platform broker / daemon.
    Broker = 0,
    /// Interactive user session agent (owns consent and input lease).
    SessionAgent = 1,
    /// On-demand media worker (owns capture, GPU surfaces, and codec).
    MediaWorker = 2,
}

impl ProcessRole {
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    #[must_use]
    pub const fn from_u8(raw: u8) -> Option<Self> {
        match raw {
            0 => Some(Self::Broker),
            1 => Some(Self::SessionAgent),
            2 => Some(Self::MediaWorker),
            _ => None,
        }
    }

    /// Whether this role is permitted to exercise the specified capability.
    #[must_use]
    pub const fn allows_capability(self, cap: RoleCapability) -> bool {
        match self {
            Self::Broker => matches!(
                cap,
                RoleCapability::TailscaleControlSocket
                    | RoleCapability::CertificateKeys
                    | RoleCapability::DisplayQuery
                    | RoleCapability::AuditLog
            ),
            Self::SessionAgent => matches!(
                cap,
                RoleCapability::ApprovalEndpoint
                    | RoleCapability::InputLease
                    | RoleCapability::DisplayQuery
                    | RoleCapability::ClipboardAccess
            ),
            Self::MediaWorker => matches!(
                cap,
                RoleCapability::MediaCapture | RoleCapability::GpuSurfaces
            ),
        }
    }

    /// Returns a human-readable name for logging.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Broker => "broker",
            Self::SessionAgent => "session-agent",
            Self::MediaWorker => "media-worker",
        }
    }
}

impl fmt::Display for ProcessRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Granular, role-specific capabilities for process isolation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum RoleCapability {
    /// Access to Tailscale `LocalAPI` socket for peer identity.
    TailscaleControlSocket = 1,
    /// Access to TLS certificate private keys for server identity.
    CertificateKeys = 2,
    /// User consent prompt and session approval handling.
    ApprovalEndpoint = 3,
    /// Authority to request and exercise OS input leases.
    InputLease = 4,
    /// Screen capture pipeline execution (e.g. `PipeWire` portal / X11).
    MediaCapture = 5,
    /// Hardware GPU surface allocation and HEVC encoding/decoding.
    GpuSurfaces = 6,
    /// Query display enumeration and screen geometry.
    DisplayQuery = 7,
    /// Access to OS clipboard reading/writing.
    ClipboardAccess = 8,
    /// Privileged local audit logging.
    AuditLog = 9,
}

impl RoleCapability {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::TailscaleControlSocket => "tailscale-control-socket",
            Self::CertificateKeys => "certificate-keys",
            Self::ApprovalEndpoint => "approval-endpoint",
            Self::InputLease => "input-lease",
            Self::MediaCapture => "media-capture",
            Self::GpuSurfaces => "gpu-surfaces",
            Self::DisplayQuery => "display-query",
            Self::ClipboardAccess => "clipboard-access",
            Self::AuditLog => "audit-log",
        }
    }
}

impl fmt::Display for RoleCapability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Monotonic generation for process lifecycle fencing (plan section 7.2).
///
/// Child process crashes or restarts advance the generation; messages tagged
/// with an older generation are rejected to prevent stale state propagation.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProcessGeneration(u64);

impl ProcessGeneration {
    /// Initial process generation.
    pub const INITIAL: Self = Self(1);

    #[must_use]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    #[must_use]
    pub const fn as_raw(self) -> u64 {
        self.0
    }

    /// Advance to the successor generation, refusing to wrap on exhaustion.
    #[must_use]
    pub const fn next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(n) => Some(Self(n)),
            None => None,
        }
    }

    /// True if `self` supersedes `other`.
    #[must_use]
    pub const fn supersedes(self, other: Self) -> bool {
        self.0 > other.0
    }
}

impl fmt::Debug for ProcessGeneration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ProcessGeneration({})", self.0)
    }
}

impl fmt::Display for ProcessGeneration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_has_strictly_forbidden_capabilities() {
        let worker = ProcessRole::MediaWorker;
        // Workers receive NO Tailscale control socket, NO cert keys, NO approval, NO input lease.
        assert!(!worker.allows_capability(RoleCapability::TailscaleControlSocket));
        assert!(!worker.allows_capability(RoleCapability::CertificateKeys));
        assert!(!worker.allows_capability(RoleCapability::ApprovalEndpoint));
        assert!(!worker.allows_capability(RoleCapability::InputLease));
        assert!(!worker.allows_capability(RoleCapability::ClipboardAccess));
        assert!(!worker.allows_capability(RoleCapability::AuditLog));

        // Workers only have media capture and GPU surfaces.
        assert!(worker.allows_capability(RoleCapability::MediaCapture));
        assert!(worker.allows_capability(RoleCapability::GpuSurfaces));
    }

    #[test]
    fn session_agent_has_consent_and_input_but_no_tailscale_or_media() {
        let agent = ProcessRole::SessionAgent;
        assert!(agent.allows_capability(RoleCapability::ApprovalEndpoint));
        assert!(agent.allows_capability(RoleCapability::InputLease));
        assert!(agent.allows_capability(RoleCapability::DisplayQuery));
        assert!(agent.allows_capability(RoleCapability::ClipboardAccess));

        // Agent has no Tailscale socket, cert keys, or media workers.
        assert!(!agent.allows_capability(RoleCapability::TailscaleControlSocket));
        assert!(!agent.allows_capability(RoleCapability::CertificateKeys));
        assert!(!agent.allows_capability(RoleCapability::MediaCapture));
        assert!(!agent.allows_capability(RoleCapability::GpuSurfaces));
    }

    #[test]
    fn broker_has_listeners_and_certs_but_no_input_or_gpu() {
        let broker = ProcessRole::Broker;
        assert!(broker.allows_capability(RoleCapability::TailscaleControlSocket));
        assert!(broker.allows_capability(RoleCapability::CertificateKeys));
        assert!(broker.allows_capability(RoleCapability::DisplayQuery));
        assert!(broker.allows_capability(RoleCapability::AuditLog));

        // Broker has no direct input authority or media/GPU allocations.
        assert!(!broker.allows_capability(RoleCapability::InputLease));
        assert!(!broker.allows_capability(RoleCapability::ApprovalEndpoint));
        assert!(!broker.allows_capability(RoleCapability::MediaCapture));
        assert!(!broker.allows_capability(RoleCapability::GpuSurfaces));
    }

    #[test]
    fn process_generation_monotonicity_and_no_wrap() {
        let g1 = ProcessGeneration::INITIAL;
        let g2 = g1.next().expect("advance succeeds");
        assert!(g2.supersedes(g1));
        assert!(!g1.supersedes(g2));

        let max = ProcessGeneration::from_raw(u64::MAX);
        assert_eq!(max.next(), None);
    }

    #[test]
    fn process_role_raw_roundtrip() {
        for raw in 0..=2 {
            let role = ProcessRole::from_u8(raw).expect("valid role");
            assert_eq!(role.as_u8(), raw);
        }
        assert_eq!(ProcessRole::from_u8(3), None);
        assert_eq!(ProcessRole::from_u8(255), None);
    }
}
