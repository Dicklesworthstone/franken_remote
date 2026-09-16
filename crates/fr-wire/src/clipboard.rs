//! Allocation-free text clipboard records on an explicitly attached reliable
//! clipboard lane. See `PROTOCOL_CLIPBOARD.md` for exact v1 bytes and limits.
//! Neither a source label nor a channel/session ID grants clipboard authority.
use crate::{
    HEADER_BYTES, Kind, Record, WireError,
    record::{Reader, Writer},
};
use core::fmt;
use fr_core::{
    clipboard::{Begin, Binding, Endpoint, MAX_CHUNK_BYTES, MAX_CHUNKS, Stamp},
    ids::{InputLeaseId, RemoteSessionId},
    limits::ProtocolLimits,
};

pub mod receive;
pub mod send;
pub mod session;
pub mod startup;

pub const CAPABILITY: &str = "controller-text-clipboard";
pub const VERSION: u16 = 1;
pub const COMMON_BYTES: usize = HEADER_BYTES + 16 + 16 + 16 + 1 + 8;
pub const BEGIN_BYTES: usize = COMMON_BYTES + 8;
pub const CHUNK_OVERHEAD: usize = COMMON_BYTES + 12;
pub const COMMIT_BYTES: usize = COMMON_BYTES + 4;
pub const CANCEL_BYTES: usize = COMMON_BYTES + 2;

/// The actual authenticated sender, not a role asserted inside an app record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Host,
    Controller,
    Observer,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    Clipboard,
    Other,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Context {
    pub scope: Binding,
    pub channel: u32,
    pub sender: Role,
    pub lane: Lane,
}
impl Context {
    fn validate(self) -> Result<Endpoint, WireError> {
        if self.lane != Lane::Clipboard {
            return Err(WireError::WrongChannel);
        }
        if self.channel == 0 || !self.scope.valid() {
            return Err(WireError::InvalidBinding);
        }
        match self.sender {
            Role::Host => Ok(Endpoint::Host),
            Role::Controller => Ok(Endpoint::Controller),
            Role::Observer => Err(WireError::WrongRole),
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum CancelReason {
    User = 1,
    Disabled = 2,
    Expired = 3,
    Superseded = 4,
    Failed = 5,
}
impl CancelReason {
    fn parse(value: u16) -> Result<Self, WireError> {
        match value {
            1 => Ok(Self::User),
            2 => Ok(Self::Disabled),
            3 => Ok(Self::Expired),
            4 => Ok(Self::Superseded),
            5 => Ok(Self::Failed),
            _ => Err(WireError::InvalidValue),
        }
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Body<'a> {
    Begin {
        total_bytes: u32,
        chunks: u32,
    },
    Chunk {
        index: u32,
        offset: u32,
        bytes: &'a [u8],
    },
    Commit {
        total_bytes: u32,
    },
    Cancel(CancelReason),
}
impl fmt::Debug for Body<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Begin { .. } => "ClipboardBegin",
            Self::Chunk { .. } => "ClipboardChunk([redacted])",
            Self::Commit { .. } => "ClipboardCommit",
            Self::Cancel(_) => "ClipboardCancel",
        })
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Message<'a> {
    pub stamp: Stamp,
    pub body: Body<'a>,
}
impl Message<'_> {
    fn kind(self) -> Kind {
        match self.body {
            Body::Begin { .. } => Kind::ClipboardBegin,
            Body::Chunk { .. } => Kind::ClipboardChunk,
            Body::Commit { .. } => Kind::ClipboardCommit,
            Body::Cancel(_) => Kind::ClipboardCancel,
        }
    }
    fn validate(self, context: Context, limits: &ProtocolLimits) -> Result<usize, WireError> {
        if self.stamp.source != context.validate()? {
            return Err(WireError::WrongRole);
        }
        if !self.stamp.valid() {
            return Err(WireError::InvalidValue);
        }
        let size = match self.body {
            Body::Begin {
                total_bytes,
                chunks,
            } => {
                Begin {
                    binding: context.scope,
                    stamp: self.stamp,
                    total_bytes,
                    chunks,
                }
                .validate(limits)
                .map_err(|_| WireError::ResourceLimit)?;
                BEGIN_BYTES
            }
            Body::Chunk {
                index,
                offset,
                bytes,
            } => {
                if bytes.is_empty()
                    || bytes.len() > MAX_CHUNK_BYTES
                    || index >= MAX_CHUNKS
                    || u64::from(offset) + bytes.len() as u64
                        > u64::from(limits.max_clipboard_item_bytes())
                {
                    return Err(WireError::ResourceLimit);
                }
                CHUNK_OVERHEAD + bytes.len()
            }
            Body::Commit { total_bytes } => {
                if total_bytes > limits.max_clipboard_item_bytes() {
                    return Err(WireError::ResourceLimit);
                }
                COMMIT_BYTES
            }
            Body::Cancel(_) => CANCEL_BYTES,
        };
        if size > limits.max_control_message_bytes() as usize {
            return Err(WireError::ResourceLimit);
        }
        Ok(size)
    }
}

pub fn encode(
    message: Message<'_>,
    context: Context,
    limits: &ProtocolLimits,
    out: &mut [u8],
) -> Result<usize, WireError> {
    let size = message.validate(context, limits)?;
    let mut w = Writer::record_bounded(
        out,
        limits.max_control_message_bytes() as usize,
        context.channel,
        message.kind(),
        size - HEADER_BYTES,
    )?;
    w.put(&context.scope.session.as_raw().to_be_bytes())?;
    w.put(&context.scope.lease.as_raw().to_be_bytes())?;
    w.put(&message.stamp.id.to_be_bytes())?;
    w.u8(message.stamp.source as u8)?;
    w.u64(message.stamp.sequence)?;
    match message.body {
        Body::Begin {
            total_bytes,
            chunks,
        } => {
            w.u32(total_bytes)?;
            w.u32(chunks)?;
        }
        Body::Chunk {
            index,
            offset,
            bytes,
        } => {
            w.u32(index)?;
            w.u32(offset)?;
            w.u32(u32::try_from(bytes.len()).map_err(|_| WireError::ResourceLimit)?)?;
            w.put(bytes)?;
        }
        Body::Commit { total_bytes } => w.u32(total_bytes)?,
        Body::Cancel(reason) => w.u16(reason as u16)?,
    }
    w.finish()
}
fn id(reader: &mut Reader<'_>) -> Result<u128, WireError> {
    Ok(u128::from_be_bytes(
        reader
            .take(16)?
            .try_into()
            .map_err(|_| WireError::Truncated)?,
    ))
}
pub fn decode<'a>(
    bytes: &'a [u8],
    context: Context,
    limits: &ProtocolLimits,
) -> Result<Message<'a>, WireError> {
    let sender = context.validate()?;
    let record = Record::decode_bounded(
        bytes,
        limits.max_control_message_bytes() as usize,
        context.channel,
        None,
    )?;
    let kind = record.kind();
    if !matches!(
        kind,
        Kind::ClipboardBegin | Kind::ClipboardChunk | Kind::ClipboardCommit | Kind::ClipboardCancel
    ) {
        return Err(WireError::UnsupportedKind);
    }
    let mut r = record.reader(kind)?;
    let scope = Binding {
        session: RemoteSessionId::from_raw(id(&mut r)?),
        lease: InputLeaseId::from_raw(id(&mut r)?),
    };
    if scope != context.scope {
        return Err(WireError::InvalidBinding);
    }
    let transfer = id(&mut r)?;
    let source = match r.u8()? {
        1 => Endpoint::Host,
        2 => Endpoint::Controller,
        _ => return Err(WireError::InvalidValue),
    };
    if source != sender {
        return Err(WireError::WrongRole);
    }
    let stamp = Stamp {
        id: transfer,
        source,
        sequence: r.u64()?,
    };
    let body = match kind {
        Kind::ClipboardBegin => Body::Begin {
            total_bytes: r.u32()?,
            chunks: r.u32()?,
        },
        Kind::ClipboardChunk => Body::Chunk {
            index: r.u32()?,
            offset: r.u32()?,
            bytes: r.data()?,
        },
        Kind::ClipboardCommit => Body::Commit {
            total_bytes: r.u32()?,
        },
        Kind::ClipboardCancel => Body::Cancel(CancelReason::parse(r.u16()?)?),
        _ => return Err(WireError::UnsupportedKind),
    };
    r.finish()?;
    let message = Message { stamp, body };
    message.validate(context, limits)?;
    Ok(message)
}
