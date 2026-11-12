//! An outbound destination pinned to fresh installed-daemon metadata.
use super::{LocalApi, SourceIdentity, now, wall_now};
#[cfg(target_os = "linux")]
use super::{Lookup, bounded};
use crate::{Error, metadata};
use asupersync::cx::Cx;
use std::{fmt, net::IpAddr, sync::Arc};
#[cfg(target_os = "linux")]
use std::{sync::atomic::Ordering, time::Duration};

pub use crate::PeerSelector;

/// The actual host addresses/name to dial, with no public metadata constructor.
/// No Permissions/Admission can be manufactured from this discovery result.
/// The host must independently admit the client and approve observation.
pub struct PeerTarget {
    pub(super) identity: metadata::outbound::Target,
    origin: Arc<()>,
    // Linux: kernel-authenticated daemon PID. macOS: the complete code
    // identity of the signed app, never an individual query child's PID.
    source_identity: SourceIdentity,
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
    /// The route reported by the selected status snapshot. This is local
    /// metadata, not evidence of a successful connection or protocol probe.
    pub fn transport_path(&self) -> crate::PeerTransportPath {
        crate::discovery::transport_path(&self.identity.peer)
    }
    pub const fn expires_us(&self) -> u64 {
        self.expires_us
    }
    pub const fn issued_us(&self) -> u64 {
        self.issued_us
    }
    pub fn same_identity(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.origin, &other.origin)
            && self.source_identity == other.source_identity
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
        // Darwin's runtime monotonic clock can pause across system sleep.
        // Wall-clock rollback refuses above; forward elapsed time may only
        // shorten this original metadata lifetime, never extend it.
        #[cfg(any(target_os = "macos", target_os = "ios", windows))]
        super::lookup_budget::check_wall_lifetime(
            self.wall_issued_us,
            wall,
            self.expires_us - self.issued_us,
        )?;
        Ok(())
    }
}
impl LocalApi {
    /// iOS has no installed daemon to select a destination from (and no
    /// `LocalApi` constructor succeeds there); the named dialer is its path.
    #[cfg(any(target_os = "ios", windows))]
    #[allow(clippy::unused_self)] // The Linux/macOS lookups need the receiver.
    pub fn peer_target(
        &self,
        _cx: &Cx,
        selector: PeerSelector<'_>,
    ) -> std::future::Ready<Result<PeerTarget, Error>> {
        std::future::ready(selector.validate().and(Err(Error::LocalApiUnavailable)))
    }
    /// Two complete, bounded status reads select the same exact local/remote
    /// identity. Unrelated peer churn is ignored; absent/conflicting targets
    /// refuse. The deadline starts before I/O and both key expiries cap it.
    #[cfg(target_os = "linux")]
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
            let (a, ca) = self.status(cx).await?;
            let a = metadata::outbound::resolve(&a, selector)?;
            let (b, cb) = self.status(cx).await?;
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
        self.verified_target(
            cx,
            VerifiedTarget {
                identity,
                source_identity: process,
                issued_us,
                wall_issued_us,
                whois_expiry: None,
            },
        )
    }
    /// Private publication after the selected platform authenticated its whole
    /// lookup. No caller-supplied bytes or trust assertions enter this boundary.
    pub(super) fn verified_target(
        &self,
        cx: &Cx,
        verified: VerifiedTarget,
    ) -> Result<PeerTarget, Error> {
        let target = verified.finish(self.origin.clone())?;
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
    /// address sets, node keys, names, local source or tailnet require a new connection.
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

pub(super) struct VerifiedTarget {
    pub(super) identity: metadata::outbound::Target,
    pub(super) source_identity: SourceIdentity,
    pub(super) issued_us: u64,
    pub(super) wall_issued_us: u64,
    pub(super) whois_expiry: Option<String>,
}
impl VerifiedTarget {
    /// Build under the original trusted read's clocks. Publication still needs
    /// the caller's cancellation, live-clock and owner checks above.
    fn finish(self, origin: Arc<()>) -> Result<PeerTarget, Error> {
        let Self {
            identity,
            source_identity,
            issued_us,
            wall_issued_us,
            whois_expiry,
        } = self;
        let mut expires_us = issued_us.checked_add(3_000_000).ok_or(Error::Clock)?;
        let mut key_expires_us: Option<u64> = None;
        for value in [
            identity
                .host
                .host
                .expiry
                .as_ref()
                .map(|value| value.0.as_str()),
            identity.peer.expiry.as_ref().map(|value| value.0.as_str()),
            whois_expiry.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            if let Some(expiry) = crate::expiry::unix_micros(value)? {
                let remaining = expiry
                    .checked_sub(wall_issued_us)
                    .ok_or(Error::KeyExpired)?;
                expires_us = expires_us.min(issued_us.checked_add(remaining).ok_or(Error::Clock)?);
                key_expires_us = Some(key_expires_us.map_or(expiry, |old| old.min(expiry)));
            }
        }
        Ok(PeerTarget {
            identity,
            origin,
            source_identity,
            issued_us,
            expires_us,
            wall_issued_us,
            key_expires_us,
        })
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests;
