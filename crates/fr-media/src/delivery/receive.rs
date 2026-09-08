//! Receiver-local deadlines never subtract unsynchronized host capture times.
use super::budget::TrackedBytes;
use super::{DeliveryError, MediaBindings, MediaBudget, MediaEpoch, deadline};
use core::fmt;
use fr_core::limits::ProtocolLimits;
use fr_wire::{
    Channel, Fragment, FrameDescriptor, MediaLimits, PipelineState, Progress, Record,
    RecoveryChunk, RepairRange, WireError, decode_fragment, decode_progress, decode_recovery,
    encode_repair,
};

const SLOTS: usize = ProtocolLimits::REASSEMBLY_WINDOW_CEILING as usize;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReceivePolicy {
    pub display_budget_micros: u64,
    pub reference_budget_micros: u64,
    pub recovery_budget_micros: u64,
    pub repair_delay_micros: u64,
    pub repair_interval_micros: u64,
    pub max_repair_attempts: u8,
}
impl Default for ReceivePolicy {
    fn default() -> Self {
        Self {
            display_budget_micros: 50_000,
            reference_budget_micros: 250_000,
            recovery_budget_micros: 2_000_000,
            repair_delay_micros: 20_000,
            repair_interval_micros: 60_000,
            max_repair_attempts: 3,
        }
    }
}
impl ReceivePolicy {
    fn validate(self) -> Result<(), DeliveryError> {
        if self.display_budget_micros == 0
            || self.display_budget_micros > self.reference_budget_micros
            || self.reference_budget_micros > 250_000
            || self.recovery_budget_micros == 0
            || self.recovery_budget_micros > 5_000_000
            || self.repair_delay_micros == 0
            || self.repair_delay_micros >= self.reference_budget_micros
            || self.repair_interval_micros == 0
            || self.repair_interval_micros > self.reference_budget_micros
            || !(1..=8).contains(&self.max_repair_attempts)
        {
            return Err(DeliveryError::InvalidPolicy);
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReceiveConfig {
    pub limits: MediaLimits,
    pub bindings: MediaBindings,
    pub epoch: MediaEpoch,
    pub policy: ReceivePolicy,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiveState {
    AwaitingConfiguration,
    AwaitingRecovery,
    DecodingRecovery,
    Streaming,
    NeedsRecovery,
    Closed,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiveUpdate {
    Accepted,
    Duplicate,
    Obsolete,
    PictureComplete,
}

/// Complete compressed picture with its memory reservation. Keep this value
/// alive while a decoder borrows its bytes. Taking it out of a queue does not
/// release receiver credit. It does NOT certify HEVC syntax or visible output.
pub struct ReceivedPicture {
    descriptor: FrameDescriptor,
    epoch: MediaEpoch,
    bindings: MediaBindings,
    queue_fresh: bool,
    bytes: TrackedBytes,
}
impl fmt::Debug for ReceivedPicture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReceivedPicture")
            .field("descriptor", &self.descriptor)
            .field("epoch", &self.epoch)
            .field("queue_fresh", &self.queue_fresh)
            .field("byte_len", &self.bytes.bytes.len())
            .finish_non_exhaustive()
    }
}
impl ReceivedPicture {
    pub const fn descriptor(&self) -> FrameDescriptor {
        self.descriptor
    }
    pub const fn epoch(&self) -> MediaEpoch {
        self.epoch
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes.bytes
    }
    /// Receiver queue age only. Presentation also needs source-freshness,
    /// clock-uncertainty, visibility and current authority checks.
    pub const fn within_display_queue_budget(&self) -> bool {
        self.queue_fresh
    }
}

struct Assembly {
    descriptor: FrameDescriptor,
    bytes: TrackedBytes,
    seen: Vec<u64>,
    received: u32,
    reliable_offset: Option<u32>,
    reliable_chunks: u32,
    display_until: u64,
    reference_until: u64,
    next_repair: u64,
    repair_attempts: u8,
}
impl Assembly {
    fn new(
        descriptor: FrameDescriptor,
        config: ReceiveConfig,
        budget: &MediaBudget,
        now: u64,
        reliable_until: Option<u64>,
    ) -> Result<Self, DeliveryError> {
        descriptor.validate(&config.limits)?;
        let count = descriptor.fragment_count()?;
        let words = if reliable_until.is_some() {
            0
        } else {
            usize::try_from(count.div_ceil(64)).map_err(|_| DeliveryError::ResourceLimit)?
        };
        let metadata = words
            .checked_mul(8)
            .and_then(|n| {
                n.checked_add(
                    core::mem::size_of::<Self>() + core::mem::size_of::<ReceivedPicture>(),
                )
            })
            .ok_or(DeliveryError::ResourceLimit)?;
        let mut bytes = budget.allocate(
            usize::try_from(descriptor.total_bytes).map_err(|_| DeliveryError::ResourceLimit)?,
            metadata,
        )?;
        let mut seen = Vec::new();
        seen.try_reserve_exact(words)
            .map_err(|_| DeliveryError::AllocationFailed)?;
        bytes.charge_extra(
            (seen.capacity() - words)
                .checked_mul(8)
                .ok_or(DeliveryError::ResourceLimit)?,
        )?;
        seen.resize(words, 0);
        Ok(Self {
            descriptor,
            bytes,
            seen,
            received: 0,
            reliable_offset: reliable_until.map(|_| 0),
            reliable_chunks: 0,
            display_until: deadline(now, config.policy.display_budget_micros)?,
            reference_until: match reliable_until {
                Some(until) => until,
                None => deadline(now, config.policy.reference_budget_micros)?,
            },
            next_repair: deadline(now, config.policy.repair_delay_micros)?,
            repair_attempts: 0,
        })
    }
    fn complete(&self) -> bool {
        self.received == self.descriptor.total_bytes
    }
    fn has_fragment(&self, index: u32) -> bool {
        self.seen[index as usize / 64] & (1_u64 << (index % 64)) != 0
    }
    fn fragment(&mut self, fragment: Fragment<'_>) -> Result<ReceiveUpdate, DeliveryError> {
        if self.reliable_offset.is_some() || fragment.descriptor != self.descriptor {
            return Err(DeliveryError::ConflictingPicture);
        }
        let range = self.descriptor.fragment_range(fragment.index)?;
        if self.has_fragment(fragment.index) {
            return if self.bytes.bytes[range] == *fragment.bytes {
                Ok(ReceiveUpdate::Duplicate)
            } else {
                Err(DeliveryError::ConflictingPicture)
            };
        }
        self.bytes.bytes[range].copy_from_slice(fragment.bytes);
        self.seen[fragment.index as usize / 64] |= 1_u64 << (fragment.index % 64);
        self.received +=
            u32::try_from(fragment.bytes.len()).map_err(|_| DeliveryError::ResourceLimit)?;
        Ok(if self.complete() {
            ReceiveUpdate::PictureComplete
        } else {
            ReceiveUpdate::Accepted
        })
    }
    fn recovery(
        &mut self,
        chunk: RecoveryChunk<'_>,
        max_chunks: u32,
    ) -> Result<ReceiveUpdate, DeliveryError> {
        if self.reliable_offset != Some(chunk.offset)
            || self.descriptor.frame != chunk.frame
            || self.descriptor.total_bytes != chunk.total_bytes
            || self.descriptor.capture_micros != chunk.capture_micros
            || self.reliable_chunks >= max_chunks
        {
            return Err(DeliveryError::NoncontiguousRecovery);
        }
        let start = usize::try_from(chunk.offset).map_err(|_| DeliveryError::ResourceLimit)?;
        let end = start
            .checked_add(chunk.bytes.len())
            .ok_or(DeliveryError::ResourceLimit)?;
        self.bytes
            .bytes
            .get_mut(start..end)
            .ok_or(DeliveryError::NoncontiguousRecovery)?
            .copy_from_slice(chunk.bytes);
        self.received = u32::try_from(end).map_err(|_| DeliveryError::ResourceLimit)?;
        self.reliable_offset = Some(self.received);
        self.reliable_chunks += 1;
        Ok(if self.complete() {
            ReceiveUpdate::PictureComplete
        } else {
            ReceiveUpdate::Accepted
        })
    }
    fn missing(&self, ranges: &mut [RepairRange]) -> Result<usize, DeliveryError> {
        let total = self.descriptor.fragment_count()?;
        let mut index = 0;
        let mut count = 0;
        while index < total && count < ranges.len() {
            if self.has_fragment(index) {
                index += 1;
                continue;
            }
            let start = index;
            while index < total && !self.has_fragment(index) {
                index += 1;
            }
            ranges[count] = RepairRange { start, end: index };
            count += 1;
        }
        Ok(count)
    }
}
#[derive(Clone, Copy)]
struct InFlight {
    frame: u64,
    deadline: u64,
}

/// One receiver subscription. Its fixed slot table, incomplete pictures and
/// outstanding decoder inputs are bounded; it cannot be cloned into two owners.
pub struct ReceivePipeline {
    config: ReceiveConfig,
    budget: MediaBudget,
    state: ReceiveState,
    failure: Option<DeliveryError>,
    slots: [Option<Assembly>; SLOTS],
    last_now: Option<u64>,
    recovery_until: Option<u64>,
    recovery_frame: Option<u64>,
    last_decoded: Option<u64>,
    in_flight: Option<InFlight>,
    progress: Option<Progress>,
}
impl fmt::Debug for ReceivePipeline {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReceivePipeline")
            .field("state", &self.state)
            .field("epoch", &self.config.epoch)
            .field("last_decoded", &self.last_decoded)
            .field("usage", &self.budget.usage())
            .finish_non_exhaustive()
    }
}
impl ReceivePipeline {
    pub fn new(config: ReceiveConfig, budget: MediaBudget) -> Result<Self, DeliveryError> {
        config.policy.validate()?;
        if !budget.fits(config.limits.protocol()) {
            return Err(DeliveryError::ResourceLimit);
        }
        Ok(Self {
            config,
            budget,
            state: ReceiveState::AwaitingConfiguration,
            failure: None,
            slots: core::array::from_fn(|_| None),
            last_now: None,
            recovery_until: None,
            recovery_frame: None,
            last_decoded: None,
            in_flight: None,
            progress: None,
        })
    }
    pub const fn state(&self) -> ReceiveState {
        self.state
    }
    pub const fn failure(&self) -> Option<DeliveryError> {
        self.failure
    }
    pub const fn latest_progress(&self) -> Option<Progress> {
        self.progress
    }
    pub fn budget_usage(&self) -> super::BudgetUsage {
        self.budget.usage()
    }
    /// Called only after the real decoder API is configured and its resources
    /// admitted. This does not wait for a frame or establish visible readiness.
    pub fn decoder_configured(&mut self, now: u64) -> Result<(), DeliveryError> {
        self.check_clock(now)?;
        if self.state != ReceiveState::AwaitingConfiguration {
            return Err(DeliveryError::WrongState);
        }
        self.recovery_until = Some(deadline(now, self.config.policy.recovery_budget_micros)?);
        self.state = ReceiveState::AwaitingRecovery;
        Ok(())
    }
    /// Receive exactly one authenticated, already-bound media record.
    pub fn receive(
        &mut self,
        channel: Channel,
        packet: &[u8],
        now: u64,
    ) -> Result<ReceiveUpdate, DeliveryError> {
        let record = match Record::decode(
            packet,
            &self.config.limits,
            self.config.bindings.for_channel(channel),
            channel,
        ) {
            Ok(record) => record,
            Err(WireError::InvalidBinding) => return Err(DeliveryError::StaleGeneration),
            Err(error) => return self.fail(error.into()),
        };
        self.tick(now)?;
        if !matches!(
            self.state,
            ReceiveState::AwaitingRecovery
                | ReceiveState::DecodingRecovery
                | ReceiveState::Streaming
        ) {
            return Err(DeliveryError::WrongState);
        }
        let result = match channel {
            Channel::Video => decode_fragment(record, &self.config.limits)
                .map_err(DeliveryError::from)
                .and_then(|fragment| self.on_fragment(fragment, now)),
            Channel::Recovery => decode_recovery(record, &self.config.limits)
                .map_err(DeliveryError::from)
                .and_then(|chunk| self.on_recovery(chunk, now)),
            Channel::MediaConfig => decode_progress(record, &self.config.limits)
                .map_err(DeliveryError::from)
                .and_then(|progress| self.on_progress(progress, now)),
            Channel::Control => Err(DeliveryError::Wire(WireError::WrongChannel)),
        };
        match result {
            Ok(update) => Ok(update),
            Err(DeliveryError::WrongState) => Err(DeliveryError::WrongState),
            Err(error) => self.fail(error),
        }
    }
    fn obsolete(&self, frame: u64) -> bool {
        self.last_decoded.is_some_and(|last| frame <= last)
            || self.in_flight.is_some_and(|pending| frame <= pending.frame)
    }
    fn slot(
        &mut self,
        descriptor: FrameDescriptor,
        now: u64,
        reliable: bool,
    ) -> Result<usize, DeliveryError> {
        if let Some(index) = self.slots.iter().position(|slot| {
            slot.as_ref()
                .is_some_and(|a| a.descriptor.frame == descriptor.frame)
        }) {
            let a = self.slots[index].as_ref().expect("located");
            if a.descriptor != descriptor || a.reliable_offset.is_some() != reliable {
                return Err(DeliveryError::ConflictingPicture);
            }
            return Ok(index);
        }
        let index = self
            .slots
            .iter()
            .position(Option::is_none)
            .ok_or(DeliveryError::ResourceLimit)?;
        let until = if reliable {
            Some(self.recovery_until.ok_or(DeliveryError::WrongState)?)
        } else {
            None
        };
        self.slots[index] = Some(Assembly::new(
            descriptor,
            self.config,
            &self.budget,
            now,
            until,
        )?);
        Ok(index)
    }
    fn on_fragment(
        &mut self,
        fragment: Fragment<'_>,
        now: u64,
    ) -> Result<ReceiveUpdate, DeliveryError> {
        if self.obsolete(fragment.descriptor.frame) {
            return Ok(ReceiveUpdate::Obsolete);
        }
        if self.state != ReceiveState::Streaming && self.recovery_frame.is_none() {
            return Err(DeliveryError::WrongState);
        }
        let index = self.slot(fragment.descriptor, now, false)?;
        self.slots[index]
            .as_mut()
            .expect("inserted")
            .fragment(fragment)
    }
    fn on_recovery(
        &mut self,
        chunk: RecoveryChunk<'_>,
        now: u64,
    ) -> Result<ReceiveUpdate, DeliveryError> {
        if self.state != ReceiveState::AwaitingRecovery {
            return Err(DeliveryError::WrongState);
        }
        if self
            .recovery_frame
            .is_some_and(|frame| frame != chunk.frame)
        {
            return Err(DeliveryError::ConflictingPicture);
        }
        let descriptor = FrameDescriptor {
            frame: chunk.frame,
            total_bytes: chunk.total_bytes,
            stride: self.config.limits.fragment_stride(),
            capture_micros: chunk.capture_micros,
            reference: None,
        };
        let index = self.slot(descriptor, now, true)?;
        self.recovery_frame = Some(chunk.frame);
        self.slots[index]
            .as_mut()
            .expect("inserted")
            .recovery(chunk, self.config.limits.max_fragments())
    }
    fn on_progress(
        &mut self,
        progress: Progress,
        now: u64,
    ) -> Result<ReceiveUpdate, DeliveryError> {
        if progress.pipeline == PipelineState::Failed {
            return Err(DeliveryError::DecodeFailed);
        }
        if self
            .progress
            .is_some_and(|old| progress.descriptor.frame < old.descriptor.frame)
        {
            return Ok(ReceiveUpdate::Obsolete);
        }
        if self.obsolete(progress.descriptor.frame) {
            self.progress = Some(progress);
            return Ok(ReceiveUpdate::Obsolete);
        }
        let reliable =
            self.state == ReceiveState::AwaitingRecovery && progress.descriptor.reference.is_none();
        if self.state != ReceiveState::Streaming && self.recovery_frame.is_none() && !reliable {
            return Err(DeliveryError::WrongState);
        }
        if reliable
            && self
                .recovery_frame
                .is_some_and(|frame| frame != progress.descriptor.frame)
        {
            return Err(DeliveryError::ConflictingPicture);
        }
        self.slot(progress.descriptor, now, reliable)?;
        if reliable {
            self.recovery_frame = Some(progress.descriptor.frame);
        }
        self.progress = Some(progress);
        Ok(ReceiveUpdate::Accepted)
    }
    /// Return a complete valid-dependency candidate. The HEVC validator and
    /// decoder must still run. Only one decode result may be outstanding here;
    /// no per-frame network round trip is involved.
    pub fn take_decodable(&mut self, now: u64) -> Result<Option<ReceivedPicture>, DeliveryError> {
        self.tick(now)?;
        if self.in_flight.is_some() {
            return Ok(None);
        }
        let selected = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| {
                let a = slot.as_ref()?;
                if !a.complete() {
                    return None;
                }
                let eligible = if self.state == ReceiveState::AwaitingRecovery {
                    a.reliable_offset.is_some() && self.recovery_frame == Some(a.descriptor.frame)
                } else if self.state == ReceiveState::Streaming {
                    a.descriptor.reference.is_none() || a.descriptor.reference == self.last_decoded
                } else {
                    false
                };
                eligible.then_some((index, a.descriptor.frame))
            })
            .min_by_key(|(_, frame)| *frame);
        let Some((index, _)) = selected else {
            return Ok(None);
        };
        let a = self.slots[index].take().expect("selected");
        if a.descriptor.reference.is_none() {
            for slot in &mut self.slots {
                if slot
                    .as_ref()
                    .is_some_and(|other| other.descriptor.frame < a.descriptor.frame)
                {
                    *slot = None;
                }
            }
        }
        if self.state == ReceiveState::AwaitingRecovery {
            self.state = ReceiveState::DecodingRecovery;
        }
        self.in_flight = Some(InFlight {
            frame: a.descriptor.frame,
            deadline: a.reference_until,
        });
        Ok(Some(ReceivedPicture {
            descriptor: a.descriptor,
            epoch: self.config.epoch,
            bindings: self.config.bindings,
            queue_fresh: now < a.display_until,
            bytes: a.bytes,
        }))
    }
    /// Called from an actual validation/decode completion, not packet receipt.
    /// A failure invalidates all dependent work. The picture keeps its budget
    /// until its owner drops it, even after successful acknowledgement.
    pub fn acknowledge_decode(
        &mut self,
        picture: &ReceivedPicture,
        success: bool,
        now: u64,
    ) -> Result<(), DeliveryError> {
        if picture.epoch != self.config.epoch
            || picture.bindings != self.config.bindings
            || !picture.bytes.belongs_to(&self.budget)
        {
            return Err(DeliveryError::StaleGeneration);
        }
        self.tick(now)?;
        if self
            .in_flight
            .is_none_or(|pending| pending.frame != picture.descriptor.frame)
        {
            return Err(DeliveryError::DecodeMismatch);
        }
        if !success {
            return self.fail(DeliveryError::DecodeFailed);
        }
        self.last_decoded = Some(picture.descriptor.frame);
        self.in_flight = None;
        self.recovery_until = None;
        self.state = ReceiveState::Streaming;
        Ok(())
    }
    /// Emit at most one bounded repair request into caller-owned storage.
    /// Repeated polls cannot extend frame lifetime or create unbounded requests.
    pub fn repair_request(
        &mut self,
        now: u64,
        out: &mut [u8],
    ) -> Result<Option<usize>, DeliveryError> {
        self.tick(now)?;
        let selected = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| {
                let a = slot.as_ref()?;
                (!a.complete()
                    && a.reliable_offset.is_none()
                    && now >= a.next_repair
                    && a.repair_attempts < self.config.policy.max_repair_attempts)
                    .then_some((index, a.descriptor.frame))
            })
            .min_by_key(|(_, frame)| *frame);
        let Some((index, _)) = selected else {
            return Ok(None);
        };
        let a = self.slots[index].as_mut().expect("selected");
        let mut ranges = [RepairRange { start: 0, end: 0 }; 64];
        let capacity = usize::from(self.config.limits.max_repair_ranges())
            .min((self.config.limits.record_bytes() - 36) / 8);
        let count = a.missing(&mut ranges[..capacity])?;
        let next = deadline(now, self.config.policy.repair_interval_micros)?;
        let n = encode_repair(
            a.descriptor.frame,
            &ranges[..count],
            a.descriptor.fragment_count()?,
            self.config.bindings.for_channel(Channel::Control),
            &self.config.limits,
            out,
        )?;
        a.next_repair = next;
        a.repair_attempts += 1;
        Ok(Some(n))
    }
    /// Call at `next_deadline` even when no more packets arrive (final-frame loss).
    pub fn tick(&mut self, now: u64) -> Result<(), DeliveryError> {
        self.check_clock(now)?;
        if matches!(
            self.state,
            ReceiveState::NeedsRecovery | ReceiveState::Closed
        ) {
            return Err(self.failure.unwrap_or(DeliveryError::WrongState));
        }
        if self.recovery_until.is_some_and(|until| now >= until) {
            return self.fail(DeliveryError::RecoveryExpired);
        }
        if self
            .in_flight
            .is_some_and(|pending| now >= pending.deadline)
            || self
                .slots
                .iter()
                .flatten()
                .any(|a| now >= a.reference_until)
        {
            return self.fail(DeliveryError::ReferenceExpired);
        }
        Ok(())
    }
    pub fn next_deadline(&self) -> Option<u64> {
        if matches!(
            self.state,
            ReceiveState::NeedsRecovery | ReceiveState::Closed
        ) {
            return None;
        }
        let mut next = self.recovery_until;
        for a in self.slots.iter().flatten() {
            let candidate = if !a.complete()
                && a.reliable_offset.is_none()
                && a.repair_attempts < self.config.policy.max_repair_attempts
            {
                a.reference_until.min(a.next_repair)
            } else {
                a.reference_until
            };
            next = Some(next.map_or(candidate, |old| old.min(candidate)));
        }
        if let Some(pending) = self.in_flight {
            next = Some(next.map_or(pending.deadline, |old| old.min(pending.deadline)));
        }
        next
    }
    /// Install a fresh subscription generation after an admitted recovery or
    /// codec change. Existing external picture reservations stay charged.
    pub fn replace(
        &mut self,
        epoch: MediaEpoch,
        bindings: MediaBindings,
        now: u64,
    ) -> Result<(), DeliveryError> {
        self.check_clock(now)?;
        if self.state == ReceiveState::Closed {
            return Err(DeliveryError::WrongState);
        }
        if !epoch.replaces(self.config.epoch) || !bindings.all_newer_than(self.config.bindings) {
            return Err(DeliveryError::StaleGeneration);
        }
        self.clear();
        self.config.epoch = epoch;
        self.config.bindings = bindings;
        self.state = ReceiveState::AwaitingConfiguration;
        self.failure = None;
        Ok(())
    }
    pub fn close(&mut self) {
        self.clear();
        self.state = ReceiveState::Closed;
    }
    fn clear(&mut self) {
        for slot in &mut self.slots {
            *slot = None;
        }
        self.in_flight = None;
        self.recovery_until = None;
        self.recovery_frame = None;
        self.last_decoded = None;
        self.progress = None;
    }
    fn fail<T>(&mut self, error: DeliveryError) -> Result<T, DeliveryError> {
        self.clear();
        self.failure = Some(error);
        if self.state != ReceiveState::Closed {
            self.state = ReceiveState::NeedsRecovery;
        }
        Err(error)
    }
    fn check_clock(&mut self, now: u64) -> Result<(), DeliveryError> {
        if self.last_now.is_some_and(|last| now < last) {
            self.close();
            self.failure = Some(DeliveryError::ClockRegression);
            return Err(DeliveryError::ClockRegression);
        }
        self.last_now = Some(now);
        Ok(())
    }
}
