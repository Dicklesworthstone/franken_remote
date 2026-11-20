//! Real instrumented input-to-photon latency measurement harness (Plan §21.3,
//! bead `fr-p1-latency-harness-x84`).
//!
//! # Honesty & Measurement Discipline (AGENTS.md §7, Plan §21)
//! - Zero hard-coded stage durations and zero silent target grading.
//! - End-to-end `input_to_photon_us` is measured strictly on the client's local
//!   monotonic clock (`frame_presented_us - input_captured_us`), never by raw
//!   subtraction of unsynchronized cross-host clocks.
//! - Cross-host transit intervals (`input_transit`, `return_transit`) carry
//!   explicit conservative uncertainty intervals derived from an authenticated
//!   clock-exchange bracket (`ClockBracket`), or zero uncertainty ONLY when
//!   explicitly marked as a same-clock local/optical lane.
//! - Every report MUST carry a complete [`MeasurementScope`] (host/client OS,
//!   process family, capture/encode/decode paths, network mode, warm/cold state,
//!   geometry/refresh, clock method). Reports without scope are refused.
#![forbid(unsafe_code)]

use fr_core::ids::HostBootId;
use std::time::Instant;

/// Maximum samples retained per harness run (bounds memory explicitly).
pub const MAX_SAMPLES: usize = 16_384;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LatencyError {
    IncompleteScope,
    NoSamples,
    SampleLimitExceeded,
    ClockRegression,
    InvalidClockBracket,
    HostBootMismatch,
}

impl std::fmt::Display for LatencyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "latency-harness: {self:?}")
    }
}

impl std::error::Error for LatencyError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkMode {
    DirectTailnet,
    RelayedDerp,
    LoopbackNamespace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarmupState {
    ColdWorkerStartup,
    WarmMediaPath,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockMethod {
    /// Optical / high-speed camera capturing physical input-to-photon directly.
    OpticalCamera,
    /// Shared single-machine monotonic clock (same host / namespace e2e).
    SharedMonotonicClock,
    /// Separate host/client monotonic clocks bounded by authenticated `ClockExchange`.
    CorrelatedClockExchange,
}

/// Mandatory measurement scope per Plan §21.1 and AGENTS.md §7.
/// A report is refused if any required descriptor is empty or zero.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeasurementScope {
    pub host_os: String,
    pub client_os: String,
    pub process_family: String,
    pub capture_path: String,
    pub encode_path: String,
    pub decode_path: String,
    pub network_mode: NetworkMode,
    pub warmup: WarmupState,
    pub clock_method: ClockMethod,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u16,
    /// Tailscale cost reported separately rather than hidden or charged to FR.
    pub tailscale_cpu_percent_tenths: Option<u32>,
}

impl MeasurementScope {
    pub fn validate(&self) -> Result<(), LatencyError> {
        let non_empty = !self.host_os.trim().is_empty()
            && !self.client_os.trim().is_empty()
            && !self.process_family.trim().is_empty()
            && !self.capture_path.trim().is_empty()
            && !self.encode_path.trim().is_empty()
            && !self.decode_path.trim().is_empty();
        if !non_empty || self.width == 0 || self.height == 0 || self.refresh_hz == 0 {
            return Err(LatencyError::IncompleteScope);
        }
        Ok(())
    }
}

/// Authenticated clock correlation bracket (Plan §11.3, §21.3).
/// The host sampled `host_sample_us` between `client_sent_us` and
/// `client_received_us`. The full exchange width plus bounded drift forms
/// the uncertainty interval; never assumes symmetric RTT/2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockBracket {
    pub host_boot: HostBootId,
    pub client_sent_us: u64,
    pub host_sample_us: u64,
    pub client_received_us: u64,
    pub drift_ppm: u32,
}

impl ClockBracket {
    pub fn validate(&self) -> Result<(), LatencyError> {
        if self.host_boot.as_raw() == 0
            || self.client_received_us < self.client_sent_us
            || self.drift_ppm > 10_000
        {
            return Err(LatencyError::InvalidClockBracket);
        }
        Ok(())
    }

    /// Half-width uncertainty of the offset estimate in microseconds, including
    /// full exchange duration plus bounded drift at `at_client_us`.
    #[must_use]
    pub fn uncertainty_us(&self, at_client_us: u64) -> u64 {
        let exchange = self.client_received_us.saturating_sub(self.client_sent_us);
        let elapsed = at_client_us.abs_diff(self.client_sent_us);
        let drift = (u128::from(elapsed) * u128::from(self.drift_ppm)).div_ceil(1_000_000);
        let drift_u64 = u64::try_from(drift).unwrap_or(u64::MAX);
        exchange.saturating_add(drift_u64)
    }

    /// Convert a host timestamp into a conservative `[min_client_us, max_client_us]`
    /// client-clock interval without raw unsynchronized clock subtraction.
    pub fn host_to_client_interval(&self, host_ts_us: u64) -> Result<(u64, u64), LatencyError> {
        self.validate()?;
        let delta = i128::from(host_ts_us) - i128::from(self.host_sample_us);
        let low = i128::from(self.client_sent_us) + delta;
        let high = i128::from(self.client_received_us) + delta;
        let drift = (delta.unsigned_abs() * u128::from(self.drift_ppm)).div_ceil(1_000_000);
        let drift_i128 = i128::try_from(drift).unwrap_or(i128::MAX / 4);
        let min_us = low.saturating_sub(drift_i128).max(0);
        let max_us = high.saturating_add(drift_i128).max(min_us);
        Ok((
            u64::try_from(min_us).unwrap_or(u64::MAX),
            u64::try_from(max_us).unwrap_or(u64::MAX),
        ))
    }
}

/// Raw monotonic timestamps captured for ONE instrumented visual-target probe.
/// Client timestamps share the client's monotonic clock; host timestamps share
/// the host's monotonic clock identified by `host_boot`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawLatencySample {
    pub sample_id: u64,
    pub host_boot: HostBootId,
    // Client monotonic clock (microseconds)
    pub client_input_captured_us: u64,
    pub client_input_submitted_us: u64,
    pub client_first_packet_received_us: u64,
    pub client_frame_reassembled_us: u64,
    pub client_frame_decoded_us: u64,
    pub client_frame_presented_us: u64,
    // Host monotonic clock (microseconds)
    pub host_input_received_us: u64,
    pub host_target_changed_us: u64,
    pub host_frame_captured_us: u64,
    pub host_frame_converted_us: u64,
    pub host_frame_encoded_us: u64,
    pub host_packet_sent_us: u64,
}

impl RawLatencySample {
    /// Validate strict monotonic ordering on BOTH independent clocks so
    /// inverted or fabricated samples are rejected immediately.
    pub fn validate(&self) -> Result<(), LatencyError> {
        let client_ok = self.client_input_captured_us <= self.client_input_submitted_us
            && self.client_input_submitted_us <= self.client_first_packet_received_us
            && self.client_first_packet_received_us <= self.client_frame_reassembled_us
            && self.client_frame_reassembled_us <= self.client_frame_decoded_us
            && self.client_frame_decoded_us <= self.client_frame_presented_us;
        let host_ok = self.host_input_received_us <= self.host_target_changed_us
            && self.host_target_changed_us <= self.host_frame_captured_us
            && self.host_frame_captured_us <= self.host_frame_converted_us
            && self.host_frame_converted_us <= self.host_frame_encoded_us
            && self.host_frame_encoded_us <= self.host_packet_sent_us;
        if !client_ok || !host_ok || self.host_boot.as_raw() == 0 {
            return Err(LatencyError::ClockRegression);
        }
        Ok(())
    }

    /// True end-to-end input-to-photon duration strictly on the client's own
    /// monotonic clock (immune to cross-host clock skew).
    #[must_use]
    pub const fn input_to_photon_us(&self) -> u64 {
        self.client_frame_presented_us
            .saturating_sub(self.client_input_captured_us)
    }

    /// Decompose into local same-clock stage durations plus optional bounded
    /// cross-host transit estimates when a matching [`ClockBracket`] is given.
    pub fn decompose(
        &self,
        bracket: Option<&ClockBracket>,
    ) -> Result<DecomposedSample, LatencyError> {
        self.validate()?;
        let (input_transit_us, return_transit_us, cross_host_uncertainty_us) = match bracket {
            Some(b) => {
                b.validate()?;
                if b.host_boot != self.host_boot {
                    return Err(LatencyError::HostBootMismatch);
                }
                let (rx_min, rx_max) = b.host_to_client_interval(self.host_input_received_us)?;
                let rx_mid = rx_min.midpoint(rx_max);
                let in_transit = rx_mid.saturating_sub(self.client_input_submitted_us);

                let (tx_min, tx_max) = b.host_to_client_interval(self.host_packet_sent_us)?;
                let tx_mid = tx_min.midpoint(tx_max);
                let ret_transit = self.client_first_packet_received_us.saturating_sub(tx_mid);

                (
                    Some(in_transit),
                    Some(ret_transit),
                    Some(b.uncertainty_us(self.client_frame_presented_us)),
                )
            }
            None => (None, None, None),
        };

        Ok(DecomposedSample {
            sample_id: self.sample_id,
            input_to_photon_us: self.input_to_photon_us(),
            client_input_queue_us: self
                .client_input_submitted_us
                .saturating_sub(self.client_input_captured_us),
            input_transit_us,
            host_app_reaction_us: self
                .host_target_changed_us
                .saturating_sub(self.host_input_received_us),
            capture_wait_us: self
                .host_frame_captured_us
                .saturating_sub(self.host_target_changed_us),
            conversion_us: self
                .host_frame_converted_us
                .saturating_sub(self.host_frame_captured_us),
            encode_us: self
                .host_frame_encoded_us
                .saturating_sub(self.host_frame_converted_us),
            host_send_queue_us: self
                .host_packet_sent_us
                .saturating_sub(self.host_frame_encoded_us),
            return_transit_us,
            reassembly_jitter_us: self
                .client_frame_reassembled_us
                .saturating_sub(self.client_first_packet_received_us),
            decode_us: self
                .client_frame_decoded_us
                .saturating_sub(self.client_frame_reassembled_us),
            display_wait_us: self
                .client_frame_presented_us
                .saturating_sub(self.client_frame_decoded_us),
            cross_host_uncertainty_us,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecomposedSample {
    pub sample_id: u64,
    pub input_to_photon_us: u64,
    pub client_input_queue_us: u64,
    pub input_transit_us: Option<u64>,
    pub host_app_reaction_us: u64,
    pub capture_wait_us: u64,
    pub conversion_us: u64,
    pub encode_us: u64,
    pub host_send_queue_us: u64,
    pub return_transit_us: Option<u64>,
    pub reassembly_jitter_us: u64,
    pub decode_us: u64,
    pub display_wait_us: u64,
    pub cross_host_uncertainty_us: Option<u64>,
}

/// Distribution quantiles for one stage or end-to-end metric (microseconds).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Distribution {
    pub count: usize,
    pub min_us: u64,
    pub p50_us: u64,
    pub p95_us: u64,
    pub p99_us: u64,
    pub max_us: u64,
    pub mean_us: u64,
}

impl Distribution {
    #[must_use]
    pub fn compute(values: &[u64]) -> Option<Self> {
        if values.is_empty() {
            return None;
        }
        let mut sorted = values.to_vec();
        sorted.sort_unstable();
        let n = sorted.len();
        let sum: u128 = sorted.iter().map(|&v| u128::from(v)).sum();
        let mean_us = u64::try_from(sum / (n as u128)).unwrap_or(u64::MAX);
        Some(Self {
            count: n,
            min_us: sorted[0],
            p50_us: percentile(&sorted, 50),
            p95_us: percentile(&sorted, 95),
            p99_us: percentile(&sorted, 99),
            max_us: sorted[n - 1],
            mean_us,
        })
    }
}

/// Nearest-rank percentile (`p` in 1..=100) on a non-empty sorted slice.
fn percentile(sorted: &[u64], p: usize) -> u64 {
    let n = sorted.len();
    let rank = (n * p).div_ceil(100);
    let idx = rank.saturating_sub(1).min(n - 1);
    sorted[idx]
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatencyReport {
    pub scope: MeasurementScope,
    pub sample_count: usize,
    pub dropped_probes: u64,
    pub stale_view_intervals: u64,
    pub input_to_photon: Distribution,
    pub client_input_queue: Distribution,
    pub input_transit: Option<Distribution>,
    pub host_app_reaction: Distribution,
    pub capture_wait: Distribution,
    pub conversion: Distribution,
    pub encode: Distribution,
    pub host_send_queue: Distribution,
    pub return_transit: Option<Distribution>,
    pub reassembly_jitter: Distribution,
    pub decode: Distribution,
    pub display_wait: Distribution,
    pub max_cross_host_uncertainty_us: Option<u64>,
}

/// Bounded collector that validates scope, clock brackets, and monotonicity,
/// then computes exact stage and end-to-end distributions.
#[derive(Debug)]
pub struct LatencyHarness {
    scope: MeasurementScope,
    bracket: Option<ClockBracket>,
    samples: Vec<DecomposedSample>,
    dropped_probes: u64,
    stale_view_intervals: u64,
}

impl LatencyHarness {
    pub fn new(
        scope: MeasurementScope,
        bracket: Option<ClockBracket>,
    ) -> Result<Self, LatencyError> {
        scope.validate()?;
        if let Some(b) = bracket {
            b.validate()?;
        }
        Ok(Self {
            scope,
            bracket,
            samples: Vec::new(),
            dropped_probes: 0,
            stale_view_intervals: 0,
        })
    }

    pub fn update_clock_bracket(&mut self, bracket: ClockBracket) -> Result<(), LatencyError> {
        bracket.validate()?;
        self.bracket = Some(bracket);
        Ok(())
    }

    pub fn record_sample(&mut self, sample: RawLatencySample) -> Result<(), LatencyError> {
        if self.samples.len() >= MAX_SAMPLES {
            return Err(LatencyError::SampleLimitExceeded);
        }
        let decomposed = sample.decompose(self.bracket.as_ref())?;
        self.samples.push(decomposed);
        Ok(())
    }

    pub const fn record_dropped_probe(&mut self) {
        self.dropped_probes = self.dropped_probes.saturating_add(1);
    }

    pub const fn record_stale_view_interval(&mut self) {
        self.stale_view_intervals = self.stale_view_intervals.saturating_add(1);
    }

    pub fn finish(&self) -> Result<LatencyReport, LatencyError> {
        self.scope.validate()?;
        if self.samples.is_empty() {
            return Err(LatencyError::NoSamples);
        }
        let collect = |f: fn(&DecomposedSample) -> u64| -> Distribution {
            let vals: Vec<u64> = self.samples.iter().map(f).collect();
            Distribution::compute(&vals).expect("non-empty samples")
        };
        let collect_opt = |f: fn(&DecomposedSample) -> Option<u64>| -> Option<Distribution> {
            let vals: Vec<u64> = self.samples.iter().filter_map(f).collect();
            Distribution::compute(&vals)
        };
        let max_uncertainty = self
            .samples
            .iter()
            .filter_map(|s| s.cross_host_uncertainty_us)
            .max();

        Ok(LatencyReport {
            scope: self.scope.clone(),
            sample_count: self.samples.len(),
            dropped_probes: self.dropped_probes,
            stale_view_intervals: self.stale_view_intervals,
            input_to_photon: collect(|s| s.input_to_photon_us),
            client_input_queue: collect(|s| s.client_input_queue_us),
            input_transit: collect_opt(|s| s.input_transit_us),
            host_app_reaction: collect(|s| s.host_app_reaction_us),
            capture_wait: collect(|s| s.capture_wait_us),
            conversion: collect(|s| s.conversion_us),
            encode: collect(|s| s.encode_us),
            host_send_queue: collect(|s| s.host_send_queue_us),
            return_transit: collect_opt(|s| s.return_transit_us),
            reassembly_jitter: collect(|s| s.reassembly_jitter_us),
            decode: collect(|s| s.decode_us),
            display_wait: collect(|s| s.display_wait_us),
            max_cross_host_uncertainty_us: max_uncertainty,
        })
    }
}

/// Visual-target probe helper that timestamps real monotonic elapsed stage
/// transitions on a live instrumented host/client loop (never synthetic constants).
#[derive(Debug)]
pub struct MonotonicProbeClock {
    origin: Instant,
}

impl MonotonicProbeClock {
    #[must_use]
    pub fn start() -> Self {
        Self {
            origin: Instant::now(),
        }
    }

    #[must_use]
    pub fn now_us(&self) -> u64 {
        u64::try_from(self.origin.elapsed().as_micros()).unwrap_or(u64::MAX)
    }
}
