//! Bounded encoder bitrate decisions from actual send-admission evidence and,
//! on a viewer's path, from the queue that path holds.
//!
//! This policy is not a bandwidth estimator. It keeps the original configured
//! rate as a ceiling, lowers only after sustained sender pressure or a held path
//! queue, and probes slowly after measured headroom. A proposal is not an applied codec change.
//! One capture token joins every request to its actual native result; a deferred
//! request retains its identity without renewing a deadline or rate allowance.

use crate::{
    pacing::{Availability, Mode, ReceiverEvidence, Report},
    quality::WorkstationProfile,
};

const PRESSURE_US: u64 = 100_000;
/// Text lets cadence back off first, then still cuts a persistently blocked
/// sender. Independently measured path queues retain their earlier hard cut.
const TEXT_SEND_DWELL_US: u64 = 250_000;
const CHANGE_DWELL_US: u64 = 2_000_000;
const HEADROOM_US: u64 = 4_000_000;
const EVIDENCE_GAP_US: u64 = 100_000;
/// A viewer's path holding a queue this long lowers the rate (fr-7al9). The
/// capture gate alone carries a link of about half the encoder's rate; below
/// that only a lower rate keeps the session.
const PATH_PRESSURE_US: u64 = 200_000;
/// A clear path probes a quarter up after this long without a queue.
const PATH_HEADROOM_US: u64 = 4_000_000;
/// Shared publishers may sample at their 200 ms adaptive capture cadence.
/// Permit that bounded interval without inheriting arbitrarily old evidence.
const PATH_EVIDENCE_GAP_US: u64 = 250_000;

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
    /// The viewer's path held a queue (`observe_path`).
    PathQueue,
    Headroom,
    /// Cancel a retained native target after its supporting evidence was invalidated.
    EvidenceReset,
}

/// Content-free evidence from one original path, or a shared encoder's whole
/// current viewer cohort. A drained/application-limited path is not measured
/// capacity: only recent acknowledged busy traffic can establish `Clear`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathEvidence {
    Unknown,
    Idle,
    Clear,
    Queued { delivered_bits_per_second: u64 },
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
    installed: u32,
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
    fn expired(&self, now: u64) -> bool {
        self.last_positive
            .is_some_and(|last| now - last > EVIDENCE_GAP_US)
    }
    fn observe(&mut self, now: u64, positive: bool, negative: bool) -> u64 {
        if negative || self.expired(now) {
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
    profile: WorkstationProfile,
    ceiling: u32,
    floor: u32,
    applied: u32,
    /// Native readback can precede the first qualified output at this rate.
    installed: u32,
    needs_idr: bool,
    pending: Option<Change>,
    in_flight: Option<Capture>,
    native_target: u32,
    next_change: u64,
    next_capture: u64,
    last_now: Option<u64>,
    last_sample: Option<u64>,
    /// Latest original capture-pacing report vetoes optimistic transport probes.
    local_probe_blocked: bool,
    changed_at: Option<u64>,
    pressure_since: Option<u64>,
    headroom: Headroom,
    /// Since when the viewer's path has held a queue, or been clear of one.
    path_queued_since: Option<u64>,
    path_clear_since: Option<u64>,
    last_path_sample: Option<u64>,
    path_sample_lifetime_us: Option<u64>,
    path_evidence: Option<PathEvidence>,
    closed: bool,
}
impl Controller {
    pub fn new(initial_bitrate: u32) -> Result<Self, Error> {
        Self::new_with_profile(initial_bitrate, WorkstationProfile::Auto)
    }
    /// Choose the local workstation preference for this original encoder.
    /// Text gives cadence one bounded opportunity before a local sender cut.
    /// Measured path queues, native transactions, rate bounds and all evidence
    /// expiry rules are independent of this preference.
    pub fn new_with_profile(
        initial_bitrate: u32,
        profile: WorkstationProfile,
    ) -> Result<Self, Error> {
        if !(10_000..=200_000_000).contains(&initial_bitrate) {
            return Err(Error::InvalidBitrate);
        }
        Ok(Self {
            profile,
            ceiling: initial_bitrate,
            floor: (initial_bitrate / 8).max(100_000).min(initial_bitrate),
            applied: initial_bitrate,
            installed: initial_bitrate,
            needs_idr: false,
            pending: None,
            in_flight: None,
            native_target: initial_bitrate,
            next_change: 1,
            next_capture: 1,
            last_now: None,
            last_sample: None,
            local_probe_blocked: false,
            changed_at: None,
            pressure_since: None,
            headroom: Headroom::default(),
            path_queued_since: None,
            path_clear_since: None,
            last_path_sample: None,
            path_sample_lifetime_us: None,
            path_evidence: None,
            closed: false,
        })
    }
    pub const fn profile(&self) -> WorkstationProfile {
        self.profile
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
        let local_gap = self.last_sample.map_or(0, |last| now - last);
        self.last_sample = Some(now);
        let evidence_gap =
            local_gap > EVIDENCE_GAP_US || report.reason == crate::pacing::Reason::EvidenceGap;
        // Once a path observer is installed, local enqueue/decoder headroom
        // cannot override its absent or congested capacity evidence. This also
        // holds when a native service polls local pacing after the path sample.
        let path_negative = self.path_evidence.is_some()
            && (self.path_evidence != Some(PathEvidence::Clear)
                || self
                    .last_path_sample
                    .zip(self.path_sample_lifetime_us)
                    .is_none_or(|(last, lifetime)| now - last > lifetime));
        self.local_probe_blocked = headroom_negative(report);
        if self.local_probe_blocked || local_gap > PATH_EVIDENCE_GAP_US {
            self.path_clear_since = None;
        }
        let negative = self.local_probe_blocked || path_negative;
        if self
            .pending
            .is_some_and(|change| change.reason == Reason::Headroom)
            && (evidence_gap || negative || self.headroom.expired(now))
        {
            // A deferred optimistic target cannot block a necessary reduction
            // or revive on first motion after its headroom disappeared. Preserve
            // any in-flight effect, and explicitly cancel a native-held target
            // using the existing restoration path and unchanged change dwell.
            self.reset_evidence()?;
        }
        if evidence_gap {
            self.clear_local_measurements();
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
            report.mode == Mode::Active && report.headroom_us > 0 && !path_negative,
            negative,
        );
        if self.pending.is_some() || self.changed_at.is_some_and(|at| now - at < CHANGE_DWELL_US) {
            return Ok(());
        }
        let pressure_dwell = if self.profile == WorkstationProfile::Text {
            TEXT_SEND_DWELL_US
        } else {
            PRESSURE_US
        };
        if blocked && pressure >= pressure_dwell && self.applied > self.floor {
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

    /// One sample of the viewer's path: whether it holds a queue, and the bits
    /// per second it delivered while busy (0 when unmeasured). A queue held for
    /// [`PATH_PRESSURE_US`] lowers the rate to three fifths of the current one,
    /// or of that delivery when lower: a rate far above its path reaches it in
    /// one change. The delivery counts every acknowledged packet as full, so
    /// it reads high (about 1.5 times on the namespace link) and never limits a
    /// cut. A path clear of any queue for [`PATH_HEADROOM_US`] probes a
    /// quarter up. Never past the original ceiling or the floor, and never
    /// within the change dwell.
    ///
    /// These boolean samples must be at most [`EVIDENCE_GAP_US`] apart. Native
    /// callers with independently verified activity use [`Self::observe_path_evidence`],
    /// whose bounded lifetime also supports the shared publisher's 200 ms cadence.
    pub fn observe_path(
        &mut self,
        now_us: u64,
        queued: bool,
        delivered_bits_per_second: u64,
    ) -> Result<(), Error> {
        self.observe_path_with_lifetime(
            now_us,
            if queued {
                PathEvidence::Queued {
                    delivered_bits_per_second,
                }
            } else {
                PathEvidence::Clear
            },
            EVIDENCE_GAP_US,
        )
    }

    /// Observe the path without treating an empty queue as spare bandwidth.
    /// `Clear` must come from recent acknowledged busy traffic, not silence,
    /// a repeated old RTT or an unavailable transport sample. Gaps in service
    /// cannot supply pressure/headroom for the time that was not observed.
    /// Native path owners independently expire unavailable/stale samples; this
    /// consumer permits at most [`PATH_EVIDENCE_GAP_US`] between observations so
    /// a shared publisher's 200 ms capture cadence can retain measured evidence.
    pub fn observe_path_evidence(
        &mut self,
        now_us: u64,
        evidence: PathEvidence,
    ) -> Result<(), Error> {
        self.observe_path_with_lifetime(now_us, evidence, PATH_EVIDENCE_GAP_US)
    }

    fn observe_path_with_lifetime(
        &mut self,
        now_us: u64,
        evidence: PathEvidence,
        lifetime_us: u64,
    ) -> Result<(), Error> {
        self.clock(now_us)?;
        let gap = self
            .last_path_sample
            .zip(self.path_sample_lifetime_us)
            .is_some_and(|(last, previous_lifetime)| {
                now_us - last > lifetime_us.min(previous_lifetime)
            });
        // ACKs can prove transport headroom while the encoder or decoder is
        // overloaded or capture has gone idle. Only veto optimistic samples:
        // a measured queue still reduces the rate. Path-only controllers have
        // no pacing owner and continue to follow their own qualified samples.
        let local_veto = self
            .last_sample
            .is_some_and(|at| self.local_probe_blocked || now_us - at > PATH_EVIDENCE_GAP_US);
        let evidence = if local_veto && evidence == PathEvidence::Clear {
            PathEvidence::Unknown
        } else {
            evidence
        };
        let queued = matches!(evidence, PathEvidence::Queued { .. });
        let measured = matches!(evidence, PathEvidence::Clear | PathEvidence::Queued { .. });
        if (gap || !measured || queued)
            && self
                .pending
                .is_some_and(|change| change.reason == Reason::Headroom)
        {
            // Preserve an in-flight/deferred native effect, but explicitly
            // withdraw its optimistic target on the next capture credit.
            self.reset_evidence()?;
        }
        if gap || !measured {
            self.clear_path_measurements();
        }
        self.last_path_sample = Some(now_us);
        self.path_sample_lifetime_us = Some(lifetime_us);
        self.path_evidence = Some(evidence);
        if !measured {
            // A previously admitted cut survives a drained queue: capture
            // waits for that drain before it can apply the lower rate.
            return Ok(());
        }
        let delivered_bits_per_second = match evidence {
            PathEvidence::Queued {
                delivered_bits_per_second,
            } => delivered_bits_per_second,
            _ => 0,
        };
        let (held, clear) = if queued {
            self.path_clear_since = None;
            (now_us - *self.path_queued_since.get_or_insert(now_us), 0)
        } else {
            self.path_queued_since = None;
            (0, now_us - *self.path_clear_since.get_or_insert(now_us))
        };
        if self.pending.is_some()
            || self
                .changed_at
                .is_some_and(|at| now_us - at < CHANGE_DWELL_US)
        {
            return Ok(());
        }
        if held >= PATH_PRESSURE_US && self.applied > self.floor {
            let delivered = u32::try_from(delivered_bits_per_second / 5 * 3).unwrap_or(u32::MAX);
            let mut target = self.applied / 5 * 3;
            if delivered_bits_per_second > 0 {
                target = target.min(delivered);
            }
            let target = target.max(self.floor);
            if target < self.applied {
                self.propose(target, Reason::PathQueue)?;
            }
        } else if clear >= PATH_HEADROOM_US && self.applied < self.ceiling {
            self.propose(
                self.applied
                    .saturating_add(self.applied.div_ceil(4))
                    .min(self.ceiling),
                Reason::Headroom,
            )?;
            // The next probe needs its own clear interval after this one.
            self.path_clear_since = Some(now_us);
        }
        Ok(())
    }

    /// One sample of several viewers' paths where some hold a queue and some do
    /// not, or none can be judged. A slow viewer never lowers a healthy viewer's
    /// rate, and a probe could only deepen the queue: neither a cut nor a probe,
    /// and both [`Self::observe_path`] intervals start over.
    pub fn observe_split_path(&mut self, now_us: u64) -> Result<(), Error> {
        self.clock(now_us)?;
        self.clear_path_measurements();
        self.path_evidence = Some(PathEvidence::Unknown);
        if self
            .pending
            .is_some_and(|change| matches!(change.reason, Reason::Headroom | Reason::PathQueue))
        {
            // A slow viewer cannot apply a cohort-wide cut after another
            // viewer has become healthy. Restore a native-retained target
            // explicitly; already completed effects remain recorded.
            self.reset_evidence()?;
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
            installed: self.installed,
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
        if self.in_flight != Some(capture)
            || self.applied != capture.previous
            || self.installed != capture.installed
        {
            return Err(Error::UnexpectedCapture);
        }
        if actual_bitrate != capture.installed && actual_bitrate != capture.bitrate {
            return Err(Error::UnexpectedBitrate);
        }
        if (actual_bitrate != self.applied || self.needs_idr) && !is_idr {
            return Err(Error::MissingIdr);
        }
        self.clock(now_us)?;
        self.in_flight = None;
        if actual_bitrate != self.applied || self.needs_idr {
            self.applied = actual_bitrate;
            self.changed_at = Some(now_us);
            self.clear_measurements();
        }
        self.installed = actual_bitrate;
        self.needs_idr = false;
        if self.pending.is_some_and(|change| {
            change.bitrate == actual_bitrate && capture.bitrate == actual_bitrate
        }) {
            self.pending = None;
        }
        Ok(())
    }

    /// Retire one completed native attempt that was rate-limited without
    /// producing a picture. The worker must certify the original request and
    /// completed codec callback; a timeout or pending callback is not a skip.
    ///
    /// The installed rate may already equal the requested target, but no
    /// output means no validated IDR. Keep the last output's applied rate,
    /// change dwell and pending decision until a real picture confirms them.
    /// The native target also remains retained: a scope reset must explicitly
    /// restore it even when the worker deferred installation before dropping.
    /// This completion carries no source-observation or presentation evidence.
    pub fn skip_capture(
        &mut self,
        capture: Capture,
        installed_bitrate: u32,
        now_us: u64,
    ) -> Result<(), Error> {
        self.check_open()?;
        if self.in_flight != Some(capture)
            || self.applied != capture.previous
            || self.installed != capture.installed
        {
            return Err(Error::UnexpectedCapture);
        }
        if installed_bitrate != capture.installed && installed_bitrate != capture.bitrate {
            return Err(Error::UnexpectedBitrate);
        }
        self.clock(now_us)?;
        self.in_flight = None;
        self.needs_idr |= installed_bitrate != self.installed;
        self.installed = installed_bitrate;
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
        self.cancel_native_intent()
    }

    /// A decoder/view replacement on the SAME original transport cohort
    /// invalidates local sender/receiver evidence and every optimistic probe.
    /// It does not erase a measured path queue or its conservative pending cut.
    /// A changed transport/member cohort must use `reset_evidence` instead.
    /// Original capture tokens and already installed native effects are retained.
    pub fn reset_receiver_evidence(&mut self) -> Result<(), Error> {
        self.check_open()?;
        self.clear_local_measurements();
        self.path_clear_since = None;
        if self
            .pending
            .is_some_and(|change| change.reason == Reason::PathQueue)
        {
            return Ok(());
        }
        self.cancel_native_intent()
    }

    fn cancel_native_intent(&mut self) -> Result<(), Error> {
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
        self.clear_local_measurements();
        self.clear_path_measurements();
    }
    fn clear_local_measurements(&mut self) {
        self.pressure_since = None;
        self.headroom.clear();
    }
    fn clear_path_measurements(&mut self) {
        self.path_queued_since = None;
        self.path_clear_since = None;
        self.last_path_sample = None;
        self.path_sample_lifetime_us = None;
        if self.path_evidence.is_some() {
            self.path_evidence = Some(PathEvidence::Unknown);
        }
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
