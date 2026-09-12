//! Negotiated decoder-service metrics, not flow-control credit or visibility.
//! A query/response binds sampling to the sender's original deadline without
//! comparing clocks. The runtime must measure Load from its real receiver owner.
use crate::{
    HEADER_BYTES, Kind, Record, WireError,
    decoder::{self, Binding},
    input::{InputDelivery, InputDirection},
    record::Writer,
};
use fr_core::limits::ProtocolLimits;

pub const CAPABILITY: &str = "decoder-metrics";
pub const VERSION: u16 = 1;
pub const QUERY_BYTES: usize = HEADER_BYTES + decoder::BINDING_BYTES + 10;
pub const REPLY_BYTES: usize = QUERY_BYTES + 22;
pub const MAX_WORK_US: u64 = 5_000_000;

/// Content-free receiver counters. `work_us` includes queued/executing/uncollected
/// decoder service, never just codec CPU time. Missing evidence stays unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Load {
    pub retained_bytes: u64,
    pub retained_pictures: u32,
    pub decoding: bool,
    pub work_us: Option<u64>,
}
impl Load {
    pub fn validate(self, limits: &ProtocolLimits) -> Result<(), WireError> {
        if self.retained_bytes > limits.per_viewer_compressed_bytes()
            || self.retained_pictures > u32::from(limits.reassembly_window_pictures())
            || ((self.retained_bytes == 0) != (self.retained_pictures == 0))
            || (self.decoding && (self.retained_pictures == 0 || self.work_us.is_none()))
            || self.work_us.is_some_and(|n| n > MAX_WORK_US)
        {
            return Err(WireError::InvalidValue);
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Message {
    Query { sequence: u64 },
    Reply { sequence: u64, load: Load },
}
impl Message {
    pub const fn sequence(self) -> u64 {
        match self {
            Self::Query { sequence } | Self::Reply { sequence, .. } => sequence,
        }
    }
    const fn tag(self) -> u8 {
        match self {
            Self::Query { .. } => 0,
            Self::Reply { .. } => 1,
        }
    }
}
fn role(tag: u8, direction: InputDirection, delivery: InputDelivery) -> Result<(), WireError> {
    if delivery != InputDelivery::Reliable {
        return Err(WireError::WrongChannel);
    }
    let expected = match tag {
        0 => InputDirection::HostToViewer,
        1 => InputDirection::ViewerToHost,
        _ => return Err(WireError::InvalidValue),
    };
    if direction != expected {
        return Err(WireError::WrongRole);
    }
    Ok(())
}
pub fn encode(
    message: Message,
    binding: Binding,
    limits: &ProtocolLimits,
    out: &mut [u8],
    direction: InputDirection,
    delivery: InputDelivery,
) -> Result<usize, WireError> {
    binding.validate()?;
    role(message.tag(), direction, delivery)?;
    if message.sequence() == 0 {
        return Err(WireError::InvalidValue);
    }
    let size = match message {
        Message::Query { .. } => QUERY_BYTES,
        Message::Reply { load, .. } => {
            load.validate(limits)?;
            REPLY_BYTES
        }
    };
    let mut w = Writer::record_bounded(
        out,
        limits.max_control_message_bytes() as usize,
        binding.parent.id,
        Kind::StageMetrics,
        size - HEADER_BYTES,
    )?;
    decoder::write_binding(&mut w, binding)?;
    w.u8(1)?; // StageMetrics subtype: decoder service v1, not generic telemetry.
    w.u8(message.tag())?;
    w.u64(message.sequence())?;
    if let Message::Reply { load, .. } = message {
        w.u64(load.retained_bytes)?;
        w.u32(load.retained_pictures)?;
        w.u8(u8::from(load.decoding))?;
        w.u8(u8::from(load.work_us.is_some()))?;
        w.u64(load.work_us.unwrap_or(0))?;
    }
    w.finish()
}
pub fn decode(
    bytes: &[u8],
    binding: Binding,
    limits: &ProtocolLimits,
    direction: InputDirection,
    delivery: InputDelivery,
) -> Result<Message, WireError> {
    binding.validate()?;
    let record = Record::decode_bounded(
        bytes,
        limits.max_control_message_bytes() as usize,
        binding.parent.id,
        None,
    )?;
    let mut r = record.reader(Kind::StageMetrics)?;
    decoder::check_binding(&mut r, binding)?;
    if r.u8()? != 1 {
        return Err(WireError::UnsupportedVersion);
    }
    let tag = r.u8()?;
    role(tag, direction, delivery)?;
    let sequence = r.u64()?;
    if sequence == 0 {
        return Err(WireError::InvalidValue);
    }
    let message = if tag == 0 {
        Message::Query { sequence }
    } else {
        let retained_bytes = r.u64()?;
        let retained_pictures = r.u32()?;
        let decoding = match r.u8()? {
            0 => false,
            1 => true,
            _ => return Err(WireError::InvalidValue),
        };
        let known = r.u8()?;
        let work = r.u64()?;
        let work_us = match (known, work) {
            (0, 0) => None,
            (1, n) => Some(n),
            _ => return Err(WireError::InvalidValue),
        };
        let load = Load {
            retained_bytes,
            retained_pictures,
            decoding,
            work_us,
        };
        load.validate(limits)?;
        Message::Reply { sequence, load }
    };
    r.finish()?;
    Ok(message)
}
