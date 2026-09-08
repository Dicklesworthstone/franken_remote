//! Asupersync-managed private media processes with bounded, non-replayable IPC.
use asupersync::{
    cx::Cx,
    io::{AsyncReadExt, AsyncWriteExt},
    process::{
        Child, ChildStdin, ChildStdout, Command, ExitStatus, ProcessGroupMode, ProcessSignalTarget,
        Stdio,
    },
    runtime::{Runtime, reactor::IoReactorBackend},
    time::sleep_until,
    types::Time,
};
use fr_core::limits::ProtocolLimits;
use fr_media::worker::{self, Configuration, HEADER_BYTES, Header, Identity, Kind, Record, Role};
use std::{
    fmt,
    future::{Future, poll_fn},
    path::{Path, PathBuf},
    pin::pin,
    task::Poll,
    time::Duration,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    MissingRuntime,
    InvalidLaunch,
    Cancelled,
    Deadline,
    ClockRegression,
    SpawnFailed,
    PipeFailed,
    PeerClosed,
    Protocol(worker::Error),
    WorkerRefused(worker::Error),
    Unavailable,
    ReapPending,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}
impl From<worker::Error> for Error {
    fn from(e: worker::Error) -> Self {
        Self::Protocol(e)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Starting,
    Running,
    Poisoned,
    Stopped,
    Reaped,
}

/// Locally selected package image and graphical-session context. Never build
/// this from a peer path/argv/environment. Package signing/protected-directory
/// validation belongs to installation; absolute paths alone do not certify it.
/// Credential-bearing environment and arbitrary command arguments are omitted.
pub struct Launch {
    image: PathBuf,
    display: String,
    xauthority: Option<PathBuf>,
    role: Role,
    epoch: u128,
}
impl Launch {
    pub fn new(
        image: &Path,
        display: &str,
        xauthority: Option<&Path>,
        role: Role,
        epoch: u128,
    ) -> Result<Self, Error> {
        if !image.is_absolute()
            || epoch == 0
            || display.len() > 64
            || !display.starts_with(':')
            || !display[1..]
                .bytes()
                .all(|b| b.is_ascii_digit() || b == b'.')
            || !display[1..].bytes().any(|b| b.is_ascii_digit())
            || xauthority.is_some_and(|p| !p.is_absolute())
        {
            return Err(Error::InvalidLaunch);
        }
        Ok(Self {
            image: image.into(),
            display: display.into(),
            xauthority: xauthority.map(Path::to_path_buf),
            role,
            epoch,
        })
    }
}
/// One absolute deadline covers the complete write and reply, including a
/// fragmented header/body. Receiving another byte never restarts the timeout.
#[derive(Debug, Clone, Copy)]
pub struct Deadline(Time);
impl Deadline {
    pub fn after(cx: &Cx, duration: Duration) -> Result<Self, Error> {
        if duration.is_zero() || duration > Duration::from_secs(5) {
            return Err(Error::Deadline);
        }
        let now = now(cx)?;
        let nanos = u64::try_from(duration.as_nanos()).map_err(|_| Error::Deadline)?;
        Ok(Self(Time::from_nanos(
            now.as_nanos().checked_add(nanos).ok_or(Error::Deadline)?,
        )))
    }
    /// Cap an already bounded operation at the authority owner's host-clock
    /// deadline. This is only valid in the SAME Asupersync monotonic clock domain.
    #[must_use]
    pub fn capped_at(self, authority: Time) -> Self {
        Self(self.0.min(authority))
    }
    pub const fn time(self) -> Time {
        self.0
    }
}
fn now(cx: &Cx) -> Result<Time, Error> {
    cx.timer_driver()
        .map(|d| d.now())
        .ok_or(Error::MissingRuntime)
}
fn runtime_ready(cx: &Cx) -> Result<(), Error> {
    let backend = Runtime::current_handle()
        .and_then(|h| h.io_reactor_capability_snapshot())
        .map(asupersync::runtime::reactor::IoReactorCapabilitySnapshot::backend);
    if !matches!(
        backend,
        Some(IoReactorBackend::Epoll | IoReactorBackend::IoUring)
    ) || Cx::current().is_none_or(|c| c.timer_driver().is_none())
        || cx.timer_driver().is_none()
    {
        return Err(Error::MissingRuntime);
    }
    cx.checkpoint().map_err(|_| Error::Cancelled)
}
/// The short watchdog is active only while an operation is in flight. It is
/// necessary because the pinned runtime exposes no public cancel-waker API.
/// Socket/pipe readiness still suspends through the reactor, not a busy loop.
async fn bounded<T>(
    cx: &Cx,
    deadline: Deadline,
    future: impl Future<Output = Result<T, Error>>,
) -> Result<T, Error> {
    let mut previous = now(cx)?;
    let mut future = pin!(future);
    let mut timer = pin!(sleep_until(previous.min(deadline.0)));
    poll_fn(|task| {
        let current = match now(cx) {
            Ok(t) => t,
            Err(e) => return Poll::Ready(Err(e)),
        };
        if current < previous {
            return Poll::Ready(Err(Error::ClockRegression));
        }
        previous = current;
        if cx.checkpoint().is_err() {
            return Poll::Ready(Err(Error::Cancelled));
        }
        if current >= deadline.0 {
            return Poll::Ready(Err(Error::Deadline));
        }
        if let Poll::Ready(result) = future.as_mut().poll(task) {
            // A foreign completion cannot extend either the clock deadline or cancellation.
            if cx.checkpoint().is_err() {
                return Poll::Ready(Err(Error::Cancelled));
            }
            return match now(cx) {
                Ok(t) if t < current => Poll::Ready(Err(Error::ClockRegression)),
                Ok(t) if t < deadline.0 => Poll::Ready(result),
                Ok(_) => Poll::Ready(Err(Error::Deadline)),
                Err(e) => Poll::Ready(Err(e)),
            };
        }
        if timer.as_mut().poll(task).is_ready() {
            let next =
                Time::from_nanos(current.as_nanos().saturating_add(10_000_000)).min(deadline.0);
            timer.as_mut().get_mut().reset(next);
            if timer.as_mut().poll(task).is_ready() {
                task.waker().wake_by_ref();
            }
        }
        Poll::Pending
    })
    .await
}

/// One child, two private pipes, one outstanding operation. No live owner is
/// cloneable. Canceled/dropped exchanges kill the process group before releasing
/// their borrow; the owning session must drive `reap` during its drain phase.
pub struct Worker {
    child: Child,
    input: Option<ChildStdin>,
    output: Option<ChildStdout>,
    limits: ProtocolLimits,
    identity: Identity,
    role: Role,
    state: State,
    exit: Option<ExitStatus>,
}
impl fmt::Debug for Worker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MediaWorker")
            .field("state", &self.state)
            .field("role", &self.role)
            .finish_non_exhaustive()
    }
}
impl Worker {
    pub async fn start(
        cx: &Cx,
        launch: Launch,
        configuration: Configuration,
        deadline: Deadline,
    ) -> Result<Self, Error> {
        runtime_ready(cx)?;
        let body = configuration.encode()?;
        let limits = configuration.limits()?;
        if now(cx)? >= deadline.0 {
            return Err(Error::Deadline);
        }
        let mut command = Command::new(&launch.image);
        command
            .env_clear()
            .env("DISPLAY", &launch.display)
            .arg(match launch.role {
                Role::Capture => "--capture",
                Role::Present => "--present",
            })
            .arg("--parent-pid")
            .arg(std::process::id().to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group_mode(ProcessGroupMode::NewProcessGroup)
            .signal_target(ProcessSignalTarget::ProcessGroup)
            .kill_on_drop(true);
        if let Some(path) = &launch.xauthority {
            command.env("XAUTHORITY", path);
        }
        // Spawn is a bounded-count local syscall, not codec setup. The child
        // performs all native initialization behind its private Configure RPC.
        let mut child = command.spawn().map_err(|_| Error::SpawnFailed)?;
        let input = child.stdin().ok_or(Error::PipeFailed)?;
        let output = child.stdout().ok_or(Error::PipeFailed)?;
        let mut worker = Self {
            child,
            input: Some(input),
            output: Some(output),
            limits,
            identity: Identity {
                epoch: launch.epoch,
                sequence: 0,
            },
            role: launch.role,
            state: State::Starting,
            exit: None,
        };
        let result = worker
            .exchange(cx, Kind::Configure, body.clone(), deadline)
            .await;
        match result {
            Ok(reply) if reply.header.kind == Kind::Ready && reply.body() == body => {
                worker.state = State::Running;
                Ok(worker)
            }
            Ok(_) => {
                worker.abort();
                Err(Error::Protocol(worker::Error::WrongState))
            }
            Err(error) => {
                worker.abort();
                Err(error)
            }
        }
    }
    pub const fn state(&self) -> State {
        self.state
    }
    pub fn id(&self) -> Option<u32> {
        self.child.id()
    }
    pub const fn role(&self) -> Role {
        self.role
    }
    pub fn abort(&mut self) {
        if self.state == State::Reaped {
            return;
        }
        self.state = State::Poisoned;
        let _ = self.child.start_kill();
        self.input = None;
        self.output = None;
    }
    pub async fn request(
        &mut self,
        cx: &Cx,
        kind: Kind,
        body: Vec<u8>,
        deadline: Deadline,
    ) -> Result<Record, Error> {
        if self.state != State::Running {
            return Err(Error::Unavailable);
        }
        if !matches!(
            (self.role, kind),
            (Role::Capture, Kind::Capture)
                | (Role::Present, Kind::Present)
                | (_, Kind::Poll | Kind::Stop)
        ) {
            return Err(Error::Protocol(worker::Error::WrongRole));
        }
        self.exchange(cx, kind, body, deadline).await
    }
    async fn exchange(
        &mut self,
        cx: &Cx,
        kind: Kind,
        body: Vec<u8>,
        deadline: Deadline,
    ) -> Result<Record, Error> {
        runtime_ready(cx)?;
        let identity = self.identity;
        let next = identity.sequence.checked_add(1).ok_or(Error::Unavailable)?;
        let request = Record::new(kind, identity, body, &self.limits)?;
        let header = request.header.encode(&self.limits)?;
        let mut guard = Exchange {
            owner: self,
            finished: false,
        };
        let limits = guard.owner.limits;
        let input = guard.owner.input.as_mut().ok_or(Error::Unavailable)?;
        let output = guard.owner.output.as_mut().ok_or(Error::Unavailable)?;
        let response = bounded(cx, deadline, async {
            input
                .write_all(&header)
                .await
                .map_err(|_| Error::PipeFailed)?;
            input
                .write_all(request.body())
                .await
                .map_err(|_| Error::PipeFailed)?;
            input.flush().await.map_err(|_| Error::PipeFailed)?;
            let mut bytes = [0; HEADER_BYTES];
            output
                .read_exact(&mut bytes)
                .await
                .map_err(|_| Error::PeerClosed)?;
            let h = Header::decode(&bytes, &limits)?;
            if h.identity != identity || h.kind.is_request() {
                return Err(Error::Protocol(worker::Error::WrongSequence));
            }
            if !allowed_reply(kind, h.kind) {
                return Err(Error::Protocol(worker::Error::WrongState));
            }
            let mut body = Vec::new();
            body.try_reserve_exact(h.length)
                .map_err(|_| Error::Protocol(worker::Error::Allocation))?;
            body.resize(h.length, 0);
            output
                .read_exact(&mut body)
                .await
                .map_err(|_| Error::PeerClosed)?;
            if h.kind == Kind::Refused {
                return Err(Error::WorkerRefused(worker::Error::from_code(
                    u16::from_be_bytes([body[0], body[1]]),
                )?));
            }
            Record::new(h.kind, h.identity, body, &limits).map_err(Error::from)
        })
        .await?;
        guard.owner.identity.sequence = next;
        if kind == Kind::Stop {
            guard.owner.state = State::Stopped;
            guard.owner.input = None;
            guard.owner.output = None;
        }
        guard.finished = true;
        Ok(response)
    }
    /// Bounded drain independent of the operation's canceled Cx. Expiration
    /// returns `ReapPending` and RETAINS the child owner; it never reports a live
    /// process as reaped. Keep this owner in the bounded closing-worker slot.
    pub async fn reap(&mut self, cx: &Cx, deadline: Deadline) -> Result<ExitStatus, Error> {
        if let Some(status) = self.exit {
            return Ok(status);
        }
        if !matches!(self.state, State::Poisoned | State::Stopped) {
            return Err(Error::Unavailable);
        }
        let mut previous = now(cx)?;
        loop {
            if let Some(status) = self.child.try_wait().map_err(|_| Error::PipeFailed)? {
                self.exit = Some(status);
                self.state = State::Reaped;
                return Ok(status);
            }
            let current = now(cx)?;
            if current < previous {
                return Err(Error::ClockRegression);
            }
            previous = current;
            if current >= deadline.0 {
                self.abort();
                return Err(Error::ReapPending);
            }
            sleep_until(
                Time::from_nanos(current.as_nanos().saturating_add(5_000_000)).min(deadline.0),
            )
            .await;
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        if self.state != State::Reaped {
            self.abort();
        }
    }
}
struct Exchange<'a> {
    owner: &'a mut Worker,
    finished: bool,
}
impl Drop for Exchange<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.owner.abort();
        }
    }
}
fn allowed_reply(request: Kind, reply: Kind) -> bool {
    reply == Kind::Refused
        || match request {
            Kind::Configure => reply == Kind::Ready,
            Kind::Capture => matches!(reply, Kind::Unit | Kind::NeedInput | Kind::NeedDrain),
            Kind::Present => matches!(reply, Kind::Presented | Kind::NeedInput | Kind::NeedDrain),
            Kind::Poll => matches!(reply, Kind::Unit | Kind::Presented | Kind::NeedInput),
            Kind::Stop => reply == Kind::Stopped,
            _ => false,
        }
}
