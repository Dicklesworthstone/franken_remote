//! One-copy encoded-frame ownership and bounded selective retransmission.
use super::{DeliveryError, MediaBindings, MediaEpoch, SharedFrame, deadline};
use core::fmt;
use fr_wire::{
    Channel, Fragment, MediaLimits, Progress, Record, RecoveryChunk, RepairRange, WireError,
    decode_repair, encode_fragment, encode_progress, encode_recovery,
};
use std::sync::Arc;

mod observation;
mod recovery;
use observation::PendingObservation;
use recovery::PendingRecovery;
pub use recovery::{RecoveryDemand, RecoveryDisposition};

const CACHE_SLOTS: usize = 64;
const RECOVERY_SLOTS: usize = 16;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendPolicy {
    pub max_cached_pictures: usize,
    pub max_cached_bytes: usize,
    pub reference_horizon_micros: u64,
    pub recovery_horizon_micros: u64,
    pub minimum_repair_interval_micros: u64,
    pub max_repair_rounds: u8,
    pub repair_bytes_per_window: usize,
    pub repair_window_micros: u64,
    /// Failed generations admitted per subscription in a sliding time window.
    /// Exceeding this allowance terminally refuses only this sender. A lower
    /// independent operating point requires a separate application admission.
    pub max_recoveries_per_window: u8,
    pub recovery_window_micros: u64,
}
impl Default for SendPolicy {
    fn default() -> Self {
        Self {
            max_cached_pictures: 32,
            max_cached_bytes: 32 * 1024 * 1024,
            reference_horizon_micros: 250_000,
            recovery_horizon_micros: 2_000_000,
            minimum_repair_interval_micros: 10_000,
            max_repair_rounds: 3,
            repair_bytes_per_window: 1024 * 1024,
            repair_window_micros: 250_000,
            max_recoveries_per_window: 4,
            recovery_window_micros: 10_000_000,
        }
    }
}
impl SendPolicy {
    fn validate(self, limits: &MediaLimits) -> Result<(), SendError> {
        if !(1..=CACHE_SLOTS).contains(&self.max_cached_pictures)
            || self.max_cached_bytes == 0
            || u64::try_from(self.max_cached_bytes).map_err(|_| SendError::InvalidPolicy)?
                > limits.protocol().per_viewer_compressed_bytes()
            || !(1..=250_000).contains(&self.reference_horizon_micros)
            || !(1..=5_000_000).contains(&self.recovery_horizon_micros)
            || self.minimum_repair_interval_micros == 0
            || self.minimum_repair_interval_micros > self.reference_horizon_micros
            || !(1..=8).contains(&self.max_repair_rounds)
            || self.repair_bytes_per_window == 0
            || self.repair_bytes_per_window > self.max_cached_bytes
            || !(1..=1_000_000).contains(&self.repair_window_micros)
            || !(1..=RECOVERY_SLOTS).contains(&usize::from(self.max_recoveries_per_window))
            || !(1_000_000..=60_000_000).contains(&self.recovery_window_micros)
        {
            return Err(SendError::InvalidPolicy);
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SendError {
    Delivery(DeliveryError),
    Wire(WireError),
    InvalidPolicy,
    Closed,
    NeedsRecovery,
    InvalidSequence,
    InvalidDependency,
    InvalidRecovery,
    InvalidPayload,
    CacheFull,
    FrameUnavailable,
    RepairBusy,
    RepairNotReady,
    RepairRateLimited,
    RepairBudgetExceeded,
    OriginalExpired,
    InvalidObservation,
    ObservationExpired,
    /// Chronic reference failure exhausted this subscription's recovery window.
    /// The cache is closed; a later clock value or generation cannot revive it.
    RecoveryLimitExceeded,
}
impl From<WireError> for SendError {
    fn from(e: WireError) -> Self {
        Self::Wire(e)
    }
}
impl From<DeliveryError> for SendError {
    fn from(e: DeliveryError) -> Self {
        Self::Delivery(e)
    }
}
impl fmt::Display for SendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl core::error::Error for SendError {}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryMode {
    Recovery,
    Datagrams,
}
/// A successfully encoded record, not confirmation of transport delivery.
/// Retain at most the admitted bounded transport work and call
/// `SendCache::authorize_write` immediately before writing the unchanged bytes.
/// Congestion admission is still the transport adapter's job. Each offer owns
/// one reference to the cache's fixed identity allocation, never media payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PacketOffer {
    channel: Channel,
    byte_len: usize,
    frame: u64,
    send_by_micros: u64,
    origin: OfferOrigin,
}
impl PacketOffer {
    pub const fn channel(&self) -> Channel {
        self.channel
    }
    pub const fn byte_len(&self) -> usize {
        self.byte_len
    }
    pub const fn frame(&self) -> u64 {
        self.frame
    }
    pub const fn send_by_micros(&self) -> u64 {
        self.send_by_micros
    }
}
#[derive(Debug, Clone)]
struct OfferOrigin {
    owner: Arc<()>,
    epoch: MediaEpoch,
}
impl PartialEq for OfferOrigin {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.owner, &other.owner) && self.epoch == other.epoch
    }
}
impl Eq for OfferOrigin {}

// The legacy single-viewer path still moves its Vec without an Arc allocation.
// Shared payloads retain one physical pool charge while each viewer is charged
// for all the bytes it can pin, not one fraction of a shared allocation.
enum PictureBytes {
    Owned(Vec<u8>),
    Shared(SharedFrame),
}
impl PictureBytes {
    fn charge(&self) -> usize {
        match self {
            Self::Owned(bytes) => bytes.capacity(),
            Self::Shared(frame) => frame.allocation_charge(),
        }
    }
}
impl core::ops::Deref for PictureBytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            Self::Owned(bytes) => bytes,
            Self::Shared(frame) => frame.bytes(),
        }
    }
}

struct CachedPicture {
    progress: Progress,
    bytes: PictureBytes,
    mode: DeliveryMode,
    charged: usize,
    send_by: u64,
    announced: bool,
    next_original: u32,
    repair_rounds: u8,
    next_repair: u64,
}
impl CachedPicture {
    fn original_complete(&self) -> bool {
        self.next_original
            >= match self.mode {
                DeliveryMode::Recovery => self.progress.descriptor.total_bytes,
                DeliveryMode::Datagrams => self
                    .progress
                    .descriptor
                    .fragment_count()
                    .expect("validated descriptor"),
            }
    }
    fn fragment(&self, index: u32) -> Result<Fragment<'_>, SendError> {
        let descriptor = self.progress.descriptor;
        Ok(Fragment {
            descriptor,
            index,
            bytes: &self.bytes[descriptor.fragment_range(index)?],
        })
    }
}
struct RepairJob {
    frame: u64,
    ranges: [RepairRange; 64],
    count: usize,
    range: usize,
    fragment: u32,
}

/// One subscription's sender and repair cache. Move already encoded buffers
/// into push; no frame clone or packet FIFO is created. The 32-picture default
/// is deliberately separate from receiver W: 250 ms at 60 fps exceeds W=12.
pub struct SendCache {
    // One fixed Arc control block per cache, shared by bounded queued offers.
    // Replacement reuses it: old generations never accumulate new identities.
    owner: Arc<()>,
    limits: MediaLimits,
    bindings: MediaBindings,
    epoch: MediaEpoch,
    policy: SendPolicy,
    pictures: [Option<CachedPicture>; CACHE_SLOTS],
    used_bytes: usize,
    used_pictures: usize,
    last_inserted: Option<u64>,
    // Fixed metadata survives payload eviction; observations never revive bytes.
    last_progress: Option<Progress>,
    observation: Option<PendingObservation>,
    last_now: Option<u64>,
    repair: Option<RepairJob>,
    repair_window_start: u64,
    repair_spent: usize,
    closed: bool,
    needs_recovery: bool,
    recovery_request: Option<PendingRecovery>,
    // Fixed metadata belongs to the subscription, not a replaceable generation.
    // Neither clear(), payload eviction nor codec reconfiguration refills it.
    recoveries: [Option<u64>; RECOVERY_SLOTS],
    recovery_charged_epoch: Option<MediaEpoch>,
}
impl fmt::Debug for SendCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SendCache")
            .field("epoch", &self.epoch)
            .field("cached_bytes", &self.used_bytes)
            .field("cached_pictures", &self.used_pictures)
            .field("needs_recovery", &self.needs_recovery)
            .field("closed", &self.closed)
            .finish_non_exhaustive()
    }
}
impl SendCache {
    pub fn new(
        limits: MediaLimits,
        bindings: MediaBindings,
        epoch: MediaEpoch,
        policy: SendPolicy,
    ) -> Result<Self, SendError> {
        policy.validate(&limits)?;
        Ok(Self {
            owner: Arc::new(()),
            limits,
            bindings,
            epoch,
            policy,
            pictures: core::array::from_fn(|_| None),
            used_bytes: 0,
            used_pictures: 0,
            last_inserted: None,
            last_progress: None,
            observation: None,
            last_now: None,
            repair: None,
            repair_window_start: 0,
            repair_spent: 0,
            closed: false,
            needs_recovery: false,
            recovery_request: None,
            recoveries: [None; RECOVERY_SLOTS],
            recovery_charged_epoch: None,
        })
    }
    pub const fn cached_bytes(&self) -> usize {
        self.used_bytes
    }
    pub const fn cached_pictures(&self) -> usize {
        self.used_pictures
    }
    pub const fn needs_recovery(&self) -> bool {
        self.needs_recovery
    }
    /// Largest Vec allocation that can fit even when the cache is empty.
    /// An upstream producer must refuse a maximum larger than this instead of
    /// waiting forever for credit that can never exist.
    pub fn maximum_capacity(&self) -> usize {
        self.policy
            .max_cached_bytes
            .saturating_sub(core::mem::size_of::<CachedPicture>())
    }
    /// Admission credit for ONE not-yet-produced access unit. The owner must
    /// retain this reservation until transferring the result to `push`, and must
    /// not enqueue other pictures in between. Includes actual Vec capacity and
    /// per-picture metadata, just like `push`; this does not allocate or evict.
    pub fn can_push_capacity(&self, capacity: usize) -> bool {
        !self.closed
            && !self.needs_recovery
            && self.used_pictures < self.policy.max_cached_pictures
            && capacity
                .checked_add(core::mem::size_of::<CachedPicture>())
                .and_then(|charge| self.used_bytes.checked_add(charge))
                .is_some_and(|total| total <= self.policy.max_cached_bytes)
    }
    /// Does not advance the packetizer or consume a source observation. A
    /// prepared transport packet remains separately owned by its egress.
    pub fn originals_pending(&self) -> bool {
        self.observation.is_some()
            || self
                .pictures
                .iter()
                .flatten()
                .any(|p| !p.announced || !p.original_complete())
    }
    /// progress is supplied by the capture adapter; the sender does not invent
    /// capture-freshness evidence or certify declared IDRs from opaque bytes.
    pub fn push(
        &mut self,
        progress: Progress,
        bytes: Vec<u8>,
        mode: DeliveryMode,
        now: u64,
    ) -> Result<(), SendError> {
        self.push_picture(progress, PictureBytes::Owned(bytes), mode, now)
    }
    /// Share one encoded allocation with other admitted subscribers. Frame,
    /// dependency, configuration and capture time come from the immutable frame;
    /// the local subscription alone supplies stride, channels and recovery epoch.
    /// A shared IDR is ordinary independent video for an already healthy viewer,
    /// and reliable recovery for a newly attached/recovered viewer. This method
    /// neither authenticates the source nor certifies opaque bytes as HEVC.
    pub fn push_shared(
        &mut self,
        frame: &SharedFrame,
        mode: DeliveryMode,
        now: u64,
    ) -> Result<(), SendError> {
        if frame.configuration() != self.epoch.configuration {
            return Err(DeliveryError::StaleGeneration.into());
        }
        self.push_picture(
            frame.progress(self.limits.fragment_stride())?,
            PictureBytes::Shared(frame.clone()),
            mode,
            now,
        )
    }
    fn push_picture(
        &mut self,
        progress: Progress,
        bytes: PictureBytes,
        mode: DeliveryMode,
        now: u64,
    ) -> Result<(), SendError> {
        self.tick(now)?;
        let d = progress.descriptor;
        d.validate(&self.limits)?;
        if usize::try_from(d.total_bytes).map_err(|_| SendError::InvalidPayload)? != bytes.len() {
            return Err(SendError::InvalidPayload);
        }
        if self.last_inserted.is_some_and(|last| d.frame <= last) {
            return Err(SendError::InvalidSequence);
        }
        if d.reference.is_some() && d.reference != self.last_inserted {
            return Err(SendError::InvalidDependency);
        }
        if self.last_inserted.is_none() && (d.reference.is_some() || mode != DeliveryMode::Recovery)
            || self.last_inserted.is_some() && mode == DeliveryMode::Recovery
            || mode == DeliveryMode::Recovery && d.stride != self.limits.fragment_stride()
        {
            return Err(SendError::InvalidRecovery);
        }
        let charged = bytes
            .charge()
            .checked_add(core::mem::size_of::<CachedPicture>())
            .ok_or(SendError::CacheFull)?;
        let used = self
            .used_bytes
            .checked_add(charged)
            .ok_or(SendError::CacheFull)?;
        if used > self.policy.max_cached_bytes
            || self.used_pictures >= self.policy.max_cached_pictures
        {
            return Err(SendError::CacheFull);
        }
        let lifetime = match mode {
            DeliveryMode::Recovery => self.policy.recovery_horizon_micros,
            DeliveryMode::Datagrams => self.policy.reference_horizon_micros,
        };
        // Codec and application backpressure are part of the picture's age.
        // Enqueueing late must not manufacture another full repair lifetime.
        if d.capture_micros > now {
            return Err(SendError::InvalidObservation);
        }
        let send_by = deadline(d.capture_micros, lifetime)?;
        if now >= send_by {
            return Err(SendError::OriginalExpired);
        }
        let index = self
            .pictures
            .iter()
            .position(Option::is_none)
            .ok_or(SendError::CacheFull)?;
        self.pictures[index] = Some(CachedPicture {
            progress,
            bytes,
            mode,
            charged,
            send_by,
            announced: false,
            next_original: 0,
            repair_rounds: 0,
            next_repair: now,
        });
        self.used_bytes = used;
        self.used_pictures += 1;
        self.last_inserted = Some(d.frame);
        self.last_progress = Some(progress);
        self.observation = None;
        Ok(())
    }
    /// Emits progress before each picture, then at most one original fragment
    /// or contiguous reliable recovery chunk. Call only with transport credit.
    pub fn next_packet(
        &mut self,
        now: u64,
        out: &mut [u8],
    ) -> Result<Option<PacketOffer>, SendError> {
        self.tick(now)?;
        let chosen = self
            .pictures
            .iter()
            .enumerate()
            .filter_map(|(i, p)| {
                let p = p.as_ref()?;
                (!p.announced || !p.original_complete()).then_some((i, p.progress.descriptor.frame))
            })
            .min_by_key(|(_, frame)| *frame);
        let Some((index, frame)) = chosen else {
            // Original announcements and references precede idle observations.
            return self.next_observation(out);
        };
        let p = self.pictures[index].as_mut().expect("selected");
        let (channel, n) = if p.announced {
            match p.mode {
                DeliveryMode::Datagrams => {
                    let n = encode_fragment(
                        p.fragment(p.next_original)?,
                        self.bindings.for_channel(Channel::Video),
                        &self.limits,
                        out,
                    )?;
                    p.next_original += 1;
                    (Channel::Video, n)
                }
                DeliveryMode::Recovery => {
                    let start = p.next_original as usize;
                    let length = (self.limits.record_bytes() - fr_wire::RECOVERY_OVERHEAD)
                        .min(p.bytes.len() - start);
                    let n = encode_recovery(
                        RecoveryChunk {
                            frame,
                            total_bytes: p.progress.descriptor.total_bytes,
                            offset: p.next_original,
                            capture_micros: p.progress.descriptor.capture_micros,
                            bytes: &p.bytes[start..start + length],
                        },
                        self.bindings.for_channel(Channel::Recovery),
                        &self.limits,
                        out,
                    )?;
                    p.next_original +=
                        u32::try_from(length).map_err(|_| SendError::InvalidPayload)?;
                    (Channel::Recovery, n)
                }
            }
        } else {
            let n = encode_progress(
                p.progress,
                self.bindings.for_channel(Channel::MediaConfig),
                &self.limits,
                out,
            )?;
            p.announced = true;
            (Channel::MediaConfig, n)
        };
        Ok(Some(PacketOffer {
            channel,
            byte_len: n,
            frame,
            send_by_micros: p.send_by,
            origin: OfferOrigin {
                owner: self.owner.clone(),
                epoch: self.epoch,
            },
        }))
    }
    /// Accept one bounded repair job from an already authorized control channel.
    /// Queueing never extends the original cache deadline or bypasses pacing.
    pub fn queue_repair(&mut self, packet: &[u8], now: u64) -> Result<(), SendError> {
        let record = Record::decode(
            packet,
            &self.limits,
            self.bindings.for_channel(Channel::Control),
            Channel::Control,
        )?;
        let request = decode_repair(record, self.limits.max_fragments(), &self.limits)?;
        self.tick(now)?;
        if self.repair.is_some() {
            return Err(SendError::RepairBusy);
        }
        let index = self
            .index(request.frame)
            .ok_or(SendError::FrameUnavailable)?;
        let p = self.pictures[index].as_mut().expect("located");
        if p.mode == DeliveryMode::Recovery || !p.original_complete() {
            return Err(SendError::RepairNotReady);
        }
        let total = p.progress.descriptor.fragment_count()?;
        let mut ranges = [RepairRange { start: 0, end: 0 }; 64];
        let mut count = 0;
        for range in request.ranges() {
            if range.end > total {
                return Err(SendError::Wire(WireError::InvalidRanges));
            }
            ranges[count] = range;
            count += 1;
        }
        if now < p.next_repair || p.repair_rounds >= self.policy.max_repair_rounds {
            return Err(SendError::RepairRateLimited);
        }
        let next_repair = deadline(now, self.policy.minimum_repair_interval_micros)?;
        p.next_repair = next_repair;
        p.repair_rounds += 1;
        self.repair = Some(RepairJob {
            frame: request.frame,
            fragment: ranges[0].start,
            ranges,
            count,
            range: 0,
        });
        Ok(())
    }
    /// Emits one original fragment from the cache. Returns no packet once the
    /// job completes/expires. Repair traffic has its own bounded offered-load
    /// allowance AND must pass the transport's normal congestion admission.
    pub fn next_repair_packet(
        &mut self,
        now: u64,
        out: &mut [u8],
    ) -> Result<Option<PacketOffer>, SendError> {
        self.tick(now)?;
        let Some(job) = self.repair.as_ref() else {
            return Ok(None);
        };
        let index = self.index(job.frame).ok_or(SendError::FrameUnavailable)?;
        let p = self.pictures[index].as_ref().expect("located");
        let fragment = p.fragment(job.fragment)?;
        let charge = fragment.bytes.len()
            + if fragment.descriptor.reference.is_some() {
                73
            } else {
                65
            };
        if charge > self.policy.repair_bytes_per_window - self.repair_spent {
            self.repair = None;
            return Err(SendError::RepairBudgetExceeded);
        }
        let n = encode_fragment(
            fragment,
            self.bindings.for_channel(Channel::Video),
            &self.limits,
            out,
        )?;
        let offer = PacketOffer {
            channel: Channel::Video,
            byte_len: n,
            frame: job.frame,
            send_by_micros: p.send_by,
            origin: OfferOrigin {
                owner: self.owner.clone(),
                epoch: self.epoch,
            },
        };
        self.repair_spent += n;
        let job = self.repair.as_mut().expect("present");
        job.fragment += 1;
        if job.fragment == job.ranges[job.range].end {
            job.range += 1;
            if job.range == job.count {
                self.repair = None;
            } else {
                job.fragment = job.ranges[job.range].start;
            }
        }
        Ok(Some(offer))
    }
    /// Recheck this exact cache/generation and its entire chain immediately
    /// before transport submission. Preparing a packet is not authorization to
    /// send after replacement, close, or an unsent predecessor's expiry.
    pub fn authorize_write(&mut self, offer: &PacketOffer, now: u64) -> Result<(), SendError> {
        self.tick(now)?;
        if !Arc::ptr_eq(&self.owner, &offer.origin.owner) || self.epoch != offer.origin.epoch {
            return Err(DeliveryError::StaleGeneration.into());
        }
        if now >= offer.send_by_micros {
            return Err(SendError::OriginalExpired);
        }
        Ok(())
    }
    /// Expire by the capture-anchored deadline, even on an idle connection. Losing an
    /// unsent reference fences all its dependents instead of sending a broken chain.
    pub fn tick(&mut self, now: u64) -> Result<(), SendError> {
        self.check_clock(now)?;
        if self.closed {
            return Err(SendError::Closed);
        }
        self.check_recovery_deadline(now)?;
        if self.needs_recovery {
            return Err(SendError::NeedsRecovery);
        }
        if self
            .pictures
            .iter()
            .flatten()
            .any(|p| now >= p.send_by && !p.original_complete())
        {
            self.clear();
            self.needs_recovery = true;
            return Err(SendError::OriginalExpired);
        }
        // Expired source metadata is replaceable, not a broken codec reference.
        if self.observation.as_ref().is_some_and(|o| now >= o.send_by) {
            self.observation = None;
        }
        for index in 0..CACHE_SLOTS {
            if self.pictures[index]
                .as_ref()
                .is_some_and(|p| now >= p.send_by)
            {
                let p = self.pictures[index].take().expect("expired");
                self.used_bytes -= p.charged;
                self.used_pictures -= 1;
            }
        }
        if self
            .repair
            .as_ref()
            .is_some_and(|job| self.index(job.frame).is_none())
        {
            self.repair = None;
        }
        if now - self.repair_window_start >= self.policy.repair_window_micros {
            self.repair_window_start = now;
            self.repair_spent = 0;
        }
        Ok(())
    }
    pub fn next_deadline(&self) -> Option<u64> {
        if self.closed {
            return None;
        }
        if self.needs_recovery {
            return self.recovery_request.as_ref().map(|p| p.until);
        }
        self.pictures
            .iter()
            .flatten()
            .map(|p| p.send_by)
            .chain(self.observation.as_ref().map(|o| o.send_by))
            .min()
    }
    pub fn replace(
        &mut self,
        epoch: MediaEpoch,
        bindings: MediaBindings,
        now: u64,
    ) -> Result<(), SendError> {
        self.check_clock(now)?;
        if self.closed {
            return Err(SendError::Closed);
        }
        self.check_recovery_deadline(now)?;
        if !epoch.replaces(self.epoch) || !bindings.all_newer_than(self.bindings) {
            return Err(DeliveryError::StaleGeneration.into());
        }
        // Replacing an expired original without first ticking must not bypass
        // the same failure allowance used by the ordinary recovery path.
        if self.needs_recovery
            || self
                .pictures
                .iter()
                .flatten()
                .any(|p| now >= p.send_by && !p.original_complete())
        {
            self.admit_recovery(now)?;
        }
        self.clear();
        self.epoch = epoch;
        self.bindings = bindings;
        self.last_inserted = None;
        self.needs_recovery = false;
        // Subscription-wide repair AND recovery allowances survive replacement.
        Ok(())
    }
    /// Charge at most once for a failed generation. Call only after validating
    /// the request/replacement and checking the subscription's monotonic clock.
    /// The sliding window avoids a fresh burst at an arbitrary window boundary.
    fn admit_recovery(&mut self, now: u64) -> Result<(), SendError> {
        if self.recovery_charged_epoch == Some(self.epoch) {
            return Ok(());
        }
        let slots = &mut self.recoveries[..usize::from(self.policy.max_recoveries_per_window)];
        for slot in slots.iter_mut() {
            if slot.is_some_and(|at| now.saturating_sub(at) >= self.policy.recovery_window_micros) {
                *slot = None;
            }
        }
        let Some(slot) = slots.iter_mut().find(|slot| slot.is_none()) else {
            self.close();
            return Err(SendError::RecoveryLimitExceeded);
        };
        *slot = Some(now);
        self.recovery_charged_epoch = Some(self.epoch);
        Ok(())
    }
    pub fn close(&mut self) {
        self.clear();
        self.closed = true;
    }
    fn index(&self, frame: u64) -> Option<usize> {
        self.pictures.iter().position(|p| {
            p.as_ref()
                .is_some_and(|p| p.progress.descriptor.frame == frame)
        })
    }
    fn clear(&mut self) {
        self.recovery_request = None;
        self.last_progress = None;
        self.observation = None;
        for p in &mut self.pictures {
            *p = None;
        }
        self.used_bytes = 0;
        self.used_pictures = 0;
        self.repair = None;
    }
    fn check_clock(&mut self, now: u64) -> Result<(), SendError> {
        if self.last_now.is_some_and(|last| now < last) {
            self.close();
            return Err(DeliveryError::ClockRegression.into());
        }
        self.last_now = Some(now);
        Ok(())
    }
}

#[cfg(test)]
mod recovery_limit_tests {
    use super::*;
    use fr_core::{ids::*, limits::ProtocolLimits};
    use fr_wire::{FrameDescriptor, PipelineState, SourceObservation};

    fn policy() -> SendPolicy {
        SendPolicy {
            // Deterministic expiry, not a claim about native codec timing.
            recovery_horizon_micros: 1,
            max_recoveries_per_window: 2,
            recovery_window_micros: 1_000_000,
            ..SendPolicy::default()
        }
    }
    fn cache(policy: SendPolicy) -> SendCache {
        SendCache::new(
            MediaLimits::new(ProtocolLimits::ABSOLUTE, 1150, 16_384, 64).unwrap(),
            MediaBindings::new(1, 2, 3, 4).unwrap(),
            MediaEpoch {
                configuration: CodecConfigurationGeneration::INITIAL,
                recovery: RecoveryGeneration::INITIAL,
            },
            policy,
        )
        .unwrap()
    }
    fn picture(cache: &mut SendCache, now: u64) {
        cache
            .push(
                Progress {
                    descriptor: FrameDescriptor {
                        frame: 0,
                        total_bytes: 128,
                        stride: cache.limits.fragment_stride(),
                        capture_micros: now,
                        reference: None,
                    },
                    observed_micros: now,
                    observation: SourceObservation::Captured,
                    pipeline: PipelineState::Running,
                },
                vec![7; 128],
                DeliveryMode::Recovery,
                now,
            )
            .unwrap();
    }
    fn replacement(cache: &SendCache) -> (MediaEpoch, MediaBindings) {
        let base = cache.bindings.for_channel(Channel::Control) + 1;
        (
            MediaEpoch {
                recovery: cache.epoch.recovery.next().unwrap(),
                ..cache.epoch
            },
            MediaBindings::new(base, base + 1, base + 2, base + 3).unwrap(),
        )
    }
    fn replace(cache: &mut SendCache, now: u64) -> Result<(), SendError> {
        let (epoch, bindings) = replacement(cache);
        cache.replace(epoch, bindings, now)
    }
    fn failed_generation(cache: &mut SendCache, now: u64) {
        picture(cache, now);
        assert_eq!(cache.tick(now + 1), Err(SendError::OriginalExpired));
        replace(cache, now + 1).unwrap();
    }
    #[test]
    fn chronic_failure_is_terminal_and_isolated_to_one_subscription() {
        let mut failed = cache(policy());
        let mut healthy = cache(policy());
        failed_generation(&mut failed, 0);
        failed_generation(&mut failed, 10);
        picture(&mut failed, 20);
        let mut bytes = [0; 1150];
        let old_offer = failed.next_packet(20, &mut bytes).unwrap().unwrap();
        assert_eq!(failed.tick(21), Err(SendError::OriginalExpired));
        assert_eq!(
            replace(&mut failed, 21),
            Err(SendError::RecoveryLimitExceeded)
        );
        assert_eq!(failed.cached_bytes(), 0);
        assert_eq!(failed.cached_pictures(), 0);
        assert_eq!(failed.next_deadline(), None);
        assert_eq!(
            failed.authorize_write(&old_offer, 21),
            Err(SendError::Closed)
        );
        assert_eq!(replace(&mut failed, 2_000_000), Err(SendError::Closed));
        picture(&mut healthy, 21);
        assert!(healthy.next_packet(21, &mut bytes).unwrap().is_some());
        assert_eq!(healthy.epoch.recovery, RecoveryGeneration::INITIAL);
    }
    #[test]
    fn replacing_without_tick_cannot_hide_an_expired_original() {
        let mut cache = cache(policy());
        for now in [0, 10] {
            picture(&mut cache, now);
            replace(&mut cache, now + 1).unwrap();
        }
        picture(&mut cache, 20);
        assert_eq!(
            replace(&mut cache, 21),
            Err(SendError::RecoveryLimitExceeded)
        );
    }
    #[test]
    fn healthy_reconfiguration_does_not_spend_or_refill_recovery_credit() {
        let mut cache = cache(policy());
        for now in 0..20 {
            replace(&mut cache, now).unwrap();
        }
        failed_generation(&mut cache, 20);
        let (mut epoch, bindings) = replacement(&cache);
        epoch.configuration = epoch.configuration.next().unwrap();
        cache.replace(epoch, bindings, 22).unwrap();
        failed_generation(&mut cache, 23);
        picture(&mut cache, 25);
        assert_eq!(
            replace(&mut cache, 26),
            Err(SendError::RecoveryLimitExceeded)
        );
    }
    #[test]
    fn invalid_replacement_does_not_spend_credit_or_change_the_failed_epoch() {
        let mut cache = cache(policy());
        failed_generation(&mut cache, 0);
        picture(&mut cache, 2);
        cache.tick(3).unwrap_err();
        let epoch = cache.epoch;
        let (_, bindings) = replacement(&cache);
        for _ in 0..100 {
            assert_eq!(
                cache.replace(epoch, bindings, 3),
                Err(DeliveryError::StaleGeneration.into())
            );
        }
        replace(&mut cache, 3).unwrap();
        assert!(!cache.needs_recovery());
    }
    #[test]
    fn sliding_window_does_not_reset_at_a_calendar_boundary() {
        let mut cache = cache(policy());
        failed_generation(&mut cache, 100);
        failed_generation(&mut cache, 999_900);
        picture(&mut cache, 1_000_000);
        assert_eq!(
            replace(&mut cache, 1_000_001),
            Err(SendError::RecoveryLimitExceeded)
        );
    }
    #[test]
    fn oldest_failure_expires_at_its_exact_window_boundary() {
        let mut cache = cache(policy());
        failed_generation(&mut cache, 0);
        failed_generation(&mut cache, 10);
        picture(&mut cache, 1_000_000);
        replace(&mut cache, 1_000_001).unwrap();
        assert_eq!(cache.recoveries.iter().flatten().count(), 2);
        picture(&mut cache, 1_000_002);
        assert_eq!(
            replace(&mut cache, 1_000_003),
            Err(SendError::RecoveryLimitExceeded)
        );
    }
    #[test]
    fn backwards_clock_is_terminal_without_refilling_the_window() {
        let mut cache = cache(policy());
        failed_generation(&mut cache, 100);
        assert_eq!(
            replace(&mut cache, 99),
            Err(DeliveryError::ClockRegression.into())
        );
        assert_eq!(replace(&mut cache, 2_000_000), Err(SendError::Closed));
    }
    #[test]
    fn recovery_policy_is_bounded_and_cannot_disable_refusal() {
        let limits = cache(policy()).limits;
        for count in [0, 17, u8::MAX] {
            assert_eq!(
                SendPolicy {
                    max_recoveries_per_window: count,
                    ..policy()
                }
                .validate(&limits),
                Err(SendError::InvalidPolicy)
            );
        }
        for window in [0, 999_999, 60_000_001, u64::MAX] {
            assert_eq!(
                SendPolicy {
                    recovery_window_micros: window,
                    ..policy()
                }
                .validate(&limits),
                Err(SendError::InvalidPolicy)
            );
        }
        for count in [1, 16] {
            for window in [1_000_000, 60_000_000] {
                SendPolicy {
                    max_recoveries_per_window: count,
                    recovery_window_micros: window,
                    ..policy()
                }
                .validate(&limits)
                .unwrap();
            }
        }
    }
}
