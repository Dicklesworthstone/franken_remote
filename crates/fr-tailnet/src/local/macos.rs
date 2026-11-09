//! Signed installed-app metadata reads for the native macOS client.
//!
//! A whole lookup owns one drained blocking task, one original deadline and
//! the global lookup slot through actual child retirement. Only the fixed
//! platform command enum can cross the FFI boundary. Host-shaped APIs in the
//! parent module always refuse; this backend provides outbound identity only.
use super::{LocalApi, Lookup, lookup_budget::Clock, now, target::VerifiedTarget, wall_now};
use crate::{Discovery, Error, PeerSelector, PeerTarget, metadata};
use asupersync::{cx::Cx, time::timeout_at, types::Time};
use fr_tailnet_platform::{Command, Read, Snapshot, SourceIdentity};
use std::{
    future::{Future, poll_fn},
    net::IpAddr,
    pin::pin,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    task::Poll,
    time::{Duration, Instant},
};

static LOOKUP_BUSY: OnceLock<Arc<AtomicBool>> = OnceLock::new();

#[derive(Clone)]
struct Operation {
    owner: Cx,
    child: Option<Cx>,
    clock: Clock,
    started: Instant,
}
impl Operation {
    fn new(cx: &Cx) -> Result<Self, Error> {
        let started = Instant::now();
        Ok(Self {
            owner: cx.clone(),
            child: None,
            clock: Clock::new(now(cx)?, wall_now()?)?,
            started,
        })
    }
    fn check(&mut self) -> Result<u64, Error> {
        if let Some(child) = &self.child {
            child.checkpoint().map_err(|_| Error::Cancelled)?;
        }
        let current = now(&self.owner)?;
        self.clock.check(current, wall_now()?)?;
        if self.started.elapsed() >= Duration::from_secs(1) {
            return Err(Error::Timeout);
        }
        Ok(current)
    }

    fn read(&mut self, command: Command) -> Result<(Vec<u8>, SourceIdentity), Error> {
        self.check()?;
        let maximum = match command {
            Command::Status => metadata::STATUS_BYTES,
            Command::WhoIs(_) => metadata::WHOIS_BYTES,
        };
        let mut snapshot = Snapshot::start(command).map_err(native_error)?;
        // Dynamic signature verification may block. It never receives a new
        // budget, and this child cannot run if the original lookup has ended.
        self.check()?;
        let source = snapshot.source_identity();
        snapshot.resume().map_err(native_error)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(maximum)
            .map_err(|_| Error::MalformedMetadata)?;
        let mut buffer = [0; 8192];
        let mut eof = false;
        loop {
            self.check()?;
            let progress = match snapshot.read(&mut buffer).map_err(native_error)? {
                Read::Bytes(count) => {
                    if count > maximum - bytes.len() {
                        return Err(Error::MalformedMetadata);
                    }
                    bytes.extend_from_slice(&buffer[..count]);
                    true
                }
                Read::Eof => {
                    eof = true;
                    false
                }
                Read::Pending => false,
            };
            match snapshot.try_wait().map_err(native_error)? {
                Some(false) => return Err(Error::LocalApiDenied),
                Some(true) if eof => break,
                _ => {}
            }
            if !progress {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        // The complete original process group is retired before its metadata
        // can publish or the next command can claim the platform process slot.
        drop(snapshot);
        self.check()?;
        Ok((bytes, source))
    }
}

fn native_error(error: fr_tailnet_platform::Error) -> Error {
    match error {
        fr_tailnet_platform::Error::Unavailable => Error::LocalApiUnavailable,
        fr_tailnet_platform::Error::Untrusted => Error::UntrustedLocalApi,
        fr_tailnet_platform::Error::Busy => Error::Busy,
        fr_tailnet_platform::Error::InvalidEndpoint => Error::InvalidEndpoint,
    }
}

// The blocking task must own the bounded selector; no borrowed caller data or
// arbitrary process argument survives into a native operation.
enum Selector {
    StableId(String),
    Pinned(String, String),
    Name(String),
    Address(IpAddr),
}
impl Selector {
    fn new(value: PeerSelector<'_>) -> Result<Self, Error> {
        value.validate()?;
        Ok(match value {
            PeerSelector::StableId(id) => Self::StableId(id.into()),
            PeerSelector::Pinned {
                stable_id,
                certificate_name,
            } => Self::Pinned(stable_id.into(), certificate_name.into()),
            PeerSelector::Name(name) => Self::Name(name.into()),
            PeerSelector::Address(ip) => Self::Address(ip),
        })
    }
    fn borrowed(&self) -> PeerSelector<'_> {
        match self {
            Self::StableId(id) => PeerSelector::StableId(id),
            Self::Pinned(stable_id, certificate_name) => PeerSelector::Pinned {
                stable_id,
                certificate_name,
            },
            Self::Name(name) => PeerSelector::Name(name),
            Self::Address(ip) => PeerSelector::Address(*ip),
        }
    }
}

impl LocalApi {
    /// Use only the fixed, dynamically authenticated installed Tailscale app.
    /// Distinct API owners share the same one-lookup admission bound while
    /// retaining distinct origin identities for their outbound targets.
    pub fn installed() -> Self {
        Self {
            origin: Arc::new(()),
            busy: LOOKUP_BUSY
                .get_or_init(|| Arc::new(AtomicBool::new(false)))
                .clone(),
            certificate_busy: Arc::new(AtomicBool::new(false)),
        }
    }

    async fn lookup<T: Send + 'static>(
        &self,
        cx: &Cx,
        operation: impl FnOnce(&mut Operation) -> Result<T, Error> + Send + 'static,
    ) -> Result<T, Error> {
        let mut lookup = Operation::new(cx)?;
        let end = Time::from_nanos(
            lookup
                .clock
                .end_us()
                .checked_mul(1000)
                .ok_or(Error::Clock)?,
        );
        self.busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| Error::Busy)?;
        let permit = Lookup(self.busy.clone());
        let mut native = lookup.clone();
        let mut task = cx
            .spawn_blocking_drained(move |child| {
                // The slot is released only after operation's native owners
                // have dropped and actually reaped their original children.
                let _permit = permit;
                let _ambient = child.clone().set_current_restricted();
                native.child = Some(child);
                native.check()?;
                let value = operation(&mut native)?;
                native.check()?;
                Ok(value)
            })
            .map_err(|_| Error::MissingRuntime)?;
        let result = {
            let mut cancelled = pin!(cx.cancelled());
            let mut joined = pin!(timeout_at(end, task.join()));
            poll_fn(|context| {
                lookup.check()?;
                if cancelled.as_mut().poll(context).is_ready() {
                    return Poll::Ready(Err(Error::Cancelled));
                }
                let result = joined.as_mut().poll(context);
                lookup.check()?;
                match result {
                    Poll::Ready(Ok(Ok(value))) => Poll::Ready(value),
                    Poll::Ready(Ok(Err(asupersync::runtime::JoinError::Cancelled(_)))) => {
                        Poll::Ready(Err(Error::Cancelled))
                    }
                    Poll::Ready(Ok(Err(_))) => Poll::Ready(Err(Error::LocalApiUnavailable)),
                    Poll::Ready(Err(_)) => Poll::Ready(Err(Error::Timeout)),
                    Poll::Pending => Poll::Pending,
                }
            })
            .await
        };
        if result.is_err() && !task.is_finished() {
            // Cleanup may outlast the work deadline. It cannot renew metadata,
            // allow a late child to resume, or release the lookup slot early.
            task.abort();
            let _ = task.join().await;
        }
        result
    }

    /// Informational candidates from one signed, bounded status query.
    /// No discovery value grants observation, input or native dial authority.
    pub async fn discover(&self, cx: &Cx) -> Result<Discovery, Error> {
        self.lookup(cx, |lookup| {
            let (bytes, _) = lookup.read(Command::Status)?;
            Discovery::from_status_json(
                &bytes,
                lookup.clock.issued_us,
                lookup.clock.wall_issued_us,
                || lookup.check(),
            )
        })
        .await
    }

    /// Pin the destination through a signed status/WhoIs/status sequence under
    /// one original second. Each child must have the same complete verified
    /// code identity. This is outbound identity only, never host admission.
    pub async fn peer_target(
        &self,
        cx: &Cx,
        selector: PeerSelector<'_>,
    ) -> Result<PeerTarget, Error> {
        let selector = Selector::new(selector)?;
        let verified = self
            .lookup(cx, move |lookup| {
                let (first, source_identity) = lookup.read(Command::Status)?;
                let first = metadata::outbound::resolve(
                    &metadata::Status::parse(&first)?,
                    selector.borrowed(),
                )?;
                let queried_address = match &selector {
                    Selector::Address(ip) => *ip,
                    _ => first.peer.ips.0[0], // resolve requires a nonempty node-owned set.
                };
                let (whois, whois_source) = lookup.read(Command::WhoIs(queried_address))?;
                if source_identity != whois_source {
                    return Err(Error::UntrustedLocalApi);
                }
                let whois_expiry = metadata::outbound::verify_whois(
                    &first,
                    queried_address,
                    &metadata::WhoIs::parse(&whois)?,
                )?;
                let (last, last_source) = lookup.read(Command::Status)?;
                if source_identity != last_source {
                    return Err(Error::UntrustedLocalApi);
                }
                let last = metadata::outbound::resolve(
                    &metadata::Status::parse(&last)?,
                    selector.borrowed(),
                )?;
                if first != last {
                    return Err(Error::SnapshotChanged);
                }
                lookup.check()?;
                Ok(VerifiedTarget {
                    identity: last,
                    source_identity,
                    issued_us: lookup.clock.issued_us,
                    wall_issued_us: lookup.clock.wall_issued_us,
                    whois_expiry,
                })
            })
            .await?;
        self.verified_target(cx, verified)
    }
}
