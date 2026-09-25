//! Content-free refusal records on an existing reliable channel. See
//! `PROTOCOL_REFUSAL.md` for the byte contract. A refusal is not an input
//! receipt, a grant, or proof that remote cleanup succeeded.
use crate::{
    HEADER_BYTES, Kind, Record, WireError, input::InputDelivery, input_result::Stage,
    record::Writer,
};
use core::fmt;
use fr_core::limits::ProtocolLimits;

pub const MIN_BYTES: usize = HEADER_BYTES + 4;
pub const MAX_BYTES: usize = HEADER_BYTES + 13;

/// Explicit v0 assignments; no library errors or peer-controlled strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum Reason {
    InvalidMessage = 1,
    UnsupportedVersion = 2,
    UnsupportedProfile = 3,
    RequiredCapability = 4,
    InvalidLimits = 5,
    InvalidSelection = 6,
    PermissionDenied = 7,
    LocalApprovalDenied = 8,
    ApprovalExpired = 9,
    ControlUnavailable = 10,
    ResourceLimit = 11,
    InvalidState = 12,
    Expired = 13,
    HostUnavailable = 14,
    TailnetMembershipUnverifiable = 15,
}
impl Reason {
    fn parse(value: u16) -> Result<Self, WireError> {
        Ok(match value {
            1 => Self::InvalidMessage,
            2 => Self::UnsupportedVersion,
            3 => Self::UnsupportedProfile,
            4 => Self::RequiredCapability,
            5 => Self::InvalidLimits,
            6 => Self::InvalidSelection,
            7 => Self::PermissionDenied,
            8 => Self::LocalApprovalDenied,
            9 => Self::ApprovalExpired,
            10 => Self::ControlUnavailable,
            11 => Self::ResourceLimit,
            12 => Self::InvalidState,
            13 => Self::Expired,
            14 => Self::HostUnavailable,
            15 => Self::TailnetMembershipUnverifiable,
            _ => return Err(WireError::InvalidValue),
        })
    }
}

/// The operation reference is opaque metadata, never a ticket or credential.
/// An optional effect stage describes only that operation. Detailed committed
/// prefixes and unknown remainders remain in the existing `InputResult`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Refused {
    pub reason: Reason,
    pub operation: Option<u64>,
    pub stage: Option<Stage>,
}
impl fmt::Debug for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Refused")
            .field("reason", &self.reason)
            .field("has_operation", &self.operation.is_some())
            .field("stage", &self.stage)
            .finish()
    }
}
impl Refused {
    pub const fn connection(reason: Reason) -> Self {
        Self {
            reason,
            operation: None,
            stage: None,
        }
    }
    fn validate(self, binding: u32) -> Result<(), WireError> {
        if self.stage.is_some() && (binding == 0 || self.operation.is_none()) {
            return Err(WireError::InvalidValue);
        }
        Ok(())
    }
}
fn maximum(value: usize, delivery: InputDelivery) -> Result<usize, WireError> {
    if delivery != InputDelivery::Reliable {
        return Err(WireError::WrongChannel);
    }
    Ok(value.min(ProtocolLimits::ABSOLUTE.max_control_message_bytes() as usize))
}
/// Either endpoint may report a refusal. The caller supplies the installed
/// reliable-channel binding; zero is allowed only during initial negotiation.
/// Sending these bytes cannot confer permission to send other message kinds.
pub fn encode(
    message: Refused,
    binding: u32,
    limit: usize,
    out: &mut [u8],
    delivery: InputDelivery,
) -> Result<usize, WireError> {
    let maximum = maximum(limit, delivery)?;
    message.validate(binding)?;
    let payload =
        4 + usize::from(message.operation.is_some()) * 8 + usize::from(message.stage.is_some());
    let mut writer = Writer::record_bounded(out, maximum, binding, Kind::Refused, payload)?;
    writer.u16(message.reason as u16)?;
    writer.optional_u64(message.operation)?;
    writer.u8(u8::from(message.stage.is_some()))?;
    if let Some(stage) = message.stage {
        writer.u8(stage as u8)?;
    }
    writer.finish()
}
pub fn decode(
    bytes: &[u8],
    binding: u32,
    limit: usize,
    delivery: InputDelivery,
) -> Result<Refused, WireError> {
    let record = Record::decode_bounded(bytes, maximum(limit, delivery)?, binding, None)?;
    let mut reader = record.reader(Kind::Refused)?;
    let reason = Reason::parse(reader.u16()?)?;
    let operation = reader.optional_u64()?;
    let stage = match reader.u8()? {
        0 => None,
        1 => Some(match reader.u8()? {
            0 => Stage::Admitted,
            1 => Stage::SubmittedToOs,
            2 => Stage::Observed,
            _ => return Err(WireError::InvalidValue),
        }),
        _ => return Err(WireError::InvalidValue),
    };
    reader.finish()?;
    let message = Refused {
        reason,
        operation,
        stage,
    };
    message.validate(binding)?;
    Ok(message)
}
