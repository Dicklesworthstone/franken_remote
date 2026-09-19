//! Allocation-free authorization envelopes for the pinned ATP file payloads.
//! A channel, directory handle or transfer ID is not authority. The transport
//! supplies the authenticated role/direction and a separately approved handle.
//! See `FILE_RECEIVE.md`; the ATP codec itself stays outside this runtime-free crate.
use crate::{
    HEADER_BYTES, Kind, Record, WireError,
    record::{Reader, Writer},
};
use core::fmt;
use fr_core::{
    ids::{InputLeaseId, RemoteSessionId},
    limits::ProtocolLimits,
};

pub const CAPABILITY: &str = "file-atp-full";
pub const VERSION: u16 = 1;
pub const ATP_PORTABLE_FULL: u16 = 1;
/// Largest extension-free 0.5.0 ATP data header plus entry index/offset.
pub const ATP_DATA_OVERHEAD: usize = 20;
pub const COMMON_BYTES: usize = HEADER_BYTES + 16 + 16 + 16 + 8;
pub const OFFER_OVERHEAD: usize = COMMON_BYTES + 2 + 4;
pub const ACCEPT_OVERHEAD: usize = COMMON_BYTES + 2 + 8 + 4 + 4 + 2 + 4;
pub const CHUNK_OVERHEAD: usize = COMMON_BYTES + 4;
pub const COMPLETE_OVERHEAD: usize = COMMON_BYTES + 1 + 2 + 8 + 4;
pub const CANCEL_BYTES: usize = COMMON_BYTES + 2;
pub const MAX_OVERHEAD: usize = ACCEPT_OVERHEAD;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    record_bytes: usize,
}
impl Limits {
    pub fn new(protocol: &ProtocolLimits, record_bytes: usize) -> Result<Self, WireError> {
        if !(MAX_OVERHEAD..=protocol.max_control_message_bytes() as usize).contains(&record_bytes) {
            return Err(WireError::InvalidLimits);
        }
        Ok(Self { record_bytes })
    }
    pub fn record_bytes(self) -> usize {
        self.record_bytes
    }
    /// A conservative uniform ATP ceiling fits every envelope, including replies.
    pub fn atp_bytes(self) -> usize {
        self.record_bytes - MAX_OVERHEAD
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Host,
    Controller,
    Observer,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    ToHost,
    ToController,
}
impl Direction {
    fn source(self) -> Role {
        match self {
            Self::ToHost => Role::Controller,
            Self::ToController => Role::Host,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    Files,
    Other,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Context {
    pub session: RemoteSessionId,
    pub lease: InputLeaseId,
    /// Locally selected directory/source handle bound to this attachment only.
    pub handle: u128,
    pub channel: u32,
    pub sender: Role,
    pub direction: Direction,
    pub lane: Lane,
}
impl Context {
    pub fn validate(self) -> Result<(), WireError> {
        if self.lane != Lane::Files {
            return Err(WireError::WrongChannel);
        }
        if self.sender == Role::Observer {
            return Err(WireError::WrongRole);
        }
        if self.channel == 0
            || self.session.as_raw() == 0
            || self.lease.as_raw() == 0
            || self.handle == 0
        {
            return Err(WireError::InvalidBinding);
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum Reason {
    None = 0,
    User = 1,
    Expired = 2,
    Permission = 3,
    Integrity = 4,
    Conflict = 5,
    Invalid = 6,
    Resource = 7,
    UnknownEffect = 8,
    Cancelled = 9,
}
impl Reason {
    fn parse(v: u16) -> Result<Self, WireError> {
        match v {
            0 => Ok(Self::None),
            1 => Ok(Self::User),
            2 => Ok(Self::Expired),
            3 => Ok(Self::Permission),
            4 => Ok(Self::Integrity),
            5 => Ok(Self::Conflict),
            6 => Ok(Self::Invalid),
            7 => Ok(Self::Resource),
            8 => Ok(Self::UnknownEffect),
            9 => Ok(Self::Cancelled),
            _ => Err(WireError::InvalidValue),
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Disposition {
    PublishedDurable = 1,
    PublishedDurabilityUnknown = 2,
    Refused = 3,
    UnknownEffect = 4,
}
impl Disposition {
    fn parse(v: u8) -> Result<Self, WireError> {
        match v {
            1 => Ok(Self::PublishedDurable),
            2 => Ok(Self::PublishedDurabilityUnknown),
            3 => Ok(Self::Refused),
            4 => Ok(Self::UnknownEffect),
            _ => Err(WireError::InvalidValue),
        }
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Body<'a> {
    Offer {
        profile: u16,
        atp: &'a [u8],
    },
    Accept {
        profile: u16,
        size: u64,
        bytes_per_second: u32,
        chunk_bytes: u32,
        concurrent_transfers: u16,
        atp: &'a [u8],
    },
    Chunk {
        atp: &'a [u8],
    },
    Complete {
        disposition: Disposition,
        reason: Reason,
        published_bytes: u64,
        atp: &'a [u8],
    },
    Cancel(Reason),
}
impl fmt::Debug for Body<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Offer { .. } => "FileOffer([redacted])",
            Self::Accept { .. } => "FileAccept",
            Self::Chunk { .. } => "FileChunk([redacted])",
            Self::Complete { .. } => "FileComplete",
            Self::Cancel(_) => "FileCancel",
        })
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Message<'a> {
    pub id: u64,
    pub body: Body<'a>,
}
impl Message<'_> {
    fn kind(self) -> Kind {
        match self.body {
            Body::Offer { .. } => Kind::FileOffer,
            Body::Accept { .. } => Kind::FileAccept,
            Body::Chunk { .. } => Kind::FileChunk,
            Body::Complete { .. } => Kind::FileComplete,
            Body::Cancel(_) => Kind::FileCancel,
        }
    }
    fn validate(self, context: Context, limits: Limits) -> Result<usize, WireError> {
        context.validate()?;
        if self.id == 0 {
            return Err(WireError::InvalidValue);
        }
        let source = context.direction.source() == context.sender;
        let (overhead, atp) = match self.body {
            Body::Offer { profile, atp } => {
                if !source {
                    return Err(WireError::WrongRole);
                }
                if profile != ATP_PORTABLE_FULL {
                    return Err(WireError::UnsupportedVersion);
                }
                if atp.is_empty() {
                    return Err(WireError::InvalidValue);
                }
                (OFFER_OVERHEAD, atp)
            }
            Body::Accept {
                profile,
                bytes_per_second,
                chunk_bytes,
                concurrent_transfers,
                atp,
                ..
            } => {
                if source {
                    return Err(WireError::WrongRole);
                }
                if profile != ATP_PORTABLE_FULL {
                    return Err(WireError::UnsupportedVersion);
                }
                if bytes_per_second == 0
                    || chunk_bytes == 0
                    || chunk_bytes as usize > limits.atp_bytes().saturating_sub(ATP_DATA_OVERHEAD)
                    || concurrent_transfers != 1
                    || atp.is_empty()
                {
                    return Err(WireError::InvalidLimits);
                }
                (ACCEPT_OVERHEAD, atp)
            }
            Body::Chunk { atp } => {
                if !source {
                    return Err(WireError::WrongRole);
                }
                if atp.is_empty() {
                    return Err(WireError::InvalidValue);
                }
                (CHUNK_OVERHEAD, atp)
            }
            Body::Complete {
                disposition,
                reason,
                published_bytes,
                atp,
            } => {
                if source {
                    return Err(WireError::WrongRole);
                }
                let valid = match disposition {
                    Disposition::PublishedDurable | Disposition::PublishedDurabilityUnknown => {
                        reason == Reason::None && !atp.is_empty()
                    }
                    Disposition::Refused => {
                        reason != Reason::None
                            && reason != Reason::UnknownEffect
                            && published_bytes == 0
                            && atp.is_empty()
                    }
                    Disposition::UnknownEffect => {
                        reason == Reason::UnknownEffect && published_bytes == 0 && atp.is_empty()
                    }
                };
                if !valid {
                    return Err(WireError::InvalidValue);
                }
                (COMPLETE_OVERHEAD, atp)
            }
            Body::Cancel(reason) => {
                if reason == Reason::None {
                    return Err(WireError::InvalidValue);
                }
                (CANCEL_BYTES, &[][..])
            }
        };
        if atp.len() > limits.atp_bytes() {
            return Err(WireError::ResourceLimit);
        }
        let total = overhead
            .checked_add(atp.len())
            .ok_or(WireError::ArithmeticOverflow)?;
        if total > limits.record_bytes {
            return Err(WireError::ResourceLimit);
        }
        Ok(total)
    }
}
pub fn encode(
    message: Message<'_>,
    context: Context,
    limits: Limits,
    out: &mut [u8],
) -> Result<usize, WireError> {
    let total = message.validate(context, limits)?;
    let mut w = Writer::record_bounded(
        out,
        limits.record_bytes,
        context.channel,
        message.kind(),
        total - HEADER_BYTES,
    )?;
    w.put(&context.session.as_raw().to_be_bytes())?;
    w.put(&context.lease.as_raw().to_be_bytes())?;
    w.put(&context.handle.to_be_bytes())?;
    w.u64(message.id)?;
    match message.body {
        Body::Offer { profile, atp } => {
            w.u16(profile)?;
            w.data(atp)?;
        }
        Body::Accept {
            profile,
            size,
            bytes_per_second,
            chunk_bytes,
            concurrent_transfers,
            atp,
        } => {
            w.u16(profile)?;
            w.u64(size)?;
            w.u32(bytes_per_second)?;
            w.u32(chunk_bytes)?;
            w.u16(concurrent_transfers)?;
            w.data(atp)?;
        }
        Body::Chunk { atp } => w.data(atp)?,
        Body::Complete {
            disposition,
            reason,
            published_bytes,
            atp,
        } => {
            w.u8(disposition as u8)?;
            w.u16(reason as u16)?;
            w.u64(published_bytes)?;
            w.data(atp)?;
        }
        Body::Cancel(reason) => w.u16(reason as u16)?,
    }
    w.finish()
}
fn id(r: &mut Reader<'_>) -> Result<u128, WireError> {
    Ok(u128::from_be_bytes(
        r.take(16)?.try_into().map_err(|_| WireError::Truncated)?,
    ))
}
pub fn decode(bytes: &[u8], context: Context, limits: Limits) -> Result<Message<'_>, WireError> {
    context.validate()?;
    let record = Record::decode_bounded(bytes, limits.record_bytes, context.channel, None)?;
    let kind = record.kind();
    if !matches!(
        kind,
        Kind::FileOffer
            | Kind::FileAccept
            | Kind::FileChunk
            | Kind::FileComplete
            | Kind::FileCancel
    ) {
        return Err(WireError::UnsupportedKind);
    }
    let mut r = record.reader(kind)?;
    if id(&mut r)? != context.session.as_raw()
        || id(&mut r)? != context.lease.as_raw()
        || id(&mut r)? != context.handle
    {
        return Err(WireError::InvalidBinding);
    }
    let transfer = r.u64()?;
    let body = match kind {
        Kind::FileOffer => Body::Offer {
            profile: r.u16()?,
            atp: r.data()?,
        },
        Kind::FileAccept => Body::Accept {
            profile: r.u16()?,
            size: r.u64()?,
            bytes_per_second: r.u32()?,
            chunk_bytes: r.u32()?,
            concurrent_transfers: r.u16()?,
            atp: r.data()?,
        },
        Kind::FileChunk => Body::Chunk { atp: r.data()? },
        Kind::FileComplete => Body::Complete {
            disposition: Disposition::parse(r.u8()?)?,
            reason: Reason::parse(r.u16()?)?,
            published_bytes: r.u64()?,
            atp: r.data()?,
        },
        Kind::FileCancel => Body::Cancel(Reason::parse(r.u16()?)?),
        _ => return Err(WireError::UnsupportedKind),
    };
    r.finish()?;
    let message = Message { id: transfer, body };
    message.validate(context, limits)?;
    Ok(message)
}
