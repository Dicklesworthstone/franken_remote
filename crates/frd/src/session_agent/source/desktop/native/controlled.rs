//! The exclusive remote-CONTROL share, opt-in per agent (`frd run --input-agent`).
//!
//! Only when the agent carries a `ControlProfile` may a FIRST viewer negotiate
//! `RequestControl`. It then gets the canonical control-capable publication and
//! managed control service on the SAME capture pipeline profile; the dispatch
//! route stays Starting, so every later viewer is refused Busy until this share
//! ends. There is no observer↔controller upgrade or handoff in this slice, and
//! an observer-first share still refuses a later controller (`ControlUnavailable`).
//!
//! Approval is the operator's explicit `approval none` choice for this profile:
//! a request is approved automatically only for locally permitted operations,
//! with input-injection permission, and only once the viewer's own presented-view
//! evidence makes the view ready. `XTest` runs in a per-lease `fr-input-agent`
//! child (never in frd) that shows the mandatory sharing indicator for the whole
//! lease; its fence is installed before any input can queue.
use super::super::{Error, LocalAction, Report, Wake};
use super::{SessionAgent, Setup, Startup, local_stage};
use crate::{
    input_agent::Seat,
    input_process::{self, Fence, InvalidLaunch, ProcessLaunch, RemoteSink},
    input_quic::grant::Error as GrantError,
    input_watchdog::StopReason,
    session_startup::{
        HostSession, ManagedControlReport, ManagedHostControlState, NativePublisher,
        PublisherPolicy,
        shared_viewers::{Entropy, Statistics},
    },
    worker::Deadline,
};
use asupersync::{cx::Cx, types::Time};
use fr_core::{
    ids::{CodecConfigurationGeneration, InputLeaseId, InputTicketId},
    input_submission::Capabilities,
    limits::ProtocolLimits,
};
use fr_media::worker::{Backend, Configuration};
use fr_wire::control::Target;
use std::{
    fmt,
    future::{Future, poll_fn},
    path::{Path, PathBuf},
    pin::pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

/// Local operator choices for control; nothing here is peer-selectable. The
/// Seat must be the ONE Seat of this OS share for the daemon's lifetime: a
/// lease whose cleanup stayed uncertain keeps it occupied, refusing later
/// control until restart rather than guessing that keys were released.
#[derive(Clone)]
pub struct ControlProfile {
    input_agent: PathBuf,
    display: String,
    xauthority: Option<PathBuf>,
    seat: Seat,
    capabilities: Capabilities,
    fps: u16,
    bitrate: u32,
    backend: Backend,
}
impl ControlProfile {
    /// `input_agent` is the locally installed `fr-input-agent` image, `display`
    /// the local `:N[.S]` X11 display also captured, and `capabilities` the
    /// native operations a controller may request (a request for anything else
    /// is denied, never clamped). The codec settings mirror the capture profile.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        input_agent: &Path,
        display: &str,
        xauthority: Option<&Path>,
        seat: Seat,
        capabilities: Capabilities,
        fps: u16,
        bitrate: u32,
        backend: Backend,
    ) -> Result<Self, InvalidLaunch> {
        // Same local-only validation the per-lease launch applies later.
        ProcessLaunch::new(input_agent, display, xauthority, 1)?;
        if fps == 0 || bitrate == 0 {
            return Err(InvalidLaunch);
        }
        Ok(Self {
            input_agent: input_agent.into(),
            display: display.into(),
            xauthority: xauthority.map(Path::to_path_buf),
            seat,
            capabilities,
            fps,
            bitrate,
            backend,
        })
    }
    pub const fn capabilities(&self) -> Capabilities {
        self.capabilities
    }
    pub fn seat(&self) -> Seat {
        self.seat.clone()
    }
    fn configuration(&self, display: fr_wire::display::Display) -> Configuration {
        Configuration {
            width: display.pixel_width,
            height: display.pixel_height,
            fps: self.fps,
            backend: self.backend,
            bitrate: self.bitrate,
            max_access_unit_bytes: ProtocolLimits::ABSOLUTE.max_encoded_access_unit_bytes(),
            generation: CodecConfigurationGeneration::INITIAL,
        }
    }
}
impl fmt::Debug for ControlProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // No paths or display names in diagnostics.
        f.debug_struct("ControlProfile")
            .field("capabilities", &self.capabilities)
            .finish_non_exhaustive()
    }
}

impl SessionAgent {
    /// Opt this agent's OS share into the exclusive controlled share. Without
    /// it the agent stays observation-only and controllers are refused.
    #[must_use]
    pub fn with_control(mut self, profile: ControlProfile) -> Self {
        self.control = Some(profile);
        self
    }
    pub fn control_profile(&self) -> Option<&ControlProfile> {
        self.control.as_ref()
    }
}

/// The controlled share's original publication (capture child, session and
/// input attachment). Keep it through `reap`; closing fences the session.
pub struct ControlledDesktop {
    media: Media,
    profile: ControlProfile,
}
/// Unlike a shared source, this publisher OWNS the viewer's connection: no
/// separate peer service holds it. It is dropped only after its capture child
/// is proven reaped, which also releases that transport's ingress lease (the
/// listener's `stop` refuses while any transport still holds one).
enum Media {
    Live(Box<NativePublisher>),
    Reaped(asupersync::process::ExitStatus),
}
impl fmt::Debug for ControlledDesktop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ControlledDesktop([original controlled share])")
    }
}
impl ControlledDesktop {
    pub fn worker_id(&self) -> Option<u32> {
        match &self.media {
            Media::Live(publisher) => publisher.worker_id(),
            Media::Reaped(_) => None,
        }
    }
    pub fn close(&mut self) {
        if let Media::Live(publisher) = &mut self.media {
            publisher.close();
        }
    }
    /// Fence at call time, then observe the exact capture child. Only a proven
    /// exit releases the connection; a failed reap retains every owner. Input
    /// cleanup is reported by `serve`; its Seat is released by the native owner.
    pub async fn reap(
        &mut self,
        cleanup: &Cx,
        deadline: Deadline,
    ) -> Result<asupersync::process::ExitStatus, crate::worker::Error> {
        let status = match &mut self.media {
            Media::Reaped(status) => return Ok(*status),
            Media::Live(publisher) => publisher.reap_media(cleanup, deadline).await?,
        };
        // Dropping the (already fenced) publisher closes the viewer transport.
        self.media = Media::Reaped(status);
        Ok(status)
    }

    /// Serve the managed control session until it ends. Local events run first
    /// on every turn and at least every 10 ms; `Stop` (or a failing callback)
    /// revokes the session, whose original drain then ends control first.
    /// `Ok` means the share ran and ended; the caller classifies a peer's own
    /// ending. Uncertain input cleanup is a host fault (`InputCleanup`).
    pub(crate) fn serve<'a, L>(
        &'a mut self,
        agent: &'a mut SessionAgent,
        cx: Cx,
        entropy: Entropy,
        mut local: L,
    ) -> impl Future<Output = Result<Report, Error>> + Send + 'a
    where
        L: FnMut(&mut SessionAgent, &mut Context<'_>) -> Result<LocalAction, ()> + Send + 'a,
    {
        let permitted = Arc::new(AtomicBool::new(input_permitted(agent)));
        let mut decider = Decider {
            profile: self.profile.clone(),
            cx: cx.clone(),
            entropy: entropy.clone(),
            permitted: permitted.clone(),
            approved: None,
        };
        let (nonce, ticket) = (entropy.clone(), entropy);
        let seat = self.profile.seat.clone();
        // The managed service takes its attachment at CALL time, as before.
        let prepared = match &mut self.media {
            Media::Live(publisher) => Ok((
                publisher.control(),
                publisher.serve_managed_control(
                    seat,
                    move |state| decider.turn(state),
                    move || nonce(),
                    move || ticket().ok().map(InputTicketId::from_raw),
                ),
            )),
            Media::Reaped(_) => Err(Error::Closed),
        };
        async move {
            let (observation, service) = prepared?;
            let mut service = pin!(service);
            let mut timer = Wake {
                driver: cx.timer_driver().ok_or(Error::Clock)?,
                handle: None,
            };
            let (mut stopped, mut failed) = (false, false);
            let report = poll_fn(|task| {
                if !stopped {
                    let action = local(agent, task);
                    permitted.store(input_permitted(agent), Ordering::Release);
                    if action != Ok(LocalAction::Continue) {
                        // Local priority: revoke first; the service then drains
                        // its input owner and returns its original evidence.
                        stopped = true;
                        failed = action.is_err();
                        observation.revoke();
                    }
                }
                if let Poll::Ready(report) = service.as_mut().poll(task) {
                    return Poll::Ready(report);
                }
                let next = timer.driver.now().as_nanos().saturating_add(10_000_000);
                timer.arm(Time::from_nanos(next), task);
                Poll::Pending
            })
            .await;
            let ManagedControlReport { session, input } = report;
            if input.is_some_and(|shutdown| !shutdown.handoff_safe()) {
                return Err(Error::InputCleanup);
            }
            if failed {
                return Err(Error::LocalEvent);
            }
            match session {
                Ok(()) => Ok(served(false)),
                Err(_) if stopped => Ok(served(false)),
                Err(error) => Err(Error::Publication(error)),
            }
        }
    }
}
/// One admitted viewer; `failed` records a viewer-side ending.
pub(super) fn served(failed: bool) -> Report {
    Report {
        source_renewals: 0,
        viewers: Statistics {
            admitted: 1,
            finished: u64::from(!failed),
            failed: u64::from(failed),
        },
    }
}
fn input_permitted(agent: &SessionAgent) -> bool {
    !agent.is_revoked() && agent.permissions().verify_input_injection().is_ok()
}

/// The local control decision, called on every managed turn.
struct Decider {
    profile: ControlProfile,
    cx: Cx,
    entropy: Entropy,
    permitted: Arc<AtomicBool>,
    approved: Option<Target>,
}
impl Decider {
    fn turn(&mut self, state: ManagedHostControlState<'_>) -> Result<Option<Target>, GrantError> {
        match state {
            ManagedHostControlState::Active { request, control } => {
                if !self.permitted.load(Ordering::Acquire) {
                    // Lock, session change or revoked permission ends input.
                    control.stop(StopReason::Suspended);
                }
                Ok(Some(request.target))
            }
            ManagedHostControlState::Pending(mut pending) => {
                let Some(request) = pending.request() else {
                    // After approval the broker consumes the request while the
                    // native owner starts; keep asserting the approved target.
                    return Ok(self.approved);
                };
                // Only locally permitted operations; never clamp or widen.
                if !self
                    .profile
                    .capabilities
                    .contains_all(request.target.capabilities)
                    || !self.permitted.load(Ordering::Acquire)
                {
                    pending.deny();
                    return Ok(None);
                }
                if pending.native_status().is_none() && pending.view_ready()? {
                    let epoch =
                        (self.entropy)().map_err(|()| GrantError::CredentialsUnavailable)?;
                    let launch = ProcessLaunch::new(
                        &self.profile.input_agent,
                        &self.profile.display,
                        self.profile.xauthority.as_deref(),
                        epoch,
                    )
                    .map_err(|_| GrantError::NativeNotReady)?;
                    let fence = Fence::default();
                    let make = input_process::factory(
                        launch,
                        self.cx.clone(),
                        request.target.bounds,
                        request.target.capabilities,
                        fence.clone(),
                    );
                    let entropy = self.entropy.clone();
                    pending.approve_fenced(
                        request.target,
                        move || {
                            Some((
                                InputLeaseId::from_raw(entropy().ok()?),
                                InputTicketId::from_raw(entropy().ok()?),
                            ))
                        },
                        make,
                        RemoteSink::native_cleanup,
                        &fence,
                    )?;
                    self.approved = Some(request.target);
                }
                Ok(Some(request.target))
            }
        }
    }
}

/// Open the controlled share after the first viewer negotiated control: the
/// complete control profile and input permission are required BEFORE the
/// capture factory runs; publication stays within the original Host deadline.
#[allow(clippy::too_many_arguments)]
pub(super) async fn open<F, P, L>(
    agent: &mut SessionAgent,
    mut session: HostSession,
    startup: &Startup,
    local: &mut L,
    entropy: &Entropy,
    factory: F,
    profile: ControlProfile,
    capture_interval: Duration,
) -> Result<ControlledDesktop, Error>
where
    F: FnOnce() -> P + Send,
    P: Future<Output = Result<Setup, ()>> + Send,
    L: FnMut(&mut SessionAgent, &mut Context<'_>) -> Result<LocalAction, ()> + Send,
{
    session
        .require_controlled_profile()
        .map_err(Error::Startup)?;
    if !input_permitted(agent) {
        return Err(Error::NoInputPermission);
    }
    // The factory is invoked only now, after both checks.
    let setup = local_stage(agent, startup, local, None, entropy, async {
        factory().await.map_err(|()| Error::SourceSetup)
    })
    .await?;
    // The controlled share runs on the session's own observation authority.
    // The factory's independently authorized source is never attached and is
    // dropped unused: revoking it would cancel the OS-share source context it
    // was created on, which the dispatch Driver itself still runs on.
    drop(setup.control);
    let now = startup.check()?;
    let policy = PublisherPolicy {
        timeout: Duration::from_micros(startup.until.saturating_sub(now))
            .min(Duration::from_secs(60)),
        streaming: crate::media::streaming::Policy {
            capture_interval,
            ..crate::media::streaming::Policy::default()
        },
        ..PublisherPolicy::default()
    };
    let random = entropy.clone();
    let codec = profile.clone();
    let publishing = session.publish_controlled_display(
        setup.launch,
        policy,
        move |display| Ok(codec.configuration(display)),
        move || random(),
    );
    let publisher = local_stage(agent, startup, local, None, entropy, async {
        publishing.await.map_err(Error::Publication)
    })
    .await?;
    Ok(ControlledDesktop {
        media: Media::Live(Box::new(publisher)),
        profile,
    })
}

#[cfg(test)]
mod tests;
