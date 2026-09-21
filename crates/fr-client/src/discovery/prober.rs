//! Bounded capability prober and probe scheduler.
//!
//! Per plan section 6.4:
//! - Desktop clients obtain peers from local Tailscale state, then perform bounded
//!   capability probes ONLY against those node addresses.
//! - Subnet scans or LAN broadcasts are strictly forbidden.
//! - Concurrency ceiling: at most 4 concurrent probes (`MAX_CONCURRENT_PROBES`).
//! - Exponential backoff with ceiling (500ms to 30s) on consecutive failures.
//! - Bounded timeouts (default 2s per probe).

use std::{
    collections::{HashMap, VecDeque},
    net::IpAddr,
};

use super::{DiscoveredHost, DiscoveryCache, PeerDiscoveryState};

/// Maximum concurrent capability probes allowed across the client.
pub const MAX_CONCURRENT_PROBES: usize = 4;

/// Initial backoff on probe failure: 500 milliseconds (in microseconds).
pub const INITIAL_BACKOFF_US: u64 = 500_000;

/// Maximum backoff on consecutive probe failures: 30 seconds (in microseconds).
pub const MAX_BACKOFF_US: u64 = 30_000_000;

/// Default timeout for a single capability probe: 2 seconds (in microseconds).
pub const DEFAULT_PROBE_TIMEOUT_US: u64 = 2_000_000;

/// Compute exponential backoff in microseconds based on consecutive failure count.
///
/// Backoff doubles with each failure:
/// - 0 failures: 0 µs
/// - 1 failure:  500 ms (500,000 µs)
/// - 2 failures: 1 s    (1,000,000 µs)
/// - 3 failures: 2 s    (2,000,000 µs)
/// - 4 failures: 4 s    (4,000,000 µs)
/// - 5 failures: 8 s    (8,000,000 µs)
/// - 6 failures: 16 s   (16,000,000 µs)
/// - 7+ failures: 30 s  (capped at `MAX_BACKOFF_US`)
#[must_use]
pub fn compute_backoff_us(consecutive_failures: u32) -> u64 {
    if consecutive_failures == 0 {
        return 0;
    }
    let shift = consecutive_failures.saturating_sub(1).min(6);
    let factor = 1u64.checked_shl(shift).unwrap_or(64);
    INITIAL_BACKOFF_US
        .saturating_mul(factor)
        .min(MAX_BACKOFF_US)
}

/// Explicit candidate target for a capability probe.
///
/// Only explicit node addresses obtained from Tailscale or saved hosts are valid.
/// Broad subnet scans or broadcast addresses are strictly prohibited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeTarget {
    /// Stable Tailscale node ID.
    pub stable_id: String,
    /// Tailnet DNS / certificate name.
    pub certificate_name: String,
    /// Exact target IP address.
    pub address: IpAddr,
    /// Port number to probe.
    pub port: u16,
    /// Whether target traverses a DERP relay.
    pub is_derp_relayed: bool,
}

impl ProbeTarget {
    /// Create a new probe target.
    ///
    /// Rejects non-unicast addresses (broadcast, unspecified, multicast) to enforce
    /// the non-scanning invariant.
    pub fn new(
        stable_id: impl Into<String>,
        certificate_name: impl Into<String>,
        address: IpAddr,
        port: u16,
        is_derp_relayed: bool,
    ) -> Result<Self, &'static str> {
        if address.is_unspecified() || address.is_multicast() {
            return Err("Probe target must be a unicast tailnet address, not broadcast/multicast");
        }
        Ok(Self {
            stable_id: stable_id.into(),
            certificate_name: certificate_name.into(),
            address,
            port,
            is_derp_relayed,
        })
    }
}

/// In-flight probe record.
#[derive(Debug, Clone)]
struct InFlightProbe {
    target: ProbeTarget,
    started_us: u64,
}

/// Scheduler that bounds probe concurrency, tracks in-flight probes, and enforces
/// exponential backoff.
#[derive(Debug)]
pub struct ProbeScheduler {
    pending_queue: VecDeque<ProbeTarget>,
    in_flight: HashMap<String, InFlightProbe>,
    max_concurrent: usize,
    timeout_us: u64,
}

impl Default for ProbeScheduler {
    fn default() -> Self {
        Self::new(MAX_CONCURRENT_PROBES, DEFAULT_PROBE_TIMEOUT_US)
    }
}

impl ProbeScheduler {
    /// Create a new probe scheduler with specified limits.
    #[must_use]
    pub fn new(max_concurrent: usize, timeout_us: u64) -> Self {
        Self {
            pending_queue: VecDeque::new(),
            in_flight: HashMap::new(),
            max_concurrent: max_concurrent.min(MAX_CONCURRENT_PROBES),
            timeout_us,
        }
    }

    /// Number of probes currently in-flight.
    #[must_use]
    pub fn in_flight_count(&self) -> usize {
        self.in_flight.len()
    }

    /// Number of probes waiting in the pending queue.
    #[must_use]
    pub fn queued_count(&self) -> usize {
        self.pending_queue.len()
    }

    /// Whether any probe is in-flight or queued for this stable ID.
    #[must_use]
    pub fn is_active(&self, stable_id: &str) -> bool {
        self.in_flight.contains_key(stable_id)
            || self.pending_queue.iter().any(|t| t.stable_id == stable_id)
    }

    /// Attempt to enqueue a candidate target for probing.
    ///
    /// Returns `true` if target was enqueued, or `false` if rejected (e.g. already
    /// in-flight, already queued, cached state is still unexpired, or under backoff).
    pub fn enqueue_candidate(
        &mut self,
        target: ProbeTarget,
        cache: &DiscoveryCache,
        now_us: u64,
    ) -> bool {
        // Skip if already in flight or already in queue
        if self.is_active(&target.stable_id) {
            return false;
        }

        // Check cache for backoff or unexpired state
        if let Some(cached) = cache.get(&target.stable_id, now_us) {
            // If under backoff, cannot probe yet
            if !cached.can_probe(now_us) {
                return false;
            }
            // If already successfully ready and probed recently (< TTL / 2), no need to re-probe
            if cached.is_ready()
                && cached
                    .last_probed_us
                    .is_some_and(|t| now_us.saturating_sub(t) < cache.ttl_us() / 2)
            {
                return false;
            }
        }

        self.pending_queue.push_back(target);
        true
    }

    /// Poll for the next eligible probe target to execute.
    ///
    /// Returns `Some(target)` if concurrency limit is not exceeded and a target is queued.
    /// Moves the target to the in-flight map.
    pub fn poll_next_probe(&mut self, now_us: u64) -> Option<ProbeTarget> {
        if self.in_flight.len() >= self.max_concurrent {
            return None;
        }

        let target = self.pending_queue.pop_front()?;
        self.in_flight.insert(
            target.stable_id.clone(),
            InFlightProbe {
                target: target.clone(),
                started_us: now_us,
            },
        );
        Some(target)
    }

    /// Record successful completion of a probe.
    ///
    /// Removes from in-flight, updates cache, and clears failure/backoff counters.
    pub fn on_probe_success(
        &mut self,
        stable_id: &str,
        state: PeerDiscoveryState,
        cache: &mut DiscoveryCache,
        now_us: u64,
    ) {
        let in_flight = self.in_flight.remove(stable_id);
        let updated = cache.update_entry(stable_id, now_us, |host| {
            host.state = state.clone();
            host.last_probed_us = Some(now_us);
            host.probe_failures = 0;
            host.backoff_until_us = 0;
        });
        if !updated && let Some(probe) = in_flight {
            let mut host = DiscoveredHost::new(
                probe.target.stable_id,
                probe.target.certificate_name,
                vec![probe.target.address],
                probe.target.port,
                probe.target.is_derp_relayed,
            );
            host.state = state;
            host.last_probed_us = Some(now_us);
            host.probe_failures = 0;
            host.backoff_until_us = 0;
            cache.insert(host, now_us);
        }
    }

    /// Record failure of a probe (network error, timeout, refused port, etc.).
    ///
    /// Removes from in-flight, increments failure count, and schedules exponential backoff.
    pub fn on_probe_failure(
        &mut self,
        stable_id: &str,
        failure_state: PeerDiscoveryState,
        cache: &mut DiscoveryCache,
        now_us: u64,
    ) {
        let in_flight = self.in_flight.remove(stable_id);
        let updated = cache.update_entry(stable_id, now_us, |host| {
            host.state = failure_state.clone();
            host.last_probed_us = Some(now_us);
            host.probe_failures = host.probe_failures.saturating_add(1);
            let backoff = compute_backoff_us(host.probe_failures);
            host.backoff_until_us = now_us.saturating_add(backoff);
        });
        if !updated && let Some(probe) = in_flight {
            let mut host = DiscoveredHost::new(
                probe.target.stable_id,
                probe.target.certificate_name,
                vec![probe.target.address],
                probe.target.port,
                probe.target.is_derp_relayed,
            );
            host.state = failure_state;
            host.last_probed_us = Some(now_us);
            host.probe_failures = 1;
            let backoff = compute_backoff_us(1);
            host.backoff_until_us = now_us.saturating_add(backoff);
            cache.insert(host, now_us);
        }
    }

    /// Check for in-flight probes that have exceeded `timeout_us`.
    ///
    /// Any timed-out probe is moved out of in-flight and treated as a failure with
    /// `PeerDiscoveryState::Offline` and exponential backoff. Returns the list of
    /// timed-out stable IDs.
    pub fn check_timeouts(&mut self, now_us: u64, cache: &mut DiscoveryCache) -> Vec<String> {
        let timeout_limit = self.timeout_us;
        let timed_out: Vec<String> = self
            .in_flight
            .iter()
            .filter(|(_, probe)| now_us.saturating_sub(probe.started_us) >= timeout_limit)
            .map(|(id, _)| id.clone())
            .collect();

        for id in &timed_out {
            self.on_probe_failure(id, PeerDiscoveryState::Offline, cache, now_us);
        }

        timed_out
    }
}

/// Simulated or pluggable capability prober interface.
pub trait CapabilityProber {
    /// Probe the specified target and return the observed discovery state.
    fn probe(&self, target: &ProbeTarget) -> PeerDiscoveryState;
}
