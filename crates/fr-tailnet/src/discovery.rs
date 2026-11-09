//! Bounded informational snapshots; parsing does not authenticate their source.
use crate::{Error, PeerSelector, metadata};
use std::{fmt, net::IpAddr};

/// Active network path to a discovered peer node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerTransportPath {
    /// Direct `WireGuard` UDP connection (optimal latency).
    Direct { cur_addr: String },
    /// Recent traffic with no direct endpoint: relayed via this Tailscale DERP
    /// region (elevated latency and potential jitter).
    DerpRelayed { relay: String },
    /// No current path: idle (no recent traffic), offline, or not yet resolved.
    /// A home DERP region alone (`Relay`) is not evidence of relaying.
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
impl Discovery {
    /// Maximum JSON bytes retained by either installed-platform reader.
    pub const MAX_STATUS_BYTES: usize = metadata::STATUS_BYTES;

    /// Project bounded machine-readable Tailscale status into candidates.
    /// The caller must authenticate its local data source separately; this
    /// method grants no observation, control, attachment or dial authority.
    /// Sample both starts before fetching the status. `completed_clock` must
    /// read the same monotonic domain after parsing and projection finish.
    pub fn from_status_json(
        bytes: &[u8],
        started_us: u64,
        started_wall_us: u64,
        completed_clock: impl FnOnce() -> Result<u64, Error>,
    ) -> Result<Self, Error> {
        let status = metadata::Status::parse(bytes)?;
        let wall = wall_now()?;
        if wall < started_wall_us {
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
            let transport_path = transport_path(peer);

            let result = metadata::outbound::resolve(&status, PeerSelector::StableId(&peer.id.0))
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
        let completed_us = completed_clock()?;
        if completed_us < started_us {
            return Err(Error::Clock);
        }
        if completed_us - started_us >= 1_000_000 {
            return Err(Error::Timeout);
        }
        Ok(Self {
            peers,
            excluded,
            started_us,
            completed_us,
        })
    }
}
/// `CurAddr` is the direct endpoint in use; `Relay` alone names the peer's
/// home DERP region. A relay is the sampled route only when the peer is active
/// without a direct endpoint. Discovery and authenticated targets use this
/// same projection, without claiming a live FR connection.
pub(crate) fn transport_path(peer: &metadata::Peer) -> PeerTransportPath {
    match (&peer.cur_addr, &peer.relay) {
        (Some(cur), _) if !cur.0.is_empty() => PeerTransportPath::Direct {
            cur_addr: cur.0.clone(),
        },
        (_, Some(relay)) if peer.active && !relay.0.is_empty() => PeerTransportPath::DerpRelayed {
            relay: relay.0.clone(),
        },
        _ => PeerTransportPath::Unknown,
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

pub(crate) fn wall_now() -> Result<u64, Error> {
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| Error::Clock)?
            .as_micros(),
    )
    .map_err(|_| Error::Clock)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn status() -> Value {
        // Existing sanitized fixture, not macOS installation qualification.
        let mut status: Value =
            serde_json::from_str(include_str!("../tests/fixtures/linux-1.102.3/status.json"))
                .unwrap();
        status["Self"]["DNSName"] = json!("local.fixture.ts.net.");
        let peer = status["Peer"]
            .as_object_mut()
            .unwrap()
            .values_mut()
            .next()
            .unwrap();
        peer["DNSName"] = json!("remote.fixture.ts.net.");
        peer.as_object_mut().unwrap().remove("KeyExpiry");
        status
    }
    fn read(status: &Value) -> Result<Discovery, Error> {
        Discovery::from_status_json(&serde_json::to_vec(status).unwrap(), 7, 0, || Ok(8))
    }
    #[test]
    fn shared_projection_preserves_candidates_and_measured_path_semantics() {
        for (active, address, relay, path) in [
            (false, "", "nyc", "unknown"),
            (true, "", "nyc", "derp"),
            (true, "192.0.2.1:1234", "nyc", "direct"),
        ] {
            let mut source = status();
            let peer = source["Peer"]
                .as_object_mut()
                .unwrap()
                .values_mut()
                .next()
                .unwrap();
            peer["Active"] = json!(active);
            peer["CurAddr"] = json!(address);
            peer["Relay"] = json!(relay);
            let snapshot = read(&source).unwrap();
            assert_eq!(snapshot.lookup_started_us(), 7);
            assert_eq!(snapshot.lookup_completed_us(), 8);
            assert_eq!(snapshot.excluded(), DiscoveryExclusions::default());
            let [peer] = snapshot.peers() else {
                panic!("one candidate")
            };
            assert_eq!(peer.stable_id(), "n-2");
            assert_eq!(peer.certificate_name(), "remote.fixture.ts.net");
            assert_eq!(peer.addresses().len(), 2);
            assert_eq!(
                match peer.transport_path() {
                    PeerTransportPath::Direct { .. } => "direct",
                    PeerTransportPath::DerpRelayed { .. } => "derp",
                    PeerTransportPath::Unknown => "unknown",
                },
                path
            );
            for secret in ["fixture", "100.64", "nodekey:", "n-2"] {
                assert!(!format!("{snapshot:?} {peer:?}").contains(secret));
            }
        }
    }
    #[test]
    fn shared_projection_refuses_bad_status_and_excludes_unusable_peers() {
        for (field, value, expected) in [
            ("ShareeNode", json!(true), (1, 0, 0)),
            ("Expired", json!(true), (0, 1, 0)),
            ("DNSName", json!("foreign.invalid"), (0, 0, 1)),
        ] {
            let mut source = status();
            source["Peer"]
                .as_object_mut()
                .unwrap()
                .values_mut()
                .next()
                .unwrap()[field] = value;
            let snapshot = read(&source).unwrap();
            assert!(snapshot.peers().is_empty());
            let excluded = snapshot.excluded();
            assert_eq!(
                (excluded.shared, excluded.expired, excluded.unusable),
                expected
            );
        }
        let mut stopped = status();
        stopped["BackendState"] = json!("NeedsLogin");
        assert_eq!(read(&stopped).unwrap_err(), Error::BackendNotRunning);
        for bytes in [
            b"{\"Peer\":{},\"Peer\":{}}".to_vec(),
            vec![b' '; Discovery::MAX_STATUS_BYTES + 1],
        ] {
            assert_eq!(
                Discovery::from_status_json(&bytes, 0, 0, || Ok(1)).unwrap_err(),
                Error::MalformedMetadata
            );
        }
    }
    #[test]
    fn parsing_cannot_publish_a_late_cancelled_or_reversed_clock_snapshot() {
        let bytes = serde_json::to_vec(&status()).unwrap();
        for (completed, expected) in [(6, Error::Clock), (1_000_007, Error::Timeout)] {
            assert_eq!(
                Discovery::from_status_json(&bytes, 7, 0, || Ok(completed)).unwrap_err(),
                expected
            );
        }
        assert_eq!(
            Discovery::from_status_json(&bytes, 7, 0, || Err(Error::Cancelled)).unwrap_err(),
            Error::Cancelled
        );
        assert_eq!(
            Discovery::from_status_json(&bytes, 7, u64::MAX, || Ok(8)).unwrap_err(),
            Error::Clock
        );
    }
}
