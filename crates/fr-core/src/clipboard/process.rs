//! Private IPC between the host's clipboard synchronizer (in frd) and ONE
//! per-lease out-of-process X11 clipboard owner (`fr-input-agent --clipboard`).
//! Both sides link only this safe codec; the child never links the broker.
//!
//! Command channel: fixed 64-byte frames on a private `SOCK_STREAM` socketpair,
//! exactly one outstanding request, sequence numbers starting at 1 and
//! increasing by one; every reply echoes its request's sequence. Only two
//! frames carry clipboard text, and each announces its exact length first:
//! `Prepare` (host → child) and `ReadText` (child → host). The receiver checks
//! that length against the item bound agreed in `Hello` BEFORE it allocates or
//! reads a single payload byte; the text itself never appears in a frame.
//!
//! No lease, ticket, nonce, peer identity or transfer ID crosses: only the
//! item stamp the X11 owner needs for echo provenance, the native revision,
//! and a publication deadline translated into the child's raw
//! `CLOCK_MONOTONIC` nanoseconds. Decoding authorizes nothing, and every
//! reserved byte and unused body byte must be zero. Debug output names the
//! message kind only.
use super::{Endpoint, PlatformError, Publication, Stamp};
use crate::limits::ProtocolLimits;
use core::fmt;

pub const MAGIC: [u8; 4] = *b"FRCB";
pub const VERSION: u8 = 1;
pub const FRAME_BYTES: usize = 64;
const HEADER_BYTES: usize = 16;
const BODY_BYTES: usize = FRAME_BYTES - HEADER_BYTES;
const STAMP_BYTES: usize = 25;
/// The largest item any frame may announce: the absolute protocol ceiling.
/// `Hello` may only lower it for one child.
pub const MAX_ITEM_BYTES: u32 = ProtocolLimits::ABSOLUTE.max_clipboard_item_bytes();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecError {
    Magic,
    Version,
    Kind,
    Reserved,
    Padding,
    Sequence,
    Value,
    Length,
}

/// Host → child.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Request {
    /// Open the locally selected display. `epoch` is a private launch
    /// identity, echoed; `max_item_bytes` bounds every later payload.
    Hello {
        epoch: u128,
        max_item_bytes: u32,
    },
    /// Start a fresh `XFixes` subscription (never content polling).
    Watch,
    /// Service bounded native events and report the newest change.
    Changes,
    /// Followed by exactly `len` UTF-8 bytes. With `revision`, refuse unless
    /// that native revision is still current after preparation.
    Prepare {
        stamp: Stamp,
        revision: Option<u64>,
        len: u32,
    },
    /// Publish the prepared item only while the child's own `CLOCK_MONOTONIC`
    /// reading is strictly before `not_after_ns`, checked immediately before
    /// the native ownership call.
    Publish {
        stamp: Stamp,
        not_after_ns: u64,
    },
    CancelPrepared,
    BeginRead,
    PollRead,
    CancelRead,
    Suspend,
    Stop,
}
impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Hello { .. } => "Hello",
            Self::Watch => "Watch",
            Self::Changes => "Changes",
            Self::Prepare { .. } => "Prepare",
            Self::Publish { .. } => "Publish",
            Self::CancelPrepared => "CancelPrepared",
            Self::BeginRead => "BeginRead",
            Self::PollRead => "PollRead",
            Self::CancelRead => "CancelRead",
            Self::Suspend => "Suspend",
            Self::Stop => "Stop",
        })
    }
}

/// Native change metadata. Descriptive only; never bearer authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Change {
    pub revision: u64,
    pub has_selection: bool,
    pub origin: Option<Stamp>,
}

/// Typed, content-free native failures (watch, read or suspend).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    Platform(PlatformError),
    Unsupported,
    NotWatching,
    AlreadyWatching,
    Exhausted,
    Busy,
    NotReading,
    NoSelection,
    Expired,
    LocalChanged,
    Limit,
    Allocation,
    InvalidUtf8,
    Malformed,
}
impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

/// Child → host. Every reply after `Ready` carries the child's current native
/// change revision, so the host's revision view is exact after each exchange.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Reply {
    Ready {
        epoch: u128,
    },
    Refused(PlatformError),
    Watching {
        revision: u64,
    },
    Changes {
        revision: u64,
        latest: Option<Change>,
        settled: bool,
    },
    Prepared {
        revision: u64,
    },
    PrepareFailed {
        revision: u64,
        error: PlatformError,
    },
    Published {
        revision: u64,
        publication: Publication,
    },
    Done {
        revision: u64,
    },
    ReadPending {
        revision: u64,
    },
    /// Followed by exactly `len` validated UTF-8 bytes of ONE complete item.
    ReadText {
        revision: u64,
        origin: Option<Stamp>,
        len: u32,
    },
    Failed {
        revision: u64,
        failure: Failure,
    },
    Stopped,
}
impl fmt::Debug for Reply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(e) => f.debug_tuple("Refused").field(e).finish(),
            Self::PrepareFailed { error, .. } => {
                f.debug_tuple("PrepareFailed").field(error).finish()
            }
            Self::Published { publication, .. } => {
                f.debug_tuple("Published").field(publication).finish()
            }
            Self::Failed { failure, .. } => f.debug_tuple("Failed").field(failure).finish(),
            other => f.write_str(match other {
                Self::Ready { .. } => "Ready",
                Self::Watching { .. } => "Watching",
                Self::Changes { .. } => "Changes",
                Self::Prepared { .. } => "Prepared",
                Self::Done { .. } => "Done",
                Self::ReadPending { .. } => "ReadPending",
                Self::ReadText { .. } => "ReadText",
                _ => "Stopped",
            }),
        }
    }
}
impl Reply {
    /// The child's native revision after this exchange, when it carries one.
    pub const fn revision(self) -> Option<u64> {
        match self {
            Self::Watching { revision }
            | Self::Changes { revision, .. }
            | Self::Prepared { revision }
            | Self::PrepareFailed { revision, .. }
            | Self::Published { revision, .. }
            | Self::Done { revision }
            | Self::ReadPending { revision }
            | Self::ReadText { revision, .. }
            | Self::Failed { revision, .. } => Some(revision),
            Self::Ready { .. } | Self::Refused(_) | Self::Stopped => None,
        }
    }
}

pub fn encode_request(sequence: u64, request: Request) -> Result<[u8; FRAME_BYTES], CodecError> {
    let mut body = [0; BODY_BYTES];
    let kind = match request {
        Request::Hello {
            epoch,
            max_item_bytes,
        } => {
            if epoch == 0 || max_item_bytes == 0 || max_item_bytes > MAX_ITEM_BYTES {
                return Err(CodecError::Value);
            }
            body[..16].copy_from_slice(&epoch.to_be_bytes());
            body[16..20].copy_from_slice(&max_item_bytes.to_be_bytes());
            1
        }
        Request::Watch => 2,
        Request::Changes => 3,
        Request::Prepare {
            stamp,
            revision,
            len,
        } => {
            if len > MAX_ITEM_BYTES {
                return Err(CodecError::Length);
            }
            put_stamp(&mut body[..STAMP_BYTES], stamp)?;
            if let Some(revision) = revision {
                body[25] = 1;
                body[26..34].copy_from_slice(&revision.to_be_bytes());
            }
            body[34..38].copy_from_slice(&len.to_be_bytes());
            4
        }
        Request::Publish {
            stamp,
            not_after_ns,
        } => {
            if not_after_ns == 0 {
                return Err(CodecError::Value);
            }
            put_stamp(&mut body[..STAMP_BYTES], stamp)?;
            body[25..33].copy_from_slice(&not_after_ns.to_be_bytes());
            5
        }
        Request::CancelPrepared => 6,
        Request::BeginRead => 7,
        Request::PollRead => 8,
        Request::CancelRead => 9,
        Request::Suspend => 10,
        Request::Stop => 11,
    };
    frame(sequence, kind, &body)
}

/// `max_payload` is the item bound agreed in `Hello` (the absolute ceiling for
/// decoding `Hello` itself). A larger announced payload is refused here,
/// before the caller reserves or reads anything.
pub fn decode_request(bytes: &[u8], max_payload: u32) -> Result<(u64, Request), CodecError> {
    let (sequence, kind, body) = unframe(bytes)?;
    let request = match kind {
        1 => {
            zero(&body[20..])?;
            let epoch = u128::from_be_bytes(body[..16].try_into().expect("fixed"));
            let max_item_bytes = u32::from_be_bytes(body[16..20].try_into().expect("fixed"));
            if epoch == 0 || max_item_bytes == 0 || max_item_bytes > MAX_ITEM_BYTES {
                return Err(CodecError::Value);
            }
            Request::Hello {
                epoch,
                max_item_bytes,
            }
        }
        4 => {
            zero(&body[38..])?;
            let stamp = stamp(&body[..STAMP_BYTES])?;
            let revision = match body[25] {
                0 => {
                    zero(&body[26..34])?;
                    None
                }
                1 => Some(u64::from_be_bytes(body[26..34].try_into().expect("fixed"))),
                _ => return Err(CodecError::Value),
            };
            let len = u32::from_be_bytes(body[34..38].try_into().expect("fixed"));
            if len > max_payload.min(MAX_ITEM_BYTES) {
                return Err(CodecError::Length);
            }
            Request::Prepare {
                stamp,
                revision,
                len,
            }
        }
        5 => {
            zero(&body[33..])?;
            let stamp = stamp(&body[..STAMP_BYTES])?;
            let not_after_ns = u64::from_be_bytes(body[25..33].try_into().expect("fixed"));
            if not_after_ns == 0 {
                return Err(CodecError::Value);
            }
            Request::Publish {
                stamp,
                not_after_ns,
            }
        }
        2 | 3 | 6..=11 => {
            zero(body)?;
            match kind {
                2 => Request::Watch,
                3 => Request::Changes,
                6 => Request::CancelPrepared,
                7 => Request::BeginRead,
                8 => Request::PollRead,
                9 => Request::CancelRead,
                10 => Request::Suspend,
                _ => Request::Stop,
            }
        }
        _ => return Err(CodecError::Kind),
    };
    Ok((sequence, request))
}

pub fn encode_reply(sequence: u64, reply: Reply) -> Result<[u8; FRAME_BYTES], CodecError> {
    let mut body = [0; BODY_BYTES];
    if let Some(revision) = reply.revision() {
        body[..8].copy_from_slice(&revision.to_be_bytes());
    }
    let kind = match reply {
        Reply::Ready { epoch } => {
            if epoch == 0 {
                return Err(CodecError::Value);
            }
            body[..16].copy_from_slice(&epoch.to_be_bytes());
            0x81
        }
        Reply::Refused(error) => {
            body[0] = platform_code(error);
            0x82
        }
        Reply::Watching { .. } => 0x83,
        Reply::Changes {
            latest, settled, ..
        } => {
            let mut flags = u8::from(settled) << 2;
            if let Some(change) = latest {
                flags |= 1 | u8::from(change.has_selection) << 1;
                body[9..17].copy_from_slice(&change.revision.to_be_bytes());
                if let Some(origin) = change.origin {
                    flags |= 1 << 3;
                    put_stamp(&mut body[17..17 + STAMP_BYTES], origin)?;
                }
            }
            body[8] = flags;
            0x84
        }
        Reply::Prepared { .. } => 0x85,
        Reply::PrepareFailed { error, .. } => {
            body[8] = platform_code(error);
            0x86
        }
        Reply::Published { publication, .. } => {
            let (code, error) = match publication {
                Publication::SubmittedToOs => (1, 0),
                Publication::NotSubmitted(error) => (2, platform_code(error)),
                Publication::UnknownEffect => (3, 0),
            };
            body[8] = code;
            body[9] = error;
            0x87
        }
        Reply::Done { .. } => 0x88,
        Reply::ReadPending { .. } => 0x89,
        Reply::ReadText { origin, len, .. } => {
            if len > MAX_ITEM_BYTES {
                return Err(CodecError::Length);
            }
            body[8..12].copy_from_slice(&len.to_be_bytes());
            if let Some(origin) = origin {
                body[12] = 1;
                put_stamp(&mut body[13..13 + STAMP_BYTES], origin)?;
            }
            0x8a
        }
        Reply::Failed { failure, .. } => {
            let (code, platform) = failure_code(failure);
            body[8] = code;
            body[9] = platform;
            0x8b
        }
        Reply::Stopped => 0x8c,
    };
    frame(sequence, kind, &body)
}

/// See `decode_request` for `max_payload`.
pub fn decode_reply(bytes: &[u8], max_payload: u32) -> Result<(u64, Reply), CodecError> {
    let (sequence, kind, body) = unframe(bytes)?;
    let revision = u64::from_be_bytes(body[..8].try_into().expect("fixed"));
    let reply = match kind {
        0x81 => {
            zero(&body[16..])?;
            let epoch = u128::from_be_bytes(body[..16].try_into().expect("fixed"));
            if epoch == 0 {
                return Err(CodecError::Value);
            }
            Reply::Ready { epoch }
        }
        0x82 => {
            zero(&body[1..])?;
            Reply::Refused(platform_error(body[0])?)
        }
        0x84 => changes(body, revision)?,
        0x86 => {
            zero(&body[9..])?;
            Reply::PrepareFailed {
                revision,
                error: platform_error(body[8])?,
            }
        }
        0x87 => {
            zero(&body[10..])?;
            let publication = match (body[8], body[9]) {
                (1, 0) => Publication::SubmittedToOs,
                (2, code) => Publication::NotSubmitted(platform_error(code)?),
                (3, 0) => Publication::UnknownEffect,
                _ => return Err(CodecError::Value),
            };
            Reply::Published {
                revision,
                publication,
            }
        }
        0x8a => {
            zero(&body[13 + STAMP_BYTES..])?;
            let len = u32::from_be_bytes(body[8..12].try_into().expect("fixed"));
            if len > max_payload.min(MAX_ITEM_BYTES) {
                return Err(CodecError::Length);
            }
            let origin = match body[12] {
                0 => {
                    zero(&body[13..13 + STAMP_BYTES])?;
                    None
                }
                1 => Some(stamp(&body[13..13 + STAMP_BYTES])?),
                _ => return Err(CodecError::Value),
            };
            Reply::ReadText {
                revision,
                origin,
                len,
            }
        }
        0x8b => {
            zero(&body[10..])?;
            Reply::Failed {
                revision,
                failure: failure(body[8], body[9])?,
            }
        }
        0x83 | 0x85 | 0x88 | 0x89 => {
            zero(&body[8..])?;
            match kind {
                0x83 => Reply::Watching { revision },
                0x85 => Reply::Prepared { revision },
                0x88 => Reply::Done { revision },
                _ => Reply::ReadPending { revision },
            }
        }
        0x8c => {
            zero(body)?;
            Reply::Stopped
        }
        _ => return Err(CodecError::Kind),
    };
    Ok((sequence, reply))
}

/// A `Changes` reply body: flags bit 0 latest, 1 has-selection, 2 settled,
/// 3 origin; absent metadata must be zero.
fn changes(body: &[u8], revision: u64) -> Result<Reply, CodecError> {
    let flags = body[8];
    if flags & !0b1111 != 0 {
        return Err(CodecError::Value);
    }
    let latest = if flags & 1 == 0 {
        if flags & 0b1010 != 0 {
            return Err(CodecError::Value);
        }
        zero(&body[9..])?;
        None
    } else {
        zero(&body[17 + STAMP_BYTES..])?;
        let origin = if flags & 0b1000 == 0 {
            zero(&body[17..17 + STAMP_BYTES])?;
            None
        } else {
            Some(stamp(&body[17..17 + STAMP_BYTES])?)
        };
        Some(Change {
            revision: u64::from_be_bytes(body[9..17].try_into().expect("fixed")),
            has_selection: flags & 0b10 != 0,
            origin,
        })
    };
    Ok(Reply::Changes {
        revision,
        latest,
        settled: flags & 0b100 != 0,
    })
}
fn frame(
    sequence: u64,
    kind: u8,
    body: &[u8; BODY_BYTES],
) -> Result<[u8; FRAME_BYTES], CodecError> {
    if sequence == 0 {
        return Err(CodecError::Sequence);
    }
    let mut bytes = [0; FRAME_BYTES];
    bytes[..4].copy_from_slice(&MAGIC);
    bytes[4] = VERSION;
    bytes[5] = kind;
    bytes[8..16].copy_from_slice(&sequence.to_be_bytes());
    bytes[HEADER_BYTES..].copy_from_slice(body);
    Ok(bytes)
}
fn unframe(bytes: &[u8]) -> Result<(u64, u8, &[u8]), CodecError> {
    if bytes.len() != FRAME_BYTES {
        return Err(CodecError::Length);
    }
    if bytes[..4] != MAGIC {
        return Err(CodecError::Magic);
    }
    if bytes[4] != VERSION {
        return Err(CodecError::Version);
    }
    if bytes[6..8] != [0, 0] {
        return Err(CodecError::Reserved);
    }
    let sequence = u64::from_be_bytes(bytes[8..16].try_into().expect("fixed"));
    if sequence == 0 {
        return Err(CodecError::Sequence);
    }
    Ok((sequence, bytes[5], &bytes[HEADER_BYTES..]))
}
fn zero(bytes: &[u8]) -> Result<(), CodecError> {
    if bytes.iter().all(|b| *b == 0) {
        Ok(())
    } else {
        Err(CodecError::Padding)
    }
}
fn put_stamp(out: &mut [u8], stamp: Stamp) -> Result<(), CodecError> {
    if !stamp.valid() {
        return Err(CodecError::Value);
    }
    out[..16].copy_from_slice(&stamp.id.to_be_bytes());
    out[16] = stamp.source as u8;
    out[17..25].copy_from_slice(&stamp.sequence.to_be_bytes());
    Ok(())
}
fn stamp(bytes: &[u8]) -> Result<Stamp, CodecError> {
    let stamp = Stamp {
        id: u128::from_be_bytes(bytes[..16].try_into().map_err(|_| CodecError::Length)?),
        source: match bytes[16] {
            1 => Endpoint::Host,
            2 => Endpoint::Controller,
            _ => return Err(CodecError::Value),
        },
        sequence: u64::from_be_bytes(bytes[17..25].try_into().map_err(|_| CodecError::Length)?),
    };
    if !stamp.valid() {
        return Err(CodecError::Value);
    }
    Ok(stamp)
}
const fn platform_code(error: PlatformError) -> u8 {
    match error {
        PlatformError::Unsupported => 1,
        PlatformError::Permission => 2,
        PlatformError::Unavailable => 3,
        PlatformError::LocalChanged => 4,
    }
}
fn platform_error(code: u8) -> Result<PlatformError, CodecError> {
    Ok(match code {
        1 => PlatformError::Unsupported,
        2 => PlatformError::Permission,
        3 => PlatformError::Unavailable,
        4 => PlatformError::LocalChanged,
        _ => return Err(CodecError::Value),
    })
}
const fn failure_code(failure: Failure) -> (u8, u8) {
    match failure {
        Failure::Platform(error) => (1, platform_code(error)),
        Failure::Unsupported => (2, 0),
        Failure::NotWatching => (3, 0),
        Failure::AlreadyWatching => (4, 0),
        Failure::Exhausted => (5, 0),
        Failure::Busy => (6, 0),
        Failure::NotReading => (7, 0),
        Failure::NoSelection => (8, 0),
        Failure::Expired => (9, 0),
        Failure::LocalChanged => (10, 0),
        Failure::Limit => (11, 0),
        Failure::Allocation => (12, 0),
        Failure::InvalidUtf8 => (13, 0),
        Failure::Malformed => (14, 0),
    }
}
fn failure(code: u8, platform: u8) -> Result<Failure, CodecError> {
    if code != 1 && platform != 0 {
        return Err(CodecError::Padding);
    }
    Ok(match code {
        1 => Failure::Platform(platform_error(platform)?),
        2 => Failure::Unsupported,
        3 => Failure::NotWatching,
        4 => Failure::AlreadyWatching,
        5 => Failure::Exhausted,
        6 => Failure::Busy,
        7 => Failure::NotReading,
        8 => Failure::NoSelection,
        9 => Failure::Expired,
        10 => Failure::LocalChanged,
        11 => Failure::Limit,
        12 => Failure::Allocation,
        13 => Failure::InvalidUtf8,
        14 => Failure::Malformed,
        _ => return Err(CodecError::Value),
    })
}

#[cfg(test)]
mod tests;
