//! Correlated clock probes on an admitted session control pair. Client time is
//! never sent or trusted by the host. Sampling is not renewal or source evidence.
use crate::{
    HEADER_BYTES, Kind, Record, WireError,
    input::{InputDelivery, InputDirection},
    negotiation::ControlBinding,
    record::{Reader, Writer},
};
use core::fmt;
use fr_core::limits::ProtocolLimits;

pub const CAPABILITY: &str = "clock-correlation";
pub const VERSION: u16 = 1;
pub const PROBE_BYTES: usize = HEADER_BYTES + 56;
pub const REPLY_BYTES: usize = PROBE_BYTES + 8;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Message {
    Probe { sequence: u64 },
    Reply { sequence: u64, host_sample_us: u64 },
}
impl fmt::Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Probe { .. } => "ClockProbe",
            Self::Reply { .. } => "ClockReply",
        })
    }
}
impl Message {
    pub const fn sequence(self) -> u64 {
        match self {
            Self::Probe { sequence } | Self::Reply { sequence, .. } => sequence,
        }
    }
    pub const fn kind(self) -> Kind {
        match self {
            Self::Probe { .. } => Kind::ClockProbe,
            Self::Reply { .. } => Kind::ClockReply,
        }
    }
}
fn validate(binding: ControlBinding, sequence: u64) -> Result<(), WireError> {
    if binding.id == 0
        || binding.host_boot.as_raw() == 0
        || binding.os_session.as_raw() == 0
        || binding.remote_session.as_raw() == 0
    {
        return Err(WireError::InvalidBinding);
    }
    if sequence == 0 {
        return Err(WireError::InvalidValue);
    }
    Ok(())
}
fn role(kind: Kind, direction: InputDirection, delivery: InputDelivery) -> Result<(), WireError> {
    if delivery != InputDelivery::Reliable {
        return Err(WireError::WrongChannel);
    }
    let expected = match kind {
        Kind::ClockProbe => InputDirection::ViewerToHost,
        Kind::ClockReply => InputDirection::HostToViewer,
        _ => return Err(WireError::UnsupportedKind),
    };
    if direction != expected {
        return Err(WireError::WrongRole);
    }
    Ok(())
}
fn id(r: &mut Reader<'_>) -> Result<u128, WireError> {
    Ok(u128::from_be_bytes(
        r.take(16)?.try_into().map_err(|_| WireError::Truncated)?,
    ))
}
pub fn encode(
    message: Message,
    binding: ControlBinding,
    limits: &ProtocolLimits,
    out: &mut [u8],
    direction: InputDirection,
    delivery: InputDelivery,
) -> Result<usize, WireError> {
    validate(binding, message.sequence())?;
    role(message.kind(), direction, delivery)?;
    let payload = match message {
        Message::Probe { .. } => PROBE_BYTES - HEADER_BYTES,
        Message::Reply { .. } => REPLY_BYTES - HEADER_BYTES,
    };
    let mut w = Writer::record_bounded(
        out,
        limits.max_control_message_bytes() as usize,
        binding.id,
        message.kind(),
        payload,
    )?;
    w.put(&binding.host_boot.as_raw().to_be_bytes())?;
    w.put(&binding.os_session.as_raw().to_be_bytes())?;
    w.put(&binding.remote_session.as_raw().to_be_bytes())?;
    w.u64(message.sequence())?;
    if let Message::Reply { host_sample_us, .. } = message {
        w.u64(host_sample_us)?;
    }
    w.finish()
}
pub fn decode(
    bytes: &[u8],
    binding: ControlBinding,
    limits: &ProtocolLimits,
    direction: InputDirection,
    delivery: InputDelivery,
) -> Result<Message, WireError> {
    validate(binding, 1)?;
    let record = Record::decode_bounded(
        bytes,
        limits.max_control_message_bytes() as usize,
        binding.id,
        None,
    )?;
    role(record.kind(), direction, delivery)?;
    let mut r = record.reader(record.kind())?;
    if id(&mut r)? != binding.host_boot.as_raw()
        || id(&mut r)? != binding.os_session.as_raw()
        || id(&mut r)? != binding.remote_session.as_raw()
    {
        return Err(WireError::InvalidBinding);
    }
    let sequence = r.u64()?;
    let result = if record.kind() == Kind::ClockProbe {
        Message::Probe { sequence }
    } else {
        Message::Reply {
            sequence,
            host_sample_us: r.u64()?,
        }
    };
    r.finish()?;
    validate(binding, sequence)?;
    Ok(result)
}
