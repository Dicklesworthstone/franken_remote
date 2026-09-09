//! Encoded-media delivery: real wire bytes, bounded ownership, and loss recovery.
//!
//! Transport adapters call these synchronous, bounded operations from their
//! Asupersync-owned task. There is no second runtime, background thread, codec,
//! or authentication bypass. Bindings must already be admitted by the session.
mod budget;
mod receive;
mod send;
pub use send::{DeliveryMode, PacketOffer, SendCache, SendError, SendPolicy};

pub use budget::{BudgetUsage, MediaBudget};
pub use receive::{
    DecodedFrame, DecoderBinding, ReceiveConfig, ReceivePipeline, ReceivePolicy, ReceiveState,
    ReceiveUpdate, ReceivedPicture,
};

use core::fmt;
use fr_core::ids::{CodecConfigurationGeneration, RecoveryGeneration};
use fr_wire::{Channel, WireError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DeliveryError {
    Wire(WireError),
    InvalidPolicy,
    WrongState,
    ResourceLimit,
    AllocationFailed,
    ConflictingPicture,
    NoncontiguousRecovery,
    ReferenceExpired,
    RecoveryExpired,
    ClockRegression,
    ClockOverflow,
    StaleGeneration,
    DecodeFailed,
    DecodeMismatch,
}
impl From<WireError> for DeliveryError {
    fn from(error: WireError) -> Self {
        Self::Wire(error)
    }
}
impl fmt::Display for DeliveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl core::error::Error for DeliveryError {}

/// Distinct, already installed channel bindings for one receiving subscription.
/// These IDs are not credentials. The session layer owns their complete tuples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaBindings {
    video: u32,
    recovery: u32,
    progress: u32,
    repair: u32,
}
impl MediaBindings {
    pub fn new(
        video: u32,
        recovery: u32,
        progress: u32,
        repair: u32,
    ) -> Result<Self, DeliveryError> {
        let values = [video, recovery, progress, repair];
        for (i, value) in values.iter().enumerate() {
            if *value == 0 || values[..i].contains(value) {
                return Err(DeliveryError::StaleGeneration);
            }
        }
        Ok(Self {
            video,
            recovery,
            progress,
            repair,
        })
    }
    pub const fn for_channel(self, channel: Channel) -> u32 {
        match channel {
            Channel::Video => self.video,
            Channel::Recovery => self.recovery,
            Channel::MediaConfig => self.progress,
            Channel::Control => self.repair,
        }
    }
    pub(crate) fn all_newer_than(self, old: Self) -> bool {
        let highest = old
            .video
            .max(old.recovery)
            .max(old.progress)
            .max(old.repair);
        [self.video, self.recovery, self.progress, self.repair]
            .iter()
            .all(|value| *value > highest)
    }
}
/// Parent host/session identity is held by the admitted binding owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaEpoch {
    pub configuration: CodecConfigurationGeneration,
    pub recovery: RecoveryGeneration,
}
impl MediaEpoch {
    pub(crate) fn replaces(self, old: Self) -> bool {
        self.configuration > old.configuration
            || (self.configuration == old.configuration && self.recovery > old.recovery)
    }
}
pub(crate) fn deadline(now: u64, duration: u64) -> Result<u64, DeliveryError> {
    now.checked_add(duration)
        .ok_or(DeliveryError::ClockOverflow)
}
