//! Asupersync-managed private media processes with bounded, non-replayable IPC.
#[cfg(unix)]
use asupersync::process::{ProcessGroupMode, ProcessSignalTarget};
use asupersync::{
    cx::Cx,
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, ProcessError, Stdio},
    runtime::{Runtime, reactor::IoReactorBackend},
    time::sleep_until,
    types::Time,
};
use fr_core::limits::ProtocolLimits;
use fr_media::worker::{self, Configuration, HEADER_BYTES, Header, Identity, Kind, Record, Role};
mod launch;
mod launcher;
#[cfg(test)]
mod live_deadline_tests;
mod readiness;
mod request_role;
mod retirement;
pub use retirement::Retirement;
mod selected;
pub use selected::MonitorDiscovery;
mod local;
pub use local::LocalPresenter;
pub mod lifecycle;
pub use lifecycle::*;

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
    SpawnFailed(SpawnFailure),
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

/// Bounded local diagnostics: no executable paths or library error strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnFailure {
    Io {
        kind: std::io::ErrorKind,
        raw_os_error: Option<i32>,
    },
    NotFound,
    PermissionDenied,
    Signaled(i32),
    Unsupported,
    InvalidConfiguration,
}
impl From<ProcessError> for SpawnFailure {
    fn from(error: ProcessError) -> Self {
        match error {
            ProcessError::Io(error) => Self::Io {
                kind: error.kind(),
                raw_os_error: error.raw_os_error(),
            },
            ProcessError::NotFound(_) => Self::NotFound,
            ProcessError::PermissionDenied(_) => Self::PermissionDenied,
            ProcessError::Signaled(signal) => Self::Signaled(signal),
            ProcessError::Unsupported(_) => Self::Unsupported,
            ProcessError::InvalidConfiguration(_) => Self::InvalidConfiguration,
        }
    }
}
impl From<std::io::Error> for SpawnFailure {
    fn from(error: std::io::Error) -> Self {
        match error.kind() {
            std::io::ErrorKind::NotFound => Self::NotFound,
            std::io::ErrorKind::PermissionDenied => Self::PermissionDenied,
            kind => Self::Io {
                kind,
                raw_os_error: error.raw_os_error(),
            },
        }
    }
}
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
    target: Option<worker::presentation::X11Target>,
    fit_target: bool,
    mapped_target: Option<worker::presentation::MappedX11Target>,
    retirement: Option<retirement::Registration>,
    local: Option<Box<dyn LocalPresenter>>,
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
            target: None,
            fit_target: false,
            mapped_target: None,
            retirement: None,
            local: None,
        })
    }
    /// A macOS capture worker. Its desktop is the logged-in window-server
    /// session of this process; no display name or X authority applies.
    #[cfg(target_os = "macos")]
    pub fn macos_capture(image: &Path, epoch: u128) -> Result<Self, Error> {
        Self::session(image, epoch, Role::Capture)
    }
    /// A Windows capture worker. Its desktop is the interactive session this
    /// process runs in; no display name or X authority applies.
    #[cfg(windows)]
    pub fn windows_capture(image: &Path, epoch: u128) -> Result<Self, Error> {
        Self::session(image, epoch, Role::Capture)
    }
    /// A separate macOS playback-capture child in the original login session.
    /// It has no X11 display, cookie, presentation target or input authority.
    #[cfg(target_os = "macos")]
    pub fn macos_audio(image: &Path, epoch: u128) -> Result<Self, Error> {
        Self::session(image, epoch, Role::Audio)
    }
    /// A worker in this process's own desktop session (macOS, Windows).
    #[cfg(any(target_os = "macos", windows))]
    fn session(image: &Path, epoch: u128, role: Role) -> Result<Self, Error> {
        if !image.is_absolute() || epoch == 0 {
            return Err(Error::InvalidLaunch);
        }
        Ok(Self {
            image: image.into(),
            display: String::new(),
            xauthority: None,
            role,
            epoch,
            target: None,
            fit_target: false,
            mapped_target: None,
            retirement: None,
            local: None,
        })
    }
    /// Present through an in-process native decoder instead of a child; see
    /// [`LocalPresenter`]. No image, display or X11 target applies.
    pub fn in_process(presenter: Box<dyn LocalPresenter>, epoch: u128) -> Result<Self, Error> {
        if epoch == 0 {
            return Err(Error::InvalidLaunch);
        }
        Ok(Self {
            image: PathBuf::new(),
            display: String::new(),
            xauthority: None,
            role: Role::Present,
            epoch,
            target: None,
            fit_target: false,
            mapped_target: None,
            retirement: None,
            local: Some(presenter),
        })
    }
    /// Keep the exact child collectable if asynchronous startup fails or is
    /// dropped before returning a Worker. Store the returned owner in the
    /// attempt BEFORE polling startup; it is single-use and not cloneable.
    pub fn retain_cleanup(mut self) -> Result<(Self, Retirement), Error> {
        if self.retirement.is_some() || self.local.is_some() {
            return Err(Error::InvalidLaunch);
        }
        let (registration, owner) = retirement::Registration::new();
        self.retirement = Some(registration);
        Ok((self, owner))
    }
    /// Select a locally owned UI window before decoder startup. This is private
    /// launch configuration, never a peer request or a decoder-selected input
    /// source. The UI must retain its window until native cleanup completes.
    pub fn present_in(mut self, target: worker::presentation::X11Target) -> Result<Self, Error> {
        if self.role != Role::Present || self.target.is_some() || self.local.is_some() {
            return Err(Error::InvalidLaunch);
        }
        self.target = Some(target);
        Ok(self)
    }
    /// Explicitly downscale the full image into this immutable UI-owned window.
    /// The worker preserves decoder geometry and refuses an older/native-only
    /// IPC peer. Rendering and input must use the same returned fit rectangle.
    pub fn present_fitted_in(self, target: worker::presentation::X11Target) -> Result<Self, Error> {
        let mut launch = self.present_in(target)?;
        launch.fit_target = true;
        Ok(launch)
    }
    /// Fit against the original display's pixel dimensions while sampling a
    /// separately validated canonical encoded picture. The exact target and
    /// display mapping are echoed before startup can publish the worker.
    pub fn present_mapped_in(
        self,
        target: worker::presentation::MappedX11Target,
    ) -> Result<Self, Error> {
        let mut launch = self.present_in(target.target())?;
        launch.mapped_target = Some(target);
        Ok(launch)
    }
    /// One-shot startup probe of the locally selected encoder (`nvenc`,
    /// `vaapi` or `software`; `videotoolbox` on macOS; `mediafoundation` on
    /// Windows): the worker opens it at the display's size and admits one
    /// real IDR. Synchronous and bounded, run before any listener exists; the
    /// child is always ended and reaped.
    pub fn probe_encoder(&self, backend: &str, limit: Duration) -> Result<EncoderProbe, Error> {
        use std::io::Read;
        #[cfg(unix)]
        use std::os::unix::process::CommandExt;
        use std::process::{Command as Probe, Stdio as ProbeStdio};
        let known = if cfg!(windows) {
            backend == "mediafoundation"
        } else {
            matches!(backend, "nvenc" | "vaapi" | "software")
                || cfg!(target_os = "macos") && backend == "videotoolbox"
        };
        if self.role != Role::Capture || self.retirement.is_some() || self.local.is_some() || !known
        {
            return Err(Error::InvalidLaunch);
        }
        let mut command = Probe::new(&self.image);
        command.env_clear();
        // A macOS or Windows launch has no X11 display; never pass an empty
        // DISPLAY.
        if !self.display.is_empty() {
            command.env("DISPLAY", &self.display);
        }
        #[cfg(windows)]
        windows_environment(|key, value| {
            command.env(key, value);
        });
        command
            .args(["--probe-encoder", backend, "--parent-pid"])
            .arg(std::process::id().to_string())
            .stdin(ProbeStdio::null())
            .stdout(ProbeStdio::piped())
            .stderr(ProbeStdio::null());
        #[cfg(unix)]
        command.process_group(0);
        if let Some(path) = &self.xauthority {
            command.env("XAUTHORITY", path);
        }
        let mut child = command
            .spawn()
            .map_err(|error| Error::SpawnFailed(error.into()))?;
        let until = std::time::Instant::now() + limit;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) if std::time::Instant::now() < until => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Ok(None) => break Err(Ok(EncoderProbe::TimedOut)),
                Err(error) => break Err(Err(Error::SpawnFailed(error.into()))),
            }
        };
        let status = match status {
            Ok(status) => status,
            Err(outcome) => {
                // As for every worker: end the whole group, then reap.
                #[cfg(unix)]
                crate::input_process::kill_group(&child);
                let _ = child.kill();
                child
                    .wait()
                    .map_err(|error| Error::SpawnFailed(error.into()))?;
                return outcome;
            }
        };
        // A probe leaves nothing behind. The group id cannot be reused while
        // any member lives, so ending it after the leader's exit is safe.
        #[cfg(unix)]
        crate::input_process::kill_group(&child);
        // The reply is already in the pipe. Read what is there without
        // waiting, so a descendant holding the write end cannot stall startup.
        // (A Windows worker starts no process that could hold it: the read
        // ends at the exited child's end of file.)
        let mut reply = [0; 256];
        let read = child.stdout.take().map_or(0, |mut out| {
            #[cfg(unix)]
            if rustix::io::ioctl_fionbio(&out, true).is_err() {
                return 0;
            }
            out.read(&mut reply).unwrap_or(0)
        });
        let line = std::str::from_utf8(&reply[..read]).unwrap_or("");
        Ok(EncoderProbe::parse(status.success(), line.trim()))
    }
}
/// The local system environment a Windows worker needs to load system
/// components (COM and Media Foundation look up `SystemRoot`), from this
/// process's own environment, never a peer's.
#[cfg(windows)]
fn windows_environment(mut set: impl FnMut(&str, &std::ffi::OsStr)) {
    for key in ["SystemRoot", "windir"] {
        if let Some(value) = std::env::var_os(key) {
            set(key, &value);
        }
    }
}
/// Outcome of [`Launch::probe_encoder`]. Content-free: dimensions and the
/// worker's typed refusal name only, never pixels or bitstream bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncoderProbe {
    Admitted { width: u32, height: u32 },
    Refused(String),
    TimedOut,
}
impl EncoderProbe {
    fn parse(success: bool, line: &str) -> Self {
        let field = |name: &str| {
            line.split(' ')
                .find_map(|word| word.strip_prefix(name)?.strip_prefix('='))
        };
        match (success, field("encoder_probe")) {
            (true, Some("admitted")) => match (
                field("width").and_then(|v| v.parse().ok()),
                field("height").and_then(|v| v.parse().ok()),
            ) {
                (Some(width), Some(height)) => Self::Admitted { width, height },
                _ => Self::Refused("malformed_probe_reply".into()),
            },
            (false, Some("refused")) => Self::Refused(
                field("reason")
                    .filter(|r| {
                        r.len() <= 64 && r.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                    })
                    .unwrap_or("unnamed")
                    .into(),
            ),
            _ => Self::Refused("no_probe_reply".into()),
        }
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
    // A real OS reactor (Windows: I/O completion ports), never a lab,
    // browser or injected one.
    if !matches!(
        backend,
        Some(
            IoReactorBackend::Epoll
                | IoReactorBackend::IoUring
                | IoReactorBackend::Kqueue
                | IoReactorBackend::Iocp
        )
    ) || Cx::current().is_none_or(|c| c.timer_driver().is_none())
        || cx.timer_driver().is_none()
    {
        return Err(Error::MissingRuntime);
    }
    cx.checkpoint().map_err(|_| Error::Cancelled)
}
/// The short watchdog is active only while an operation is in flight. It is
/// necessary because the pinned runtime exposes no public cancel-waker API.
/// Only I/O wakeups repoll the exchange. Network turns and this watchdog still
/// check cancellation and deadlines, without repeatedly reading an idle pipe.
async fn bounded<T, E: From<Error>>(
    cx: &Cx,
    deadline: Deadline,
    mut live: impl FnMut() -> Result<Time, E>,
    future: impl Future<Output = Result<T, Error>>,
) -> Result<T, E> {
    let mut previous = now(cx)?;
    let mut future = pin!(future);
    let readiness = readiness::Readiness::new();
    let mut wake_at = previous.min(deadline.0);
    let mut timer = pin!(sleep_until(wake_at));
    poll_fn(|task| {
        if let Err(error) = live_deadline(cx, deadline, &mut previous, &mut live) {
            return Poll::Ready(Err(error));
        }
        let result = readiness.poll(future.as_mut(), task);
        // A foreign poll can consume time or coincide with revocation even if
        // it returns Pending. Recheck the same owner before retaining its work.
        let (current, until) = match live_deadline(cx, deadline, &mut previous, &mut live) {
            Ok(bounds) => bounds,
            Err(error) => return Poll::Ready(Err(error)),
        };
        if let Poll::Ready(result) = result {
            return Poll::Ready(result.map_err(E::from));
        }
        let next = Time::from_nanos(current.as_nanos().saturating_add(10_000_000)).min(until);
        if next < wake_at {
            timer.as_mut().get_mut().reset(next);
            wake_at = next;
        }
        if timer.as_mut().poll(task).is_ready() {
            timer.as_mut().get_mut().reset(next);
            wake_at = next;
            // reset clears the registration. Re-enter to register the new sleep,
            // never poll it twice here: time may already have passed `next`, and
            // a second Ready would leave a completed Sleep for the next turn.
            task.waker().wake_by_ref();
        }
        Poll::Pending
    })
    .await
}

fn check_deadline(cx: &Cx, deadline: Deadline, previous: &mut Time) -> Result<Time, Error> {
    let current = now(cx)?;
    if current < *previous {
        return Err(Error::ClockRegression);
    }
    *previous = current;
    cx.checkpoint().map_err(|_| Error::Cancelled)?;
    if current >= deadline.0 {
        return Err(Error::Deadline);
    }
    Ok(current)
}

fn live_deadline<E: From<Error>>(
    cx: &Cx,
    deadline: Deadline,
    previous: &mut Time,
    live: &mut impl FnMut() -> Result<Time, E>,
) -> Result<(Time, Time), E> {
    check_deadline(cx, deadline, previous)?;
    let until = live()?.min(deadline.0);
    // The callback itself can consume time. It does not authorize a native
    // poll or completion at an already elapsed authority/work boundary.
    let current = check_deadline(cx, deadline, previous)?;
    if current >= until {
        return Err(Error::Deadline.into());
    }
    Ok((current, until))
}

/// One media owner and one outstanding operation. No live owner is cloneable.
/// Canceled/dropped exchanges kill the child process group or fence the local
/// blocking task before releasing their borrow. The owning session must drive
/// `reap` until the exact child or native task retires.
pub struct Worker {
    child: Option<Child>,
    retirement: Option<retirement::Registration>,
    input: Option<ChildStdin>,
    output: Option<ChildStdout>,
    limits: ProtocolLimits,
    identity: Identity,
    role: Role,
    state: State,
    exit: Option<ExitStatus>,
    selected: Option<fr_wire::display::Display>,
    /// In-process presentation, retained until its exact blocking task retires.
    local: Option<local::Owner>,
    in_process: bool,
    /// While a capture worker lives, this process's timed waits (the capture
    /// cadence) end on time instead of on Windows' 15.625 ms default tick.
    #[cfg(windows)]
    _timer: Option<fr_winsys::time::FineTimer>,
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
        Self::start_checked(cx, launch, configuration, deadline, move || Ok(deadline.0)).await
    }
    /// `start` with the configure exchange checked against the caller's live
    /// authority on every poll, as `request_checked` checks running work. A
    /// timely renewal can carry a slow start past the first sampled expiry;
    /// nothing moves the original work ceiling.
    pub(crate) async fn start_checked<E: From<Error> + Send + 'static>(
        cx: &Cx,
        launch: Launch,
        configuration: Configuration,
        deadline: Deadline,
        live: impl FnMut() -> Result<Time, E> + Clone + Send + 'static,
    ) -> Result<Self, E> {
        if launch.mapped_target.is_some() {
            // This target requires an admitted decoder record and its distinct
            // configuration echo; the generic bootstrap cannot discard it.
            return Err(Error::Protocol(worker::Error::WrongState).into());
        }
        let body = configuration.encode().map_err(Error::from)?;
        Self::start_with_body(
            cx,
            launch,
            configuration,
            body,
            Kind::Configure,
            deadline,
            live,
        )
        .await
    }
    /// Start the separate audio role: the worker connects the locally
    /// selected playback monitor and configures real Opus before echoing the
    /// exact configuration. `Unsupported` is a typed unavailable source.
    pub async fn start_audio(
        cx: &Cx,
        launch: Launch,
        capture: &worker::audio::Capture,
        deadline: Deadline,
    ) -> Result<Self, Error> {
        // Role and profile are refused before any runtime or process work.
        if launch.role != Role::Audio {
            return Err(Error::Protocol(worker::Error::WrongRole));
        }
        let body = capture.encode()?;
        let mut worker =
            Self::spawn_checked(cx, launch, ProtocolLimits::ABSOLUTE, deadline, move || {
                Ok::<_, Error>(deadline.0)
            })
            .await?;
        match worker
            .exchange(cx, Kind::ConfigureAudio, body.clone(), deadline)
            .await
        {
            Ok(reply) if reply.header.kind == Kind::AudioReady && reply.body() == body => {
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
    /// Configure a real presentation decoder with exact admitted parameter sets.
    /// The private echo confirms API setup only, never decode or visibility.
    pub async fn start_decoder(
        cx: &Cx,
        launch: Launch,
        configuration: Configuration,
        record: &fr_media::hevc::DecoderRecord,
        deadline: Deadline,
    ) -> Result<Self, Error> {
        if launch.role != Role::Present {
            return Err(Error::Protocol(worker::Error::WrongRole));
        }
        if launch.local.is_some() {
            return Self::start_local(cx, launch, configuration, record, deadline).await;
        }
        let (kind, body) = match launch.target {
            Some(_) if launch.mapped_target.is_some() => (
                Kind::ConfigureMappedPresentation,
                launch
                    .mapped_target
                    .expect("mapped target checked")
                    .encode_decoder(configuration, record)?,
            ),
            Some(target) if launch.fit_target => (
                Kind::ConfigureFittedPresentation,
                target.encode_fitted_decoder(configuration, record)?,
            ),
            Some(target) => (
                Kind::ConfigurePresentation,
                target.encode_decoder(configuration, record)?,
            ),
            None => (
                Kind::ConfigureDecoder,
                configuration.encode_decoder(record)?,
            ),
        };
        Self::start_with_body(cx, launch, configuration, body, kind, deadline, move || {
            Ok(deadline.0)
        })
        .await
    }
    async fn start_with_body<E: From<Error> + Send + 'static>(
        cx: &Cx,
        launch: Launch,
        configuration: Configuration,
        body: Vec<u8>,
        kind: Kind,
        deadline: Deadline,
        live: impl FnMut() -> Result<Time, E> + Clone + Send + 'static,
    ) -> Result<Self, E> {
        let limits = configuration.limits().map_err(Error::from)?;
        let mut worker = Self::spawn_checked(cx, launch, limits, deadline, live.clone()).await?;
        let result = worker
            .exchange_checked(cx, kind, body.clone(), None, deadline, live)
            .await;
        let expected = match kind {
            Kind::ConfigureDecoder => Kind::DecoderReady,
            Kind::ConfigurePresentation => Kind::PresentationReady,
            Kind::ConfigureFittedPresentation => Kind::FittedPresentationReady,
            Kind::ConfigureMappedPresentation => Kind::MappedPresentationReady,
            _ => Kind::Ready,
        };
        match result {
            Ok(reply) if reply.header.kind == expected && reply.body() == body => {
                worker.state = State::Running;
                Ok(worker)
            }
            Ok(_) => {
                worker.abort();
                Err(Error::Protocol(worker::Error::WrongState).into())
            }
            Err(error) => {
                worker.abort();
                Err(error)
            }
        }
    }
    fn spawn(launch: Launch, limits: ProtocolLimits) -> Result<Self, Error> {
        let Launch {
            image,
            display,
            xauthority,
            role,
            epoch,
            target: _, // Already bound into the immutable private configuration.
            fit_target: _,
            mapped_target: _,
            retirement,
            local,
        } = launch;
        if local.is_some() {
            return Err(Error::InvalidLaunch);
        }
        let mut command = Command::new(image);
        command.env_clear();
        if !display.is_empty() {
            command.env("DISPLAY", display);
        }
        #[cfg(windows)]
        windows_environment(|key, value| {
            command.env(key, value);
        });
        command
            .arg(match role {
                Role::Capture => "--capture",
                Role::Present => "--present",
                Role::Audio => "--audio",
            })
            .arg("--parent-pid")
            .arg(std::process::id().to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        // A Unix worker leads its own process group, ended as one. A Windows
        // worker starts no processes; it ends itself when frd does
        // (`--parent-pid`).
        #[cfg(unix)]
        command
            .process_group_mode(ProcessGroupMode::NewProcessGroup)
            .signal_target(ProcessSignalTarget::ProcessGroup);
        if let Some(path) = xauthority {
            command.env("XAUTHORITY", path);
        }
        // The drained blocking launch owns this potentially slow fork/exec.
        // Native codec initialization remains behind the private Configure RPC.
        let mut child = command
            .spawn()
            .map_err(|error| Error::SpawnFailed(error.into()))?;
        let input = child.stdin();
        let output = child.stdout();
        // Establish custody before any fallible post-spawn check. Even absent
        // pipes must leave the original child available for confirmed cleanup.
        let worker = Self {
            child: Some(child),
            retirement,
            input,
            output,
            limits,
            identity: Identity { epoch, sequence: 0 },
            role,
            state: State::Starting,
            exit: None,
            selected: None,
            local: None,
            in_process: false,
            #[cfg(windows)]
            _timer: (role == Role::Capture).then(fr_winsys::time::FineTimer::begin),
        };
        if worker.input.is_none() || worker.output.is_none() {
            return Err(Error::PipeFailed);
        }
        Ok(worker)
    }
    /// Enumerate on the capture child without opening a codec or reading pixels.
    /// The returned owner retains that exact child and X connection until selection.
    pub async fn discover_capture(
        cx: &Cx,
        launch: Launch,
        deadline: Deadline,
    ) -> Result<CaptureDiscovery, Error> {
        if launch.role != Role::Capture {
            return Err(Error::Protocol(worker::Error::WrongRole));
        }
        let mut worker =
            Self::spawn_checked(cx, launch, ProtocolLimits::ABSOLUTE, deadline, move || {
                Ok::<_, Error>(deadline.0)
            })
            .await?;
        let reply = worker
            .exchange(cx, Kind::DiscoverCapture, Vec::new(), deadline)
            .await?;
        let screens = worker::capture::Screens::decode(reply.body())?;
        Ok(CaptureDiscovery { worker, screens })
    }
    pub const fn selected_display(&self) -> Option<fr_wire::display::Display> {
        self.selected
    }
    pub const fn state(&self) -> State {
        self.state
    }
    pub fn id(&self) -> Option<u32> {
        self.child.as_ref().and_then(Child::id)
    }
    pub const fn role(&self) -> Role {
        self.role
    }
    pub fn abort(&mut self) {
        if self.state == State::Reaped {
            return;
        }
        self.state = State::Poisoned;
        if let Some(local) = &mut self.local {
            local.abort();
        }
        if let Some(child) = &mut self.child {
            let _ = child.start_kill();
        }
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
        self.request_with_response_capacity(cx, kind, body, None, deadline)
            .await
    }
    /// Bound a reply's allocation before reading its payload. The caller owns
    /// matching physical credit for the entire exchange; this cannot widen the
    /// worker's negotiated protocol limits. None retains the ordinary per-worker
    /// bound. An oversized reply poisons the IPC sequence before any body read.
    pub async fn request_with_response_capacity(
        &mut self,
        cx: &Cx,
        kind: Kind,
        body: Vec<u8>,
        maximum_capacity: Option<usize>,
        deadline: Deadline,
    ) -> Result<Record, Error> {
        self.request_checked(
            cx,
            kind,
            body,
            maximum_capacity,
            deadline,
            || Ok(deadline.0),
        )
        .await
    }
    /// Keep the original work ceiling while checking the current authority on
    /// every IPC poll. Only the caller's original authority owner can renew it;
    /// no worker reply, watchdog turn, or callback creates admission.
    pub(crate) async fn request_checked<E: From<Error>>(
        &mut self,
        cx: &Cx,
        kind: Kind,
        body: Vec<u8>,
        maximum_capacity: Option<usize>,
        deadline: Deadline,
        live: impl FnMut() -> Result<Time, E>,
    ) -> Result<Record, E> {
        if self.state != State::Running {
            return Err(Error::Unavailable.into());
        }
        if !request_role::allowed(self.role, kind) {
            return Err(Error::Protocol(worker::Error::WrongRole).into());
        }
        if kind == Kind::CheckMonitor && self.selected.is_none() {
            return Err(Error::Protocol(worker::Error::WrongState).into());
        }
        if maximum_capacity == Some(0) {
            return Err(Error::Protocol(worker::Error::ResourceLimit).into());
        }
        // Pixel export has an independent fixed ceiling. It never widens the
        // normal control/AU limits, and a caller may further narrow its credit.
        let maximum_capacity = if kind == Kind::ReadPresentation {
            let ceiling = worker::presentation_snapshot::MAX_BODY_BYTES;
            Some(maximum_capacity.map_or(ceiling, |maximum| maximum.min(ceiling)))
        } else {
            maximum_capacity
        };
        self.exchange_checked(cx, kind, body, maximum_capacity, deadline, live)
            .await
    }
    async fn exchange(
        &mut self,
        cx: &Cx,
        kind: Kind,
        body: Vec<u8>,
        deadline: Deadline,
    ) -> Result<Record, Error> {
        self.exchange_checked(cx, kind, body, None, deadline, || Ok(deadline.0))
            .await
    }
    async fn exchange_checked<E: From<Error>>(
        &mut self,
        cx: &Cx,
        kind: Kind,
        body: Vec<u8>,
        maximum_capacity: Option<usize>,
        deadline: Deadline,
        live: impl FnMut() -> Result<Time, E>,
    ) -> Result<Record, E> {
        if self.in_process {
            return self
                .exchange_local(cx, kind, body, maximum_capacity, deadline, live)
                .await;
        }
        runtime_ready(cx)?;
        let identity = self.identity;
        let next = identity.sequence.checked_add(1).ok_or(Error::Unavailable)?;
        let request = Record::new(kind, identity, body, &self.limits).map_err(Error::from)?;
        let header = request.header.encode(&self.limits).map_err(Error::from)?;
        let mut guard = Exchange {
            owner: self,
            finished: false,
        };
        let limits = guard.owner.limits;
        let input = guard.owner.input.as_mut().ok_or(Error::Unavailable)?;
        let output = guard.owner.output.as_mut().ok_or(Error::Unavailable)?;
        let response = bounded(cx, deadline, live, async {
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
            if maximum_capacity.is_some_and(|maximum| h.length > maximum) {
                return Err(Error::Protocol(worker::Error::ResourceLimit));
            }
            let mut body = Vec::new();
            body.try_reserve_exact(h.length)
                .map_err(|_| Error::Protocol(worker::Error::Allocation))?;
            // Vec may reserve more than requested. Never read pixels into an
            // allocation which exceeds the caller's retained physical credit.
            if maximum_capacity.is_some_and(|maximum| body.capacity() > maximum) {
                return Err(Error::Protocol(worker::Error::ResourceLimit));
            }
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
    /// returns `ReapPending` and RETAINS the child or local task owner; it never
    /// reports live native work as reaped. Keep the bounded closing-worker slot.
    pub async fn reap(&mut self, cx: &Cx, deadline: Deadline) -> Result<ExitStatus, Error> {
        if let Some(status) = self.exit {
            return Ok(status);
        }
        if !matches!(self.state, State::Poisoned | State::Stopped) {
            return Err(Error::Unavailable);
        }
        if self.in_process {
            return self.reap_local(cx, deadline).await;
        }
        let mut previous = now(cx)?;
        loop {
            if let Some(status) = self
                .child
                .as_mut()
                .ok_or(Error::Unavailable)?
                .try_wait()
                .map_err(|_| Error::PipeFailed)?
            {
                self.exit = Some(status);
                if let Some(registration) = &self.retirement {
                    registration.reaped(status);
                }
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
        if let (Some(registration), Some(child)) = (&self.retirement, self.child.take()) {
            registration.retire(child, self.exit);
        }
    }
}
/// Single-use discovered capture process. Native root IDs stay on these private
/// pipes; the broker assigns separate network display handles after local policy.
pub struct CaptureDiscovery {
    worker: Worker,
    screens: worker::capture::Screens,
}
impl fmt::Debug for CaptureDiscovery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CaptureDiscovery")
            .field("screens", &self.screens.entries().len())
            .finish_non_exhaustive()
    }
}
impl CaptureDiscovery {
    pub fn screens(&self) -> &worker::capture::Screens {
        &self.screens
    }
    pub fn id(&self) -> Option<u32> {
        self.worker.id()
    }
    pub fn abort(&mut self) {
        self.worker.abort();
    }
    pub async fn reap(&mut self, cx: &Cx, deadline: Deadline) -> Result<ExitStatus, Error> {
        self.worker.reap(cx, deadline).await
    }
    pub async fn stop(&mut self, cx: &Cx, deadline: Deadline) -> Result<(), Error> {
        if self.worker.state != State::Starting {
            return Err(Error::Unavailable);
        }
        self.worker
            .exchange(cx, Kind::Stop, Vec::new(), deadline)
            .await?;
        Ok(())
    }
    pub async fn configure(
        mut self,
        cx: &Cx,
        screen: worker::capture::Screen,
        configuration: Configuration,
        deadline: Deadline,
    ) -> Result<Worker, Error> {
        if self.worker.state != State::Starting || !self.screens.contains(screen) {
            return Err(Error::Protocol(worker::Error::WrongState));
        }
        let body = worker::capture::configure(configuration, screen)?;
        self.worker.limits = configuration.limits()?;
        let reply = self
            .worker
            .exchange(cx, Kind::ConfigureCapture, body.clone(), deadline)
            .await?;
        if reply.body() != body {
            return Err(Error::Protocol(worker::Error::WrongState));
        }
        self.worker.state = State::Running;
        Ok(self.worker)
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
            Kind::DiscoverCapture => reply == Kind::CaptureScreens,
            Kind::DiscoverMonitors => reply == Kind::CaptureMonitors,
            Kind::ConfigureMonitor => reply == Kind::MonitorReady,
            Kind::CheckMonitor => reply == Kind::MonitorValid,
            Kind::ConfigureCapture => reply == Kind::CaptureReady,
            Kind::ConfigureDecoder => reply == Kind::DecoderReady,
            Kind::ConfigurePresentation => reply == Kind::PresentationReady,
            Kind::ConfigureFittedPresentation => reply == Kind::FittedPresentationReady,
            Kind::ConfigureMappedPresentation => reply == Kind::MappedPresentationReady,
            Kind::Capture => matches!(reply, Kind::Unit | Kind::NeedInput | Kind::NeedDrain),
            Kind::CaptureIfChanged => matches!(
                reply,
                Kind::Unit | Kind::Unchanged | Kind::NeedInput | Kind::NeedDrain
            ),
            Kind::CaptureAtRate => matches!(
                reply,
                Kind::RateUnit | Kind::RateUnchanged | Kind::NeedInput | Kind::NeedDrain
            ),
            Kind::Present => matches!(reply, Kind::Presented | Kind::NeedInput | Kind::NeedDrain),
            Kind::Decode => matches!(reply, Kind::Decoded | Kind::NeedInput | Kind::NeedDrain),
            Kind::Poll => matches!(
                reply,
                Kind::Unit | Kind::RateUnit | Kind::Presented | Kind::Decoded | Kind::NeedInput
            ),
            Kind::Stop => reply == Kind::Stopped,
            // Typed cursor states (outside/unsupported/unrepresentable) share
            // one reply kind; NeedInput means the pointer moved mid-query.
            Kind::ReadCursor => matches!(reply, Kind::CursorSnapshot | Kind::NeedInput),
            Kind::CursorOverlay => reply == Kind::CursorOverlayApplied,
            Kind::ReadPresentation => {
                matches!(reply, Kind::PresentationPixels | Kind::PresentationAbsent)
            }
            Kind::ConfigureAudio => reply == Kind::AudioReady,
            // Zero packets is an ordinary AudioPackets reply, never NeedInput.
            Kind::ReadAudio => reply == Kind::AudioPackets,
            _ => false,
        }
}

#[cfg(test)]
mod tests {

    use super::*;
    use asupersync::{
        runtime::RuntimeBuilder,
        time::{TimerDriverHandle, VirtualClock},
    };
    use std::sync::Arc;

    #[test]
    fn a_claimed_late_launch_keeps_its_actual_child_until_retirement() {
        let runtime = RuntimeBuilder::new()
            .worker_threads(1)
            .blocking_threads(1, 2)
            .enable_platform_reactor(true)
            .build()
            .unwrap();
        runtime.block_on(async {
            let cleanup = Cx::current().unwrap();
            for expire in [false, true] {
                let owner = runtime.request_cx_with_budget(asupersync::types::Budget::INFINITE);
                let (launch, mut closing) =
                    Launch::new(&probe_image("exec sleep 60"), ":0", None, Role::Capture, 74)
                        .unwrap()
                        .retain_cleanup()
                        .unwrap();
                let until = Deadline::after(&owner, Duration::from_secs(2)).unwrap();
                let caller_thread = std::thread::current().id();
                let (entered, receipt) = std::sync::mpsc::channel();
                let (release, held) = std::sync::mpsc::channel::<()>();
                let held = Arc::new(std::sync::Mutex::new(held));
                let mut reads = 0;
                let mut starting = Box::pin(Worker::spawn_checked(
                    &owner,
                    launch,
                    ProtocolLimits::ABSOLUTE,
                    until,
                    move || {
                        // Hold the blocking thread's second authority check:
                        // the real child already exists, but is unpublished.
                        if std::thread::current().id() != caller_thread {
                            reads += 1;
                            if reads == 2 {
                                entered.send(()).unwrap();
                                let _ = held.lock().unwrap().recv_timeout(Duration::from_secs(5));
                            }
                        }
                        Ok::<_, Error>(until.time())
                    },
                ));
                poll_fn(|task| {
                    assert!(starting.as_mut().poll(task).is_pending());
                    if receipt.try_recv().is_ok() {
                        Poll::Ready(())
                    } else {
                        Poll::Pending
                    }
                })
                .await;
                if expire {
                    assert_eq!(starting.await.unwrap_err(), Error::Deadline);
                } else {
                    drop(starting);
                }
                let status = {
                    let deadline = Deadline::after(&cleanup, Duration::from_secs(1)).unwrap();
                    let mut reaping = pin!(closing.reap(&cleanup, deadline));
                    poll_fn(|task| {
                        assert!(reaping.as_mut().poll(task).is_pending());
                        Poll::Ready(())
                    })
                    .await;
                    drop(release);
                    reaping.await.unwrap().unwrap()
                };
                assert!(!status.success());
                assert_eq!(
                    closing
                        .reap(
                            &cleanup,
                            Deadline::after(&cleanup, Duration::from_secs(1)).unwrap()
                        )
                        .await,
                    Ok(Some(status))
                );
            }
        });
    }

    #[test]
    fn presentation_export_replies_are_private_to_the_explicit_request() {
        for reply in [Kind::PresentationPixels, Kind::PresentationAbsent] {
            assert!(allowed_reply(Kind::ReadPresentation, reply));
            for request in [
                Kind::Present,
                Kind::Decode,
                Kind::Poll,
                Kind::ReadCursor,
                Kind::Capture,
            ] {
                assert!(!allowed_reply(request, reply));
            }
        }
        for reply in [
            Kind::Presented,
            Kind::Decoded,
            Kind::NeedInput,
            Kind::CursorSnapshot,
        ] {
            assert!(!allowed_reply(Kind::ReadPresentation, reply));
        }
    }

    /// A stand-in worker image speaking only the probe's one-line reply.
    fn probe_image(script: &str) -> std::path::PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "fr-probe-{}-{}.sh",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        crate::executable::write(&path, format!("#!/bin/sh\n{script}\n"));
        path
    }
    fn probe(script: &str, backend: &str, limit: Duration) -> Result<EncoderProbe, Error> {
        let image = probe_image(script);
        Launch::new(&image, ":0", None, Role::Capture, 7)
            .unwrap()
            .probe_encoder(backend, limit)
    }

    /// The process whose pid the stand-in wrote is gone (or a zombie) within 2 s.
    fn assert_descendant_ended(marker: &std::path::Path) {
        let descendant = std::fs::read_to_string(marker).unwrap();
        let stat = format!("/proc/{}/stat", descendant.trim());
        let until = std::time::Instant::now() + Duration::from_secs(2);
        while std::fs::read_to_string(&stat).is_ok_and(|line| {
            line.rsplit(") ")
                .next()
                .is_some_and(|s| !s.starts_with('Z'))
        }) {
            assert!(std::time::Instant::now() < until, "descendant survived");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn encoder_probe_reports_admission_typed_refusal_silence_and_timeout() {
        let second = Duration::from_secs(5);
        assert_eq!(
            probe(
                "echo 'encoder_probe=admitted width=1920 height=1080 idr_bytes=4096'",
                "nvenc",
                second
            ),
            Ok(EncoderProbe::Admitted {
                width: 1920,
                height: 1080
            })
        );
        assert_eq!(
            probe(
                "echo 'encoder_probe=refused reason=Unsupported'; exit 2",
                "vaapi",
                second
            ),
            Ok(EncoderProbe::Refused("Unsupported".into()))
        );
        // A crash or a missing reply is a refusal, never an admission.
        assert_eq!(
            probe("exit 1", "nvenc", second),
            Ok(EncoderProbe::Refused("no_probe_reply".into()))
        );
        assert_eq!(
            probe(
                "echo 'encoder_probe=admitted width=1920 height=1080'; exit 3",
                "nvenc",
                second
            ),
            Ok(EncoderProbe::Refused("no_probe_reply".into()))
        );
        // Untrusted reply text never reaches the refusal unfiltered.
        assert_eq!(
            probe(
                "echo 'encoder_probe=refused reason=a/b'; exit 2",
                "nvenc",
                second
            ),
            Ok(EncoderProbe::Refused("unnamed".into()))
        );
        // A hung child is ended at the bound and reaped.
        let started = std::time::Instant::now();
        assert_eq!(
            probe("sleep 30", "nvenc", Duration::from_millis(300)),
            Ok(EncoderProbe::TimedOut)
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        // A missing image is a typed spawn failure, not a refusal.
        assert_eq!(
            Launch::new(
                std::path::Path::new("/nonexistent/fr-media-worker"),
                ":0",
                None,
                Role::Capture,
                7
            )
            .unwrap()
            .probe_encoder("nvenc", second),
            Err(Error::SpawnFailed(SpawnFailure::NotFound))
        );
        // Only a capture launch probes, and only the three local names.
        assert_eq!(probe("exit 0", "h264", second), Err(Error::InvalidLaunch));
        let image = probe_image("exit 0");
        assert_eq!(
            Launch::new(&image, ":0", None, Role::Present, 7)
                .unwrap()
                .probe_encoder("nvenc", second),
            Err(Error::InvalidLaunch)
        );
    }

    #[test]
    fn encoder_probe_leaves_no_descendant_and_never_waits_on_one() {
        let second = Duration::from_secs(5);
        // The bound ends the probe's whole process group, not only the child.
        let marker =
            std::env::temp_dir().join(format!("fr-probe-{}-descendant.pid", std::process::id()));
        assert_eq!(
            probe(
                &format!("sleep 30 & echo $! > '{}'; wait", marker.display()),
                "nvenc",
                Duration::from_millis(300)
            ),
            Ok(EncoderProbe::TimedOut)
        );
        assert_descendant_ended(&marker);
        // A replying child that leaves a descendant holding its stdout neither
        // stalls the probe nor leaves the descendant running.
        let started = std::time::Instant::now();
        assert_eq!(
            probe(
                &format!(
                    "echo 'encoder_probe=admitted width=1 height=1'; sleep 30 & echo $! > '{}'",
                    marker.display()
                ),
                "nvenc",
                second
            ),
            Ok(EncoderProbe::Admitted {
                width: 1,
                height: 1
            })
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_descendant_ended(&marker);
        // One that escaped the group (its own session) still cannot stall it.
        let started = std::time::Instant::now();
        assert_eq!(
            probe(
                "echo 'encoder_probe=admitted width=1 height=1'; setsid sleep 4 &",
                "nvenc",
                second
            ),
            Ok(EncoderProbe::Admitted {
                width: 1,
                height: 1
            })
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn audio_packets_answer_only_audio_pulls_and_never_other_roles() {
        assert!(allowed_reply(Kind::ReadAudio, Kind::AudioPackets));
        assert!(allowed_reply(Kind::ConfigureAudio, Kind::AudioReady));
        for (request, reply) in [
            (Kind::ReadAudio, Kind::NeedInput),
            (Kind::ReadAudio, Kind::Unit),
            (Kind::Capture, Kind::AudioPackets),
            (Kind::Poll, Kind::AudioPackets),
            (Kind::ReadCursor, Kind::AudioPackets),
            (Kind::Configure, Kind::AudioReady),
        ] {
            assert!(!allowed_reply(request, reply), "{request:?} {reply:?}");
        }
        // Only the audio role may pull audio, and it has no video requests.
        let launch = |role| {
            Launch::new(
                std::path::Path::new("/usr/bin/fr-media-worker"),
                ":0",
                None,
                role,
                7,
            )
            .unwrap()
        };
        assert_eq!(launch(Role::Audio).role, Role::Audio);
        let runtime = RuntimeBuilder::current_thread()
            .with_timer_driver(TimerDriverHandle::with_virtual_clock(Arc::new(
                VirtualClock::new(),
            )))
            .build()
            .unwrap();
        runtime.block_on(async {
            let cx = Cx::current().unwrap();
            let deadline = Deadline::after(&cx, Duration::from_secs(1)).unwrap();
            let capture = worker::audio::Capture {
                generation: fr_core::audio::AudioGeneration::from_raw(1),
                channels: fr_core::audio::AudioChannels::Stereo,
                frame_duration_ms: 20,
                bitrate: 96_000,
                max_packet_bytes: 1000,
                source: worker::audio::Source::Pulse {
                    server: "/run/user/1000/pulse/native".into(),
                    monitor: worker::audio::Monitor::DefaultSink,
                },
            };
            // A capture/present launch can never start the audio role, and
            // a hostile profile refuses before any spawn.
            for role in [Role::Capture, Role::Present] {
                assert!(matches!(
                    Worker::start_audio(&cx, launch(role), &capture, deadline).await,
                    Err(Error::Protocol(worker::Error::WrongRole))
                ));
            }
            let hostile = worker::audio::Capture {
                source: worker::audio::Source::Pulse {
                    server: "relative/native".into(),
                    monitor: worker::audio::Monitor::DefaultSink,
                },
                ..capture
            };
            assert!(matches!(
                Worker::start_audio(&cx, launch(Role::Audio), &hostile, deadline).await,
                Err(Error::Protocol(worker::Error::Malformed))
            ));
        });
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_audio_launch_keeps_its_distinct_role_and_has_no_x11_selection() {
        let image = std::path::Path::new("/local/fr-media-worker");
        let audio = Launch::macos_audio(image, 7).unwrap();
        assert_eq!(audio.role, Role::Audio);
        assert!(audio.display.is_empty() && audio.xauthority.is_none());
        assert!(audio.target.is_none() && audio.local.is_none());
        assert_eq!(Launch::macos_capture(image, 8).unwrap().role, Role::Capture);
        assert!(matches!(
            Launch::macos_audio(image, 0),
            Err(Error::InvalidLaunch)
        ));
        assert!(matches!(
            Launch::macos_audio(std::path::Path::new("relative-worker"), 7),
            Err(Error::InvalidLaunch)
        ));
    }

    #[test]
    fn unchanged_is_only_a_reply_to_an_explicit_conditional_capture() {
        assert!(allowed_reply(Kind::CaptureIfChanged, Kind::Unchanged));
        assert!(allowed_reply(Kind::CaptureIfChanged, Kind::Unit));
        for request in [
            Kind::Capture,
            Kind::Present,
            Kind::Decode,
            Kind::Poll,
            Kind::Configure,
            Kind::Stop,
        ] {
            assert!(!allowed_reply(request, Kind::Unchanged));
        }
    }

    #[test]
    fn unrelated_turns_do_not_poll_io_but_still_enforce_the_original_deadline() {
        let clock = Arc::new(VirtualClock::new());
        let runtime = RuntimeBuilder::current_thread()
            .with_timer_driver(TimerDriverHandle::with_virtual_clock(Arc::clone(&clock)))
            .build()
            .unwrap();
        runtime.block_on(async {
            let cx = Cx::current().unwrap();
            let deadline = Deadline::after(&cx, Duration::from_secs(1)).unwrap();
            let polls = std::cell::Cell::new(0);
            let mut pending = pin!(bounded(
                &cx,
                deadline,
                || Ok::<_, Error>(deadline.time()),
                poll_fn(|_| {
                    polls.set(polls.get() + 1);
                    Poll::<Result<(), Error>>::Pending
                })
            ));
            poll_fn(|task| {
                for _ in 0..1000 {
                    assert!(pending.as_mut().poll(task).is_pending());
                    clock.advance(500_000);
                }
                assert_eq!(polls.get(), 1);
                clock.advance(500_000_000);
                assert_eq!(
                    pending.as_mut().poll(task),
                    Poll::Ready(Err(Error::Deadline))
                );
                assert_eq!(polls.get(), 1);
                Poll::Ready(())
            })
            .await;
        });
    }

    #[test]
    fn elapsed_rearmed_watchdog_is_never_polled_after_completion() {
        let clock = Arc::new(VirtualClock::new());
        let runtime = RuntimeBuilder::current_thread()
            .with_timer_driver(TimerDriverHandle::with_virtual_clock(Arc::clone(&clock)))
            .build()
            .unwrap();
        runtime.block_on(async {
            let cx = Cx::current().unwrap();
            let deadline = Deadline::after(&cx, Duration::from_secs(1)).unwrap();
            let mut polls = 0;
            let result = bounded(
                &cx,
                deadline,
                || Ok::<_, Error>(deadline.time()),
                poll_fn(|task| {
                    polls += 1;
                    task.waker().wake_by_ref();
                    match polls {
                        1 => {
                            // I/O polling may consume the entire next watchdog interval.
                            // Advancing lab time forces that race without sleeping.
                            clock.advance(20_000_000);
                            Poll::Pending
                        }
                        2 => Poll::Pending,
                        _ => Poll::Ready(Ok::<(), Error>(())),
                    }
                }),
            )
            .await;
            assert_eq!(result, Ok(()));
            assert_eq!(polls, 3);
        });
    }
}
