//! One per-lease out-of-process X11 clipboard owner (`fr-input-agent
//! --clipboard`), the host's `NativeClipboard` for the controlled share.
//!
//! frd never loads Xlib/XCB (AGENTS 3.2/3.5). The canonical synchronizer, the
//! channel ledger and every authority/expiry check stay on the existing
//! clipboard worker thread (`clipboard_quic::WorkerSeed::spawn`); only the
//! NATIVE owner moves behind a private socketpair to a child that owns the X11
//! CLIPBOARD connection for exactly one clipboard lane, which itself exists
//! only while the controller's input lease is active. It reuses the input
//! executor's launch validation, image, process group and kill/reap custody.
//!
//! Bounds: one outstanding request; fixed 64-byte frames; the only payloads
//! (`Prepare` text, `ReadText` reply) are announced in the frame and checked
//! against the item bound agreed in `Hello` BEFORE allocation or reading; one
//! absolute deadline covers a whole exchange including its payload. A timed
//! out, mismatched or malformed exchange poisons the channel: the child's group
//! is killed, nothing is resent, every later call fails typed, and the
//! synchronizer retires clipboard only (input and media keep their owners).
//!
//! Publication: the core's last authority check yields an exclusive deadline
//! (the item's, bounded by the lease); it is translated into the child's
//! `CLOCK_MONOTONIC` and re-checked there immediately before the X11 ownership
//! call. A publication without a deadline never reaches the child.
//!
//! Clipboard text never appears in Debug output, errors or logs here. This is
//! crash/hang isolation of the broker, NOT a security sandbox: the child runs as
//! the same desktop user with the same X server authority.
use crate::{
    input_process::{ProcessLaunch, kill_group, monotonic_ns},
    input_watchdog,
};
use asupersync::cx::Cx;
use fr_core::{
    clipboard::{
        ClipboardSink, PlatformError, Publication, Stamp,
        process::{self, Failure, Reply, Request},
    },
    time::HostInstant,
};
use fr_wire::clipboard::session::synchronize::{
    NativeChange, NativeChanges, NativeClipboard, NativeText,
};
use std::{
    fmt,
    io::{self, Read, Write},
    os::{
        fd::OwnedFd,
        unix::{net::UnixStream, process::CommandExt},
    },
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

/// Opening the display and negotiating `XFixes`.
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// One exchange, payload included. The child's own worst case is two 100 ms
/// preparation waits, or 32 events plus one 16 KiB property read.
const REPLY_TIMEOUT: Duration = Duration::from_secs(1);
const STOP_TIMEOUT: Duration = Duration::from_secs(1);

/// Build the clipboard worker's native factory. Merely building it spawns
/// nothing; the factory runs on the clipboard worker thread only after the
/// original grant and bilateral clipboard readiness. `cx` must carry the host
/// timer the clipboard session uses (deadlines are translated from it).
pub fn factory(
    launch: ProcessLaunch,
    cx: Cx,
) -> impl FnOnce() -> Result<RemoteClipboard, PlatformError> + Send + 'static {
    move || RemoteClipboard::start(&launch, cx, process::MAX_ITEM_BYTES)
}

/// Typed and content-free: a native failure the child reported, or a lost /
/// poisoned channel. Never a library string, path, display or text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteError {
    Native(Failure),
    Channel,
}

/// Private bytes, cleared on drop. Neither Clone nor content-Debug.
struct Secret(Vec<u8>);
impl Drop for Secret {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

/// ONE complete, validated UTF-8 selection read by the child.
pub struct RemoteText {
    bytes: Secret,
    origin: Option<Stamp>,
}
impl NativeText for RemoteText {
    fn text(&self) -> &str {
        // Validated when received; an invalid payload poisoned the channel.
        std::str::from_utf8(&self.bytes.0).unwrap_or_default()
    }
    fn origin(&self) -> Option<Stamp> {
        self.origin
    }
}
impl fmt::Debug for RemoteText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RemoteClipboardText([redacted])")
    }
}

/// The `NativeClipboard` for one out-of-process X11 owner. Thread-confined to
/// the clipboard worker by use; one outstanding request at a time.
pub struct RemoteClipboard {
    child: Option<Child>,
    command: UnixStream,
    cx: Cx,
    sequence: u64,
    /// The item bound agreed in `Hello`; checked before every allocation.
    max: u32,
    /// The child's native revision as of its last reply (exact after each
    /// exchange, so `revision()` needs no I/O).
    revision: u64,
    poisoned: bool,
}
impl fmt::Debug for RemoteClipboard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteClipboard")
            .field("poisoned", &self.poisoned)
            .field("closed", &self.child.is_none())
            .finish_non_exhaustive()
    }
}
impl RemoteClipboard {
    pub(crate) fn start(launch: &ProcessLaunch, cx: Cx, max: u32) -> Result<Self, PlatformError> {
        if max == 0 || max > process::MAX_ITEM_BYTES {
            return Err(PlatformError::Unsupported);
        }
        input_watchdog::host_now(&cx).map_err(|_| PlatformError::Unavailable)?;
        let (command, child_command) =
            UnixStream::pair().map_err(|_| PlatformError::Unavailable)?;
        let mut spawn = Command::new(&launch.image);
        spawn
            .env_clear()
            .env("DISPLAY", &launch.display)
            .arg("--clipboard")
            .arg("--parent-pid")
            .arg(std::process::id().to_string())
            .stdin(Stdio::from(OwnedFd::from(child_command)))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        if let Some(path) = &launch.xauthority {
            spawn.env("XAUTHORITY", path);
        }
        let child = spawn.spawn().map_err(|error| match error.kind() {
            io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied => PlatformError::Unsupported,
            _ => PlatformError::Unavailable,
        })?;
        // Close our copy of the child's end: its exit must read as EOF here.
        drop(spawn);
        // Custody first: every failure below kills and reaps this child.
        let mut owner = Self {
            child: Some(child),
            command,
            cx,
            sequence: 0,
            max,
            revision: 0,
            poisoned: false,
        };
        let hello = Request::Hello {
            epoch: launch.epoch,
            max_item_bytes: max,
        };
        match owner.exchange(hello, None, HELLO_TIMEOUT) {
            Ok((Reply::Ready { epoch }, None)) if epoch == launch.epoch => Ok(owner),
            Ok((Reply::Refused(error), None)) => {
                owner.poison();
                Err(error)
            }
            _ => {
                owner.poison();
                Err(PlatformError::Unavailable)
            }
        }
    }
    pub const fn is_poisoned(&self) -> bool {
        self.poisoned
    }
    fn exchange(
        &mut self,
        request: Request,
        payload: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<(Reply, Option<RemoteText>), ()> {
        if self.poisoned || self.child.is_none() {
            return Err(());
        }
        let result = self.round_trip(request, payload, timeout);
        if result.is_err() {
            self.poison();
        }
        result
    }
    fn round_trip(
        &mut self,
        request: Request,
        payload: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<(Reply, Option<RemoteText>), ()> {
        let deadline = Instant::now().checked_add(timeout).ok_or(())?;
        let sequence = self.sequence.checked_add(1).ok_or(())?;
        let frame = process::encode_request(sequence, request).map_err(drop)?;
        self.sequence = sequence;
        write_until(&mut self.command, &frame, deadline).map_err(drop)?;
        if let Some(payload) = payload {
            write_until(&mut self.command, payload, deadline).map_err(drop)?;
        }
        let mut header = [0; process::FRAME_BYTES];
        read_until(&mut self.command, &mut header, deadline).map_err(drop)?;
        // The announced length is refused here, before any reservation.
        let (echo, reply) = process::decode_reply(&header, self.max).map_err(drop)?;
        if echo != sequence {
            return Err(());
        }
        if let Some(revision) = reply.revision() {
            // Revisions are monotonic for one native connection.
            if revision < self.revision {
                return Err(());
            }
            self.revision = revision;
        }
        let text = match reply {
            Reply::ReadText { len, origin, .. } => {
                if request != Request::PollRead {
                    return Err(());
                }
                let mut bytes = Secret(Vec::new());
                bytes.0.try_reserve_exact(len as usize).map_err(drop)?;
                bytes.0.resize(len as usize, 0);
                read_until(&mut self.command, &mut bytes.0, deadline).map_err(drop)?;
                std::str::from_utf8(&bytes.0).map_err(drop)?;
                Some(RemoteText { bytes, origin })
            }
            _ => None,
        };
        Ok((reply, text))
    }
    /// Terminal: no further request is ever sent. The child is killed now and
    /// reaped by `close`/Drop; its unreaped group id cannot be reused before.
    fn poison(&mut self) {
        self.poisoned = true;
        if let Some(child) = &self.child {
            kill_group(child);
        }
    }
    fn protocol(&mut self) -> RemoteError {
        self.poison();
        RemoteError::Channel
    }
    fn prepare_inner(
        &mut self,
        text: &str,
        stamp: Stamp,
        revision: Option<u64>,
    ) -> Result<(), PlatformError> {
        // Refused before any IPC, allocation or child work.
        let Ok(len) = u32::try_from(text.len()) else {
            return Err(PlatformError::Unsupported);
        };
        if len > self.max || !stamp.valid() {
            return Err(PlatformError::Unsupported);
        }
        let request = Request::Prepare {
            stamp,
            revision,
            len,
        };
        match self.exchange(request, Some(text.as_bytes()), REPLY_TIMEOUT) {
            Ok((Reply::Prepared { .. }, None)) => Ok(()),
            Ok((Reply::PrepareFailed { error, .. }, None)) => Err(error),
            Ok(_) => {
                self.poison();
                Err(PlatformError::Unavailable)
            }
            Err(()) => Err(PlatformError::Unavailable),
        }
    }
    fn done(&mut self, request: Request) -> Result<(), RemoteError> {
        match self.exchange(request, None, REPLY_TIMEOUT) {
            Ok((Reply::Done { .. }, None)) => Ok(()),
            Ok((Reply::Failed { failure, .. }, None)) => Err(RemoteError::Native(failure)),
            Ok(_) => Err(self.protocol()),
            Err(()) => Err(RemoteError::Channel),
        }
    }
    /// The exclusive deadline in the child's raw monotonic clock. Monotonic
    /// FIRST, then the host timer: the translation errs early.
    fn not_after_ns(&self, until: HostInstant) -> Option<u64> {
        let monotonic = monotonic_ns()?;
        let now = input_watchdog::host_now(&self.cx).ok()?;
        fr_core::input_submission::process::not_after_ns(monotonic, now, until)
    }
}
impl ClipboardSink for RemoteClipboard {
    fn prepare(&mut self, text: &str, stamp: Stamp) -> Result<(), PlatformError> {
        self.prepare_inner(text, stamp, None)
    }
    /// Without a deadline nothing reaches the child (fail closed).
    fn publish(&mut self, _text: &str, _stamp: Stamp) -> Publication {
        Publication::NotSubmitted(PlatformError::Unavailable)
    }
    fn publish_until(&mut self, _text: &str, stamp: Stamp, until: HostInstant) -> Publication {
        if self.poisoned {
            return Publication::NotSubmitted(PlatformError::Unavailable);
        }
        let Some(not_after_ns) = self.not_after_ns(until) else {
            // Already late: nothing sent, the child keeps its preparation
            // until the core's guard cancels it.
            return Publication::NotSubmitted(PlatformError::Unavailable);
        };
        match self.exchange(
            Request::Publish {
                stamp,
                not_after_ns,
            },
            None,
            REPLY_TIMEOUT,
        ) {
            Ok((Reply::Published { publication, .. }, None)) => publication,
            Ok(_) => {
                self.poison();
                Publication::UnknownEffect
            }
            // Timed out or lost: the ownership call may have been entered.
            Err(()) => Publication::UnknownEffect,
        }
    }
    fn cancel_prepared(&mut self) {
        if !self.poisoned && self.done(Request::CancelPrepared).is_err() {
            self.poison();
        }
    }
}
impl NativeClipboard for RemoteClipboard {
    type Text = RemoteText;
    type Error = RemoteError;
    fn watch(&mut self) -> Result<u64, RemoteError> {
        match self.exchange(Request::Watch, None, REPLY_TIMEOUT) {
            Ok((Reply::Watching { revision }, None)) => Ok(revision),
            Ok((Reply::Failed { failure, .. }, None)) => Err(RemoteError::Native(failure)),
            Ok(_) => Err(self.protocol()),
            Err(()) => Err(RemoteError::Channel),
        }
    }
    fn changes(&mut self) -> Result<NativeChanges, RemoteError> {
        match self.exchange(Request::Changes, None, REPLY_TIMEOUT) {
            Ok((
                Reply::Changes {
                    latest, settled, ..
                },
                None,
            )) => Ok(NativeChanges {
                latest: latest.map(|change| NativeChange {
                    revision: change.revision,
                    has_selection: change.has_selection,
                    origin: change.origin,
                }),
                settled,
            }),
            Ok((Reply::Failed { failure, .. }, None)) => Err(RemoteError::Native(failure)),
            Ok(_) => Err(self.protocol()),
            Err(()) => Err(RemoteError::Channel),
        }
    }
    fn revision(&self) -> u64 {
        self.revision
    }
    fn prepare_for_revision(
        &mut self,
        text: &str,
        stamp: Stamp,
        revision: u64,
    ) -> Result<(), PlatformError> {
        self.prepare_inner(text, stamp, Some(revision))
    }
    fn begin_read(&mut self) -> Result<(), RemoteError> {
        self.done(Request::BeginRead)
    }
    fn poll_read(&mut self) -> Result<Option<RemoteText>, RemoteError> {
        match self.exchange(Request::PollRead, None, REPLY_TIMEOUT) {
            Ok((Reply::ReadPending { .. }, None)) => Ok(None),
            Ok((Reply::ReadText { .. }, Some(text))) => Ok(Some(text)),
            Ok((Reply::Failed { failure, .. }, None)) => Err(RemoteError::Native(failure)),
            Ok(_) => Err(self.protocol()),
            Err(()) => Err(RemoteError::Channel),
        }
    }
    fn cancel_read(&mut self) {
        if !self.poisoned && self.done(Request::CancelRead).is_err() {
            self.poison();
        }
    }
    fn suspend(&mut self) -> Result<(), RemoteError> {
        self.done(Request::Suspend)
    }
    /// Orderly stop (the child erases its private copies and closes X11
    /// without resetting another application's newer selection), then kill
    /// and reap. Idempotent.
    fn close(&mut self) {
        if !self.poisoned
            && !matches!(
                self.exchange(Request::Stop, None, STOP_TIMEOUT),
                Ok((Reply::Stopped, None))
            )
        {
            self.poison();
        }
        self.poisoned = true;
        if let Some(mut child) = self.child.take() {
            kill_group(&child);
            let _ = child.wait();
        }
    }
}
impl Drop for RemoteClipboard {
    fn drop(&mut self) {
        NativeClipboard::close(self);
    }
}

/// One absolute deadline covers the whole buffer; a partial transfer never
/// restarts it.
fn read_until(stream: &mut UnixStream, buffer: &mut [u8], deadline: Instant) -> io::Result<()> {
    let mut filled = 0;
    while filled < buffer.len() {
        stream.set_read_timeout(Some(remaining(deadline)?))?;
        match stream.read(&mut buffer[filled..]) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => filled += n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}
fn write_until(stream: &mut UnixStream, bytes: &[u8], deadline: Instant) -> io::Result<()> {
    let mut sent = 0;
    while sent < bytes.len() {
        stream.set_write_timeout(Some(remaining(deadline)?))?;
        match stream.write(&bytes[sent..]) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => sent += n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}
fn remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| io::ErrorKind::TimedOut.into())
}

#[cfg(test)]
pub(crate) mod tests;
