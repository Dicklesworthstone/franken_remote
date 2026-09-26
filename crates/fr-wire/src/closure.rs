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

/// Fixed session-terminal report, including the ordinary FRD0 header.
pub const CLOSED_BYTES: usize = HEADER_BYTES + 28;

/// Final session reason. Values are independent of `CloseRequest`'s reason codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum ClosedReason {
    ClientRequested = 1,
    HostStopping = 2,
    AuthorityExpired = 3,
    PermissionLost = 4,
    ViewInvalidated = 5,
    ProtocolError = 6,
    HostFailure = 7,
    SessionReplaced = 8,
}
impl ClosedReason {
    fn parse(value: u16) -> Result<Self, WireError> {
        Ok(match value {
            1 => Self::ClientRequested,
            2 => Self::HostStopping,
            3 => Self::AuthorityExpired,
            4 => Self::PermissionLost,
            5 => Self::ViewInvalidated,
            6 => Self::ProtocolError,
            7 => Self::HostFailure,
            8 => Self::SessionReplaced,
            _ => return Err(WireError::InvalidValue),
        })
    }
    pub const fn code(self) -> &'static str {
        match self {
            Self::ClientRequested => "client_requested",
            Self::HostStopping => "host_stopping",
            Self::AuthorityExpired => "authority_expired",
            Self::PermissionLost => "permission_lost",
            Self::ViewInvalidated => "view_invalidated",
            Self::ProtocolError => "protocol_error",
            Self::HostFailure => "host_failure",
            Self::SessionReplaced => "session_replaced",
        }
    }
}

/// Authority MUST already be fenced in every case. Completion is about this
/// session's cleanup, not another viewer's shared source or rollback of input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Cleanup {
    /// Ordered teardown has fenced authority; native cleanup is not confirmed.
    Unconfirmed = 1,
    /// All cleanup owed by this session has actually completed.
    Complete = 2,
    /// A cleanup failure is known; remaining native ownership is still retained.
    Incomplete = 3,
}
impl Cleanup {
    fn parse(value: u8) -> Result<Self, WireError> {
        match value {
            1 => Ok(Self::Unconfirmed),
            2 => Ok(Self::Complete),
            3 => Ok(Self::Incomplete),
            _ => Err(WireError::InvalidValue),
        }
    }
}

/// Bounded receipt-accounting summary, not a replacement for `InputResult`.
/// Unknown is NOT Known { pending: 0, uncertain: 0 }. These counts must come from
/// the input owner; a timeout, lost receipt or closed socket cannot invent zeros.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutstandingEffects {
    Unknown,
    Known {
        /// Actions still awaiting a final receipt from their original owner.
        pending: u32,
        /// Final receipts with an unknown/partial external-effect outcome.
        uncertain: u32,
    },
}

/// A peer's final report for one immutable remote session. Receiving this report
/// does not confirm physical input release; preserve its explicit cleanup/effect
/// stages and all prior per-action receipts. Absence is unknown, not success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Closed {
    pub reason: ClosedReason,
    pub cleanup: Cleanup,
    pub effects: OutstandingEffects,
}

pub fn encode_closed(
    closed: Closed,
    binding: Binding,
    limits: &ProtocolLimits,
    out: &mut [u8],
    direction: InputDirection,
    delivery: InputDelivery,
) -> Result<usize, WireError> {
    closed_route(binding, direction, delivery)?;
    let mut w = Writer::record_bounded(
        out,
        limits.max_control_message_bytes() as usize,
        binding.channel,
        Kind::Closed,
        CLOSED_BYTES - HEADER_BYTES,
    )?;
    w.put(&binding.session.as_raw().to_be_bytes())?;
    w.u16(closed.reason as u16)?;
    w.u8(closed.cleanup as u8)?;
    let (known, pending, uncertain) = match closed.effects {
        OutstandingEffects::Unknown => (0, 0, 0),
        OutstandingEffects::Known { pending, uncertain } => (1, pending, uncertain),
    };
    w.u8(known)?;
    w.u32(pending)?;
    w.u32(uncertain)?;
    w.finish()
}

pub fn decode_closed(
    bytes: &[u8],
    binding: Binding,
    limits: &ProtocolLimits,
    direction: InputDirection,
    delivery: InputDelivery,
) -> Result<Closed, WireError> {
    closed_route(binding, direction, delivery)?;
    let record = Record::decode_bounded(
        bytes,
        limits.max_control_message_bytes() as usize,
        binding.channel,
        None,
    )?;
    let mut r = record.reader(Kind::Closed)?;
    if r.take(16)? != binding.session.as_raw().to_be_bytes() {
        return Err(WireError::InvalidBinding);
    }
    let reason = ClosedReason::parse(r.u16()?)?;
    let cleanup = Cleanup::parse(r.u8()?)?;
    let known = r.u8()?;
    let pending = r.u32()?;
    let uncertain = r.u32()?;
    let effects = match (known, pending, uncertain) {
        (0, 0, 0) => OutstandingEffects::Unknown,
        (1, pending, uncertain) => OutstandingEffects::Known { pending, uncertain },
        _ => return Err(WireError::InvalidValue),
    };
    r.finish()?;
    Ok(Closed {
        reason,
        cleanup,
        effects,
    })
}
fn closed_route(
    binding: Binding,
    direction: InputDirection,
    delivery: InputDelivery,
) -> Result<(), WireError> {
    if direction != InputDirection::HostToViewer {
        return Err(WireError::WrongRole);
    }
    // Keep CloseRequest's direction contract unchanged while sharing the
    // mandatory session/reliable-route checks, not its payload or semantics.
    route(binding, InputDirection::ViewerToHost, delivery)
}
