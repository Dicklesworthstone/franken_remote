//! Bounded, same-user commands for one original foreground file owner.
//! This local protocol conveys explicit selections, never file contents or a
//! remote path-browsing capability. Its instance is a public comparison fence.
#![forbid(unsafe_code)]

use std::{
    fmt,
    io::{self, Read, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
    thread,
    time::{Duration, Instant},
};

mod server;
mod wire;
pub(super) use server::Owner;

pub const MAX_PATHS: usize = 8;
pub const MAX_PATH_BYTES: usize = 4096;
pub const MAX_TOTAL_PATH_BYTES: usize = MAX_PATHS * MAX_PATH_BYTES;
pub const MAX_HISTORY: usize = 64;
const MAX_NAME: usize = 64;
const MAX_PEERS: usize = 4;
const HANDSHAKE: Duration = Duration::from_secs(2);
const TICK: Duration = Duration::from_millis(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionFailure {
    TooMany,
    Missing,
    Symlink,
    Directory,
    SpecialFile,
    NotPortable,
    DuplicateName,
    Unreadable,
    Changed,
}

/// Content-free refusals. An uncertain command must never be retried silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidTarget,
    InvalidInstance,
    InvalidPaths,
    InvalidRequest,
    AlreadyRunning,
    NoSession,
    WrongInstance,
    UntrustedPeer,
    Busy,
    Closed,
    Exhausted,
    UnknownRequest,
    Selection {
        index: u8,
        reason: SelectionFailure,
    },
    Unavailable,
    Protocol,
    Timeout,
    /// The original owner may have admitted the selection or cancellation.
    OutcomeUnknown,
}
impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "local-file-command: {self:?}")
    }
}
impl std::error::Error for Error {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Phase {
    #[default]
    NoSelection,
    WaitingForControl,
    Negotiating,
    WaitingForSelection,
    SelectionAccepted,
    Sending,
    Finished,
    Ended,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Stage {
    #[default]
    Unknown,
    Idle,
    Queued,
    Preparing,
    AwaitingAcceptance,
    Streaming,
    AwaitingProof,
    Finished,
    Closed,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Cleanup {
    #[default]
    Unknown,
    Confirmed,
    Failed,
}

/// One immutable command's current evidence. Publication counts come only from
/// retained recipient receipts; queued bytes and cleanup do not prove delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
// Independent admission, receipt and cancellation observations may coexist;
// replacing them with one state enum would erase valid partial evidence.
#[allow(clippy::struct_excessive_bools)]
pub struct Status {
    pub sequence: u64,
    pub ready: bool,
    pub closed: bool,
    pub phase: Phase,
    pub stage: Stage,
    pub selected: u16,
    pub started: u16,
    pub published: u16,
    pub refused: u16,
    pub interrupted: u16,
    pub publication_unknown: u16,
    pub durability_unknown: u16,
    pub complete: bool,
    pub failed: bool,
    pub cancel_requested: bool,
    pub queued_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
    pub cleanup: Cleanup,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Info {
    pub target: String,
    pub instance: String,
    pub status: Status,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submission {
    pub info: Info,
    /// Admission to the original local intake, not publication at the viewer.
    pub sequence: u64,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cancellation {
    pub info: Info,
    /// A local request only. Earlier publication and uncertain effects remain.
    pub requested: bool,
}

/// The socket worker handles only bounded metadata. Source selection belongs
/// to the one separate worker; neither callback runs on the authority runtime.
pub(super) trait Handler: Send + Sync + 'static {
    fn status(&self, sequence: Option<u64>) -> Result<Status, Error>;
    fn select(
        &self,
        paths: &[PathBuf],
        until: Instant,
        stopped: &AtomicBool,
    ) -> Result<Status, Error>;
    fn cancel(&self, sequence: u64) -> Result<(Status, bool), Error>;
    fn close(&self);
}

/// Inspect the current owner once. Callers retain this instance for every
/// subsequent command; finding a replacement never retries an old command.
pub fn inspect(target: &str) -> Result<Info, Error> {
    let (_, info, _, _) = identify(target)?;
    Ok(info)
}
pub fn status(target: &str, instance: &str, sequence: Option<u64>) -> Result<Info, Error> {
    if sequence == Some(0) {
        return Err(Error::InvalidRequest);
    }
    command(target, instance, wire::Command::Status(sequence), &[]).map(|reply| reply.0)
}
pub fn send(target: &str, instance: &str, paths: &[PathBuf]) -> Result<Submission, Error> {
    let payload = wire::paths(paths)?;
    let (info, _) = command(target, instance, wire::Command::Send(paths.len()), &payload)?;
    Ok(Submission {
        sequence: info.status.sequence,
        info,
    })
}
pub fn cancel(target: &str, instance: &str, sequence: u64) -> Result<Cancellation, Error> {
    if sequence == 0 {
        return Err(Error::InvalidRequest);
    }
    let (info, requested) = command(target, instance, wire::Command::Cancel(sequence), &[])?;
    Ok(Cancellation { info, requested })
}

fn command(
    target: &str,
    expected: &str,
    command: wire::Command,
    payload: &[u8],
) -> Result<(Info, bool), Error> {
    let expected = wire::parse_instance(expected)?;
    let (mut stream, mut info, instance, until) = identify(target)?;
    if instance != expected {
        return Err(Error::WrongInstance);
    }
    let request = wire::command(command, instance, payload.len())?;
    // From the first command write onward a lost reply cannot prove refusal.
    let result = (|| {
        write(&mut stream, &request, until)?;
        write(&mut stream, payload, until)?;
        stream
            .shutdown(Shutdown::Write)
            .map_err(|_| Error::Unavailable)?;
        let mut bytes = [0; wire::RESULT_BYTES];
        read(&mut stream, &mut bytes, until)?;
        Ok(bytes)
    })()
    .map_err(|_: Error| Error::OutcomeUnknown)?;
    let (status, cancelled) = wire::result(&result, instance)?;
    let matches = match command {
        wire::Command::Send(count) => {
            status.sequence != 0 && usize::from(status.selected) == count && !cancelled
        }
        wire::Command::Status(sequence) => {
            sequence.is_none_or(|sequence| status.sequence == sequence) && !cancelled
        }
        wire::Command::Cancel(sequence) => status.sequence == sequence,
    };
    if !matches {
        return Err(Error::OutcomeUnknown);
    }
    info.status = status;
    Ok((info, cancelled))
}

fn identify(target: &str) -> Result<(UnixStream, Info, [u8; 16], Instant), Error> {
    check_target(target)?;
    let until = Instant::now()
        .checked_add(HANDSHAKE)
        .ok_or(Error::Unavailable)?;
    let uid = rustix::process::geteuid().as_raw();
    let mut stream = connect(uid, target)?;
    check_peer(&stream, uid)?;
    write(&mut stream, &wire::IDENTIFY, until)?;
    let mut bytes = [0; wire::IDENTITY_BYTES];
    read(&mut stream, &mut bytes, until)?;
    let (instance, length, status) = wire::identity(&bytes)?;
    let mut name = [0; MAX_NAME];
    read(&mut stream, &mut name[..length], until)?;
    if name[..length] != *target.as_bytes() {
        return Err(Error::Protocol);
    }
    Ok((
        stream,
        Info {
            target: target.to_owned(),
            instance: wire::hex(&instance),
            status,
        },
        instance,
        until,
    ))
}
fn connect(uid: u32, target: &str) -> Result<UnixStream, Error> {
    use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType};
    let socket = rustix::net::socket_with(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::NONBLOCK | SocketFlags::CLOEXEC,
        None,
    )
    .map_err(|_| Error::Unavailable)?;
    let address =
        SocketAddrUnix::new_abstract_name(&name(uid, target)).map_err(|_| Error::InvalidTarget)?;
    rustix::net::connect(&socket, &address).map_err(|error| match error {
        rustix::io::Errno::CONNREFUSED | rustix::io::Errno::NOENT => Error::NoSession,
        rustix::io::Errno::AGAIN | rustix::io::Errno::INPROGRESS => Error::Busy,
        _ => Error::Unavailable,
    })?;
    Ok(UnixStream::from(socket))
}
fn check_peer(stream: &UnixStream, uid: u32) -> Result<(), Error> {
    if rustix::net::sockopt::socket_peercred(stream)
        .is_ok_and(|credentials| credentials.uid.as_raw() == uid)
    {
        Ok(())
    } else {
        Err(Error::UntrustedPeer)
    }
}
fn check_target(target: &str) -> Result<(), Error> {
    if target.is_empty()
        || target.len() > MAX_NAME
        || !target
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    {
        return Err(Error::InvalidTarget);
    }
    Ok(())
}
fn name(uid: u32, target: &str) -> Vec<u8> {
    format!("frankenremote.send.v1.{uid}.{target}").into_bytes()
}
/// Validate the same local endpoint label accepted by bind and every caller.
pub fn validate_target(target: &str) -> Result<(), Error> {
    check_target(target)
}
fn check_time(until: Instant) -> Result<(), Error> {
    if Instant::now() >= until {
        Err(Error::Timeout)
    } else {
        Ok(())
    }
}
fn pause(until: Instant) -> Result<(), Error> {
    let remaining = until
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or(Error::Timeout)?;
    thread::sleep(TICK.min(remaining));
    Ok(())
}
fn read(stream: &mut UnixStream, mut bytes: &mut [u8], until: Instant) -> Result<(), Error> {
    while !bytes.is_empty() {
        check_time(until)?;
        match stream.read(bytes) {
            Ok(0) => return Err(Error::Protocol),
            Ok(count) => bytes = &mut bytes[count..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => pause(until)?,
            Err(_) => return Err(Error::Unavailable),
        }
    }
    check_time(until)
}
fn write(stream: &mut UnixStream, mut bytes: &[u8], until: Instant) -> Result<(), Error> {
    while !bytes.is_empty() {
        check_time(until)?;
        match stream.write(bytes) {
            Ok(0) => return Err(Error::Unavailable),
            Ok(count) => bytes = &bytes[count..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => pause(until)?,
            Err(_) => return Err(Error::Unavailable),
        }
    }
    check_time(until)
}

#[cfg(test)]
mod tests;
