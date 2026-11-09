//! Installed local identity: credential-checked Linux Unix sockets, or on
//! macOS the standalone Tailscale app's root-published loopback `LocalAPI`
//! (`macos/endpoint.rs`). No proxy, redirect or public listener.
//! Host-shaped APIs remain available for shared session signatures; their
//! underlying reads still refuse on macOS until a macOS host exists.
use crate::{
    AdmissionProfile, Authorization, ConnectionAddresses, Error, GrantPolicy, expiry,
    metadata::{self, Status, WhoIs},
};
use asupersync::{cx::Cx, http::h1::Request, net::unix::UCred, time::sleep, types::Time};
#[cfg(target_os = "linux")]
use asupersync::{http::h1::Http1Client, net::unix::UnixStream};
#[cfg(target_os = "linux")]
use std::path::PathBuf;
use std::{
    fmt,
    future::{Future, poll_fn},
    path::Path,
    pin::pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::Poll,
    time::Duration,
};

/// The daemon that answered: its PID on Linux (kernel credentials), its
/// published endpoint's run identity on macOS. Reads of one lookup must agree.
type SourceIdentity = i32;

/// One fixed installed-daemon status read. This does not register a reader or
/// grant trust to caller-supplied metadata. Host reads refuse on macOS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LocalQuery {
    Status { peers: bool },
}

#[derive(Clone)]
pub struct LocalApi {
    #[cfg(target_os = "linux")]
    path: Arc<PathBuf>,
    origin: Arc<()>,
    busy: Arc<AtomicBool>,
    certificate_busy: Arc<AtomicBool>,
    #[cfg(target_os = "linux")]
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
    /// A locally configured absolute path. The kernel-reported server UID must
    /// be root even at a custom path. The selected host OS/root remain trusted.
    #[cfg(target_os = "linux")]
    pub fn new(path: impl AsRef<Path>) -> Result<Self, Error> {
        use std::os::unix::ffi::OsStrExt;
        let path = path.as_ref();
        let bytes = path.as_os_str().as_bytes();
        if !path.is_absolute()
            || bytes.len() >= 108
            || bytes.contains(&0)
            || path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(Error::InvalidEndpoint);
        }
        Ok(Self {
            path: Arc::new(path.to_path_buf()),
            origin: Arc::new(()),
            busy: Arc::new(AtomicBool::new(false)),
            certificate_busy: Arc::new(AtomicBool::new(false)),
            daemon_uid: 0,
        })
    }
    /// Custom socket endpoints are not an authenticated macOS app transport.
    /// This signature remains available to shared clients but always refuses.
    #[cfg(target_os = "macos")]
    pub fn new(_path: impl AsRef<Path>) -> Result<Self, Error> {
        Err(Error::InvalidEndpoint)
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
                    return Ok((
                        identity,
                        permissions,
                        expiries,
                        last_process.pid.ok_or(Error::UntrustedLocalApi)?,
                    ));
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
    async fn status(&self, cx: &Cx) -> Result<(Status, UCred), Error> {
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
    ) -> Result<(Vec<u8>, UCred), Error> {
        let uri = match query {
            LocalQuery::Status { peers: true } => "/localapi/v0/status?peers=true",
            LocalQuery::Status { peers: false } => "/localapi/v0/status?peers=false",
        };
        self.get(uri, maximum).await
    }
    async fn whois(&self, addresses: ConnectionAddresses) -> Result<(WhoIs, UCred), Error> {
        let request = Request::get("/localapi/v0/whois")
            .query([("addr", addresses.peer.to_string())])
            .build();
        let (body, process) = self.get(&request.uri, metadata::WHOIS_BYTES).await?;
        Ok((WhoIs::parse(&body)?, process))
    }
    async fn get(&self, uri: &str, maximum: usize) -> Result<(Vec<u8>, UCred), Error> {
        self.get_typed(uri, maximum, "application/json").await
    }
    #[cfg(target_os = "linux")]
    async fn get_typed(
        &self,
        uri: &str,
        maximum: usize,
        content_type: &str,
    ) -> Result<(Vec<u8>, UCred), Error> {
        let socket = UnixStream::connect(&*self.path)
            .await
            .map_err(|_| Error::LocalApiUnavailable)?;
        let credentials = socket.peer_cred().map_err(|_| Error::UntrustedLocalApi)?;
        if credentials.uid != self.daemon_uid || credentials.pid.is_none_or(|pid| pid <= 0) {
            return Err(Error::UntrustedLocalApi);
        }
        let request = Request::get(uri)
            .header("Host", "local-tailscaled.sock")
            .header("Connection", "close")
            .header("Accept", content_type)
            .build();
        let (response, _socket, withheld) =
            Http1Client::request_with_io_and_max_body_size(socket, request, maximum)
                .await
                .map_err(|_| Error::Http)?;
        checked(response, withheld, content_type).map(|body| (body, credentials))
    }
    #[cfg(target_os = "macos")]
    #[allow(clippy::unused_self)] // The Linux transport needs the same receiver.
    fn get_typed(
        &self,
        _uri: &str,
        _maximum: usize,
        _content_type: &str,
    ) -> std::future::Ready<Result<(Vec<u8>, UCred), Error>> {
        // Only macos::{discover, peer_target} can read the signed app. Host
        // operations must not reuse outbound identity or invent socket UCred.
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
pub(crate) fn now(cx: &Cx) -> Result<u64, Error> {
    cx.checkpoint().map_err(|_| Error::Cancelled)?;
    Ok(cx
        .timer_driver()
        .ok_or(Error::MissingRuntime)?
        .now()
        .as_nanos()
        / 1000)
}
use crate::discovery::wall_now;
/// Pulse cancellation independently of a stalled HTTP read without restarting
/// that read or accumulating timers. Dropping this future drops its owned socket.
async fn bounded<T>(
    cx: &Cx,
    duration: Duration,
    operation: impl Future<Output = Result<T, Error>>,
) -> Result<T, Error> {
    let start = now(cx)?;
    let end = start
        .checked_add(u64::try_from(duration.as_micros()).map_err(|_| Error::InvalidPolicy)?)
        .ok_or(Error::Clock)?;
    let mut last = start;
    let mut pulse = Box::pin(sleep(
        Time::from_nanos(start.checked_mul(1000).ok_or(Error::Clock)?),
        Duration::from_millis(10).min(duration),
    ));
    let mut operation = pin!(operation);
    poll_fn(|task| {
        let current = match now(cx) {
            Ok(v) => v,
            Err(e) => return Poll::Ready(Err(e)),
        };
        if current < last {
            return Poll::Ready(Err(Error::Clock));
        }
        last = current;
        if current >= end {
            return Poll::Ready(Err(Error::Timeout));
        }
        let result = operation.as_mut().poll(task);
        // A poll can finish after the deadline or observe cancellation while
        // parsing a response. Check again before accepting output or waiting.
        let current = match now(cx) {
            Ok(v) => v,
            Err(e) => return Poll::Ready(Err(e)),
        };
        if current < last {
            return Poll::Ready(Err(Error::Clock));
        }
        last = current;
        if current >= end {
            return Poll::Ready(Err(Error::Timeout));
        }
        if let Poll::Ready(result) = result {
            return Poll::Ready(result);
        }
        if pulse.as_mut().poll(task).is_ready() {
            pulse = Box::pin(sleep(
                Time::from_nanos(current * 1000),
                Duration::from_micros((end - current).min(10_000)),
            ));
            // Time may have advanced during the operation poll, so this new
            // pulse can already be ready. Poll it on the next turn, where its
            // result is handled, rather than discarding Ready and repolling a
            // completed Sleep. The wake also ensures registration is not lost.
            task.waker().wake_by_ref();
        }
        Poll::Pending
    })
    .await
}

mod node;
pub use node::NodeIdentity;

#[cfg(all(test, target_os = "linux"))]
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

#[cfg(any(target_os = "macos", test))]
mod lookup_budget;
#[cfg(target_os = "macos")]
mod macos;

pub mod ingress;
