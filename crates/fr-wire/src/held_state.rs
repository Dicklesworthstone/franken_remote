//! Fixed, allocation-free release-only held-state record, on the ordered input
//! stream. No ticket or fresh lifetime is implied by a reconciliation snapshot.
use crate::{
    HEADER_BYTES, Kind, Record, WireError,
    input::{InputDelivery, InputDirection},
    record::{Reader, Writer},
};
use fr_core::{
    held_state::{HeldState, HeldStateRequest, KEY_BITMAP_BYTES},
    ids::{InputLeaseId, RemoteSessionId},
    limits::ProtocolLimits,
};

pub const HELD_STATE_BYTES: usize = HEADER_BYTES + 16 + 16 + 8 + 8 + KEY_BITMAP_BYTES + 1;
fn role(direction: InputDirection, delivery: InputDelivery) -> Result<(), WireError> {
    if direction != InputDirection::ViewerToHost {
        return Err(WireError::WrongRole);
    }
    if delivery != InputDelivery::Reliable {
        return Err(WireError::WrongChannel);
    }
    Ok(())
}
fn valid(request: HeldStateRequest, binding: u32) -> Result<(), WireError> {
    if binding == 0 || request.session.as_raw() == 0 || request.lease.as_raw() == 0 {
        return Err(WireError::InvalidBinding);
    }
    Ok(())
}
fn id(reader: &mut Reader<'_>) -> Result<u128, WireError> {
    Ok(u128::from_be_bytes(
        reader
            .take(16)?
            .try_into()
            .map_err(|_| WireError::Truncated)?,
    ))
}
pub fn encode(
    request: HeldStateRequest,
    out: &mut [u8],
    limits: &ProtocolLimits,
    binding: u32,
    direction: InputDirection,
    delivery: InputDelivery,
) -> Result<usize, WireError> {
    role(direction, delivery)?;
    valid(request, binding)?;
    let mut w = Writer::record_bounded(
        out,
        limits.max_control_message_bytes() as usize,
        binding,
        Kind::HeldState,
        HELD_STATE_BYTES - HEADER_BYTES,
    )?;
    w.put(&request.session.as_raw().to_be_bytes())?;
    w.put(&request.lease.as_raw().to_be_bytes())?;
    w.u64(request.sequence)?;
    w.u64(request.next_action)?;
    w.put(request.held.key_bits())?;
    w.u8(request.held.button_bits())?;
    w.finish()
}
pub fn decode(
    bytes: &[u8],
    limits: &ProtocolLimits,
    binding: u32,
    direction: InputDirection,
    delivery: InputDelivery,
) -> Result<HeldStateRequest, WireError> {
    role(direction, delivery)?;
    let record = Record::decode_bounded(
        bytes,
        limits.max_control_message_bytes() as usize,
        binding,
        None,
    )?;
    let mut r = record.reader(Kind::HeldState)?;
    let session = RemoteSessionId::from_raw(id(&mut r)?);
    let lease = InputLeaseId::from_raw(id(&mut r)?);
    let sequence = r.u64()?;
    let next_action = r.u64()?;
    let keys = r
        .take(KEY_BITMAP_BYTES)?
        .try_into()
        .map_err(|_| WireError::Truncated)?;
    let held = HeldState::from_bits(keys, r.u8()?).ok_or(WireError::InvalidValue)?;
    r.finish()?;
    let request = HeldStateRequest {
        session,
        lease,
        sequence,
        next_action,
        held,
    };
    valid(request, binding)?;
    Ok(request)
}
