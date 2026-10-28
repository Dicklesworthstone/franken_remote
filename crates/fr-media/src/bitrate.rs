//! Bounded encoder bitrate decisions from actual send-admission evidence.
//!
//! This policy is not a bandwidth estimator. It keeps the original configured
//! rate as a ceiling, lowers only after sustained sender pressure, and probes
//! slowly after measured headroom. A proposal is not an applied codec change.
//! One capture token joins every request to its actual native result; a deferred
//! request retains its identity without renewing a deadline or rate allowance.

use crate::pacing::{Availability, Mode, ReceiverEvidence, Report};

const PRESSURE_US: u64 = 100_000;
const CHANGE_DWELL_US: u64 = 2_000_000;
const HEADROOM_US: u64 = 4_000_000;
const EVIDENCE_GAP_US: u64 = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidBitrate,
    ClockRegression,
    Closed,
    CaptureInFlight,
    UnexpectedCapture,
    UnexpectedBitrate,
    MissingIdr,
    SequenceExhausted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    SendPressure,
    Headroom,
    /// Cancel a retained native target after the evidence scope changed.
    EvidenceReset,
}

/// One pending decision, unchanged while native captures defer its target.
/// This is scheduling metadata, never source, input or observation authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Change {
    id: u64,
    bitrate: u32,
    reason: Reason,
}
impl Change {
    pub const fn id(self) -> u64 {
        self.id
    }
    pub const fn bitrate(self) -> u32 {
        self.bitrate
    }
    pub const fn reason(self) -> Reason {
        self.reason
    }
}

/// One original capture's budget request. Its identity is local to its policy
/// owner; the surrounding worker protocol must still enforce source/IPC epochs.
/// Cancellation cannot pretend that this already-submitted request had no effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub struct Capture {
    sequence: u64,
    bitrate: u32,
    previous: u32,
}
impl Capture {
    pub const fn bitrate(self) -> u32 {
        self.bitrate
    }
}

#[derive(Debug, Default)]
struct Headroom {
    last_positive: Option<u64>,
    previous_positive: bool,
    measured_us: u64,
}
impl Headroom {
    fn clear(&mut self) {
        *self = Self::default();
    }
    fn observe(&mut self, now: u64, positive: bool, negative: bool) -> u64 {
        if negative
            || self
                .last_positive
                .is_some_and(|last| now - last > EVIDENCE_GAP_US)
        {
            self.clear();
        }
        if positive {
            if self.previous_positive
                && let Some(last) = self.last_positive
            {
                self.measured_us = self.measured_us.saturating_add(now - last);
            }
            self.last_positive = Some(now);
            self.previous_positive = true;
            self.measured_us
        } else {
            // Neither edge adjacent to Unknown contributes any measured time.
            self.previous_positive = false;
            0
        }
    }
}

/// Fixed-space state for one original encoder. Replacing a view or membership
/// scope clears evidence, not the applied rate, original ceiling or change dwell.
/// Native code independently enforces its own real-clock reconfiguration bound.
#[derive(Debug)]
pub struct Controller {
    ceiling: u32,
    floor: u32,
    applied: u32,
    pending: Option<Change>,
    in_flight: Option<Capture>,
    native_target: u32,
    next_change: u64,
    next_capture: u64,
    last_now: Option<u64>,
    last_sample: Option<u64>,
    changed_at: Option<u64>,
    pressure_since: Option<u64>,
    headroom: Headroom,
    closed: bool,
}
impl Controller {
    pub fn new(initial_bitrate: u32) -> Result<Self, Error> {
        if !(10_000..=200_000_000).contains(&initial_bitrate) {
            return Err(Error::InvalidBitrate);
        }
        Ok(Self {
            ceiling: initial_bitrate,
            floor: (initial_bitrate / 8).max(100_000).min(initial_bitrate),
            applied: initial_bitrate,
            pending: None,
            in_flight: None,
            native_target: initial_bitrate,
            next_change: 1,
            next_capture: 1,
            last_now: None,
            last_sample: None,
            changed_at: None,
            pressure_since: None,
            headroom: Headroom::default(),
            closed: false,
        })
    }
    pub const fn applied_bitrate(&self) -> u32 {
        self.applied
    }
    pub const fn ceiling_bitrate(&self) -> u32 {
        self.ceiling
    }
    pub const fn minimum_bitrate(&self) -> u32 {
        self.floor
    }
    pub const fn pending(&self) -> Option<Change> {
        self.pending
    }

    /// Use the original capture-pacing report, including its measured evidence.
    /// A maximum capture interval does not suppress a necessary bitrate cut.
    /// Decoder, source-work and capture-credit pressure alone never reduce rate.
    pub fn observe(&mut self, report: &Report) -> Result<(), Error> {
        let now = report.sample.now_us;
        self.clock(now)?;
        let gap = self
            .last_sample
            .is_some_and(|last| now - last > EVIDENCE_GAP_US);
        self.last_sample = Some(now);
        if gap || report.reason == crate::pacing::Reason::EvidenceGap {
            self.clear_measurements();
        }
        let blocked = report.mode == Mode::Active && report.sample.send == Availability::Blocked;
        let pressure = if blocked {
            now - *self.pressure_since.get_or_insert(now)
        } else {
            self.pressure_since = None;
            0
        };
        let headroom = self.headroom.observe(
            now,
            report.mode == Mode::Active && report.headroom_us > 0,
            headroom_negative(report),
        );
        if self.pending.is_some() || self.changed_at.is_some_and(|at| now - at < CHANGE_DWELL_US) {
            return Ok(());
        }
        if blocked && pressure >= PRESSURE_US && self.applied > self.floor {
            self.propose((self.applied / 2).max(self.floor), Reason::SendPressure)?;
        } else if headroom >= HEADROOM_US && self.applied < self.ceiling {
            self.propose(
                self.applied
                    .saturating_add(self.applied.div_ceil(16))
                    .min(self.ceiling),
                Reason::Headroom,
            )?;
        }
        Ok(())
    }

    /// Carry this command only on the existing single capture credit. Always
    /// send its explicit rate, including a same-rate cancellation command.
    /// No applied rate or cooldown changes merely because work was submitted.
    pub fn begin_capture(&mut self) -> Result<Capture, Error> {
        self.check_open()?;
        if self.in_flight.is_some() {
            return Err(Error::CaptureInFlight);
        }
        let sequence = self.next_capture;
        self.next_capture = self
            .next_capture
            .checked_add(1)
            .ok_or_else(|| self.exhausted())?;
        let capture = Capture {
            sequence,
            bitrate: self.pending.map_or(self.applied, Change::bitrate),
            previous: self.applied,
        };
        self.native_target = capture.bitrate;
        self.in_flight = Some(capture);
        Ok(capture)
    }

    /// Confirm only after the original worker reply and output have been fully
    /// validated. An actual change requires an IDR. Unchanged/old-rate replies
    /// retain the same pending decision and do not restart its dwell.
    ///
    /// Call before egress admission: a later expired send cannot undo a native
    /// encoder change. The IPC owner, not this policy token, certifies the source.
    pub fn complete_capture(
        &mut self,
        capture: Capture,
        actual_bitrate: u32,
        is_idr: bool,
        now_us: u64,
    ) -> Result<(), Error> {
        self.check_open()?;
        if self.in_flight != Some(capture) || self.applied != capture.previous {
            return Err(Error::UnexpectedCapture);
        }
        if actual_bitrate != capture.previous && actual_bitrate != capture.bitrate {
            return Err(Error::UnexpectedBitrate);
        }
        if actual_bitrate != self.applied && !is_idr {
            return Err(Error::MissingIdr);
        }
        self.clock(now_us)?;
        self.in_flight = None;
        if actual_bitrate != self.applied {
            self.applied = actual_bitrate;
            self.changed_at = Some(now_us);
            self.clear_measurements();
        }
        if self.pending.is_some_and(|change| {
            change.bitrate == actual_bitrate && capture.bitrate == actual_bitrate
        }) {
            self.pending = None;
        }
        Ok(())
    }

    /// A new view/member scope cannot inherit another cohort's pressure or
    /// headroom. Unsent proposals disappear. A submitted/deferred target needs
    /// an explicit current-rate command; it must not silently apply later.
    ///
    /// An in-flight capture retains its original token. If that old request
    /// actually changes the encoder, its completion is recorded honestly and
    /// the pending restoration remains for the next original capture credit.
    pub fn reset_evidence(&mut self) -> Result<(), Error> {
        self.check_open()?;
        self.clear_measurements();
        if self
            .pending
            .is_some_and(|change| change.reason == Reason::EvidenceReset)
        {
            // An old request may already have changed the encoder after reset.
            // Preserve the original restoration intent through repeated resets.
            return Ok(());
        }
        if self.native_target == self.applied {
            self.pending = None;
        } else {
            self.propose(self.applied, Reason::EvidenceReset)?;
        }
        Ok(())
    }

    pub fn close(&mut self) {
        self.closed = true;
        self.pending = None;
        self.clear_measurements();
    }
    fn check_open(&self) -> Result<(), Error> {
        if self.closed {
            Err(Error::Closed)
        } else {
            Ok(())
        }
    }
    fn clock(&mut self, now: u64) -> Result<(), Error> {
        self.check_open()?;
        if self.last_now.is_some_and(|last| now < last) {
            self.close();
            return Err(Error::ClockRegression);
        }
        self.last_now = Some(now);
        Ok(())
    }
    fn clear_measurements(&mut self) {
        self.pressure_since = None;
        self.headroom.clear();
    }
    fn propose(&mut self, bitrate: u32, reason: Reason) -> Result<(), Error> {
        let id = self.next_change;
        self.next_change = self
            .next_change
            .checked_add(1)
            .ok_or_else(|| self.exhausted())?;
        self.pending = Some(Change {
            id,
            bitrate,
            reason,
        });
        Ok(())
    }
    fn exhausted(&mut self) -> Error {
        self.close();
        Error::SequenceExhausted
    }
}

fn headroom_negative(report: &Report) -> bool {
    report.mode != Mode::Active
        || report.sample.send == Availability::Blocked
        || report.sample.capture_credit == Availability::Blocked
        || report
            .sample
            .source_work_us
            .is_some_and(|work| work > report.interval_us.saturating_mul(3) / 4)
        || matches!(
            report.receiver,
            ReceiverEvidence::Measured(load)
                if load.retained_pictures > 1
                    || load.work_us.is_some_and(|work| work >= report.interval_us)
        )
}
