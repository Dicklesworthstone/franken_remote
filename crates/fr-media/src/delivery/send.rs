//! One-copy encoded-frame ownership and bounded selective retransmission.
use super::{DeliveryError, MediaBindings, MediaEpoch, deadline};
use core::fmt;
use fr_wire::{
    Channel, Fragment, MediaLimits, Progress, Record, RecoveryChunk, RepairRange, WireError,
    decode_repair, encode_fragment, encode_progress, encode_recovery,
};
use std::sync::Arc;

const CACHE_SLOTS: usize = 64;
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

struct CachedPicture {
    progress: Progress,
    bytes: Vec<u8>,
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
    last_now: Option<u64>,
    repair: Option<RepairJob>,
    repair_window_start: u64,
    repair_spent: usize,
    closed: bool,
    needs_recovery: bool,
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
            last_now: None,
            repair: None,
            repair_window_start: 0,
            repair_spent: 0,
            closed: false,
            needs_recovery: false,
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
    /// progress is supplied by the capture adapter; the sender does not invent
    /// capture-freshness evidence or certify declared IDRs from opaque bytes.
    pub fn push(
        &mut self,
        progress: Progress,
        bytes: Vec<u8>,
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
            .capacity()
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
        let send_by = deadline(now, lifetime)?;
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
            return Ok(None);
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
    /// Expire by the insertion deadline, even on an idle connection. Losing an
    /// unsent reference fences all its dependents instead of sending a broken chain.
    pub fn tick(&mut self, now: u64) -> Result<(), SendError> {
        self.check_clock(now)?;
        if self.closed {
            return Err(SendError::Closed);
        }
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
        if self.closed || self.needs_recovery {
            return None;
        }
        self.pictures.iter().flatten().map(|p| p.send_by).min()
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
        if !epoch.replaces(self.epoch) || !bindings.all_newer_than(self.bindings) {
            return Err(DeliveryError::StaleGeneration.into());
        }
        self.clear();
        self.epoch = epoch;
        self.bindings = bindings;
        self.last_inserted = None;
        self.needs_recovery = false;
        // Session-wide repair allowance is deliberately NOT reset by recovery.
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
