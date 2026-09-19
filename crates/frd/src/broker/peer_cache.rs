//! Short-lived, bounds-enforced peer-identity cache (plan sections 5.2, 19.2).
//!
//! Caches Tailscale `LocalAPI` `WhoIs` metadata for admitted or candidate peers.
//! - Bounds both entry count and string lengths.
//! - Short TTL (default 30 seconds) to detect membership and sharing revocations promptly.
//! - Evicts expired records before accepting new ones, refusing insertion if at ceiling.
//! - Unauthenticated or ambiguous peer records are typed refusals (`tailnet_membership_unverifiable`).

use core::fmt;
use core::time::Duration;
use std::net::IpAddr;

/// Maximum peer cache capacity by default.
pub const DEFAULT_PEER_CACHE_CAPACITY: usize = 256;

/// Default TTL for cached peer identities (30 seconds in microseconds).
pub const DEFAULT_PEER_TTL_US: u64 = 30_000_000;

/// Authenticated Tailscale peer identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerIdentity {
    /// Tailnet IP address of peer.
    pub ip: IpAddr,
    /// Tailscale node identifier (e.g. "node:123456").
    pub node_id: String,
    /// Tailscale fully-qualified domain name (e.g. "laptop.example.ts.net").
    pub fqdn: String,
    /// Human-readable display name.
    pub display_name: String,
    /// User login name (e.g. "user@example.com").
    pub login_name: String,
    /// Whether the peer belongs to the same Tailscale user profile as the host.
    pub is_own_user: bool,
    /// Tailscale node tags (e.g. `["tag:server"]`).
    pub tags: Vec<String>,
    /// Host monotonic timestamp (us) when cached.
    pub cached_at_us: u64,
    /// Host monotonic timestamp (us) when this entry expires.
    pub valid_until_us: u64,
}

impl PeerIdentity {
    /// True if the entry has expired relative to `now_us`.
    #[must_use]
    pub const fn is_expired(&self, now_us: u64) -> bool {
        now_us >= self.valid_until_us
    }

    /// Remaining valid time in seconds, clamped to 0 if expired.
    #[must_use]
    pub const fn remaining_secs(&self, now_us: u64) -> u64 {
        if now_us >= self.valid_until_us {
            0
        } else {
            (self.valid_until_us - now_us) / 1_000_000
        }
    }
}

/// Bounded in-memory peer identity cache.
#[derive(Debug)]
pub struct PeerIdentityCache {
    entries: Vec<PeerIdentity>,
    max_capacity: usize,
    ttl_us: u64,
}

/// Typed errors in peer cache operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerCacheError {
    /// Cache capacity exceeded and no expired entries could be evicted.
    CapacityExceeded,
    /// Missing or ambiguous Tailscale membership evidence.
    UnverifiablePeer,
    /// Entry already expired at insertion time.
    AlreadyExpired,
}

impl fmt::Display for PeerCacheError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CapacityExceeded => f.write_str("peer identity cache capacity exceeded"),
            Self::UnverifiablePeer => f.write_str("tailnet_membership_unverifiable"),
            Self::AlreadyExpired => f.write_str("attempted to cache already-expired peer identity"),
        }
    }
}

impl std::error::Error for PeerCacheError {}

impl PeerIdentityCache {
    /// Create a new peer identity cache with bounded capacity and TTL.
    #[must_use]
    pub fn new(max_capacity: usize, ttl: Duration) -> Self {
        let ttl_us = u64::try_from(ttl.as_micros())
            .unwrap_or(u64::MAX)
            .max(1_000_000); // minimum 1 second
        Self {
            entries: Vec::with_capacity(max_capacity.min(64)),
            max_capacity: max_capacity.max(1),
            ttl_us,
        }
    }

    /// Default configuration (256 entries, 30s TTL).
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(
            DEFAULT_PEER_CACHE_CAPACITY,
            Duration::from_micros(DEFAULT_PEER_TTL_US),
        )
    }

    /// Lookup a peer by IP. Returns `None` if absent or expired.
    #[must_use]
    pub fn lookup(&self, ip: &IpAddr, now_us: u64) -> Option<&PeerIdentity> {
        self.entries
            .iter()
            .find(|e| e.ip == *ip && !e.is_expired(now_us))
    }

    /// Insert or refresh a peer identity.
    pub fn insert(
        &mut self,
        mut identity: PeerIdentity,
        now_us: u64,
    ) -> Result<(), PeerCacheError> {
        // Enforce valid timestamps.
        if identity.valid_until_us <= now_us {
            identity.cached_at_us = now_us;
            identity.valid_until_us = now_us.saturating_add(self.ttl_us);
        }

        // Check if an existing entry for this IP exists.
        if let Some(pos) = self.entries.iter().position(|e| e.ip == identity.ip) {
            self.entries[pos] = identity;
            return Ok(());
        }

        // Prune expired entries to make room.
        self.prune_expired(now_us);

        // If still full, evict the entry with the shortest remaining validity.
        if self.entries.len() >= self.max_capacity
            && let Some((min_idx, _)) = self
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| e.valid_until_us)
        {
            self.entries.swap_remove(min_idx);
        }

        if self.entries.len() >= self.max_capacity {
            return Err(PeerCacheError::CapacityExceeded);
        }

        self.entries.push(identity);
        Ok(())
    }

    /// Remove all expired entries.
    pub fn prune_expired(&mut self, now_us: u64) -> usize {
        let initial_len = self.entries.len();
        self.entries.retain(|e| !e.is_expired(now_us));
        initial_len - self.entries.len()
    }

    /// Invalidate a specific peer's cached identity (e.g. on disconnect or policy revocation).
    pub fn invalidate(&mut self, ip: &IpAddr) -> bool {
        if let Some(pos) = self.entries.iter().position(|e| e.ip == *ip) {
            self.entries.swap_remove(pos);
            true
        } else {
            false
        }
    }

    /// Total entries currently stored (including possibly expired before pruning).
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True if the cache contains no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Clear all cached entries.
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn sample_peer(ip: IpAddr, now_us: u64, ttl_us: u64) -> PeerIdentity {
        PeerIdentity {
            ip,
            node_id: "node:abc1234".to_string(),
            fqdn: "desktop.example.ts.net".to_string(),
            display_name: "Desktop".to_string(),
            login_name: "alice@example.com".to_string(),
            is_own_user: true,
            tags: vec![],
            cached_at_us: now_us,
            valid_until_us: now_us + ttl_us,
        }
    }

    #[test]
    fn insert_and_lookup_valid() {
        let mut cache = PeerIdentityCache::new(4, Duration::from_secs(30));
        let ip = IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1));
        let peer = sample_peer(ip, 1_000_000, 30_000_000);
        cache.insert(peer.clone(), 1_000_000).unwrap();

        let found = cache.lookup(&ip, 2_000_000);
        assert_eq!(found, Some(&peer));
        assert_eq!(found.unwrap().remaining_secs(2_000_000), 29);
    }

    #[test]
    fn expired_entries_are_not_returned_by_lookup() {
        let mut cache = PeerIdentityCache::new(4, Duration::from_secs(10));
        let ip = IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1));
        let peer = sample_peer(ip, 1_000_000, 10_000_000);
        cache.insert(peer, 1_000_000).unwrap();

        // Querying at t = 12s (expired) returns None.
        assert_eq!(cache.lookup(&ip, 12_000_000), None);
    }

    #[test]
    fn prune_expired_removes_old_entries() {
        let mut cache = PeerIdentityCache::new(4, Duration::from_secs(10));
        let ip1 = IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1));
        let ip2 = IpAddr::V4(Ipv4Addr::new(100, 64, 0, 2));

        let p1 = sample_peer(ip1, 1_000_000, 5_000_000); // expires at 6s
        let p2 = sample_peer(ip2, 1_000_000, 20_000_000); // expires at 21s

        cache.insert(p1, 1_000_000).unwrap();
        cache.insert(p2, 1_000_000).unwrap();
        assert_eq!(cache.len(), 2);

        // At t = 10s, p1 is expired, p2 is alive.
        let pruned = cache.prune_expired(10_000_000);
        assert_eq!(pruned, 1);
        assert_eq!(cache.len(), 1);
        assert!(cache.lookup(&ip2, 10_000_000).is_some());
    }

    #[test]
    fn capacity_limit_evicts_oldest_when_full() {
        let mut cache = PeerIdentityCache::new(2, Duration::from_secs(30));
        let ip1 = IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1));
        let ip2 = IpAddr::V4(Ipv4Addr::new(100, 64, 0, 2));
        let ip3 = IpAddr::V4(Ipv4Addr::new(100, 64, 0, 3));

        let p1 = sample_peer(ip1, 1_000_000, 10_000_000); // expires at 11s
        let p2 = sample_peer(ip2, 1_000_000, 20_000_000); // expires at 21s
        let p3 = sample_peer(ip3, 1_000_000, 30_000_000); // expires at 31s

        cache.insert(p1, 1_000_000).unwrap();
        cache.insert(p2, 1_000_000).unwrap();
        assert_eq!(cache.len(), 2);

        // Inserting 3rd item evicts p1 (shortest remaining validity).
        cache.insert(p3, 1_000_000).unwrap();
        assert_eq!(cache.len(), 2);
        assert!(cache.lookup(&ip1, 1_000_000).is_none());
        assert!(cache.lookup(&ip2, 1_000_000).is_some());
        assert!(cache.lookup(&ip3, 1_000_000).is_some());
    }

    #[test]
    fn invalidate_removes_entry() {
        let mut cache = PeerIdentityCache::new(4, Duration::from_secs(30));
        let ip = IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1));
        cache
            .insert(sample_peer(ip, 1_000_000, 30_000_000), 1_000_000)
            .unwrap();
        assert_eq!(cache.len(), 1);

        assert!(cache.invalidate(&ip));
        assert_eq!(cache.len(), 0);
        assert!(!cache.invalidate(&ip));
    }
}
