//! Initial control intent is not permission. The host grants only its exact
//! locally approved target after reserving the OS seat and initializing input.
use crate::{
    HEADER_BYTES, Kind, Record, WireError,
    input::{InputDelivery, InputDirection},
    input_ticket::{MAX_TICKET_LIFETIME_US, Ticket},
    negotiation::ControlBinding,
    record::{Reader, Writer},
};
use core::fmt;
use fr_core::{
    ids::{
        CodecConfigurationGeneration, DisplayGeometryGeneration, HostBootId, InputLeaseId,
        InputTicketId, OsSessionId, RecoveryGeneration, RemoteSessionId, ViewportMappingGeneration,
    },
    input::{DesktopPoint, InputBounds, InputCredentials, InputView},
    input_submission::{Capabilities, Capability},
    limits::ProtocolLimits,
};

pub const REQUEST_BYTES: usize = HEADER_BYTES + 109;
pub const GRANTED_BYTES: usize = REQUEST_BYTES + 76;
pub const MAX_LEASE_LIFETIME_US: u64 = 3_000_000;
const CAPS: [Capability; 8] = [
    Capability::Keys,
    Capability::Repeat,
    Capability::Absolute,
    Capability::Buttons,
    Capability::Relative,
    Capability::PixelScroll,
    Capability::LineScroll,
    Capability::Text,
];

/// The display binding, exact mapped view, desktop bounds and required native
/// operations. The host compares all fields to LOCAL permission, never clamps
/// a peer target or grants an unrequested capability.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Target {
    pub display_binding: u32,
    pub view: InputView,
    pub bounds: InputBounds,
    pub capabilities: Capabilities,
}
impl fmt::Debug for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ControlTarget([redacted])")
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Request {
    pub parent: ControlBinding,
    pub sequence: u64,
    pub target: Target,
}
impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ControlRequest([redacted])")
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Granted {
    pub request: Request,
    pub input_channel: u32,
    pub lease: InputLeaseId,
    pub ticket: InputTicketId,
    pub issued_at_us: u64,
    pub lease_until_us: u64,
    pub ticket_until_us: u64,
    /// This profile creates a fresh replay ledger, beginning at zero.
    pub first_action: u64,
    pub first_pointer: u64,
}
impl fmt::Debug for Granted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LeaseGranted([redacted])")
    }
}
impl Granted {
    pub const fn credentials(self) -> InputCredentials {
        InputCredentials {
            session: self.request.parent.remote_session,
            lease: self.lease,
            ticket: self.ticket,
            view: self.request.target.view,
        }
    }
    /// The initial credential occupies ticket sequence zero. The native QUIC
    /// attachment continues at one; neither input sequence is consumed.
    pub const fn initial_ticket(self) -> Ticket {
        Ticket {
            credentials: self.credentials(),
            sequence: 0,
            issued_at_us: self.issued_at_us,
            expires_at_us: self.ticket_until_us,
        }
    }
}
fn valid_request(request: Request, limits: &ProtocolLimits) -> Result<(), WireError> {
    let p = request.parent;
    let t = request.target;
    if p.id == 0
        || p.host_boot.as_raw() == 0
        || p.os_session.as_raw() == 0
        || p.remote_session.as_raw() == 0
        || t.display_binding == 0
        || t.display_binding == p.id
    {
        return Err(WireError::InvalidBinding);
    }
    limits
        .validate_coded_dimensions(t.bounds.width(), t.bounds.height())
        .map_err(|_| WireError::ResourceLimit)?;
    let c = t.capabilities;
    if c == Capabilities::default()
        || (c.contains(Capability::Repeat) && !c.contains(Capability::Keys))
        || ([
            Capability::Buttons,
            Capability::PixelScroll,
            Capability::LineScroll,
        ]
        .into_iter()
        .any(|v| c.contains(v))
            && !c.contains(Capability::Absolute))
    {
        return Err(WireError::InvalidValue);
    }
    Ok(())
}
fn valid_grant(grant: Granted, limits: &ProtocolLimits) -> Result<(), WireError> {
    valid_request(grant.request, limits)?;
    if grant.input_channel == 0
        || grant.input_channel == grant.request.parent.id
        || grant.input_channel == grant.request.target.display_binding
        || grant.lease.as_raw() == 0
        || grant.ticket.as_raw() == 0
    {
        return Err(WireError::InvalidBinding);
    }
    if grant.first_action != 0
        || grant.first_pointer != 0
        || !grant
            .lease_until_us
            .checked_sub(grant.issued_at_us)
            .is_some_and(|n| (1..=MAX_LEASE_LIFETIME_US).contains(&n))
        || !grant
            .ticket_until_us
            .checked_sub(grant.issued_at_us)
            .is_some_and(|n| (1..=MAX_TICKET_LIFETIME_US).contains(&n))
        || grant.ticket_until_us > grant.lease_until_us
    {
        return Err(WireError::InvalidValue);
    }
    Ok(())
}
fn role(
    direction: InputDirection,
    wanted: InputDirection,
    delivery: InputDelivery,
) -> Result<(), WireError> {
    if direction != wanted {
        return Err(WireError::WrongRole);
    }
    if delivery != InputDelivery::Reliable {
        return Err(WireError::WrongChannel);
    }
    Ok(())
}
fn write_request(w: &mut Writer<'_>, v: Request) -> Result<(), WireError> {
    for id in [
        v.parent.host_boot.as_raw(),
        v.parent.os_session.as_raw(),
        v.parent.remote_session.as_raw(),
    ] {
        w.put(&id.to_be_bytes())?;
    }
    w.u64(v.sequence)?;
    w.u32(v.target.display_binding)?;
    let view = v.target.view;
    for n in [
        view.geometry.as_raw(),
        view.viewport.as_raw(),
        view.configuration.as_raw(),
        view.recovery.as_raw(),
    ] {
        w.u64(n)?;
    }
    let b = v.target.bounds;
    w.put(&b.origin().x.to_be_bytes())?;
    w.put(&b.origin().y.to_be_bytes())?;
    w.u32(b.width())?;
    w.u32(b.height())?;
    let bits = CAPS.into_iter().fold(0_u8, |bits, cap| {
        bits | (u8::from(v.target.capabilities.contains(cap)) << cap as u8)
    });
    w.u8(bits)
}
fn id(r: &mut Reader<'_>) -> Result<u128, WireError> {
    Ok(u128::from_be_bytes(
        r.take(16)?.try_into().map_err(|_| WireError::Truncated)?,
    ))
}
fn coordinate(r: &mut Reader<'_>) -> Result<i32, WireError> {
    Ok(i32::from_be_bytes(
        r.take(4)?.try_into().map_err(|_| WireError::Truncated)?,
    ))
}
fn read_request(r: &mut Reader<'_>, channel: u32) -> Result<Request, WireError> {
    let parent = ControlBinding {
        id: channel,
        host_boot: HostBootId::from_raw(id(r)?),
        os_session: OsSessionId::from_raw(id(r)?),
        remote_session: RemoteSessionId::from_raw(id(r)?),
    };
    let sequence = r.u64()?;
    let display_binding = r.u32()?;
    let view = InputView {
        geometry: DisplayGeometryGeneration::from_raw(r.u64()?),
        viewport: ViewportMappingGeneration::from_raw(r.u64()?),
        configuration: CodecConfigurationGeneration::from_raw(r.u64()?),
        recovery: RecoveryGeneration::from_raw(r.u64()?),
    };
    let origin = DesktopPoint {
        x: coordinate(r)?,
        y: coordinate(r)?,
    };
    let bounds = InputBounds::new(origin, r.u32()?, r.u32()?).ok_or(WireError::InvalidValue)?;
    let bits = r.u8()?;
    let capabilities = CAPS
        .into_iter()
        .filter(|c| bits & (1 << *c as u8) != 0)
        .fold(Capabilities::default(), Capabilities::with);
    Ok(Request {
        parent,
        sequence,
        target: Target {
            display_binding,
            view,
            bounds,
            capabilities,
        },
    })
}
pub fn encode_request(
    request: Request,
    out: &mut [u8],
    limits: &ProtocolLimits,
    direction: InputDirection,
    delivery: InputDelivery,
) -> Result<usize, WireError> {
    role(direction, InputDirection::ViewerToHost, delivery)?;
    valid_request(request, limits)?;
    let mut w = Writer::record_bounded(
        out,
        limits.max_control_message_bytes() as usize,
        request.parent.id,
        Kind::ControlRequest,
        REQUEST_BYTES - HEADER_BYTES,
    )?;
    write_request(&mut w, request)?;
    w.finish()
}
pub fn decode_request(
    bytes: &[u8],
    expected: ControlBinding,
    limits: &ProtocolLimits,
    direction: InputDirection,
    delivery: InputDelivery,
) -> Result<Request, WireError> {
    role(direction, InputDirection::ViewerToHost, delivery)?;
    let record = Record::decode_bounded(
        bytes,
        limits.max_control_message_bytes() as usize,
        expected.id,
        None,
    )?;
    let mut r = record.reader(Kind::ControlRequest)?;
    let request = read_request(&mut r, expected.id)?;
    r.finish()?;
    valid_request(request, limits)?;
    if request.parent != expected {
        return Err(WireError::InvalidBinding);
    }
    Ok(request)
}
pub fn encode_granted(
    grant: Granted,
    out: &mut [u8],
    limits: &ProtocolLimits,
    direction: InputDirection,
    delivery: InputDelivery,
) -> Result<usize, WireError> {
    role(direction, InputDirection::HostToViewer, delivery)?;
    valid_grant(grant, limits)?;
    let mut w = Writer::record_bounded(
        out,
        limits.max_control_message_bytes() as usize,
        grant.request.parent.id,
        Kind::LeaseGranted,
        GRANTED_BYTES - HEADER_BYTES,
    )?;
    write_request(&mut w, grant.request)?;
    w.u32(grant.input_channel)?;
    w.put(&grant.lease.as_raw().to_be_bytes())?;
    w.put(&grant.ticket.as_raw().to_be_bytes())?;
    for n in [
        grant.issued_at_us,
        grant.lease_until_us,
        grant.ticket_until_us,
        grant.first_action,
        grant.first_pointer,
    ] {
        w.u64(n)?;
    }
    w.finish()
}
pub fn decode_granted(
    bytes: &[u8],
    expected: ControlBinding,
    limits: &ProtocolLimits,
    direction: InputDirection,
    delivery: InputDelivery,
) -> Result<Granted, WireError> {
    role(direction, InputDirection::HostToViewer, delivery)?;
    let record = Record::decode_bounded(
        bytes,
        limits.max_control_message_bytes() as usize,
        expected.id,
        None,
    )?;
    let mut r = record.reader(Kind::LeaseGranted)?;
    let grant = Granted {
        request: read_request(&mut r, expected.id)?,
        input_channel: r.u32()?,
        lease: InputLeaseId::from_raw(id(&mut r)?),
        ticket: InputTicketId::from_raw(id(&mut r)?),
        issued_at_us: r.u64()?,
        lease_until_us: r.u64()?,
        ticket_until_us: r.u64()?,
        first_action: r.u64()?,
        first_pointer: r.u64()?,
    };
    r.finish()?;
    valid_grant(grant, limits)?;
    if grant.request.parent != expected {
        return Err(WireError::InvalidBinding);
    }
    Ok(grant)
}
