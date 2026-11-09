//! Installed-app metadata reads for the native macOS client.
//!
//! Reads go to the standalone Tailscale app's loopback `LocalAPI`, the same
//! server its own CLI uses (`endpoint.rs` says what is trusted). The owner
//! chose this on 2026-11-08 over starting a signed CLI child per read: the
//! child itself connects to this port with this token, so it authenticated
//! only our own client binary, at 60-80 ms of CPU per read and three reads per
//! renewal. A whole lookup owns one original deadline and the global lookup
//! slot. The host's reads (status, whois, certificate pair) use the same
//! endpoint through [`get`], under the parent module's own bounds and slots.
mod endpoint;

use super::{LocalApi, Lookup, SourceIdentity, bounded, checked, lookup_budget::Clock, now};
use super::{target::VerifiedTarget, wall_now};
use crate::{Discovery, Error, PeerSelector, PeerTarget, metadata};
use asupersync::{
    cx::Cx,
    http::h1::{Http1Client, Request},
    net::TcpStream,
};
use std::{
    net::{IpAddr, SocketAddr},
    path::Path,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

static LOOKUP_BUSY: OnceLock<Arc<AtomicBool>> = OnceLock::new();
const LOOKUP: Duration = Duration::from_secs(1);

/// The fixed read vocabulary; there is no caller-supplied URI.
#[derive(Clone, Copy)]
enum Query {
    Status,
    WhoIs(IpAddr),
}

struct Operation {
    owner: Cx,
    clock: Clock,
    started: Instant,
}
impl Operation {
    fn new(cx: &Cx) -> Result<Self, Error> {
        let started = Instant::now();
        Ok(Self {
            owner: cx.clone(),
            clock: Clock::new(now(cx)?, wall_now()?)?,
            started,
        })
    }
    fn check(&mut self) -> Result<u64, Error> {
        let current = now(&self.owner)?;
        self.clock.check(current, wall_now()?)?;
        if self.started.elapsed() >= LOOKUP {
            return Err(Error::Timeout);
        }
        Ok(current)
    }

    async fn read(&mut self, query: Query) -> Result<(Vec<u8>, SourceIdentity), Error> {
        self.check()?;
        let (uri, maximum) = match query {
            Query::Status => (
                "/localapi/v0/status?peers=true".to_owned(),
                metadata::STATUS_BYTES,
            ),
            Query::WhoIs(ip) => (
                Request::get("/localapi/v0/whois")
                    .query([("addr", ip.to_string())])
                    .build()
                    .uri,
                metadata::WHOIS_BYTES,
            ),
        };
        let read = get(&uri, maximum, "application/json").await?;
        self.check()?;
        Ok(read)
    }
}

/// One read of an exact local URI over the installed app's loopback endpoint,
/// accepting only a complete 200 response of `content_type` within `maximum`.
/// The daemon run that published this endpoint stands in for Linux's daemon
/// PID: a restart between reads breaks a lookup's snapshot.
pub(super) async fn get(
    uri: &str,
    maximum: usize,
    content_type: &str,
) -> Result<(Vec<u8>, SourceIdentity), Error> {
    let endpoint = endpoint::endpoint(Path::new(endpoint::INSTALLED))?;
    let socket = TcpStream::connect(SocketAddr::from(([127, 0, 0, 1], endpoint.port)))
        .await
        .map_err(|_| Error::LocalApiUnavailable)?;
    let request = Request::get(uri)
        .header("Host", "local-tailscaled.sock")
        .header("Connection", "close")
        .header("Accept", content_type)
        .header("Authorization", endpoint.authorization())
        .build();
    let (response, _socket, withheld) =
        Http1Client::request_with_io_and_max_body_size(socket, request, maximum)
            .await
            .map_err(|_| Error::Http)?;
    let body = checked(response, withheld, content_type)?;
    Ok((body, endpoint.instance))
}

// The lookup owns the bounded selector; no borrowed caller data survives into
// a read.
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
    /// Use only the installed Tailscale app's root-published endpoint.
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

    async fn lookup<T>(
        &self,
        cx: &Cx,
        operation: impl AsyncFnOnce(&mut Operation) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let mut lookup = Operation::new(cx)?;
        self.busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| Error::Busy)?;
        let _permit = Lookup(self.busy.clone());
        // Dropping the work on timeout or cancellation drops its socket. Its
        // three HTTP exchanges live on the heap, not in every caller's future.
        bounded(
            cx,
            LOOKUP,
            Box::pin(async {
                let value = operation(&mut lookup).await?;
                lookup.check()?;
                Ok(value)
            }),
        )
        .await
    }

    /// Informational candidates from one bounded status read.
    /// No discovery value grants observation, input or native dial authority.
    pub async fn discover(&self, cx: &Cx) -> Result<Discovery, Error> {
        self.lookup(cx, async |lookup| {
            let (bytes, _) = lookup.read(Query::Status).await?;
            Discovery::from_status_json(
                &bytes,
                lookup.clock.issued_us,
                lookup.clock.wall_issued_us,
                || lookup.check(),
            )
        })
        .await
    }

    /// Pin the destination through a status/WhoIs/status sequence under one
    /// original second. Every read must come from the same daemon run. This
    /// is outbound identity only, never host admission.
    pub async fn peer_target(
        &self,
        cx: &Cx,
        selector: PeerSelector<'_>,
    ) -> Result<PeerTarget, Error> {
        let selector = Selector::new(selector)?;
        let verified = self
            .lookup(cx, async move |lookup| {
                let (first, source_identity) = lookup.read(Query::Status).await?;
                let first = metadata::outbound::resolve(
                    &metadata::Status::parse(&first)?,
                    selector.borrowed(),
                )?;
                let queried_address = match &selector {
                    Selector::Address(ip) => *ip,
                    _ => first.peer.ips.0[0], // resolve requires a nonempty node-owned set.
                };
                let (whois, whois_source) = lookup.read(Query::WhoIs(queried_address)).await?;
                if source_identity != whois_source {
                    return Err(Error::UntrustedLocalApi);
                }
                let whois_expiry = metadata::outbound::verify_whois(
                    &first,
                    queried_address,
                    &metadata::WhoIs::parse(&whois)?,
                )?;
                let (last, last_source) = lookup.read(Query::Status).await?;
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
