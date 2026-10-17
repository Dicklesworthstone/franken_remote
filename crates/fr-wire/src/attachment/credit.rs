//! Optional, bilateral record-credit negotiation within the existing attachment
//! exchange. The viewer's `BindingAccepted` carries its local role-specific cap;
//! the host can only narrow its original grant before issuing the ticket.
//! Parsing, capability selection and authority remain separate checks.
use super::{GRANT_RECORD_BYTES, Grant};
use crate::{
    HEADER_BYTES, Kind, Record, WireError,
    input::{InputDelivery, InputDirection},
    negotiation::{ControlBinding, Selection},
    record::Writer,
};
use fr_core::limits::ProtocolLimits;

pub const CAPABILITY: &str = "native-attachment-credit";
pub const VERSION: u16 = 1;
pub const ACCEPTED_BYTES: usize = HEADER_BYTES + 12;

/// This extension belongs to the native attachment profile, in either role.
/// It never selects a channel, increases a local budget, or grants authority.
pub fn selected(selection: &Selection) -> bool {
    selection.validate().is_ok()
        && [(super::CAPABILITY, super::VERSION), (CAPABILITY, VERSION)]
            .iter()
            .all(|&(name, version)| {
                selection
                    .capabilities
                    .iter()
                    .any(|cap| cap.name == name && cap.version == version)
            })
}

/// Authenticated viewer acceptance of one binding and its local record cap.
/// No ticket, OS permission, deadline, picture allowance or generation is added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Accepted {
    pub binding: u32,
    pub byte_allowance: u64,
}
impl Accepted {
    fn validate(self, parent: ControlBinding, limits: &ProtocolLimits) -> Result<(), WireError> {
        if parent.id == 0 || self.binding == 0 || self.binding == parent.id {
            return Err(WireError::InvalidBinding);
        }
        if self.byte_allowance < GRANT_RECORD_BYTES as u64
            || self.byte_allowance > u64::from(limits.max_control_message_bytes())
        {
            return Err(WireError::InvalidValue);
        }
        Ok(())
    }

    /// Intersect with the exact host reservation before its Ticket is encoded.
    /// The exchange owner must still check phase, positive selection and original
    /// connection. This cannot change any other field or enlarge either cap.
    pub fn negotiate(
        self,
        offered: Grant,
        parent: ControlBinding,
        limits: &ProtocolLimits,
    ) -> Result<Grant, WireError> {
        self.validate(parent, limits)?;
        offered.validate(parent, limits)?;
        if self.binding != offered.descriptor.binding.parent.id {
            return Err(WireError::InvalidBinding);
        }
        if offered.byte_allowance < GRANT_RECORD_BYTES as u64 {
            return Err(WireError::InvalidValue);
        }
        Ok(Grant {
            byte_allowance: self.byte_allowance.min(offered.byte_allowance),
            ..offered
        })
    }
}

fn direction(sender: InputDirection, delivery: InputDelivery) -> Result<(), WireError> {
    if delivery != InputDelivery::Reliable {
        return Err(WireError::WrongChannel);
    }
    if sender != InputDirection::ViewerToHost {
        return Err(WireError::WrongRole);
    }
    Ok(())
}

/// Extended `BindingAccepted` (0x001c), only after positive capability selection.
/// The legacy attachment decoder deliberately continues to reject this layout.
pub fn encode(
    accepted: Accepted,
    parent: ControlBinding,
    limits: &ProtocolLimits,
    out: &mut [u8],
    sender: InputDirection,
    delivery: InputDelivery,
) -> Result<usize, WireError> {
    direction(sender, delivery)?;
    accepted.validate(parent, limits)?;
    let mut writer = Writer::record_bounded(
        out,
        limits.max_control_message_bytes() as usize,
        parent.id,
        Kind::BindingAccepted,
        ACCEPTED_BYTES - HEADER_BYTES,
    )?;
    writer.u32(accepted.binding)?;
    writer.u64(accepted.byte_allowance)?;
    writer.finish()
}

/// Decode exactly one fixed-size acceptance on the original control binding.
/// Selecting this decoder without negotiating the extension is a caller error;
/// a selected peer sending the legacy layout is not silently downgraded.
pub fn decode(
    bytes: &[u8],
    parent: ControlBinding,
    limits: &ProtocolLimits,
    sender: InputDirection,
    delivery: InputDelivery,
) -> Result<Accepted, WireError> {
    direction(sender, delivery)?;
    let record = Record::decode_bounded(
        bytes,
        ACCEPTED_BYTES.min(limits.max_control_message_bytes() as usize),
        parent.id,
        None,
    )?;
    let mut reader = record.reader(Kind::BindingAccepted)?;
    let accepted = Accepted {
        binding: reader.u32()?,
        byte_allowance: reader.u64()?,
    };
    reader.finish()?;
    accepted.validate(parent, limits)?;
    Ok(accepted)
}
