//! Read-only Linux `LocalAPI` over authenticated Unix sockets. No TCP fallback,
//! subprocess, ambient HTTP proxy, redirect, public listener, or mutation API.
use crate::{
    Authorization, ConnectionAddresses, Error, GrantPolicy, expiry,
    metadata::{self, Status, WhoIs},
};
use asupersync::{
    cx::Cx,
    http::h1::{Http1Client, Request},
    net::unix::{UCred, UnixStream},
    time::sleep,
    types::Time,
};
use std::{
    fmt,
    future::{Future, poll_fn},
    path::{Path, PathBuf},
    pin::{Pin, pin},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::Poll,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Clone)]
pub struct LocalApi {
    path: Arc<PathBuf>,
    origin: Arc<()>,
    busy: Arc<AtomicBool>,
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
    pub fn installed() -> Self {
        Self::new("/var/run/tailscale/tailscaled.sock").expect("fixed protected socket")
    }
    /// A locally configured absolute path. The kernel-reported server UID must
    /// be root even at a custom path. The selected host OS/root remain trusted.
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
            daemon_uid: 0,
        })
    }
    /// Exact endpoint tuple MUST originate at a TUN-restricted, established
    /// transport. This proves identity/grants, not the listener's ingress path.
    /// At most one lookup per shared client; at most two three-request attempts.
    pub async fn authorize_app_capability(
        &self,
        cx: &Cx,
        addresses: ConnectionAddresses,
        policy: GrantPolicy,
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
                    let (first, process) = self.status().await?;
                    first.validate(addresses)?;
                    let (who, who_process) = self.whois(addresses).await?;
                    let (last, last_process) = self.status().await?;
                    if first != last || process != who_process || process != last_process {
                        continue;
                    }
                    let (identity, permissions, expiries) =
                        metadata::evaluate(&last, &who, addresses, policy)?;
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
            .authorize_app_capability(cx, addresses, old.policy)
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
    async fn status(&self) -> Result<(Status, UCred), Error> {
        let (body, process) = self
            .get("/localapi/v0/status?peers=true", metadata::STATUS_BYTES)
            .await?;
        Ok((Status::parse(&body)?, process))
    }
    async fn whois(&self, addresses: ConnectionAddresses) -> Result<(WhoIs, UCred), Error> {
        let request = Request::get("/localapi/v0/whois")
            .query([("addr", addresses.peer.to_string())])
            .build();
        let (body, process) = self.get(&request.uri, metadata::WHOIS_BYTES).await?;
        Ok((WhoIs::parse(&body)?, process))
    }
    async fn get(&self, uri: &str, maximum: usize) -> Result<(Vec<u8>, UCred), Error> {
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
            .header("Accept", "application/json")
            .build();
        let (response, _socket, withheld) =
            Http1Client::request_with_io_and_max_body_size(socket, request, maximum)
                .await
                .map_err(|_| Error::Http)?;
        if response.status != 200 {
            return Err(Error::LocalApiDenied);
        }
        if withheld
            || !response.trailers.is_empty()
            || response.header_value("Content-Encoding").is_some()
            || !response.header_value("Content-Type").is_some_and(|v| {
                v.split(';')
                    .next()
                    .is_some_and(|m| m.trim().eq_ignore_ascii_case("application/json"))
            })
        {
            return Err(Error::Http);
        }
        Ok((response.body, credentials))
    }
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
fn wall_now() -> Result<u64, Error> {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| Error::Clock)?
            .as_micros(),
    )
    .map_err(|_| Error::Clock)
}
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
        if let Poll::Ready(result) = operation.as_mut().poll(task) {
            return Poll::Ready(result);
        }
        if pulse.as_mut().poll(task).is_ready() {
            pulse = Box::pin(sleep(
                Time::from_nanos(current * 1000),
                Duration::from_micros((end - current).min(10_000)),
            ));
            // Register the replacement timer before yielding again.
            let _ = Pin::as_mut(&mut pulse).poll(task);
        }
        Poll::Pending
    })
    .await
}

#[cfg(test)]
mod tests;
