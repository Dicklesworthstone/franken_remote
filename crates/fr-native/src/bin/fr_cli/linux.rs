#[cfg(feature = "linux-desktop")]
#[path = "linux/audio.rs"]
mod audio;
#[cfg(feature = "linux-desktop")]
#[path = "linux/control.rs"]
mod control;
#[path = "linux/displays.rs"]
mod displays;
#[path = "linux/doctor.rs"]
mod doctor;
#[cfg(feature = "linux-desktop")]
#[path = "linux/files.rs"]
mod files;
#[cfg(any(test, feature = "linux-desktop"))]
#[path = "linux/lapse.rs"]
mod lapse;
#[path = "linux/refusal.rs"]
mod refusal;
#[path = "linux/robot.rs"]
mod robot;
#[path = "linux/session_control.rs"]
mod session_control;
#[cfg(feature = "linux-desktop")]
use super::options::DisplayChoice;
use super::{
    Failure,
    options::{Command, Connection, DoctorOptions, Options, Target},
    output,
};
use asupersync::{
    cx::Cx,
    runtime::{Runtime, RuntimeBuilder},
    signal::{Signal, SignalKind, signal},
    tls::Certificate,
    types::{Budget, CancelKind},
};
#[cfg(feature = "linux-desktop")]
use fr_native::{
    desktop::{
        Configuration as DesktopConfiguration, Desktop,
        reconnect::{Mode, Session, Ui},
    },
    viewer_window::{Status as WindowStatus, StopReason, WindowControl},
};
#[cfg(feature = "linux-desktop")]
use fr_wire::display::Catalog;
use fr_wire::negotiation::Offer;
#[cfg(feature = "linux-desktop")]
use frd::{
    native_connection::reconnect::{self, CallbackError, Status},
    session_startup::{InteractiveViewerState, Presentation, viewer_events::Layout},
};
use frd::{
    native_connection::{
        AddressFamily, Client, Configuration, LocalApi, PeerSelector, TailnetError,
    },
    session_startup::ObserverPolicy,
};
use std::{
    cell::Cell,
    future::{Future, poll_fn},
    path::Path,
    pin::pin,
    task::Poll,
    time::Duration,
};
#[cfg(feature = "linux-desktop")]
use std::{cell::RefCell, rc::Rc};

fn failure(code: &'static str, next: &'static str) -> Failure {
    Failure::new(code, next, 1)
}
/// Why local input capture ended control, as a stable code. `Local`/`Closed`
/// are consequences of a stop that is reported elsewhere, never a cause.
#[cfg(feature = "linux-desktop")]
fn input_stopped(reason: fr_native::viewer_input::StopReason) -> Option<Failure> {
    use fr_native::viewer_input::StopReason as R;
    let (code, next) = match reason {
        R::Local | R::Closed => return None,
        R::FocusLost => (
            "control_focus_lost",
            "The viewer window was not keyboard-focused (or lost focus), so local input capture stopped and control ended; focus the FrankenRemote window and reconnect. Nothing is replayed.",
        ),
        R::WindowChanged => (
            "control_window_changed",
            "The viewer window was resized, hidden, obscured or unmapped, so control ended; reconnect to acquire control again.",
        ),
        R::InitialHeld => (
            "control_input_held_at_start",
            "A key or mouse button was held when control started; release every key and button, then reconnect.",
        ),
        R::KeymapChanged | R::InputDevicesChanged => (
            "control_input_devices_changed",
            "The local keyboard map or input devices changed during control, so it ended; reconnect to acquire control with the new layout.",
        ),
        R::SyntheticInput => (
            "control_synthetic_input",
            "Synthetic (sent-event) input reached the viewer window, so control ended; only real local input may drive the host.",
        ),
        R::LocalEscape => (
            "control_released_locally",
            "Control was released with the local escape; reconnect to acquire it again.",
        ),
        R::SystemShortcutsUnavailable => (
            "system_shortcuts_unavailable",
            "The requested system-shortcut capture was unavailable, so control ended. It needs granted keyboard control and an available local X11 keyboard grab; check host permissions and release other grabs before reconnecting, or explicitly connect without --capture-system-shortcuts.",
        ),
        R::NativeFailure
        | R::Overflow
        | R::Expired
        | R::Clock
        | R::UnsupportedKey
        | R::Queue
        | R::EscapeUnavailable => (
            "control_input_capture_failed",
            "Local input capture stopped (native failure, overload or an unsupported key), so control ended; reconnect.",
        ),
    };
    Some(failure(code, next))
}
/// Automation retains its own window cause: it deliberately has no native
/// event-capture owner from which `last_input_stop` could recover that reason.
#[cfg(feature = "linux-desktop")]
fn automation_focus_stopped(reason: StopReason) -> Option<Failure> {
    let (code, next) = match reason {
        StopReason::FocusLost => (
            "control_focus_lost",
            "The foreground automation window lost keyboard focus and its original control session ended. Open a new explicit session and keep its viewer focused; never replay an action with an uncertain outcome.",
        ),
        StopReason::FocusUnavailable => (
            "control_focus_unavailable",
            "Fresh keyboard-focus evidence for the foreground automation window was unavailable or timed out. Check the local X11 desktop before opening a new explicit session; previously submitted actions are not replayed or rolled back.",
        ),
        _ => return None,
    };
    Some(failure(code, next))
}
/// Native setup can stop the original viewer before attachment returns its
/// status handle. Retain that exact typed cause through the attachment error.
#[cfg(feature = "linux-desktop")]
fn input_attach_stopped(error: fr_native::desktop::Error) -> Option<Failure> {
    use frd::session_startup::viewer_events::CaptureStartError;
    let fr_native::desktop::Error::InputCapture(CaptureStartError::Native(
        fr_native::viewer_input::Error::Stopped(reason),
    )) = error
    else {
        return None;
    };
    input_stopped(reason)
}
fn tailnet(error: TailnetError) -> Failure {
    match error {
        TailnetError::Cancelled => Failure::new(
            "cancelled",
            "The lookup/session was stopped; no actions were replayed.",
            130,
        ),
        TailnetError::LocalApiUnavailable => failure(
            "tailscale_unavailable",
            "Start installed Tailscale and check the protected LocalAPI socket.",
        ),
        TailnetError::BackendNotRunning => failure(
            "tailscale_not_running",
            "Connect the installed Tailscale client before retrying.",
        ),
        TailnetError::UntrustedLocalApi => failure(
            "untrusted_localapi",
            "Use the root-owned installed tailscaled endpoint; do not substitute a user-owned proxy.",
        ),
        TailnetError::Timeout => failure(
            "tailnet_lookup_timeout",
            "Check the installed Tailscale daemon before retrying.",
        ),
        TailnetError::HandshakeTimeout => failure(
            "host_not_answering",
            "The host did not complete the connection in time: check that frd run is running there (it serves one viewer at a time, so another session may be active) and that tailnet policy allows UDP to its port; fr hosts shows the path.",
        ),
        TailnetError::NativeRouteIo => failure(
            "tailnet_route_unavailable",
            "The local UDP path failed during connection setup. Check the installed Tailscale connection and routing; fr hosts shows the host path.",
        ),
        TailnetError::KeyExpired => failure(
            "tailnet_key_expired",
            "Reauthenticate the affected Tailscale node locally.",
        ),
        TailnetError::PeerCertificateRejected => failure(
            "tls_certificate_rejected",
            "The host's certificate did not verify for the selected tailnet name against the trusted roots (wrong host, expired, or an untrusted issuer; the TLS stack does not say which). Check the host's Tailscale certificate and any --trust-roots; never disable certificate checks.",
        ),
        TailnetError::NativeHandshake
        | TailnetError::CertificateRejected
        | TailnetError::InvalidTrustStore => failure(
            "tls_identity_refused",
            "Verify the locally provisioned CA roots and selected canonical host; never disable certificate checks.",
        ),
        _ => failure(
            "tailnet_identity_refused",
            "Inspect installed Tailscale identity, sharing and policy; rerun fr hosts before reconnecting.",
        ),
    }
}
struct Shutdown {
    interrupt: Signal,
    terminate: Signal,
    hangup: Signal,
}
impl Shutdown {
    fn new() -> Result<Self, Failure> {
        let open = |kind| {
            signal(kind).map_err(|_| {
                failure(
                    "signal_unavailable",
                    "Resolve local signal registration before starting a session.",
                )
            })
        };
        Ok(Self {
            interrupt: open(SignalKind::interrupt())?,
            terminate: open(SignalKind::terminate())?,
            hangup: open(SignalKind::hangup())?,
        })
    }
    async fn run<T>(
        &mut self,
        cx: &Cx,
        stopped: &Cell<bool>,
        operation: impl Future<Output = T>,
    ) -> T {
        await_shutdown(cx, stopped, operation, self.requested()).await
    }
    async fn requested(&mut self) {
        let mut interrupt = pin!(self.interrupt.recv());
        let mut terminate = pin!(self.terminate.recv());
        let mut hangup = pin!(self.hangup.recv());
        poll_fn(|task| {
            if interrupt.as_mut().poll(task).is_ready()
                || terminate.as_mut().poll(task).is_ready()
                || hangup.as_mut().poll(task).is_ready()
            {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
    }
    #[cfg(feature = "linux-desktop")]
    async fn run_connection<T>(
        &mut self,
        cx: &Cx,
        stopped: &Cell<bool>,
        operation: impl Future<Output = T>,
        local: &Cx,
        owner: &mut session_control::Owner,
    ) -> T {
        await_shutdown(
            cx,
            stopped,
            operation,
            local_or_signal(owner.wait_for_stop(local), self.requested()),
        )
        .await
    }
}
/// A listener failure is not an authenticated stop request. Keep ordinary
/// signal handling alive, without polling a failed local service again.
#[cfg(any(test, feature = "linux-desktop"))]
async fn local_or_signal<E>(
    local: impl Future<Output = Result<(), E>>,
    signal: impl Future<Output = ()>,
) {
    let mut local = pin!(local);
    let mut signal = pin!(signal);
    let mut local_failed = false;
    poll_fn(|task| {
        if signal.as_mut().poll(task).is_ready() {
            return Poll::Ready(());
        }
        if !local_failed {
            match local.as_mut().poll(task) {
                Poll::Ready(Ok(())) => return Poll::Ready(()),
                Poll::Ready(Err(_)) => local_failed = true,
                Poll::Pending => {}
            }
        }
        Poll::Pending
    })
    .await;
}
/// Signal readiness cancels the supervisor but NEVER drops the reconnect future:
/// its independent bounded cleanup still runs and its actual result is retained.
async fn await_shutdown<T>(
    cx: &Cx,
    stopped: &Cell<bool>,
    operation: impl Future<Output = T>,
    shutdown: impl Future<Output = ()>,
) -> T {
    let mut shutdown = pin!(shutdown);
    let mut operation = pin!(operation);
    poll_fn(|task| {
        if !stopped.get() && shutdown.as_mut().poll(task).is_ready() {
            stopped.set(true);
            cx.cancel_fast(CancelKind::User);
        }
        operation.as_mut().poll(task)
    })
    .await
}
pub fn run(options: &Options) -> Result<String, Failure> {
    run_mode(options, false)
}
pub(super) fn run_automation(options: &Options) -> Result<String, Failure> {
    run_mode(options, true)
}
fn run_mode(options: &Options, automation: bool) -> Result<String, Failure> {
    if let Command::SavedHosts(saved) = &options.command {
        if let Some(text) = super::saved_hosts::local(saved, options.json)? {
            return Ok(text);
        }
    }
    let mut shutdown = Shutdown::new()?;
    let runtime = RuntimeBuilder::current_thread()
        .enable_platform_reactor(true)
        .build()
        .map_err(|_| {
            failure(
                "runtime_unavailable",
                "Check local process/thread resources.",
            )
        })?;
    let cx = runtime
        .handle()
        .try_request_cx_with_budget(Budget::INFINITE)
        .map_err(|_| failure("runtime_unavailable", "Check local runtime resources."))?;
    let stopped = Cell::new(false);
    // Local session closure still works when installed Tailscale is stopped or
    // its socket is unavailable. It never opens another remote connection.
    match &options.command {
        Command::Disconnect(opts) => {
            return robot::run_disconnect(
                &runtime,
                &cx,
                &mut shutdown,
                &stopped,
                opts,
                options.json,
            );
        }
        Command::Robot(opts) => {
            return robot::run_robot(&runtime, &cx, &mut shutdown, &stopped, opts, options.json);
        }
        _ => {}
    }
    let api = match &options.socket {
        Some(path) => LocalApi::new(path).map_err(tailnet)?,
        None => LocalApi::installed(),
    };
    match &options.command {
        Command::Help => unreachable!("help handled before runtime construction"),
        Command::Hosts => {
            let snapshot = runtime
                .block_on(shutdown.run(&cx, &stopped, api.discover(&cx)))
                .map_err(tailnet)?;
            Ok(output::hosts(&snapshot, options.json))
        }
        Command::SavedHosts(saved) => {
            let super::options::SavedHostAction::Save { node, .. } = &saved.action else {
                unreachable!("local saved-host command handled above");
            };
            let target = runtime
                .block_on(shutdown.run(
                    &cx,
                    &stopped,
                    api.peer_target(&cx, PeerSelector::StableId(node)),
                ))
                .map_err(tailnet)?;
            super::saved_hosts::save(
                saved,
                target.stable_id(),
                target.certificate_name(),
                options.json,
            )
        }
        Command::Displays(target) => displays::run(
            &runtime,
            &cx,
            &mut shutdown,
            &stopped,
            api,
            target,
            options.json,
        ),
        Command::Doctor(doctor_opts) => doctor::run(
            &runtime,
            &cx,
            &mut shutdown,
            &stopped,
            api,
            doctor_opts,
            options.json,
        ),
        Command::Connect(connection) => connect(
            &runtime,
            &cx,
            &mut shutdown,
            &stopped,
            api,
            connection,
            options.json,
            automation,
        ),
        Command::Status => {
            robot::run_status(&runtime, &cx, &mut shutdown, &stopped, &api, options.json)
        }
        Command::Inspect(inspect_opts) => robot::run_inspect(
            &runtime,
            &cx,
            &mut shutdown,
            &stopped,
            &api,
            inspect_opts,
            options.json,
        ),
        Command::Disconnect(_) | Command::Robot(_) => unreachable!("local command handled above"),
    }
}

#[cfg(feature = "linux-desktop")]
#[allow(clippy::too_many_lines, clippy::too_many_arguments)] // One original native session owner.
fn connect(
    runtime: &Runtime,
    cx: &Cx,
    shutdown: &mut Shutdown,
    stopped: &Cell<bool>,
    api: LocalApi,
    connection: &Connection,
    json: bool,
    automation: bool,
) -> Result<String, Failure> {
    let selected = super::saved_hosts::select(&connection.target)?;
    let roots = read_roots(&connection.target.roots)?;
    let mut client = Client::new(api, roots, Duration::from_secs(5)).map_err(|_| {
        failure(
            "invalid_native_configuration",
            "Verify the locally provisioned trust store and native connection policy.",
        )
    })?;
    // Classify every --send path by type before any network I/O.
    let sends = files::select(&connection.upload.paths, connection.upload.file_drop)?;
    let downloads = files::download::select(connection.receive.as_ref())?;
    let configuration = desktop_configuration(cx, connection)?;
    let audio_output = audio::resolve(connection.audio.as_ref())?;
    let state = Rc::new(RefCell::new(Progress {
        control: connection.control.then(|| control::Counters {
            clipboard: connection
                .clipboard
                .then(control::ClipboardCounters::default),
            ..control::Counters::default()
        }),
        audio: audio_output
            .as_ref()
            .map(|(_, request)| Rc::new(RefCell::new(audio::Report::for_request(request)))),
        files: sends,
        downloads,
        ..Progress::default()
    }));
    let clipboard = if connection.clipboard {
        Some(control::Clipboard {
            display: x_display(connection)?,
            cx: cx.clone(),
        })
    } else {
        None
    };
    let mode = if connection.control {
        Mode::ControlCapable(control::policy())
    } else {
        Mode::Observe
    };
    let mut application = Session::new(
        configuration,
        ObserverPolicy::default(),
        mode,
        Interface {
            display: connection.display,
            progress: state.clone(),
            attempt: None,
            clipboard,
            audio: audio_output,
        },
    )
    // A controller's window close runs the closing exchange, as a viewer's does.
    .with_cleanup_runtime(runtime.handle());
    let selector = selected.selector();
    let cfg = Configuration {
        port: selected.port,
        family: if connection.target.ipv6 {
            AddressFamily::Ipv6
        } else {
            AddressFamily::Automatic
        },
        ..Default::default()
    };
    let policy = reconnect::Policy {
        max_attempts: connection.attempts,
        ..Default::default()
    };
    // This local control context outlives cancellation of the connection so
    // its caller can receive the real cleanup result from the same owner.
    let local = runtime
        .handle()
        .try_request_cx_with_budget(Budget::INFINITE)
        .map_err(|_| failure("runtime_unavailable", "Check local runtime resources."))?;
    let mut owner = runtime
        .block_on(session_control::Owner::bind(&local, &selected.node))
        .map_err(robot::local_failure)?;
    if automation {
        state.borrow_mut().automation =
            Some(owner.enable_input(&local).map_err(robot::local_failure)?);
    }
    let result = if connection.control {
        let offer = if connection.receive.is_some() && connection.uploads() {
            fr_client::native::control_offer_with_duplex(
                connection.clipboard,
                connection.audio.is_some(),
            )
        } else if connection.receive.is_some() {
            fr_client::native::control_offer_with_download(
                connection.clipboard,
                connection.audio.is_some(),
            )
        } else if connection.audio.is_some() {
            fr_client::native::control_offer_with_auxiliary(
                connection.clipboard,
                connection.uploads(),
                true,
            )
        } else {
            files::offer(connection.clipboard, connection.uploads())
        };
        let operation = client.run_control_capable(
            cx.clone(),
            runtime.handle(),
            selector,
            cfg,
            offer,
            policy,
            &mut application,
        );
        runtime.block_on(shutdown.run_connection(cx, stopped, operation, &local, &mut owner))
    } else {
        let operation = client.run_observing(
            cx.clone(),
            runtime.handle(),
            selector,
            cfg,
            audio::observation_offer(connection.audio.is_some()),
            policy,
            &mut application,
        );
        runtime.block_on(shutdown.run_connection(cx, stopped, operation, &local, &mut owner))
    };
    state.borrow_mut().media = application.statistics();
    let cleanup = if native_cleanup_confirmed(&application, &result) {
        session_control::Completion::Confirmed
    } else {
        session_control::Completion::Unconfirmed
    };
    // A lost requester cannot undo shutdown or turn confirmed local cleanup
    // into failure. The requester independently reports its missing reply.
    let _ = runtime.block_on(owner.complete(&local, cleanup));
    completed(&application, result, stopped.get(), &state.borrow(), json)
}
/// The existing local X11 session: `--x-display`, else DISPLAY.
#[cfg(feature = "linux-desktop")]
fn x_display(connection: &Connection) -> Result<String, Failure> {
    connection
        .x_display
        .clone()
        .or_else(|| std::env::var("DISPLAY").ok())
        .ok_or_else(|| {
            failure(
                "display_unavailable",
                "Set DISPLAY or --x-display for your existing local X11 session.",
            )
        })
}
/// Local window/worker settings only; no network, peer or credential input.
#[cfg(feature = "linux-desktop")]
fn desktop_configuration(
    cx: &Cx,
    connection: &Connection,
) -> Result<DesktopConfiguration, Failure> {
    let x_display = x_display(connection)?;
    let xauthority = std::env::var_os("XAUTHORITY").map(std::path::PathBuf::from);
    let mut epoch = [0_u8; 16];
    cx.random_bytes(&mut epoch);
    let epoch = u128::from_be_bytes(epoch);
    let configuration = DesktopConfiguration::new(
        &connection.worker, &x_display, xauthority.as_deref(), epoch,
    ).map_err(|_| failure(
        "invalid_worker_configuration",
        "Select a trusted locally installed worker and the matching local graphical-session settings.",
    ))?;
    let configuration = match connection.fit_window {
        Some((width, height)) => configuration
            .with_fitted_window(width, height)
            .map_err(|_| {
                failure(
                    "invalid_window_size",
                    "Choose bounded, even physical-pixel dimensions for --fit.",
                )
            })?,
        None => configuration,
    };
    let configuration = if connection.control && connection.clipboard {
        configuration.with_clipboard_space()
    } else {
        configuration
    };
    let configuration = if connection.capture_system_shortcuts {
        configuration.with_system_shortcut_capture()
    } else {
        configuration
    };
    Ok(if connection.display == DisplayChoice::Choose {
        configuration.with_display_picker()
    } else {
        configuration
    })
}
#[cfg(feature = "linux-desktop")]
fn completed(
    application: &Session<Interface>,
    result: Result<(), reconnect::Failure>,
    stopped: bool,
    progress: &Progress,
    json: bool,
) -> Result<String, Failure> {
    let result = completed_result(application, result, stopped, progress, json);
    // Preserve the original host report even when local native cleanup or the
    // supervisor failed. A failure status must not erase known external effects.
    // Name earlier attempts' ends too: a reconnect's own end can hide them.
    crate::closure::preserve(result, close_outcome(application)).map_err(|mut failure| {
        failure.earlier = progress.ended.before(progress.attempts);
        if let Some(control) = progress.control {
            failure.control_history =
                Some(Box::leak(Box::new(control_history(progress, &control))));
        }
        if let Some(downloads) = &progress.downloads {
            downloads.summary().preserve(&mut failure);
        }
        failure
    })
}
/// One attempt's typed end, for `attempts_ended`: the same host-report and
/// lapse names the final end gets, else the generic code.
#[cfg(feature = "linux-desktop")]
fn end_code(failure: reconnect::Failure) -> &'static str {
    refusal::reconnect(failure)
        .or_else(|| lapse::reconnect(failure))
        .map_or("native_session_failed", |named| named.code)
}
/// The original closing exchange: a viewer's, or a granted controller's
/// session exchange (its separate lease report is not translated into it).
#[cfg(feature = "linux-desktop")]
fn close_outcome(
    application: &Session<Interface>,
) -> Option<frd::session_startup::ViewerCloseOutcome> {
    application.disconnect_outcome().or_else(|| {
        application
            .control_disconnect_outcome()
            .map(|outcome| outcome.exchange)
    })
}
/// The exact terminal evidence that can veto viewing after cleanup; partial
/// acknowledgements without a report do not invent a host revocation.
#[cfg(feature = "linux-desktop")]
fn control_retry_stop(summary: fr_native::desktop::reconnect::CleanupSummary) -> Option<Failure> {
    if let Some(named) = summary.input_stop.and_then(input_stopped) {
        return Some(named);
    }
    if let Some(report) = summary
        .control_disconnect
        .and_then(|outcome| outcome.revocation)
    {
        return Some(crate::terminal::failure(report));
    }
    summary
        .disconnect
        .or_else(|| summary.control_disconnect.map(|outcome| outcome.exchange))
        .and_then(|outcome| outcome.report)
        .map(crate::closure::failure)
}
#[cfg(feature = "linux-desktop")]
fn completed_result(
    application: &Session<Interface>,
    result: Result<(), reconnect::Failure>,
    stopped: bool,
    progress: &Progress,
    json: bool,
) -> Result<String, Failure> {
    // A signal or UI close never conceals unsuccessful native cleanup.
    if !native_cleanup_confirmed(application, &result) {
        return Err(failure(
            "native_cleanup_incomplete",
            "Native cleanup was not confirmed; inspect remaining workers before another connection.",
        ));
    }
    if stopped {
        return Err(Failure::new(
            "cancelled",
            if progress.control.is_some() {
                "Session stopped and the supervisor completed cleanup; no input is replayed or retried."
            } else {
                "Session stopped and the supervisor completed cleanup; no control/input was requested."
            },
            130,
        ));
    }
    if let Some(named) = progress
        .automation_focus_stop
        .and_then(automation_focus_stopped)
    {
        return Err(named);
    }
    if application.last_error()
        == Some(fr_native::desktop::Error::Picker(
            fr_native::display_picker::Error::Cancelled,
        ))
    {
        return Err(Failure::new(
            "display_selection_cancelled",
            "Display choice cancelled and native cleanup completed; no renderer or input was started.",
            130,
        ));
    }
    if progress.missing_display {
        return Err(failure(
            "display_not_offered",
            "Run fr displays to inspect the host; --display only requires exactly one currently offered display.",
        ));
    }
    // Cleanup itself may stop the window with User. Only a pre-cleanup status
    // or the original WM-request latch retained through reap identifies intent.
    if let Err(error) = result
        && !progress.user_closed
    {
        // Every attempt, including a recovered viewing-only connection, may
        // publish terminal evidence during cleanup. Name the current evidence
        // before consulting the separately retained control-request history.
        if error == reconnect::Failure::Notification
            && let Some(named) = progress.cleanup.and_then(control_retry_stop).or_else(|| {
                progress
                    .control
                    .and_then(|control| control.latest_cleanup())
                    .and_then(control_retry_stop)
            })
        {
            return Err(named);
        }
        if let Some(refused) = refusal::reconnect(error).or_else(|| lapse::reconnect(error)) {
            return Err(refused);
        }
        if let reconnect::Failure::Connection(frd::native_connection::Error::Tailnet(e)) = error {
            return Err(tailnet(e));
        }
        // The typed chain is content-free (Copy enums: no addresses, names,
        // text or payloads), so an unnamed end stays diagnosable; stdout keeps
        // the stable code.
        // A local input-capture stop ends control by cancelling the session
        // scope, so the connection itself only reports Cancelled. Name it.
        if let Some(named) = application
            .last_input_stop()
            .and_then(input_stopped)
            .or_else(|| application.last_error().and_then(input_attach_stopped))
        {
            return Err(named);
        }
        // The local desktop error is often the real cause: a failed local
        // component cancels the connection, which then reports Cancelled.
        eprintln!(
            "fr: session ended without a named reason: {error:?}; local: {:?}",
            application.last_error()
        );
        return Err(failure(
            "native_session_failed",
            "Verify host approval, selected display, worker/HEVC support and transport qualification; do not bypass admission checks.",
        ));
    }
    Ok(completion_with_lease(
        progress,
        json,
        close_outcome(application).map(crate::closure::Report::from),
        application
            .control_disconnect_outcome()
            .and_then(|outcome| outcome.revocation),
    ))
}

#[cfg(feature = "linux-desktop")]
fn native_cleanup_confirmed(
    application: &Session<Interface>,
    result: &Result<(), reconnect::Failure>,
) -> bool {
    application.cleanup_failure().is_none()
        && application.desktop().is_none()
        && !matches!(
            result,
            Err(reconnect::Failure::Cleanup | reconnect::Failure::CleanupExpired)
        )
}

#[cfg(not(feature = "linux-desktop"))]
#[allow(clippy::too_many_arguments)]
fn connect(
    _runtime: &Runtime,
    _cx: &Cx,
    _shutdown: &mut Shutdown,
    _stopped: &Cell<bool>,
    _api: LocalApi,
    _connection: &Connection,
    _json: bool,
    _automation: bool,
) -> Result<String, Failure> {
    Err(Failure::new(
        "gui_unavailable",
        "Graphical session presentation requires building with `--features linux-desktop`. Standalone CLI commands (hosts, doctor, displays) are active.",
        2,
    ))
}

/// The native client accepts at most this many roots (`NativeClient::new`).
const MAX_CLIENT_ROOTS: usize = 256;

fn read_roots(path: &Path) -> Result<Vec<Certificate>, Failure> {
    let refused = || {
        failure(
            "invalid_trust_store",
            "Use a locally provisioned regular (non-symlink) PEM CA-root file, at most 1 MiB and 256 certificates; the default is the distribution bundle. Never use roots from the contacted peer.",
        )
    };
    let roots = fr_tailnet::trust::read_certificates(path).map_err(|_| refused())?;
    if roots.len() > MAX_CLIENT_ROOTS {
        return Err(refused());
    }
    Ok(roots)
}
fn offer() -> Offer {
    fr_client::native::observation_offer()
}

#[cfg(feature = "linux-desktop")]
#[derive(Default)]
struct Progress {
    /// Only the explicit foreground automation mode exposes this bounded owner.
    automation: Option<session_control::input::Shared>,
    /// Original native focus cause, independent of cleanup-generated status.
    automation_focus_stop: Option<StopReason>,
    attempts: u8,
    opened: u8,
    presented: u64,
    approval_pending: bool,
    missing_display: bool,
    user_closed: bool,
    window: Option<WindowControl>,
    /// Current-attempt evidence, even when no control request was made.
    cleanup: Option<fr_native::desktop::reconnect::CleanupSummary>,
    /// Some only for `--control`: content-free request/grant/result tallies.
    control: Option<control::Counters>,
    /// Some only for `--audio`: content-free playback outcome.
    audio: Option<Rc<RefCell<audio::Report>>>,
    /// Some only with `--send`: the explicit selection and its outcome.
    files: Option<files::Local>,
    /// Some only with `--receive`: the pinned local directory and actual receipts.
    downloads: Option<files::download::Local>,
    /// The last attempt's content-free viewer counters (set after the run).
    media: Option<frd::session_startup::ViewerStatistics>,
    /// Each failed attempt's typed end, recorded as it is cleaned up.
    ended: crate::output::Earlier,
}
#[cfg(feature = "linux-desktop")]
impl Progress {
    fn begin(&mut self, attempt: u8) {
        self.attempts = attempt;
        self.user_closed = false;
        self.missing_display = false;
        self.approval_pending = false;
        // The previous owner has already been reaped. Its stopped handle must
        // not classify a connection failure before this attempt opens a window.
        self.window = None;
        self.automation_focus_stop = None;
        self.cleanup = None;
        if let Some(control) = &mut self.control {
            control.begin_attempt(attempt);
        }
    }
    fn retain_cleanup(
        &mut self,
        attempt: u8,
        summary: fr_native::desktop::reconnect::CleanupSummary,
    ) {
        // A failed later dial may reap no new desktop and repeat the previous
        // owner's callback. It must not become this attempt's terminal report.
        if attempt != 0 && attempt == self.attempts {
            self.cleanup = Some(summary);
        }
        if let Some(control) = &mut self.control {
            control.retain_cleanup(attempt, summary);
        }
    }
    fn retain_automation_focus_stop(&mut self, reason: Option<StopReason>) {
        if self.automation.is_some() && self.automation_focus_stop.is_none() {
            self.automation_focus_stop = reason.filter(|reason| {
                matches!(reason, StopReason::FocusLost | StopReason::FocusUnavailable)
            });
        }
    }
    /// A WM request can publish after the initial cleanup notice. Only that
    /// retained latch, never a cleanup-generated status, is safe after reaping.
    fn after_cleanup(&mut self, close_requested: bool) {
        self.user_closed |= close_requested;
    }
    fn before_cleanup(&mut self, window: Option<WindowStatus>) -> Result<(), CallbackError> {
        self.user_closed |= matches!(
            window,
            Some(WindowStatus::CloseRequested | WindowStatus::Stopped(StopReason::User))
        );
        if self.user_closed || self.automation_focus_stop.is_some() {
            // Cleaning notification failure stops retries AFTER mandatory
            // cleanup. Neither user close nor lost focus can open a new owner.
            Err(CallbackError)
        } else {
            Ok(())
        }
    }
}
#[cfg(feature = "linux-desktop")]
struct Interface {
    display: DisplayChoice,
    progress: Rc<RefCell<Progress>>,
    /// The current opened `--control` attempt; never carried to the next one.
    attempt: Option<control::Attempt>,
    /// Default-on unless `--no-clipboard`; never reconfigured after a request.
    clipboard: Option<control::Clipboard>,
    /// `--audio`: the resolved local server and request for either role.
    audio: Option<(std::path::PathBuf, super::options::AudioRequest)>,
}
#[cfg(feature = "linux-desktop")]
impl Ui for Interface {
    fn offer(&mut self, _: u8, original: &Offer) -> Result<Offer, CallbackError> {
        let mut offer = original.clone();
        if self
            .progress
            .borrow()
            .control
            .as_ref()
            .is_some_and(|control| control.requested)
        {
            // The original source selections and receive consent were spent.
            // Keep a control-capable session for a NEW local input decision,
            // without advertising file lanes that have no fresh local owner.
            offer.capabilities.retain(|capability| {
                !matches!(
                    capability.name.as_str(),
                    fr_wire::attachment::FILES_CAPABILITY
                        | fr_wire::files::CAPABILITY
                        | fr_wire::files::CHANNEL_SCOPE_CAPABILITY
                        | fr_wire::files::DOWNLOAD_CAPABILITY
                        | fr_wire::files::DUPLEX_CAPABILITY
                )
            });
        }
        Ok(offer)
    }
    fn choose(&mut self, _: u8, catalog: &Catalog) -> Result<Option<u128>, CallbackError> {
        let selected = self.display.select(catalog);
        if selected.is_none() {
            self.progress.borrow_mut().missing_display = true;
            return Err(CallbackError);
        }
        Ok(selected)
    }
    fn approval(
        &mut self,
        _: u8,
        _: fr_client::startup::ApprovalNotice,
    ) -> Result<(), CallbackError> {
        self.progress.borrow_mut().approval_pending = true;
        Ok(())
    }
    fn ready(&mut self, _: u8, desktop: &mut Desktop) -> Result<(), CallbackError> {
        let mut p = self.progress.borrow_mut();
        p.opened = p.opened.saturating_add(1);
        p.approval_pending = false;
        p.window = desktop.window();
        if let Some(automation) = &p.automation {
            p.window
                .as_ref()
                .ok_or(CallbackError)?
                .enable_automation_focus()
                .map_err(|_| CallbackError)?;
            let observation = fr_native::desktop::reconnect::Observation::initial_from(desktop)
                .ok_or(CallbackError)?;
            automation
                .bind_snapshot(
                    desktop.snapshot_handle().map_err(|_| CallbackError)?,
                    observation.display,
                    observation.completion.frame.as_raw(),
                    snapshot_evidence(observation.completion.stage),
                )
                .map_err(|()| CallbackError)?;
        }
        if let Some(counters) = p.control.as_mut() {
            let mode = counters.opened();
            let mut attempt = control::Attempt::new(desktop, mode).ok_or(CallbackError)?;
            if mode == control::AttemptMode::Control {
                if let (Some(local), Some(clipboard)) =
                    (&self.clipboard, counters.clipboard.as_mut())
                {
                    attempt.configure_clipboard(desktop, local, clipboard);
                }
                if let Some(files) = p.files.as_mut() {
                    files.configure(desktop);
                }
                if let Some(downloads) = p.downloads.as_mut() {
                    downloads.configure(desktop);
                }
            }
            self.attempt = Some(attempt);
        }
        if let (Some((server, request)), Some(report)) = (&self.audio, &p.audio) {
            install_audio(desktop, server, request, report);
        }
        Ok(())
    }
    fn interactive(
        &mut self,
        _: u8,
        state: InteractiveViewerState<'_>,
        frame: Option<Presentation>,
    ) -> Result<Option<Layout>, CallbackError> {
        let mut progress = self.progress.borrow_mut();
        if frame.is_some() {
            progress.presented = progress.presented.saturating_add(1);
        }
        let automation = progress.automation.clone();
        if let (Some(automation), Some(frame)) = (&automation, frame) {
            automation
                .presented(frame.frame.as_raw(), snapshot_evidence(frame.stage))
                .map_err(|()| CallbackError)?;
        }
        match (self.attempt.as_mut(), progress.control.as_mut()) {
            (Some(attempt), Some(counters)) => match automation {
                Some(automation) => attempt.automation_turn(state, frame, counters, &automation),
                None => attempt.turn(state, frame, counters),
            },
            _ => Err(CallbackError),
        }
    }
    fn input_result(&mut self, _: u8, result: fr_client::input::ResultEvent) {
        let mut progress = self.progress.borrow_mut();
        if let Some(counters) = progress.control.as_mut() {
            counters.result(&result);
        }
        if let Some(automation) = &progress.automation {
            automation.result(result);
        }
    }
    fn cleanup_finished(
        &mut self,
        attempt: u8,
        summary: fr_native::desktop::reconnect::CleanupSummary,
    ) {
        let mut progress = self.progress.borrow_mut();
        let focus_stop = progress
            .window
            .as_ref()
            .and_then(WindowControl::automation_focus_stop);
        progress.retain_automation_focus_stop(focus_stop);
        let close_requested = progress
            .window
            .as_ref()
            .is_some_and(WindowControl::close_requested);
        progress.after_cleanup(close_requested);
        progress.retain_cleanup(attempt, summary);
    }
    fn frame(&mut self, _: u8, frame: Option<Presentation>) -> Result<(), CallbackError> {
        if frame.is_some() {
            let mut p = self.progress.borrow_mut();
            p.presented = p.presented.saturating_add(1);
        }
        Ok(())
    }
    fn status(&mut self, status: Status) -> Result<(), CallbackError> {
        let mut progress = self.progress.borrow_mut();
        match status {
            Status::Connecting { attempt } => {
                progress.begin(attempt);
                self.attempt = None;
            }
            Status::Cleaning { attempt, failure } => {
                let focus_stop = progress
                    .window
                    .as_ref()
                    .and_then(WindowControl::automation_focus_stop);
                progress.retain_automation_focus_stop(focus_stop);
                if let Some(automation) = &progress.automation {
                    automation.retire();
                }
                if let Some(failure) = failure {
                    progress.ended.push(attempt, end_code(failure));
                }
                // Last content-free clipboard sample before the attempt's handle goes.
                if let (Some(attempt), Some(counters)) = (
                    self.attempt.take(),
                    progress.control.as_mut().and_then(|c| c.clipboard.as_mut()),
                ) {
                    attempt.clipboard_turn(counters);
                }
                // The supervisor cancels the original viewer before this
                // notice. status() may therefore report SessionEnded even
                // when a WM-close request was already published. Preserve that
                // independent local intent before polling current liveness.
                let window = progress.window.as_ref().map(|window| {
                    if window.close_requested() {
                        WindowStatus::CloseRequested
                    } else {
                        window.status()
                    }
                });
                let closed = progress.before_cleanup(window);
                if let Some(counters) = progress.control.as_mut() {
                    counters.cleaning(failure);
                }
                return closed;
            }
            Status::Reconnecting { .. } => {
                if progress.user_closed
                    || progress.automation_focus_stop.is_some()
                    || progress.cleanup.and_then(control_retry_stop).is_some()
                    || progress
                        .control
                        .is_some_and(|control| control.retry_stopped())
                {
                    return Err(CallbackError);
                }
            }
            _ => {}
        }
        Ok(())
    }
}
/// Install this attempt's local output. Absence is typed, never fatal.
#[cfg(feature = "linux-desktop")]
fn install_audio(
    desktop: &mut Desktop,
    server: &Path,
    request: &super::options::AudioRequest,
    report: &Rc<RefCell<audio::Report>>,
) {
    let volume = report.borrow_mut().begin_attempt();
    #[cfg(feature = "linux-audio")]
    {
        let output = audio::output(server.to_path_buf(), request, report, volume.clone());
        match desktop.configure_audio_with_controls(output, volume) {
            Ok(()) => report.borrow_mut().controls = desktop.audio_controls(),
            Err(fr_native::desktop::Error::Audio(
                frd::session_startup::ViewerAudioUnavailable::NotSelected,
            )) => report.borrow_mut().absent("host_did_not_offer"),
            Err(_) => report.borrow_mut().absent("audio_setup_refused"),
        }
    }
    #[cfg(not(feature = "linux-audio"))]
    {
        let _ = (desktop, server, request, volume);
        report.borrow_mut().absent("audio_unavailable_in_build");
    }
}
#[cfg(feature = "linux-desktop")]
#[cfg(test)]
fn completion(progress: &Progress, json: bool, closure: Option<crate::closure::Report>) -> String {
    completion_with_lease(progress, json, closure, None)
}
/// `lease` is a granted controller's own lease report from its closing
/// exchange (control completions only).
#[cfg(feature = "linux-desktop")]
fn completion_with_lease(
    progress: &Progress,
    json: bool,
    closure: Option<crate::closure::Report>,
    lease: Option<fr_wire::lease_revoked::Revoked>,
) -> String {
    if let Some(control) = progress.control {
        return control_completion(progress, &control, json, closure, lease);
    }
    let audio = audio_completion(progress, json);
    if json {
        let closure = closure.map_or_else(|| "null".to_owned(), crate::closure::Report::json);
        format!(
            "{{\"schema_version\":1,\"timestamp_unix_ms\":{},\"outcome\":\"stopped\",\"role\":\"observe\",\"attempts\":{},\"opened\":{},\"subsequent_decoder_completions\":{},\"last_attempt_media\":{}{audio},\"cleanup_confirmed\":true,\"transport_qualified\":false,\"physical_visibility_proven\":false,\"close_exchange\":{closure}}}\n",
            output::timestamp(),
            progress.attempts,
            progress.opened,
            progress.presented,
            media_json(progress.media.as_ref()),
        )
    } else {
        let closure = closure.map_or_else(
            || {
                "No completed close exchange; host cleanup and outstanding effects are unknown."
                    .to_owned()
            },
            crate::closure::Report::text,
        );
        format!(
            "View-only session stopped; {} attempt(s), {} opened session(s), local cleanup confirmed. {closure}{audio} Native transport/hardware remain unqualified.\n",
            progress.attempts, progress.opened
        )
    }
}
/// Loss-recovery and freshness evidence for the LAST attempt: decoded
/// pictures, repair requests admitted to transport, completed recovery
/// handshakes, and the presented source-age upper bound at admitted reports as
/// 10 ms bucket edges (p50/p95). Counts and bounds only; not visibility,
/// delivery or latency proof.
#[cfg(feature = "linux-desktop")]
fn media_json(media: Option<&frd::session_startup::ViewerStatistics>) -> String {
    media.map_or_else(
        || "null".to_owned(),
        |m| {
            let age = |permille| {
                m.presented_age
                    .quantile_upper_us(permille)
                    .map_or_else(|| "null".to_owned(), |us| (us / 1000).to_string())
            };
            format!(
                "{{\"decoded\":{},\"repair_requests\":{},\"recovered_streams\":{},\"presented_age_reports\":{},\"presented_age_p50_ms_at_most\":{},\"presented_age_p95_ms_at_most\":{}}}",
                m.decoded,
                m.repair_requests,
                m.recovered_streams,
                m.presented_age.reports(),
                age(500),
                age(950)
            )
        },
    )
}
/// The controller uses the same content-free audio report as an observer.
/// Configuration alone is not playback, and submitted PCM is not audibility.
#[cfg(feature = "linux-desktop")]
fn audio_completion(progress: &Progress, json: bool) -> String {
    let audio = progress.audio.as_ref().map(|report| report.borrow());
    let (requested, active, played, resets, absence) =
        audio.as_ref().map_or((false, false, 0, 0, None), |report| {
            (
                true,
                report.active(),
                report.submitted,
                report.resets,
                report.absence,
            )
        });
    let (expired, failed, recovered, first_failure) =
        audio.as_ref().map_or((0, 0, 0, None), |report| {
            (
                report.expired_generations,
                report.failed_generations,
                report.recovered_generations,
                report.first_failure,
            )
        });
    let dropped = audio.as_ref().map_or(0, |report| report.dropped_packets);
    let (volume, muted) = audio
        .as_ref()
        .map_or((1.0, false), |report| report.volume.snapshot());
    let volume = volume * 100.0;
    let controls = audio.as_ref().map_or("not_requested", |report| {
        use fr_native::file_progress::{Status, StopReason};
        match report.controls.as_ref() {
            None => "not_started",
            Some(Err(_)) => "unavailable",
            Some(Ok(control)) => match control.status() {
                Status::Opening => "opening",
                Status::Running => "running",
                Status::Stopped(StopReason::Hidden) => "hidden",
                Status::Stopped(StopReason::SessionEnded | StopReason::OwnerStopped) => "closed",
                Status::Stopped(_) => "unavailable",
            },
        }
    });
    if json {
        let absence = absence.map_or_else(|| "null".to_owned(), |a| format!("\"{a}\""));
        let first_failure = first_failure.map_or_else(|| "null".to_owned(), |a| format!("\"{a}\""));
        format!(
            ",\"audio_requested\":{requested},\"audio_active\":{active},\"audio_frames_submitted\":{played},\"audio_output_resets\":{resets},\"audio_output_dropped_packets\":{dropped},\"audio_output_expired_generations\":{expired},\"audio_output_failed_generations\":{failed},\"audio_output_recovered_generations\":{recovered},\"audio_output_first_failure\":{first_failure},\"audio_absence\":{absence},\"audio_volume_percent\":{volume:.0},\"audio_muted\":{muted},\"audio_controls\":\"{controls}\",\"audibility_proven\":false"
        )
    } else {
        let mut text = match (requested, active, absence) {
            (false, ..) => String::new(),
            (true, true, None) => format!(
                " Host audio: {played} decoded frame(s) submitted to the local audio server (not audibility proof)."
            ),
            (true, _, Some(reason)) => format!(" Host audio absent: {reason}."),
            (true, false, None) => " Host audio: requested but no playback confirmed.".to_owned(),
        };
        if expired > 0 || failed > 0 {
            use std::fmt::Write;
            let _ = write!(
                text,
                " Audio output generations: {expired} expired, {failed} failed, {recovered} recovered after fresh submission."
            );
            if let Some(reason) = first_failure {
                let _ = write!(text, " First output failure: {reason}.");
            }
        }
        if dropped > 0 {
            use std::fmt::Write;
            let _ = write!(
                text,
                " Audio output inbox: {dropped} packet(s) dropped at capacity."
            );
        }
        if requested {
            use std::fmt::Write;
            let _ = write!(
                text,
                " Local audio volume: {volume:.0}%, {}. Controls: {controls}.",
                if muted { "muted" } else { "unmuted" }
            );
        }
        text
    }
}
/// Counts are host-reported stages, not local effect proof; visibility stays
/// the X11 submission witness described in `control.rs`, never optical proof.
/// Clipboard fields are content-free: requested, became active, host items
/// committed to the local CLIPBOARD, or the typed reason it never became active.
#[cfg(feature = "linux-desktop")]
fn control_completion(
    progress: &Progress,
    control: &control::Counters,
    json: bool,
    closure: Option<crate::closure::Report>,
    lease: Option<fr_wire::lease_revoked::Revoked>,
) -> String {
    let history = control_history(progress, control);
    let downloads = progress.downloads.as_ref().map_or_else(
        files::download::Summary::not_requested,
        files::download::Local::summary,
    );
    let downloads = if json {
        downloads.json()
    } else {
        downloads.text()
    };
    let audio = audio_completion(progress, json);
    if json {
        format!(
            "{{\"schema_version\":1,\"timestamp_unix_ms\":{},\"outcome\":\"stopped\",\"role\":\"control\",\"attempts\":{},\"opened\":{},\"subsequent_decoder_completions\":{},\"last_attempt_media\":{}{}{downloads}{audio},\"cleanup_confirmed\":true,\"transport_qualified\":false,\"physical_visibility_proven\":false,\"close_exchange\":{},\"lease_report\":{}}}\n",
            output::timestamp(),
            progress.attempts,
            progress.opened,
            progress.presented,
            media_json(progress.media.as_ref()),
            history.json,
            closure.map_or_else(|| "null".to_owned(), crate::closure::Report::json),
            crate::terminal::lease_json(lease),
        )
    } else {
        let close = closure.map_or_else(
            || " No completed final close exchange; host cleanup and outstanding effects are unknown.".to_owned(),
            |report| format!(" Final connection close: {}", report.text()),
        );
        let lease = lease.map_or_else(String::new, |report| {
            format!(
                " Final connection lease report: {}.",
                crate::terminal::lease_text(report)
            )
        });
        format!(
            "Control session stopped; {} attempt(s), {} opened session(s).{} Local cleanup confirmed.{close}{lease}{downloads}{audio} Native transport/hardware remain unqualified.\n",
            progress.attempts, progress.opened, history.text,
        )
    }
}
/// Historical effects are separate from the current attempt's role and closing
/// exchange. Retain the same bounded projection when a later retry fails.
#[cfg(feature = "linux-desktop")]
#[allow(clippy::too_many_lines)] // One bounded history projection keeps both output formats aligned.
fn control_history(progress: &Progress, control: &control::Counters) -> crate::ControlHistory {
    let files = progress
        .files
        .as_ref()
        .map_or_else(files::Summary::not_requested, files::Local::summary);
    let clipboard = control.clipboard.unwrap_or_default();
    let absence = control
        .clipboard
        .and_then(|c| c.absence())
        .unwrap_or("not_requested");
    let ended = control
        .ended
        .map(|result| result.err().map_or("completed", end_code));
    let exchange = control
        .cleanup
        .and_then(|summary| {
            summary
                .disconnect
                .or_else(|| summary.control_disconnect.map(|outcome| outcome.exchange))
        })
        .map(crate::closure::Report::from);
    let lease = control
        .cleanup
        .and_then(|summary| summary.control_disconnect)
        .and_then(|outcome| outcome.revocation);
    let granted = control.capabilities.map_or_else(
        || "null".to_owned(),
        |granted| {
            format!(
                "[{}]",
                control::capability_names(granted)
                    .iter()
                    .map(|name| format!("\"{name}\""))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        },
    );
    let control_text = match (control.requested, control.granted) {
        (_, true) if control.wheel_unavailable() => {
            "granted (the host cannot inject wheel scrolling: wheel unavailable)"
        }
        (_, true) => "granted",
        (true, false) => "requested but not granted",
        (false, false) => "not requested",
    };
    let clipboard_text = if clipboard.active {
        let ended = clipboard
            .ended
            .map_or_else(String::new, |code| format!(", then ended: {code}"));
        format!(
            "active ({} host item(s) received{ended})",
            clipboard.received
        )
    } else {
        format!("absent: {absence}")
    };
    let viewing = if control.viewing_reconnects == 0 {
        String::new()
    } else {
        format!(
            " Viewing reconnected {} time(s), each initially without control; final attempt mode {}.",
            control.viewing_reconnects,
            control.mode.name()
        )
    };
    let fresh = control.fresh_history();
    let mut history = crate::ControlHistory {
        json: format!(
            ",\"last_attempt_mode\":\"{}\",\"viewing_reconnects\":{},\"control_request_attempt\":{},\"control_attempt_end\":{},\"control_requested\":{},\"control_granted\":{},\"control_capabilities_granted\":{granted},\"wheel_unavailable\":{},\"input_results\":{},\"input_submitted_to_os\":{},\"input_suspensions\":{},\"input_suspended_ms\":{},\"input_dropped_aged\":{},\"clipboard_requested\":{},\"clipboard_active\":{},\"clipboard_received\":{},\"clipboard_absence\":{},\"clipboard_ended\":{},\"control_close_exchange\":{},\"control_lease_report\":{}{}",
            control.mode.name(),
            control.viewing_reconnects,
            control
                .request_attempt
                .map_or_else(|| "null".to_owned(), |attempt| attempt.to_string()),
            ended.map_or_else(|| "null".to_owned(), output::quoted),
            control.requested,
            control.granted,
            control.wheel_unavailable(),
            control.results,
            control.submitted,
            control.suspensions,
            control.suspended_us / 1_000,
            control.dropped_aged,
            control.clipboard.is_some(),
            clipboard.active,
            clipboard.received,
            if clipboard.active {
                "null".to_owned()
            } else {
                output::quoted(absence)
            },
            clipboard
                .ended
                .map_or_else(|| "null".to_owned(), output::quoted),
            exchange.map_or_else(|| "null".to_owned(), crate::closure::Report::json),
            crate::terminal::lease_json(lease),
            files.json(),
        ),
        text: format!(
            " Original control {control_text}; {} host input result(s) ({} submitted to the host OS); clipboard {clipboard_text}.{}{}{}{}{}",
            control.results,
            control.submitted,
            ended.map_or_else(String::new, |code| format!(
                " Control attempt ended: {code}."
            )),
            viewing,
            exchange.map_or_else(String::new, |report| format!(
                " Original control close: {}",
                report.text()
            )),
            lease.map_or_else(String::new, |report| format!(
                " Original control lease report: {}.",
                crate::terminal::lease_text(report)
            )),
            files.text(),
        ),
    };
    history.json.push_str(&fresh.json);
    history.text.push_str(&fresh.text);
    history
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_offer_contains_no_control_clipboard_or_audio_capability() {
        let offer = offer();
        assert!(offer.validate().is_ok());
        assert_eq!(offer.role, fr_wire::negotiation::Role::Observe);
        assert_eq!(offer, fr_client::native::observation_offer());
        assert_eq!(offer.capabilities.len(), 8);
        assert!(offer.capabilities.iter().any(|c| {
            c.name == fr_wire::attachment::credit::CAPABILITY
                && c.version == fr_wire::attachment::credit::VERSION
                && !c.required
        }));
        assert!(offer.capabilities.iter().all(|c| !c.name.contains("input")
            && !c.name.contains("clipboard")
            && !c.name.contains("audio")));
    }
    #[test]
    fn local_udp_route_failure_reports_network_guidance() {
        let route = tailnet(TailnetError::NativeRouteIo);
        assert_eq!(route.code, "tailnet_route_unavailable");
        assert_eq!(route.exit, 1);
        assert!(route.next.contains("Tailscale connection and routing"));
    }
    #[test]
    fn a_rejected_host_certificate_is_named_apart_from_other_tls_failures() {
        let rejected = tailnet(TailnetError::PeerCertificateRejected);
        assert_eq!(rejected.code, "tls_certificate_rejected");
        assert_eq!(rejected.exit, 1);
        assert!(rejected.next.contains("never disable certificate checks"));
        for other in [
            TailnetError::NativeHandshake,
            TailnetError::CertificateRejected,
            TailnetError::InvalidTrustStore,
        ] {
            assert_eq!(tailnet(other).code, "tls_identity_refused");
        }
    }
    #[test]
    fn signal_cancels_without_abandoning_the_original_cleanup_future() {
        let runtime = RuntimeBuilder::current_thread()
            .enable_platform_reactor(true)
            .build()
            .unwrap();
        let cx = runtime.request_cx_with_budget(Budget::INFINITE);
        let stopped = Cell::new(false);
        let collected = Cell::new(false);
        let result = runtime.block_on(await_shutdown(
            &cx,
            &stopped,
            async {
                assert!(cx.is_cancel_requested());
                asupersync::time::sleep_until(
                    cx.timer_driver()
                        .unwrap()
                        .now()
                        .saturating_add_nanos(1_000_000),
                )
                .await;
                collected.set(true);
                42
            },
            async {},
        ));
        assert_eq!(result, 42);
        assert!(stopped.get() && collected.get());
    }

    #[test]
    fn a_failed_local_control_service_does_not_forge_a_stop_or_block_signals() {
        let polls = Cell::new(0);
        let signalled = Cell::new(false);
        let local = poll_fn(|_| {
            polls.set(polls.get() + 1);
            assert_eq!(polls.get(), 1, "a failed listener is never repolled");
            Poll::Ready(Err(()))
        });
        let signal = poll_fn(|_| {
            if signalled.get() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        });
        let mut join = pin!(local_or_signal(local, signal));
        let mut task = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(join.as_mut().poll(&mut task).is_pending());
        assert!(join.as_mut().poll(&mut task).is_pending());
        signalled.set(true);
        assert!(join.as_mut().poll(&mut task).is_ready());
        assert_eq!(polls.get(), 1);
    }

    #[test]
    fn an_authenticated_local_stop_waits_for_the_original_cleanup() {
        let runtime = RuntimeBuilder::current_thread()
            .enable_platform_reactor(true)
            .build()
            .unwrap();
        let cx = runtime.request_cx_with_budget(Budget::INFINITE);
        let stopped = Cell::new(false);
        let cleaned = Cell::new(false);
        let result = runtime.block_on(await_shutdown(
            &cx,
            &stopped,
            async {
                assert!(cx.is_cancel_requested());
                asupersync::time::sleep_until(
                    cx.timer_driver()
                        .unwrap()
                        .now()
                        .saturating_add_nanos(1_000_000),
                )
                .await;
                cleaned.set(true);
                19
            },
            local_or_signal(std::future::ready(Ok::<(), ()>(())), std::future::pending()),
        ));
        assert_eq!(result, 19);
        assert!(stopped.get() && cleaned.get());
    }

    #[cfg(feature = "linux-desktop")]
    mod desktop_tests {
        use super::*;

        #[test]
        fn refused_shortcut_capture_keeps_its_specific_local_cause() {
            use fr_native::viewer_input::StopReason as R;
            let refused = input_stopped(R::SystemShortcutsUnavailable).unwrap();
            assert_eq!(refused.code, "system_shortcuts_unavailable");
            assert_eq!(refused.exit, 1);
            assert!(refused.next.contains("control ended"));
            assert_eq!(
                input_stopped(R::FocusLost).unwrap().code,
                "control_focus_lost"
            );
            assert_eq!(
                input_stopped(R::NativeFailure).unwrap().code,
                "control_input_capture_failed"
            );
            assert!(input_stopped(R::Local).is_none());
        }

        #[test]
        fn early_native_attachment_stop_retains_only_its_exact_local_cause() {
            use fr_native::{
                desktop::Error as DesktopError,
                viewer_input::{Error as NativeError, StopReason as R},
            };
            use frd::session_startup::{ControlledViewerError, viewer_events::CaptureStartError};
            let attach = |reason| {
                DesktopError::InputCapture(CaptureStartError::Native(NativeError::Stopped(reason)))
            };
            let refused = input_attach_stopped(attach(R::SystemShortcutsUnavailable)).unwrap();
            assert_eq!(refused.code, "system_shortcuts_unavailable");
            assert_eq!(refused.exit, 1);
            assert_eq!(
                input_attach_stopped(attach(R::LocalEscape)).unwrap().code,
                "control_released_locally"
            );
            assert_eq!(
                input_attach_stopped(attach(R::FocusLost)).unwrap().code,
                "control_focus_lost"
            );
            for unrelated in [
                attach(R::Local),
                attach(R::Closed),
                DesktopError::InputCapture(CaptureStartError::Native(NativeError::InvalidDisplay)),
                DesktopError::InputCapture(CaptureStartError::Viewer(
                    ControlledViewerError::Closed,
                )),
                DesktopError::NotViewing,
            ] {
                assert!(input_attach_stopped(unrelated).is_none());
            }
        }

        #[test]
        fn explicit_display_selection_never_falls_back_to_another_display() {
            let mut ui = Interface {
                display: DisplayChoice::Handle(99),
                progress: Rc::new(RefCell::new(Progress::default())),
                attempt: None,
                clipboard: None,
                audio: None,
            };
            // Real wire catalog constructor also validates the selected display.
            let display = fr_wire::display::Display {
                handle: 9,
                geometry: fr_core::ids::DisplayGeometryGeneration::INITIAL,
                x: 0,
                y: 0,
                pixel_width: 320,
                pixel_height: 240,
                logical_width: 320,
                logical_height: 240,
                scale_numerator: 1,
                scale_denominator: 1,
                rotation: 0,
            };
            let catalog =
                Catalog::new(1, &[display], &fr_core::limits::ProtocolLimits::ABSOLUTE).unwrap();
            assert_eq!(ui.choose(1, &catalog), Err(CallbackError));
            assert!(ui.progress.borrow().missing_display);
        }
        fn unused_session() -> Session<Interface> {
            Session::new(
                DesktopConfiguration::new(Path::new("/usr/bin/false"), ":0", None, 1).unwrap(),
                ObserverPolicy::default(),
                Mode::Observe,
                Interface {
                    display: DisplayChoice::Handle(9),
                    progress: Rc::new(RefCell::new(Progress::default())),
                    attempt: None,
                    clipboard: None,
                    audio: None,
                },
            )
        }
        #[test]
        fn automation_focus_causes_survive_cleanup_status_and_keep_control_evidence() {
            for (reason, code) in [
                (StopReason::FocusLost, "control_focus_lost"),
                (StopReason::FocusUnavailable, "control_focus_unavailable"),
            ] {
                let mut progress = Progress {
                    automation: Some(
                        session_control::input::Shared::new([1; 16], [2; 16]).unwrap(),
                    ),
                    control: Some(control::Counters::default()),
                    ..Progress::default()
                };
                progress.begin(1);
                progress.control.as_mut().unwrap().spend_request();
                // The reporting latch precedes cancellation. A status reader
                // may already see SessionEnded when the supervisor wakes.
                progress.retain_automation_focus_stop(Some(reason));
                assert_eq!(
                    progress.before_cleanup(Some(WindowStatus::Stopped(StopReason::SessionEnded))),
                    Err(CallbackError)
                );
                progress.retain_automation_focus_stop(Some(StopReason::NativeFailure));
                progress.after_cleanup(true);
                assert_eq!(progress.automation_focus_stop, Some(reason));
                let failure = completed(
                    &unused_session(),
                    Err(reconnect::Failure::Notification),
                    false,
                    &progress,
                    true,
                )
                .unwrap_err();
                assert_eq!((failure.code, failure.exit), (code, 1));
                assert!(failure.control_history.is_some());
                assert!(!failure.unknown_effect);
                let output: serde_json::Value =
                    serde_json::from_str(&crate::terminal::output(failure, true)).unwrap();
                assert_eq!(output["error"]["code"], code);
                assert_eq!(output["control_requested"], true);
                assert_eq!(output["control_request_attempt"], 1);
                // A focus diagnosis cannot imply native or host cleanup.
                assert_eq!(
                    completed(
                        &unused_session(),
                        Err(reconnect::Failure::CleanupExpired),
                        false,
                        &progress,
                        true,
                    )
                    .unwrap_err()
                    .code,
                    "native_cleanup_incomplete"
                );
            }
        }
        #[test]
        fn focus_reporting_is_opt_in_original_attempt_only_and_first_cause_wins() {
            let mut progress = Progress::default();
            progress.retain_automation_focus_stop(Some(StopReason::FocusLost));
            assert!(progress.automation_focus_stop.is_none());
            progress.automation =
                Some(session_control::input::Shared::new([1; 16], [2; 16]).unwrap());
            progress.begin(1);
            for reason in [
                StopReason::SessionEnded,
                StopReason::User,
                StopReason::NativeFailure,
            ] {
                assert!(automation_focus_stopped(reason).is_none());
                progress.retain_automation_focus_stop(Some(reason));
                assert!(progress.automation_focus_stop.is_none());
            }
            progress.retain_automation_focus_stop(Some(StopReason::FocusUnavailable));
            progress.retain_automation_focus_stop(Some(StopReason::FocusLost));
            assert_eq!(
                progress.automation_focus_stop,
                Some(StopReason::FocusUnavailable)
            );
            progress.begin(2);
            assert!(progress.automation_focus_stop.is_none());
            assert_eq!(progress.before_cleanup(None), Ok(()));
        }
        #[test]
        fn cleanup_stopping_a_window_is_not_evidence_of_a_user_close() {
            let mut progress = Progress::default();
            progress.begin(1);
            for before in [
                None,
                Some(WindowStatus::Mapped),
                Some(WindowStatus::Stopped(StopReason::SessionEnded)),
                Some(WindowStatus::Stopped(StopReason::NativeFailure)),
            ] {
                assert_eq!(progress.before_cleanup(before), Ok(()));
                assert!(!progress.user_closed);
                // Programmatic cleanup may now publish Stopped(User). It is never
                // sampled here: the failure must retain its original disposition.
                assert_eq!(
                    completed(
                        &unused_session(),
                        Err(reconnect::Failure::Connection(
                            frd::native_connection::Error::Tailnet(
                                TailnetError::LocalApiUnavailable
                            )
                        )),
                        false,
                        &progress,
                        true
                    )
                    .unwrap_err()
                    .code,
                    "tailscale_unavailable"
                );
            }
        }
        #[test]
        fn recorded_user_close_requests_a_terminal_notice_and_keeps_its_disposition() {
            let mut progress = Progress::default();
            progress.begin(1);
            assert_eq!(
                progress.before_cleanup(Some(WindowStatus::Stopped(StopReason::User))),
                Err(CallbackError)
            );
            assert!(progress.user_closed);
            assert!(
                completed(
                    &unused_session(),
                    Err(reconnect::Failure::Notification),
                    false,
                    &progress,
                    true
                )
                .unwrap()
                .contains("\"outcome\":\"stopped\"")
            );
        }
        #[test]
        fn a_new_attempt_cannot_inherit_the_old_windows_close_intent() {
            let mut progress = Progress::default();
            progress.begin(1);
            let _ = progress.before_cleanup(Some(WindowStatus::Stopped(StopReason::User)));
            progress.begin(2);
            assert!(!progress.user_closed && progress.window.is_none());
            assert_eq!(progress.before_cleanup(None), Ok(()));
            assert!(
                completed(
                    &unused_session(),
                    Err(reconnect::Failure::Observation(
                        frd::session_startup::ObserverError::Order
                    )),
                    false,
                    &progress,
                    true
                )
                .is_err()
            );
        }
        #[test]
        fn desktop_completion_retains_host_refusal_but_does_not_override_local_stop() {
            let refusal =
                reconnect::Failure::Observation(frd::session_startup::ObserverError::Session(
                    frd::session_startup::Error::ClientStartup(
                        fr_client::startup::Error::Protocol(fr_wire::negotiation::Error::Refused(
                            fr_wire::refusal::Refused::connection(
                                fr_wire::refusal::Reason::LocalApprovalDenied,
                            ),
                        )),
                    ),
                ));
            let progress = Progress::default();
            let error =
                completed(&unused_session(), Err(refusal), false, &progress, true).unwrap_err();
            assert_eq!(error.code, "host_local_approval_denied");
            assert!(output::failure(error, true).contains("\"outcome\":\"refused\""));
            let stopped =
                completed(&unused_session(), Err(refusal), true, &progress, true).unwrap_err();
            assert_eq!(stopped.code, "cancelled");
            assert_eq!(stopped.exit, 130);
        }

        #[test]
        fn the_control_completion_reports_how_long_input_was_suspended() {
            let mut ended = Progress {
                control: Some(control::Counters::default()),
                ..Progress::default()
            };
            let counters = ended.control.as_mut().unwrap();
            counters.requested = true;
            counters.granted = true;
            counters.suspensions = 2;
            counters.suspended_us = 1_499_999;
            counters.dropped_aged = 3;
            counters.ended = Some(Ok(()));
            let json = completion(&ended, true, None);
            assert!(json.contains("\"input_suspensions\":2"), "{json}");
            assert!(json.contains("\"input_suspended_ms\":1499"), "{json}");
            assert!(json.contains("\"input_dropped_aged\":3"), "{json}");
            assert!(serde_json::from_str::<serde_json::Value>(&json).is_ok());
        }

        #[test]
        fn transient_control_loss_reconnects_viewing_and_preserves_the_original_outcome() {
            let control = || Progress {
                control: Some(control::Counters::default()),
                ..Progress::default()
            };
            let mut ui = Interface {
                display: DisplayChoice::Only,
                progress: Rc::new(RefCell::new(control())),
                attempt: None,
                clipboard: None,
                audio: None,
            };
            ui.status(Status::Connecting { attempt: 1 }).unwrap();
            // No request yet: the ordinary reconnect policy still applies.
            assert_eq!(
                ui.status(Status::Cleaning {
                    attempt: 1,
                    failure: Some(reconnect::Failure::Cancelled)
                }),
                Ok(())
            );
            ui.progress
                .borrow_mut()
                .control
                .as_mut()
                .unwrap()
                .spend_request();
            let refusal = reconnect::Failure::Connection(frd::native_connection::Error::Tailnet(
                TailnetError::LocalApiUnavailable,
            ));
            assert_eq!(
                ui.status(Status::Cleaning {
                    attempt: 1,
                    failure: Some(refusal)
                }),
                Ok(())
            );
            assert!(reconnect::retryable(refusal));
            ui.status(Status::Reconnecting {
                attempt: 2,
                delay: Duration::from_millis(250),
                failure: refusal,
            })
            .unwrap();
            ui.status(Status::Connecting { attempt: 2 }).unwrap();
            {
                let mut progress = ui.progress.borrow_mut();
                let counters = progress.control.as_mut().unwrap();
                assert_eq!(counters.opened(), control::AttemptMode::ViewingOnly);
                assert_eq!(counters.ended, Some(Err(refusal)));
            }
            ui.status(Status::Cleaning {
                attempt: 2,
                failure: None,
            })
            .unwrap();
            let progress = ui.progress.borrow();
            // A later callback failure is not relabeled as the original link
            // error; historical control evidence is retained separately.
            assert_eq!(
                completed(
                    &unused_session(),
                    Err(reconnect::Failure::Notification),
                    false,
                    &progress,
                    true
                )
                .unwrap_err()
                .code,
                "native_session_failed"
            );
            let recovered = completed(&unused_session(), Ok(()), false, &progress, true).unwrap();
            let recovered: serde_json::Value = serde_json::from_str(&recovered).unwrap();
            assert_eq!(recovered["last_attempt_mode"], "view_only");
            assert_eq!(recovered["viewing_reconnects"], 1);
            assert_eq!(recovered["control_request_attempt"], 1);
            assert_eq!(recovered["control_attempt_end"], "native_session_failed");
            let mut ended = control();
            let counters = ended.control.as_mut().unwrap();
            counters.requested = true;
            counters.granted = true;
            counters.capabilities = Some(control::capabilities());
            counters.results = 3;
            counters.submitted = 2;
            counters.ended = Some(Ok(()));
            let done = completed(&unused_session(), Ok(()), false, &ended, true).unwrap();
            for field in [
                "\"role\":\"control\"",
                "\"control_requested\":true",
                "\"control_granted\":true",
                "\"control_capabilities_granted\":[\"keys\",\"repeat\",\"absolute_pointer\",\"buttons\",\"line_scroll\"]",
                "\"wheel_unavailable\":false",
                "\"input_results\":3",
                "\"input_submitted_to_os\":2",
                "\"physical_visibility_proven\":false",
            ] {
                assert!(done.contains(field), "{field}");
            }
            // The notice alone, without a recorded control end, stays a failure.
            assert!(
                completed(
                    &unused_session(),
                    Err(reconnect::Failure::Notification),
                    false,
                    &control(),
                    true
                )
                .is_err()
            );
            let text = completion(&ended, false, None);
            assert!(text.starts_with("Control session stopped;") && text.contains("granted"));
            assert!(!text.contains("wheel unavailable"), "{text}");
            // A host whose executor cannot scroll by lines: granted, no wheel.
            let counters = ended.control.as_mut().unwrap();
            counters.capabilities =
                Some(control::capabilities().meet(fr_wire::control::requestable(&[])));
            let json = completion(&ended, true, None);
            assert!(json.contains("\"wheel_unavailable\":true"), "{json}");
            assert!(!json.contains("line_scroll"), "{json}");
            assert!(serde_json::from_str::<serde_json::Value>(&json).is_ok());
            assert!(completion(&ended, false, None).contains("wheel unavailable"));
            // Not granted: no set, and no wheel claim either way.
            let counters = ended.control.as_mut().unwrap();
            counters.granted = false;
            counters.capabilities = None;
            let json = completion(&ended, true, None);
            assert!(
                json.contains("\"control_capabilities_granted\":null"),
                "{json}"
            );
            assert!(json.contains("\"wheel_unavailable\":false"), "{json}");
        }

        #[test]
        fn completions_report_the_last_attempts_recovery_counts_or_null() {
            let media = frd::session_startup::ViewerStatistics {
                decoded: 41,
                repair_requests: 3,
                recovered_streams: 1,
                ..Default::default()
            };
            let expected = "\"last_attempt_media\":{\"decoded\":41,\"repair_requests\":3,\"recovered_streams\":1,\"presented_age_reports\":0,\"presented_age_p50_ms_at_most\":null,\"presented_age_p95_ms_at_most\":null}";
            for progress in [
                Progress {
                    media: Some(media),
                    ..Progress::default()
                },
                Progress {
                    media: Some(media),
                    control: Some(control::Counters::default()),
                    ..Progress::default()
                },
            ] {
                let json = completion(&progress, true, None);
                assert!(json.contains(expected), "{json}");
                assert!(serde_json::from_str::<serde_json::Value>(&json).is_ok());
            }
            let none = completion(&Progress::default(), true, None);
            assert!(none.contains("\"last_attempt_media\":null"), "{none}");
        }

        #[test]
        fn control_completion_reports_the_clipboard_by_type_and_count_only() {
            let with = |clipboard| Progress {
                control: Some(control::Counters {
                    requested: true,
                    granted: true,
                    clipboard,
                    ..control::Counters::default()
                }),
                ..Progress::default()
            };
            let cases = [
                (
                    with(None),
                    [
                        "\"clipboard_requested\":false",
                        "\"clipboard_active\":false",
                        "\"clipboard_received\":0",
                        "\"clipboard_absence\":\"not_requested\"",
                    ],
                ),
                (
                    with(Some(control::ClipboardCounters {
                        absence: Some("host_clipboard_unavailable"),
                        ..control::ClipboardCounters::default()
                    })),
                    [
                        "\"clipboard_requested\":true",
                        "\"clipboard_active\":false",
                        "\"clipboard_received\":0",
                        "\"clipboard_absence\":\"host_clipboard_unavailable\"",
                    ],
                ),
                (
                    with(Some(control::ClipboardCounters {
                        active: true,
                        received: 2,
                        ..control::ClipboardCounters::default()
                    })),
                    [
                        "\"clipboard_requested\":true",
                        "\"clipboard_active\":true",
                        "\"clipboard_received\":2",
                        "\"clipboard_absence\":null,\"clipboard_ended\":null",
                    ],
                ),
                (
                    with(Some(control::ClipboardCounters {
                        active: true,
                        received: 1,
                        ended: Some("clipboard_item_expired"),
                        ..control::ClipboardCounters::default()
                    })),
                    [
                        "\"clipboard_requested\":true",
                        "\"clipboard_active\":true",
                        "\"clipboard_received\":1",
                        "\"clipboard_absence\":null,\"clipboard_ended\":\"clipboard_item_expired\"",
                    ],
                ),
            ];
            for (progress, fields) in cases {
                let json = completion(&progress, true, None);
                serde_json::from_str::<serde_json::Value>(json.trim()).unwrap();
                for field in fields {
                    assert!(json.contains(field), "{field}: {json}");
                }
            }
            let text = completion(
                &with(Some(control::ClipboardCounters {
                    active: true,
                    received: 2,
                    ..control::ClipboardCounters::default()
                })),
                false,
                None,
            );
            assert!(
                text.contains("clipboard active (2 host item(s) received)"),
                "{text}"
            );
            let text = completion(
                &with(Some(control::ClipboardCounters {
                    active: true,
                    received: 2,
                    ended: Some("clipboard_item_expired"),
                    ..control::ClipboardCounters::default()
                })),
                false,
                None,
            );
            assert!(
                text.contains(
                    "clipboard active (2 host item(s) received, then ended: clipboard_item_expired)"
                ),
                "{text}"
            );
        }
        #[test]
        fn observation_completion_keeps_host_report_separate_from_local_cleanup() {
            use fr_wire::closure::{Cleanup, Closed, ClosedReason, OutstandingEffects};
            let report = crate::closure::Report::from(frd::session_startup::ViewerCloseOutcome {
                request_acknowledged: true,
                report: Some(Closed {
                    reason: ClosedReason::ClientRequested,
                    cleanup: Cleanup::Unconfirmed,
                    effects: OutstandingEffects::Unknown,
                }),
                transport: Ok(()),
            });
            let progress = Progress::default();
            let json: serde_json::Value =
                serde_json::from_str(&completion(&progress, true, Some(report))).unwrap();
            assert_eq!(json["role"], "observe");
            assert_eq!(json["cleanup_confirmed"], true); // Local cleanup only.
            assert_eq!(
                json["close_exchange"]["host_report"]["cleanup_stage"],
                "unconfirmed"
            );
            assert!(
                json["close_exchange"]["host_report"]["outstanding_effects"]["pending"].is_null()
            );
            let text = completion(&progress, false, Some(report));
            assert!(text.contains("local cleanup confirmed"));
            assert!(text.contains("Host-reported reason=client_requested, cleanup=unconfirmed"));
            let absent: serde_json::Value =
                serde_json::from_str(&completion(&progress, true, None)).unwrap();
            assert!(absent.get("close_exchange").unwrap().is_null());
            assert!(completion(&progress, false, None).contains("No completed close exchange"));
        }
        /// A controller's completion carries its OWN closing exchange and
        /// lease report; without them both are null, never omitted or guessed.
        #[test]
        fn a_control_completion_reports_its_own_close_exchange_and_lease_report() {
            let progress = Progress {
                control: Some(control::Counters::default()),
                ..Progress::default()
            };
            let json: serde_json::Value =
                serde_json::from_str(&completion(&progress, true, None)).unwrap();
            assert_eq!(json["role"], "control");
            assert!(json.get("close_exchange").unwrap().is_null());
            assert!(json.get("lease_report").unwrap().is_null());
            let lease = fr_wire::lease_revoked::Revoked {
                lease: fr_core::ids::InputLeaseId::from_raw(7),
                reason: fr_wire::lease_revoked::Reason::SessionEnded,
                cleanup: fr_wire::lease_revoked::CleanupStage::Released,
                effects: fr_wire::lease_revoked::EffectStage::ReceiptsComplete,
            };
            let json: serde_json::Value =
                serde_json::from_str(&completion_with_lease(&progress, true, None, Some(lease)))
                    .unwrap();
            assert_eq!(
                json["lease_report"],
                serde_json::json!({
                    "reason": "session_ended",
                    "cleanup_stage": "released",
                    "effect_stage": "receipts_complete"
                })
            );
            // Stages only: no lease ID or other identifier reaches the output.
            assert_eq!(json["lease_report"].as_object().unwrap().len(), 3);
        }
        #[test]
        fn supervisor_cleanup_failure_cannot_be_masked_by_a_user_close_or_signal() {
            let mut progress = Progress::default();
            let _ = progress.before_cleanup(Some(WindowStatus::Stopped(StopReason::User)));
            for error in [
                reconnect::Failure::Cleanup,
                reconnect::Failure::CleanupExpired,
            ] {
                for signal in [false, true] {
                    assert_eq!(
                        completed(&unused_session(), Err(error), signal, &progress, true)
                            .unwrap_err()
                            .code,
                        "native_cleanup_incomplete"
                    );
                }
            }
        }
    }

    #[test]
    #[cfg(not(feature = "linux-desktop"))]
    fn connect_without_desktop_feature_returns_gui_unavailable() {
        let options = crate::options::Connection {
            target: crate::options::Target {
                saved: None,
                node: "node-1".into(),
                by_name: false,
                roots: std::path::PathBuf::from("/dev/null"),
                port: 8443,
                ipv6: false,
            },
            display: crate::options::DisplayChoice::Only,
            worker: std::path::PathBuf::from("/dev/null"),
            x_display: None,
            attempts: 1,
            fit_window: None,
            control: false,
            capture_system_shortcuts: false,
            clipboard: false,
            audio: None,
            upload: crate::options::UploadOptions {
                paths: Vec::new(),
                file_drop: false,
            },
            receive: None,
        };
        let runtime = RuntimeBuilder::current_thread()
            .enable_platform_reactor(true)
            .build()
            .unwrap();
        let cx = runtime.request_cx_with_budget(Budget::INFINITE);
        let mut shutdown = Shutdown::new().unwrap();
        let stopped = Cell::new(false);
        let api = LocalApi::new(std::path::Path::new("/dev/null")).unwrap();
        let result = connect(
            &runtime,
            &cx,
            &mut shutdown,
            &stopped,
            api,
            &options,
            true,
            false,
        );
        assert_eq!(result.unwrap_err().code, "gui_unavailable");
    }
}

#[cfg(all(test, feature = "linux-desktop"))]
#[path = "linux/controlled_audio_tests.rs"]
mod controlled_audio_tests;

#[cfg(all(test, feature = "linux-desktop"))]
#[path = "linux/audio_overflow_tests.rs"]
mod audio_overflow_tests;

#[cfg(all(test, feature = "linux-desktop"))]
#[path = "linux/control_reconnect_tests.rs"]
mod control_reconnect_tests;

#[cfg(feature = "linux-desktop")]
fn snapshot_evidence(stage: frd::media::PresentationStage) -> fr_client::robot::live::Evidence {
    match stage {
        frd::media::PresentationStage::DecodedOnly => fr_client::robot::live::Evidence::Decoded,
        frd::media::PresentationStage::SubmittedToCompositor => {
            fr_client::robot::live::Evidence::SubmittedToCompositor
        }
    }
}
