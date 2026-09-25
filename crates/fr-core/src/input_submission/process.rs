//! Private IPC between the host's canonical input owner and ONE per-lease
//! out-of-process injection executor (`fr-input-agent`). Both sides link only
//! this safe codec; the executor never links the broker.
//!
//! Command channel: fixed 64-byte frames on a private `SOCK_STREAM` socketpair,
//! exactly one outstanding request, sequence numbers starting at 1 and
//! increasing by one; every reply echoes its request's sequence. Signal
//! channel: fixed 16-byte datagrams on a private `SOCK_DGRAM` socketpair, so a
//! fence reaches the executor while a request is in flight.
//!
//! No lease, ticket, nonce, session identity or peer data crosses: only the
//! derived exclusive deadline, translated into the executor's raw
//! `CLOCK_MONOTONIC` nanoseconds. Decoding authorizes nothing, and every
//! reserved byte and unused body byte must be zero. Debug hides operations.
use super::{Capabilities, Operation, PlatformError, Submission};
use crate::{
    input::{DesktopPoint, InputBounds, KeyTransition, PhysicalKey, PointerButton, ScrollUnit},
    input_submission::scroll::WheelDirection,
    time::HostInstant,
};
use core::fmt;

pub const MAGIC: [u8; 4] = *b"FRIA";
pub const SIGNAL_MAGIC: [u8; 4] = *b"FRIF";
pub const VERSION: u8 = 1;
pub const FRAME_BYTES: usize = 64;
pub const SIGNAL_BYTES: usize = 16;
const HEADER_BYTES: usize = 16;
const BODY_BYTES: usize = FRAME_BYTES - HEADER_BYTES;
/// Every capability bit `Capabilities::with` can currently set.
const CAPABILITY_BITS: u16 = (1 << 8) - 1;

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

/// Host → executor. `Release` carries release-only transitions; it is the only
/// input an executor accepts without a deadline, including after a fence.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Request {
    /// Open the locally selected display and revalidate exact bounds and the
    /// negotiated capabilities. `epoch` is a private launch identity, echoed.
    Hello {
        epoch: u128,
        bounds: InputBounds,
        required: Capabilities,
    },
    Prepare(Operation),
    /// Submit the matching prepared operation only while the executor's own
    /// `CLOCK_MONOTONIC` reading is strictly before `not_after_ns`.
    Submit {
        operation: Operation,
        not_after_ns: u64,
    },
    Release(Operation),
    Cancel,
    Cleanup,
    Stop,
}
impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Hello { .. } => "Hello",
            Self::Prepare(_) => "Prepare",
            Self::Submit { .. } => "Submit",
            Self::Release(_) => "Release",
            Self::Cancel => "Cancel",
            Self::Cleanup => "Cleanup",
            Self::Stop => "Stop",
        })
    }
}

/// Executor → host. Submission-stage replies name API submission or its
/// confirmed absence, never an application effect.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Reply {
    Ready {
        epoch: u128,
        capabilities: Capabilities,
        repeat_requires_pair: bool,
        line_scroll_requires_pairs: bool,
    },
    Refused(PlatformError),
    Prepared,
    PrepareFailed(PlatformError),
    Submitted,
    NotSubmitted(PlatformError),
    Unknown,
    Expired,
    Fenced,
    Cancelled,
    Cleaned(bool),
    Stopped,
}
impl Reply {
    /// The submission-stage meaning of this reply, if it is one.
    pub const fn submission(self) -> Option<Submission> {
        Some(match self {
            Self::Submitted => Submission::Submitted,
            Self::NotSubmitted(error) => Submission::NotSubmitted(error),
            Self::Unknown => Submission::Unknown,
            Self::Expired => Submission::Expired,
            Self::Fenced => Submission::Fenced,
            _ => return None,
        })
    }
}
impl fmt::Debug for Reply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ready { capabilities, .. } => f
                .debug_struct("Ready")
                .field("capabilities", capabilities)
                .finish_non_exhaustive(),
            Self::Refused(e) => f.debug_tuple("Refused").field(e).finish(),
            Self::Prepared => f.write_str("Prepared"),
            Self::PrepareFailed(e) => f.debug_tuple("PrepareFailed").field(e).finish(),
            Self::Submitted => f.write_str("Submitted"),
            Self::NotSubmitted(e) => f.debug_tuple("NotSubmitted").field(e).finish(),
            Self::Unknown => f.write_str("Unknown"),
            Self::Expired => f.write_str("Expired"),
            Self::Fenced => f.write_str("Fenced"),
            Self::Cancelled => f.write_str("Cancelled"),
            Self::Cleaned(done) => f.debug_tuple("Cleaned").field(done).finish(),
            Self::Stopped => f.write_str("Stopped"),
        }
    }
}

/// Signal-channel datagrams. `Fence` travels host → executor only;
/// `LocalRevoke` (the local sharing indicator was used) executor → host only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Fence,
    LocalRevoke,
}

/// Translate the owner's exclusive deadline into the executor's raw monotonic
/// nanoseconds. Sample `monotonic_ns` BEFORE `now` (the owner's runtime clock
/// with the same rate), so the result errs early by the sampling gap. `None`
/// means already expired or unrepresentable: send nothing, report expiry.
pub fn not_after_ns(monotonic_ns: u64, now: HostInstant, until: HostInstant) -> Option<u64> {
    let remaining = until.checked_duration_since(now)?.as_micros();
    if remaining == 0 {
        return None;
    }
    monotonic_ns.checked_add(remaining.checked_mul(1_000)?)
}

pub fn encode_request(sequence: u64, request: Request) -> Result<[u8; FRAME_BYTES], CodecError> {
    let mut body = [0; BODY_BYTES];
    let kind = match request {
        Request::Hello {
            epoch,
            bounds,
            required,
        } => {
            if epoch == 0 {
                return Err(CodecError::Value);
            }
            body[..16].copy_from_slice(&epoch.to_be_bytes());
            body[16..20].copy_from_slice(&bounds.origin().x.to_be_bytes());
            body[20..24].copy_from_slice(&bounds.origin().y.to_be_bytes());
            body[24..28].copy_from_slice(&bounds.width().to_be_bytes());
            body[28..32].copy_from_slice(&bounds.height().to_be_bytes());
            body[32..34].copy_from_slice(&required.0.to_be_bytes());
            1
        }
        Request::Prepare(operation) => {
            put_operation(&mut body, operation);
            2
        }
        Request::Submit {
            operation,
            not_after_ns,
        } => {
            if not_after_ns == 0 {
                return Err(CodecError::Value);
            }
            put_operation(&mut body, operation);
            body[16..24].copy_from_slice(&not_after_ns.to_be_bytes());
            3
        }
        Request::Release(operation) => {
            if !operation.is_release() {
                return Err(CodecError::Value);
            }
            put_operation(&mut body, operation);
            4
        }
        Request::Cancel => 5,
        Request::Cleanup => 6,
        Request::Stop => 7,
    };
    frame(sequence, kind, &body)
}

pub fn decode_request(bytes: &[u8]) -> Result<(u64, Request), CodecError> {
    let (sequence, kind, body) = unframe(bytes)?;
    let request = match kind {
        1 => {
            let epoch = u128::from_be_bytes(body[..16].try_into().expect("fixed"));
            let bounds = InputBounds::new(
                DesktopPoint {
                    x: i32::from_be_bytes(body[16..20].try_into().expect("fixed")),
                    y: i32::from_be_bytes(body[20..24].try_into().expect("fixed")),
                },
                u32::from_be_bytes(body[24..28].try_into().expect("fixed")),
                u32::from_be_bytes(body[28..32].try_into().expect("fixed")),
            )
            .ok_or(CodecError::Value)?;
            let required = capabilities(&body[32..34])?;
            zero(&body[34..])?;
            if epoch == 0 {
                return Err(CodecError::Value);
            }
            Request::Hello {
                epoch,
                bounds,
                required,
            }
        }
        2 => {
            zero(&body[16..])?;
            Request::Prepare(operation(&body[..16])?)
        }
        3 => {
            let operation = operation(&body[..16])?;
            let not_after_ns = u64::from_be_bytes(body[16..24].try_into().expect("fixed"));
            zero(&body[24..])?;
            if not_after_ns == 0 {
                return Err(CodecError::Value);
            }
            Request::Submit {
                operation,
                not_after_ns,
            }
        }
        4 => {
            zero(&body[16..])?;
            let operation = operation(&body[..16])?;
            if !operation.is_release() {
                return Err(CodecError::Value);
            }
            Request::Release(operation)
        }
        5..=7 => {
            zero(body)?;
            match kind {
                5 => Request::Cancel,
                6 => Request::Cleanup,
                _ => Request::Stop,
            }
        }
        _ => return Err(CodecError::Kind),
    };
    Ok((sequence, request))
}

pub fn encode_reply(sequence: u64, reply: Reply) -> Result<[u8; FRAME_BYTES], CodecError> {
    let mut body = [0; BODY_BYTES];
    let kind = match reply {
        Reply::Ready {
            epoch,
            capabilities,
            repeat_requires_pair,
            line_scroll_requires_pairs,
        } => {
            if epoch == 0 {
                return Err(CodecError::Value);
            }
            body[..16].copy_from_slice(&epoch.to_be_bytes());
            body[16..18].copy_from_slice(&capabilities.0.to_be_bytes());
            body[18] = u8::from(repeat_requires_pair) | u8::from(line_scroll_requires_pairs) << 1;
            0x81
        }
        Reply::Refused(error) => {
            body[0] = platform_code(error);
            0x82
        }
        Reply::Prepared => 0x83,
        Reply::PrepareFailed(error) => {
            body[0] = platform_code(error);
            0x84
        }
        Reply::Submitted => 0x85,
        Reply::NotSubmitted(error) => {
            body[0] = platform_code(error);
            0x86
        }
        Reply::Unknown => 0x87,
        Reply::Expired => 0x88,
        Reply::Fenced => 0x89,
        Reply::Cancelled => 0x8a,
        Reply::Cleaned(done) => {
            body[0] = u8::from(done);
            0x8b
        }
        Reply::Stopped => 0x8c,
    };
    frame(sequence, kind, &body)
}

pub fn decode_reply(bytes: &[u8]) -> Result<(u64, Reply), CodecError> {
    let (sequence, kind, body) = unframe(bytes)?;
    let reply = match kind {
        0x81 => {
            let epoch = u128::from_be_bytes(body[..16].try_into().expect("fixed"));
            let capabilities = capabilities(&body[16..18])?;
            let flags = body[18];
            zero(&body[19..])?;
            if epoch == 0 || flags & !0b11 != 0 {
                return Err(CodecError::Value);
            }
            Reply::Ready {
                epoch,
                capabilities,
                repeat_requires_pair: flags & 1 != 0,
                line_scroll_requires_pairs: flags & 2 != 0,
            }
        }
        0x82 | 0x84 | 0x86 => {
            zero(&body[1..])?;
            let error = platform_error(body[0])?;
            match kind {
                0x82 => Reply::Refused(error),
                0x84 => Reply::PrepareFailed(error),
                _ => Reply::NotSubmitted(error),
            }
        }
        0x8b => {
            zero(&body[1..])?;
            Reply::Cleaned(flag(body[0])?)
        }
        0x83 | 0x85 | 0x87..=0x8a | 0x8c => {
            zero(body)?;
            match kind {
                0x83 => Reply::Prepared,
                0x85 => Reply::Submitted,
                0x87 => Reply::Unknown,
                0x88 => Reply::Expired,
                0x89 => Reply::Fenced,
                0x8a => Reply::Cancelled,
                _ => Reply::Stopped,
            }
        }
        _ => return Err(CodecError::Kind),
    };
    Ok((sequence, reply))
}

pub fn encode_signal(signal: Signal) -> [u8; SIGNAL_BYTES] {
    let mut bytes = [0; SIGNAL_BYTES];
    bytes[..4].copy_from_slice(&SIGNAL_MAGIC);
    bytes[4] = VERSION;
    bytes[5] = match signal {
        Signal::Fence => 1,
        Signal::LocalRevoke => 2,
    };
    bytes
}

/// A datagram of any other length (including a truncated read) is refused.
pub fn decode_signal(bytes: &[u8]) -> Result<Signal, CodecError> {
    if bytes.len() != SIGNAL_BYTES {
        return Err(CodecError::Length);
    }
    if bytes[..4] != SIGNAL_MAGIC {
        return Err(CodecError::Magic);
    }
    if bytes[4] != VERSION {
        return Err(CodecError::Version);
    }
    zero(&bytes[6..])?;
    match bytes[5] {
        1 => Ok(Signal::Fence),
        2 => Ok(Signal::LocalRevoke),
        _ => Err(CodecError::Kind),
    }
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
fn flag(byte: u8) -> Result<bool, CodecError> {
    match byte {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(CodecError::Value),
    }
}
fn capabilities(bytes: &[u8]) -> Result<Capabilities, CodecError> {
    let bits = u16::from_be_bytes(bytes.try_into().map_err(|_| CodecError::Length)?);
    if bits & !CAPABILITY_BITS != 0 {
        return Err(CodecError::Value);
    }
    Ok(Capabilities(bits))
}
const fn platform_code(error: PlatformError) -> u8 {
    match error {
        PlatformError::Unsupported => 1,
        PlatformError::Permission => 2,
        PlatformError::GeometryChanged => 3,
        PlatformError::Unavailable => 4,
    }
}
fn platform_error(code: u8) -> Result<PlatformError, CodecError> {
    Ok(match code {
        1 => PlatformError::Unsupported,
        2 => PlatformError::Permission,
        3 => PlatformError::GeometryChanged,
        4 => PlatformError::Unavailable,
        _ => return Err(CodecError::Value),
    })
}
fn put_operation(body: &mut [u8; BODY_BYTES], operation: Operation) {
    let point = |body: &mut [u8; BODY_BYTES], x: i32, y: i32| {
        body[1..5].copy_from_slice(&x.to_be_bytes());
        body[5..9].copy_from_slice(&y.to_be_bytes());
    };
    match operation {
        Operation::Key { key, transition } => {
            body[0] = 1;
            body[1..3].copy_from_slice(&key.usage().to_be_bytes());
            body[3] = transition as u8;
        }
        Operation::Absolute(position) => {
            body[0] = 2;
            point(body, position.x, position.y);
        }
        Operation::Button { button, pressed } => {
            body[0] = 3;
            body[1] = button as u8;
            body[2] = u8::from(pressed);
        }
        Operation::Relative { x, y } => {
            body[0] = 4;
            point(body, x, y);
        }
        Operation::Scroll { x, y, unit } => {
            body[0] = 5;
            point(body, x, y);
            body[9] = unit as u8;
        }
        Operation::Wheel { direction, pressed } => {
            body[0] = 6;
            body[1] = match direction {
                WheelDirection::Left => 0,
                WheelDirection::Right => 1,
                WheelDirection::Up => 2,
                WheelDirection::Down => 3,
            };
            body[2] = u8::from(pressed);
        }
        Operation::Text(scalar) => {
            body[0] = 7;
            body[1..5].copy_from_slice(&u32::from(scalar).to_be_bytes());
        }
    }
}
fn operation(bytes: &[u8]) -> Result<Operation, CodecError> {
    let int = |at: usize| i32::from_be_bytes(bytes[at..at + 4].try_into().expect("fixed"));
    let (operation, used) = match bytes[0] {
        1 => {
            let key = PhysicalKey::new(u16::from_be_bytes([bytes[1], bytes[2]]))
                .ok_or(CodecError::Value)?;
            let transition = match bytes[3] {
                0 => KeyTransition::Release,
                1 => KeyTransition::Press,
                2 => KeyTransition::Repeat,
                _ => return Err(CodecError::Value),
            };
            (Operation::Key { key, transition }, 4)
        }
        2 => (
            Operation::Absolute(DesktopPoint {
                x: int(1),
                y: int(5),
            }),
            9,
        ),
        3 => {
            let button = match bytes[1] {
                1 => PointerButton::Primary,
                2 => PointerButton::Secondary,
                3 => PointerButton::Middle,
                4 => PointerButton::Back,
                5 => PointerButton::Forward,
                _ => return Err(CodecError::Value),
            };
            (
                Operation::Button {
                    button,
                    pressed: flag(bytes[2])?,
                },
                3,
            )
        }
        4 => (
            Operation::Relative {
                x: int(1),
                y: int(5),
            },
            9,
        ),
        5 => {
            let unit = match bytes[9] {
                0 => ScrollUnit::Pixels,
                1 => ScrollUnit::Lines,
                _ => return Err(CodecError::Value),
            };
            (
                Operation::Scroll {
                    x: int(1),
                    y: int(5),
                    unit,
                },
                10,
            )
        }
        6 => {
            let direction = match bytes[1] {
                0 => WheelDirection::Left,
                1 => WheelDirection::Right,
                2 => WheelDirection::Up,
                3 => WheelDirection::Down,
                _ => return Err(CodecError::Value),
            };
            (
                Operation::Wheel {
                    direction,
                    pressed: flag(bytes[2])?,
                },
                3,
            )
        }
        7 => {
            let scalar = char::from_u32(u32::from_be_bytes(bytes[1..5].try_into().expect("fixed")))
                .ok_or(CodecError::Value)?;
            (Operation::Text(scalar), 5)
        }
        _ => return Err(CodecError::Value),
    };
    zero(&bytes[used..])?;
    Ok(operation)
}

#[cfg(test)]
mod tests;
