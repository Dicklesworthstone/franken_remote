//! In-memory, atomically rotated TLS credentials from the installed daemon.
//! This authenticates the HOST to a client. It neither proves tunnel ingress nor
//! admits a peer. The enclosing session must still apply its independent gates.
use super::{LocalApi, Lookup, NodeIdentity, bounded, now, wall_now};
use crate::Error;
use asupersync::{
    cx::Cx,
    net::quic_native::{handshake_driver::QuicHandshakeDriver, tls::QuicServerIdentityVerifier},
    time::sleep,
    tls::{Certificate, CertificateChain, PrivateKey, RootCertStore, TlsAcceptor},
    types::Time,
};
use std::{
    fmt,
    future::{Future, poll_fn},
    pin::pin,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    task::{Poll, Waker},
    time::Duration,
};

const PAIR_LIMIT: usize = 64 * 1024;
const MAX_CERTIFICATES: usize = 8;
const PARAMETER_LIMIT: usize = 4096;
const ALPN: &[u8] = b"fr-remote/0";
const SERVICE_PULSE: Duration = Duration::from_millis(10);

/// Explicit local provisioning policy. Calling the provisioning API can ask
/// tailscaled to obtain/renew a public certificate; it is NOT an identity read.
/// Tailscale owns its on-disk key cache. `FrankenRemote` retains no PEM files.
#[derive(Debug, Clone, Copy)]
pub struct CertificatePolicy {
    pub request_timeout: Duration,
    pub refresh_interval: Duration,
    pub retry_initial: Duration,
    pub retry_maximum: Duration,
}
impl Default for CertificatePolicy {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(60),
            refresh_interval: Duration::from_secs(3600),
            retry_initial: Duration::from_secs(5),
            retry_maximum: Duration::from_secs(300),
        }
    }
}
impl CertificatePolicy {
    fn validate(self) -> Result<(), Error> {
        if self.request_timeout < Duration::from_millis(100)
            || self.request_timeout > Duration::from_secs(120)
            || self.refresh_interval < Duration::from_secs(1)
            || self.refresh_interval > Duration::from_secs(3600)
            || self.retry_initial < Duration::from_secs(1)
            || self.retry_maximum < self.retry_initial
            || self.retry_maximum > Duration::from_secs(300)
        {
            return Err(Error::InvalidPolicy);
        }
        Ok(())
    }
}

/// Sanitized scheduling information, never certificate material or a hostname.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CredentialStatus {
    pub generation: u64,
    pub next_refresh_us: u64,
}
struct Pair {
    chain: CertificateChain,
    acceptor: TlsAcceptor,
}
impl Pair {
    fn parse(bytes: &[u8]) -> Result<Self, Error> {
        let split = check_pem(bytes)?;
        let key = PrivateKey::from_pem(&bytes[..split]).map_err(|_| Error::CertificateRejected)?;
        let certificates =
            Certificate::from_pem(&bytes[split..]).map_err(|_| Error::CertificateRejected)?;
        if certificates.is_empty() || certificates.len() > MAX_CERTIFICATES {
            return Err(Error::CertificateRejected);
        }
        let chain = CertificateChain::from(certificates);
        let acceptor = TlsAcceptor::builder(chain.clone(), key)
            .alpn_protocols_required(vec![ALPN.to_vec()])
            .disable_early_data()
            .min_protocol_version(0x0304u16.into())
            .max_protocol_version(0x0304u16.into())
            .build()
            .map_err(|_| Error::CertificateRejected)?;
        Ok(Self { chain, acceptor })
    }
    fn verify(&self, verifier: &QuicServerIdentityVerifier, name: &str) -> Result<(), Error> {
        // Use the same time source rustls will use, not a guessed 90-day lifetime
        // or tailscaled's cache age. WebPKI checks chain, DNS name and validity.
        let time = self
            .acceptor
            .config()
            .time_provider
            .current_time()
            .ok_or(Error::Clock)?;
        verifier
            .verify_server_chain(name, self.chain.clone(), time)
            .map_err(|_| Error::CertificateRejected)?;
        Ok(())
    }
}

struct State {
    anchor: Arc<NodeIdentity>,
    active: Option<Arc<Pair>>,
    status: CredentialStatus,
    retry_us: u64,
    last_us: u64,
    last_wall_us: u64,
    renewing: bool,
}
impl State {
    fn check(&mut self, current: u64, wall: u64) -> Result<(), Error> {
        if self.active.is_none() {
            return Err(Error::Revoked);
        }
        if current < self.last_us || wall < self.last_wall_us {
            self.active = None;
            return Err(Error::Clock);
        }
        self.last_us = current;
        self.last_wall_us = wall;
        Ok(())
    }
}

/// One service and one wake slot per shared credential lifetime. Registering
/// under the mutex and checking stop before releasing it prevents a lost wake.
#[derive(Default)]
struct ServiceWake {
    claimed: AtomicBool,
    stopped: AtomicBool,
    waker: Mutex<Option<Waker>>,
}
impl ServiceWake {
    fn claim(&self) -> Result<(), Error> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(Error::Revoked);
        }
        self.claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| Error::Busy)?;
        if self.stopped.load(Ordering::Acquire) {
            self.claimed.store(false, Ordering::Release);
            return Err(Error::Revoked);
        }
        Ok(())
    }
    fn register(&self, waker: &Waker) -> Result<bool, Error> {
        let mut slot = self.waker.lock().map_err(|_| Error::Revoked)?;
        if self.stopped.load(Ordering::Acquire) {
            return Ok(false);
        }
        if slot.as_ref().is_none_or(|old| !old.will_wake(waker)) {
            *slot = Some(waker.clone());
        }
        Ok(true)
    }
    fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        let wake = self.waker.lock().ok().and_then(|mut slot| slot.take());
        // Never call arbitrary executor code while holding our mutex.
        if let Some(waker) = wake {
            waker.wake();
        }
    }
}

/// One shared credential lifetime with one complete active TLS configuration.
/// Renewal never replaces half a pair or blocks new handshakes behind `LocalAPI`
/// I/O. Clones share stop, backoff, generation and atomic publication.
#[derive(Clone)]
pub struct NativeServerIdentity {
    api: LocalApi,
    verifier: Arc<QuicServerIdentityVerifier>,
    policy: CertificatePolicy,
    state: Arc<Mutex<State>>,
    service: Arc<ServiceWake>,
}
impl fmt::Debug for NativeServerIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NativeServerIdentity([protected credentials])")
    }
}
impl LocalApi {
    /// Provision ONLY the name obtained from authenticated self metadata. Roots
    /// must come from locally configured platform trust, never the returned pair
    /// or an incoming peer. HTTPS/certificate issuance must be enabled by the
    /// operator; there is no self-signed, TOFU, or accept-any fallback.
    pub async fn native_server_identity(
        &self,
        cx: &Cx,
        roots: RootCertStore,
        policy: CertificatePolicy,
    ) -> Result<NativeServerIdentity, Error> {
        policy.validate()?;
        let verifier = Arc::new(
            QuicServerIdentityVerifier::from_root_store(roots)
                .map_err(|_| Error::InvalidTrustStore)?,
        );
        let (node, pair) = self.load_pair(cx, &verifier, policy, None).await?;
        let current = now(cx)?;
        let state = State {
            anchor: Arc::new(node),
            active: Some(Arc::new(pair)),
            status: CredentialStatus {
                generation: 1,
                next_refresh_us: later(current, policy.refresh_interval)?,
            },
            retry_us: micros(policy.retry_initial)?,
            last_us: current,
            last_wall_us: wall_now()?,
            renewing: false,
        };
        Ok(NativeServerIdentity {
            api: self.clone(),
            verifier,
            policy,
            state: Arc::new(Mutex::new(state)),
            service: Arc::new(ServiceWake::default()),
        })
    }
    async fn load_pair(
        &self,
        cx: &Cx,
        verifier: &QuicServerIdentityVerifier,
        policy: CertificatePolicy,
        anchor: Option<&NodeIdentity>,
    ) -> Result<(NodeIdentity, Pair), Error> {
        now(cx)?;
        self.certificate_busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| Error::Busy)?;
        let _slot = Lookup(self.certificate_busy.clone());
        // The independent certificate slot MUST NOT occupy the identity lookup
        // slot during ACME I/O: admitted media/input need their short renewals.
        Box::pin(bounded(cx, policy.request_timeout, async {
            let before = self.node_identity(cx).await?;
            if anchor.is_some_and(|old| !old.same_node(&before)) {
                return Err(Error::IdentityChanged);
            }
            let uri = format!(
                "/localapi/v0/cert/{}?type=pair&min_validity=24h",
                before.certificate_name()
            );
            let (bytes, process) = self.get_typed(&uri, PAIR_LIMIT, "text/plain").await?;
            let bytes = Secret(bytes);
            if !before.matches_process(process.pid) {
                return Err(Error::UntrustedLocalApi);
            }
            let after = self.node_identity(cx).await?;
            // Issuance may outlive the original three-second metadata snapshot.
            // Fresh metadata is required after issuance; matching immutable
            // provenance is NOT renewal of that old snapshot or of any session.
            if !before.same_node(&after) {
                return Err(Error::IdentityChanged);
            }
            let pair = Pair::parse(&bytes.0)?;
            pair.verify(verifier, after.certificate_name())?;
            self.check_node(cx, &after)?;
            Ok((after, pair))
        }))
        .await
    }
}
impl NativeServerIdentity {
    fn state(&self) -> Result<MutexGuard<'_, State>, Error> {
        self.state.lock().map_err(|_| Error::Revoked)
    }
    pub fn status(&self, cx: &Cx) -> Result<CredentialStatus, Error> {
        let mut state = self.state()?;
        state.check(now(cx)?, wall_now()?)?;
        Ok(state.status)
    }
    /// Stop is shared and terminal. A pending successful renewal cannot revive
    /// it. Already-created connections retain their independent admission and
    /// shutdown owners; dropping a certificate config does not revoke a peer.
    /// A running renewal service is also woken to drop its pending API request.
    pub fn stop(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.active = None;
        }
        self.service.stop();
    }
    /// Create the actual TLS 1.3 native QUIC handshake driver. Each new handshake
    /// requires fresh self metadata, the original daemon/node, valid `WebPKI` trust
    /// and hostname, and an unexpired certificate. No raw key/config is exported.
    pub fn quic_handshake(
        &self,
        cx: &Cx,
        node: &NodeIdentity,
        transport_parameters: Vec<u8>,
    ) -> Result<QuicHandshakeDriver, Error> {
        self.api.check_node(cx, node)?;
        if transport_parameters.len() > PARAMETER_LIMIT {
            return Err(Error::InvalidPolicy);
        }
        let mut state = self.state()?;
        state.check(now(cx)?, wall_now()?)?;
        if !state.anchor.same_node(node) {
            state.active = None;
            return Err(Error::IdentityChanged);
        }
        let pair = state.active.as_ref().ok_or(Error::Revoked)?;
        pair.verify(&self.verifier, node.certificate_name())?;
        let driver =
            QuicHandshakeDriver::server(pair.acceptor.config().clone(), transport_parameters)
                .map_err(|_| Error::CertificateRejected)?;
        self.api.check_node(cx, node)?;
        state.check(now(cx)?, wall_now()?)?;
        Ok(driver)
    }
    /// Own automatic renewal on the caller's existing structured task. Claiming
    /// occurs before the returned future is polled; dropping even an unpolled
    /// service stops this credential lifetime. A second service returns `Busy`.
    ///
    /// Explicit `stop` completes successfully. Cancellation, clock faults and
    /// changed identity are terminal errors. Transient issuance failures retain
    /// the previous complete pair, subject to its independently checked validity,
    /// and retry only on the existing bounded schedule. Nothing is spawned here.
    pub fn serve_renewal<'a>(
        &'a self,
        cx: &'a Cx,
    ) -> Result<impl Future<Output = Result<(), Error>> + 'a, Error> {
        self.status(cx)?;
        self.service.claim()?;
        let guard = ServiceGuard(self.clone());
        Ok(async move {
            let _guard = guard;
            self.drive_renewal(cx).await
        })
    }
    async fn drive_renewal(&self, cx: &Cx) -> Result<(), Error> {
        let mut operation = pin!(self.renewal_loop(cx));
        let mut pulse = Box::pin(sleep(cx.now(), SERVICE_PULSE));
        poll_fn(|task| {
            match self.service.register(task.waker()) {
                Ok(false) => return Poll::Ready(Ok(())),
                Err(error) => return Poll::Ready(Err(error)),
                Ok(true) => {}
            }
            if let Err(error) = self.status(cx) {
                return Poll::Ready(Err(error));
            }
            let result = operation.as_mut().poll(task);
            if self.service.stopped.load(Ordering::Acquire) {
                return Poll::Ready(Ok(()));
            }
            // Recheck cancellation and authority after a poll that may have
            // completed native certificate verification or an HTTP response.
            if let Err(error) = self.status(cx) {
                return Poll::Ready(Err(error));
            }
            if let Poll::Ready(result) = result {
                return Poll::Ready(result);
            }
            if pulse.as_mut().poll(task).is_ready() {
                pulse = Box::pin(sleep(cx.now(), SERVICE_PULSE));
                task.waker().wake_by_ref();
            }
            Poll::Pending
        })
        .await
    }
    async fn renewal_loop(&self, cx: &Cx) -> Result<(), Error> {
        loop {
            let status = self.status(cx)?;
            let current = now(cx)?;
            if current < status.next_refresh_us {
                sleep(
                    Time::from_nanos(current.checked_mul(1000).ok_or(Error::Clock)?),
                    Duration::from_micros(status.next_refresh_us - current),
                )
                .await;
                continue;
            }
            match self.refresh(cx).await {
                Ok(()) | Err(Error::CertificateNotDue) => {}
                Err(Error::Busy) => {
                    // A manual renewal or another identity sharing this API
                    // may own the bounded issuance slot. Never retry in a spin.
                    sleep(cx.now(), self.policy.retry_initial).await;
                }
                Err(
                    Error::LocalApiUnavailable
                    | Error::Timeout
                    | Error::LocalApiDenied
                    | Error::Http
                    | Error::MalformedMetadata
                    | Error::SnapshotChanged
                    | Error::BackendNotRunning
                    | Error::CertificateRejected,
                ) => {
                    // refresh reserved its next deadline before awaiting. Do
                    // not reset that deadline or extend the old certificate.
                }
                Err(error) => return Err(error),
            }
        }
    }
    /// Poll at `next_refresh_us`. Failed/cancelled renewal leaves the complete old
    /// pair available only while it remains valid, with capped exponential retry.
    /// Slow `LocalAPI` work holds no credential-state lock. No detached tasks.
    pub async fn refresh(&self, cx: &Cx) -> Result<(), Error> {
        let (anchor, generation) = {
            let mut state = self.state()?;
            let current = now(cx)?;
            state.check(current, wall_now()?)?;
            if state.renewing {
                return Err(Error::Busy);
            }
            if current < state.status.next_refresh_us {
                return Err(Error::CertificateNotDue);
            }
            // Reserve backoff before awaiting so abandonment cannot hot-loop.
            state.status.next_refresh_us =
                current.checked_add(state.retry_us).ok_or(Error::Clock)?;
            state.retry_us = state
                .retry_us
                .saturating_mul(2)
                .min(micros(self.policy.retry_maximum)?);
            state.renewing = true;
            (state.anchor.clone(), state.status.generation)
        };
        let _renewal = Renewal(self.state.clone());
        let candidate = self
            .api
            .load_pair(cx, &self.verifier, self.policy, Some(&anchor))
            .await;
        if matches!(
            candidate,
            Err(Error::IdentityChanged | Error::UntrustedLocalApi)
        ) {
            self.stop();
        }
        let (node, pair) = candidate?;
        let mut state = self.state()?;
        state.check(now(cx)?, wall_now()?)?;
        self.api.check_node(cx, &node)?;
        if state.status.generation != generation {
            return Err(Error::SnapshotChanged);
        }
        if !state.anchor.same_node(&node) {
            state.active = None;
            return Err(Error::IdentityChanged);
        }
        pair.verify(&self.verifier, node.certificate_name())?;
        let generation = state.status.generation.checked_add(1).ok_or(Error::Clock)?;
        let next_refresh_us = later(now(cx)?, self.policy.refresh_interval)?;
        state.active = Some(Arc::new(pair));
        state.status = CredentialStatus {
            generation,
            next_refresh_us,
        };
        state.retry_us = micros(self.policy.retry_initial)?;
        Ok(())
    }
}
struct ServiceGuard(NativeServerIdentity);
impl Drop for ServiceGuard {
    fn drop(&mut self) {
        self.0.stop();
        self.0.service.claimed.store(false, Ordering::Release);
    }
}
struct Renewal(Arc<Mutex<State>>);
impl Drop for Renewal {
    fn drop(&mut self) {
        if let Ok(mut state) = self.0.lock() {
            state.renewing = false;
        }
    }
}
fn micros(value: Duration) -> Result<u64, Error> {
    u64::try_from(value.as_micros()).map_err(|_| Error::InvalidPolicy)
}
fn later(current: u64, value: Duration) -> Result<u64, Error> {
    current.checked_add(micros(value)?).ok_or(Error::Clock)
}
struct Secret(Vec<u8>);
impl Drop for Secret {
    fn drop(&mut self) {
        // Best-effort clearing, not a claim of memory-dump/allocator isolation.
        self.0.fill(0);
    }
}
/// The API's pair response is exactly one key followed by a bounded chain.
/// Do not let a permissive PEM reader skip hidden keys, text or unknown blocks.
fn check_pem(bytes: &[u8]) -> Result<usize, Error> {
    if bytes.len() > PAIR_LIMIT {
        return Err(Error::CertificateRejected);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| Error::CertificateRejected)?;
    let mut remaining = text.trim_start_matches(|c: char| c.is_ascii_whitespace());
    let mut split = None;
    let mut count = 0usize;
    while !remaining.is_empty() {
        let label = if split.is_none() {
            ["PRIVATE KEY", "RSA PRIVATE KEY", "EC PRIVATE KEY"]
                .into_iter()
                .find(|label| remaining.starts_with(&format!("-----BEGIN {label}-----")))
                .ok_or(Error::CertificateRejected)?
        } else {
            if count == MAX_CERTIFICATES {
                return Err(Error::CertificateRejected);
            }
            "CERTIFICATE"
        };
        let begin = format!("-----BEGIN {label}-----");
        let end = format!("-----END {label}-----");
        let body = remaining
            .strip_prefix(&begin)
            .ok_or(Error::CertificateRejected)?;
        let stop = body.find(&end).ok_or(Error::CertificateRejected)?;
        if body[..stop].is_empty()
            || !body[..stop].bytes().all(|b| {
                b.is_ascii_alphanumeric() || b.is_ascii_whitespace() || b"+/=".contains(&b)
            })
        {
            return Err(Error::CertificateRejected);
        }
        remaining = body[stop + end.len()..].trim_start_matches(|c: char| c.is_ascii_whitespace());
        if split.is_none() {
            split = Some(text.len() - remaining.len());
        } else {
            count += 1;
        }
    }
    if count == 0 {
        return Err(Error::CertificateRejected);
    }
    split.ok_or(Error::CertificateRejected)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod service_tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::task::Wake;

    #[derive(Default)]
    struct Count(AtomicUsize);
    impl Wake for Count {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::AcqRel);
        }
    }

    #[test]
    fn one_service_claim_cannot_be_reused_after_stop() {
        let service = ServiceWake::default();
        assert_eq!(service.claim(), Ok(()));
        assert_eq!(service.claim(), Err(Error::Busy));
        service.stop();
        service.claimed.store(false, Ordering::Release);
        assert_eq!(service.claim(), Err(Error::Revoked));
    }

    #[test]
    fn stop_wakes_the_registered_owner_once_and_retires_the_slot() {
        let service = ServiceWake::default();
        let count = Arc::new(Count::default());
        let waker = Waker::from(count.clone());
        assert_eq!(service.register(&waker), Ok(true));
        assert_eq!(service.register(&waker), Ok(true));
        service.stop();
        service.stop();
        assert_eq!(count.0.load(Ordering::Acquire), 1);
        assert!(service.waker.lock().unwrap().is_none());
        assert_eq!(service.register(&waker), Ok(false));
    }

    #[test]
    fn stop_before_registration_cannot_leave_a_waiting_owner() {
        let service = ServiceWake::default();
        service.stop();
        let count = Arc::new(Count::default());
        assert_eq!(service.register(&Waker::from(count.clone())), Ok(false));
        assert!(service.waker.lock().unwrap().is_none());
        assert_eq!(count.0.load(Ordering::Acquire), 0);
    }

    #[test]
    fn changing_executor_waker_keeps_only_the_latest_waiter() {
        let service = ServiceWake::default();
        let old = Arc::new(Count::default());
        let current = Arc::new(Count::default());
        assert_eq!(service.register(&Waker::from(old.clone())), Ok(true));
        assert_eq!(service.register(&Waker::from(current.clone())), Ok(true));
        service.stop();
        assert_eq!(old.0.load(Ordering::Acquire), 0);
        assert_eq!(current.0.load(Ordering::Acquire), 1);
    }
}
