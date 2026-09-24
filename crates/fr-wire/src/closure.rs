//! Terminal session-close requests on the original reliable control stream.
//! Parsing does not close anything or authenticate a peer. The authority owner
//! must fence the matched session before dispatching another application record.
//! See `PROTOCOL_CLOSURE.md` for the independent byte contract and scope limits.
use crate::{
    HEADER_BYTES, Kind, Record, WireError,
    authority::Binding,
    input::{InputDelivery, InputDirection},
    record::Writer,
};
use fr_core::limits::ProtocolLimits;

pub const REQUEST_BYTES: usize = HEADER_BYTES + 19;

/// Stable content-free v0 reasons, not free-form peer diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum Reason {
    Requested = 1,
    ClientStopping = 2,
    ClientFailure = 3,
    InspectionComplete = 4,
}
impl Reason {
    fn parse(value: u16) -> Result<Self, WireError> {
        Ok(match value {
            1 => Self::Requested,
            2 => Self::ClientStopping,
            3 => Self::ClientFailure,
            4 => Self::InspectionComplete,
            _ => return Err(WireError::InvalidValue),
        })
    }
}

/// Ends this entire remote session, not the OS share or another viewer. The
/// control-release-only scope remains explicitly unsupported by this slice.
/// This request carries no claim about committed input or completed cleanup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CloseRequest {
    pub reason: Reason,
}
fn route(
    binding: Binding,
    direction: InputDirection,
    delivery: InputDelivery,
) -> Result<(), WireError> {
    if direction != InputDirection::ViewerToHost {
        return Err(WireError::WrongRole);
    }
    if delivery != InputDelivery::Reliable {
        return Err(WireError::WrongChannel);
    }
    if binding.channel == 0 || binding.session.as_raw() == 0 {
        return Err(WireError::InvalidBinding);
    }
    Ok(())
}
pub fn encode_request(
    request: CloseRequest,
    binding: Binding,
    limits: &ProtocolLimits,
    out: &mut [u8],
    direction: InputDirection,
    delivery: InputDelivery,
) -> Result<usize, WireError> {
    route(binding, direction, delivery)?;
    let mut writer = Writer::record_bounded(
        out,
        limits.max_control_message_bytes() as usize,
        binding.channel,
        Kind::CloseRequest,
        19,
    )?;
    writer.put(&binding.session.as_raw().to_be_bytes())?;
    writer.u8(1)?; // Session close. Zero is control-release-only, not implemented.
    writer.u16(request.reason as u16)?;
    writer.finish()
}
pub fn decode_request(
    bytes: &[u8],
    binding: Binding,
    limits: &ProtocolLimits,
    direction: InputDirection,
    delivery: InputDelivery,
) -> Result<CloseRequest, WireError> {
    route(binding, direction, delivery)?;
    let record = Record::decode_bounded(
        bytes,
        limits.max_control_message_bytes() as usize,
        binding.channel,
        None,
    )?;
    let mut reader = record.reader(Kind::CloseRequest)?;
    if reader.take(16)? != binding.session.as_raw().to_be_bytes() {
        return Err(WireError::InvalidBinding);
    }
    match reader.u8()? {
        1 => {}
        0 => return Err(WireError::UnsupportedKind),
        _ => return Err(WireError::InvalidValue),
    }
    let reason = Reason::parse(reader.u16()?)?;
    reader.finish()?;
    Ok(CloseRequest { reason })
}
