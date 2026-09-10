//! Host-issued input tickets on the bound reliable input feedback stream.
//! Receiving these bytes grants no lease, mapping, visibility or input effect.
use crate::{
    HEADER_BYTES, Kind, Record, WireError,
    input::{InputDelivery, InputDirection},
    record::{Reader, Writer},
};
use core::fmt;
use fr_core::{
    ids::{
        CodecConfigurationGeneration, DisplayGeometryGeneration, InputLeaseId, InputTicketId,
        RecoveryGeneration, RemoteSessionId, ViewportMappingGeneration,
    },
    input::{InputCredentials, InputView},
    limits::ProtocolLimits,
};

pub const INPUT_TICKET_BYTES: usize = HEADER_BYTES + 104;
pub const MAX_TICKET_LIFETIME_US: u64 = 1_500_000;
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Ticket {
    pub credentials: InputCredentials,
    pub sequence: u64,
    pub issued_at_us: u64,
    pub expires_at_us: u64,
}
impl fmt::Debug for Ticket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("InputTicket([redacted])")
    }
}
fn role(direction: InputDirection, delivery: InputDelivery) -> Result<(), WireError> {
    if direction != InputDirection::HostToViewer {
        return Err(WireError::WrongRole);
    }
    if delivery != InputDelivery::Reliable {
        return Err(WireError::WrongChannel);
    }
    Ok(())
}
fn valid(ticket: Ticket, channel: u32) -> Result<(), WireError> {
    let c = ticket.credentials;
    if channel == 0 || c.session.as_raw() == 0 || c.lease.as_raw() == 0 || c.ticket.as_raw() == 0 {
        return Err(WireError::InvalidBinding);
    }
    if !ticket
        .expires_at_us
        .checked_sub(ticket.issued_at_us)
        .is_some_and(|n| (1..=MAX_TICKET_LIFETIME_US).contains(&n))
    {
        return Err(WireError::InvalidValue);
    }
    Ok(())
}
fn id(r: &mut Reader<'_>) -> Result<u128, WireError> {
    Ok(u128::from_be_bytes(
        r.take(16)?.try_into().map_err(|_| WireError::Truncated)?,
    ))
}
pub fn encode(
    ticket: Ticket,
    out: &mut [u8],
    limits: &ProtocolLimits,
    channel: u32,
    direction: InputDirection,
    delivery: InputDelivery,
) -> Result<usize, WireError> {
    role(direction, delivery)?;
    valid(ticket, channel)?;
    let mut w = Writer::record_bounded(
        out,
        limits.max_control_message_bytes() as usize,
        channel,
        Kind::InputTicket,
        INPUT_TICKET_BYTES - HEADER_BYTES,
    )?;
    let c = ticket.credentials;
    w.put(&c.session.as_raw().to_be_bytes())?;
    w.put(&c.lease.as_raw().to_be_bytes())?;
    w.put(&c.ticket.as_raw().to_be_bytes())?;
    for n in [
        c.view.geometry.as_raw(),
        c.view.viewport.as_raw(),
        c.view.configuration.as_raw(),
        c.view.recovery.as_raw(),
        ticket.sequence,
        ticket.issued_at_us,
        ticket.expires_at_us,
    ] {
        w.u64(n)?;
    }
    w.finish()
}
pub fn decode(
    bytes: &[u8],
    limits: &ProtocolLimits,
    channel: u32,
    direction: InputDirection,
    delivery: InputDelivery,
) -> Result<Ticket, WireError> {
    role(direction, delivery)?;
    let record = Record::decode_bounded(
        bytes,
        limits.max_control_message_bytes() as usize,
        channel,
        None,
    )?;
    let mut r = record.reader(Kind::InputTicket)?;
    let ticket = Ticket {
        credentials: InputCredentials {
            session: RemoteSessionId::from_raw(id(&mut r)?),
            lease: InputLeaseId::from_raw(id(&mut r)?),
            ticket: InputTicketId::from_raw(id(&mut r)?),
            view: InputView {
                geometry: DisplayGeometryGeneration::from_raw(r.u64()?),
                viewport: ViewportMappingGeneration::from_raw(r.u64()?),
                configuration: CodecConfigurationGeneration::from_raw(r.u64()?),
                recovery: RecoveryGeneration::from_raw(r.u64()?),
            },
        },
        sequence: r.u64()?,
        issued_at_us: r.u64()?,
        expires_at_us: r.u64()?,
    };
    r.finish()?;
    valid(ticket, channel)?;
    Ok(ticket)
}
