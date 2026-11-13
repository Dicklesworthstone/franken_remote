#![forbid(unsafe_code)]
// macOS reads the installed app's loopback LocalAPI or the open-source
// daemon's root Unix socket (local.rs); Linux-only paths such as the nftables
// ingress helper stay compiled but unused there. iOS has no installed daemon
// to read: its `LocalApi` type exists only so shared session code compiles,
// has no constructor that succeeds, and the client dials by name (named.rs).
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]
//! Installed Tailscale authority, not a VPN, identity database, or proxy.
//! The Linux adapter implements exact installed-daemon membership admission and
//! an explicit app-capability alternative. It never infers membership from names,
//! routed prefixes, reachability, or missing authority evidence.
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "ios", windows))]
mod directory;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "ios", windows))]
pub use directory::{Directory, DirectoryNode, MAX_NODES as MAX_DIRECTORY_NODES};
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "ios", windows))]
mod discovery;
mod expiry;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "ios", windows))]
pub use discovery::{DiscoveredPeer, Discovery, DiscoveryExclusions, PeerTransportPath};
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "ios", windows))]
mod selector;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "ios", windows))]
pub use selector::PeerSelector;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "ios", windows))]
mod outbound;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "ios", windows))]
pub use outbound::{TargetLease, TargetOwner};
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "ios", windows))]
mod lease;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "ios", windows))]
pub use lease::{Admission, ApprovalIdentity, Lease};
#[path = "local/endpoint.rs"]
pub mod endpoint;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "ios", windows))]
mod local;
pub mod trust;
pub use endpoint::{
    CERTIFICATE_TRANSPARENCY_NOTICE, DEFAULT_SERVICE_PORT, PROJECT_ALPN, PortCollision,
    TransportProtocol, WEBTRANSPORT_ALPN, check_port_collision, honest_https_endpoint,
    honest_quic_endpoint,
};
mod metadata;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "ios", windows))]
pub use local::{
    CertificateEvent, CertificateEventKind, CertificatePolicy, CredentialStatus, LocalQuery,
    NativeServerIdentity, NodeIdentity,
};
// Host ingress enforcement: no iOS host.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub use local::ingress;
// The one dial profile (TLS, ALPN, transport limits) both dialers share.
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "ios", windows))]
mod profile;
// A LocalAPI-less client's dialer: an exact MagicDNS name under strict WebPKI.
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "ios", windows))]
mod named;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "ios", windows))]
pub use local::{ConnectedPeer, DialRoute, LocalApi, NativeClient, PeerTarget};
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "ios", windows))]
pub use named::{HostName, HostPin, NamedClient, NamedConnection};

use std::{fmt, net::SocketAddr, time::Duration};

pub const DESKTOP_CAPABILITY: &str = "github.com/Dicklesworthstone/franken_remote/cap/desktop";

/// Content-free failure: never carries response bodies, paths or peer names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub enum Error {
    InvalidPolicy,
    InvalidEndpoint,
    MissingRuntime,
    Busy,
    Cancelled,
    Timeout,
    LocalApiUnavailable,
    UntrustedLocalApi,
    Http,
    LocalApiDenied,
    MalformedMetadata,
    SnapshotChanged,
    BackendNotRunning,
    AddressMismatch,
    SharedPeer,
    IdentityMismatch,
    ScopeDenied,
    ExplicitScopeRequired,
    /// Authenticated `WhoIs` positively reported that the machine is not approved.
    MachineNotAuthorized,
    /// Installed authority did not provide positive machine-membership evidence.
    TailnetMembershipUnverifiable,
    CapabilityDenied,
    InvalidCapability,
    CertificateRejected,
    CertificateNotDue,
    InvalidTrustStore,
    NativeBind,
    /// The native handshake reported a local UDP send/receive failure. No
    /// endpoint, OS diagnostic or peer-supplied text is retained.
    NativeRouteIo,
    NativeHandshake,
    /// The dial deadline passed before the host completed the QUIC/TLS
    /// handshake (distinct from a local `LocalAPI` lookup `Timeout`).
    HandshakeTimeout,
    /// The peer's certificate failed verification for the selected tailnet name
    /// against the configured roots. The pinned TLS stack does not say whether
    /// the name, validity period or issuer failed.
    PeerCertificateRejected,
    EntropyUnavailable,
    KeyExpired,
    Clock,
    Expired,
    IdentityChanged,
    Revoked,
    /// The system resolver returned no address for a named destination in
    /// time (on iOS, typically: the Tailscale VPN is not connected).
    NameUnresolved,
    /// A named destination resolved to an address outside Tailscale's
    /// ranges, or a pin held one. Nothing is sent to such an address.
    OutsideTailnet,
    /// The host answered this dial's Initial with `CONNECTION_REFUSED`: it
    /// serves one viewer at a time and another session is active. The
    /// answer is unauthenticated (Initial keys are public), so it can only
    /// end this dial early; it never grants anything.
    HostBusy,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Scope {
    #[default]
    OwnUser,
    Tailnet,
}

/// Explicit local policy. App grants must be restricted to tailnet members/tags
/// by the administrator. Selecting this profile NEVER writes Tailscale policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrantPolicy {
    pub scope: Scope,
    pub lookup_timeout: Duration,
    pub validity: Duration,
}
impl Default for GrantPolicy {
    fn default() -> Self {
        Self {
            scope: Scope::OwnUser,
            lookup_timeout: Duration::from_secs(1),
            validity: Duration::from_secs(1),
        }
    }
}
impl GrantPolicy {
    fn validate(self) -> Result<(), Error> {
        if self.lookup_timeout.is_zero()
            || self.lookup_timeout > Duration::from_secs(3)
            || self.validity.is_zero()
            || self.validity > Duration::from_secs(3)
        {
            return Err(Error::InvalidPolicy);
        }
        Ok(())
    }
}

/// Taken from the established transport, not from peer-supplied headers.
/// This identifies endpoints; the listener must separately enforce TUN ingress.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ConnectionAddresses {
    pub local: SocketAddr,
    pub peer: SocketAddr,
}
impl fmt::Debug for ConnectionAddresses {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ConnectionAddresses([redacted])")
    }
}
impl ConnectionAddresses {
    fn validate(self) -> Result<(), Error> {
        for a in [self.local, self.peer] {
            if a.port() == 0
                || a.ip().is_unspecified()
                || a.ip().is_multicast()
                || a.ip().is_loopback()
                || matches!(a.ip(), std::net::IpAddr::V6(v) if v.to_ipv4_mapped().is_some())
            {
                return Err(Error::InvalidEndpoint);
            }
        }
        if self.local.ip() == self.peer.ip() {
            return Err(Error::InvalidEndpoint);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdmissionProfile {
    Membership,
    AppCapability,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Permissions {
    observe: bool,
    control: bool,
}
impl Permissions {
    pub const fn observe(self) -> bool {
        self.observe
    }
    pub const fn control(self) -> bool {
        self.control
    }
}

/// One short-lived, non-cloneable result of authenticated `LocalAPI` reads.
/// Expiry is anchored before the first read, never at a delayed response.
/// No public constructor accepts JSON or a caller's assertion of identity.
pub struct Authorization {
    identity: metadata::Identity,
    origin: std::sync::Arc<()>,
    daemon_pid: i32,
    addresses: ConnectionAddresses,
    permissions: Permissions,
    profile: AdmissionProfile,
    policy: GrantPolicy,
    issued_us: u64,
    expires_us: u64,
    wall_issued_us: u64,
    key_expires_us: Option<u64>,
}
impl fmt::Debug for Authorization {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Authorization")
            .field("permissions", &self.permissions)
            .field("expires_us", &self.expires_us)
            .finish_non_exhaustive()
    }
}
impl Authorization {
    pub const fn permissions(&self) -> Permissions {
        self.permissions
    }
    pub const fn expires_us(&self) -> u64 {
        self.expires_us
    }
    pub const fn addresses(&self) -> ConnectionAddresses {
        self.addresses
    }
    pub fn matches_identity(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.origin, &other.origin)
            && self.daemon_pid == other.daemon_pid
            && self.identity == other.identity
            && self.addresses == other.addresses
            && self.profile == other.profile
            && self.policy == other.policy
    }
    /// Supply the same host clock domain that was used for `LocalAPI` admission.
    /// Suspend is an explicit lifetime boundary at the enclosing session.
    pub fn check_at(
        &self,
        addresses: ConnectionAddresses,
        now_us: u64,
    ) -> Result<Permissions, Error> {
        if addresses != self.addresses {
            return Err(Error::AddressMismatch);
        }
        if now_us < self.issued_us {
            return Err(Error::Clock);
        }
        if now_us >= self.expires_us {
            return Err(Error::Expired);
        }
        Ok(self.permissions)
    }
}
