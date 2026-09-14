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
    pub fn receive(&mut self, bytes: &[u8], now: u64) -> Result<Decision, Error> {
        tick(&mut self.last_now, now)?;
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
        self.floor = report.sequence;
        let Some(sample) = report.visible else {
            return Ok(Decision::Unavailable);
        };
        let until_us = sample
            .stamp
            .observed_us
            .checked_add(wire::MAX_SOURCE_AGE_US)
            .ok_or(Error::Overflow)?;
        if self.confirmed_until.is_some_and(|old| until_us <= old) {
            return Ok(Decision::Obsolete);
        }
        if now >= until_us {
            return Ok(Decision::Unavailable);
        }
        if !self.history.contains(&Some(sample.stamp)) {
            return Err(Error::UnknownSource);
        }
        self.confirmed_until = Some(until_us);
        Ok(Decision::Ready { until_us })
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
    limits: ProtocolLimits,
    sequence: u64,
    last_now: u64,
    after: u64,
    sent: Option<Stamp>,
    unavailable_sent: bool,
    pending: Option<Pending>,
}
impl Reporter {
    pub fn new(binding: Binding, limits: ProtocolLimits, now: u64) -> Result<Self, Error> {
        binding.validate().map_err(Error::Wire)?;
        Ok(Self {
            binding,
            limits,
            sequence: 1,
            last_now: now,
            after: now,
            sent: None,
            unavailable_sent: false,
            pending: None,
        })
    }
    pub fn has_reported(&self) -> bool {
        self.sent.is_some()
    }
    pub fn prepare(&mut self, sample: Option<Sample>, now: u64) -> Result<(), Error> {
        tick(&mut self.last_now, now)?;
        // Once loss of visibility is observed, its negative report cannot be
        // replaced by a later positive sample before the host receives it.
        if self.pending.as_ref().is_some_and(|p| p.stamp.is_none()) {
            self.pending(now)?;
            return Ok(());
        }
        let Some(sample) = sample else {
            self.pending = None;
            if self.sent.is_some() && !self.unavailable_sent {
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
            .checked_add(wire::MAX_SOURCE_AGE_US - sample.age_upper_us)
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
    pub fn pending(&mut self, now: u64) -> Result<Option<(&[u8], u64)>, Error> {
        tick(&mut self.last_now, now)?;
        if self.pending.as_ref().is_some_and(|p| now >= p.until) {
            return Err(Error::Expired);
        }
        Ok(self.pending.as_ref().map(|p| (p.bytes.as_slice(), p.until)))
    }
    pub fn queued(&mut self, now: u64) -> Result<(), Error> {
        self.pending(now)?.ok_or(Error::Expired)?;
        let pending = self.pending.take().ok_or(Error::Expired)?;
        if let Some(stamp) = pending.stamp {
            self.sent = Some(stamp);
            self.unavailable_sent = false;
        } else {
            self.unavailable_sent = true;
        }
        self.sequence = self.sequence.checked_add(1).ok_or(Error::Overflow)?;
        self.after = now.checked_add(REPORT_INTERVAL_US).ok_or(Error::Overflow)?;
        Ok(())
    }
}
