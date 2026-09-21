//! Bounded machine discovery from the installed daemon, never desktop admission.
use super::{LocalApi, Lookup, bounded, now, wall_now};
use crate::{Error, PeerSelector, metadata};
use asupersync::cx::Cx;
use std::{fmt, net::IpAddr, sync::atomic::Ordering, time::Duration};

/// Active network path to a discovered peer node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerTransportPath {
    /// Direct `WireGuard` UDP connection (optimal latency).
    Direct { cur_addr: String },
    /// Relayed via Tailscale DERP region (elevated latency and potential jitter).
    DerpRelayed { relay: String },
    /// Path not yet resolved or peer offline.
    Unknown,
}

impl PeerTransportPath {
    /// True if connection is actively traversing a DERP relay.
    #[must_use]
    pub const fn is_derp_relayed(&self) -> bool {
        matches!(self, Self::DerpRelayed { .. })
    }
}

/// One selectable machine. No desktop capability, online status, observation
/// permission or input authority is inferred from its name or tailnet address.
/// Connecting MUST resolve this stable ID again through `LocalApi::peer_target`.
pub struct DiscoveredPeer {
    id: String,
    name: String,
    addresses: Vec<IpAddr>,
    transport_path: PeerTransportPath,
}
impl DiscoveredPeer {
    pub fn stable_id(&self) -> &str {
        &self.id
    }
    pub fn certificate_name(&self) -> &str {
        &self.name
    }
    pub fn addresses(&self) -> &[IpAddr] {
        &self.addresses
    }
    pub fn transport_path(&self) -> &PeerTransportPath {
        &self.transport_path
    }
    pub fn from_parts(
        id: String,
        name: String,
        addresses: Vec<IpAddr>,
        transport_path: PeerTransportPath,
    ) -> Self {
        Self {
            id,
            name,
            addresses,
            transport_path,
        }
    }
}
impl fmt::Debug for DiscoveredPeer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DiscoveredPeer([machine metadata; not authority])")
    }
}
/// Counts explain an incomplete candidate list without logging excluded peer
/// identities. A malformed outer response still refuses the entire lookup.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DiscoveryExclusions {
    pub shared: usize,
    pub expired: usize,
    pub unusable: usize,
}
/// A single informational snapshot, not a cache, grant or dialable identity.
/// Bounds are inherited from the canonical parser: 1 MiB response, 1024 peers,
/// 128-byte IDs, 254-byte names and eight addresses per peer. No probes run.
pub struct Discovery {
    peers: Vec<DiscoveredPeer>,
    excluded: DiscoveryExclusions,
    started_us: u64,
    completed_us: u64,
}
impl Discovery {
    pub fn from_parts(
        peers: Vec<DiscoveredPeer>,
        excluded: DiscoveryExclusions,
        started_us: u64,
        completed_us: u64,
    ) -> Self {
        Self {
            peers,
            excluded,
            started_us,
            completed_us,
        }
    }
    pub fn peers(&self) -> &[DiscoveredPeer] {
        &self.peers
    }
    pub const fn excluded(&self) -> DiscoveryExclusions {
        self.excluded
    }
    pub const fn lookup_started_us(&self) -> u64 {
        self.started_us
    }
    pub const fn lookup_completed_us(&self) -> u64 {
        self.completed_us
    }
}
impl fmt::Debug for Discovery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Discovery")
            .field("count", &self.peers.len())
            .field("excluded", &self.excluded)
            .finish_non_exhaustive()
    }
}
impl LocalApi {
    /// Enumerate candidates using ONE credential-checked installed-daemon read.
    /// This shares the existing lookup exclusion and fixed one-second budget.
    /// Invalid/ambiguous/expired/shared candidates are counted and omitted, never
    /// guessed from names or prefixes. Host failure refuses the whole snapshot.
    /// The result deliberately cannot attach, dial or authorize a session.
    pub async fn discover(&self, cx: &Cx) -> Result<Discovery, Error> {
        let started_us = now(cx)?;
        let started_wall = wall_now()?;
        self.busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| Error::Busy)?;
        let _lookup = Lookup(self.busy.clone());
        Box::pin(bounded(cx, Duration::from_secs(1), async {
            let (status, _) = self.status().await?;
            let wall = wall_now()?;
            if wall < started_wall {
                return Err(Error::Clock);
            }
            let host = status.host_identity()?;
            check_expiry(host.host.expiry.as_ref().map(|e| e.0.as_str()), wall)?;
            let mut peers = Vec::new();
            peers
                .try_reserve_exact(status.peers.len())
                .map_err(|_| Error::MalformedMetadata)?;
            let mut excluded = DiscoveryExclusions::default();
            for peer in status.peers.values() {
                let transport_path = if let Some(ref relay) = peer.relay {
                    if relay.0.is_empty() {
                        PeerTransportPath::Unknown
                    } else {
                        PeerTransportPath::DerpRelayed {
                            relay: relay.0.clone(),
                        }
                    }
                } else if let Some(ref cur) = peer.cur_addr {
                    if cur.0.is_empty() {
                        PeerTransportPath::Unknown
                    } else {
                        PeerTransportPath::Direct {
                            cur_addr: cur.0.clone(),
                        }
                    }
                } else {
                    PeerTransportPath::Unknown
                };

                let result =
                    metadata::outbound::resolve(&status, PeerSelector::StableId(&peer.id.0))
                        .and_then(|target| {
                            check_expiry(target.peer.expiry.as_ref().map(|e| e.0.as_str()), wall)?;
                            Ok(DiscoveredPeer {
                                id: target.peer.id.0,
                                name: target.certificate_name,
                                addresses: target.peer.ips.0,
                                transport_path,
                            })
                        });
                match result {
                    Ok(peer) => peers.push(peer),
                    Err(Error::SharedPeer) => excluded.shared += 1,
                    Err(Error::KeyExpired) => excluded.expired += 1,
                    Err(_) => excluded.unusable += 1,
                }
            }
            peers.sort_unstable_by(|a, b| a.id.cmp(&b.id));
            let completed_us = now(cx)?;
            // Parsing/projecting is synchronous bounded work. Check its elapsed
            // time explicitly: a timer cannot interrupt a CPU-only projection.
            if completed_us < started_us {
                return Err(Error::Clock);
            }
            if completed_us - started_us >= 1_000_000 {
                return Err(Error::Timeout);
            }
            Ok(Discovery {
                peers,
                excluded,
                started_us,
                completed_us,
            })
        }))
        .await
    }
}
fn check_expiry(value: Option<&str>, wall: u64) -> Result<(), Error> {
    if let Some(expiry) = value.map(crate::expiry::unix_micros).transpose()?.flatten()
        && wall >= expiry
    {
        return Err(Error::KeyExpired);
    }
    Ok(())
}

#[cfg(test)]
#[path = "tests/discovery.rs"]
mod tests;
