//! Bound viewer-to-host requests to abandon a failed reference chain.
//! This is not a keyframe command or a grant: the host admits recovery for this
//! subscription, installs fresh bindings, and coalesces any shared IDR work.
use crate::{
    HEADER_BYTES, Kind, Record, WireError,
    decoder::{self, Binding},
    input::{InputDelivery, InputDirection},
    record::Writer,
};
use fr_core::limits::ProtocolLimits;

pub const CAPABILITY: &str = "reference-recovery";
pub const VERSION: u16 = 1;
/// Fixed size without header extensions; unknown last-useful frame is explicit.
pub const REQUEST_BYTES: usize = HEADER_BYTES + decoder::BINDING_BYTES + 11;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Reason {
    ReferenceExpired = 1,
    RecoveryExpired = 2,
    DecodeFailed = 3,
}
impl Reason {
    fn parse(value: u8) -> Result<Self, WireError> {
        match value {
            1 => Ok(Self::ReferenceExpired),
            2 => Ok(Self::RecoveryExpired),
            3 => Ok(Self::DecodeFailed),
            _ => Err(WireError::InvalidValue),
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Request {
    pub reason: Reason,
    /// Successful decode, not receipt, compositor submission or visible output.
    pub last_useful_frame: Option<u64>,
}
fn role(direction: InputDirection, delivery: InputDelivery) -> Result<(), WireError> {
    if delivery != InputDelivery::Reliable {
        return Err(WireError::WrongChannel);
    }
    if direction != InputDirection::ViewerToHost {
        return Err(WireError::WrongRole);
    }
    Ok(())
}
pub fn encode(
    request: Request,
    binding: Binding,
    limits: &ProtocolLimits,
    out: &mut [u8],
    direction: InputDirection,
    delivery: InputDelivery,
) -> Result<usize, WireError> {
    binding.validate()?;
    role(direction, delivery)?;
    let mut w = Writer::record_bounded(
        out,
        limits.max_control_message_bytes() as usize,
        binding.parent.id,
        Kind::RecoveryRequest,
        REQUEST_BYTES - HEADER_BYTES,
    )?;
    decoder::write_binding(&mut w, binding)?;
    w.u8(1)?;
    w.u8(request.reason as u8)?;
    w.u8(u8::from(request.last_useful_frame.is_some()))?;
    w.u64(request.last_useful_frame.unwrap_or(0))?;
    w.finish()
}
pub fn decode(
    bytes: &[u8],
    binding: Binding,
    limits: &ProtocolLimits,
    direction: InputDirection,
    delivery: InputDelivery,
) -> Result<Request, WireError> {
    binding.validate()?;
    role(direction, delivery)?;
    let record = Record::decode_bounded(
        bytes,
        limits.max_control_message_bytes() as usize,
        binding.parent.id,
        None,
    )?;
    let mut r = record.reader(Kind::RecoveryRequest)?;
    decoder::check_binding(&mut r, binding)?;
    if r.u8()? != 1 {
        return Err(WireError::UnsupportedVersion);
    }
    let reason = Reason::parse(r.u8()?)?;
    let known = r.u8()?;
    let frame = r.u64()?;
    let last_useful_frame = match (known, frame) {
        (0, 0) => None,
        (1, value) => Some(value),
        _ => return Err(WireError::InvalidValue),
    };
    r.finish()?;
    Ok(Request {
        reason,
        last_useful_frame,
    })
}
