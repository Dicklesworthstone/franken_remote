//! `frd run`: the installed Linux host service. Composes the existing protected
//! listener, installed-tailnet admission and native desktop driver; nothing here
//! adds a transport, runtime, task or authority path. Each loop iteration owns one
//! OS-share lifetime (capacity-one networking) and is torn down before the next.
//! A viewer leaving, before or after its share is up, is a reported peer outcome
//! that ends only that share; only host faults count toward stopping the service.
//!
//! Profile limits, stated rather than faked: observation only unless the
//! operator passes an input agent (then a first viewer may take the exclusive
//! controlled share, unattended by that choice, with the child's mandatory
//! indicator). An explicitly configured desktop UI can require one-use
//! local consent before any observation; legacy callers refuse that mode. Encoder selection is explicit local policy; legacy entry
//! points retain software, and hardware never silently falls back. The
//! controller's text clipboard is a further, separate operator opt-in
//! (`clipboard`, requires the input agent); it follows the controller's lease.
//! So is the controller's drop directory (`files`, requires the input agent):
//! explicit viewer-to-host file sends land there only under the live lease.
pub mod audio;
pub mod approval;
pub mod encoder;
pub use encoder::Encoder;
pub mod policy;
pub mod video;
pub use audio::AudioOptions;

use crate::{
    input_agent::Seat,
    input_process::{self, ProcessLaunch},
    media::{ObservationControl, host_now},
    native_connection::host::{
        LinuxError, LinuxServer, Request, Server,
        desktop::{Connections, End, PeerResult},
        serial,
    },
    session_agent::{
        ApprovalMode, PermissionKind, PermissionStatus, PlatformKind, SessionAgent,
        source::{
            desktop::{ControlProfile, LocalAction, dispatch},
            prepare::Setup,
        },
    },
    session_monitor::{self, Monitor, Status as Lifetime},
    session_startup::{Configuration, host_offer_with_files, shared_viewers, with_line_scroll},
    worker::{Deadline, Launch, Retirement},
};
use asupersync::{
    cx::Cx,
    net::quic_core::ConnectionId,
    runtime::RuntimeBuilder,
    signal::{SignalKind, signal},
    types::{Budget, CancelKind},
};
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::{CodecConfigurationGeneration, HostBootId, OsSessionId, RemoteSessionId},
    input::{DesktopPoint, InputBounds},
    input_submission::{Capabilities, Capability, PlatformError},
    limits::ProtocolLimits,
};
use fr_media::{
    delivery::SharedFramePool,
    worker::{Configuration as Codec, Role as WorkerRole},
};
use fr_tailnet::{CertificatePolicy, GrantPolicy, LocalApi, Scope, ingress, trust::TrustError};
use fr_transport::native_accept;
use fr_wire::{
    display::{Catalog, Select},
    negotiation::{ControlBinding, Offer},
};
use std::{
    fmt,
    future::{Future, poll_fn},
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    pin::pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::{Poll, Waker},
    time::Duration,
};

/// Local operator choices; nothing here is peer-selectable.
#[derive(Debug, Clone)]
pub struct Options {
    pub socket: Option<PathBuf>,
    pub port: u16,
    pub interface: String,
    pub worker: PathBuf,
    pub display: String,
    pub xauthority: Option<PathBuf>,
    pub trust_roots: PathBuf,
    pub sharing: Scope,
    pub fps: u16,
    pub bitrate: u32,
    /// Replacement nft/ip executables (root-owned); `None` uses /usr/sbin.
    pub ingress_tools: Option<(PathBuf, PathBuf)>,
    /// Serve one OS-share lifetime, then stop (tests and one-shot sharing).
    pub once: bool,
    pub handle_signals: bool,
    /// Absolute `fr-input-agent` image. `None` keeps the host observation-only;
    /// `Some` lets a first viewer take the exclusive controlled share.
    pub input_agent: Option<PathBuf>,
    /// Let that controller's text clipboard follow its lease, through the
    /// input agent image's per-lane `--clipboard` child. Requires `input_agent`.
    pub clipboard: bool,
    /// `--audio`: local playback-audio enable. `None` keeps audio off and the
    /// capability unoffered.
    pub audio: Option<AudioOptions>,
    /// `--files DIR`: the operator's pinned, validated drop directory for the
    /// controller's explicit sends. Requires `input_agent`. `None` keeps the
    /// file capabilities unoffered (a controller asking gets typed absence).
    pub files: Option<crate::native_files::Directory>,
    /// The operator's selected local session and its read-only monitor image.
    /// `Some`: fresh logind evidence is required before binding, and lock,
    /// logout, switch, suspend or lost evidence ends every share and the run
    /// with a typed cause (no automatic resume). `None`: the session lifetime
    /// is not observed; the operator is told that locking does not end sharing.
    pub session_monitor: Option<session_monitor::Configuration>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Runtime,
    Entropy,
    Configuration,
    Cleanup(&'static str),
    Policy(crate::host_policy::live::Error),
    LocalApprovalUnavailable,
    Trust(TrustError),
    Tailnet(fr_tailnet::Error),
    Ingress(ingress::Error),
    Listener(Box<LinuxError>),
    Desktop(dispatch::Error),
    NoTailnetAddress,
    /// The session monitor never produced fresh evidence; nothing was bound.
    SessionMonitor(session_monitor::Error),
    /// The selected session's evidence ended (lock, logout, switch, suspend,
    /// or lost evidence); every share was ended first.
    SessionEnded(session_monitor::Error),
    /// `--input-agent`: the startup probe of the local executor failed
    /// (missing image, display refused, no `XTest`, timeout). Nothing was bound.
    InputAgentUnavailable(PlatformError),
    /// `--input-agent`: the local executor cannot perform an operation control
    /// needs on this display (named). Nothing was bound; observation-only
    /// sharing needs no input agent.
    ControlCapabilityMissing(Capability),
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "frd-run: {self:?}")
    }
}
impl std::error::Error for Error {}
impl Error {
    /// Stable machine-readable refusal code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Runtime => "runtime_unavailable",
            Self::Entropy => "entropy_unavailable",
            Self::Configuration => "invalid_configuration",
            Self::Cleanup(_) => "cleanup_incomplete",
            Self::Policy(_) => "host_policy_unavailable",
            Self::LocalApprovalUnavailable => "local_approval_unavailable",
            Self::Trust(_) => "trust_roots_unavailable",
            Self::Tailnet(fr_tailnet::Error::LocalApiDenied) => "tailscale_permission_denied",
            Self::Tailnet(_) => "tailscale_unavailable",
            // `ingress_unenforced` / `ingress_helper_unavailable`; the Debug
            // detail carries the specific (typed helper) reason.
            Self::Ingress(error) => error.refusal_code(),
            Self::Listener(error) => match **error {
                LinuxError::Ingress(error) => error.refusal_code(),
                _ => "listener_failed",
            },
            Self::Desktop(_) => "desktop_failed",
            Self::NoTailnetAddress => "no_tailnet_addresses",
            Self::SessionMonitor(_) => "session_monitor_unavailable",
            Self::SessionEnded(cause) => session_ended_code(*cause),
            Self::InputAgentUnavailable(_) => "input_agent_unavailable",
            Self::ControlCapabilityMissing(_) => "control_capability_missing",
        }
    }
}

/// Specific causes only where the native evidence names one; everything else
/// (expiry, a dead or misbehaving monitor) is lost evidence, never "locked".
fn session_ended_code(cause: session_monitor::Error) -> &'static str {
    use session_monitor::{Error as E, State};
    match cause {
        E::Native(State::Locked) => "session_locked",
        E::Native(State::Inactive) => "session_inactive",
        E::Native(State::Suspending) => "session_suspending",
        E::Native(State::SessionUnavailable) => "session_ended",
        E::Native(State::IdentityChanged) => "session_identity_changed",
        _ => "session_evidence_lost",
    }
}

/// Progress for the operator; carries no peer names, addresses or content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Fetching this node's Tailscale HTTPS certificate; a first issuance can
    /// take a minute (it is published in Certificate Transparency logs).
    ObtainingCertificate,
    Listening {
        address: SocketAddr,
    },
    /// One peer attempt ended, with the share's listener counters. Idle accept
    /// windows that saw no Initial packet are not reported.
    PeerFinished {
        attempts: u64,
        admitted: u64,
        refused: u64,
        outcome: String,
    },
    ShareEnded {
        outcome: String,
    },
    CleanupFailed {
        stage: &'static str,
    },
    Stopped,
}

pub type Reporter = Arc<dyn Fn(Event) + Send + Sync>;

fn random_bytes<const N: usize>() -> Result<[u8; N], Error> {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes).map_err(|_| Error::Entropy)?;
    Ok(bytes)
}
fn random_nonzero_u128() -> Result<u128, Error> {
    loop {
        let value = u128::from_le_bytes(random_bytes()?);
        if value != 0 {
            return Ok(value);
        }
    }
}
fn random_nonzero_u32() -> Result<u32, Error> {
    loop {
        let value = u32::from_le_bytes(random_bytes()?);
        if value != 0 {
            return Ok(value);
        }
    }
}

/// The host's offer: the four bootstrap capabilities the native viewer
/// requires, plus (only with an input agent) the optional control boundaries
/// and (only with the clipboard enable too) the optional clipboard ones, and
/// (only with a drop directory too) the optional file ones.
/// Optional client capabilities outside this set are dropped by negotiation.
/// `control` is what this host can grant (see [`grantable`]); line scrolling
/// is offered, optionally, only when the probed executor has it.
fn offer(control: Option<Capabilities>, clipboard: bool, audio: bool, files: bool) -> Offer {
    let offer = host_offer_with_files(control.is_some(), clipboard, audio, files);
    if control.is_some_and(|c| c.contains(Capability::LineScroll)) {
        with_line_scroll(offer)
    } else {
        offer
    }
}
/// Operations control cannot work without. An executor missing any of them
/// refuses `frd run --input-agent` at startup rather than every later grant.
const REQUIRED_CONTROL: [Capability; 4] = [
    Capability::Keys,
    Capability::Repeat,
    Capability::Absolute,
    Capability::Buttons,
];
/// What this host can grant: the static policy met with what the local
/// executor measured on this display (plan 15.1). A missing core operation
/// is named; a missing optional one (line scrolling) is simply not offered.
fn grantable(probed: Capabilities) -> Result<Capabilities, Capability> {
    let grant = control_capabilities().meet(probed);
    match REQUIRED_CONTROL.into_iter().find(|&c| !grant.contains(c)) {
        Some(missing) => Err(missing),
        None => Ok(grant),
    }
}
/// One bounded startup probe of the configured `fr-input-agent` on the
/// selected display, before any runtime, listener or tailnet I/O.
fn probe_control(options: &Options) -> Result<Option<Capabilities>, Error> {
    let Some(image) = &options.input_agent else {
        return Ok(None);
    };
    let launch = ProcessLaunch::new(
        image,
        &options.display,
        options.xauthority.as_deref(),
        random_nonzero_u128()?,
    )
    .map_err(|_| Error::Configuration)?;
    let probed = input_process::probe(&launch).map_err(Error::InputAgentUnavailable)?;
    grantable(probed)
        .map(Some)
        .map_err(Error::ControlCapabilityMissing)
}
/// The static host policy for this X11 slice: operations a controller may
/// request if the local executor also has them. Discrete wheel input uses
/// bounded `XTest` press/release pairs, not pixel-scroll emulation.
fn control_capabilities() -> Capabilities {
    Capabilities::default()
        .with(Capability::Keys)
        .with(Capability::Repeat)
        .with(Capability::Absolute)
        .with(Capability::Buttons)
        .with(Capability::LineScroll)
}

fn request(
    attempt: u64,
    host_boot: HostBootId,
    os_session: u32,
    scope: Scope,
    (control, clipboard, files): (Option<Capabilities>, bool, bool),
    audio: bool,
) -> Result<Request, serial::Error> {
    let fresh = |_| serial::Error::Configuration;
    Ok(Request {
        connection_id: ConnectionId::new(&random_bytes::<16>().map_err(fresh)?)
            .map_err(|_| serial::Error::Configuration)?,
        admission: GrantPolicy {
            scope,
            ..GrantPolicy::default()
        },
        session: Configuration {
            offer: offer(control, clipboard, audio, files),
            binding: ControlBinding {
                id: u32::try_from(attempt % u64::from(u32::MAX))
                    .unwrap_or(1)
                    .max(1),
                host_boot,
                os_session: OsSessionId::from_raw(u128::from(os_session)),
                remote_session: RemoteSessionId::from_raw(random_nonzero_u128().map_err(fresh)?),
            },
            // Local approval needs the session-agent process; this profile is
            // unattended by explicit operator choice (checked by the caller).
            require_approval: false,
            startup_timeout: Duration::from_secs(5),
            authority: AuthorityPolicy::plan_defaults(),
            transport: fr_transport::quic::Policy::default(),
        },
    })
}

/// Pick the only display, or the one at the desktop origin when several exist.
fn choose(
    catalog: &Catalog,
    fps: u16,
    bitrate: u32,
    encoder: Encoder,
) -> Result<(Select, Codec), ()> {
    let displays = catalog.displays();
    let display = match displays {
        [only] => only,
        many => many
            .iter()
            .find(|d| d.x == 0 && d.y == 0)
            .or_else(|| many.first())
            .ok_or(())?,
    };
    Ok((
        catalog.selection(display.handle).map_err(|_| ())?,
        Codec {
            width: display.pixel_width,
            height: display.pixel_height,
            fps,
            backend: encoder.backend(),
            bitrate,
            max_access_unit_bytes: ProtocolLimits::ABSOLUTE.max_encoded_access_unit_bytes(),
            generation: CodecConfigurationGeneration::INITIAL,
        },
    ))
}

/// Cooperative stop for signals and embedders: sets a flag and wakes the host
/// loop, so an idle host needs no polling timer to notice it.
#[derive(Default)]
pub struct StopHandle {
    flag: AtomicBool,
    waker: Mutex<Option<Waker>>,
}
impl StopHandle {
    pub fn request(&self) {
        self.flag.store(true, Ordering::Release);
        if let Some(waker) = self.waker.lock().ok().and_then(|mut slot| slot.take()) {
            waker.wake();
        }
    }
    pub fn is_requested(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }
    fn register(&self, waker: &Waker) {
        if let Ok(mut slot) = self.waker.lock() {
            *slot = Some(waker.clone());
        }
    }
}

/// Consecutive host faults (source setup, preparation, capture, consent, clock)
/// that stop the service. Peer outcomes neither count nor reset the run.
const MAX_CONSECUTIVE_FAILURES: u32 = 5;

/// How one OS-share lifetime ended, for the failure policy.
enum Ended {
    /// The source served a viewer, or the share was stopped locally.
    Served,
    /// Its first viewer left, timed out, broke protocol or was refused before
    /// the share was up: a normal peer outcome, not evidence about the host.
    Peer,
    PolicyChanged,
    Failed(dispatch::Error),
    /// The selected session's evidence ended while this share was up.
    Session(session_monitor::Error),
}

/// The terminal cause once the selected session's evidence has ended.
fn session_ended(lifetime: Option<&session_monitor::Control>) -> Option<session_monitor::Error> {
    match lifetime?.status() {
        Lifetime::Stopped(cause) => Some(cause),
        Lifetime::Opening | Lifetime::Active => None,
    }
}

/// Wait for the monitor's first fresh evidence. The monitor bounds opening
/// itself (two seconds), so this cannot wait forever.
async fn session_opened(lifetime: &session_monitor::Control) -> Result<(), Error> {
    poll_fn(|task| {
        lifetime.register(task);
        match lifetime.status() {
            Lifetime::Opening => Poll::Pending,
            Lifetime::Active => Poll::Ready(Ok(())),
            Lifetime::Stopped(cause) => Poll::Ready(Err(Error::SessionMonitor(cause))),
        }
    })
    .await
}

/// Stop the monitor and reap its thread and child within a bound; residue is
/// reported, never silently kept.
fn retire_session_monitor(mut monitor: Monitor, report: &Reporter) {
    monitor.stop();
    let until = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        match monitor.try_finish() {
            Some(Ok(())) => return,
            None if std::time::Instant::now() < until => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Some(Err(_)) | None => break,
        }
    }
    report(Event::CleanupFailed {
        stage: "session_monitor",
    });
}

/// One admitted encoder period, rounded up to the source clock resolution.
/// Backpressure may reduce actual capture; missed opportunities never queue.
fn capture_interval(fps: u16) -> Option<Duration> {
    video::Profile::new(fps, video::DEFAULT_BITRATE)
        .ok()
        .map(video::Profile::capture_interval)
}

/// 1s, 2s, 4s ... capped at 30s; wakes early when stop is requested.
async fn backoff(cx: &Cx, stop: &StopHandle, failures: u32) {
    let delay = Duration::from_secs(1u64 << failures.min(5)).min(Duration::from_secs(30));
    let mut waited = Duration::ZERO;
    while waited < delay && !stop.is_requested() {
        let tick = Duration::from_millis(250);
        asupersync::time::sleep(cx.now(), tick).await;
        waited += tick;
    }
}

/// Run until stopped. Returns after the last share is torn down.
pub fn run(options: &Options, report: &Reporter, stop: &Arc<StopHandle>) -> Result<(), Error> {
    run_inner(options, report, stop, None, Encoder::SoftwareExplicit, None)
}

/// Serve with live saved policy on the original listener and independent source.
/// Every observed revision fences old grants and retires the entire old share
/// before another is opened. Local approval remains a typed refusal in this
/// observation-only host. The caller's explicit overrides never mutate the Store.
pub fn run_with_policy(
    options: &Options,
    report: &Reporter,
    stop: &Arc<StopHandle>,
    policy: policy::Configuration,
) -> Result<(), Error> {
    run_with_policy_and_encoder(options, report, stop, policy, Encoder::SoftwareExplicit)
}

/// Explicit local encoder selection for observation AND controlled shares.
/// The existing worker must still configure the chosen codec, produce admitted
/// HEVC and complete decoder startup. No hardware-availability claim is made by
/// selecting a name, and failure never substitutes software or another device
/// backend. This selection is retained across policy revisions and share retries.
pub fn run_with_policy_and_encoder(
    options: &Options,
    report: &Reporter,
    stop: &Arc<StopHandle>,
    policy: policy::Configuration,
    encoder: Encoder,
) -> Result<(), Error> {
    run_inner(options, report, stop, Some(policy), encoder, None)
}

/// Local desktop consent backed by the installed UI child and an explicitly
/// monitored OS session. Saved policy still decides whether a prompt is required;
/// this supplies a capability to display it, not approval or unattended fallback.
pub fn run_with_desktop_approval(
    options: &Options,
    report: &Reporter,
    stop: &Arc<StopHandle>,
    policy: policy::Configuration,
    encoder: Encoder,
    approval: approval::Configuration,
) -> Result<(), Error> {
    run_inner(options, report, stop, Some(policy), encoder, Some(approval))
}

fn run_inner(
    options: &Options,
    report: &Reporter,
    stop: &Arc<StopHandle>,
    policy: Option<policy::Configuration>,
    encoder: Encoder,
    approval: Option<approval::Configuration>,
) -> Result<(), Error> {
    check(options)?;
    if let Some(approval) = &approval { approval.validate(options)?; }
    let control = probe_control(options)?;
    // Started before anything is bound; kept for the whole run.
    let monitor = match &options.session_monitor {
        Some(configuration) => {
            Some(Monitor::start(configuration.clone()).map_err(Error::SessionMonitor)?)
        }
        None => None,
    };
    let lifetime = monitor.as_ref().map(Monitor::control);
    let result = serve_run(options, report, stop, policy, lifetime.as_ref(), control, encoder, approval.as_ref());
    // Sampled before our own stop: only a native end is a session end.
    let ended = session_ended(lifetime.as_ref());
    if let Some(monitor) = monitor {
        retire_session_monitor(monitor, report);
    }
    match (result, ended) {
        (Ok(()), Some(cause)) => Err(Error::SessionEnded(cause)),
        (result, _) => result,
    }
}

#[allow(clippy::too_many_arguments)]
fn serve_run(
    options: &Options,
    report: &Reporter,
    stop: &Arc<StopHandle>,
    policy: Option<policy::Configuration>,
    lifetime: Option<&session_monitor::Control>,
    control: Option<Capabilities>,
    encoder: Encoder,
    approval: Option<&approval::Configuration>,
) -> Result<(), Error> {
    // One Seat per run: an uncertain release keeps later control refused.
    let seat = Seat::default();
    let runtime = RuntimeBuilder::new()
        .worker_threads(2)
        .enable_platform_reactor(true)
        .build()
        .map_err(|_| Error::Runtime)?;
    let broker = runtime.request_cx_with_budget(Budget::INFINITE);
    let renewal = runtime.request_cx_with_budget(Budget::INFINITE);
    let handle = runtime.handle();
    // The active share's listener supervisor; cancelling it ends that share
    // promptly even while no viewer is connected.
    let active: Arc<Mutex<Option<Cx>>> = Arc::new(Mutex::new(None));
    runtime.block_on(async {
        let mut policy = policy::Owner::start(&broker, policy)?;
        let result = async {
            let mut signals = signals(options.handle_signals);
            // Fresh evidence for the selected session before any tailnet I/O
            // or binding; a locked or unverifiable session shares nothing.
            if let Some(lifetime) = lifetime {
                let Some(opened) =
                    unless_stopped(session_opened(lifetime), stop, &mut signals).await
                else {
                    return Ok(());
                };
                opened?;
            }
            let api = match &options.socket {
                Some(path) => LocalApi::new(path).map_err(Error::Tailnet)?,
                None => LocalApi::installed(),
            };
            let roots =
                fr_tailnet::trust::root_store(&options.trust_roots).map_err(Error::Trust)?;
            let Some(ready) = unless_stopped(policy.ready_with_approval(&broker, approval.is_some()), stop, &mut signals).await
            else {
                return Ok(());
            };
            ready?;
            // Nothing is bound yet, so a stop during the fetch just drops it.
            report(Event::ObtainingCertificate);
            let fetch = api.native_server_identity(&broker, roots, CertificatePolicy::default());
            let identity = unless_stopped(fetch, stop, &mut signals).await;
            let Some(identity) = identity else {
                return Ok(());
            };
            let identity = identity.map_err(Error::Tailnet)?;
            let host_boot = HostBootId::from_raw(random_nonzero_u128()?);
            let renew = identity.serve_renewal(&renewal).map_err(Error::Tailnet)?;
            let serving = async {
                let mut failures = 0u32;
                while !stop.is_requested() {
                    let share = Share {
                        options,
                        runtime: &handle,
                        api: &api,
                        identity: &identity,
                        policy: policy.handle(),
                        host_boot,
                        stop: stop.clone(),
                        active: active.clone(),
                        report,
                        seat: seat.clone(),
                        lifetime,
                        control,
                        encoder,
                        approval,
                    };
                    match share.serve(&broker).await? {
                        Ended::Served => failures = 0,
                        Ended::Peer | Ended::PolicyChanged => {}
                        Ended::Session(cause) => return Err(Error::SessionEnded(cause)),
                        Ended::Failed(error) => {
                            if options.once {
                                return Err(Error::Desktop(error));
                            }
                            failures += 1;
                            if failures >= MAX_CONSECUTIVE_FAILURES {
                                return Err(Error::Desktop(error));
                            }
                            backoff(&broker, stop, failures).await;
                        }
                    }
                    if options.once {
                        break;
                    }
                }
                Ok(())
            };
            let result = Box::pin(supervise(
                async { renew.await.map_err(Error::Tailnet) },
                serving,
                &active,
                stop,
                |task| local_stops(task, &mut signals, lifetime, stop),
            ))
            .await;
            identity.stop();
            result
        }
        .await;
        let cleanup = policy.finish(&broker).await;
        if cleanup.is_err() {
            report(Event::CleanupFailed { stage: "policy" });
        }
        report(Event::Stopped);
        // A failed stop must remain visible even when service already failed.
        cleanup.and(result)
    })
}

type Signals = (
    Option<asupersync::signal::Signal>,
    Option<asupersync::signal::Signal>,
);

/// A handled signal, or the end of the selected session's evidence (lock,
/// logout, switch, suspend) even while idle, stops the run; the stop cancels
/// the active share and its lease promptly.
fn local_stops(
    task: &mut std::task::Context<'_>,
    signals: &mut Signals,
    lifetime: Option<&session_monitor::Control>,
    stop: &StopHandle,
) {
    for s in [&mut signals.0, &mut signals.1].into_iter().flatten() {
        if pin!(s.recv()).poll(task).is_ready() {
            stop.request();
        }
    }
    if let Some(lifetime) = lifetime {
        lifetime.register(task);
        if session_ended(Some(lifetime)).is_some() {
            stop.request();
        }
    }
}

/// The share's local maintenance decision: a local stop, the end of the
/// selected session's evidence, or a changed policy revision ends it.
fn local_action(
    stop: &StopHandle,
    lifetime: Option<&session_monitor::Control>,
    epoch: Option<&crate::host_policy::live::Lease>,
) -> LocalAction {
    if stop.is_requested()
        || session_ended(lifetime).is_some()
        || epoch.is_some_and(|lease| lease.check().is_err())
    {
        LocalAction::Stop
    } else {
        LocalAction::Continue
    }
}

fn check(options: &Options) -> Result<(), Error> {
    if !options.worker.is_absolute()
        || options.display.is_empty()
        || video::Profile::new(options.fps, options.bitrate).is_err()
        || options
            .input_agent
            .as_ref()
            .is_some_and(|p| !p.is_absolute())
        || (options.clipboard && options.input_agent.is_none())
        || (options.files.is_some() && options.input_agent.is_none())
    {
        return Err(Error::Configuration);
    }
    audio::check(options)
}

fn signals(
    enabled: bool,
) -> (
    Option<asupersync::signal::Signal>,
    Option<asupersync::signal::Signal>,
) {
    if enabled {
        (
            signal(SignalKind::interrupt()).ok(),
            signal(SignalKind::terminate()).ok(),
        )
    } else {
        (None, None)
    }
}

/// Drive `work` unless a handled signal or the stop handle ends the wait first
/// (`None`); used only before anything is bound, so dropping `work` is cleanup.
async fn unless_stopped<T>(
    work: impl Future<Output = T>,
    stop: &StopHandle,
    signals: &mut (
        Option<asupersync::signal::Signal>,
        Option<asupersync::signal::Signal>,
    ),
) -> Option<T> {
    let mut work = pin!(work);
    poll_fn(|task| {
        for s in [&mut signals.0, &mut signals.1].into_iter().flatten() {
            if pin!(s.recv()).poll(task).is_ready() {
                stop.request();
            }
        }
        stop.register(task.waker());
        if stop.is_requested() {
            return Poll::Ready(None);
        }
        work.as_mut().poll(task).map(Some)
    })
    .await
}

// Credential failure is a stop request, not permission to abandon a share's
// cleanup await. Retain its first error while the ORIGINAL service fences and
// reaps its resources. The cleanup context is independent and stays usable.
async fn supervise(
    renewal: impl Future<Output = Result<(), Error>>,
    serving: impl Future<Output = Result<(), Error>>,
    active: &Mutex<Option<Cx>>,
    stop: &StopHandle,
    mut signals: impl FnMut(&mut std::task::Context<'_>),
) -> Result<(), Error> {
    let (mut renewal, mut serving) = (pin!(renewal), pin!(serving));
    let mut renewing = true;
    let mut failure = None;
    poll_fn(|task| {
        signals(task);
        if renewing && let Poll::Ready(outcome) = renewal.as_mut().poll(task) {
            renewing = false;
            failure = outcome.err();
            stop.request();
        }
        stop.register(task.waker());
        if stop.is_requested()
            && let Some(supervisor) = active.lock().ok().and_then(|slot| slot.clone())
        {
            supervisor.cancel_fast(CancelKind::User);
        }
        match serving.as_mut().poll(task) {
            Poll::Ready(result) => Poll::Ready(failure.take().map_or(result, Err)),
            Poll::Pending => Poll::Pending,
        }
    })
    .await
}

struct Share<'a> {
    options: &'a Options,
    runtime: &'a asupersync::runtime::RuntimeHandle,
    api: &'a LocalApi,
    identity: &'a fr_tailnet::NativeServerIdentity,
    policy: Option<&'a crate::host_policy::live::Handle>,
    host_boot: HostBootId,
    stop: Arc<StopHandle>,
    active: Arc<Mutex<Option<Cx>>>,
    report: &'a Reporter,
    seat: Seat,
    lifetime: Option<&'a session_monitor::Control>,
    /// Some only with `--input-agent`: the probed grantable operations.
    control: Option<Capabilities>,
    /// Immutable for the run; policy refresh or a native failure cannot switch it.
    encoder: Encoder,
    approval: Option<&'a approval::Configuration>,
}
impl Share<'_> {
    fn cx(&self) -> Result<Cx, Error> {
        self.runtime
            .try_request_cx_with_budget(Budget::INFINITE)
            .map_err(|_| Error::Runtime)
    }

    async fn bind(&self, broker: &Cx) -> Result<LinuxServer, Error> {
        let node = self
            .api
            .node_identity(broker)
            .await
            .map_err(Error::Tailnet)?;
        let ip = node
            .addresses()
            .iter()
            .copied()
            .find(IpAddr::is_ipv4)
            .or_else(|| node.addresses().first().copied())
            .ok_or(Error::NoTailnetAddress)?;
        let address = SocketAddr::new(ip, self.options.port);
        let mut ingress = ingress::Configuration::new(address, &self.options.interface)
            .map_err(Error::Ingress)?;
        if let Some((nft, ip)) = &self.options.ingress_tools {
            ingress = ingress.executables(nft, ip).map_err(Error::Ingress)?;
        }
        // Without CAP_NET_ADMIN the root `frd ingress-helper` owns the rule.
        let helper = std::path::Path::new(ingress::helper::DEFAULT_SOCKET);
        ingress = ingress
            .enforcement(ingress::Enforcement::detect(helper))
            .map_err(Error::Ingress)?;
        let mut server = Server::new(self.api.clone(), self.identity.clone());
        if let Some(policy) = self.policy {
            server = server.with_live_policy(policy.clone());
        }
        server
            .bind_linux(broker, ingress, native_accept::Configuration::default())
            .await
            .map_err(|e| Error::Listener(Box::new(e)))
    }

    fn driver(
        &self,
        source: &Cx,
        os_session: u32,
        audio_retired: &Arc<Mutex<Option<Retirement>>>,
    ) -> Result<dispatch::Driver, Error> {
        let fps = self.options.fps;
        let mut agent = SessionAgent::new(
            ApprovalMode::Unattended,
            PlatformKind::LinuxX11,
            os_session,
            InputBounds::new(DesktopPoint { x: 0, y: 0 }, 8192, 8192)
                .ok_or(Error::Configuration)?,
        );
        // X11 has no capture consent prompt: access to the display (and its
        // XAUTHORITY) chosen locally by the operator IS the permission here.
        agent
            .permissions_mut()
            .set_permission(PermissionKind::ScreenCapture, PermissionStatus::Granted);
        if let (Some(input_agent), Some(capabilities)) = (&self.options.input_agent, self.control) {
            let profile = ControlProfile::new(
                input_agent,
                &self.options.display,
                self.options.xauthority.as_deref(),
                self.seat.clone(),
                capabilities,
                fps,
                self.options.bitrate,
                self.encoder.backend(),
            )
            .map_err(|_| Error::Configuration)?;
            let profile = if self.options.clipboard {
                profile.with_clipboard()
            } else {
                profile
            };
            agent = agent.with_control(match &self.options.files {
                Some(directory) => profile.with_files(directory.clone()),
                None => profile,
            });
        }
        if let Some(options) = &self.options.audio {
            agent = agent.with_audio(audio::profile(
                self.options,
                options,
                audio_retired.clone(),
            )?);
        }
        let entropy: shared_viewers::Entropy = Arc::new(|| random_nonzero_u128().map_err(|_| ()));
        agent
            .native_incoming(
                source.clone(),
                shared_viewers::Policy::default(),
                capture_interval(fps).ok_or(Error::Configuration)?,
                entropy,
            )
            .map(|(_, driver)| driver)
            .map_err(Error::Desktop)
    }

    /// The source is created only after consent and media negotiation, and its
    /// launch receipt is retained for the fixed cleanup order.
    fn factory(
        &self,
        source: Cx,
        retained: Arc<Mutex<Option<Retirement>>>,
    ) -> impl FnOnce() -> std::pin::Pin<Box<dyn Future<Output = Result<Setup, ()>> + Send>> + Send
    {
        let worker = self.options.worker.clone();
        let display = self.options.display.clone();
        let xauthority = self.options.xauthority.clone();
        move || {
            Box::pin(async move {
                let mut authority = SessionAuthority::new(
                    RemoteSessionId::from_raw(random_nonzero_u128().map_err(|_| ())?),
                    AuthorityPolicy::plan_defaults(),
                );
                authority.mark_capabilities_checked().map_err(|_| ())?;
                authority
                    .authorize_observation(host_now(&source).map_err(|_| ())?)
                    .map_err(|_| ())?;
                let control = ObservationControl::new(source, authority).map_err(|_| ())?;
                let (launch, retirement) = Launch::new(
                    &worker,
                    &display,
                    xauthority.as_deref(),
                    WorkerRole::Capture,
                    random_nonzero_u128().map_err(|_| ())?,
                )
                .and_then(Launch::retain_cleanup)
                .map_err(|_| ())?;
                *retained.lock().map_err(|_| ())? = Some(retirement);
                Ok(Setup {
                    control,
                    launch,
                    pool: SharedFramePool::new(ProtocolLimits::ABSOLUTE, 32 * 1024 * 1024, 8)
                        .map_err(|_| ())?,
                })
            })
        }
    }

    /// Fixed cleanup order: the source child, then its launch receipt, then the
    /// firewall owner (refused while a transport still holds its lease).
    async fn cleanup(
        &self,
        driver: &mut dispatch::Driver,
        retirement: &Mutex<Option<Retirement>>,
        audio_retired: &Mutex<Option<Retirement>>,
        linux: &mut LinuxServer,
    ) -> Result<(), Error> {
        let cleanup = self.cx()?;
        let deadline =
            Deadline::after(&cleanup, Duration::from_secs(3)).map_err(|_| Error::Runtime)?;
        let mut failure = None;
        if driver.reap(&cleanup, deadline).await.is_err() {
            (self.report)(Event::CleanupFailed { stage: "source" });
            failure = Some(Error::Cleanup("source"));
        }
        let pending = retirement.lock().ok().and_then(|mut slot| slot.take());
        if let Some(mut retirement) = pending
            && retirement.reap(&cleanup, deadline).await.is_err()
        {
            (self.report)(Event::CleanupFailed { stage: "launch" });
            failure.get_or_insert(Error::Cleanup("launch"));
        }
        // The share's last audio child (already killed with the share).
        let pending = audio_retired.lock().ok().and_then(|mut slot| slot.take());
        if let Some(mut retirement) = pending
            && retirement.reap(&cleanup, deadline).await.is_err()
        {
            (self.report)(Event::CleanupFailed { stage: "audio" });
            failure.get_or_insert(Error::Cleanup("audio"));
        }
        if linux.stop(&cleanup).await.is_err() {
            (self.report)(Event::CleanupFailed { stage: "ingress" });
            failure.get_or_insert(Error::Cleanup("ingress"));
        }
        // Attempt every cleanup stage, but never launch a replacement share
        // after one failed. Restrictive residue is not successful retirement.
        failure.map_or(Ok(()), Err)
    }

    /// Outer error: fatal to the host. `Failed`: this share's source failed
    /// (retried with backoff by the caller). `Peer`: its viewer ended it.
    async fn serve(self, broker: &Cx) -> Result<Ended, Error> {
        let epoch = policy::lease_with_approval(self.policy, self.approval.is_some())?;
        let required = approval::required(epoch.as_ref())?;
        let mut consent = approval::Owner::new(if required { self.approval } else { None }, self.options);
        let result = self.serve_with_consent(broker, epoch, &mut consent).await;
        let cleanup = consent.finish(&self.cx()?).await;
        if cleanup.is_err() { (self.report)(Event::CleanupFailed { stage: "approval" }); }
        cleanup.and(result)
    }

    async fn serve_with_consent(
        &self,
        broker: &Cx,
        epoch: Option<crate::host_policy::live::Lease>,
        consent: &mut approval::Owner,
    ) -> Result<Ended, Error> {
        let mut linux = self.bind(broker).await?;
        (self.report)(Event::Listening {
            address: linux.address(),
        });
        let source = self.cx()?;
        let supervisor = self.cx()?;
        if let Ok(mut slot) = self.active.lock() {
            *slot = Some(supervisor.clone());
        }
        if self.stop.is_requested() {
            supervisor.cancel_fast(CancelKind::User);
        }
        let os_session = random_nonzero_u32()?;
        let audio_retired = Arc::new(Mutex::new(None));
        let mut driver = self.driver(&source, os_session, &audio_retired)?;
        let retirement = Arc::new(Mutex::new(None));
        let factory = self.factory(source, retirement.clone());
        let (fps, bitrate) = (self.options.fps, self.options.bitrate);
        let encoder = self.encoder;
        let (host_boot, scope) = (self.host_boot, self.options.sharing);
        let control = (
            self.control,
            self.options.clipboard,
            self.options.files.is_some(),
        );
        let audio = self.options.audio.is_some();
        let report = self.report.clone();
        let stop = self.stop.clone();
        let source_epoch = epoch.clone();
        let lifetime = self.lifetime.cloned();
        let notify = consent.notify();
        let require_approval = consent.enabled();
        let end = linux
            .serve_desktop(
                &mut driver,
                supervisor,
                self.runtime.clone(),
                serial::Policy::default(),
                Connections {
                    request: move |attempt| {
                        let mut request = request(attempt, host_boot, os_session, scope, control, audio)?;
                        if require_approval {
                            approval::require(&mut request);
                        }
                        Ok(request)
                    },
                    approval: move |original, role| notify.submit(original, role),
                    completed: move |stats: serial::Statistics, outcome: PeerResult| {
                        // An accept window that closed before any Initial arrived
                        // had no peer; it is idle listening, not a refusal.
                        if matches!(
                            outcome,
                            Err(crate::native_connection::host::Error::Accept(
                                fr_transport::native_accept::Error::InitialTimeout
                            ))
                        ) {
                            return Ok(serial::Action::Continue);
                        }
                        report(Event::PeerFinished {
                            attempts: stats.attempts,
                            admitted: stats.admitted,
                            refused: stats.refused,
                            outcome: format!("{outcome:?}"),
                        });
                        Ok(serial::Action::Continue)
                    },
                },
                factory,
                move |catalog| choose(catalog, fps, bitrate, encoder),
                |_, _| {
                    // Local stop, lock/logout/switch and saved-policy revision
                    // beat any delivered positive reply. Host::open independently
                    // rechecks current peer admission before authorizing pixels.
                    Ok(consent.turn(local_action(
                        &stop,
                        lifetime.as_ref(),
                        source_epoch.as_ref(),
                    )))
                },
            )
            .await;
        // Fence outstanding consent before media/ingress cleanup can await.
        consent.close();
        (self.report)(Event::ShareEnded {
            outcome: format!("{end:?}"),
        });
        if let Ok(mut slot) = self.active.lock() {
            *slot = None;
        }
        self.cleanup(&mut driver, &retirement, &audio_retired, &mut linux)
            .await?;
        if let Some(cause) = session_ended(self.lifetime) {
            return Ok(Ended::Session(cause));
        }
        if let Some(epoch) = epoch {
            match epoch.check() {
                Err(crate::host_policy::live::Error::Changed) => return Ok(Ended::PolicyChanged),
                Err(error) => return Err(Error::Policy(error)),
                Ok(_) => {}
            }
        }
        match end {
            End::Listener(Err(error)) => Err(Error::Listener(Box::new(error))),
            End::DrainExpired => Err(Error::Cleanup("session")),
            End::Desktop(Err(error)) if error.is_peer_outcome() => Ok(Ended::Peer),
            End::Desktop(Err(error)) => Ok(Ended::Failed(error)),
            End::Desktop(Ok(_)) | End::Listener(Ok(_)) | End::Cancelled => Ok(Ended::Served),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_end_codes_name_only_what_the_evidence_names() {
        use crate::session_monitor::{Error as E, State};
        for (cause, code) in [
            (E::Native(State::Locked), "session_locked"),
            (E::Native(State::Inactive), "session_inactive"),
            (E::Native(State::Suspending), "session_suspending"),
            (E::Native(State::SessionUnavailable), "session_ended"),
            (
                E::Native(State::IdentityChanged),
                "session_identity_changed",
            ),
            (E::EvidenceExpired, "session_evidence_lost"),
            (E::ProcessExited, "session_evidence_lost"),
            (E::Native(State::Failed), "session_evidence_lost"),
        ] {
            assert_eq!(Error::SessionEnded(cause).code(), code, "{cause:?}");
        }
        assert_eq!(
            Error::SessionMonitor(E::OpeningExpired).code(),
            "session_monitor_unavailable"
        );
    }

    /// A probed executor with the whole required core but no line scrolling.
    fn core() -> Capabilities {
        REQUIRED_CONTROL
            .into_iter()
            .fold(Capabilities::default(), Capabilities::with)
    }

    #[test]
    fn control_is_the_static_policy_met_with_the_probed_executor() {
        use fr_wire::control::{LINE_SCROLL_CAPABILITY, LINE_SCROLL_VERSION};
        let all = [
            Capability::Keys,
            Capability::Repeat,
            Capability::Absolute,
            Capability::Buttons,
            Capability::Relative,
            Capability::PixelScroll,
            Capability::LineScroll,
            Capability::Text,
        ];
        let everything = all
            .into_iter()
            .fold(Capabilities::default(), Capabilities::with);
        // A probe never widens the static policy.
        assert_eq!(grantable(everything), Ok(control_capabilities()));
        // Several X screens or unmapped wheel buttons: the core, no wheel.
        assert_eq!(grantable(core()), Ok(core()));
        // A missing core operation is named; a missing optional one is unoffered.
        for missing in REQUIRED_CONTROL {
            let probed = all
                .into_iter()
                .filter(|&c| c != missing)
                .fold(Capabilities::default(), Capabilities::with);
            assert_eq!(grantable(probed), Err(missing));
        }
        assert_eq!(grantable(Capabilities::default()), Err(Capability::Keys));
        // Line scrolling is offered, optionally, only when grantable.
        let wheel = grantable(core().with(Capability::LineScroll)).unwrap();
        let line_scroll = |c: &fr_wire::negotiation::Capability| c.name == LINE_SCROLL_CAPABILITY;
        let with = offer(Some(wheel), false, false, false);
        let without = offer(Some(core()), false, false, false);
        assert_eq!(with.capabilities.len(), without.capabilities.len() + 1);
        assert!(
            with.capabilities
                .iter()
                .any(|c| line_scroll(c) && c.version == LINE_SCROLL_VERSION && !c.required)
        );
        assert!(!without.capabilities.iter().any(line_scroll));
        assert!(
            !offer(None, true, true, true)
                .capabilities
                .iter()
                .any(line_scroll)
        );
        assert!(with.validate().is_ok());
        assert!(offer(Some(wheel), true, true, true).validate().is_ok());
        assert_eq!(
            Error::InputAgentUnavailable(PlatformError::Unavailable).code(),
            "input_agent_unavailable"
        );
        assert_eq!(
            Error::ControlCapabilityMissing(Capability::Keys).code(),
            "control_capability_missing"
        );
    }

    #[test]
    fn host_offer_matches_the_native_viewer_bootstrap_capabilities() {
        use fr_wire::{attachment, decoder, display, negotiation::Role};
        let observe = offer(None, false, false, false);
        assert_eq!(observe.role, Role::Observe);
        let names: Vec<_> = observe
            .capabilities
            .iter()
            .map(|c| c.name.clone())
            .collect();
        for required in [
            display::CAPABILITY,
            decoder::CAPABILITY,
            attachment::CAPABILITY,
            attachment::DELIVERY_CAPABILITY,
        ] {
            assert!(names.iter().any(|n| n == required), "{required}");
        }
        // Only remote-cursor forwarding and reference recovery are optional;
        // bootstrap stays mandatory. Recovery is offered and used in both
        // modes: a controller's input stays suspended until it completes.
        assert!(observe.capabilities.iter().all(|c| c.required
            != (c.name == fr_wire::cursor::CAPABILITY
                || c.name == fr_wire::recovery_request::CAPABILITY)));
        assert!(observe.capabilities.iter().any(|c| {
            c.name == fr_wire::recovery_request::CAPABILITY
                && c.version == fr_wire::recovery_request::VERSION
        }));
        // Control boundaries are offered only with an input agent, optionally.
        let control = offer(Some(core()), false, false, false);
        assert_eq!(control.capabilities.len(), 10);
        // Audio-down is offered only with the local enable, and optionally.
        for control_offer in [false, true] {
            let without = offer(control_offer.then(core), false, false, false);
            assert!(
                without
                    .capabilities
                    .iter()
                    .all(|c| c.name != fr_wire::audio::CAPABILITY)
            );
            let with = offer(control_offer.then(core), false, true, false);
            assert_eq!(with.capabilities.len(), without.capabilities.len() + 1);
            assert!(with.capabilities.iter().any(|c| {
                c.name == fr_wire::audio::CAPABILITY
                    && c.version == fr_wire::audio::VERSION
                    && !c.required
            }));
            assert!(with.validate().is_ok());
        }
        assert_eq!(
            control.capabilities.iter().filter(|c| c.required).count(),
            4
        );
        // The clipboard enable adds three optional boundaries, only with control.
        assert_eq!(offer(None, true, false, false), observe);
        let clipboard = offer(Some(core()), true, false, false);
        assert_eq!(clipboard.capabilities.len(), 13);
        assert_eq!(
            clipboard.capabilities.iter().filter(|c| c.required).count(),
            4
        );
        // The drop directory adds three optional file boundaries, only with
        // control; the required set never changes.
        assert_eq!(offer(None, false, false, true), observe);
        let files = offer(Some(core()), false, false, true);
        assert_eq!(files.capabilities.len(), control.capabilities.len() + 3);
        assert_eq!(files.capabilities.iter().filter(|c| c.required).count(), 4);
        for (name, version) in crate::session_startup::FILE_CAPABILITIES {
            assert!(
                files
                    .capabilities
                    .iter()
                    .any(|c| c.name == name && c.version == version && !c.required),
                "{name}"
            );
        }
        assert!(files.validate().is_ok());
        for allowed in [
            Capability::Keys,
            Capability::Repeat,
            Capability::Absolute,
            Capability::Buttons,
            Capability::LineScroll,
        ] {
            assert!(control_capabilities().contains(allowed));
        }
        for absent in [
            Capability::Text,
            Capability::PixelScroll,
            Capability::Relative,
        ] {
            assert!(!control_capabilities().contains(absent));
        }
    }

    #[test]
    fn every_request_allocates_fresh_unpredictable_identifiers() {
        let boot = HostBootId::from_raw(9);
        let a = request(1, boot, 5, Scope::OwnUser, (None, false, false), false).unwrap();
        let b = request(
            2,
            boot,
            5,
            Scope::OwnUser,
            (Some(core()), false, false),
            false,
        )
        .unwrap();
        let c = request(
            3,
            boot,
            5,
            Scope::OwnUser,
            (Some(core()), true, false),
            false,
        )
        .unwrap();
        let d = request(4, boot, 5, Scope::OwnUser, (None, false, false), true).unwrap();
        let files = request(
            5,
            boot,
            5,
            Scope::OwnUser,
            (Some(core()), false, true),
            false,
        )
        .unwrap();
        assert_eq!(files.session.offer, offer(Some(core()), false, false, true));
        assert_ne!(
            a.session.binding.remote_session,
            b.session.binding.remote_session
        );
        assert_ne!(a.connection_id, b.connection_id);
        assert_eq!(a.session.binding.os_session.as_raw(), 5);
        assert!(!a.session.require_approval);
        assert_eq!(a.admission.scope, Scope::OwnUser);
        assert_eq!(a.session.offer, offer(None, false, false, false));
        assert_eq!(b.session.offer, offer(Some(core()), false, false, false));
        assert_eq!(c.session.offer, offer(Some(core()), true, false, false));
        assert_eq!(d.session.offer, offer(None, false, true, false));
    }

    #[test]
    fn run_refuses_relative_worker_or_empty_display_before_any_io() {
        let options = Options {
            socket: None,
            port: 8443,
            interface: "tailscale0".into(),
            worker: PathBuf::from("fr-media-worker"),
            display: ":0".into(),
            xauthority: None,
            trust_roots: PathBuf::from(fr_tailnet::trust::SYSTEM_BUNDLE),
            sharing: Scope::OwnUser,
            fps: 30,
            bitrate: 8_000_000,
            ingress_tools: None,
            once: true,
            handle_signals: false,
            input_agent: None,
            clipboard: false,
            audio: None,
            files: None,
            session_monitor: None,
        };
        let report: Reporter = Arc::new(|_| {});
        let stop = Arc::new(StopHandle::default());
        assert_eq!(run(&options, &report, &stop), Err(Error::Configuration));
        for (fps, bitrate) in [(0, 8_000_000), (241, 8_000_000), (30, 9_999), (30, 200_000_001)] {
            let mut invalid_video = options.clone();
            invalid_video.worker = PathBuf::from("/unprovisioned/fr-media-worker");
            invalid_video.input_agent = Some(PathBuf::from("/unprovisioned/fr-input-agent"));
            invalid_video.fps = fps;
            invalid_video.bitrate = bitrate;
            // Configuration refusal must precede even the local executor probe.
            assert_eq!(run(&invalid_video, &report, &stop), Err(Error::Configuration));
        }
        let mut relative_agent = options.clone();
        relative_agent.worker = PathBuf::from("/usr/bin/fr-media-worker");
        relative_agent.input_agent = Some(PathBuf::from("fr-input-agent"));
        assert_eq!(
            run(&relative_agent, &report, &stop),
            Err(Error::Configuration)
        );
        // A drop directory alone (no input agent, so no lease) is refused.
        let private =
            std::env::temp_dir().join(format!("fr-host-run-files-{}", std::process::id()));
        std::fs::create_dir_all(&private).unwrap();
        std::fs::set_permissions(
            &private,
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        let mut files_only = options.clone();
        files_only.worker = PathBuf::from("/usr/bin/fr-media-worker");
        files_only.files = Some(
            crate::native_files::Directory::open(&private, crate::native_files::Limits::default())
                .unwrap(),
        );
        assert_eq!(run(&files_only, &report, &stop), Err(Error::Configuration));
        let _ = std::fs::remove_dir(&private);
        // The clipboard enable alone (no input agent, so no lease) is refused.
        let mut clipboard_only = options.clone();
        clipboard_only.worker = PathBuf::from("/usr/bin/fr-media-worker");
        clipboard_only.clipboard = true;
        assert_eq!(
            run(&clipboard_only, &report, &stop),
            Err(Error::Configuration)
        );
        let mut empty = options.clone();
        empty.worker = PathBuf::from("/usr/bin/fr-media-worker");
        empty.display = String::new();
        assert_eq!(run(&empty, &report, &stop), Err(Error::Configuration));
        // A relative audio server or a hostile sink name refuses before I/O.
        for (server, sink) in [
            ("run/user/1000/pulse/native", None),
            ("/run/user/1000/pulse/native", Some("bad sink")),
            ("/run/user/1000/pulse/native", Some("@DEFAULT_SINK@")),
        ] {
            let mut audio = options.clone();
            audio.worker = PathBuf::from("/usr/bin/fr-media-worker");
            audio.audio = Some(AudioOptions {
                server: PathBuf::from(server),
                sink: sink.map(str::to_owned),
            });
            assert_eq!(run(&audio, &report, &stop), Err(Error::Configuration));
        }
    }

    #[test]
    fn credential_termination_drains_the_share_before_returning_its_original_error() {
        use std::cell::Cell;
        for outcome in [Ok(()), Err(Error::Tailnet(fr_tailnet::Error::KeyExpired))] {
            let runtime = RuntimeBuilder::current_thread()
                .enable_platform_reactor(true)
                .build()
                .unwrap();
            let broker = runtime.request_cx_with_budget(Budget::INFINITE);
            let peer = runtime.request_cx_with_budget(Budget::INFINITE);
            let active = Mutex::new(Some(peer.clone()));
            let stop = StopHandle::default();
            let (polls, cleaned) = (Cell::new(0), Cell::new(false));
            let expected = outcome.clone();
            let result = runtime.block_on(supervise(
                poll_fn(|_| {
                    polls.set(polls.get() + 1);
                    assert_eq!(polls.get(), 1, "never repoll terminal renewal");
                    Poll::Ready(outcome.clone())
                }),
                async {
                    assert!(stop.is_requested());
                    assert!(peer.is_cancel_requested(), "fence before cleanup");
                    asupersync::time::sleep(broker.now(), Duration::from_millis(1)).await;
                    assert!(!broker.is_cancel_requested());
                    cleaned.set(true);
                    Ok(())
                },
                &active,
                &stop,
                |_| {},
            ));
            assert_eq!(result, expected);
            assert!(cleaned.get(), "credential failure must not abandon cleanup");
        }
    }

    #[test]
    fn requested_stop_preserves_cleanup_failure_instead_of_reporting_success() {
        let runtime = RuntimeBuilder::current_thread()
            .enable_platform_reactor(true)
            .build()
            .unwrap();
        let peer = runtime.request_cx_with_budget(Budget::INFINITE);
        let active = Mutex::new(Some(peer.clone()));
        let stop = StopHandle::default();
        let result = runtime.block_on(supervise(
            std::future::pending(),
            async {
                assert!(peer.is_cancel_requested());
                Err(Error::Cleanup("source"))
            },
            &active,
            &stop,
            |_| stop.request(),
        ));
        assert_eq!(result, Err(Error::Cleanup("source")));
    }

    #[test]
    fn a_stop_during_the_certificate_fetch_ends_the_wait_without_polling_it() {
        let runtime = RuntimeBuilder::current_thread()
            .enable_platform_reactor(true)
            .build()
            .unwrap();
        let stop = StopHandle::default();
        let mut signals = (None, None);
        let done = runtime.block_on(unless_stopped(async { 7 }, &stop, &mut signals));
        assert_eq!(done, Some(7));
        stop.request();
        let polled = std::cell::Cell::new(false);
        let stopped = runtime.block_on(unless_stopped(
            async {
                polled.set(true);
                7
            },
            &stop,
            &mut signals,
        ));
        assert_eq!(stopped, None);
        assert!(!polled.get());
    }

    #[test]
    fn capture_pacing_never_outruns_the_encoder_frame_rate() {
        assert_eq!(capture_interval(30), Some(Duration::from_micros(33_334)));
        assert_eq!(capture_interval(15), Some(Duration::from_micros(66_667)));
        assert_eq!(capture_interval(1), Some(Duration::from_secs(1)));
        assert_eq!(capture_interval(0), None);
    }

    #[test]
    fn a_denied_certificate_request_has_its_own_refusal_code() {
        assert_eq!(
            Error::Tailnet(fr_tailnet::Error::LocalApiDenied).code(),
            "tailscale_permission_denied"
        );
        assert_eq!(
            Error::Tailnet(fr_tailnet::Error::LocalApiUnavailable).code(),
            "tailscale_unavailable"
        );
    }
}
