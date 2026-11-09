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

mod endpoint;
mod server;
mod wire;
pub(super) use server::Owner;

#[derive(Clone, Copy)]
enum Scope {
    Host,
    Upload,
}
impl Scope {
    fn check_target(self, target: &str) -> Result<(), Error> {
        check_target(target)?;
        if matches!(self, Self::Upload) {
            // A client endpoint belongs to one exact foreground connection,
            // never a convenient host label or a replacement connection.
            wire::parse_instance(target).map_err(|_| Error::InvalidTarget)?;
        }
        Ok(())
    }
}

/// Commands for an explicitly enabled original client upload. The target is
/// its foreground connection ID. Its separate instance must also match; this
/// namespace can never address a host's independently opted-in send endpoint.
pub mod upload;

// Current callers require the pacing/status contract before sending commands.
// The owner still accepts V1 clients on this same socket; clients never retry
// with an older version after either identification or an uncertain command.
const CLIENT_VERSION: wire::Version = wire::Version::V2;
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
    /// The original owner may have admitted the selection or changed its local control.
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
    /// Latest local intent; unavailable for legacy V1 peers or no selection.
    pub pause_requested: Option<bool>,
    /// Last original-sender observation, not acknowledgement of the latest intent.
    pub paused: Option<bool>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pacing {
    pub info: Info,
    /// This command changed the original local intent, not necessarily the
    /// sender's current gate. The status retains those observations separately.
    pub changed: bool,
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
    fn set_paused(&self, sequence: u64, paused: bool) -> Result<(Status, bool), Error>;
    fn close(&self);
}

/// Inspect the current owner once. Callers retain this instance for every
/// subsequent command; finding a replacement never retries an old command.
pub fn inspect(target: &str) -> Result<Info, Error> {
    let (_, info, _, _) = identify(Scope::Host, target)?;
    Ok(info)
}
pub fn status(target: &str, instance: &str, sequence: Option<u64>) -> Result<Info, Error> {
    if sequence == Some(0) {
        return Err(Error::InvalidRequest);
    }
    command(
        Scope::Host,
        target,
        instance,
        wire::Command::Status(sequence),
        &[],
    )
    .map(|reply| reply.0)
}
pub fn send(target: &str, instance: &str, paths: &[PathBuf]) -> Result<Submission, Error> {
    let payload = wire::paths(paths)?;
    let (info, _) = command(
        Scope::Host,
        target,
        instance,
        wire::Command::Send(paths.len()),
        &payload,
    )?;
    Ok(Submission {
        sequence: info.status.sequence,
        info,
    })
}
pub fn cancel(target: &str, instance: &str, sequence: u64) -> Result<Cancellation, Error> {
    if sequence == 0 {
        return Err(Error::InvalidRequest);
    }
    let (info, requested) = command(
        Scope::Host,
        target,
        instance,
        wire::Command::Cancel(sequence),
        &[],
    )?;
    Ok(Cancellation { info, requested })
}

/// Pause one original host selection without extending its authority or expiry.
pub fn pause(target: &str, instance: &str, sequence: u64) -> Result<Pacing, Error> {
    set_paused(Scope::Host, target, instance, sequence, true)
}
/// Resume that same selection; never reselect, retry or recreate expired work.
pub fn resume(target: &str, instance: &str, sequence: u64) -> Result<Pacing, Error> {
    set_paused(Scope::Host, target, instance, sequence, false)
}
fn set_paused(
    scope: Scope,
    target: &str,
    instance: &str,
    sequence: u64,
    paused: bool,
) -> Result<Pacing, Error> {
    if sequence == 0 {
        return Err(Error::InvalidRequest);
    }
    let operation = if paused {
        wire::Command::Pause(sequence)
    } else {
        wire::Command::Resume(sequence)
    };
    let (info, changed) = command(scope, target, instance, operation, &[])?;
    Ok(Pacing { info, changed })
}

fn command(
    scope: Scope,
    target: &str,
    expected: &str,
    command: wire::Command,
    payload: &[u8],
) -> Result<(Info, bool), Error> {
    let expected = wire::parse_instance(expected)?;
    let (mut stream, mut info, instance, until) = identify(scope, target)?;
    if instance != expected {
        return Err(Error::WrongInstance);
    }
    let request = wire::command(CLIENT_VERSION, command, instance, payload.len())?;
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
    let (status, changed) = wire::result(CLIENT_VERSION, &result, instance)?;
    let matches = match command {
        wire::Command::Send(count) => {
            status.sequence != 0 && usize::from(status.selected) == count && !changed
        }
        wire::Command::Status(sequence) => {
            sequence.is_none_or(|sequence| status.sequence == sequence) && !changed
        }
        wire::Command::Cancel(sequence) => status.sequence == sequence,
        wire::Command::Pause(sequence) => {
            status.sequence == sequence && (!changed || status.pause_requested == Some(true))
        }
        wire::Command::Resume(sequence) => {
            status.sequence == sequence && (!changed || status.pause_requested == Some(false))
        }
    };
    if !matches {
        return Err(Error::OutcomeUnknown);
    }
    info.status = status;
    Ok((info, changed))
}

fn identify(scope: Scope, target: &str) -> Result<(UnixStream, Info, [u8; 16], Instant), Error> {
    scope.check_target(target)?;
    let until = Instant::now()
        .checked_add(HANDSHAKE)
        .ok_or(Error::Unavailable)?;
    let uid = rustix::process::geteuid().as_raw();
    let mut stream = endpoint::connect(scope, uid, target, until)?;
    check_peer(&stream, uid)?;
    write(&mut stream, &CLIENT_VERSION.identify(), until)?;
    let mut bytes = [0; wire::IDENTITY_BYTES];
    read(&mut stream, &mut bytes, until)?;
    let (instance, length, status) = wire::identity(CLIENT_VERSION, &bytes)?;
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
fn check_peer(stream: &UnixStream, uid: u32) -> Result<(), Error> {
    if endpoint::peer_uid(stream).is_ok_and(|actual| actual == uid) {
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
#[cfg(target_os = "linux")]
fn name(scope: Scope, uid: u32, target: &str) -> Vec<u8> {
    let direction = match scope {
        Scope::Host => "send",
        Scope::Upload => "upload",
    };
    format!("frankenremote.{direction}.v1.{uid}.{target}").into_bytes()
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
fn wait_io(until: Instant) -> Result<(), Error> {
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
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => wait_io(until)?,
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
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => wait_io(until)?,
            Err(_) => return Err(Error::Unavailable),
        }
    }
    check_time(until)
}

#[cfg(test)]
mod tests;
