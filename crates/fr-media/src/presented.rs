//! Bounded source-provenance verification and one pending presentation report.
//! No authority is created here. The runtime authenticates the binding/route,
//! records ONLY its real capture owner, and applies deadlines to native authority.
use fr_core::limits::ProtocolLimits;
use fr_wire::{
    PipelineState, Progress, SourceObservation, WireError,
    decoder::Binding,
    input::{InputDelivery, InputDirection},
    presented::{self as wire, Report, Sample, Stamp},
};
use std::{num::NonZeroU64, sync::Arc};
pub const HISTORY: usize = 64;
pub const REPORT_INTERVAL_US: u64 = 50_000;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Wire(WireError),
    Clock,
    Overflow,
    UnknownSource,
    Replay,
    Expired,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Ready { until_us: u64 },
    Unavailable,
    Obsolete,
}

/// Snapshot of one original reporter's admitted sequence. A zero sequence
/// means that this reporter has not handed any report to its transport yet.
/// The private identity survives observation-to-control promotion; another
/// reporter with identical numeric bindings cannot substitute for this owner.
#[derive(Debug, Clone)]
pub struct AdmittedCursor {
    binding: Binding,
    sequence: u64,
    visible: Option<Stamp>,
    lineage: Arc<()>,
}
impl AdmittedCursor {
    pub const fn binding(&self) -> Binding {
        self.binding
    }
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }
    pub const fn visible(&self) -> Option<Stamp> {
        self.visible
    }
    /// Validate another snapshot of this same original reporter, independent
    /// of when its admitted prefix was sampled.
    pub fn is_from(&self, cursor: &Self) -> bool {
        Arc::ptr_eq(&self.lineage, &cursor.lineage) && self.binding == cursor.binding
    }
}
impl PartialEq for AdmittedCursor {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.lineage, &other.lineage)
            && self.binding == other.binding
            && self.sequence == other.sequence
            && self.visible == other.visible
    }
}
impl Eq for AdmittedCursor {}

/// Receipt from the reporter's actual transport-admission hook. This is local
/// queue admission, not evidence that the host applied the report or that the
/// view remains fresh. Receipts have nonzero sequences and cannot be forged
/// from a binding, a decoded wire report, or a replacement reporter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedReport(AdmittedCursor);
impl QueuedReport {
    pub const fn binding(&self) -> Binding {
        self.0.binding
    }
    pub const fn sequence(&self) -> u64 {
        self.0.sequence
    }
    pub const fn visible(&self) -> Option<Stamp> {
        self.0.visible
    }
    /// Compare original ownership, including the complete recovery binding.
    /// The caller still checks sequence order and its own admitted ceiling.
    pub fn is_from(&self, cursor: &AdmittedCursor) -> bool {
        self.0.is_from(cursor)
    }
}
fn tick(last: &mut u64, now: u64) -> Result<(), Error> {
    if now < *last {
        return Err(Error::Clock);
    }
    *last = now;
    Ok(())
}
/// Fixed metadata, never another encoded-frame or surface-retention queue.
pub struct Verifier {
    binding: Binding,
    limits: ProtocolLimits,
    history: [Option<Stamp>; HISTORY],
    cursor: usize,
    last_source: Option<Stamp>,
    last_now: u64,
    floor: u64,
    confirmed_until: Option<u64>,
}
impl Verifier {
    pub fn new(binding: Binding, limits: ProtocolLimits, now: u64) -> Result<Self, Error> {
        binding.validate().map_err(Error::Wire)?;
        Ok(Self {
            binding,
            limits,
            history: [None; HISTORY],
            cursor: 0,
            last_source: None,
            last_now: now,
            floor: 0,
            confirmed_until: None,
        })
    }
    /// Call from the actual host packetizer's last admitted progress, never
    /// with a peer-provided stamp. Same-source polls do not prolong its history.
    pub fn observe(&mut self, progress: Progress, now: u64) -> Result<(), Error> {
        tick(&mut self.last_now, now)?;
        if !matches!(
            progress.pipeline,
            PipelineState::Running | PipelineState::Idle
        ) || progress.observation == SourceObservation::Unknown
        {
            self.history.fill(None);
            self.last_source = None;
            return Ok(());
        }
        let stamp = Stamp {
            frame: progress.descriptor.frame,
            captured_us: progress.descriptor.capture_micros,
            observed_us: progress.observed_micros,
            source: progress.observation,
        };
        Sample {
            stamp,
            age_upper_us: 0,
        }
        .validate()
        .map_err(Error::Wire)?;
        if stamp.observed_us > now {
            return Err(Error::Clock);
        }
        if self.last_source == Some(stamp) {
            return Ok(());
        }
        if self
            .last_source
            .is_some_and(|p| stamp.frame < p.frame || stamp.observed_us < p.observed_us)
        {
            return Err(Error::Clock);
        }
        self.history[self.cursor] = Some(stamp);
        self.cursor = (self.cursor + 1) % HISTORY;
        self.last_source = Some(stamp);
        Ok(())
    }
    /// `bound_us` is the host's source-age bound for this path
    /// (`wire::source_age_bound_us` with `HOST_RTT_MULTIPLE`). A report whose
    /// deadline would not extend the confirmed one (a shrunken bound included)
    /// is obsolete: it never shortens an existing view deadline.
    pub fn receive(&mut self, bytes: &[u8], now: u64, bound_us: u64) -> Result<Decision, Error> {
        self.receive_report(bytes, now, bound_us)
            .map(|(_, decision)| decision)
    }
    /// Return the sequence from this exact verified report, never the replay
    /// floor. Only a `Ready` decision that the authority owner successfully
    /// applies can become a host acknowledgement of readiness.
    pub fn receive_report(
        &mut self,
        bytes: &[u8],
        now: u64,
        bound_us: u64,
    ) -> Result<(NonZeroU64, Decision), Error> {
        tick(&mut self.last_now, now)?;
        if !(1..=wire::MAX_SOURCE_AGE_US).contains(&bound_us) {
            return Err(Error::Overflow);
        }
        let report = wire::decode(
            bytes,
            self.binding,
            &self.limits,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )
        .map_err(Error::Wire)?;
        if report.sequence <= self.floor {
            return Err(Error::Replay);
        }
        let sequence = NonZeroU64::new(report.sequence).ok_or(Error::Replay)?;
        self.floor = report.sequence;
        let Some(sample) = report.visible else {
            return Ok((sequence, Decision::Unavailable));
        };
        let until_us = sample
            .stamp
            .observed_us
            .checked_add(bound_us)
            .ok_or(Error::Overflow)?;
        if self.confirmed_until.is_some_and(|old| until_us <= old) {
            return Ok((sequence, Decision::Obsolete));
        }
        if now >= until_us {
            return Ok((sequence, Decision::Unavailable));
        }
        if !self.history.contains(&Some(sample.stamp)) {
            return Err(Error::UnknownSource);
        }
        self.confirmed_until = Some(until_us);
        Ok((sequence, Decision::Ready { until_us }))
    }
}
struct Pending {
    stamp: Option<Stamp>,
    bytes: [u8; wire::BYTES],
    until: u64,
}
/// Latest-source metadata is replaceable BEFORE enqueue, unlike input actions.
/// Unchanged source identity never generates heartbeat-like readiness extensions.
pub struct Reporter {
    binding: Binding,
    lineage: Arc<()>,
    limits: ProtocolLimits,
    sequence: u64,
    last_now: u64,
    after: u64,
    sent: Option<Stamp>,
    /// One expired, never-admitted source. Dropping its bytes must not let the
    /// same source acquire a new send deadline or count as reported evidence.
    discarded: Option<Stamp>,
    unavailable_sent: bool,
    pending: Option<Pending>,
}
impl Reporter {
    pub fn new(binding: Binding, limits: ProtocolLimits, now: u64) -> Result<Self, Error> {
        binding.validate().map_err(Error::Wire)?;
        Ok(Self {
            binding,
            lineage: Arc::new(()),
            limits,
            sequence: 1,
            last_now: now,
            after: now,
            sent: None,
            discarded: None,
            unavailable_sent: false,
            pending: None,
        })
    }
    pub fn has_reported(&self) -> bool {
        self.sent.is_some()
    }
    /// The last admitted record, not a prepared candidate or an intended send.
    pub fn admitted_cursor(&self) -> AdmittedCursor {
        AdmittedCursor {
            binding: self.binding,
            sequence: self.sequence - 1,
            visible: self.sent.filter(|_| !self.unavailable_sent),
            lineage: self.lineage.clone(),
        }
    }
    /// Reserve a mandatory negative report for a newly observed local lapse.
    /// Its sequence is a barrier, NOT an admission receipt. Even a reporter
    /// which has never sent a positive report must establish this barrier.
    /// Repeated calls preserve already-pending negative bytes and send-by;
    /// an expired negative stays terminal and can never be given a new budget.
    pub fn force_unavailable(&mut self, now: u64) -> Result<u64, Error> {
        self.discard_expired_positive(now)?;
        if self.pending.as_ref().is_some_and(|p| p.stamp.is_none()) {
            return Ok(self.sequence);
        }
        let until = now.checked_add(REPORT_INTERVAL_US).ok_or(Error::Overflow)?;
        let mut bytes = [0; wire::BYTES];
        wire::encode(
            Report {
                sequence: self.sequence,
                visible: None,
            },
            self.binding,
            &self.limits,
            &mut bytes,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )
        .map_err(Error::Wire)?;
        self.pending = Some(Pending {
            stamp: None,
            bytes,
            until,
        });
        Ok(self.sequence)
    }
    /// `bound_us` is the viewer's source-age bound for this path. A sample at
    /// or past it is not fresh evidence: it is reported like no visible view.
    pub fn prepare(
        &mut self,
        sample: Option<Sample>,
        now: u64,
        bound_us: u64,
    ) -> Result<(), Error> {
        tick(&mut self.last_now, now)?;
        if !(1..=wire::MAX_SOURCE_AGE_US).contains(&bound_us) {
            return Err(Error::Overflow);
        }
        let sample =
            sample.filter(|s| s.age_upper_us < bound_us && self.discarded != Some(s.stamp));
        // Once loss of visibility is observed, its negative report cannot be
        // replaced by a later positive sample before the host receives it.
        if self.pending.as_ref().is_some_and(|p| p.stamp.is_none()) {
            self.pending(now)?;
            return Ok(());
        }
        let Some(sample) = sample else {
            self.pending = None;
            if self.sent.is_some() && !self.unavailable_sent {
                self.force_unavailable(now)?;
            }
            return Ok(());
        };
        sample.validate().map_err(Error::Wire)?;
        if self
            .pending
            .as_ref()
            .is_some_and(|p| p.stamp != Some(sample.stamp))
        {
            self.pending = None;
        }
        if let Some(p) = &self.pending {
            if now >= p.until {
                return Err(Error::Expired);
            }
            return Ok(());
        }
        if self.sent == Some(sample.stamp) || now < self.after {
            return Ok(());
        }
        let until = now
            .checked_add(bound_us - sample.age_upper_us)
            .ok_or(Error::Overflow)?;
        let mut bytes = [0; wire::BYTES];
        wire::encode(
            Report {
                sequence: self.sequence,
                visible: Some(sample),
            },
            self.binding,
            &self.limits,
            &mut bytes,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )
        .map_err(Error::Wire)?;
        self.pending = Some(Pending {
            stamp: Some(sample.stamp),
            bytes,
            until,
        });
        Ok(())
    }
    /// Stop advertising an unconfirmed compositor candidate without erasing a
    /// pending explicit loss report or renewing the preceding visible source.
    pub fn pause(&mut self, now: u64) -> Result<(), Error> {
        tick(&mut self.last_now, now)?;
        if self.pending.as_ref().is_some_and(|p| p.stamp.is_some()) {
            self.pending = None;
        }
        self.pending(now)?;
        Ok(())
    }
    /// Discard only a positive report which expired BEFORE transport admitted
    /// it. Already-queued records are not owned here and cannot be retracted.
    /// Keep a bounded source tombstone: re-sampling the same source cannot
    /// refresh its expired send-by. This neither reports visibility nor consumes
    /// a sequence, and the host's previous readiness deadline is unchanged.
    /// An expired explicit loss report remains an error, with its original
    /// bytes/deadline retained; it cannot be erased or replaced by visibility.
    pub fn discard_expired_positive(&mut self, now: u64) -> Result<bool, Error> {
        tick(&mut self.last_now, now)?;
        let Some(pending) = self.pending.as_ref().filter(|p| now >= p.until) else {
            return Ok(false);
        };
        let stamp = pending.stamp.ok_or(Error::Expired)?;
        self.discarded = Some(stamp);
        self.pending = None;
        Ok(true)
    }
    /// For accounting only: negative reports are not positive age samples.
    pub fn pending_is_positive(&self) -> bool {
        self.pending.as_ref().is_some_and(|p| p.stamp.is_some())
    }
    pub fn pending(&mut self, now: u64) -> Result<Option<(&[u8], u64)>, Error> {
        tick(&mut self.last_now, now)?;
        if self.pending.as_ref().is_some_and(|p| now >= p.until) {
            return Err(Error::Expired);
        }
        Ok(self.pending.as_ref().map(|p| (p.bytes.as_slice(), p.until)))
    }
    pub fn queued(&mut self, now: u64) -> Result<(), Error> {
        self.queued_receipt(now).map(|_| ())
    }
    /// Call only after transport actually accepts these exact pending bytes.
    /// Backpressure, preparation, expiry and discarded candidates emit nothing.
    pub fn queued_receipt(&mut self, now: u64) -> Result<QueuedReport, Error> {
        self.pending(now)?.ok_or(Error::Expired)?;
        // Arithmetic failure cannot partially advance the admitted cursor or
        // let a later caller wrap/reuse the original report identity.
        let next = self.sequence.checked_add(1).ok_or(Error::Overflow)?;
        let after = now.checked_add(REPORT_INTERVAL_US).ok_or(Error::Overflow)?;
        let pending = self.pending.take().ok_or(Error::Expired)?;
        let receipt = QueuedReport(AdmittedCursor {
            binding: self.binding,
            sequence: self.sequence,
            visible: pending.stamp,
            lineage: self.lineage.clone(),
        });
        if let Some(stamp) = pending.stamp {
            self.sent = Some(stamp);
            self.unavailable_sent = false;
        } else {
            self.unavailable_sent = true;
        }
        self.sequence = next;
        self.after = after;
        Ok(receipt)
    }
}

#[cfg(test)]
mod overflow_tests;
