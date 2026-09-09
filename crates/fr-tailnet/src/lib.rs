#![forbid(unsafe_code)]
//! Installed Tailscale authority, not a VPN, identity database, or proxy.
//! The initial Linux adapter implements the explicit app-capability profile.
//! It does NOT guess zero-setup same-tailnet membership from names or addresses.
mod expiry;
#[cfg(target_os = "linux")]
mod lease;
#[cfg(target_os = "linux")]
pub use lease::{Admission, Lease};
#[cfg(target_os = "linux")]
mod local;
mod metadata;
#[cfg(target_os = "linux")]
pub use local::LocalApi;

use std::{fmt, net::SocketAddr, time::Duration};

pub const DESKTOP_CAPABILITY: &str = "github.com/Dicklesworthstone/franken_remote/cap/desktop";

/// Content-free failure: never carries response bodies, paths or peer names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// Authenticated `WhoIs` did not positively report machine approval.
    MachineNotAuthorized,
    CapabilityDenied,
    InvalidCapability,
    KeyExpired,
    Clock,
    Expired,
    IdentityChanged,
    Revoked,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
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
