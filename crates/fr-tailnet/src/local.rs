//! Installed local identity: the daemon's credential-checked Unix socket
//! (`socket.rs`), or on macOS the standalone Tailscale app's root-published
//! loopback `LocalAPI` (`macos/endpoint.rs`). No proxy, redirect or public
//! listener. Host and client reads share whichever transport is selected.
//!
//! Linux: [`LocalApi::installed`] is `/var/run/tailscale/tailscaled.sock`;
//! [`LocalApi::new`] is one configured socket.
//!
//! macOS selection, in this order:
//! 1. [`LocalApi::new`] (`--socket PATH`): only that Unix socket.
//! 2. [`LocalApi::installed`], decided again on every read: the standalone
//!    app's endpoint when it has published one in `/Library/Tailscale`;
//!    otherwise, when that directory or its `ipnport` link is absent or the
//!    directory is root-only (the open-source daemon keeps its state there),
//!    the open-source daemon's fixed socket `/var/run/tailscaled.socket`.
//!    Tailscale's own CLI uses the same order.
//!
//! Every socket server must be the root daemon by kernel credentials. An
//! untrusted or denied app publication, or a refused or untrusted socket, is a
//! typed refusal and never falls back to the other transport. The App Store
//! variant (sandboxed group container) is not supported and reports
//! `LocalApiUnavailable`, as before.
use crate::{
    AdmissionProfile, Authorization, ConnectionAddresses, Error, GrantPolicy, expiry,
    metadata::{self, Status, WhoIs},
};
use asupersync::{cx::Cx, http::h1::Request};
use std::{
    fmt,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
// The fixtures below (`tests`) import these through `super::*`; the clock
// and pulse that used them moved to `crate::profile`.
#[cfg(test)]
use asupersync::{time::sleep, types::Time};
#[cfg(test)]
use std::{
    future::{Future, poll_fn},
    pin::pin,
    task::Poll,
    time::Duration,
};

/// The daemon that answered: a Unix-socket server's kernel-reported PID, or
/// the macOS app endpoint's published run identity. Reads of one lookup must
/// agree.
type SourceIdentity = i32;

/// One fixed installed-daemon status read. This does not register a reader or
/// grant trust to caller-supplied metadata. On macOS host reads go to the
/// installed app's loopback `LocalAPI`, like the client's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LocalQuery {
    Status { peers: bool },
}

#[derive(Clone)]
pub struct LocalApi {
    /// The Unix socket (on macOS `installed()`, read only while the app has
    /// published no endpoint).
    path: Arc<PathBuf>,
    /// macOS `installed()`: the standalone app's state directory, read first
    /// on every request. `None` for a configured socket.
    #[cfg(target_os = "macos")]
    app: Option<Arc<PathBuf>>,
    origin: Arc<()>,
    busy: Arc<AtomicBool>,
    certificate_busy: Arc<AtomicBool>,
    /// One directory read at a time per API owner.
    directory_busy: Arc<AtomicBool>,
    daemon_uid: u32,
}
impl fmt::Debug for LocalApi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LocalApi([protected endpoint])")
    }
}
struct Lookup(Arc<AtomicBool>);
impl Drop for Lookup {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl LocalApi {
    #[cfg(target_os = "linux")]
    pub fn installed() -> Self {
        Self::new("/var/run/tailscale/tailscaled.sock").expect("fixed protected socket")
    }
    /// iOS has no installed daemon and no readable `LocalAPI`: every
    /// endpoint is refused, so no read on iOS ever reaches a transport.
    #[cfg(target_os = "ios")]
    pub fn new(_path: impl AsRef<Path>) -> Result<Self, Error> {
        Err(Error::LocalApiUnavailable)
    }
    /// A locally configured absolute Unix-socket path (macOS: never the app
    /// endpoint). The kernel-reported server UID must be root even at a custom
    /// path. The selected host OS/root remain trusted.
    #[cfg(not(target_os = "ios"))]
    pub fn new(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        if !socket::valid_path(path) {
            return Err(Error::InvalidEndpoint);
        }
        Ok(Self {
            path: Arc::new(path.to_path_buf()),
            #[cfg(target_os = "macos")]
            app: None,
            origin: Arc::new(()),
            busy: Arc::new(AtomicBool::new(false)),
            certificate_busy: Arc::new(AtomicBool::new(false)),
            directory_busy: Arc::new(AtomicBool::new(false)),
            daemon_uid: 0,
        })
    }
    /// Exact endpoint tuple MUST originate at a TUN-restricted, established
    /// transport. This proves identity/grants, not the listener's ingress path.
    /// At most one lookup per shared client; at most two three-request attempts.
    /// Default product profile: authorize exact Tailscale membership and local
    /// sharing scope from the installed daemon. Missing machine-approval evidence
    /// is a typed refusal; this never falls back to names, prefixes or `CapMap`.
    pub async fn authorize_membership(
        &self,
        cx: &Cx,
        addresses: ConnectionAddresses,
        policy: GrantPolicy,
    ) -> Result<Authorization, Error> {
        self.authorize(cx, addresses, policy, AdmissionProfile::Membership)
            .await
    }

    /// Explicit administrator-selected app-capability profile. This is not the
    /// default own-user path and never serves as a fallback for membership.
    pub async fn authorize_app_capability(
        &self,
        cx: &Cx,
        addresses: ConnectionAddresses,
        policy: GrantPolicy,
    ) -> Result<Authorization, Error> {
        self.authorize(cx, addresses, policy, AdmissionProfile::AppCapability)
            .await
    }

    async fn authorize(
        &self,
        cx: &Cx,
        addresses: ConnectionAddresses,
        policy: GrantPolicy,
        profile: AdmissionProfile,
    ) -> Result<Authorization, Error> {
        policy.validate()?;
        addresses.validate()?;
        if self
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(Error::Busy);
        }
        let _lookup = Lookup(self.busy.clone());
        let issued_us = now(cx)?;
        let wall_issued_us = wall_now()?;
        let validity =
            u64::try_from(policy.validity.as_micros()).map_err(|_| Error::InvalidPolicy)?;
        let mut expires_us = issued_us.checked_add(validity).ok_or(Error::Clock)?;
        let (identity, permissions, expiries, daemon_pid) =
            Box::pin(bounded(cx, policy.lookup_timeout, async {
                for _ in 0..2 {
                    let (first, process) = self.status(cx).await?;
                    first.validate(addresses)?;
                    let (who, who_process) = self.whois(addresses).await?;
                    let (last, last_process) = self.status(cx).await?;
                    if first != last || process != who_process || process != last_process {
                        continue;
                    }
                    let (identity, permissions, expiries) = match profile {
                        AdmissionProfile::Membership => {
                            metadata::evaluate_membership(&last, &who, addresses, policy)?
                        }
                        AdmissionProfile::AppCapability => {
                            metadata::evaluate(&last, &who, addresses, policy)?
                        }
                    };
                    return Ok((identity, permissions, expiries, last_process));
                }
                Err(Error::SnapshotChanged)
            }))
            .await?;
        let mut key_expires_us = None;
        for value in expiries.into_iter().flatten() {
            if let Some(expiry) = expiry::unix_micros(&value)? {
                let remaining = expiry
                    .checked_sub(wall_issued_us)
                    .ok_or(Error::KeyExpired)?;
                expires_us = expires_us.min(issued_us.checked_add(remaining).ok_or(Error::Clock)?);
                key_expires_us = Some(key_expires_us.map_or(expiry, |old: u64| old.min(expiry)));
            }
        }
        let proof = Authorization {
            identity,
            origin: self.origin.clone(),
            daemon_pid,
            addresses,
            permissions,
            profile,
            policy,
            issued_us,
            expires_us,
            wall_issued_us,
            key_expires_us,
        };
        self.check(cx, &proof, addresses)?;
        Ok(proof)
    }
    /// No renewal on failed reads or changed identity. An already expired proof
    /// cannot be resurrected; establish a new app session instead.
    pub async fn revalidate(
        &self,
        cx: &Cx,
        old: &Authorization,
        addresses: ConnectionAddresses,
    ) -> Result<Authorization, Error> {
        self.check(cx, old, addresses)?;
        let new = self
            .authorize(cx, addresses, old.policy, old.profile)
            .await?;
        // Original authority must still be live after awaiting the local daemon.
        self.check(cx, old, addresses)?;
        if !old.matches_identity(&new) {
            return Err(Error::IdentityChanged);
        }
        Ok(new)
    }
    pub fn check(
        &self,
        cx: &Cx,
        proof: &Authorization,
        addresses: ConnectionAddresses,
    ) -> Result<crate::Permissions, Error> {
        if !Arc::ptr_eq(&self.origin, &proof.origin) {
            return Err(Error::IdentityChanged);
        }
        let permission = proof.check_at(addresses, now(cx)?)?;
        let wall = wall_now()?;
        if wall < proof.wall_issued_us {
            return Err(Error::Clock);
        }
        if proof.key_expires_us.is_some_and(|expiry| wall >= expiry) {
            return Err(Error::KeyExpired);
        }
        Ok(permission)
    }
    /// The directory this admitted viewer may see (`crate::directory`): one
    /// bounded status read, only while `proof` is live, before and after.
    /// Never a substitute for admission and never cached.
    pub(crate) async fn directory(
        &self,
        cx: &Cx,
        proof: &Authorization,
    ) -> Result<crate::Directory, Error> {
        self.check(cx, proof, proof.addresses())?;
        if self
            .directory_busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(Error::Busy);
        }
        let _lookup = Lookup(self.directory_busy.clone());
        let (body, _) = Box::pin(bounded(
            cx,
            proof.policy.lookup_timeout,
            self.query(
                cx,
                LocalQuery::Status { peers: true },
                metadata::STATUS_BYTES,
            ),
        ))
        .await?;
        let status = crate::directory::Status::parse(&body)?;
        let directory = crate::directory::select(&status, proof.identity.directory_scope())?;
        self.check(cx, proof, proof.addresses())?;
        Ok(directory)
    }
    async fn status(&self, cx: &Cx) -> Result<(Status, SourceIdentity), Error> {
        let (body, process) = self
            .query(
                cx,
                LocalQuery::Status { peers: true },
                metadata::STATUS_BYTES,
            )
            .await?;
        Ok((Status::parse(&body)?, process))
    }
    async fn query(
        &self,
        _cx: &Cx,
        query: LocalQuery,
        maximum: usize,
    ) -> Result<(Vec<u8>, SourceIdentity), Error> {
        let uri = match query {
            LocalQuery::Status { peers: true } => "/localapi/v0/status?peers=true",
            LocalQuery::Status { peers: false } => "/localapi/v0/status?peers=false",
        };
        self.get(uri, maximum).await
    }
    async fn whois(
        &self,
        addresses: ConnectionAddresses,
    ) -> Result<(WhoIs, SourceIdentity), Error> {
        let request = Request::get("/localapi/v0/whois")
            .query([("addr", addresses.peer.to_string())])
            .build();
        let (body, process) = self.get(&request.uri, metadata::WHOIS_BYTES).await?;
        Ok((WhoIs::parse(&body)?, process))
    }
    async fn get(&self, uri: &str, maximum: usize) -> Result<(Vec<u8>, SourceIdentity), Error> {
        self.get_typed(uri, maximum, "application/json").await
    }
    /// One read over the selected transport (module docs). On macOS the app's
    /// endpoint is chosen afresh for every read, as its files are; a read that
    /// changes transport mid-lookup cannot share the other's identity.
    #[cfg(not(target_os = "ios"))]
    async fn get_typed(
        &self,
        uri: &str,
        maximum: usize,
        content_type: &str,
    ) -> Result<(Vec<u8>, SourceIdentity), Error> {
        #[cfg(target_os = "macos")]
        if let Some(app) = &self.app
            && let Some(read) = macos::get(app, uri, maximum, content_type).await?
        {
            return Ok(read);
        }
        socket::get(&self.path, self.daemon_uid, uri, maximum, content_type).await
    }
    /// iOS: no transport exists. Unreachable in practice (no iOS constructor
    /// succeeds); refused with a type anyway.
    #[cfg(target_os = "ios")]
    #[allow(clippy::unused_self)] // The Linux/macOS transports need the receiver.
    fn get_typed(
        &self,
        _uri: &str,
        _maximum: usize,
        _content_type: &str,
    ) -> std::future::Ready<Result<(Vec<u8>, SourceIdentity), Error>> {
        std::future::ready(Err(Error::LocalApiUnavailable))
    }
}
/// Accept only a complete, uncompressed 200 response of the expected type.
fn checked(
    response: asupersync::http::h1::Response,
    withheld: bool,
    content_type: &str,
) -> Result<Vec<u8>, Error> {
    if response.status != 200 {
        return Err(Error::LocalApiDenied);
    }
    if withheld
        || !response.trailers.is_empty()
        || response.header_value("Content-Encoding").is_some()
        || !response.header_value("Content-Type").is_some_and(|v| {
            v.split(';')
                .next()
                .is_some_and(|m| m.trim().eq_ignore_ascii_case(content_type))
        })
    {
        return Err(Error::Http);
    }
    Ok(response.body)
}
// The shared monotonic clock and bounded-operation pulse (crate::profile).
use crate::discovery::wall_now;
pub(crate) use crate::profile::{bounded, now};
mod node;
pub use node::NodeIdentity;

// The fake daemons prove themselves with kernel peer credentials: Linux
// SO_PEERCRED, macOS getpeereid and LOCAL_PEERPID.
#[cfg(test)]
mod tests;

mod certificate;
pub use certificate::{
    CertificateEvent, CertificateEventKind, CertificatePolicy, CredentialStatus,
    NativeServerIdentity,
};

mod target;
pub use target::PeerTarget;

mod dial;
pub use dial::{ConnectedPeer, DialRoute, NativeClient};

#[cfg(target_os = "linux")]
mod discovery;
#[cfg(not(target_os = "ios"))]
mod socket;

#[cfg(any(target_os = "macos", target_os = "ios", test))]
mod lookup_budget;
#[cfg(target_os = "macos")]
mod macos;

#[cfg(not(target_os = "ios"))]
pub mod ingress;
