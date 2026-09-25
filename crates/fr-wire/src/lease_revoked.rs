//! Terminal input-lease notification on the existing reliable control stream.
//! A sender must fence input locally BEFORE constructing this report. Delivery
//! never performs that fence or certifies native cleanup. See
//! `PROTOCOL_LEASE_REVOKED.md` for the byte contract and stage semantics.
use crate::{
    HEADER_BYTES, Kind, Record, WireError,
    authority::Binding,
    input::{InputDelivery, InputDirection},
    record::Writer,
};
use core::fmt;
use fr_core::{ids::InputLeaseId, limits::ProtocolLimits};

pub const REVOKED_BYTES: usize = HEADER_BYTES + 36;

/// Content-free reasons. No peer text or native-library error crosses the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum Reason {
    LocalRevoke = 1,
    LeaseExpired = 2,
    ObservationEnded = 3,
    ViewInvalidated = 4,
    SessionEnded = 5,
    PermissionLost = 6,
    HostFailure = 7,
    ClientRequested = 8,
    Suspended = 9,
}
impl Reason {
    fn parse(value: u16) -> Result<Self, WireError> {
        Ok(match value {
            1 => Self::LocalRevoke,
            2 => Self::LeaseExpired,
            3 => Self::ObservationEnded,
            4 => Self::ViewInvalidated,
            5 => Self::SessionEnded,
            6 => Self::PermissionLost,
            7 => Self::HostFailure,
            8 => Self::ClientRequested,
            9 => Self::Suspended,
            _ => return Err(WireError::InvalidValue),
        })
    }
    pub const fn code(self) -> &'static str {
        match self {
            Self::LocalRevoke => "local_revoke",
            Self::LeaseExpired => "lease_expired",
            Self::ObservationEnded => "observation_ended",
            Self::ViewInvalidated => "view_invalidated",
            Self::SessionEnded => "session_ended",
            Self::PermissionLost => "permission_lost",
            Self::HostFailure => "host_failure",
            Self::ClientRequested => "client_requested",
            Self::Suspended => "suspended",
        }
    }
}

/// Fencing is mandatory in every variant. A report of Fenced alone must not be
/// rendered as "all keys released": the native owner can still be draining.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CleanupStage {
    Fenced = 1,
    Released = 2,
    Failed = 3,
}
impl CleanupStage {
    fn parse(value: u8) -> Result<Self, WireError> {
        Ok(match value {
            1 => Self::Fenced,
            2 => Self::Released,
            3 => Self::Failed,
            _ => return Err(WireError::InvalidValue),
        })
    }
}

/// Receipt accounting, NOT a claim that external effects have been undone.
/// Individual `InputResult` records remain authoritative and must be retained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum EffectStage {
    Unknown = 0,
    ReceiptsPending = 1,
    ReceiptsComplete = 2,
}
impl EffectStage {
    fn parse(value: u8) -> Result<Self, WireError> {
        Ok(match value {
            0 => Self::Unknown,
            1 => Self::ReceiptsPending,
            2 => Self::ReceiptsComplete,
            _ => return Err(WireError::InvalidValue),
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Revoked {
    pub lease: InputLeaseId,
    pub reason: Reason,
    pub cleanup: CleanupStage,
    pub effects: EffectStage,
}
impl fmt::Debug for Revoked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LeaseRevoked")
            .field("reason", &self.reason)
            .field("cleanup", &self.cleanup)
            .field("effects", &self.effects)
            .finish_non_exhaustive()
    }
}
fn route(
    binding: Binding,
    lease: InputLeaseId,
    direction: InputDirection,
    delivery: InputDelivery,
) -> Result<(), WireError> {
    if direction != InputDirection::HostToViewer {
        return Err(WireError::WrongRole);
    }
    if delivery != InputDelivery::Reliable {
        return Err(WireError::WrongChannel);
    }
    if binding.channel == 0 || binding.session.as_raw() == 0 || lease.as_raw() == 0 {
        return Err(WireError::InvalidBinding);
    }
    Ok(())
}
pub fn encode(
    revoked: Revoked,
    binding: Binding,
    limits: &ProtocolLimits,
    out: &mut [u8],
    direction: InputDirection,
    delivery: InputDelivery,
) -> Result<usize, WireError> {
    route(binding, revoked.lease, direction, delivery)?;
    let mut writer = Writer::record_bounded(
        out,
        limits.max_control_message_bytes() as usize,
        binding.channel,
        Kind::LeaseRevoked,
        REVOKED_BYTES - HEADER_BYTES,
    )?;
    writer.put(&binding.session.as_raw().to_be_bytes())?;
    writer.put(&revoked.lease.as_raw().to_be_bytes())?;
    writer.u16(revoked.reason as u16)?;
    writer.u8(revoked.cleanup as u8)?;
    writer.u8(revoked.effects as u8)?;
    writer.finish()
}

/// The caller supplies the current immutable lease as well as the installed
/// session binding. A delayed report for an old lease cannot stop a new one.
pub fn decode(
    bytes: &[u8],
    binding: Binding,
    lease: InputLeaseId,
    limits: &ProtocolLimits,
    direction: InputDirection,
    delivery: InputDelivery,
) -> Result<Revoked, WireError> {
    route(binding, lease, direction, delivery)?;
    let record = Record::decode_bounded(
        bytes,
        limits.max_control_message_bytes() as usize,
        binding.channel,
        None,
    )?;
    let mut reader = record.reader(Kind::LeaseRevoked)?;
    if reader.take(16)? != binding.session.as_raw().to_be_bytes()
        || reader.take(16)? != lease.as_raw().to_be_bytes()
    {
        return Err(WireError::InvalidBinding);
    }
    let revoked = Revoked {
        lease,
        reason: Reason::parse(reader.u16()?)?,
        cleanup: CleanupStage::parse(reader.u8()?)?,
        effects: EffectStage::parse(reader.u8()?)?,
    };
    reader.finish()?;
    Ok(revoked)
}
