//! Host-issued input tickets on the bound reliable input feedback stream.
//! Receiving these bytes grants no lease, mapping, visibility or input effect.
use crate::{
    HEADER_BYTES, Kind, Record, WireError,
    input::{InputDelivery, InputDirection},
    negotiation::{self, Selection},
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
/// Optional negotiated profile carrying the original control/observation cap.
/// Selecting it grants nothing and never extends the enclosed input ticket.
pub const AUTHORITY_CAPABILITY: &str = "input-ticket-authority";
pub const AUTHORITY_VERSION: u16 = 1;
pub const AUTHORITY_TICKET_BYTES: usize = INPUT_TICKET_BYTES + 8;
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
/// The ticket and actual exclusive authority cap sampled by its original host
/// owner at issuance. This is not receipt time plus a new lease lifetime.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct AuthorityTicket {
    pub ticket: Ticket,
    pub authority_until_us: u64,
}
impl fmt::Debug for AuthorityTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("InputTicketAuthority([redacted])")
    }
}

/// Require a positive negotiated selection before choosing the extended codec.
/// Absence preserves the legacy profile. An explicitly unsupported version or
/// incomplete selection refuses instead of silently changing layouts.
pub fn authority_selected(selection: &Selection) -> Result<bool, WireError> {
    selection.validate().map_err(|error| match error {
        negotiation::Error::Version | negotiation::Error::Profile => WireError::UnsupportedVersion,
        negotiation::Error::Limits => WireError::InvalidLimits,
        negotiation::Error::Wire(error) => error,
        _ => WireError::InvalidValue,
    })?;
    let Some(authority) = selection
        .capabilities
        .iter()
        .find(|capability| capability.name == AUTHORITY_CAPABILITY)
    else {
        return Ok(false);
    };
    if authority.version != AUTHORITY_VERSION {
        return Err(WireError::UnsupportedVersion);
    }
    if selection.role != negotiation::Role::RequestControl {
        return Err(WireError::WrongRole);
    }
    if (selection.limits.max_control_message_bytes() as usize) < AUTHORITY_TICKET_BYTES {
        return Err(WireError::InvalidLimits);
    }
    if ![
        (
            crate::attachment::INPUT_CAPABILITY,
            crate::attachment::INPUT_VERSION,
        ),
        (crate::control::GRANT_CAPABILITY, 1),
        (crate::clock::CAPABILITY, crate::clock::VERSION),
        (crate::presented::CAPABILITY, crate::presented::VERSION),
    ]
    .into_iter()
    .all(|(name, version)| {
        selection
            .capabilities
            .iter()
            .any(|capability| capability.name == name && capability.version == version)
    }) {
        return Err(WireError::InvalidDependency);
    }
    Ok(true)
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
fn valid_authority(ticket: AuthorityTicket, channel: u32) -> Result<(), WireError> {
    valid(ticket.ticket, channel)?;
    if ticket.ticket.expires_at_us > ticket.authority_until_us
        || !ticket
            .authority_until_us
            .checked_sub(ticket.ticket.issued_at_us)
            .is_some_and(|lifetime| (1..=crate::control::MAX_LEASE_LIFETIME_US).contains(&lifetime))
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
    write_body(&mut w, ticket)?;
    w.finish()
}
fn write_body(w: &mut Writer<'_>, ticket: Ticket) -> Result<(), WireError> {
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
    Ok(())
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
    let ticket = read_body(&mut r)?;
    r.finish()?;
    valid(ticket, channel)?;
    Ok(ticket)
}
fn read_body(r: &mut Reader<'_>) -> Result<Ticket, WireError> {
    Ok(Ticket {
        credentials: InputCredentials {
            session: RemoteSessionId::from_raw(id(r)?),
            lease: InputLeaseId::from_raw(id(r)?),
            ticket: InputTicketId::from_raw(id(r)?),
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
    })
}

/// Encode only after [`authority_selected`] returned true for the authenticated
/// session. Legacy peers continue using [`encode`] and its exact 128-byte layout.
pub fn encode_authority(
    ticket: AuthorityTicket,
    out: &mut [u8],
    limits: &ProtocolLimits,
    channel: u32,
    direction: InputDirection,
    delivery: InputDelivery,
) -> Result<usize, WireError> {
    role(direction, delivery)?;
    valid_authority(ticket, channel)?;
    let mut w = Writer::record_bounded(
        out,
        limits.max_control_message_bytes() as usize,
        channel,
        Kind::InputTicket,
        AUTHORITY_TICKET_BYTES - HEADER_BYTES,
    )?;
    write_body(&mut w, ticket.ticket)?;
    w.u64(ticket.authority_until_us)?;
    w.finish()
}

/// Decode the positively selected extended profile. Legacy records refuse;
/// callers must not infer a profile or retry another codec from received length.
pub fn decode_authority(
    bytes: &[u8],
    limits: &ProtocolLimits,
    channel: u32,
    direction: InputDirection,
    delivery: InputDelivery,
) -> Result<AuthorityTicket, WireError> {
    role(direction, delivery)?;
    let record = Record::decode_bounded(
        bytes,
        limits.max_control_message_bytes() as usize,
        channel,
        None,
    )?;
    let mut r = record.reader(Kind::InputTicket)?;
    let ticket = AuthorityTicket {
        ticket: read_body(&mut r)?,
        authority_until_us: r.u64()?,
    };
    r.finish()?;
    valid_authority(ticket, channel)?;
    Ok(ticket)
}
