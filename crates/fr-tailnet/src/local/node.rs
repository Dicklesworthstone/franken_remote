//! Installed host identity for binding a listener, not permission for a peer.
use super::{LocalApi, Lookup, bounded, now, wall_now};
use crate::{Error, metadata};
use asupersync::cx::Cx;
use std::time::Duration;
use std::{
    fmt,
    net::IpAddr,
    sync::{Arc, atomic::Ordering},
};

const VALIDITY_US: u64 = 3_000_000;
const STATUS_LIMIT: usize = 64 * 1024;

/// A non-cloneable, short-lived status snapshot from the credential-checked
/// installed daemon. Addresses and certificate name are host metadata, never
/// client suggestions. This does NOT establish tunnel ingress or peer admission.
pub struct NodeIdentity {
    identity: metadata::HostIdentity,
    origin: Arc<()>,
    daemon_pid: i32,
    issued_us: u64,
    pub(super) expires_us: u64,
    pub(super) wall_issued_us: u64,
    key_expires_us: Option<u64>,
}
impl fmt::Debug for NodeIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NodeIdentity")
            .field("expires_us", &self.expires_us)
            .finish_non_exhaustive()
    }
}
impl NodeIdentity {
    pub fn addresses(&self) -> &[IpAddr] {
        &self.identity.host.ips.0
    }
    pub fn certificate_name(&self) -> &str {
        &self.identity.certificate_name
    }
    pub const fn expires_us(&self) -> u64 {
        self.expires_us
    }
    pub(crate) fn check_at(&self, current: u64, wall: u64) -> Result<(), Error> {
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
    pub(crate) fn same_node(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.origin, &other.origin)
            && self.daemon_pid == other.daemon_pid
            && self.identity == other.identity
    }
}
impl LocalApi {
    /// Two bounded, credential-checked snapshots establish current self-owned
    /// addresses and a tailnet-qualified certificate name before socket binding.
    /// The deadline begins before lookup; delayed replies do not slide it.
    pub async fn node_identity(&self, cx: &Cx) -> Result<NodeIdentity, Error> {
        now(cx)?;
        self.busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| Error::Busy)?;
        let _slot = Lookup(self.busy.clone());
        let issued_us = now(cx)?;
        let wall_issued_us = wall_now()?;
        let mut expires_us = issued_us.checked_add(VALIDITY_US).ok_or(Error::Clock)?;
        let lookup_end = issued_us.checked_add(1_000_000).ok_or(Error::Clock)?;
        let (identity, credential, key_expires_us) = Box::pin(bounded(
            cx,
            Duration::from_micros(lookup_end.checked_sub(now(cx)?).ok_or(Error::Timeout)?),
            async {
                let (a, ca) = self
                    .get("/localapi/v0/status?peers=false", STATUS_LIMIT)
                    .await?;
                let (b, cb) = self
                    .get("/localapi/v0/status?peers=false", STATUS_LIMIT)
                    .await?;
                if ca != cb {
                    return Err(Error::UntrustedLocalApi);
                }
                let a = metadata::Status::parse(&a)?.host_identity()?;
                let b = metadata::Status::parse(&b)?.host_identity()?;
                if a != b {
                    return Err(Error::SnapshotChanged);
                }
                let key_expires = a
                    .host
                    .expiry
                    .as_ref()
                    .map(|v| crate::expiry::unix_micros(&v.0))
                    .transpose()?
                    .flatten();
                Ok((a, ca, key_expires))
            },
        ))
        .await?;
        if let Some(expiry) = key_expires_us {
            let remaining = expiry.checked_sub(wall_now()?).ok_or(Error::KeyExpired)?;
            // Starting at issue time subtracts the whole lookup duration, never
            // extending key validity by time spent reading metadata.
            expires_us = expires_us.min(issued_us.checked_add(remaining).ok_or(Error::Clock)?);
        }
        let node = NodeIdentity {
            identity,
            origin: self.origin.clone(),
            daemon_pid: credential.pid.ok_or(Error::UntrustedLocalApi)?,
            issued_us,
            expires_us,
            wall_issued_us,
            key_expires_us,
        };
        self.check_node(cx, &node)?;
        Ok(node)
    }
    pub fn check_node(&self, cx: &Cx, node: &NodeIdentity) -> Result<(), Error> {
        now(cx)?;
        if !Arc::ptr_eq(&self.origin, &node.origin) {
            return Err(Error::IdentityMismatch);
        }
        node.check_at(now(cx)?, wall_now()?)
    }
    /// Refresh only an unexpired snapshot belonging to this exact daemon
    /// instance. An address, certificate name, key or tailnet change requires
    /// closing old listeners/sessions and creating a new checked owner.
    pub async fn revalidate_node(
        &self,
        cx: &Cx,
        old: &NodeIdentity,
    ) -> Result<NodeIdentity, Error> {
        self.check_node(cx, old)?;
        let new = self.node_identity(cx).await?;
        self.check_node(cx, old)?;
        if !old.same_node(&new) {
            return Err(Error::IdentityChanged);
        }
        Ok(new)
    }
}
