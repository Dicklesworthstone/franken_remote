//! An outbound destination pinned to fresh installed-daemon metadata.
use super::{LocalApi, Lookup, bounded, now, wall_now};
use crate::{Error, metadata};
use asupersync::cx::Cx;
use std::{
    fmt,
    net::IpAddr,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

/// A local selection, not a URL, DNS lookup, network scan, or authorization.
#[derive(Clone, Copy)]
pub enum PeerSelector<'a> {
    StableId(&'a str),
    Name(&'a str),
    Address(IpAddr),
}
impl fmt::Debug for PeerSelector<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PeerSelector([redacted])")
    }
}
impl PeerSelector<'_> {
    fn validate(self) -> Result<(), Error> {
        match self {
            Self::StableId(s)
                if !s.is_empty() && s.len() <= 128 && !s.chars().any(char::is_control) =>
            {
                Ok(())
            }
            Self::Name(s)
                if !s.is_empty()
                    && s.len() <= 254
                    && s.is_ascii()
                    && !s
                        .bytes()
                        .any(|b| b.is_ascii_control() || b.is_ascii_whitespace())
                    && !s.bytes().any(|b| b"/:?#@\\".contains(&b)) =>
            {
                Ok(())
            }
            Self::Address(ip)
                if !ip.is_unspecified()
                    && !ip.is_loopback()
                    && !ip.is_multicast()
                    && !matches!(ip, IpAddr::V6(v) if v.to_ipv4_mapped().is_some()) =>
            {
                Ok(())
            }
            _ => Err(Error::InvalidEndpoint),
        }
    }
}

/// The actual host addresses/name to dial, with no public metadata constructor.
/// No Permissions/Admission can be manufactured from this discovery result.
/// The host must independently admit the client and approve observation.
pub struct PeerTarget {
    pub(super) identity: metadata::outbound::Target,
    origin: Arc<()>,
    daemon_pid: i32,
    issued_us: u64,
    pub(super) expires_us: u64,
    wall_issued_us: u64,
    key_expires_us: Option<u64>,
}
impl fmt::Debug for PeerTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PeerTarget")
            .field("expires_us", &self.expires_us)
            .finish_non_exhaustive()
    }
}
impl PeerTarget {
    pub fn certificate_name(&self) -> &str {
        &self.identity.certificate_name
    }
    pub fn stable_id(&self) -> &str {
        &self.identity.peer.id.0
    }
    pub fn addresses(&self) -> &[IpAddr] {
        &self.identity.peer.ips.0
    }
    pub fn local_addresses(&self) -> &[IpAddr] {
        &self.identity.host.host.ips.0
    }
    pub const fn expires_us(&self) -> u64 {
        self.expires_us
    }
    pub const fn issued_us(&self) -> u64 {
        self.issued_us
    }
    pub fn same_identity(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.origin, &other.origin)
            && self.daemon_pid == other.daemon_pid
            && self.identity == other.identity
    }
    fn check_at(&self, current: u64, wall: u64) -> Result<(), Error> {
        if current < self.issued_us || wall < self.wall_issued_us {
            return Err(Error::Clock);
        }
        if current >= self.expires_us {
            return Err(Error::Expired);
        }
        if self.key_expires_us.is_some_and(|expiry| wall >= expiry) {
            return Err(Error::KeyExpired);
        }
        Ok(())
    }
}
impl LocalApi {
    /// Two complete, bounded status reads select the same exact local/remote
    /// identity. Unrelated peer churn is ignored; absent/conflicting targets
    /// refuse. The deadline starts before I/O and both key expiries cap it.
    pub async fn peer_target(
        &self,
        cx: &Cx,
        selector: PeerSelector<'_>,
    ) -> Result<PeerTarget, Error> {
        selector.validate()?;
        let issued_us = now(cx)?;
        let wall_issued_us = wall_now()?;
        self.busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| Error::Busy)?;
        let _lookup = Lookup(self.busy.clone());
        let (identity, process) = Box::pin(bounded(cx, Duration::from_secs(1), async {
            let (a, ca) = self.status().await?;
            let a = metadata::outbound::resolve(&a, selector)?;
            let (b, cb) = self.status().await?;
            let b = metadata::outbound::resolve(&b, selector)?;
            if ca != cb {
                return Err(Error::UntrustedLocalApi);
            }
            if a != b {
                return Err(Error::SnapshotChanged);
            }
            Ok((b, cb))
        }))
        .await?;
        let mut expires_us = issued_us.checked_add(3_000_000).ok_or(Error::Clock)?;
        let mut key_expires_us: Option<u64> = None;
        for value in [&identity.host.host.expiry, &identity.peer.expiry]
            .into_iter()
            .flatten()
        {
            if let Some(expiry) = crate::expiry::unix_micros(&value.0)? {
                let remaining = expiry
                    .checked_sub(wall_issued_us)
                    .ok_or(Error::KeyExpired)?;
                expires_us = expires_us.min(issued_us.checked_add(remaining).ok_or(Error::Clock)?);
                key_expires_us = Some(key_expires_us.map_or(expiry, |old| old.min(expiry)));
            }
        }
        let target = PeerTarget {
            identity,
            origin: self.origin.clone(),
            daemon_pid: process.pid.ok_or(Error::UntrustedLocalApi)?,
            issued_us,
            expires_us,
            wall_issued_us,
            key_expires_us,
        };
        self.check_peer_target(cx, &target)?;
        Ok(target)
    }
    pub fn check_peer_target(&self, cx: &Cx, target: &PeerTarget) -> Result<(), Error> {
        if !Arc::ptr_eq(&self.origin, &target.origin) {
            return Err(Error::IdentityMismatch);
        }
        target.check_at(now(cx)?, wall_now()?)
    }
    /// Refresh by stable node identity, NEVER by an alias that might have been
    /// reassigned. Old authority must still be live at completion. Changed
    /// address sets, node keys, names, daemon or tailnet require a new connection.
    pub async fn revalidate_peer_target(
        &self,
        cx: &Cx,
        old: &PeerTarget,
    ) -> Result<PeerTarget, Error> {
        self.check_peer_target(cx, old)?;
        let new = self
            .peer_target(cx, PeerSelector::StableId(old.stable_id()))
            .await?;
        self.check_peer_target(cx, old)?;
        if !old.same_identity(&new) {
            return Err(Error::IdentityChanged);
        }
        Ok(new)
    }
}
