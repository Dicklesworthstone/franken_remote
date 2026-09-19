#[path = "linux/displays.rs"]
mod displays;
#[path = "linux/doctor.rs"]
mod doctor;
use super::{
    Failure,
    options::{Command, Connection, DisplayChoice, DoctorOptions, Options, Target},
    output,
};
use asupersync::{
    cx::Cx,
    runtime::{Runtime, RuntimeBuilder},
    signal::{Signal, SignalKind, signal},
    tls::Certificate,
    types::{Budget, CancelKind},
};
use fr_native::{
    desktop::{
        Configuration as DesktopConfiguration, Desktop,
        reconnect::{Mode, Session, Ui},
    },
    viewer_window::{Status as WindowStatus, StopReason, WindowControl},
};
use fr_wire::{display::Catalog, negotiation::Offer};
use frd::{
    native_connection::{
        AddressFamily, Client, Configuration, LocalApi, PeerSelector, TailnetError,
        reconnect::{self, CallbackError, Status},
    },
    session_startup::{ObserverPolicy, Presentation},
};
use std::{
    cell::{Cell, RefCell},
    fs::File,
    future::{Future, poll_fn},
    io::Read,
    path::Path,
    pin::pin,
    rc::Rc,
    task::Poll,
    time::Duration,
};

fn failure(code: &'static str, next: &'static str) -> Failure {
    Failure::new(code, next, 1)
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
        TailnetError::KeyExpired => failure(
            "tailnet_key_expired",
            "Reauthenticate the affected Tailscale node locally.",
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
        await_shutdown(cx, stopped, operation, async {
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
        })
        .await
    }
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
    let api = match &options.socket {
        Some(path) => LocalApi::new(path).map_err(tailnet)?,
        None => LocalApi::installed(),
    };
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
    match &options.command {
        Command::Help => unreachable!("help handled before runtime construction"),
        Command::Hosts => {
            let snapshot = runtime
                .block_on(shutdown.run(&cx, &stopped, api.discover(&cx)))
                .map_err(tailnet)?;
            Ok(output::hosts(&snapshot, options.json))
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
        ),
    }
}
fn connect(
    runtime: &Runtime,
    cx: &Cx,
    shutdown: &mut Shutdown,
    stopped: &Cell<bool>,
    api: LocalApi,
    connection: &Connection,
    json: bool,
) -> Result<String, Failure> {
    let roots = read_roots(&connection.target.roots)?;
    let mut client = Client::new(api, roots, Duration::from_secs(5)).map_err(|_| {
        failure(
            "invalid_native_configuration",
            "Verify the locally provisioned trust store and native connection policy.",
        )
    })?;
    let x_display = connection
        .x_display
        .clone()
        .or_else(|| std::env::var("DISPLAY").ok())
        .ok_or_else(|| {
            failure(
                "display_unavailable",
                "Set DISPLAY or --x-display for your existing local X11 session.",
            )
        })?;
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
    let configuration = if connection.display == DisplayChoice::Choose {
        configuration.with_display_picker()
    } else {
        configuration
    };
    let state = Rc::new(RefCell::new(Progress::default()));
    let mut application = Session::new(
        configuration,
        ObserverPolicy::default(),
        Mode::Observe,
        Interface {
            display: connection.display,
            progress: state.clone(),
        },
    );
    let selector = if connection.target.by_name {
        PeerSelector::Name(&connection.target.node)
    } else {
        PeerSelector::StableId(&connection.target.node)
    };
    let cfg = Configuration {
        port: connection.target.port,
        family: if connection.target.ipv6 {
            AddressFamily::Ipv6
        } else {
            AddressFamily::Ipv4
        },
        ..Default::default()
    };
    let policy = reconnect::Policy {
        max_attempts: connection.attempts,
        ..Default::default()
    };
    let operation = client.run_observing(
        cx.clone(),
        runtime.handle(),
        selector,
        cfg,
        offer(),
        policy,
        &mut application,
    );
    let result = runtime.block_on(shutdown.run(cx, stopped, operation));
    completed(&application, result, stopped.get(), &state.borrow(), json)
}
fn completed(
    application: &Session<Interface>,
    result: Result<(), reconnect::Failure>,
    stopped: bool,
    progress: &Progress,
    json: bool,
) -> Result<String, Failure> {
    // A signal or UI close never conceals unsuccessful native cleanup.
    if application.cleanup_failure().is_some()
        || application.desktop().is_some()
        || matches!(
            result,
            Err(reconnect::Failure::Cleanup | reconnect::Failure::CleanupExpired)
        )
    {
        return Err(failure(
            "native_cleanup_incomplete",
            "Native cleanup was not confirmed; inspect remaining workers before another connection.",
        ));
    }
    if stopped {
        return Err(Failure::new(
            "cancelled",
            "Session stopped and the supervisor completed cleanup; no control/input was requested.",
            130,
        ));
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
    // Cleanup itself stops the window with User. Only intent recorded BEFORE
    // cleanup may explain an otherwise failed observation as a normal UI exit.
    if let Err(error) = result
        && !progress.user_closed
    {
        return Err(match error {
            reconnect::Failure::Connection(frd::native_connection::Error::Tailnet(e)) => tailnet(e),
            _ => failure(
                "native_session_failed",
                "Verify host approval, selected display, worker/HEVC support and transport qualification; do not bypass admission checks.",
            ),
        });
    }
    Ok(completion(progress, json))
}

fn read_roots(path: &Path) -> Result<Vec<Certificate>, Failure> {
    let refused = || {
        failure(
            "invalid_trust_store",
            "Use a locally provisioned regular PEM CA-root file, at most 1 MiB and 64 certificates; never use roots from the contacted peer.",
        )
    };
    if !std::fs::symlink_metadata(path)
        .map_err(|_| refused())?
        .is_file()
    {
        return Err(refused());
    }
    let file = File::open(path).map_err(|_| refused())?;
    if !file.metadata().map_err(|_| refused())?.is_file() {
        return Err(refused());
    }
    let mut bytes = Vec::new();
    file.take(1_048_577)
        .read_to_end(&mut bytes)
        .map_err(|_| refused())?;
    let marker = b"-----BEGIN CERTIFICATE-----";
    if bytes.len() > 1_048_576 || bytes.windows(marker.len()).filter(|s| *s == marker).count() > 64
    {
        return Err(refused());
    }
    let roots = Certificate::from_pem(&bytes).map_err(|_| refused())?;
    if roots.is_empty() || roots.len() > 64 {
        return Err(refused());
    }
    Ok(roots)
}
fn offer() -> Offer {
    fr_client::native::observation_offer()
}

#[derive(Default)]
struct Progress {
    attempts: u8,
    opened: u8,
    presented: u64,
    approval_pending: bool,
    missing_display: bool,
    user_closed: bool,
    window: Option<WindowControl>,
}
impl Progress {
    fn begin(&mut self, attempt: u8) {
        self.attempts = attempt;
        self.user_closed = false;
        self.missing_display = false;
        self.approval_pending = false;
        // The previous owner has already been reaped. Its stopped handle must
        // not classify a connection failure before this attempt opens a window.
        self.window = None;
    }
    fn before_cleanup(&mut self, window: Option<WindowStatus>) -> Result<(), CallbackError> {
        self.user_closed = window == Some(WindowStatus::Stopped(StopReason::User));
        if self.user_closed {
            // Cleaning notification failure stops retries AFTER mandatory
            // cleanup. A user's close must never reconnect another window.
            Err(CallbackError)
        } else {
            Ok(())
        }
    }
}
struct Interface {
    display: DisplayChoice,
    progress: Rc<RefCell<Progress>>,
}
impl Ui for Interface {
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
        Ok(())
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
            Status::Connecting { attempt } => progress.begin(attempt),
            Status::Cleaning { .. } => {
                let window = progress.window.as_ref().map(WindowControl::status);
                return progress.before_cleanup(window);
            }
            _ => {}
        }
        Ok(())
    }
}
fn completion(progress: &Progress, json: bool) -> String {
    if json {
        format!(
            "{{\"schema_version\":1,\"timestamp_unix_ms\":{},\"outcome\":\"stopped\",\"role\":\"observe\",\"attempts\":{},\"opened\":{},\"subsequent_decoder_completions\":{},\"cleanup_confirmed\":true,\"transport_qualified\":false,\"physical_visibility_proven\":false}}\n",
            output::timestamp(),
            progress.attempts,
            progress.opened,
            progress.presented
        )
    } else {
        format!(
            "View-only session stopped; {} attempt(s), {} opened session(s), cleanup confirmed. Native transport/hardware remain unqualified.\n",
            progress.attempts, progress.opened
        )
    }
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
        assert_eq!(offer.capabilities.len(), 6);
        assert!(offer.capabilities.iter().all(|c| !c.name.contains("input")
            && !c.name.contains("clipboard")
            && !c.name.contains("audio")));
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
    fn explicit_display_selection_never_falls_back_to_another_display() {
        let mut ui = Interface {
            display: DisplayChoice::Handle(99),
            progress: Rc::new(RefCell::new(Progress::default())),
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
            },
        )
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
                        frd::native_connection::Error::Tailnet(TailnetError::LocalApiUnavailable)
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
