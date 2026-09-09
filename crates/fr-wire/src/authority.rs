//! Fixed, allocation-free authority challenges. Identifiers are bindings, not
//! authentication. Only the host's existing authority owner can renew a grant.
use crate::{
    HEADER_BYTES, Kind, Record, WireError,
    input::{InputDelivery, InputDirection},
    record::{Reader, Writer},
};
use core::fmt;
use fr_core::{
    ids::{InputLeaseId, RemoteSessionId},
    limits::ProtocolLimits,
};

pub const OBSERVATION_CHALLENGE_BYTES: usize = HEADER_BYTES + 42;
pub const OBSERVATION_RESPONSE_BYTES: usize = HEADER_BYTES + 34;
pub const MAX_AUTHORITY_BYTES: usize = OBSERVATION_CHALLENGE_BYTES + 16;

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Binding {
    pub channel: u32,
    pub session: RemoteSessionId,
}
impl fmt::Debug for Binding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AuthorityBinding")
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Observation,
    Control(InputLeaseId),
}
impl fmt::Debug for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Observation => "Observation",
            Self::Control(_) => "Control",
        })
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Message {
    Challenge {
        scope: Scope,
        nonce: u128,
        deadline_micros: u64,
    },
    Response {
        scope: Scope,
        nonce: u128,
    },
}
impl fmt::Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Challenge { .. } => "Challenge",
            Self::Response { .. } => "ChallengeResponse",
        })
    }
}
impl Message {
    pub const fn scope(self) -> Scope {
        match self {
            Self::Challenge { scope, .. } | Self::Response { scope, .. } => scope,
        }
    }
    pub const fn nonce(self) -> u128 {
        match self {
            Self::Challenge { nonce, .. } | Self::Response { nonce, .. } => nonce,
        }
    }
    fn kind(self) -> Kind {
        match self {
            Self::Challenge { .. } => Kind::Challenge,
            Self::Response { .. } => Kind::ChallengeResponse,
        }
    }
    fn validate(self) -> Result<(), WireError> {
        if self.nonce() == 0
            || matches!(self.scope(), Scope::Control(lease) if lease.as_raw() == 0)
            || matches!(
                self,
                Self::Challenge {
                    deadline_micros: 0,
                    ..
                }
            )
        {
            return Err(WireError::InvalidValue);
        }
        Ok(())
    }
}
fn role(kind: Kind, direction: InputDirection, delivery: InputDelivery) -> Result<(), WireError> {
    if delivery != InputDelivery::Reliable {
        return Err(WireError::WrongChannel);
    }
    let expected = match kind {
        Kind::Challenge => InputDirection::HostToViewer,
        Kind::ChallengeResponse => InputDirection::ViewerToHost,
        _ => return Err(WireError::UnsupportedKind),
    };
    if direction != expected {
        return Err(WireError::WrongRole);
    }
    Ok(())
}
fn validate_binding(binding: Binding) -> Result<(), WireError> {
    if binding.channel == 0 || binding.session.as_raw() == 0 {
        Err(WireError::InvalidBinding)
    } else {
        Ok(())
    }
}
fn u128_read(reader: &mut Reader<'_>) -> Result<u128, WireError> {
    Ok(u128::from_be_bytes(
        reader
            .take(16)?
            .try_into()
            .map_err(|_| WireError::Truncated)?,
    ))
}
/// `deadline_micros` is an opaque host deadline to a viewer. No viewer-clock
/// comparison or receipt-time deadline extension is implied by these bytes.
pub fn encode(
    message: Message,
    binding: Binding,
    limits: &ProtocolLimits,
    out: &mut [u8],
    direction: InputDirection,
    delivery: InputDelivery,
) -> Result<usize, WireError> {
    validate_binding(binding)?;
    message.validate()?;
    role(message.kind(), direction, delivery)?;
    let control = matches!(message.scope(), Scope::Control(_));
    let payload = 34
        + if control { 16 } else { 0 }
        + if matches!(message, Message::Challenge { .. }) {
            8
        } else {
            0
        };
    let mut writer = Writer::record_bounded(
        out,
        limits.max_control_message_bytes() as usize,
        binding.channel,
        message.kind(),
        payload,
    )?;
    writer.put(&binding.session.as_raw().to_be_bytes())?;
    writer.u8(u8::from(control))?;
    writer.u8(u8::from(control))?;
    if let Scope::Control(lease) = message.scope() {
        writer.put(&lease.as_raw().to_be_bytes())?;
    }
    writer.put(&message.nonce().to_be_bytes())?;
    if let Message::Challenge {
        deadline_micros, ..
    } = message
    {
        writer.u64(deadline_micros)?;
    }
    writer.finish()
}
pub fn decode(
    bytes: &[u8],
    binding: Binding,
    limits: &ProtocolLimits,
    direction: InputDirection,
    delivery: InputDelivery,
) -> Result<Message, WireError> {
    validate_binding(binding)?;
    let record = Record::decode_bounded(
        bytes,
        limits.max_control_message_bytes() as usize,
        binding.channel,
        None,
    )?;
    role(record.kind(), direction, delivery)?;
    let mut reader = record.reader(record.kind())?;
    if u128_read(&mut reader)? != binding.session.as_raw() {
        return Err(WireError::InvalidBinding);
    }
    let scope = match (reader.u8()?, reader.u8()?) {
        (0, 0) => Scope::Observation,
        (1, 1) => Scope::Control(InputLeaseId::from_raw(u128_read(&mut reader)?)),
        _ => return Err(WireError::InvalidValue),
    };
    let nonce = u128_read(&mut reader)?;
    let message = if record.kind() == Kind::Challenge {
        Message::Challenge {
            scope,
            nonce,
            deadline_micros: reader.u64()?,
        }
    } else {
        Message::Response { scope, nonce }
    };
    reader.finish()?;
    message.validate()?;
    Ok(message)
}
