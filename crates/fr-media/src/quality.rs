//! Deterministic, hysteretic, pressure-aware adaptive quality controller (Plan §13).
//!
//! This controller implements the Phase 1 quality and rate-control state machine.
//! It is a pure synchronous function: decisions depend strictly on the logged
//! [`InputSnapshot`] and internal dwell/hysteresis timers.
//!
//! Attribution separates four non-interchangeable pressure sources:
//! - **Network**: packet loss, RTT inflation, and transport egress queue bloat;
//! - **Encoder**: hardware encoder latency exceeding frame intervals or queue backlog;
//! - **Decoder**: receiver retained picture backlog or excessive decode work;
//! - **Presentation**: view hidden or display wait stalls.
//!
//! Downward adjustments occur quickly on persistent pressure; upward adjustments
//! occur slowly only after sustained headroom. Idle / application-limited intervals
//! never claim link capacity and never trigger upward probes or false downward steps.

use core::fmt;

pub const MIN_PRESSURE_DWELL_US: u64 = 150_000;
pub const MIN_REDUCE_DWELL_US: u64 = 250_000;
pub const MIN_PROBE_DWELL_US: u64 = 2_000_000;
pub const MAX_SAMPLE_GAP_US: u64 = 200_000;

/// Default baseline Opus audio allowance in bits per second.
pub const DEFAULT_AUDIO_ALLOWANCE_BPS: u64 = 64_000;
/// Default auxiliary allowance (cursor, clipboard, control framing) in bits per second.
pub const DEFAULT_AUX_ALLOWANCE_BPS: u64 = 32_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QualityError {
    InvalidPolicy,
    ClockRegression,
    Closed,
}

impl fmt::Display for QualityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPolicy => write!(f, "invalid quality controller policy"),
            Self::ClockRegression => write!(f, "monotonic clock regressed in quality update"),
            Self::Closed => write!(f, "quality controller is closed"),
        }
    }
}

impl core::error::Error for QualityError {}

/// User-facing workstation operating profile (Plan §13.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WorkstationProfile {
    /// Balanced trade-off between frame rate and spatial resolution.
    #[default]
    Auto,
    /// Document / text work: reduce frame rate before degrading spatial detail.
    Text,
    /// High-motion work: trade spatial detail for temporal smoothness.
    Motion,
}

/// Bandwidth budget partition for an active operating point (Plan §13.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BandwidthBudget {
    /// Bits per second allocated to steady-state video frames (P-frames).
    pub video_bitrate_bps: u64,
    /// Headroom reserved for periodic or recovery IDR access units.
    pub keyframe_allowance_bps: u64,
    /// Headroom reserved for selective retransmissions and packet repairs.
    pub retransmission_allowance_bps: u64,
    /// Headroom reserved for audio streaming.
    pub audio_allowance_bps: u64,
    /// Headroom reserved for cursor, clipboard, and control messages.
    pub auxiliary_allowance_bps: u64,
}

impl BandwidthBudget {
    /// Calculate budget allocation from total target bitrate.
    #[must_use]
    pub fn from_total(total_bps: u64) -> Self {
        let audio = DEFAULT_AUDIO_ALLOWANCE_BPS.min(total_bps / 10);
        let aux = DEFAULT_AUX_ALLOWANCE_BPS.min(total_bps / 20);
        let non_video = audio.saturating_add(aux);
        let media_pool = total_bps.saturating_sub(non_video);

        // Keyframe headroom: ~12.5% of media pool
        let keyframe = (media_pool / 8).max(10_000);
        // Retransmission headroom: ~6.25% of media pool
        let retrans = (media_pool / 16).max(5_000);

        let video = media_pool.saturating_sub(keyframe.saturating_add(retrans));

        Self {
            video_bitrate_bps: video,
            keyframe_allowance_bps: keyframe,
            retransmission_allowance_bps: retrans,
            audio_allowance_bps: audio,
            auxiliary_allowance_bps: aux,
        }
    }

    /// Total sum of all budget allowances.
    #[must_use]
    pub const fn total_bps(&self) -> u64 {
        self.video_bitrate_bps
            .saturating_add(self.keyframe_allowance_bps)
            .saturating_add(self.retransmission_allowance_bps)
            .saturating_add(self.audio_allowance_bps)
            .saturating_add(self.auxiliary_allowance_bps)
    }
}

/// A complete, discrete quality operating point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OperatingPoint {
    pub generation: u64,
    pub profile: WorkstationProfile,
    pub target_bitrate_bps: u64,
    pub frame_interval_us: u64,
    pub budget: BandwidthBudget,
    pub aggregate_ceiling_bps: u64,
}

impl OperatingPoint {
    /// Fair share for each viewer under a shared-host aggregate ceiling.
    #[must_use]
    pub fn per_viewer_share(&self, total_viewers: usize) -> u64 {
        if total_viewers == 0 {
            return self.target_bitrate_bps;
        }
        let per_viewer_cap = self.aggregate_ceiling_bps / (total_viewers as u64);
        self.target_bitrate_bps.min(per_viewer_cap)
    }
}

/// Static policy configuration for the quality controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QualityPolicy {
    pub min_bitrate_bps: u64,
    pub max_bitrate_bps: u64,
    pub initial_bitrate_bps: u64,
    pub aggregate_ceiling_bps: u64,
    pub default_profile: WorkstationProfile,
    pub min_frame_interval_us: u64,
    pub max_frame_interval_us: u64,
}

impl QualityPolicy {
    /// Standard desktop workstation policy (500 kbps floor, 35 Mbps ceiling, 60fps).
    #[must_use]
    pub const fn desktop_default() -> Self {
        Self {
            min_bitrate_bps: 500_000,
            max_bitrate_bps: 35_000_000,
            initial_bitrate_bps: 8_000_000,
            aggregate_ceiling_bps: 60_000_000,
            default_profile: WorkstationProfile::Auto,
            min_frame_interval_us: 16_666, // ~60 fps
            max_frame_interval_us: 66_666, // ~15 fps
        }
    }

    pub fn validate(&self) -> Result<(), QualityError> {
        if self.min_bitrate_bps == 0
            || self.min_bitrate_bps > self.initial_bitrate_bps
            || self.initial_bitrate_bps > self.max_bitrate_bps
            || self.max_bitrate_bps > self.aggregate_ceiling_bps
            || self.min_frame_interval_us == 0
            || self.min_frame_interval_us > self.max_frame_interval_us
        {
            return Err(QualityError::InvalidPolicy);
        }
        Ok(())
    }
}

/// Telemetry for the network transport stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NetworkMetrics {
    /// Estimated round-trip time, None if unavailable or unmeasured.
    pub rtt_us: Option<u64>,
    /// Loss in basis points (1 bp = 0.01%, `10_000` bp = 100%), None if unknown.
    pub loss_basis_points: Option<u16>,
    /// Current unsent bytes in transport send queue.
    pub send_queue_bytes: u64,
    /// Transport send queue capacity.
    pub send_queue_capacity: u64,
    /// Measured sustainable delivery estimate, None if application-limited or unmeasured.
    pub sustainable_delivery_estimate_bps: Option<u64>,
}

/// Telemetry for the hardware capture and encoder stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EncoderMetrics {
    /// Hardware encode execution duration, None if no recent encode.
    pub encode_latency_us: Option<u64>,
    /// In-flight encoder surfaces / queue depth.
    pub in_flight_surfaces: u32,
}

/// Telemetry for the client decoder stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DecoderMetrics {
    /// Solicited receiver metrics, None if unnegotiated or uncollected.
    pub load: Option<fr_wire::receiver_metrics::Load>,
}

/// Telemetry for the presentation stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresentationMetrics {
    pub is_visible: bool,
    pub presentation_latency_us: Option<u64>,
}

impl Default for PresentationMetrics {
    fn default() -> Self {
        Self {
            is_visible: true,
            presentation_latency_us: None,
        }
    }
}

/// An immutable, complete snapshot of all stage inputs for one decision instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputSnapshot {
    pub now_us: u64,
    pub network: NetworkMetrics,
    pub encoder: EncoderMetrics,
    pub decoder: DecoderMetrics,
    pub presentation: PresentationMetrics,
    /// True when screen is stationary/idle; observed rate is application-limited.
    pub application_limited: bool,
    /// True if an underlying path switch occurred (e.g. interface migration or direct/relay switch).
    pub route_changed: bool,
    /// True if connection was freshly re-established.
    pub reconnected: bool,
}

/// Active pressure flags across distinct system dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct PressureFlags {
    pub network: bool,
    pub encoder: bool,
    pub decoder: bool,
    pub presentation: bool,
}

impl PressureFlags {
    #[must_use]
    pub const fn has_any(self) -> bool {
        self.network || self.encoder || self.decoder || self.presentation
    }
}

/// Detailed reason why an operating point adjustment was made (or held).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionReason {
    /// No change: conditions healthy and within dwell window.
    Hold,
    /// Network pressure: RTT inflation, packet loss, or send queue congestion.
    NetworkBackoff {
        rtt_inflation: bool,
        packet_loss: bool,
        send_congested: bool,
    },
    /// Encoder overload: hardware encode latency or in-flight surface buildup.
    EncoderThrottle {
        latency_exceeded: bool,
        queue_backlog: bool,
    },
    /// Decoder overload: receiver picture backlog or excessive work duration.
    DecoderThrottle {
        picture_backlog: bool,
        excessive_work: bool,
    },
    /// Presentation throttle: view hidden/backgrounded or presentation stalled.
    PresentationThrottle {
        hidden: bool,
        stalled: bool,
    },
    /// Upward probe: sustained headroom across all dimensions for >= 2.0 seconds.
    HeadroomProbe,
    /// Held unchanged: screen is stationary/idle (application-limited).
    ApplicationLimitedHold,
    /// Conservative reset after route change or reconnection.
    ConservativeReset {
        route_changed: bool,
        reconnected: bool,
    },
    /// Gap in sample sequence; conservative hold.
    SampleGap,
}

/// Replayable decision record containing the complete input snapshot and outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QualityDecision {
    pub snapshot: InputSnapshot,
    pub previous_point: OperatingPoint,
    pub current_point: OperatingPoint,
    pub reason: DecisionReason,
    /// Sustained headroom duration in microseconds.
    pub headroom_us: u64,
    /// Pressure duration in microseconds: [network, encoder, decoder, presentation].
    pub pressure_us: [u64; 4],
}

/// Deterministic adaptive quality controller state machine.
#[derive(Debug)]
pub struct QualityController {
    policy: QualityPolicy,
    current_point: OperatingPoint,
    last_now_us: Option<u64>,
    last_adjust_us: Option<u64>,
    pressure_start_us: [Option<u64>; 4],
    headroom_start_us: Option<u64>,
    headroom_evidence_us: u64,
    closed: bool,
    history: [Option<QualityDecision>; 16],
    history_cursor: usize,
}

impl QualityController {
    pub fn new(policy: QualityPolicy) -> Result<Self, QualityError> {
        policy.validate()?;

        let budget = BandwidthBudget::from_total(policy.initial_bitrate_bps);
        let current_point = OperatingPoint {
            generation: 1,
            profile: policy.default_profile,
            target_bitrate_bps: policy.initial_bitrate_bps,
            frame_interval_us: policy.min_frame_interval_us,
            budget,
            aggregate_ceiling_bps: policy.aggregate_ceiling_bps,
        };

        Ok(Self {
            policy,
            current_point,
            last_now_us: None,
            last_adjust_us: None,
            pressure_start_us: [None; 4],
            headroom_start_us: None,
            headroom_evidence_us: 0,
            closed: false,
            history: [None; 16],
            history_cursor: 0,
        })
    }

    #[must_use]
    pub const fn policy(&self) -> QualityPolicy {
        self.policy
    }

    #[must_use]
    pub const fn current_point(&self) -> OperatingPoint {
        self.current_point
    }

    /// Set user-facing workstation profile (Auto, Text, Motion).
    pub fn set_profile(&mut self, profile: WorkstationProfile) {
        if self.current_point.profile != profile {
            self.current_point.profile = profile;
            self.current_point.generation = self.current_point.generation.wrapping_add(1);
        }
    }

    /// Iterator over the bounded decision history in chronological order.
    pub fn decisions(&self) -> impl Iterator<Item = &QualityDecision> {
        (0..self.history.len()).filter_map(move |i| {
            self.history[(self.history_cursor + i) % self.history.len()].as_ref()
        })
    }

    /// Advance the controller with a fresh input snapshot.
    pub fn update(&mut self, snapshot: InputSnapshot) -> Result<QualityDecision, QualityError> {
        if self.closed {
            return Err(QualityError::Closed);
        }

        match self.evaluate(snapshot) {
            Ok(decision) => {
                if decision.reason != DecisionReason::Hold {
                    self.history[self.history_cursor] = Some(decision);
                    self.history_cursor = (self.history_cursor + 1) % self.history.len();
                }
                Ok(decision)
            }
            Err(err) => {
                self.closed = true;
                Err(err)
            }
        }
    }

    fn evaluate(&mut self, s: InputSnapshot) -> Result<QualityDecision, QualityError> {
        let prev_point = self.current_point;

        if self.last_now_us.is_some_and(|prev| s.now_us < prev) {
            return Err(QualityError::ClockRegression);
        }

        let is_gap = self
            .last_now_us
            .is_some_and(|prev| s.now_us.saturating_sub(prev) > MAX_SAMPLE_GAP_US);
        self.last_now_us = Some(s.now_us);

        // Check conservative resets (route change or reconnection)
        if s.route_changed || s.reconnected {
            self.reset_all_timers();
            let new_bitrate = self
                .policy
                .min_bitrate_bps
                .max(self.current_point.target_bitrate_bps * 3 / 4);
            self.current_point.target_bitrate_bps = new_bitrate;
            self.current_point.budget = BandwidthBudget::from_total(new_bitrate);
            self.current_point.frame_interval_us = self.policy.min_frame_interval_us;
            self.current_point.generation = self.current_point.generation.wrapping_add(1);
            self.last_adjust_us = Some(s.now_us);

            return Ok(QualityDecision {
                snapshot: s,
                previous_point: prev_point,
                current_point: self.current_point,
                reason: DecisionReason::ConservativeReset {
                    route_changed: s.route_changed,
                    reconnected: s.reconnected,
                },
                headroom_us: 0,
                pressure_us: [0; 4],
            });
        }

        if is_gap {
            self.reset_all_timers();
            self.last_adjust_us = Some(s.now_us);
            return Ok(QualityDecision {
                snapshot: s,
                previous_point: prev_point,
                current_point: self.current_point,
                reason: DecisionReason::SampleGap,
                headroom_us: 0,
                pressure_us: [0; 4],
            });
        }

        // Assess pressures
        let (flags, pressure_durations) = self.assess_pressures(&s);

        // Track headroom
        let (has_headroom, headroom_us) = self.track_headroom(&s, flags);

        // Application-limited check
        if s.application_limited && !flags.has_any() {
            // Idle screen: maintain current operating point without probing upward
            return Ok(QualityDecision {
                snapshot: s,
                previous_point: prev_point,
                current_point: self.current_point,
                reason: DecisionReason::ApplicationLimitedHold,
                headroom_us,
                pressure_us: pressure_durations,
            });
        }

        // Check if adjustments can be made
        let time_since_adjust = self
            .last_adjust_us
            .map_or(u64::MAX, |last| s.now_us.saturating_sub(last));

        // 1. Backoff on sustained pressure
        if let Some((category_idx, reason)) = self.detect_backoff_trigger(&s, pressure_durations)
            && time_since_adjust >= MIN_REDUCE_DWELL_US
        {
            self.apply_backoff(category_idx);
            self.last_adjust_us = Some(s.now_us);
            self.headroom_start_us = None;
            self.headroom_evidence_us = 0;

            return Ok(QualityDecision {
                snapshot: s,
                previous_point: prev_point,
                current_point: self.current_point,
                reason,
                headroom_us: 0,
                pressure_us: pressure_durations,
            });
        }

        // 2. Probe upward on sustained headroom
        if has_headroom
            && headroom_us >= MIN_PROBE_DWELL_US
            && time_since_adjust >= MIN_PROBE_DWELL_US
            && self.current_point.target_bitrate_bps < self.policy.max_bitrate_bps
        {
            self.apply_probe(&s);
            self.last_adjust_us = Some(s.now_us);
            self.headroom_start_us = Some(s.now_us);
            self.headroom_evidence_us = 0;

            return Ok(QualityDecision {
                snapshot: s,
                previous_point: prev_point,
                current_point: self.current_point,
                reason: DecisionReason::HeadroomProbe,
                headroom_us,
                pressure_us: pressure_durations,
            });
        }

        Ok(QualityDecision {
            snapshot: s,
            previous_point: prev_point,
            current_point: self.current_point,
            reason: DecisionReason::Hold,
            headroom_us,
            pressure_us: pressure_durations,
        })
    }

    fn assess_pressures(&mut self, s: &InputSnapshot) -> (PressureFlags, [u64; 4]) {
        // 0: Network
        let rtt_pressure = s.network.rtt_us.is_some_and(|rtt| rtt > 120_000);
        let loss_pressure = s.network.loss_basis_points.is_some_and(|lbp| lbp >= 200); // >= 2.0%
        let queue_pressure = s.network.send_queue_capacity > 0
            && (s.network.send_queue_bytes * 4 >= s.network.send_queue_capacity * 3); // >= 75%
        let network = rtt_pressure || loss_pressure || queue_pressure;

        // 1: Encoder
        let enc_latency = s
            .encoder
            .encode_latency_us
            .is_some_and(|lat| lat >= self.current_point.frame_interval_us);
        let enc_queue = s.encoder.in_flight_surfaces > 1;
        let encoder = enc_latency || enc_queue;

        // 2: Decoder
        let dec_backlog = s
            .decoder
            .load
            .is_some_and(|load| load.retained_pictures > 1);
        let dec_work = s
            .decoder
            .load
            .and_then(|l| l.work_us)
            .is_some_and(|w| w >= self.current_point.frame_interval_us);
        let decoder = dec_backlog || dec_work;

        // 3: Presentation
        let pres_hidden = !s.presentation.is_visible;
        let pres_stall = s
            .presentation
            .presentation_latency_us
            .is_some_and(|p| p > 80_000);
        let presentation = pres_hidden || pres_stall;

        let flags = [network, encoder, decoder, presentation];
        let mut durations = [0u64; 4];

        for i in 0..4 {
            if flags[i] {
                let start = *self.pressure_start_us[i].get_or_insert(s.now_us);
                durations[i] = s.now_us.saturating_sub(start);
            } else {
                self.pressure_start_us[i] = None;
            }
        }

        (
            PressureFlags {
                network,
                encoder,
                decoder,
                presentation,
            },
            durations,
        )
    }

    fn track_headroom(&mut self, s: &InputSnapshot, flags: PressureFlags) -> (bool, u64) {
        // Headroom condition: visible, healthy network queues, no encoder or decoder backlog,
        // and send queue low (< 25% capacity).
        let send_healthy = s.network.send_queue_capacity == 0
            || (s.network.send_queue_bytes * 4 < s.network.send_queue_capacity);
        let loss_clean = s.network.loss_basis_points.is_none_or(|lbp| lbp < 50);
        let rtt_healthy = s.network.rtt_us.is_none_or(|rtt| rtt < 70_000);

        let is_healthy = !flags.has_any()
            && s.presentation.is_visible
            && send_healthy
            && loss_clean
            && rtt_healthy
            && !s.application_limited;

        if is_healthy {
            if let Some(prev) = self.headroom_start_us {
                let delta = s.now_us.saturating_sub(prev);
                self.headroom_evidence_us = self.headroom_evidence_us.saturating_add(delta);
            }
            self.headroom_start_us = Some(s.now_us);
        } else {
            self.headroom_start_us = None;
            self.headroom_evidence_us = 0;
        }

        (is_healthy, self.headroom_evidence_us)
    }

    fn detect_backoff_trigger(
        &self,
        s: &InputSnapshot,
        pressure_durations: [u64; 4],
    ) -> Option<(usize, DecisionReason)> {
        for (i, &dur) in pressure_durations.iter().enumerate() {
            if dur >= MIN_PRESSURE_DWELL_US {
                let reason = match i {
                    0 => DecisionReason::NetworkBackoff {
                        rtt_inflation: s.network.rtt_us.is_some_and(|r| r > 120_000),
                        packet_loss: s.network.loss_basis_points.is_some_and(|l| l >= 200),
                        send_congested: s.network.send_queue_capacity > 0
                            && (s.network.send_queue_bytes * 4
                                >= s.network.send_queue_capacity * 3),
                    },
                    1 => DecisionReason::EncoderThrottle {
                        latency_exceeded: s
                            .encoder
                            .encode_latency_us
                            .is_some_and(|lat| lat >= self.current_point.frame_interval_us),
                        queue_backlog: s.encoder.in_flight_surfaces > 1,
                    },
                    2 => DecisionReason::DecoderThrottle {
                        picture_backlog: s
                            .decoder
                            .load
                            .is_some_and(|load| load.retained_pictures > 1),
                        excessive_work: s
                            .decoder
                            .load
                            .and_then(|l| l.work_us)
                            .is_some_and(|w| w >= self.current_point.frame_interval_us),
                    },
                    _ => DecisionReason::PresentationThrottle {
                        hidden: !s.presentation.is_visible,
                        stalled: s
                            .presentation
                            .presentation_latency_us
                            .is_some_and(|p| p > 80_000),
                    },
                };
                return Some((i, reason));
            }
        }
        None
    }

    fn apply_backoff(&mut self, category_idx: usize) {
        match self.current_point.profile {
            WorkstationProfile::Text => {
                // In Text profile, prefer stepping down frame rate (increasing interval)
                // first to protect spatial clarity and text edge sharpness.
                if self.current_point.frame_interval_us < self.policy.max_frame_interval_us {
                    let stepped_interval = (self.current_point.frame_interval_us * 3 / 2)
                        .min(self.policy.max_frame_interval_us);
                    self.current_point.frame_interval_us = stepped_interval;
                    self.current_point.generation = self.current_point.generation.wrapping_add(1);
                    return;
                }
            }
            WorkstationProfile::Motion => {
                // In Motion profile, aggressively reduce bitrate first to preserve high frame rate.
            }
            WorkstationProfile::Auto => {
                // Balanced: if encoder or decoder is overloaded, stepping down frame rate
                // directly reduces CPU/GPU workload.
                if (category_idx == 1 || category_idx == 2)
                    && self.current_point.frame_interval_us < self.policy.max_frame_interval_us
                {
                    let stepped_interval = (self.current_point.frame_interval_us * 3 / 2)
                        .min(self.policy.max_frame_interval_us);
                    self.current_point.frame_interval_us = stepped_interval;
                    self.current_point.generation = self.current_point.generation.wrapping_add(1);
                    return;
                }
            }
        }

        // Bitrate reduction: step down by 25%
        let reduced = (self.current_point.target_bitrate_bps * 3 / 4)
            .max(self.policy.min_bitrate_bps);
        self.current_point.target_bitrate_bps = reduced;
        self.current_point.budget = BandwidthBudget::from_total(reduced);
        self.current_point.generation = self.current_point.generation.wrapping_add(1);
    }

    fn apply_probe(&mut self, s: &InputSnapshot) {
        // In Text or Motion profile, if frame rate was reduced, restore frame rate step
        if self.current_point.frame_interval_us > self.policy.min_frame_interval_us {
            let stepped_interval = (self.current_point.frame_interval_us * 4 / 5)
                .max(self.policy.min_frame_interval_us);
            self.current_point.frame_interval_us = stepped_interval;
            self.current_point.generation = self.current_point.generation.wrapping_add(1);
            return;
        }

        // Bounded probe: +10% increase
        let increment = (self.current_point.target_bitrate_bps / 10).max(50_000);
        let mut probed = self
            .current_point
            .target_bitrate_bps
            .saturating_add(increment)
            .min(self.policy.max_bitrate_bps);

        // Clamp to 85% of measured sustainable delivery estimate if known
        if let Some(sustainable) = s.network.sustainable_delivery_estimate_bps {
            let ceiling_85 = sustainable * 85 / 100;
            if ceiling_85 >= self.policy.min_bitrate_bps {
                probed = probed.min(ceiling_85);
            }
        }

        if probed != self.current_point.target_bitrate_bps {
            self.current_point.target_bitrate_bps = probed;
            self.current_point.budget = BandwidthBudget::from_total(probed);
            self.current_point.generation = self.current_point.generation.wrapping_add(1);
        }
    }

    fn reset_all_timers(&mut self) {
        self.pressure_start_us = [None; 4];
        self.headroom_start_us = None;
        self.headroom_evidence_us = 0;
    }
}
