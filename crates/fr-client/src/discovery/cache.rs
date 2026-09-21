//! Bounded discovery cache indexed by stable node identity with TTL and tailnet generation fencing.
//!
//! Per plan section 6.4:
//! - Results are cached by stable node identity with an expiry.
//! - Cache is invalidated on any tailnet change.
//! - Bounded capacity (max 256 entries) to prevent unbounded memory growth.

use super::DiscoveredHost;
use std::collections::HashMap;

/// Maximum number of discovered hosts retained in cache.
pub const MAX_CACHE_ENTRIES: usize = 256;

/// Default time-to-live for a cached discovery state: 60 seconds (in microseconds).
pub const DEFAULT_TTL_US: u64 = 60_000_000;

/// A cached entry with metadata for invalidation and expiration.
#[derive(Debug, Clone)]
struct CacheEntry {
    host: DiscoveredHost,
    inserted_at_us: u64,
    generation: u64,
}

/// Bounded cache of discovered hosts with tailnet generation fencing and TTL.
#[derive(Debug, Clone)]
pub struct DiscoveryCache {
    entries: HashMap<String, CacheEntry>,
    tailnet_generation: u64,
    ttl_us: u64,
    max_entries: usize,
}

impl Default for DiscoveryCache {
    fn default() -> Self {
        Self::new()
    }
}

impl DiscoveryCache {
    /// Create a new empty discovery cache with default settings.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            tailnet_generation: 1,
            ttl_us: DEFAULT_TTL_US,
            max_entries: MAX_CACHE_ENTRIES,
        }
    }

    /// Create a cache with custom TTL and capacity (useful for testing).
    #[must_use]
    pub fn with_limits(ttl_us: u64, max_entries: usize) -> Self {
        Self {
            entries: HashMap::new(),
            tailnet_generation: 1,
            ttl_us,
            max_entries: max_entries.min(MAX_CACHE_ENTRIES),
        }
    }

    /// Current tailnet generation counter.
    #[must_use]
    pub fn tailnet_generation(&self) -> u64 {
        self.tailnet_generation
    }

    /// Configured TTL in microseconds.
    #[must_use]
    pub fn ttl_us(&self) -> u64 {
        self.ttl_us
    }

    /// Number of valid, unexpired entries in the cache at `now_us`.
    #[must_use]
    pub fn valid_count(&self, now_us: u64) -> usize {
        self.entries
            .values()
            .filter(|e| self.is_entry_valid(e, now_us))
            .count()
    }

    /// Total number of entries physically stored (including expired).
    #[must_use]
    pub fn raw_count(&self) -> usize {
        self.entries.len()
    }

    /// True if the cache has no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Check if a specific entry is valid (generation matches and TTL not exceeded).
    fn is_entry_valid(&self, entry: &CacheEntry, now_us: u64) -> bool {
        entry.generation == self.tailnet_generation
            && now_us.saturating_sub(entry.inserted_at_us) < self.ttl_us
    }

    /// Invalidate the entire cache due to a tailnet change (e.g. network switch,
    /// re-authentication, or local daemon status change).
    ///
    /// Increments `tailnet_generation`, fencing all existing entries instantly.
    pub fn invalidate_tailnet(&mut self) {
        self.tailnet_generation = self.tailnet_generation.saturating_add(1);
        self.entries.clear();
    }

    /// Invalidate or remove a specific host by its stable node ID.
    pub fn invalidate_host(&mut self, stable_id: &str) -> bool {
        self.entries.remove(stable_id).is_some()
    }

    /// Retrieve a reference to a cached host if present, unexpired, and matching generation.
    #[must_use]
    pub fn get(&self, stable_id: &str, now_us: u64) -> Option<&DiscoveredHost> {
        let entry = self.entries.get(stable_id)?;
        if self.is_entry_valid(entry, now_us) {
            Some(&entry.host)
        } else {
            None
        }
    }

    /// Retrieve a mutable reference to a cached host if present, unexpired, and matching generation.
    pub fn get_mut(&mut self, stable_id: &str, now_us: u64) -> Option<&mut DiscoveredHost> {
        let current_gen = self.tailnet_generation;
        let ttl = self.ttl_us;
        let entry = self.entries.get_mut(stable_id)?;
        if entry.generation == current_gen && now_us.saturating_sub(entry.inserted_at_us) < ttl {
            Some(&mut entry.host)
        } else {
            None
        }
    }

    /// Update an existing cached host in place and refresh its insertion timestamp to `now_us`.
    ///
    /// If the entry exists and matches the current tailnet generation, applies `f` to the host,
    /// updates `inserted_at_us = now_us`, and returns `true`.
    pub fn update_entry<F>(&mut self, stable_id: &str, now_us: u64, f: F) -> bool
    where
        F: FnOnce(&mut DiscoveredHost),
    {
        let current_gen = self.tailnet_generation;
        if let Some(entry) = self.entries.get_mut(stable_id)
            && entry.generation == current_gen
        {
            f(&mut entry.host);
            entry.inserted_at_us = now_us;
            return true;
        }
        false
    }

    /// Insert or update a host entry in the cache.
    ///
    /// If cache capacity is reached, expired entries are pruned first. If still full,
    /// the oldest entry by insertion timestamp is evicted.
    pub fn insert(&mut self, host: DiscoveredHost, now_us: u64) {
        // If updating an existing entry, update in place
        if let Some(entry) = self.entries.get_mut(&host.stable_id) {
            entry.host = host;
            entry.inserted_at_us = now_us;
            entry.generation = self.tailnet_generation;
            return;
        }

        // Capacity check
        if self.entries.len() >= self.max_entries {
            self.prune_expired(now_us);
            if self.entries.len() >= self.max_entries {
                self.evict_oldest();
            }
        }

        let key = host.stable_id.clone();
        let entry = CacheEntry {
            host,
            inserted_at_us: now_us,
            generation: self.tailnet_generation,
        };
        self.entries.insert(key, entry);
    }

    /// Remove all expired entries or entries from prior generations.
    pub fn prune_expired(&mut self, now_us: u64) {
        let current_gen = self.tailnet_generation;
        let ttl = self.ttl_us;
        self.entries.retain(|_, e| {
            e.generation == current_gen && now_us.saturating_sub(e.inserted_at_us) < ttl
        });
    }

    /// Evict the single oldest entry by insertion timestamp.
    fn evict_oldest(&mut self) {
        let oldest_key = self
            .entries
            .iter()
            .min_by_key(|(_, e)| e.inserted_at_us)
            .map(|(k, _)| k.clone());

        if let Some(key) = oldest_key {
            self.entries.remove(&key);
        }
    }

    /// Return all currently valid, unexpired discovered hosts.
    #[must_use]
    pub fn all_valid(&self, now_us: u64) -> Vec<DiscoveredHost> {
        self.entries
            .values()
            .filter(|e| self.is_entry_valid(e, now_us))
            .map(|e| e.host.clone())
            .collect()
    }
}
