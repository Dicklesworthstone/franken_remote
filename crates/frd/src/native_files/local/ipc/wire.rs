//! Fixed metadata frames and bounded raw Unix path arguments.
use super::{
    Cleanup, Error, MAX_NAME, MAX_PATH_BYTES, MAX_PATHS, MAX_TOTAL_PATH_BYTES, Phase,
    SelectionFailure, Stage, Status,
};
use std::{
    ffi::OsString,
    fmt::Write as _,
    os::unix::ffi::OsStrExt,
    os::unix::ffi::OsStringExt,
    path::{Component, PathBuf},
};

pub(super) const IDENTIFY: [u8; 8] = *b"FRFS1ID\0";
const IDENTITY: [u8; 8] = *b"FRFS1ME\0";
const SEND: [u8; 8] = *b"FRFS1SN\0";
const STATUS: [u8; 8] = *b"FRFS1ST\0";
const CANCEL: [u8; 8] = *b"FRFS1CA\0";
const RESULT: [u8; 8] = *b"FRFS1RS\0";
/// Both versions retain the original fixed frame and path budgets. V1 never
/// emits pacing fields or admits pacing commands; V2 uses two reserved status
/// bytes for independent requested/observed tri-state samples.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Version {
    V1,
    V2,
}
impl Version {
    fn tag(self, mut tag: [u8; 8]) -> [u8; 8] {
        tag[4] = match self {
            Self::V1 => b'1',
            Self::V2 => b'2',
        };
        tag
    }
    pub(super) fn identify(self) -> [u8; 8] {
        self.tag(IDENTIFY)
    }
    pub(super) fn from_identify(bytes: &[u8]) -> Result<Self, Error> {
        match bytes {
            b"FRFS1ID\0" => Ok(Self::V1),
            b"FRFS2ID\0" => Ok(Self::V2),
            _ => Err(Error::Protocol),
        }
    }
}
const PAUSE: [u8; 8] = *b"FRFS1PA\0";
const RESUME: [u8; 8] = *b"FRFS1RE\0";
const STATUS_BYTES: usize = 48;
pub(super) const IDENTITY_BYTES: usize = 26 + STATUS_BYTES;
pub(super) const COMMAND_BYTES: usize = 38;
pub(super) const RESULT_BYTES: usize = 28 + STATUS_BYTES;
pub(super) const MAX_PAYLOAD: usize = MAX_TOTAL_PATH_BYTES + 2 * MAX_PATHS;

#[derive(Clone, Copy)]
pub(super) enum Command {
    Send(usize),
    Status(Option<u64>),
    Cancel(u64),
    Pause(u64),
    Resume(u64),
}
pub(super) fn command(
    version: Version,
    command: Command,
    instance: [u8; 16],
    length: usize,
) -> Result<[u8; COMMAND_BYTES], Error> {
    let mut bytes = [0; COMMAND_BYTES];
    let (tag, sequence, count) = match command {
        Command::Send(count) if (1..=MAX_PATHS).contains(&count) => (SEND, 0, count),
        Command::Status(sequence) => (STATUS, sequence.unwrap_or(0), 0),
        Command::Cancel(sequence) if sequence != 0 => (CANCEL, sequence, 0),
        Command::Pause(sequence) if version == Version::V2 && sequence != 0 => (PAUSE, sequence, 0),
        Command::Resume(sequence) if version == Version::V2 && sequence != 0 => {
            (RESUME, sequence, 0)
        }
        _ => return Err(Error::InvalidRequest),
    };
    bytes[..8].copy_from_slice(&version.tag(tag));
    bytes[8..24].copy_from_slice(&instance);
    bytes[24..32].copy_from_slice(&sequence.to_be_bytes());
    bytes[32..34].copy_from_slice(
        &u16::try_from(count)
            .map_err(|_| Error::InvalidPaths)?
            .to_be_bytes(),
    );
    bytes[34..38].copy_from_slice(
        &u32::try_from(length)
            .map_err(|_| Error::InvalidPaths)?
            .to_be_bytes(),
    );
    header(version, &bytes)?;
    Ok(bytes)
}
pub(super) fn header(
    version: Version,
    bytes: &[u8; COMMAND_BYTES],
) -> Result<(Command, [u8; 16], usize), Error> {
    let mut instance = [0; 16];
    instance.copy_from_slice(&bytes[8..24]);
    let sequence = u64::from_be_bytes(bytes[24..32].try_into().map_err(|_| Error::Protocol)?);
    let count = usize::from(u16::from_be_bytes([bytes[32], bytes[33]]));
    let length = usize::try_from(u32::from_be_bytes(
        bytes[34..38].try_into().map_err(|_| Error::Protocol)?,
    ))
    .map_err(|_| Error::Protocol)?;
    let command = if bytes[..8] == version.tag(SEND) {
        if sequence != 0
            || !(1..=MAX_PATHS).contains(&count)
            || !(3..=MAX_PAYLOAD).contains(&length)
        {
            return Err(Error::InvalidPaths);
        }
        Command::Send(count)
    } else if bytes[..8] == version.tag(STATUS) && count == 0 && length == 0 {
        Command::Status((sequence != 0).then_some(sequence))
    } else if bytes[..8] == version.tag(CANCEL) && sequence != 0 && count == 0 && length == 0 {
        Command::Cancel(sequence)
    } else if version == Version::V2
        && sequence != 0
        && count == 0
        && length == 0
        && bytes[..8] == version.tag(PAUSE)
    {
        Command::Pause(sequence)
    } else if version == Version::V2
        && sequence != 0
        && count == 0
        && length == 0
        && bytes[..8] == version.tag(RESUME)
    {
        Command::Resume(sequence)
    } else {
        return Err(Error::Protocol);
    };
    Ok((command, instance, length))
}
pub(super) fn paths(paths: &[PathBuf]) -> Result<Vec<u8>, Error> {
    if !(1..=MAX_PATHS).contains(&paths.len()) {
        return Err(Error::InvalidPaths);
    }
    let mut bytes = Vec::new();
    for path in paths {
        let raw = path.as_os_str().as_bytes();
        if !path.is_absolute()
            || raw.is_empty()
            || raw.len() > MAX_PATH_BYTES
            || raw.contains(&0)
            || path
                .components()
                .any(|component| component == Component::ParentDir)
        {
            return Err(Error::InvalidPaths);
        }
        let length = u16::try_from(raw.len()).map_err(|_| Error::InvalidPaths)?;
        bytes.extend_from_slice(&length.to_be_bytes());
        bytes.extend_from_slice(raw);
    }
    if bytes.len() > MAX_PAYLOAD {
        return Err(Error::InvalidPaths);
    }
    Ok(bytes)
}
pub(super) fn decode_paths(mut bytes: &[u8], count: usize) -> Result<Vec<PathBuf>, Error> {
    if !(1..=MAX_PATHS).contains(&count) || bytes.len() > MAX_PAYLOAD {
        return Err(Error::InvalidPaths);
    }
    let mut paths = Vec::with_capacity(count);
    let mut total = 0usize;
    for _ in 0..count {
        let size = bytes.get(..2).ok_or(Error::InvalidPaths)?;
        let length = usize::from(u16::from_be_bytes([size[0], size[1]]));
        if !(1..=MAX_PATH_BYTES).contains(&length) {
            return Err(Error::InvalidPaths);
        }
        bytes = &bytes[2..];
        let raw = bytes.get(..length).ok_or(Error::InvalidPaths)?;
        if raw[0] != b'/' || raw.contains(&0) {
            return Err(Error::InvalidPaths);
        }
        total = total.checked_add(length).ok_or(Error::InvalidPaths)?;
        if total > MAX_TOTAL_PATH_BYTES {
            return Err(Error::InvalidPaths);
        }
        let path = PathBuf::from(OsString::from_vec(raw.to_vec()));
        if path
            .components()
            .any(|component| component == Component::ParentDir)
        {
            return Err(Error::InvalidPaths);
        }
        paths.push(path);
        bytes = &bytes[length..];
    }
    if !bytes.is_empty() {
        return Err(Error::InvalidPaths);
    }
    Ok(paths)
}

pub(super) fn identity_record(
    version: Version,
    target: &str,
    instance: [u8; 16],
    status: Status,
) -> Vec<u8> {
    let mut bytes = vec![0; IDENTITY_BYTES];
    bytes[..8].copy_from_slice(&version.tag(IDENTITY));
    bytes[8..24].copy_from_slice(&instance);
    bytes[24..26].copy_from_slice(
        &u16::try_from(target.len())
            .expect("validated local label")
            .to_be_bytes(),
    );
    encode_status(version, status, &mut bytes[26..]);
    bytes.extend_from_slice(target.as_bytes());
    bytes
}
pub(super) fn identity(
    version: Version,
    bytes: &[u8; IDENTITY_BYTES],
) -> Result<([u8; 16], usize, Status), Error> {
    if bytes[..8] != version.tag(IDENTITY) {
        return Err(Error::Protocol);
    }
    let mut instance = [0; 16];
    instance.copy_from_slice(&bytes[8..24]);
    let length = usize::from(u16::from_be_bytes([bytes[24], bytes[25]]));
    if !(1..=MAX_NAME).contains(&length) {
        return Err(Error::Protocol);
    }
    Ok((instance, length, decode_status(version, &bytes[26..])?))
}
pub(super) fn result_record(
    version: Version,
    instance: [u8; 16],
    result: Result<(Status, bool), Error>,
) -> Vec<u8> {
    let mut bytes = vec![0; RESULT_BYTES];
    bytes[..8].copy_from_slice(&version.tag(RESULT));
    bytes[8..24].copy_from_slice(&instance);
    match result {
        Ok((status, changed)) => {
            bytes[27] = u8::from(changed);
            encode_status(version, status, &mut bytes[28..]);
        }
        Err(error) => {
            let (code, index) = error_code(error);
            bytes[24..26].copy_from_slice(&code.to_be_bytes());
            bytes[26] = index;
        }
    }
    bytes
}
pub(super) fn result(
    version: Version,
    bytes: &[u8; RESULT_BYTES],
    instance: [u8; 16],
) -> Result<(Status, bool), Error> {
    if bytes[..8] != version.tag(RESULT) || bytes[8..24] != instance || bytes[27] > 1 {
        return Err(Error::OutcomeUnknown);
    }
    let code = u16::from_be_bytes([bytes[24], bytes[25]]);
    if code != 0 {
        if bytes[27..].iter().any(|byte| *byte != 0) {
            return Err(Error::OutcomeUnknown);
        }
        return Err(decode_error(code, bytes[26]).unwrap_or(Error::OutcomeUnknown));
    }
    if bytes[26] != 0 {
        return Err(Error::OutcomeUnknown);
    }
    Ok((
        decode_status(version, &bytes[28..]).map_err(|_| Error::OutcomeUnknown)?,
        bytes[27] != 0,
    ))
}
fn encode_status(version: Version, status: Status, bytes: &mut [u8]) {
    bytes[0] = u8::from(status.ready);
    bytes[1] = u8::from(status.closed);
    bytes[2] = status.phase as u8;
    bytes[3] = status.stage as u8;
    bytes[4] = status.cleanup as u8;
    bytes[5] = u8::from(status.complete)
        | u8::from(status.failed) << 1
        | u8::from(status.cancel_requested) << 2
        | u8::from(status.queued_bytes.is_some()) << 3
        | u8::from(status.total_bytes.is_some()) << 4;
    if version == Version::V2 {
        let observation = |value: Option<bool>| value.map_or(0, |value| 1 + u8::from(value));
        bytes[6] = observation(status.pause_requested);
        bytes[7] = observation(status.paused);
    }
    bytes[8..16].copy_from_slice(&status.sequence.to_be_bytes());
    for (offset, value) in [
        status.selected,
        status.started,
        status.published,
        status.refused,
        status.interrupted,
        status.publication_unknown,
        status.durability_unknown,
    ]
    .into_iter()
    .enumerate()
    {
        bytes[16 + offset * 2..18 + offset * 2].copy_from_slice(&value.to_be_bytes());
    }
    bytes[32..40].copy_from_slice(&status.queued_bytes.unwrap_or(0).to_be_bytes());
    bytes[40..48].copy_from_slice(&status.total_bytes.unwrap_or(0).to_be_bytes());
}
fn decode_status(version: Version, bytes: &[u8]) -> Result<Status, Error> {
    if bytes.len() != STATUS_BYTES
        || bytes[0] > 1
        || bytes[1] > 1
        || bytes[5] & !31 != 0
        || (version == Version::V1 && bytes[6..8] != [0; 2])
        || bytes[6] > 2
        || bytes[7] > 2
        || bytes[30..32] != [0; 2]
    {
        return Err(Error::Protocol);
    }
    let phase = match bytes[2] {
        0 => Phase::NoSelection,
        1 => Phase::WaitingForControl,
        2 => Phase::Negotiating,
        3 => Phase::WaitingForSelection,
        4 => Phase::SelectionAccepted,
        5 => Phase::Sending,
        6 => Phase::Finished,
        7 => Phase::Ended,
        _ => return Err(Error::Protocol),
    };
    let stage = match bytes[3] {
        0 => Stage::Unknown,
        1 => Stage::Idle,
        2 => Stage::Queued,
        3 => Stage::Preparing,
        4 => Stage::AwaitingAcceptance,
        5 => Stage::Streaming,
        6 => Stage::AwaitingProof,
        7 => Stage::Finished,
        8 => Stage::Closed,
        _ => return Err(Error::Protocol),
    };
    let cleanup = match bytes[4] {
        0 => Cleanup::Unknown,
        1 => Cleanup::Confirmed,
        2 => Cleanup::Failed,
        _ => return Err(Error::Protocol),
    };
    let number = |offset| u16::from_be_bytes([bytes[offset], bytes[offset + 1]]);
    let status = Status {
        sequence: u64::from_be_bytes(bytes[8..16].try_into().map_err(|_| Error::Protocol)?),
        ready: bytes[0] != 0,
        closed: bytes[1] != 0,
        phase,
        stage,
        cleanup,
        selected: number(16),
        started: number(18),
        published: number(20),
        refused: number(22),
        interrupted: number(24),
        publication_unknown: number(26),
        durability_unknown: number(28),
        complete: bytes[5] & 1 != 0,
        failed: bytes[5] & 2 != 0,
        cancel_requested: bytes[5] & 4 != 0,
        pause_requested: (bytes[6] != 0).then_some(bytes[6] == 2),
        paused: (bytes[7] != 0).then_some(bytes[7] == 2),
        queued_bytes: (bytes[5] & 8 != 0)
            .then(|| u64::from_be_bytes(bytes[32..40].try_into().expect("fixed status"))),
        total_bytes: (bytes[5] & 16 != 0)
            .then(|| u64::from_be_bytes(bytes[40..48].try_into().expect("fixed status"))),
    };
    if usize::from(status.selected) > MAX_PATHS
        || status.started > status.selected
        || u32::from(status.published)
            + u32::from(status.refused)
            + u32::from(status.interrupted)
            + u32::from(status.publication_unknown)
            > u32::from(status.started)
        || status.durability_unknown > status.published
    {
        return Err(Error::Protocol);
    }
    Ok(status)
}
fn error_code(error: Error) -> (u16, u8) {
    let code = match error {
        Error::WrongInstance => 1,
        Error::Busy => 2,
        Error::Closed => 3,
        Error::Exhausted => 4,
        Error::UnknownRequest => 5,
        Error::InvalidPaths => 6,
        Error::InvalidRequest => 7,
        Error::Timeout => 8,
        Error::Unavailable => 9,
        Error::Protocol => 10,
        Error::Selection { index, reason } => return (100 + reason as u16, index),
        _ => 11,
    };
    (code, 0)
}
fn decode_error(code: u16, index: u8) -> Option<Error> {
    if code >= 100 {
        if usize::from(index) > MAX_PATHS {
            return None;
        }
        let reason = match code {
            100 => SelectionFailure::TooMany,
            101 => SelectionFailure::Missing,
            102 => SelectionFailure::Symlink,
            103 => SelectionFailure::Directory,
            104 => SelectionFailure::SpecialFile,
            105 => SelectionFailure::NotPortable,
            106 => SelectionFailure::DuplicateName,
            107 => SelectionFailure::Unreadable,
            108 => SelectionFailure::Changed,
            _ => return None,
        };
        return Some(Error::Selection { index, reason });
    }
    if index != 0 {
        return None;
    }
    Some(match code {
        1 => Error::WrongInstance,
        2 => Error::Busy,
        3 => Error::Closed,
        4 => Error::Exhausted,
        5 => Error::UnknownRequest,
        6 => Error::InvalidPaths,
        7 => Error::InvalidRequest,
        8 => Error::Timeout,
        9 => Error::Unavailable,
        10 => Error::Protocol,
        11 => Error::OutcomeUnknown,
        _ => return None,
    })
}
pub(super) fn hex(bytes: &[u8; 16]) -> String {
    let mut text = String::with_capacity(32);
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
    text
}
pub(super) fn parse_instance(text: &str) -> Result<[u8; 16], Error> {
    if text.len() != 32 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(Error::InvalidInstance);
    }
    let mut bytes = [0; 16];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16)
            .map_err(|_| Error::InvalidInstance)?;
    }
    Ok(bytes)
}
