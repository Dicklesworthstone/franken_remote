//! `frd run`: the installed Linux host service. Composes the existing protected
//! listener, installed-tailnet admission and native desktop driver; nothing here
//! adds a transport, runtime, task or authority path. Each loop iteration owns one
//! OS-share lifetime (capacity-one networking) and is torn down before the next.
//! A viewer leaving, before or after its share is up, is a reported peer outcome
//! that ends only that share; only host faults count toward stopping the service.
//!
//! Profile limits, stated rather than faked: observation only (no input is
//! admitted, so the agent's input bounds are the protocol ceiling), local
//! approval is refused until the separate session-agent process exists, and the
//! encoder is the explicit software HEVC profile.
pub mod policy;

use crate::{
    media::{ObservationControl, host_now},
    native_connection::host::{
        LinuxError, LinuxServer, Request, Server,
        desktop::{Connections, End, PeerResult},
        serial,
    },
    session_agent::{
        ApprovalMode, PermissionKind, PermissionStatus, PlatformKind, SessionAgent,
        source::{
            desktop::{LocalAction, dispatch},
            prepare::Setup,
        },
    },
    session_startup::{Configuration, shared_viewers},
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
    limits::ProtocolLimits,
};
use fr_media::{
    delivery::SharedFramePool,
    worker::{Backend, Configuration as Codec, Role as WorkerRole},
};
use fr_tailnet::{CertificatePolicy, GrantPolicy, LocalApi, Scope, ingress, trust::TrustError};
use fr_transport::native_accept;
use fr_wire::{
    attachment, decoder,
    display::{self, Catalog, Select},
    negotiation::{Capability, ControlBinding, Offer, Role},
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
            Self::Ingress(_) => "ingress_unenforced",
            Self::Listener(_) => "listener_failed",
            Self::Desktop(_) => "desktop_failed",
            Self::NoTailnetAddress => "no_tailnet_addresses",
        }
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

/// The host's observation offer: the four bootstrap capabilities the native
/// viewer requires. Optional client capabilities outside this set are dropped
/// by negotiation rather than silently assumed.
fn offer() -> Offer {
    let mut capabilities: Vec<_> = [
        display::CAPABILITY,
        decoder::CAPABILITY,
        attachment::CAPABILITY,
        attachment::DELIVERY_CAPABILITY,
    ]
    .into_iter()
    .map(|name| Capability {
        name: name.into(),
        version: 1,
        required: true,
    })
    .collect();
    capabilities.sort_by(|a, b| a.name.cmp(&b.name));
    Offer {
        versions: vec![0],
        profile: 1,
        profile_version: 0,
        role: Role::Observe,
        limits: ProtocolLimits::ABSOLUTE,
        capabilities,
    }
}

fn request(
    attempt: u64,
    host_boot: HostBootId,
    os_session: u32,
    scope: Scope,
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
            offer: offer(),
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
fn choose(catalog: &Catalog, fps: u16, bitrate: u32) -> Result<(Select, Codec), ()> {
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
            backend: Backend::SoftwareExplicit,
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
    run_inner(options, report, stop, None)
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
    run_inner(options, report, stop, Some(policy))
}

fn run_inner(
    options: &Options,
    report: &Reporter,
    stop: &Arc<StopHandle>,
    policy: Option<policy::Configuration>,
) -> Result<(), Error> {
    if !options.worker.is_absolute()
        || options.display.is_empty()
        || options.fps == 0
        || options.bitrate == 0
    {
        return Err(Error::Configuration);
    }
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
            let api = match &options.socket {
                Some(path) => LocalApi::new(path).map_err(Error::Tailnet)?,
                None => LocalApi::installed(),
            };
            let roots =
                fr_tailnet::trust::root_store(&options.trust_roots).map_err(Error::Trust)?;
            let mut signals = signals(options.handle_signals);
            let Some(ready) = unless_stopped(policy.ready(&broker), stop, &mut signals).await
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
                    };
                    match share.serve(&broker).await? {
                        Ended::Served => failures = 0,
                        Ended::Peer | Ended::PolicyChanged => {}
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
                |task| {
                    for s in [&mut signals.0, &mut signals.1].into_iter().flatten() {
                        if pin!(s.recv()).poll(task).is_ready() {
                            stop.request();
                        }
                    }
                },
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
        let mut server = Server::new(self.api.clone(), self.identity.clone());
        if let Some(policy) = self.policy {
            server = server.with_live_policy(policy.clone());
        }
        server
            .bind_linux(broker, ingress, native_accept::Configuration::default())
            .await
            .map_err(|e| Error::Listener(Box::new(e)))
    }

    fn driver(source: &Cx, os_session: u32) -> Result<dispatch::Driver, Error> {
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
        let entropy: shared_viewers::Entropy = Arc::new(|| random_nonzero_u128().map_err(|_| ()));
        agent
            .native_incoming(
                source.clone(),
                shared_viewers::Policy::default(),
                Duration::from_millis(50),
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
        let epoch = policy::lease(self.policy)?;
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
        let mut driver = Self::driver(&source, os_session)?;
        let retirement = Arc::new(Mutex::new(None));
        let factory = self.factory(source, retirement.clone());
        let (fps, bitrate) = (self.options.fps, self.options.bitrate);
        let (host_boot, scope) = (self.host_boot, self.options.sharing);
        let report = self.report.clone();
        let stop = self.stop.clone();
        let source_epoch = epoch.clone();
        let end = linux
            .serve_desktop(
                &mut driver,
                supervisor,
                self.runtime.clone(),
                serial::Policy::default(),
                Connections {
                    request: move |attempt| request(attempt, host_boot, os_session, scope),
                    // require_approval is false: no notification is ever delivered.
                    approval: |_, _| Err(()),
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
                move |catalog| choose(catalog, fps, bitrate),
                move |_, _| {
                    Ok(
                        if stop.is_requested()
                            || source_epoch
                                .as_ref()
                                .is_some_and(|lease| lease.check().is_err())
                        {
                            LocalAction::Stop
                        } else {
                            LocalAction::Continue
                        },
                    )
                },
            )
            .await;
        (self.report)(Event::ShareEnded {
            outcome: format!("{end:?}"),
        });
        if let Ok(mut slot) = self.active.lock() {
            *slot = None;
        }
        self.cleanup(&mut driver, &retirement, &mut linux).await?;
        if let Some(epoch) = epoch {
            match epoch.check() {
                Err(crate::host_policy::live::Error::Changed) => return Ok(Ended::PolicyChanged),
                Err(error) => return Err(Error::Policy(error)),
                Ok(_) => {}
            }
        }
        match end {
            End::Listener(Err(error)) => Err(Error::Listener(Box::new(error))),
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
    fn host_offer_matches_the_native_viewer_bootstrap_capabilities() {
        let offer = offer();
        assert_eq!(offer.role, Role::Observe);
        let names: Vec<_> = offer.capabilities.iter().map(|c| c.name.clone()).collect();
        for required in [
            display::CAPABILITY,
            decoder::CAPABILITY,
            attachment::CAPABILITY,
            attachment::DELIVERY_CAPABILITY,
        ] {
            assert!(names.iter().any(|n| n == required), "{required}");
        }
        assert!(offer.capabilities.iter().all(|c| c.required));
    }

    #[test]
    fn every_request_allocates_fresh_unpredictable_identifiers() {
        let boot = HostBootId::from_raw(9);
        let a = request(1, boot, 5, Scope::OwnUser).unwrap();
        let b = request(2, boot, 5, Scope::OwnUser).unwrap();
        assert_ne!(
            a.session.binding.remote_session,
            b.session.binding.remote_session
        );
        assert_ne!(a.connection_id, b.connection_id);
        assert_eq!(a.session.binding.os_session.as_raw(), 5);
        assert!(!a.session.require_approval);
        assert_eq!(a.admission.scope, Scope::OwnUser);
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
        };
        let report: Reporter = Arc::new(|_| {});
        let stop = Arc::new(StopHandle::default());
        assert_eq!(run(&options, &report, &stop), Err(Error::Configuration));
        let mut empty = options;
        empty.worker = PathBuf::from("/usr/bin/fr-media-worker");
        empty.display = String::new();
        assert_eq!(run(&empty, &report, &stop), Err(Error::Configuration));
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
