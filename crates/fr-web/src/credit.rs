//! Cumulative `0x0081 ReceiverPressure` credit tracker for browser media channels.
//!
//! Per `PROTOCOL.md` §5 and `AGENTS.md` §5:
//! - Receive credits return ONLY when decoded `VideoFrame` objects are closed
//!   or canceled decoder inputs are disposed, and receive allocations are freed.
//! - Receipt, decode, or presentation acknowledgements alone NEVER advance credit.
//! - Cumulative counters are strictly monotone within a nonzero `credit_epoch`
//!   and never exceed cumulative received counts/bytes.

use core::fmt;
use fr_core::limits::ProtocolLimits;
use fr_wire::{
    WireError,
    input::{InputDelivery, InputDirection},
    negotiation::ControlBinding,
    receiver_pressure::{self, ChannelRole, QueuePressure, RECORD_BYTES, ReceiverPressure},
};

/// Typed errors from browser receiver-credit accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserCreditError {
    InvalidBindingOrEpoch,
    ZeroCharge,
    ReleaseExceedsReceived,
    CounterOverflow,
    ResourceLimit,
    InvalidQueueUsage,
    Wire(WireError),
}

impl fmt::Display for BrowserCreditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidBindingOrEpoch => {
                f.write_str("channel binding and credit epoch must be nonzero")
            }
            Self::ZeroCharge => f.write_str("frame charge must be nonzero"),
            Self::ReleaseExceedsReceived => {
                f.write_str("credit release cannot exceed received frames or bytes")
            }
            Self::CounterOverflow => f.write_str("cumulative credit counter overflow"),
            Self::ResourceLimit => f.write_str("browser receiver credit window exceeded"),
            Self::InvalidQueueUsage => f.write_str("receiver queue does not match retained credit"),
            Self::Wire(e) => write!(f, "receiver pressure wire error: {e}"),
        }
    }
}

impl core::error::Error for BrowserCreditError {}

/// Single owner of a browser data channel's cumulative `0x0081 ReceiverPressure` counters.
#[derive(Debug, Clone)]
pub struct BrowserCreditTracker {
    control_binding: ControlBinding,
    channel_binding: u32,
    channel_role: ChannelRole,
    credit_epoch: u64,
    limits: ProtocolLimits,
    received_bytes: u64,
    received_frames: u64,
    released_bytes: u64,
    released_frames: u64,
}

impl BrowserCreditTracker {
    /// Construct a new tracker for one attached browser media channel.
    pub fn new(
        control_binding: ControlBinding,
        channel_binding: u32,
        channel_role: ChannelRole,
        credit_epoch: u64,
        limits: ProtocolLimits,
    ) -> Result<Self, BrowserCreditError> {
        if control_binding.id == 0
            || control_binding.host_boot.as_raw() == 0
            || control_binding.os_session.as_raw() == 0
            || control_binding.remote_session.as_raw() == 0
            || channel_binding == 0
            || credit_epoch == 0
        {
            return Err(BrowserCreditError::InvalidBindingOrEpoch);
        }
        if !channel_role.admits_report_direction(InputDirection::ViewerToHost) {
            return Err(BrowserCreditError::Wire(WireError::WrongRole));
        }
        limits
            .validate_control_message_len(RECORD_BYTES)
            .map_err(|_| BrowserCreditError::ResourceLimit)?;
        Ok(Self {
            control_binding,
            channel_binding,
            channel_role,
            credit_epoch,
            limits,
            received_bytes: 0,
            received_frames: 0,
            released_bytes: 0,
            released_frames: 0,
        })
    }

    /// Record one received complete frame entering the browser receive budget.
    pub fn record_received(&mut self, charged_bytes: u64) -> Result<(), BrowserCreditError> {
        let (next_bytes, next_frames) = self.next_received(charged_bytes)?;
        self.received_bytes = next_bytes;
        self.received_frames = next_frames;
        Ok(())
    }

    /// Check receive admission before any codec/reference state is changed.
    pub fn can_record_received(&self, charged_bytes: u64) -> Result<(), BrowserCreditError> {
        self.next_received(charged_bytes).map(|_| ())
    }

    fn next_received(&self, charged_bytes: u64) -> Result<(u64, u64), BrowserCreditError> {
        if charged_bytes == 0 {
            return Err(BrowserCreditError::ZeroCharge);
        }
        let next_bytes = self
            .received_bytes
            .checked_add(charged_bytes)
            .ok_or(BrowserCreditError::CounterOverflow)?;
        let next_frames = self
            .received_frames
            .checked_add(1)
            .ok_or(BrowserCreditError::CounterOverflow)?;
        if next_bytes - self.released_bytes > self.limits.per_viewer_compressed_bytes()
            || next_frames - self.released_frames
                > u64::from(self.limits.max_fragments_per_access_unit())
        {
            return Err(BrowserCreditError::ResourceLimit);
        }
        Ok((next_bytes, next_frames))
    }

    /// Record that a `VideoFrame.close()` (or equivalent freed allocation) released
    /// `freed_bytes` for one frame, returning the encoded `0x0081 ReceiverPressure` wire record.
    pub fn record_freed_and_encode(
        &mut self,
        freed_bytes: u64,
        queued_bytes: u64,
        queued_items: u32,
    ) -> Result<[u8; RECORD_BYTES], BrowserCreditError> {
        if freed_bytes == 0 {
            return Err(BrowserCreditError::ZeroCharge);
        }
        let next_released_bytes = self
            .released_bytes
            .checked_add(freed_bytes)
            .ok_or(BrowserCreditError::CounterOverflow)?;
        let next_released_frames = self
            .released_frames
            .checked_add(1)
            .ok_or(BrowserCreditError::CounterOverflow)?;
        if next_released_bytes > self.received_bytes || next_released_frames > self.received_frames
        {
            return Err(BrowserCreditError::ReleaseExceedsReceived);
        }
        if queued_bytes != self.received_bytes - next_released_bytes
            || u64::from(queued_items) != self.received_frames - next_released_frames
        {
            return Err(BrowserCreditError::InvalidQueueUsage);
        }

        let pressure = if queued_items == 0 {
            QueuePressure::Low
        } else if queued_items <= 2 {
            QueuePressure::Moderate
        } else {
            QueuePressure::High
        };

        let report = ReceiverPressure {
            channel_binding: self.channel_binding,
            channel_role: self.channel_role,
            credit_epoch: self.credit_epoch,
            released_bytes: next_released_bytes,
            released_frames: next_released_frames,
            pressure,
            queued_bytes,
            queued_items,
        };

        let mut out = [0u8; RECORD_BYTES];
        let written = receiver_pressure::encode(
            &report,
            self.control_binding,
            &self.limits,
            &mut out,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )
        .map_err(BrowserCreditError::Wire)?;
        if written != RECORD_BYTES {
            return Err(BrowserCreditError::Wire(WireError::InvalidValue));
        }

        self.released_bytes = next_released_bytes;
        self.released_frames = next_released_frames;
        Ok(out)
    }

    /// Current cumulative released bytes.
    #[must_use]
    pub const fn released_bytes(&self) -> u64 {
        self.released_bytes
    }

    /// Current cumulative released frames.
    #[must_use]
    pub const fn released_frames(&self) -> u64 {
        self.released_frames
    }

    /// Current outstanding (not-yet-closed) bytes.
    #[must_use]
    pub const fn outstanding_bytes(&self) -> u64 {
        self.received_bytes - self.released_bytes
    }

    /// Current outstanding (not-yet-closed) frames.
    #[must_use]
    pub const fn outstanding_frames(&self) -> u64 {
        self.received_frames - self.released_frames
    }
}
