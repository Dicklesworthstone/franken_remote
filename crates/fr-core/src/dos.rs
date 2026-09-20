#![forbid(unsafe_code)]
//! Denial-of-service bounds, rate limiting, and flood fairness (plan section 19.3).
//!
//! An admitted peer, a pre-admission stranger, or a flood of metadata must
//! never exhaust the host. This module provides:
//! - Rate limiting using monotonic host time ([`RateLimiterRegistry`], [`TokenBucket`]).
//! - Global and per-session admission accounting ([`AdmissionAccountant`]).
//! - Multi-lane flood-fair queuing with count AND byte bounds ([`FloodFairQueue`]).
//! - Idle session watchdog resisting unauthenticated garbage ([`IdleSessionTracker`]).
//! - Codec probe lifecycle binding to cancel orphan probes ([`CodecProbeTracker`]).
//! - Pre-allocation resource validation for decoded dimensions ([`PreAllocValidator`]).
//! - Metadata fragment bound enforcement ([`MetadataFragmentValidator`]).
//! - Strongly typed refusals ([`DosRefusal`]).

use crate::ids::RemoteSessionId;
use crate::limits::{LimitField, ProtocolLimits};
use crate::time::{HostDuration, HostInstant};
use core::error::Error;
use core::fmt;
use std::collections::VecDeque;

/// Priority lane in a flood-fair queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QueueLane {
    /// Emergency/high-priority events: revoke, lease expiry, teardown, panic stop.
    /// Never starved or dropped by bulk traffic.
    High,
    /// Normal operational messages: control requests, heartbeats, input.
    Normal,
    /// Bulk and recovery traffic: fragment retransmissions, probes, cursor uploads.
    Bulk,
}

impl fmt::Display for QueueLane {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::High => f.write_str("High"),
            Self::Normal => f.write_str("Normal"),
            Self::Bulk => f.write_str("Bulk"),
        }
    }
}

/// Typed refusal from denial-of-service bounds enforcement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DosRefusal {
    /// Rate limit exceeded on a specific operation.
    RateLimitExceeded {
        field: LimitField,
        retry_after: HostDuration,
    },
    /// Resource capacity exceeded.
    CapacityExceeded {
        field: LimitField,
        current: u64,
        limit: u64,
    },
    /// Queue lane count capacity exceeded.
    QueueFull {
        lane: QueueLane,
        current_count: usize,
        max_count: usize,
    },
    /// Queue lane byte capacity exceeded.
    ByteLimitExceeded {
        lane: QueueLane,
        current_bytes: usize,
        max_bytes: usize,
    },
    /// Generation mismatch or stale generation.
    StaleGeneration { expected: u64, actual: u64 },
    /// Decoded surface pre-allocation exceeds resource budget before allocation.
    DecoderAllocationExceeded {
        requested_bytes: u64,
        max_bytes: u64,
    },
    /// Excessive metadata fragments attempt (e.g. zero-byte fragment exhaustion).
    ExcessiveMetadataFragments { count: u32, max: u32 },
    /// Session expired due to inactivity.
    SessionIdleExpired {
        idle_duration: HostDuration,
        timeout: HostDuration,
    },
    /// Unauthenticated garbage flood detected.
    UnauthenticatedFloodDetected { attempts: u32 },
    /// Unicode name exceeds byte length limit.
    UnicodeNameTooLong { bytes: usize, max_bytes: usize },
    /// Cursor dimensions exceed ceiling.
    CursorTooLarge {
        width: u32,
        height: u32,
        max_dimension: u32,
    },
    /// Parameter set (VPS/SPS/PPS) exceeds size limit.
    ParameterSetTooLarge { bytes: usize, max_bytes: u32 },
}

impl fmt::Display for DosRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RateLimitExceeded { field, retry_after } => {
                write!(
                    f,
                    "rate limit exceeded for {field}; retry after {retry_after}"
                )
            }
            Self::CapacityExceeded {
                field,
                current,
                limit,
            } => {
                write!(
                    f,
                    "capacity exceeded for {field}: current {current} >= limit {limit}"
                )
            }
            Self::QueueFull {
                lane,
                current_count,
                max_count,
            } => {
                write!(f, "{lane} queue count full: {current_count}/{max_count}")
            }
            Self::ByteLimitExceeded {
                lane,
                current_bytes,
                max_bytes,
            } => {
                write!(f, "{lane} queue bytes full: {current_bytes}/{max_bytes}")
            }
            Self::StaleGeneration { expected, actual } => {
                write!(f, "stale generation: expected {expected}, got {actual}")
            }
            Self::DecoderAllocationExceeded {
                requested_bytes,
                max_bytes,
            } => {
                write!(
                    f,
                    "decoder pre-allocation exceeded: {requested_bytes} > {max_bytes}"
                )
            }
            Self::ExcessiveMetadataFragments { count, max } => {
                write!(f, "excessive metadata fragments: {count} > {max}")
            }
            Self::SessionIdleExpired {
                idle_duration,
                timeout,
            } => {
                write!(
                    f,
                    "session idle expired: idle {idle_duration} > timeout {timeout}"
                )
            }
            Self::UnauthenticatedFloodDetected { attempts } => {
                write!(
                    f,
                    "unauthenticated garbage flood detected after {attempts} attempts"
                )
            }
            Self::UnicodeNameTooLong { bytes, max_bytes } => {
                write!(f, "unicode name too long: {bytes} > {max_bytes} bytes")
            }
            Self::CursorTooLarge {
                width,
                height,
                max_dimension,
            } => {
                write!(
                    f,
                    "cursor {width}x{height} exceeds max dimension {max_dimension}"
                )
            }
            Self::ParameterSetTooLarge { bytes, max_bytes } => {
                write!(f, "parameter set too large: {bytes} > {max_bytes} bytes")
            }
        }
    }
}

impl Error for DosRefusal {}

/// Statistics for a flood-fair queue.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueueStats {
    pub admitted_high_count: u64,
    pub admitted_high_bytes: u64,
    pub admitted_normal_count: u64,
    pub admitted_normal_bytes: u64,
    pub admitted_bulk_count: u64,
    pub admitted_bulk_bytes: u64,
    pub dropped_bulk_count: u64,
    pub dropped_bulk_bytes: u64,
}

/// Multi-lane flood-fair queue with count AND byte bounds per lane.
///
/// High-priority events (revoke, expiry, teardown) are never starved or dropped
/// by a flood of low-priority or bulk requests (e.g. recovery requests).
#[derive(Debug)]
pub struct FloodFairQueue<T> {
    high: VecDeque<(T, usize)>,
    normal: VecDeque<(T, usize)>,
    bulk: VecDeque<(T, usize)>,
    high_bytes: usize,
    normal_bytes: usize,
    bulk_bytes: usize,
    max_high_count: usize,
    max_high_bytes: usize,
    max_normal_count: usize,
    max_normal_bytes: usize,
    max_bulk_count: usize,
    max_bulk_bytes: usize,
    deficit_counter: u32,
    stats: QueueStats,
}

impl<T> FloodFairQueue<T> {
    /// Creates a new flood-fair queue with explicit count and byte bounds per lane.
    pub fn new(
        max_high_count: usize,
        max_high_bytes: usize,
        max_normal_count: usize,
        max_normal_bytes: usize,
        max_bulk_count: usize,
        max_bulk_bytes: usize,
    ) -> Self {
        Self {
            high: VecDeque::with_capacity(max_high_count.min(64)),
            normal: VecDeque::with_capacity(max_normal_count.min(128)),
            bulk: VecDeque::with_capacity(max_bulk_count.min(256)),
            high_bytes: 0,
            normal_bytes: 0,
            bulk_bytes: 0,
            max_high_count,
            max_high_bytes,
            max_normal_count,
            max_normal_bytes,
            max_bulk_count,
            max_bulk_bytes,
            deficit_counter: 0,
            stats: QueueStats::default(),
        }
    }

    /// Creates a flood-fair queue sized from governing protocol limits.
    pub fn from_limits(limits: &ProtocolLimits) -> Self {
        Self::new(
            64,
            256 * 1024,
            128,
            usize::try_from(limits.max_control_message_bytes())
                .unwrap_or(usize::MAX)
                .saturating_mul(4),
            256,
            usize::try_from(limits.per_viewer_compressed_bytes()).unwrap_or(usize::MAX) / 4,
        )
    }

    /// Pushes an item into the specified priority lane.
    ///
    /// Returns `Err(DosRefusal)` if lane count or byte capacity is exceeded.
    pub fn push(&mut self, lane: QueueLane, item: T, byte_len: usize) -> Result<(), DosRefusal> {
        match lane {
            QueueLane::High => {
                if self.high.len() >= self.max_high_count {
                    return Err(DosRefusal::QueueFull {
                        lane,
                        current_count: self.high.len(),
                        max_count: self.max_high_count,
                    });
                }
                if self.high_bytes.saturating_add(byte_len) > self.max_high_bytes {
                    return Err(DosRefusal::ByteLimitExceeded {
                        lane,
                        current_bytes: self.high_bytes,
                        max_bytes: self.max_high_bytes,
                    });
                }
                self.high.push_back((item, byte_len));
                self.high_bytes = self.high_bytes.saturating_add(byte_len);
                self.stats.admitted_high_count += 1;
                self.stats.admitted_high_bytes += byte_len as u64;
                Ok(())
            }
            QueueLane::Normal => {
                if self.normal.len() >= self.max_normal_count {
                    return Err(DosRefusal::QueueFull {
                        lane,
                        current_count: self.normal.len(),
                        max_count: self.max_normal_count,
                    });
                }
                if self.normal_bytes.saturating_add(byte_len) > self.max_normal_bytes {
                    return Err(DosRefusal::ByteLimitExceeded {
                        lane,
                        current_bytes: self.normal_bytes,
                        max_bytes: self.max_normal_bytes,
                    });
                }
                self.normal.push_back((item, byte_len));
                self.normal_bytes = self.normal_bytes.saturating_add(byte_len);
                self.stats.admitted_normal_count += 1;
                self.stats.admitted_normal_bytes += byte_len as u64;
                Ok(())
            }
            QueueLane::Bulk => {
                if self.bulk.len() >= self.max_bulk_count {
                    self.stats.dropped_bulk_count += 1;
                    self.stats.dropped_bulk_bytes += byte_len as u64;
                    return Err(DosRefusal::QueueFull {
                        lane,
                        current_count: self.bulk.len(),
                        max_count: self.max_bulk_count,
                    });
                }
                if self.bulk_bytes.saturating_add(byte_len) > self.max_bulk_bytes {
                    self.stats.dropped_bulk_count += 1;
                    self.stats.dropped_bulk_bytes += byte_len as u64;
                    return Err(DosRefusal::ByteLimitExceeded {
                        lane,
                        current_bytes: self.bulk_bytes,
                        max_bytes: self.max_bulk_bytes,
                    });
                }
                self.bulk.push_back((item, byte_len));
                self.bulk_bytes = self.bulk_bytes.saturating_add(byte_len);
                self.stats.admitted_bulk_count += 1;
                self.stats.admitted_bulk_bytes += byte_len as u64;
                Ok(())
            }
        }
    }

    /// Pops the next item according to strict anti-starvation policy:
    /// 1. High lane is always serviced first.
    /// 2. Normal lane is serviced with bounded deficit round-robin against Bulk.
    pub fn pop(&mut self) -> Option<(QueueLane, T)> {
        // High priority is always serviced first.
        if let Some((item, bytes)) = self.high.pop_front() {
            self.high_bytes = self.high_bytes.saturating_sub(bytes);
            return Some((QueueLane::High, item));
        }

        // Deficit round-robin between Normal (weight 3) and Bulk (weight 1).
        if !self.normal.is_empty()
            && (self.deficit_counter < 3 || self.bulk.is_empty())
            && let Some((item, bytes)) = self.normal.pop_front()
        {
            self.normal_bytes = self.normal_bytes.saturating_sub(bytes);
            self.deficit_counter = self.deficit_counter.saturating_add(1);
            return Some((QueueLane::Normal, item));
        }

        if let Some((item, bytes)) = self.bulk.pop_front() {
            self.bulk_bytes = self.bulk_bytes.saturating_sub(bytes);
            self.deficit_counter = 0;
            return Some((QueueLane::Bulk, item));
        }

        // Final fallback if Normal had items and bulk was empty.
        if let Some((item, bytes)) = self.normal.pop_front() {
            self.normal_bytes = self.normal_bytes.saturating_sub(bytes);
            self.deficit_counter = 0;
            return Some((QueueLane::Normal, item));
        }

        None
    }

    /// Total count of items currently in the queue across all lanes.
    pub fn len(&self) -> usize {
        self.high.len() + self.normal.len() + self.bulk.len()
    }

    /// True if all lanes are empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Current count of items in a specific lane.
    pub fn lane_count(&self, lane: QueueLane) -> usize {
        match lane {
            QueueLane::High => self.high.len(),
            QueueLane::Normal => self.normal.len(),
            QueueLane::Bulk => self.bulk.len(),
        }
    }

    /// Current byte sum of items in a specific lane.
    pub fn lane_bytes(&self, lane: QueueLane) -> usize {
        match lane {
            QueueLane::High => self.high_bytes,
            QueueLane::Normal => self.normal_bytes,
            QueueLane::Bulk => self.bulk_bytes,
        }
    }

    /// Returns queue statistics.
    pub fn stats(&self) -> QueueStats {
        self.stats
    }

    /// Clears all lanes.
    pub fn clear(&mut self) {
        self.high.clear();
        self.normal.clear();
        self.bulk.clear();
        self.high_bytes = 0;
        self.normal_bytes = 0;
        self.bulk_bytes = 0;
        self.deficit_counter = 0;
    }
}

/// Token bucket rate limiter with microsecond precision using host-monotonic time.
#[derive(Debug, Clone, Copy)]
pub struct TokenBucket {
    rate_per_sec: u32,
    capacity: u32,
    scaled_tokens: u64,
    last_leak: HostInstant,
}

const SCALE: u64 = 1_000_000;

impl TokenBucket {
    /// Creates a new token bucket with rate per second and burst capacity.
    pub fn new(rate_per_sec: u32, capacity: u32, start: HostInstant) -> Self {
        Self {
            rate_per_sec,
            capacity,
            scaled_tokens: u64::from(capacity).saturating_mul(SCALE),
            last_leak: start,
        }
    }

    /// Creates a new token bucket with rate per minute.
    pub fn new_per_min(rate_per_min: u32, capacity: u32, start: HostInstant) -> Self {
        // Rate per second calculated as ceil(rate_per_min / 60) with floor 1.
        let rate_per_sec = rate_per_min.div_ceil(60).max(1);
        Self::new(rate_per_sec, capacity, start)
    }

    /// Tries to consume tokens at `now`.
    pub fn try_consume(
        &mut self,
        now: HostInstant,
        tokens: u32,
        field: LimitField,
    ) -> Result<(), DosRefusal> {
        self.replenish(now);
        let needed = u64::from(tokens).saturating_mul(SCALE);
        if self.scaled_tokens >= needed {
            self.scaled_tokens -= needed;
            Ok(())
        } else {
            let missing = needed - self.scaled_tokens;
            let retry_micros = if self.rate_per_sec > 0 {
                missing / u64::from(self.rate_per_sec)
            } else {
                1_000_000
            };
            Err(DosRefusal::RateLimitExceeded {
                field,
                retry_after: HostDuration::from_micros(retry_micros.max(1)),
            })
        }
    }

    fn replenish(&mut self, now: HostInstant) {
        if let Some(elapsed) = now.checked_duration_since(self.last_leak) {
            let added = elapsed
                .as_micros()
                .saturating_mul(u64::from(self.rate_per_sec));
            let cap = u64::from(self.capacity).saturating_mul(SCALE);
            self.scaled_tokens = self.scaled_tokens.saturating_add(added).min(cap);
            self.last_leak = now;
        }
    }
}

/// Registry of rate limiters covering all critical protocol operations.
#[derive(Debug, Clone, Copy)]
pub struct RateLimiterRegistry {
    preadmission: TokenBucket,
    codec_probes: TokenBucket,
    control_requests: TokenBucket,
    cursor_uploads: TokenBucket,
    recovery_requests: TokenBucket,
    diagnostic_exports: TokenBucket,
    decoder_reconfigs: TokenBucket,
    worker_restarts: TokenBucket,
}

impl RateLimiterRegistry {
    /// Creates a rate limiter registry configured from governing protocol limits.
    pub fn from_limits(limits: &ProtocolLimits, start: HostInstant) -> Self {
        Self {
            preadmission: TokenBucket::new(
                limits.max_preadmission_rate_per_sec(),
                limits.max_preadmission_rate_per_sec(),
                start,
            ),
            codec_probes: TokenBucket::new_per_min(
                limits.max_codec_probes_per_min(),
                limits.max_codec_probes_per_min().min(5),
                start,
            ),
            control_requests: TokenBucket::new(
                limits.max_control_requests_per_sec(),
                limits.max_control_requests_per_sec(),
                start,
            ),
            cursor_uploads: TokenBucket::new(
                limits.max_cursor_uploads_per_sec(),
                limits.max_cursor_uploads_per_sec(),
                start,
            ),
            recovery_requests: TokenBucket::new(
                limits.max_recovery_requests_per_sec(),
                limits.max_recovery_requests_per_sec(),
                start,
            ),
            diagnostic_exports: TokenBucket::new_per_min(
                limits.max_diagnostic_exports_per_min(),
                limits.max_diagnostic_exports_per_min().min(3),
                start,
            ),
            decoder_reconfigs: TokenBucket::new_per_min(
                limits.max_decoder_reconfigurations_per_min(),
                limits.max_decoder_reconfigurations_per_min().min(5),
                start,
            ),
            worker_restarts: TokenBucket::new_per_min(
                limits.max_worker_restarts_per_min(),
                limits.max_worker_restarts_per_min().min(3),
                start,
            ),
        }
    }

    /// Checks rate limit for pre-admission connection.
    pub fn check_preadmission(&mut self, now: HostInstant) -> Result<(), DosRefusal> {
        self.preadmission
            .try_consume(now, 1, LimitField::PreadmissionRatePerSec)
    }

    /// Checks rate limit for expensive codec probe.
    pub fn check_codec_probe(&mut self, now: HostInstant) -> Result<(), DosRefusal> {
        self.codec_probes
            .try_consume(now, 1, LimitField::CodecProbesPerMin)
    }

    /// Checks rate limit for ordinary control requests.
    pub fn check_control_request(&mut self, now: HostInstant) -> Result<(), DosRefusal> {
        self.control_requests
            .try_consume(now, 1, LimitField::ControlRequestsPerSec)
    }

    /// Checks rate limit for cursor shape uploads.
    pub fn check_cursor_upload(&mut self, now: HostInstant) -> Result<(), DosRefusal> {
        self.cursor_uploads
            .try_consume(now, 1, LimitField::CursorUploadsPerSec)
    }

    /// Checks rate limit for recovery requests.
    pub fn check_recovery_request(&mut self, now: HostInstant) -> Result<(), DosRefusal> {
        self.recovery_requests
            .try_consume(now, 1, LimitField::RecoveryRequestsPerSec)
    }

    /// Checks rate limit for diagnostic exports.
    pub fn check_diagnostic_export(&mut self, now: HostInstant) -> Result<(), DosRefusal> {
        self.diagnostic_exports
            .try_consume(now, 1, LimitField::DiagnosticExportsPerMin)
    }

    /// Checks rate limit for decoder reconfigurations.
    pub fn check_decoder_reconfig(&mut self, now: HostInstant) -> Result<(), DosRefusal> {
        self.decoder_reconfigs
            .try_consume(now, 1, LimitField::DecoderReconfigurationsPerMin)
    }

    /// Checks rate limit for worker restarts.
    pub fn check_worker_restart(&mut self, now: HostInstant) -> Result<(), DosRefusal> {
        self.worker_restarts
            .try_consume(now, 1, LimitField::WorkerRestartsPerMin)
    }
}

/// Admission accounting for global resources: encoder sessions, GPU surfaces,
/// viewers, half-attached channels, and memory.
#[derive(Debug, Default, Clone)]
pub struct AdmissionAccountant {
    active_encoder_sessions: u32,
    active_gpu_surfaces: u32,
    active_viewers: u32,
    active_half_attached: u32,
    active_pending_approvals: u32,
    allocated_bandwidth_bps: u64,
    viewer_memory: Vec<(RemoteSessionId, u64)>,
}

impl AdmissionAccountant {
    /// Creates a fresh admission accountant.
    pub fn new() -> Self {
        Self::default()
    }

    /// Attempts to acquire an encoder session under governing limits.
    pub fn acquire_encoder_session(&mut self, limits: &ProtocolLimits) -> Result<(), DosRefusal> {
        if self.active_encoder_sessions >= limits.max_encoder_sessions() {
            return Err(DosRefusal::CapacityExceeded {
                field: LimitField::EncoderSessions,
                current: u64::from(self.active_encoder_sessions),
                limit: u64::from(limits.max_encoder_sessions()),
            });
        }
        self.active_encoder_sessions += 1;
        Ok(())
    }

    /// Releases an encoder session.
    pub fn release_encoder_session(&mut self) {
        self.active_encoder_sessions = self.active_encoder_sessions.saturating_sub(1);
    }

    /// Attempts to acquire GPU surfaces under governing limits.
    pub fn acquire_gpu_surfaces(
        &mut self,
        count: u32,
        limits: &ProtocolLimits,
    ) -> Result<(), DosRefusal> {
        let proposed = self.active_gpu_surfaces.saturating_add(count);
        if proposed > limits.max_gpu_surfaces() {
            return Err(DosRefusal::CapacityExceeded {
                field: LimitField::GpuSurfaces,
                current: u64::from(self.active_gpu_surfaces),
                limit: u64::from(limits.max_gpu_surfaces()),
            });
        }
        self.active_gpu_surfaces = proposed;
        Ok(())
    }

    /// Releases GPU surfaces.
    pub fn release_gpu_surfaces(&mut self, count: u32) {
        self.active_gpu_surfaces = self.active_gpu_surfaces.saturating_sub(count);
    }

    /// Attempts to acquire a viewer seat under governing limits.
    pub fn acquire_viewer(
        &mut self,
        session_id: RemoteSessionId,
        limits: &ProtocolLimits,
    ) -> Result<(), DosRefusal> {
        if self.active_viewers >= limits.max_viewers() {
            return Err(DosRefusal::CapacityExceeded {
                field: LimitField::Viewers,
                current: u64::from(self.active_viewers),
                limit: u64::from(limits.max_viewers()),
            });
        }
        self.active_viewers += 1;
        self.viewer_memory.push((session_id, 0));
        Ok(())
    }

    /// Releases a viewer seat and clears its memory allocation.
    pub fn release_viewer(&mut self, session_id: RemoteSessionId) {
        if let Some(pos) = self
            .viewer_memory
            .iter()
            .position(|(s, _)| *s == session_id)
        {
            self.viewer_memory.swap_remove(pos);
            self.active_viewers = self.active_viewers.saturating_sub(1);
        }
    }

    /// Attempts to acquire a half-attached channel slot under governing limits.
    pub fn acquire_half_attached(&mut self, limits: &ProtocolLimits) -> Result<(), DosRefusal> {
        if self.active_half_attached >= limits.max_half_attached_channels() {
            return Err(DosRefusal::CapacityExceeded {
                field: LimitField::HalfAttachedChannels,
                current: u64::from(self.active_half_attached),
                limit: u64::from(limits.max_half_attached_channels()),
            });
        }
        self.active_half_attached += 1;
        Ok(())
    }

    /// Releases a half-attached channel slot.
    pub fn release_half_attached(&mut self) {
        self.active_half_attached = self.active_half_attached.saturating_sub(1);
    }

    /// Attempts to acquire a pending approval slot under governing limits.
    pub fn acquire_pending_approval(&mut self, limits: &ProtocolLimits) -> Result<(), DosRefusal> {
        if self.active_pending_approvals >= limits.max_pending_approvals() {
            return Err(DosRefusal::CapacityExceeded {
                field: LimitField::PendingApprovals,
                current: u64::from(self.active_pending_approvals),
                limit: u64::from(limits.max_pending_approvals()),
            });
        }
        self.active_pending_approvals += 1;
        Ok(())
    }

    /// Releases a pending approval slot.
    pub fn release_pending_approval(&mut self) {
        self.active_pending_approvals = self.active_pending_approvals.saturating_sub(1);
    }

    /// Allocates memory to a viewer session under the per-viewer compressed media budget.
    pub fn allocate_viewer_memory(
        &mut self,
        session_id: RemoteSessionId,
        bytes: u64,
        limits: &ProtocolLimits,
    ) -> Result<(), DosRefusal> {
        if let Some((_, current)) = self
            .viewer_memory
            .iter_mut()
            .find(|(s, _)| *s == session_id)
        {
            let proposed = current.saturating_add(bytes);
            if proposed > limits.per_viewer_compressed_bytes() {
                return Err(DosRefusal::CapacityExceeded {
                    field: LimitField::PerViewerCompressedBytes,
                    current: *current,
                    limit: limits.per_viewer_compressed_bytes(),
                });
            }
            *current = proposed;
            Ok(())
        } else {
            Err(DosRefusal::CapacityExceeded {
                field: LimitField::Viewers,
                current: 0,
                limit: u64::from(limits.max_viewers()),
            })
        }
    }

    /// Releases memory from a viewer session.
    pub fn release_viewer_memory(&mut self, session_id: RemoteSessionId, bytes: u64) {
        if let Some((_, current)) = self
            .viewer_memory
            .iter_mut()
            .find(|(s, _)| *s == session_id)
        {
            *current = current.saturating_sub(bytes);
        }
    }

    /// Attempts to allocate outbound bandwidth under governing limits.
    pub fn allocate_bandwidth(
        &mut self,
        bps: u64,
        limits: &ProtocolLimits,
    ) -> Result<(), DosRefusal> {
        let proposed = self.allocated_bandwidth_bps.saturating_add(bps);
        if proposed > limits.max_bandwidth_bps() {
            return Err(DosRefusal::CapacityExceeded {
                field: LimitField::BandwidthBps,
                current: self.allocated_bandwidth_bps,
                limit: limits.max_bandwidth_bps(),
            });
        }
        self.allocated_bandwidth_bps = proposed;
        Ok(())
    }

    /// Releases outbound bandwidth.
    pub fn release_bandwidth(&mut self, bps: u64) {
        self.allocated_bandwidth_bps = self.allocated_bandwidth_bps.saturating_sub(bps);
    }

    /// Currently allocated bandwidth in bits per second.
    pub fn allocated_bandwidth_bps(&self) -> u64 {
        self.allocated_bandwidth_bps
    }

    /// Currently active encoder sessions.
    pub fn active_encoder_sessions(&self) -> u32 {
        self.active_encoder_sessions
    }

    /// Currently active GPU surfaces.
    pub fn active_gpu_surfaces(&self) -> u32 {
        self.active_gpu_surfaces
    }

    /// Currently active viewers.
    pub fn active_viewers(&self) -> u32 {
        self.active_viewers
    }

    /// Currently active half-attached channels.
    pub fn active_half_attached(&self) -> u32 {
        self.active_half_attached
    }

    /// Currently active pending approvals.
    pub fn active_pending_approvals(&self) -> u32 {
        self.active_pending_approvals
    }
}

/// Idle session watchdog that refuses to extend timeout on unauthenticated garbage or stale challenges.
#[derive(Debug, Clone, Copy)]
pub struct IdleSessionTracker {
    last_authenticated: HostInstant,
    last_unauthenticated: Option<HostInstant>,
    consecutive_unauthenticated: u32,
    max_consecutive_unauthenticated: u32,
}

impl IdleSessionTracker {
    /// Creates a new idle tracker initialized at `start`.
    pub fn new(start: HostInstant) -> Self {
        Self {
            last_authenticated: start,
            last_unauthenticated: None,
            consecutive_unauthenticated: 0,
            max_consecutive_unauthenticated: 10,
        }
    }

    /// Records valid authenticated traffic, extending the idle timeout.
    pub fn record_authenticated(&mut self, now: HostInstant) {
        self.last_authenticated = now;
        self.consecutive_unauthenticated = 0;
    }

    /// Records unauthenticated traffic or stale challenge.
    ///
    /// CRITICAL SECURITY INVARIANT (Plan §19.3):
    /// This NEVER updates `last_authenticated` and cannot extend the session timeout!
    /// Returns `Err(DosRefusal::UnauthenticatedFloodDetected)` if garbage attempts exceed the flood threshold.
    pub fn record_unauthenticated_garbage(&mut self, now: HostInstant) -> Result<(), DosRefusal> {
        self.last_unauthenticated = Some(now);
        self.consecutive_unauthenticated = self.consecutive_unauthenticated.saturating_add(1);
        if self.consecutive_unauthenticated > self.max_consecutive_unauthenticated {
            Err(DosRefusal::UnauthenticatedFloodDetected {
                attempts: self.consecutive_unauthenticated,
            })
        } else {
            Ok(())
        }
    }

    /// Checks whether the session has timed out due to authenticated inactivity.
    pub fn check_idle_timeout(
        &self,
        now: HostInstant,
        timeout_secs: u32,
    ) -> Result<(), DosRefusal> {
        let timeout =
            HostDuration::from_millis_checked(u64::from(timeout_secs).saturating_mul(1000))
                .unwrap_or(HostDuration::ZERO);
        let idle = now
            .checked_duration_since(self.last_authenticated)
            .unwrap_or(HostDuration::ZERO);
        if idle > timeout {
            Err(DosRefusal::SessionIdleExpired {
                idle_duration: idle,
                timeout,
            })
        } else {
            Ok(())
        }
    }

    /// Last authenticated activity timestamp.
    pub fn last_authenticated(&self) -> HostInstant {
        self.last_authenticated
    }
}

/// Codec probe lifecycle tracker to cancel orphan probes when requesting sessions disconnect.
#[derive(Debug, Default, Clone)]
pub struct CodecProbeTracker {
    probes: Vec<(RemoteSessionId, u32)>,
}

impl CodecProbeTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a probe requested by a session.
    pub fn register_probe(
        &mut self,
        session_id: RemoteSessionId,
        probe_id: u32,
        limits: &ProtocolLimits,
    ) -> Result<(), DosRefusal> {
        if self.probes.len() >= limits.max_codec_probes_per_min() as usize {
            return Err(DosRefusal::CapacityExceeded {
                field: LimitField::CodecProbesPerMin,
                current: self.probes.len() as u64,
                limit: u64::from(limits.max_codec_probes_per_min()),
            });
        }
        self.probes.push((session_id, probe_id));
        Ok(())
    }

    /// Notifies that a session disconnected/closed, returning all orphaned probe IDs for immediate cancellation.
    pub fn on_session_closed(&mut self, session_id: RemoteSessionId) -> Vec<u32> {
        let mut cancelled = Vec::new();
        let mut i = 0;
        while i < self.probes.len() {
            if self.probes[i].0 == session_id {
                cancelled.push(self.probes.swap_remove(i).1);
            } else {
                i += 1;
            }
        }
        cancelled
    }

    /// Marks a probe as completed.
    pub fn complete_probe(&mut self, session_id: RemoteSessionId, probe_id: u32) -> bool {
        if let Some(pos) = self
            .probes
            .iter()
            .position(|(s, p)| *s == session_id && *p == probe_id)
        {
            self.probes.swap_remove(pos);
            true
        } else {
            false
        }
    }

    /// Active probe count.
    pub fn active_probe_count(&self) -> usize {
        self.probes.len()
    }
}

/// Pre-allocation resource validator: checks declared dimensions and DPB requirements
/// BEFORE allocating decoded buffers or invoking foreign drivers.
#[derive(Debug, Clone, Copy)]
pub struct PreAllocValidator;

impl PreAllocValidator {
    /// Validates declared dimensions and reference buffer memory requirements.
    ///
    /// Computes the decoded picture buffer (DPB) memory:
    /// `width * height * 1.5 * (bit_depth / 8) * (reference_surfaces + 2)`
    /// and checks against the governing limits before any allocation.
    pub fn validate_surface_allocation(
        width: u32,
        height: u32,
        bit_depth: u8,
        reference_surfaces: u32,
        limits: &ProtocolLimits,
    ) -> Result<u64, DosRefusal> {
        // Validate dimensions first
        if limits.validate_coded_dimensions(width, height).is_err() {
            return Err(DosRefusal::CursorTooLarge {
                width,
                height,
                max_dimension: limits.max_dimension_pixels(),
            });
        }

        let bpp_numerator = match bit_depth {
            10 => 4_u64, // 2 bytes for Y + 2 for UV = 4
            _ => 3_u64,  // 1.5 bytes per pixel for 4:2:0 8-bit (3/2)
        };

        let pixels = u64::from(width).saturating_mul(u64::from(height));
        let frame_bytes = pixels.saturating_mul(bpp_numerator) / 2;
        let total_buffers = u64::from(reference_surfaces).saturating_add(2); // DPB references + working buffers
        let total_required = frame_bytes.saturating_mul(total_buffers);

        // Must fit within per-viewer memory budget
        if total_required > limits.per_viewer_compressed_bytes() {
            return Err(DosRefusal::DecoderAllocationExceeded {
                requested_bytes: total_required,
                max_bytes: limits.per_viewer_compressed_bytes(),
            });
        }

        Ok(total_required)
    }
}

/// Metadata fragment count validator: protects against zero-byte fragment exhaustion attacks.
#[derive(Debug, Clone, Copy)]
pub struct MetadataFragmentValidator;

impl MetadataFragmentValidator {
    /// Validates metadata fragment count against payload bytes.
    ///
    /// Thousands of zero-byte fragments without crossing payload limits are rejected.
    pub fn validate(
        fragment_count: u32,
        total_payload_bytes: usize,
        limits: &ProtocolLimits,
    ) -> Result<(), DosRefusal> {
        if fragment_count > limits.max_fragments_per_access_unit() {
            return Err(DosRefusal::ExcessiveMetadataFragments {
                count: fragment_count,
                max: limits.max_fragments_per_access_unit(),
            });
        }
        // Zero-byte payload cannot have multiple fragments
        if total_payload_bytes == 0 && fragment_count > 1 {
            return Err(DosRefusal::ExcessiveMetadataFragments {
                count: fragment_count,
                max: 1,
            });
        }
        Ok(())
    }
}
