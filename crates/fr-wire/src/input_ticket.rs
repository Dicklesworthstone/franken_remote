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
/// Optional acknowledgement of an actually applied presentation report. This
/// depends on the authority profile and never replaces fresh local visibility.
pub const READINESS_CAPABILITY: &str = "input-ticket-readiness";
pub const READINESS_VERSION: u16 = 1;
pub const READINESS_TICKET_BYTES: usize = AUTHORITY_TICKET_BYTES + 8;

/// The exact feedback layout selected for the original session. Received
/// lengths never select a profile or permit retry through another decoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Legacy,
    Authority,
    Readiness,
}
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

/// The host's latest successfully applied positive presentation report,
/// sampled with the ticket and existing authority cap. The report sequence is
/// nonzero and scoped to this ticket's view/recovery generation. A client must
/// additionally relate it to its own admitted reports and current suspension.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ReadinessTicket {
    pub authority: AuthorityTicket,
    pub presented_sequence: u64,
}
impl fmt::Debug for ReadinessTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("InputTicketReadiness([redacted])")
    }
}

/// Require a positive negotiated selection before choosing the extended codec.
/// Absence preserves the legacy profile. An explicitly unsupported version or
/// incomplete selection refuses instead of silently changing layouts.
pub fn selected_profile(selection: &Selection) -> Result<Profile, WireError> {
    selection.validate().map_err(|error| match error {
        negotiation::Error::Version | negotiation::Error::Profile => WireError::UnsupportedVersion,
        negotiation::Error::Limits => WireError::InvalidLimits,
        negotiation::Error::Wire(error) => error,
        _ => WireError::InvalidValue,
    })?;
    let authority = selection
        .capabilities
        .iter()
        .find(|capability| capability.name == AUTHORITY_CAPABILITY);
    let readiness = selection
        .capabilities
        .iter()
        .find(|capability| capability.name == READINESS_CAPABILITY);
    if authority.is_some_and(|capability| capability.version != AUTHORITY_VERSION)
        || readiness.is_some_and(|capability| capability.version != READINESS_VERSION)
    {
        return Err(WireError::UnsupportedVersion);
    }
    let profile = match (authority, readiness) {
        (None, None) => return Ok(Profile::Legacy),
        (None, Some(_)) => return Err(WireError::InvalidDependency),
        (Some(_), None) => Profile::Authority,
        (Some(_), Some(_)) => Profile::Readiness,
    };
    if selection.role != negotiation::Role::RequestControl {
        return Err(WireError::WrongRole);
    }
    let bytes = match profile {
        Profile::Readiness => READINESS_TICKET_BYTES,
        Profile::Authority | Profile::Legacy => AUTHORITY_TICKET_BYTES,
    };
    if (selection.limits.max_control_message_bytes() as usize) < bytes {
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
    Ok(profile)
}

/// Both extended profiles carry the actual authority cap. Call
/// [`selected_profile`] when choosing the exact codec.
pub fn authority_selected(selection: &Selection) -> Result<bool, WireError> {
    Ok(matches!(
        selected_profile(selection)?,
        Profile::Authority | Profile::Readiness
    ))
}

/// True only for the positively selected readiness acknowledgement profile.
pub fn readiness_selected(selection: &Selection) -> Result<bool, WireError> {
    Ok(selected_profile(selection)? == Profile::Readiness)
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
fn valid_readiness(ticket: ReadinessTicket, channel: u32) -> Result<(), WireError> {
    valid_authority(ticket.authority, channel)?;
    if ticket.presented_sequence == 0 {
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

/// Encode only for [`Profile::Authority`]. Legacy peers continue using [`encode`]
/// and its exact 128-byte layout; readiness uses [`encode_readiness`].
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
    write_authority_body(&mut w, ticket)?;
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
    let ticket = read_authority_body(&mut r)?;
    r.finish()?;
    valid_authority(ticket, channel)?;
    Ok(ticket)
}

fn write_authority_body(w: &mut Writer<'_>, ticket: AuthorityTicket) -> Result<(), WireError> {
    write_body(w, ticket.ticket)?;
    w.u64(ticket.authority_until_us)
}

fn read_authority_body(r: &mut Reader<'_>) -> Result<AuthorityTicket, WireError> {
    Ok(AuthorityTicket {
        ticket: read_body(r)?,
        authority_until_us: r.u64()?,
    })
}

/// Encode the positively selected [`Profile::Readiness`] layout. A host without
/// an actually applied report proof must withhold this ticket, never encode zero
/// or downgrade its selected profile. Input and queued-record expiries stay short.
pub fn encode_readiness(
    ticket: ReadinessTicket,
    out: &mut [u8],
    limits: &ProtocolLimits,
    channel: u32,
    direction: InputDirection,
    delivery: InputDelivery,
) -> Result<usize, WireError> {
    role(direction, delivery)?;
    valid_readiness(ticket, channel)?;
    let mut w = Writer::record_bounded(
        out,
        limits.max_control_message_bytes() as usize,
        channel,
        Kind::InputTicket,
        READINESS_TICKET_BYTES - HEADER_BYTES,
    )?;
    write_authority_body(&mut w, ticket.authority)?;
    w.u64(ticket.presented_sequence)?;
    w.finish()
}

/// Decode only the fixed 144-byte readiness profile. The acknowledgement does
/// not itself establish its causal relationship to the receiver's suspension;
/// the original client owner checks that against its actually admitted reports.
pub fn decode_readiness(
    bytes: &[u8],
    limits: &ProtocolLimits,
    channel: u32,
    direction: InputDirection,
    delivery: InputDelivery,
) -> Result<ReadinessTicket, WireError> {
    role(direction, delivery)?;
    if bytes.len() != READINESS_TICKET_BYTES {
        return Err(if bytes.len() < READINESS_TICKET_BYTES {
            WireError::Truncated
        } else {
            WireError::TrailingBytes
        });
    }
    let record = Record::decode_bounded(
        bytes,
        limits.max_control_message_bytes() as usize,
        channel,
        None,
    )?;
    let mut r = record.reader(Kind::InputTicket)?;
    let ticket = ReadinessTicket {
        authority: read_authority_body(&mut r)?,
        presented_sequence: r.u64()?,
    };
    r.finish()?;
    valid_readiness(ticket, channel)?;
    Ok(ticket)
}
