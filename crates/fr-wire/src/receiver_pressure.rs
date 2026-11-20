//! Wire codec for `0x0081 ReceiverPressure` (`PROTOCOL.md` §5 and §6).
//!
//! Receiver-to-sender cumulative freed-resource and queue-pressure report on the
//! reliable control channel. Per `PROTOCOL.md` §5:
//! - Reports cumulative freed bytes and frames in one nonzero `credit_epoch`.
//! - Never grants observation/control authority or renews any lease.
//! - Must match the admitted channel role and valid data direction.
use crate::{
    HEADER_BYTES, Kind, Record, WireError,
    input::{InputDelivery, InputDirection},
    negotiation::ControlBinding,
    record::{Reader, Writer},
};
use fr_core::limits::ProtocolLimits;

pub const CAPABILITY: &str = "receiver-pressure";
pub const VERSION: u16 = 1;
/// Fixed wire size including the 24-byte `FRD0` header:
/// 24 + 48 (`ControlBinding`) + 4 (`channel_binding`) + 1 (`channel_role`) +
/// 1 (`direction`) + 8 (`credit_epoch`) + 8 (`released_bytes`) +
/// 8 (`released_frames`) + 1 (`pressure`) + 8 (`queued_bytes`) +
/// 4 (`queued_items`) = 115 bytes.
pub const RECORD_BYTES: usize = HEADER_BYTES + 48 + 4 + 1 + 1 + 8 + 8 + 8 + 1 + 8 + 4;

/// Admitted channel role whose receiver is returning credit and pressure state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ChannelRole {
    Video = 1,
    AudioDown = 2,
    AudioUp = 3,
    Clipboard = 4,
    Files = 5,
    FilesDownload = 6,
}

impl ChannelRole {
    fn parse(v: u8) -> Result<Self, WireError> {
        match v {
            1 => Ok(Self::Video),
            2 => Ok(Self::AudioDown),
            3 => Ok(Self::AudioUp),
            4 => Ok(Self::Clipboard),
            5 => Ok(Self::Files),
            6 => Ok(Self::FilesDownload),
            _ => Err(WireError::InvalidValue),
        }
    }

    /// Whether a receiver-to-sender pressure report on this role may travel in `dir`.
    pub const fn admits_report_direction(self, dir: InputDirection) -> bool {
        match self {
            // Host sends data -> Viewer is the receiver -> ViewerToHost report.
            Self::Video | Self::AudioDown | Self::FilesDownload => {
                matches!(dir, InputDirection::ViewerToHost)
            }
            // Controller sends data -> Host is the receiver -> HostToViewer report.
            Self::AudioUp | Self::Files => matches!(dir, InputDirection::HostToViewer),
            // Clipboard transfers flow both ways between controller and host.
            Self::Clipboard => true,
        }
    }
}

/// Qualitative queue-pressure tier observed by the receiver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum QueuePressure {
    Low = 0,
    Moderate = 1,
    High = 2,
    Starved = 3,
}

impl QueuePressure {
    fn parse(v: u8) -> Result<Self, WireError> {
        match v {
            0 => Ok(Self::Low),
            1 => Ok(Self::Moderate),
            2 => Ok(Self::High),
            3 => Ok(Self::Starved),
            _ => Err(WireError::InvalidValue),
        }
    }
}

/// Content-free cumulative credit release and local queue-pressure snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReceiverPressure {
    /// Nonzero compact binding ID of the admitted data channel being credited.
    pub channel_binding: u32,
    pub channel_role: ChannelRole,
    /// Nonzero credit epoch installed when the data channel attached.
    pub credit_epoch: u64,
    /// Cumulative freed bytes since attachment (never an incremental delta).
    pub released_bytes: u64,
    /// Cumulative freed frames/records since attachment.
    pub released_frames: u64,
    pub pressure: QueuePressure,
    /// Current receiver-retained bytes in this channel (bounded by protocol limits).
    pub queued_bytes: u64,
    /// Current receiver-retained items in this channel.
    pub queued_items: u32,
}

impl ReceiverPressure {
    pub fn validate(&self, limits: &ProtocolLimits) -> Result<(), WireError> {
        if self.channel_binding == 0 || self.credit_epoch == 0 {
            return Err(WireError::InvalidBinding);
        }
        if (self.released_bytes == 0) != (self.released_frames == 0) {
            return Err(WireError::InvalidValue);
        }
        if (self.queued_bytes == 0) != (self.queued_items == 0) {
            return Err(WireError::InvalidValue);
        }
        if self.queued_bytes > limits.per_viewer_compressed_bytes()
            || self.queued_items > limits.max_fragments_per_access_unit()
        {
            return Err(WireError::ResourceLimit);
        }
        Ok(())
    }
}

fn parent_valid(binding: ControlBinding) -> Result<(), WireError> {
    if binding.id == 0
        || binding.host_boot.as_raw() == 0
        || binding.os_session.as_raw() == 0
        || binding.remote_session.as_raw() == 0
    {
        return Err(WireError::InvalidBinding);
    }
    Ok(())
}

fn direction_byte(direction: InputDirection) -> u8 {
    match direction {
        InputDirection::ViewerToHost => 1,
        InputDirection::HostToViewer => 2,
    }
}

fn parse_direction(v: u8) -> Result<InputDirection, WireError> {
    match v {
        1 => Ok(InputDirection::ViewerToHost),
        2 => Ok(InputDirection::HostToViewer),
        _ => Err(WireError::InvalidValue),
    }
}

fn read_u128(r: &mut Reader<'_>) -> Result<u128, WireError> {
    Ok(u128::from_be_bytes(
        r.take(16)?.try_into().map_err(|_| WireError::Truncated)?,
    ))
}

pub fn encode(
    report: &ReceiverPressure,
    binding: ControlBinding,
    limits: &ProtocolLimits,
    out: &mut [u8],
    direction: InputDirection,
    delivery: InputDelivery,
) -> Result<usize, WireError> {
    parent_valid(binding)?;
    if delivery != InputDelivery::Reliable {
        return Err(WireError::WrongChannel);
    }
    if !report.channel_role.admits_report_direction(direction) {
        return Err(WireError::WrongRole);
    }
    report.validate(limits)?;
    let mut w = Writer::record_bounded(
        out,
        limits.max_control_message_bytes() as usize,
        binding.id,
        Kind::ReceiverPressure,
        RECORD_BYTES - HEADER_BYTES,
    )?;
    for id in [
        binding.host_boot.as_raw(),
        binding.os_session.as_raw(),
        binding.remote_session.as_raw(),
    ] {
        w.put(&id.to_be_bytes())?;
    }
    w.u32(report.channel_binding)?;
    w.u8(report.channel_role as u8)?;
    w.u8(direction_byte(direction))?;
    w.u64(report.credit_epoch)?;
    w.u64(report.released_bytes)?;
    w.u64(report.released_frames)?;
    w.u8(report.pressure as u8)?;
    w.u64(report.queued_bytes)?;
    w.u32(report.queued_items)?;
    w.finish()
}

pub fn decode(
    bytes: &[u8],
    binding: ControlBinding,
    limits: &ProtocolLimits,
    direction: InputDirection,
    delivery: InputDelivery,
) -> Result<ReceiverPressure, WireError> {
    parent_valid(binding)?;
    if delivery != InputDelivery::Reliable {
        return Err(WireError::WrongChannel);
    }
    let record = Record::decode_bounded(
        bytes,
        limits.max_control_message_bytes() as usize,
        binding.id,
        None,
    )?;
    let mut r = record.reader(Kind::ReceiverPressure)?;
    for expected in [
        binding.host_boot.as_raw(),
        binding.os_session.as_raw(),
        binding.remote_session.as_raw(),
    ] {
        if read_u128(&mut r)? != expected {
            return Err(WireError::InvalidBinding);
        }
    }
    let channel_binding = r.u32()?;
    let channel_role = ChannelRole::parse(r.u8()?)?;
    let wire_direction = parse_direction(r.u8()?)?;
    if wire_direction != direction || !channel_role.admits_report_direction(direction) {
        return Err(WireError::WrongRole);
    }
    let report = ReceiverPressure {
        channel_binding,
        channel_role,
        credit_epoch: r.u64()?,
        released_bytes: r.u64()?,
        released_frames: r.u64()?,
        pressure: QueuePressure::parse(r.u8()?)?,
        queued_bytes: r.u64()?,
        queued_items: r.u32()?,
    };
    r.finish()?;
    report.validate(limits)?;
    Ok(report)
}
