//! One per-lease out-of-process injection executor (`fr-input-agent`).
//!
//! frd never loads Xlib (AGENTS 3.2/3.5; Xlib I/O errors terminate their
//! process). The canonical `InputSession` and its prepare → final check →
//! submit order stay on the existing native input thread; only the SINK moves
//! behind a private socketpair to a child owning the X11 connection for exactly
//! one lease. Before every native call the child re-checks the fence, its local
//! sharing indicator and the deadline derived from frd's final check.
//!
//! A timed-out, mismatched or malformed exchange is `Unknown`: the channel is
//! poisoned, the child's process group is killed, nothing is resent, and native
//! cleanup reports unresolved state so the Seat stays occupied. The IPC is
//! bounded std I/O on the existing foreign-call thread, not another runtime.
//!
//! This is crash/hang isolation of the broker, NOT a security sandbox: the
//! child runs as the same desktop user with the same X server authority.
use crate::input_watchdog;
use asupersync::cx::Cx;
use fr_core::{
    input::InputBounds,
    input_submission::{
        Capabilities, InputSink, Operation, PlatformError, Submission,
        process::{self, Reply, Request, Signal},
    },
    time::HostInstant,
};
use std::{
    fmt,
    io::{self, Read, Write},
    os::{
        fd::OwnedFd,
        unix::{
            net::{UnixDatagram, UnixStream},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

/// Opening the display, probing XTest/XKB and mapping the sharing indicator.
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// One prepare, submit, release, cancel or cleanup exchange.
const REPLY_TIMEOUT: Duration = Duration::from_secs(1);
const STOP_TIMEOUT: Duration = Duration::from_secs(1);
/// Signal datagrams drained per poll; later ones wait for the next poll.
const SIGNALS_PER_POLL: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidLaunch;

/// Locally selected package image and graphical-session context. Never build
/// this from a peer path, argv or environment. The environment is cleared; the
/// child gets only DISPLAY, optional XAUTHORITY and `--parent-pid`.
pub struct ProcessLaunch {
    pub(crate) image: PathBuf,
    pub(crate) display: String,
    pub(crate) xauthority: Option<PathBuf>,
    pub(crate) epoch: u128,
}
impl ProcessLaunch {
    pub fn new(
        image: &Path,
        display: &str,
        xauthority: Option<&Path>,
        epoch: u128,
    ) -> Result<Self, InvalidLaunch> {
        if !image.is_absolute()
            || epoch == 0
            || !local_display(display)
            || xauthority.is_some_and(|p| !p.is_absolute())
        {
            return Err(InvalidLaunch);
        }
        Ok(Self {
            image: image.into(),
            display: display.into(),
            xauthority: xauthority.map(Path::to_path_buf),
            epoch,
        })
    }
}
impl fmt::Debug for ProcessLaunch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // No paths, display names or launch identity in diagnostics.
        f.write_str("ProcessLaunch")
    }
}
fn local_display(name: &str) -> bool {
    let Some(rest) = name.strip_prefix(':') else {
        return false;
    };
    let valid = |s: &str| {
        !s.is_empty()
            && s.len() <= 5
            && s.bytes().all(|b| b.is_ascii_digit())
            && s.parse::<u16>().is_ok()
    };
    let mut parts = rest.split('.');
    parts.next().is_some_and(valid) && parts.next().is_none_or(valid) && parts.next().is_none()
}

#[derive(Default)]
struct FenceState {
    signalled: bool,
    sent: bool,
    socket: Option<Arc<UnixDatagram>>,
}
/// Local, one-way, nonblocking revocation of ONE lease's executor. Install it
/// with `Control::install_fence` before any input can queue. Signalling before
/// the child exists refuses the launch; afterwards one datagram reaches the
/// child even while a request is in flight. It never grants or resumes input.
#[derive(Clone, Default)]
pub struct Fence(Arc<Mutex<FenceState>>);
impl Fence {
    pub fn signal(&self) {
        let socket = {
            let mut state = self.lock();
            state.signalled = true;
            if state.sent {
                None
            } else {
                state.sent = state.socket.is_some();
                state.socket.clone()
            }
        };
        if let Some(socket) = socket {
            // Nonblocking send of one fixed datagram. A dead child needs none.
            let _ = socket.send(&process::encode_signal(Signal::Fence));
        }
    }
    pub fn is_signalled(&self) -> bool {
        self.lock().signalled
    }
    fn bind(&self, socket: Arc<UnixDatagram>) -> Result<(), ()> {
        let mut state = self.lock();
        if state.signalled || state.socket.is_some() {
            return Err(());
        }
        state.socket = Some(socket);
        Ok(())
    }
    fn lock(&self) -> MutexGuard<'_, FenceState> {
        // Only bounded assignments happen under this lock.
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
impl fmt::Debug for Fence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InputFence")
            .field("signalled", &self.is_signalled())
            .finish_non_exhaustive()
    }
}

/// Build the native-thread factory for `Seat::start`/`approve_fenced`. Merely
/// building it spawns nothing. The child revalidates exact bounds and the
/// required capabilities before replying Ready; nothing is injected during
/// initialization. Pass `RemoteSink::native_cleanup` as the native cleanup.
/// `cx` must carry the production wall-clock timer: deadlines are translated
/// into the child's `CLOCK_MONOTONIC`, which ticks at the same rate.
pub fn factory(
    launch: ProcessLaunch,
    cx: Cx,
    bounds: InputBounds,
    required: Capabilities,
    fence: Fence,
) -> impl FnOnce() -> Result<RemoteSink, PlatformError> + Send + 'static {
    move || RemoteSink::start(&launch, cx, bounds, required, &fence)
}

/// What the child will still accept. Both refusal states admit only
/// release-only cleanup; neither is ever reopened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gate {
    Open,
    /// The child reported itself fenced (frd's fence or its own indicator).
    Fenced,
    /// The child's local sharing indicator was used (implies fenced).
    LocallyRevoked,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Prepared {
    /// The child holds this prepared operation (possibly reversible state).
    Remote(Operation),
    /// The child is fenced: nothing was prepared; submission reports Fenced.
    Refused(Operation),
}

/// The `InputSink` for one out-of-process executor. Thread-confined to the
/// native input thread by use; one outstanding request at a time.
pub struct RemoteSink {
    child: Option<Child>,
    command: UnixStream,
    signals: Arc<UnixDatagram>,
    cx: Cx,
    sequence: u64,
    capabilities: Capabilities,
    repeat_pairs: bool,
    line_pairs: bool,
    prepared: Option<Prepared>,
    gate: Gate,
    poisoned: bool,
}
impl fmt::Debug for RemoteSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteInputSink")
            .field("gate", &self.gate)
            .field("poisoned", &self.poisoned)
            .finish_non_exhaustive()
    }
}
impl RemoteSink {
    fn start(
        launch: &ProcessLaunch,
        cx: Cx,
        bounds: InputBounds,
        required: Capabilities,
        fence: &Fence,
    ) -> Result<Self, PlatformError> {
        if fence.is_signalled() {
            return Err(PlatformError::Permission);
        }
        input_watchdog::host_now(&cx).map_err(|_| PlatformError::Unavailable)?;
        let (command, child_command) =
            UnixStream::pair().map_err(|_| PlatformError::Unavailable)?;
        let (signals, child_signals) =
            UnixDatagram::pair().map_err(|_| PlatformError::Unavailable)?;
        signals
            .set_nonblocking(true)
            .map_err(|_| PlatformError::Unavailable)?;
        let mut spawn = Command::new(&launch.image);
        spawn
            .env_clear()
            .env("DISPLAY", &launch.display)
            .arg("--parent-pid")
            .arg(std::process::id().to_string())
            .stdin(Stdio::from(OwnedFd::from(child_command)))
            .stdout(Stdio::from(OwnedFd::from(child_signals)))
            .stderr(Stdio::null())
            .process_group(0);
        if let Some(path) = &launch.xauthority {
            spawn.env("XAUTHORITY", path);
        }
        let child = spawn.spawn().map_err(|error| match error.kind() {
            io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied => PlatformError::Unsupported,
            _ => PlatformError::Unavailable,
        })?;
        // Close our copies of the child's ends: its exit must read as EOF here.
        drop(spawn);
        let signals = Arc::new(signals);
        // Custody first: every failure below kills and reaps this child.
        let mut sink = Self {
            child: Some(child),
            command,
            signals: signals.clone(),
            cx,
            sequence: 0,
            capabilities: Capabilities::default(),
            repeat_pairs: true,
            line_pairs: true,
            prepared: None,
            gate: Gate::Open,
            poisoned: false,
        };
        if fence.bind(signals).is_err() {
            sink.poison();
            return Err(PlatformError::Permission);
        }
        let hello = Request::Hello {
            epoch: launch.epoch,
            bounds,
            required,
        };
        match sink.exchange(hello, HELLO_TIMEOUT) {
            Ok(Reply::Ready {
                epoch,
                capabilities,
                repeat_requires_pair,
                line_scroll_requires_pairs,
            }) if epoch == launch.epoch && capabilities.contains_all(required) => {
                sink.capabilities = capabilities;
                sink.repeat_pairs = repeat_requires_pair;
                sink.line_pairs = line_scroll_requires_pairs;
                Ok(sink)
            }
            Ok(Reply::Refused(error)) => {
                sink.poison();
                Err(error)
            }
            _ => {
                sink.poison();
                Err(PlatformError::Unavailable)
            }
        }
    }
    /// The child's revalidated native capabilities (a superset of `required`).
    pub const fn capabilities(&self) -> Capabilities {
        self.capabilities
    }
    /// Release-only native cleanup in the child (keys, buttons, wheel and XKB
    /// repeat restoration). False means unresolved: retry locally, never hand
    /// off. A poisoned or dead child is always unresolved.
    pub fn native_cleanup(&mut self) -> bool {
        self.cancel_prepared();
        matches!(
            self.exchange(Request::Cleanup, REPLY_TIMEOUT),
            Ok(Reply::Cleaned(true))
        )
    }
    fn exchange(&mut self, request: Request, timeout: Duration) -> Result<Reply, ()> {
        if self.poisoned {
            return Err(());
        }
        let result = self.round_trip(request, timeout);
        if result.is_err() {
            self.poison();
        }
        result
    }
    fn round_trip(&mut self, request: Request, timeout: Duration) -> Result<Reply, ()> {
        let deadline = Instant::now().checked_add(timeout).ok_or(())?;
        let sequence = self.sequence.checked_add(1).ok_or(())?;
        let frame = process::encode_request(sequence, request).map_err(drop)?;
        self.sequence = sequence;
        self.command
            .set_write_timeout(Some(timeout))
            .map_err(drop)?;
        self.command.write_all(&frame).map_err(drop)?;
        let bytes = read_frame(&mut self.command, deadline).map_err(drop)?;
        let (echo, reply) = process::decode_reply(&bytes).map_err(drop)?;
        if echo != sequence {
            return Err(());
        }
        Ok(reply)
    }
    /// Terminal: no further request is ever sent. The child is killed now and
    /// reaped by Drop; its unreaped group id cannot be reused before then.
    fn poison(&mut self) {
        self.poisoned = true;
        self.prepared = None;
        if let Some(child) = &self.child {
            kill_group(child);
        }
    }
    fn drain_signals(&mut self) {
        let mut buffer = [0; process::SIGNAL_BYTES + 1];
        for _ in 0..SIGNALS_PER_POLL {
            match self.signals.recv(&mut buffer) {
                // A host-direction or malformed datagram breaks the protocol.
                Ok(n) => {
                    if process::decode_signal(&buffer[..n]) == Ok(Signal::LocalRevoke) {
                        self.gate = Gate::LocallyRevoked;
                    } else {
                        self.poison();
                        return;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    self.poison();
                    return;
                }
            }
        }
    }
    fn fence_seen(&mut self) {
        if self.gate == Gate::Open {
            self.gate = Gate::Fenced;
        }
    }
    fn deadline(&self, until: HostInstant) -> Option<u64> {
        // Monotonic FIRST, then the runtime clock: the translation errs early.
        let monotonic = monotonic_ns()?;
        let now = input_watchdog::host_now(&self.cx).ok()?;
        process::not_after_ns(monotonic, now, until)
    }
}
impl InputSink for RemoteSink {
    fn prepare(&mut self, operation: Operation) -> Result<(), PlatformError> {
        self.cancel_prepared();
        if self.poisoned {
            return Err(PlatformError::Unavailable);
        }
        if self.gate != Gate::Open && !operation.is_release() {
            self.prepared = Some(Prepared::Refused(operation));
            return Ok(());
        }
        match self.exchange(Request::Prepare(operation), REPLY_TIMEOUT) {
            Ok(Reply::Prepared) => {
                self.prepared = Some(Prepared::Remote(operation));
                Ok(())
            }
            Ok(Reply::PrepareFailed(error)) => Err(error),
            // The child's fence or local indicator was used: nothing prepared.
            // Report it at submission as Fenced (Revoked), not as a platform
            // permission failure. Releases are never refused this way.
            Ok(Reply::Fenced) if !operation.is_release() => {
                self.fence_seen();
                self.prepared = Some(Prepared::Refused(operation));
                Ok(())
            }
            Ok(_) => {
                self.poison();
                Err(PlatformError::Unavailable)
            }
            Err(()) => Err(PlatformError::Unavailable),
        }
    }
    fn submit(&mut self, operation: Operation) -> Submission {
        // Without a deadline only release-only cleanup may reach the child.
        if !operation.is_release() {
            return Submission::NotSubmitted(PlatformError::Unsupported);
        }
        if self.prepared != Some(Prepared::Remote(operation)) {
            return Submission::NotSubmitted(PlatformError::Unsupported);
        }
        self.prepared = None;
        match self.exchange(Request::Release(operation), REPLY_TIMEOUT) {
            Ok(reply) => reply.submission().unwrap_or_else(|| {
                self.poison();
                Submission::Unknown
            }),
            Err(()) => Submission::Unknown,
        }
    }
    fn submit_until(&mut self, operation: Operation, until: HostInstant) -> Submission {
        match self.prepared {
            Some(Prepared::Refused(prepared)) if prepared == operation => {
                self.prepared = None;
                return Submission::Fenced;
            }
            Some(Prepared::Remote(prepared)) if prepared == operation => {}
            _ => return Submission::NotSubmitted(PlatformError::Unsupported),
        }
        let Some(not_after_ns) = self.deadline(until) else {
            // Nothing sent. The retained preparation is cancelled by the
            // owner's guard through `cancel_prepared`.
            return Submission::Expired;
        };
        self.prepared = None;
        let request = Request::Submit {
            operation,
            not_after_ns,
        };
        match self.exchange(request, REPLY_TIMEOUT) {
            Ok(Reply::Fenced) => {
                self.fence_seen();
                Submission::Fenced
            }
            Ok(reply) => reply.submission().unwrap_or_else(|| {
                self.poison();
                Submission::Unknown
            }),
            // Timed out, mismatched or dead: the call may have been entered.
            Err(()) => Submission::Unknown,
        }
    }
    fn cancel_prepared(&mut self) {
        if let Some(Prepared::Remote(_)) = self.prepared.take()
            && !matches!(
                self.exchange(Request::Cancel, REPLY_TIMEOUT),
                Ok(Reply::Cancelled)
            )
        {
            self.poison();
        }
    }
    fn repeat_requires_pair(&self) -> bool {
        self.repeat_pairs
    }
    fn line_scroll_requires_pairs(&self) -> bool {
        self.line_pairs
    }
    fn locally_revoked(&mut self) -> bool {
        if !self.poisoned {
            self.drain_signals();
        }
        self.gate == Gate::LocallyRevoked
    }
    fn native_failed(&mut self) -> bool {
        if !self.poisoned && !channel_idle(&mut self.command) {
            self.poison();
        }
        self.poisoned
    }
}
impl Drop for RemoteSink {
    fn drop(&mut self) {
        // Orderly stop: the child releases, restores, closes X11, then replies.
        let _ = self.exchange(Request::Stop, STOP_TIMEOUT);
        if let Some(mut child) = self.child.take() {
            kill_group(&child);
            let _ = child.wait();
        }
    }
}

/// Between exchanges the child must send nothing: EOF (it exited) or any
/// unsolicited byte is a lost executor.
fn channel_idle(command: &mut UnixStream) -> bool {
    if command.set_nonblocking(true).is_err() {
        return false;
    }
    let mut byte = [0; 1];
    let idle = loop {
        match command.read(&mut byte) {
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break true,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            _ => break false,
        }
    };
    command.set_nonblocking(false).is_ok() && idle
}
/// One absolute deadline covers the whole frame; a partial read never
/// restarts it.
fn read_frame(
    command: &mut UnixStream,
    deadline: Instant,
) -> io::Result<[u8; process::FRAME_BYTES]> {
    let mut frame = [0; process::FRAME_BYTES];
    let mut filled = 0;
    while filled < frame.len() {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or(io::ErrorKind::TimedOut)?;
        command.set_read_timeout(Some(remaining))?;
        match command.read(&mut frame[filled..]) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => filled += n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(frame)
}
pub(crate) fn kill_group(child: &Child) {
    if let Some(pid) = i32::try_from(child.id())
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
}
/// Raw `CLOCK_MONOTONIC` nanoseconds, the same clock the child checks.
pub(crate) fn monotonic_ns() -> Option<u64> {
    let now = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    u64::try_from(now.tv_sec)
        .ok()?
        .checked_mul(1_000_000_000)?
        .checked_add(u64::try_from(now.tv_nsec).ok()?)
}

#[cfg(test)]
pub(crate) mod tests;
