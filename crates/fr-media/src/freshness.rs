//! Source age and visible-view evidence, separate from reassembly queue age.
//!
//! Host and viewer clocks have unrelated origins. A session-authenticated clock
//! exchange brackets the offset without assuming symmetric delay. Platform code
//! must confirm visibility separately; decoding or compositor submission alone
//! never establishes a usable view. This module neither grants nor renews input.
use crate::delivery::{DecodedFrame, MediaBindings, MediaEpoch, ReceivePipeline};
use fr_core::ids::HostBootId;
use fr_wire::{
    Channel, FrameDescriptor, MediaLimits, PipelineState, Progress, Record, SourceObservation,
    decode_progress,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidClock,
    ClockExpired,
    ClockRegression,
    ClockOverflow,
    FutureObservation,
    StaleBinding,
    InvalidProgress,
    Obsolete,
    NotSubmitted,
    QueueExpired,
    SourceUnknown,
    SourceStale,
    Closed,
}

/// One correlated request/response: the host sampled its clock after the viewer
/// sent the request and before the viewer received the response. The transport
/// must authenticate and correlate that exchange; arbitrary peer timestamps are
/// not such evidence. Drift is a locally qualified *relative* clock-rate bound.
#[derive(Debug, Clone, Copy)]
pub struct ClockSample {
    pub host_boot: HostBootId,
    pub client_sent_us: u64,
    pub host_sample_us: u64,
    pub client_received_us: u64,
}
#[derive(Debug, Clone, Copy)]
pub struct ClockPolicy {
    pub max_exchange_us: u64,
    pub valid_for_us: u64,
    pub drift_ppm: u32,
}
impl Default for ClockPolicy {
    fn default() -> Self {
        Self {
            max_exchange_us: 1_000_000,
            valid_for_us: 10_000_000,
            drift_ppm: 1_000,
        }
    }
}
#[derive(Debug, Clone, Copy)]
pub struct ClockCorrelation {
    sample: ClockSample,
    policy: ClockPolicy,
    valid_until_us: u64,
}
impl ClockCorrelation {
    pub fn new(sample: ClockSample, policy: ClockPolicy) -> Result<Self, Error> {
        if sample.host_boot.as_raw() == 0
            || sample.client_received_us < sample.client_sent_us
            || !(1..=10_000_000).contains(&policy.max_exchange_us)
            || !(1..=60_000_000).contains(&policy.valid_for_us)
            || policy.drift_ppm > 10_000
            || sample.client_received_us - sample.client_sent_us > policy.max_exchange_us
        {
            return Err(Error::InvalidClock);
        }
        let valid_until_us = sample
            .client_received_us
            .checked_add(policy.valid_for_us)
            .ok_or(Error::ClockOverflow)?;
        Ok(Self {
            sample,
            policy,
            valid_until_us,
        })
    }
    pub const fn host_boot(&self) -> HostBootId {
        self.sample.host_boot
    }
    pub const fn received_at_us(&self) -> u64 {
        self.sample.client_received_us
    }
    pub const fn valid_until_us(&self) -> u64 {
        self.valid_until_us
    }

    /// Conservative age upper bound at viewer time `now_us`. The ENTIRE exchange
    /// latency is uncertainty, not RTT/2. A delayed response never resets age.
    /// Arithmetic uses u128 so unrelated u64 origins cannot wrap or saturate fresh.
    pub fn age_upper_us(&self, host_observed_us: u64, now_us: u64) -> Result<u64, Error> {
        if now_us < self.sample.client_received_us {
            return Err(Error::ClockRegression);
        }
        if now_us >= self.valid_until_us {
            return Err(Error::ClockExpired);
        }
        let elapsed = u128::from(now_us - self.sample.client_sent_us);
        let drift = (elapsed * u128::from(self.policy.drift_ppm)).div_ceil(1_000_000);
        let latest_host = u128::from(self.sample.host_sample_us) + elapsed + drift;
        if latest_host > u128::from(u64::MAX) {
            return Err(Error::ClockOverflow);
        }
        let age = latest_host
            .checked_sub(u128::from(host_observed_us))
            .ok_or(Error::FutureObservation)?;
        u64::try_from(age).map_err(|_| Error::ClockOverflow)
    }
}

/// Fixed metadata only. No pixels, compressed data, ticket, or lease is retained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewEvidence {
    pub serial: u64,
    pub frame: u64,
    pub epoch: MediaEpoch,
    pub observed_at_client_us: u64,
    pub pixel_age_upper_us: u64,
    pub source_age_upper_us: u64,
    pub source: SourceObservation,
}
#[derive(Debug, Clone, Copy)]
struct Candidate {
    descriptor: FrameDescriptor,
    display_until_us: u64,
}

/// One configured stream's decode -> submit -> visible path. On replacement,
/// create a new tracker with new bindings and invalidate the previous input
/// owner. Late callbacks cannot rebind this owner. At most one pending renderer
/// candidate and one visible descriptor are retained, regardless of packet rate.
#[derive(Debug)]
pub struct ViewTracker {
    scope: Arc<AtomicBool>,
    bindings: MediaBindings,
    epoch: MediaEpoch,
    clock: ClockCorrelation,
    max_source_age_us: u64,
    last_now_us: u64,
    last_decoded: Option<u64>,
    pending: Option<Candidate>,
    visible: Option<FrameDescriptor>,
    progress: Option<Progress>,
    serial: u64,
    closed: bool,
}
impl ViewTracker {
    pub fn new(
        receiver: &ReceivePipeline,
        clock: ClockCorrelation,
        max_source_age_us: u64,
        now_us: u64,
    ) -> Result<Self, Error> {
        if !(1..=1_500_000).contains(&max_source_age_us) {
            return Err(Error::InvalidProgress);
        }
        let (scope, config) = receiver.presentation_scope();
        if !scope.load(Ordering::Acquire) {
            return Err(Error::StaleBinding);
        }
        clock.age_upper_us(clock.sample.host_sample_us, now_us)?;
        Ok(Self {
            scope,
            bindings: config.bindings,
            epoch: config.epoch,
            clock,
            max_source_age_us,
            last_now_us: now_us,
            last_decoded: None,
            pending: None,
            visible: None,
            progress: None,
            serial: 0,
            closed: false,
        })
    }
    pub const fn epoch(&self) -> MediaEpoch {
        self.epoch
    }
    pub fn synchronize(&mut self, clock: ClockCorrelation, now_us: u64) -> Result<(), Error> {
        if clock.host_boot() != self.clock.host_boot() {
            return Err(Error::StaleBinding);
        }
        self.tick(now_us)?;
        if clock.received_at_us() <= self.clock.received_at_us() {
            return Err(Error::Obsolete);
        }
        clock.age_upper_us(clock.sample.host_sample_us, now_us)?;
        self.clock = clock;
        self.bump()?;
        Ok(())
    }
    /// Parse current-bound `MediaProgress`. A heartbeat is not accepted here.
    /// Duplicate or reordered source observations cannot refresh a visible frame.
    pub fn progress(
        &mut self,
        bytes: &[u8],
        limits: &MediaLimits,
        now_us: u64,
    ) -> Result<(), Error> {
        let record = Record::decode(
            bytes,
            limits,
            self.bindings.for_channel(Channel::MediaConfig),
            Channel::MediaConfig,
        )
        .map_err(|e| match e {
            fr_wire::WireError::InvalidBinding => Error::StaleBinding,
            _ => Error::InvalidProgress,
        })?;
        let p = decode_progress(record, limits).map_err(|_| Error::InvalidProgress)?;
        self.tick(now_us)?;
        if p.pipeline == PipelineState::Failed {
            return self.fail(Error::SourceUnknown);
        }
        if let Some(old) = self.progress {
            if p.descriptor.frame < old.descriptor.frame {
                return Err(Error::Obsolete);
            }
            if p.descriptor.frame == old.descriptor.frame {
                if p.descriptor != old.descriptor {
                    return self.fail(Error::InvalidProgress);
                }
                if p.observation != SourceObservation::Unknown
                    && old.observation != SourceObservation::Unknown
                    && p.observed_micros < old.observed_micros
                {
                    return Err(Error::Obsolete);
                }
                if p == old {
                    return Err(Error::Obsolete);
                }
            }
        }
        if p.observation != SourceObservation::Unknown {
            if p.observed_micros < p.descriptor.capture_micros
                || (p.observation == SourceObservation::Captured
                    && p.observed_micros != p.descriptor.capture_micros)
            {
                return self.fail(Error::InvalidProgress);
            }
            self.clock.age_upper_us(p.observed_micros, now_us)?;
        }
        if self
            .visible
            .is_some_and(|d| d.frame == p.descriptor.frame && d != p.descriptor)
            || self.pending.is_some_and(|c| {
                c.descriptor.frame == p.descriptor.frame && c.descriptor != p.descriptor
            })
        {
            return self.fail(Error::InvalidProgress);
        }
        self.progress = Some(p);
        self.bump()?;
        Ok(())
    }
    /// Consume a receiver-issued successful-decode token. `submitted` is true
    /// only after the real compositor API accepted this picture, not on IPC send.
    /// The token retains the original queue deadline through a slow decoder.
    pub fn decoded(
        &mut self,
        frame: DecodedFrame,
        submitted: bool,
        now_us: u64,
    ) -> Result<(), Error> {
        if !frame.belongs_to(&self.scope) {
            return Err(Error::StaleBinding);
        }
        let (descriptor, epoch, bindings, display_until_us) = frame.into_parts();
        if epoch != self.epoch || bindings != self.bindings {
            return Err(Error::StaleBinding);
        }
        self.tick(now_us)?;
        if self.last_decoded.is_some_and(|id| descriptor.frame <= id) {
            return Err(Error::Obsolete);
        }
        if self
            .progress
            .is_some_and(|p| p.descriptor.frame == descriptor.frame && p.descriptor != descriptor)
        {
            return self.fail(Error::InvalidProgress);
        }
        self.last_decoded = Some(descriptor.frame);
        if submitted {
            self.visible = None;
        }
        self.pending = submitted.then_some(Candidate {
            descriptor,
            display_until_us,
        });
        Ok(())
    }
    /// A current, qualified platform visibility callback, NOT a decode callback.
    /// Older callbacks cannot replace a newer submitted frame. Occlusion or
    /// backgrounding must call `hide` and release input through its lifecycle path.
    pub fn visible(&mut self, frame: u64, now_us: u64) -> Result<ViewEvidence, Error> {
        self.tick(now_us)?;
        let candidate = self.pending.ok_or(Error::NotSubmitted)?;
        if candidate.descriptor.frame != frame {
            return Err(Error::Obsolete);
        }
        self.pending = None;
        self.visible = None;
        if now_us >= candidate.display_until_us {
            return Err(Error::QueueExpired);
        }
        self.visible = Some(candidate.descriptor);
        self.bump()?;
        self.evidence(now_us)
    }
    /// Poll before enabling/sending input and on idle. Only source evidence for
    /// the EXACT visible picture can extend freshness without new pixel data.
    /// Pixel age stays old during qualified unchanged-source observations.
    pub fn evidence(&mut self, now_us: u64) -> Result<ViewEvidence, Error> {
        self.tick(now_us)?;
        let shown = self.visible.ok_or(Error::NotSubmitted)?;
        let p = self.progress.ok_or(Error::SourceUnknown)?;
        if !matches!(p.pipeline, PipelineState::Running | PipelineState::Idle)
            || p.observation == SourceObservation::Unknown
        {
            return Err(Error::SourceUnknown);
        }
        // Progress for an unpresented newer picture cannot bless old pixels.
        let (observed, source) = if p.descriptor == shown {
            (p.observed_micros, p.observation)
        } else {
            (shown.capture_micros, SourceObservation::Captured)
        };
        let pixel_age_upper_us = self.clock.age_upper_us(shown.capture_micros, now_us)?;
        let source_age_upper_us = self.clock.age_upper_us(observed, now_us)?;
        if source_age_upper_us >= self.max_source_age_us {
            return Err(Error::SourceStale);
        }
        Ok(ViewEvidence {
            serial: self.serial,
            frame: shown.frame,
            epoch: self.epoch,
            observed_at_client_us: now_us,
            pixel_age_upper_us,
            source_age_upper_us,
            source,
        })
    }
    pub fn hide(&mut self) {
        self.pending = None;
        self.visible = None;
    }
    pub fn close(&mut self) {
        self.hide();
        self.progress = None;
        self.closed = true;
    }
    pub const fn is_closed(&self) -> bool {
        self.closed
    }
    fn tick(&mut self, now_us: u64) -> Result<(), Error> {
        if self.closed {
            return Err(Error::Closed);
        }
        if !self.scope.load(Ordering::Acquire) {
            return self.fail(Error::StaleBinding);
        }
        if now_us < self.last_now_us {
            return self.fail(Error::ClockRegression);
        }
        self.last_now_us = now_us;
        Ok(())
    }
    fn bump(&mut self) -> Result<(), Error> {
        let Some(next) = self.serial.checked_add(1) else {
            return self.fail(Error::ClockOverflow);
        };
        self.serial = next;
        Ok(())
    }
    fn fail<T>(&mut self, error: Error) -> Result<T, Error> {
        self.close();
        Err(error)
    }
}
